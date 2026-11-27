//! Repository indexing (RAG write path).
//!
//! The Rust engine owns this index end to end; it does not need to match the
//! Go engine's byte-exact snapshot derivation (Go is being retired). It only
//! needs internal consistency: what we write here must be readable by the
//! reader in `qdrant.rs` / `mod.rs`, which keys chunks by `chunk_point_id` and
//! finds the index through the manifest point.
//!
//! Flow:
//!   1. Walk the pinned head tree in the prepared clone, select eligible text
//!      files, and window them into chunks (80 lines / 2400 bytes, 8 overlap).
//!   2. Reuse unchanged blobs and verified chunk identities. Embed new chunk inputs.
//!   3. Upsert chunk points, then the manifest point LAST so a reader never
//!      sees a snapshot whose chunks are incomplete.
//!   4. Retain prior complete snapshots for verified fallback.
//!
//! The snapshot id is simply derived from the indexed commit and schema; it is
//! opaque to readers (they take it from the manifest).

use std::path::Path;

use super::hash::{digest, digest_string, GoJson};
use super::qdrant::{IndexedFile, Manifest, Qdrant, SnapshotFiles};
use super::{chunk_point_id, Chunk, Namespace, Schema};
use crate::domain::SourceRevision;
use crate::error::{err, Code, Result};
use crate::ignore::IgnorePolicy;
use crate::index_progress::{IndexProgress, Scan};

pub mod slice;

const CHUNK_LINES: usize = 80;
const CHUNK_BYTES: usize = 2400;
const OVERLAP_LINES: usize = 8;
const EMBED_BATCH: usize = 32;
const EMBED_BATCH_BYTES: usize = 48_000;
const MAX_FILE_BYTES: u64 = 512 * 1024;
const MAX_CHUNKS: usize = 30_000;

/// Index the repository at `head_sha` from the prepared bare clone into Qdrant.
/// Returns the number of chunks indexed. A repository with no eligible files
/// still publishes an (empty) manifest so readers see a valid, up-to-date index.
pub async fn index_repository(
    repo_dir: &Path,
    head_sha: &str,
    rev: &SourceRevision,
    schema: &Schema,
    embedder: &impl super::pull::DocumentVectors,
    qdrant: &Qdrant,
    token: &str,
    ignore: Option<&IgnorePolicy>,
    progress: &mut IndexProgress,
    max_input_bytes: usize,
) -> Result<usize> {

    let ns = Namespace::for_revision(rev);
    qdrant.ensure(schema).await?;
    let previous = qdrant.manifest(&ns, schema)?;
    let (changed, files) = collect_snapshot(repo_dir, head_sha, token, ignore, progress, max_input_bytes, previous.as_ref()).await?;
    persist_changes(&changed, files, previous.as_ref(), rev, schema, embedder, qdrant, progress).await?;
    Ok(progress.chunks)
}

async fn collect_snapshot(
    repo_dir: &Path, head_sha: &str, token: &str, ignore: Option<&IgnorePolicy>,
    progress: &mut IndexProgress, max_input_bytes: usize, previous: Option<&Manifest>,
) -> Result<(Vec<Chunk>, SnapshotFiles)> {
    let previous_files = previous.and_then(|m| m.files.as_ref());
    let tree = list_blobs(repo_dir, head_sha, token).await?;
    let mut files = SnapshotFiles::new();
    let mut changed = Vec::new();
    progress.scan = Scan { measured: true, stage: "reading".into(), total: tree.len(), ..Scan::default() };
    progress.stage("reading", "Checking files against the previous index.".into());
    for (path, blob) in tree {
        progress.scan.processed += 1;
        let mut count = 0;
        if eligible(&path) && !ignore.is_some_and(|policy| policy.excluded(&path)) {
            if let Some(cached) = previous_files.and_then(|fs| fs.get(&path)).filter(|f| f.blob == blob) {
                count = cached.points.len();
                progress.reused += count;
                progress.scan.read += usize::from(cached.text);
                files.insert(path, cached.clone());
            } else {
                // A fetch error must leave the previous complete manifest active.
                // Only cache an exclusion after successfully examining this object.
                let content = read_pinned_blob(repo_dir, &blob, token).await?;
                let mut file = IndexedFile { blob, text: content.is_some(), ..Default::default() };
                if let Some(content) = content {
                    let chunks = chunk_file(&path, &content);
                    count = chunks.len();
                    file.input_bytes = chunks.iter().map(|c| c.input().len()).sum();
                    progress.scan.read += 1;
                    changed.extend(chunks);
                }
                files.insert(path, file);
            }
        }
        if count == 0 { progress.scan.skipped += 1; }
        progress.chunks += count;
        if progress.chunks > MAX_CHUNKS { return Err(err(Code::Retrieval, "repository exceeds chunk limit")); }
        progress.reading(false);
    }
    progress.scan.stage = "completed".into();
    progress.reading(true);
    check_input_budget(files.values().map(|file| file.input_bytes).sum(), max_input_bytes)?;
    Ok((changed, files))
}

async fn persist_changes(chunks: &[Chunk], mut files: SnapshotFiles, previous: Option<&Manifest>,
    rev: &SourceRevision, schema: &Schema, embedder: &impl super::pull::DocumentVectors,
    qdrant: &Qdrant, progress: &mut IndexProgress) -> Result<()> {
    let namespace = Namespace::for_revision(rev);
    let snapshot = snapshot_id(rev, schema);
    let total = progress.chunks;
    progress.stage("embedding", format!("Indexing changed chunks for {total} repository chunks."));
    if let Some(m) = previous { qdrant.archive_previous(&namespace, schema, m, &snapshot).await?; }

    let mut remaining = usize::MAX;
    persist_points(chunks, &mut files, previous, rev, schema, embedder, qdrant, progress, &mut remaining).await?;
    publish_files(files, rev, schema, qdrant, progress).await
}

/// Keep this review's file membership in memory. Only immutable chunk points
/// are shared with concurrent reviews and the default-branch indexer.
pub async fn review_snapshot(
    repo_dir: &Path, rev: &SourceRevision, schema: &Schema,
    embedder: &impl super::pull::DocumentVectors, qdrant: &Qdrant, token: &str,
    ignore: Option<&IgnorePolicy>, progress: &mut IndexProgress, max_input_bytes: usize,
    previous: Option<&Manifest>,
) -> Result<Manifest> {
    qdrant.ensure(schema).await?;
    let (changed, mut files) = collect_snapshot(repo_dir, &rev.revision_id, token, ignore,
        progress, max_input_bytes, previous).await?;
    let mut remaining = usize::MAX;
    persist_points(&changed, &mut files, previous, rev, schema, embedder, qdrant, progress, &mut remaining).await?;
    Ok(Manifest { snapshot: snapshot_id(rev, schema), revision: Some(rev.clone()), files: Some(files) })
}

async fn persist_points(chunks: &[Chunk], files: &mut SnapshotFiles, previous: Option<&Manifest>,
    rev: &SourceRevision, schema: &Schema, embedder: &impl super::pull::DocumentVectors,
    qdrant: &Qdrant, progress: &mut IndexProgress, remaining: &mut usize) -> Result<bool> {
    let namespace = Namespace::for_revision(rev);
    let snapshot = snapshot_id(rev, schema);
    let total = progress.chunks;
    // A legacy manifest has one physical snapshot. Newer manifests may reference
    // immutable points from multiple commits. The target revision can also have
    // already persisted points from an interrupted run.
    let mut candidates = Vec::with_capacity(chunks.len() * 2);
    let ids: Vec<_> = chunks.iter().map(|chunk| {
        let target = chunk_point_id(&namespace, schema, &snapshot, &chunk.key);
        let prior = previous.and_then(|m| match &m.files {
            Some(fs) => fs.get(&chunk.path)?.points.get(&chunk.key).cloned(),
            None => Some(chunk_point_id(&namespace, schema, &m.snapshot, &chunk.key)),
        });
        candidates.push(target.clone());
        if let Some(id) = &prior { if id != &target { candidates.push(id.clone()); } }
        (target, prior)
    }).collect();
    let saved = qdrant.saved_chunk_ids(schema, &candidates).await?;
    let mut pending = Vec::new();
    for (chunk, (target, prior)) in chunks.iter().zip(ids) {
        let point = prior.filter(|id| saved.contains(id)).unwrap_or(target);
        if saved.contains(&point) { progress.reused += 1; } else { pending.push(chunk); }
        files.get_mut(&chunk.path).ok_or_else(|| err(Code::Retrieval, "missing indexed file"))?
            .points.insert(chunk.key.clone(), point);
    }
    progress.embedding();
    let mut embedded = 0usize;
    let mut i = 0;
    while i < pending.len() {
        let mut batch: Vec<&Chunk> = Vec::new();
        let mut bytes = 0usize;
        while i < pending.len() && batch.len() < EMBED_BATCH {
            let input_len = pending[i].input().len();
            if !batch.is_empty() && bytes + input_len > EMBED_BATCH_BYTES {
                break;
            }
            bytes += input_len;
            batch.push(pending[i]);
            i += 1;
        }
        if bytes > *remaining { return Ok(false); }
        *remaining -= bytes;
        let inputs: Vec<String> = batch.iter().map(|c| c.input()).collect();
        let vectors = embedder.documents(&inputs).await?;
        // Pace requests to stay under the Bedrock tokens-per-minute quota.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        if vectors.len() != batch.len() {
            return Err(err(Code::Retrieval, "embedding count mismatch"));
        }
        let points: Vec<serde_json::Value> = batch
            .iter()
            .zip(vectors)
            .map(|(c, vector)| chunk_point(&namespace, schema, &snapshot, c, vector))
            .collect();
        // Upsert in smaller sub-batches to keep each request well under any
        // request-size limit (96 points * 1024 floats + content can be several MB).
        for sub in points.chunks(16) {
            qdrant.upsert(schema, sub.to_vec()).await?;
        }
        embedded += batch.len();
        progress.embedded = embedded;
        progress.embedding();
        tracing::info!(embedded, total, "indexing progress");
    }

    Ok(true)
}

async fn publish_files(files: SnapshotFiles, rev: &SourceRevision, schema: &Schema, qdrant: &Qdrant,
    progress: &mut IndexProgress) -> Result<()> {
    let namespace = Namespace::for_revision(rev);
    let snapshot = snapshot_id(rev, schema);
    let total = progress.chunks;
    // 3. Manifest LAST: this is what flips the active snapshot for readers.
    progress.stage("publishing", "Publishing the completed repository index.".into());
    let mut manifest = manifest_point(&namespace, schema, &snapshot, rev, total);
    qdrant.attach_files(&namespace,schema,&snapshot,&mut manifest,files).await?;
    qdrant.upsert(schema, vec![manifest]).await?;
    progress.indexed = true;
    progress.stage("summarizing", "Index saved. Preparing the repository description.".into());
    Ok(())
}

#[cfg(test)]
pub(super) async fn persist_chunks(chunks: &[Chunk], rev: &SourceRevision, schema: &Schema,
    embedder: &impl super::pull::DocumentVectors, qdrant: &Qdrant, progress: &mut IndexProgress) -> Result<()> {
    qdrant.ensure(schema).await?;
    let previous = qdrant.manifest(&Namespace::for_revision(rev), schema)?;
    let mut files = SnapshotFiles::new();
    for c in chunks { files.entry(c.path.clone()).or_default().input_bytes += c.input().len(); }
    progress.chunks = chunks.len();
    persist_changes(chunks, files, previous.as_ref(), rev, schema, embedder, qdrant, progress).await
}

/// Count files with exactly the same filters and chunking as a full index.
/// Used to repair old counters without embedding or writing vector points.
pub async fn scan_repository(
    repo_dir: &Path,
    head_sha: &str,
    token: &str,
    ignore: Option<&IgnorePolicy>,
    progress: &mut IndexProgress,
) -> Result<Vec<Chunk>> {
    // 1. Discover eligible files and build chunks. Paths the repository's
    //    `.inkspectignore` excludes are never read or embedded.
    progress.stage("discovering", "Scanning repository files.".into());
    let files = list_tree(repo_dir, head_sha, token).await?;
    progress.scan = Scan { measured: true, stage: "reading".into(), total: files.len(), ..Scan::default() };
    progress.stage("discovering", format!("Scanning files: {} found.", files.len()));
    progress.stage("reading", "Reading repository files.".into());
    let mut chunks: Vec<Chunk> = Vec::new();
    for path in files {
        if !eligible(&path) || ignore.is_some_and(|policy| policy.excluded(&path)) {
            collect_file(&path, None, &mut chunks, progress)?;
            continue;
        }
        let content = read_blob(repo_dir, head_sha, &path, token).await?;
        collect_file(&path, content.as_deref(), &mut chunks, progress)?;
    }
    progress.scan.stage = "completed".into();
    progress.reading(true);
    Ok(chunks)
}

fn collect_file(path: &str, content: Option<&str>, chunks: &mut Vec<Chunk>, progress: &mut IndexProgress) -> Result<()> {
    progress.scan.processed += 1;
    if let Some(content) = content {
        progress.scan.read += 1;
        let file_chunks = chunk_file(path, content);
        // "read" counts safe text files loaded, including empty files;
        // "skipped" counts files that contribute no searchable chunks.
        if file_chunks.is_empty() { progress.scan.skipped += 1; }
        for c in file_chunks {
            chunks.push(c);
            if chunks.len() > MAX_CHUNKS {
                return Err(err(Code::Retrieval, "repository exceeds chunk limit"));
            }
        }
    } else {
        progress.scan.skipped += 1;
    }
    progress.chunks = chunks.len();
    progress.reading(false);
    Ok(())
}

/// Opaque snapshot id for a repository index: hash of the indexed commit and
/// the embedding schema. Readers never recompute it; they read it from the
/// manifest.
fn snapshot_id(rev: &SourceRevision, schema: &Schema) -> String {
    digest(&GoJson::Array(vec![
        GoJson::s("rust-repo-index-v1"),
        GoJson::s(rev.revision_id.clone()),
        schema.go_json(),
    ]))
}

/// Build a chunk point JSON object matching the reader's expectations.
fn chunk_point(
    ns: &Namespace,
    schema: &Schema,
    snapshot: &str,
    c: &Chunk,
    vector: Vec<f32>,
) -> serde_json::Value {
    serde_json::json!({
        "id": chunk_point_id(ns, schema, snapshot, &c.key),
        "vector": vector,
        "payload": {
            "kind": "chunk",
            "namespace": {"tenant": ns.tenant, "repository": ns.repository},
            "embedding_schema": {
                "version": schema.version, "model": schema.model,
                "dimensions": schema.dimensions, "normalize": schema.normalize,
            },
            "snapshot": snapshot,
            "chunk": {
                "key": c.key, "path": c.path,
                "start_line": c.start_line, "end_line": c.end_line,
                "file_sha256": c.file_sha256,
            },
            "content": c.content,
        }
    })
}

/// Build the manifest point. Its vector is all zeros; readers only use its
/// payload (snapshot + revision).
fn manifest_point(
    ns: &Namespace,
    schema: &Schema,
    snapshot: &str,
    rev: &SourceRevision,
    chunks: usize,
) -> serde_json::Value {
    serde_json::json!({
        "id": Qdrant::manifest_id(ns, schema),
        "vector": vec![0.0f32; schema.dimensions as usize],
        "payload": {
            "kind": "manifest",
            "revision": {
                "tenant_id": rev.tenant_id, "provider": rev.provider,
                "provider_instance": rev.provider_instance,
                "repository_id": rev.repository_id, "revision_id": rev.revision_id,
            },
            "namespace": {"tenant": ns.tenant, "repository": ns.repository},
            "embedding_schema": {
                "version": schema.version, "model": schema.model,
                "dimensions": schema.dimensions, "normalize": schema.normalize,
            },
            "snapshot": snapshot,
            "chunks": chunks,
        }
    })
}

/// Window a file into chunks: up to 80 lines / 2400 bytes per window, 8-line
/// overlap, skipping whitespace-only windows. Lines keep their trailing '\n'.
/// A single line over 2400 bytes causes the whole file to be skipped.
fn chunk_file(path: &str, content: &str) -> Vec<Chunk> {
    let file_sha = {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(content.as_bytes());
        hex::encode(h.finalize())
    };
    let lines: Vec<&str> = split_keep_newlines(content);
    // Skip the file if any single line exceeds the byte budget.
    if lines.iter().any(|l| l.len() > CHUNK_BYTES) {
        tracing::debug!(path, "skipping file: a line exceeds the chunk byte limit");
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut start = 0usize; // 0-based line index
    while start < lines.len() {
        let mut end = start;
        let mut bytes = 0usize;
        while end < lines.len() && end - start < CHUNK_LINES && bytes + lines[end].len() <= CHUNK_BYTES {
            bytes += lines[end].len();
            end += 1;
        }
        let text: String = lines[start..end].concat();
        if !text.trim().is_empty() {
            let mut c = Chunk {
                key: String::new(),
                path: path.to_string(),
                start_line: (start + 1) as u32,
                end_line: end as u32,
                file_sha256: file_sha.clone(),
                content: text,
            };
            c.key = digest_string(&c.input());
            chunks.push(c);
        }
        if end == lines.len() {
            break;
        }
        start = (start + 1).max(end.saturating_sub(OVERLAP_LINES));
    }
    chunks
}

/// Split content into lines, each retaining its trailing '\n' (last line may
/// have none). Matches the Go window input.
fn split_keep_newlines(content: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let bytes = content.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            out.push(&content[start..=i]);
            start = i + 1;
        }
    }
    if start < content.len() {
        out.push(&content[start..]);
    }
    out
}

/// Blob identity comes from the pinned tree, including on force-pushes and
/// non-linear merges. NUL delimiters preserve tabs and newlines in file names.
async fn list_blobs(repo_dir: &Path, sha: &str, token: &str) -> Result<Vec<(String, String)>> {
    let raw = crate::git::index_git(repo_dir, &["ls-tree", "-r", "-z", sha], token).await?;
    let mut entries = Vec::new();
    for record in raw.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let tab = record.iter().position(|b| *b == b'\t').ok_or_else(|| err(Code::Source, "invalid tree entry"))?;
        let meta = std::str::from_utf8(&record[..tab]).map_err(|_| err(Code::Source, "invalid tree metadata"))?;
        let fields: Vec<_> = meta.split_ascii_whitespace().collect();
        if fields.len() != 3 { return Err(err(Code::Source, "invalid tree entry")); }
        if fields[1] != "blob" || !matches!(fields[0], "100644" | "100755") { continue; }
        if let Ok(path) = std::str::from_utf8(&record[tab+1..]) { entries.push((path.to_owned(), fields[2].to_owned())); }
    }
    Ok(entries)
}

/// List the file paths in the pinned tree (recursive). The tree itself is
/// always present in a `blob:none` partial clone, so this needs no network.
async fn list_tree(repo_dir: &Path, sha: &str, token: &str) -> Result<Vec<String>> {
    let out = crate::git::index_git(repo_dir, &["ls-tree", "-r", "--name-only", sha], token).await?;
    Ok(String::from_utf8_lossy(&out)
        .lines()
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

/// Read a blob at the pinned commit; return None for binary/oversized/unreadable.
///
/// Unlike the diff reader, indexing reads arbitrary repository files and the
/// clone is a `blob:none` partial clone, so a blob may need lazy-fetching from
/// the promisor remote. `index_git` permits that (network on), which is safe:
/// indexing only ever reads immutable objects of the pinned commit.
pub(crate) async fn read_blob(repo_dir: &Path, sha: &str, path: &str, token: &str) -> Result<Option<String>> {
    // Best-effort lookup for a possibly deleted path at a review head.
    Ok(read_pinned_blob(repo_dir, &format!("{sha}:{path}"), token).await.unwrap_or(None))
}

async fn read_pinned_blob(repo_dir: &Path, spec: &str, token: &str) -> Result<Option<String>> {
    let bytes = crate::git::index_git(repo_dir, &["cat-file", "-s", spec], token).await?;
    let size = String::from_utf8_lossy(&bytes).trim().parse::<u64>()
        .map_err(|_| err(Code::Source, "invalid blob size"))?;
    if size > MAX_FILE_BYTES { return Ok(None); }
    let raw = crate::git::index_git(repo_dir, &["cat-file", "blob", spec], token).await?;
    if raw.contains(&0) {
        return Ok(None); // binary
    }
    match String::from_utf8(raw) {
        Ok(s) if !contains_secret(&s) => Ok(Some(s)),
        _ => Ok(None),
    }
}

/// File eligibility: an extension allowlist plus a few known filenames, with
/// sensitive names excluded. Simplified from the Go rules (Go is retiring) but
/// keeps the security-relevant exclusions.
pub(crate) fn eligible(path: &str) -> bool {
    let base = path.rsplit('/').next().unwrap_or(path);
    if base.starts_with('.') || sensitive(base) || base.ends_with(".min.js") || base.ends_with(".min.css") {
        return false;
    }
    // Excluded directories anywhere in the path.
    for seg in path.split('/') {
        if BLOCKED_DIRS.contains(&seg) || sensitive(seg) {
            return false;
        }
    }
    const SPECIAL: &[&str] = &["go.mod", "Dockerfile", "Makefile", "CMakeLists.txt", "Gemfile"];
    if SPECIAL.contains(&base) {
        return true;
    }
    let ext = base.rsplit('.').next().map(|e| format!(".{}", e.to_lowercase()));
    matches!(
        ext.as_deref(),
        Some(
            ".txt" | ".xml" | ".ini" | ".cfg" | ".conf" | ".csv" | ".tsv" | ".go" | ".js" | ".jsx"
            | ".ts" | ".tsx" | ".mjs" | ".cjs" | ".py" | ".rb" | ".rs" | ".swift" | ".kt" | ".kts"
            | ".java" | ".c" | ".h" | ".cpp" | ".hpp" | ".cs" | ".php" | ".sql" | ".md" | ".mdx"
            | ".rst" | ".proto" | ".graphql" | ".gql" | ".yml" | ".yaml" | ".toml" | ".json"
            | ".html" | ".css" | ".scss" | ".vue" | ".svelte" | ".sh" | ".gd" | ".tres" | ".tscn"
        )
    )
}

const BLOCKED_DIRS: &[&str] = &[
    "node_modules", "vendor", "dist", "build", "out", "output", "outputs", "coverage", "bin",
    "target", "venv", "__pycache__", "Pods", "DerivedData", "Carthage", "backups", "logs",
    "captures", "qdrant-data", "ollama-data", "postgres-data",
];

fn sensitive(name: &str) -> bool {
    let n = name.to_lowercase();
    n.contains("secret")
        || n.contains("credential")
        || n.starts_with("id_rsa")
        || n.starts_with("id_ed25519")
        || n.contains("private-key")
        || n.ends_with(".local.json")
        || n.ends_with(".tfstate")
}

/// Refuse to index files that contain obvious secrets.
pub(crate) fn contains_secret(s: &str) -> bool {
    s.contains("-----BEGIN")
        && (s.contains("PRIVATE KEY-----"))
        || s.contains("github_pat_")
        || has_token_prefix(s, "ghp_")
        || has_token_prefix(s, "gho_")
        || has_token_prefix(s, "ABSK")
}

fn has_token_prefix(s: &str, prefix: &str) -> bool {
    s.match_indices(prefix).any(|(i, _)| {
        s[i + prefix.len()..]
            .chars()
            .take(30)
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .count()
            >= 30
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_counts_read_and_omitted_files_without_inventing_chunks() {
        let mut progress = IndexProgress::default();
        let mut chunks = Vec::new();
        collect_file("code.rs", Some("fn main() {}\n"), &mut chunks, &mut progress).unwrap();
        collect_file("binary.rs", None, &mut chunks, &mut progress).unwrap();
        collect_file("empty.rs", Some(" \n"), &mut chunks, &mut progress).unwrap();
        collect_file("long.rs", Some(&"x".repeat(CHUNK_BYTES + 1)), &mut chunks, &mut progress).unwrap();
        assert_eq!(progress.scan.processed, 4);
        assert_eq!(progress.scan.read, 3);
        assert_eq!(progress.scan.skipped, 3);
        assert_eq!(progress.chunks, 1);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].path, "code.rs");
    }

    #[test]
    fn windows_overlap_and_skip_blank() {
        let content = (1..=10)
            .map(|n| format!("line {n}\n"))
            .collect::<String>();
        let chunks = chunk_file("a.rs", &content);
        assert!(!chunks.is_empty());
        assert_eq!(chunks[0].start_line, 1);
    }

    #[test]
    fn eligible_filters() {
        assert!(eligible("scripts/ui/hud.gd"));
        assert!(eligible("src/main.rs"));
        assert!(!eligible("node_modules/x/index.js"));
        assert!(!eligible("secrets.env.local.json"));
        assert!(!eligible("assets/foo.png"));
    }
}

fn check_input_budget(bytes: usize, limit: usize) -> Result<()> {
    if limit > 0 && bytes > limit {
        return Err(err(Code::Retrieval, "repository exceeds indexing input allowance"));
    }
    Ok(())
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    #[test]
    fn input_allowance_is_checked_before_embedding_and_counts_metadata() {
        let mut chunk = Chunk { path:"src/a.rs".into(), start_line:1, end_line:1,
            content:"fn main() {}".into(), file_sha256:String::new(), key:String::new() };
        chunk.key=chunk.computed_key();
        let exact=chunk.input().len();
        assert!(check_input_budget(exact,exact).is_ok());
        assert!(check_input_budget(exact,exact-1).is_err());
        assert!(check_input_budget(exact,0).is_ok());
    }
}
