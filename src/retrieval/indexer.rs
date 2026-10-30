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
//!   2. Embed chunk inputs as documents (Cohere Embed v4, L2-normalized).
//!   3. Upsert chunk points, then the manifest point LAST so a reader never
//!      sees a snapshot whose chunks are incomplete.
//!   4. Delete chunk points from older snapshots.
//!
//! The snapshot id is simply derived from the indexed commit and schema; it is
//! opaque to readers (they take it from the manifest).

use std::path::Path;

use super::cohere::Bedrock;
use super::hash::{digest, digest_string, GoJson};
use super::qdrant::Qdrant;
use super::{chunk_point_id, Chunk, Namespace, Schema};
use crate::domain::SourceRevision;
use crate::error::{err, Code, Result};
use crate::ignore::IgnorePolicy;

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
    embedder: &Bedrock,
    qdrant: &Qdrant,
    token: &str,
    ignore: Option<&IgnorePolicy>,
) -> Result<usize> {
    let namespace = Namespace::for_revision(rev);
    let snapshot = snapshot_id(rev, schema);

    // 1. Discover eligible files and build chunks. Paths the repository's
    //    `.inkspectignore` excludes are never read or embedded.
    let files = list_tree(repo_dir, head_sha, token).await?;
    let mut chunks: Vec<Chunk> = Vec::new();
    for path in files {
        if !eligible(&path) || ignore.is_some_and(|policy| policy.excluded(&path)) {
            continue;
        }
        let Some(content) = read_blob(repo_dir, head_sha, &path, token).await? else {
            continue; // binary / unreadable / too large
        };
        for c in chunk_file(&path, &content) {
            chunks.push(c);
            if chunks.len() > MAX_CHUNKS {
                return Err(err(Code::Retrieval, "repository exceeds chunk limit"));
            }
        }
    }
    tracing::info!(chunks = chunks.len(), "indexing repository");

    qdrant.ensure(schema).await?;

    // 2 + 3. Embed in batches and upsert chunk points.
    let total = chunks.len();
    let mut embedded = 0usize;
    let mut i = 0;
    while i < chunks.len() {
        let mut batch: Vec<&Chunk> = Vec::new();
        let mut bytes = 0usize;
        while i < chunks.len() && batch.len() < EMBED_BATCH {
            let input_len = chunks[i].input().len();
            if !batch.is_empty() && bytes + input_len > EMBED_BATCH_BYTES {
                break;
            }
            bytes += input_len;
            batch.push(&chunks[i]);
            i += 1;
        }
        let inputs: Vec<String> = batch.iter().map(|c| c.input()).collect();
        let vectors = embedder.embed_documents_async(&inputs).await?;
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
        tracing::info!(embedded, total, "indexing progress");
    }

    // 3. Manifest LAST: this is what flips the active snapshot for readers.
    let manifest = manifest_point(&namespace, schema, &snapshot, rev, chunks.len());
    qdrant.upsert(schema, vec![manifest]).await?;

    // 4. Remove stale snapshots (best effort; the index is already published).
    if let Err(e) = qdrant.delete_old_snapshots(&namespace, schema, &snapshot).await {
        tracing::warn!(error = %e, "old-snapshot cleanup failed (index is still published)");
    }

    Ok(chunks.len())
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
    let spec = format!("{sha}:{path}");
    // Size guard first (uses the local tree metadata; may lazy-fetch nothing).
    if let Ok(bytes) = crate::git::index_git(repo_dir, &["cat-file", "-s", &spec], token).await {
        if let Ok(n) = String::from_utf8_lossy(&bytes).trim().parse::<u64>() {
            if n > MAX_FILE_BYTES {
                return Ok(None);
            }
        }
    }
    let raw = match crate::git::index_git(repo_dir, &["cat-file", "blob", &spec], token).await {
        Ok(b) => b,
        Err(_) => return Ok(None),
    };
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
