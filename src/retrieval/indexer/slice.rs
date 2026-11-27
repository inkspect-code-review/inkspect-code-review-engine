//! Each invocation checkpoints one page of a pinned tree. Paid vector batches
//! are immutable and can be recovered even before a page has been committed.
use super::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};

#[derive(Default, Serialize, Deserialize)]
struct Cursor {
    signature: String,
    next: usize,
    pages: usize,
    bytes: usize,
    chunks: usize,
    read: usize,
}

fn checkpoint_dir() -> Result<PathBuf> {
    let journal = std::env::var_os("INKSPECT_USAGE_FILE")
        .map(PathBuf::from)
        .ok_or_else(|| err(Code::Internal, "missing index journal"))?;
    let operation = journal
        .parent()
        .ok_or_else(|| err(Code::Internal, "invalid index journal"))?;
    let (parent, number) = operation
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.rsplit_once("-slice-"))
        .ok_or_else(|| err(Code::Internal, "invalid slice directory"))?;
    if parent.is_empty()
        || !parent
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        || number.parse::<u32>().is_err()
    {
        return Err(err(Code::Internal, "invalid index operation"));
    }
    Ok(operation.with_file_name(parent).join("index-pages"))
}

fn save(path: &Path, value: &impl Serialize) -> Result<()> {
    let temp = path.with_extension("tmp");
    let io = || err(Code::Internal, "index checkpoint write failed");
    let file = fs::File::create(&temp).map_err(|_| io())?;
    serde_json::to_writer(&file, value).map_err(|_| io())?;
    file.sync_all().map_err(|_| io())?;
    fs::rename(temp, path).map_err(|_| io())
}

pub async fn run(
    repo: &Path,
    rev: &SourceRevision,
    schema: &Schema,
    embedder: &impl crate::retrieval::pull::DocumentVectors,
    db: &Qdrant,
    token: &str,
    policy: Option<&IgnorePolicy>,
    progress: &mut IndexProgress,
    ceiling: usize,
    allowance: usize,
) -> Result<Value> {
    run_at(
        &checkpoint_dir()?,
        repo,
        rev,
        schema,
        embedder,
        db,
        token,
        policy,
        progress,
        ceiling,
        allowance,
    )
    .await
}

pub(in crate::retrieval) async fn run_at(
    root: &Path,
    repo: &Path,
    rev: &SourceRevision,
    schema: &Schema,
    embedder: &impl crate::retrieval::pull::DocumentVectors,
    db: &Qdrant,
    token: &str,
    policy: Option<&IgnorePolicy>,
    progress: &mut IndexProgress,
    ceiling: usize,
    allowance: usize,
) -> Result<Value> {
    fs::create_dir_all(&root).map_err(|_| err(Code::Internal, "index checkpoint unavailable"))?;
    let ns = Namespace::for_revision(rev);
    db.ensure(schema).await?;
    let prior = db.manifest(&ns, schema)?;
    let signature = format!(
        "{}:{}",
        snapshot_id(rev, schema),
        policy.map(|p| p.digest.as_str()).unwrap_or("")
    );
    let path = root.join("cursor.json");
    let mut cursor: Cursor = match fs::read(&path) {
        Ok(raw) => serde_json::from_slice(&raw)
            .map_err(|_| err(Code::Internal, "invalid index checkpoint"))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Cursor {
            signature: signature.clone(),
            ..Cursor::default()
        },
        Err(_) => return Err(err(Code::Internal, "index checkpoint unavailable")),
    };
    if cursor.signature != signature {
        return Err(err(Code::Internal, "index checkpoint scope changed"));
    }
    let tree = list_blobs(repo, &rev.revision_id, token).await?;
    let total = tree.len();
    let mut left = allowance;
    let mut page = SnapshotFiles::new();
    let started = Instant::now();
    progress.stage("reading", "Preparing repository index.".into());
    let start = cursor.next;
    for (path, blob) in tree.into_iter().skip(start) {
        if cursor.next > start
            && (cursor.next - start >= 512 || started.elapsed() > Duration::from_secs(45))
        {
            break;
        }
        if eligible(&path) && !policy.is_some_and(|p| p.excluded(&path)) {
            let file = if let Some(old) = prior
                .as_ref()
                .and_then(|m| m.files.as_ref())
                .and_then(|f| f.get(&path))
                .filter(|f| f.blob == blob)
            {
                old.clone()
            } else {
                let content = read_pinned_blob(repo, &blob, token).await?;
                let chunks = content
                    .as_ref()
                    .map(|text| chunk_file(&path, text))
                    .unwrap_or_default();
                let bytes = chunks.iter().map(|c| c.input().len()).sum();
                if ceiling == 0 || cursor.bytes.saturating_add(bytes) > ceiling {
                    return Ok(json!({"size_exceeded":true}));
                }
                let mut part = SnapshotFiles::new();
                part.insert(
                    path.clone(),
                    IndexedFile {
                        blob,
                        text: content.is_some(),
                        input_bytes: bytes,
                        ..Default::default()
                    },
                );
                if !persist_points(
                    &chunks,
                    &mut part,
                    prior.as_ref(),
                    rev,
                    schema,
                    embedder,
                    db,
                    progress,
                    &mut left,
                )
                .await?
                {
                    break;
                }
                part.remove(&path).unwrap()
            };
            cursor.bytes += file.input_bytes;
            if cursor.bytes > ceiling {
                return Ok(json!({"size_exceeded":true}));
            }
            cursor.chunks += file.points.len();
            cursor.read += usize::from(file.text);
            page.insert(path, file);
        }
        cursor.next += 1;
    }
    if cursor.next > start {
        save(&root.join(format!("{}.json", cursor.pages)), &page)?;
        cursor.pages += 1;
        save(&path, &cursor)?;
    }
    progress.chunks = cursor.chunks;
    progress.scan = Scan {
        measured: true,
        stage: "reading".into(),
        total,
        processed: cursor.next,
        read: cursor.read,
        chunks: cursor.chunks,
        ..Scan::default()
    };
    progress.reading(true);
    if cursor.next < total {
        return Ok(json!({"indexed":false,"processed":cursor.next,"total":total}));
    }
    let mut files = SnapshotFiles::new();
    for n in 0..cursor.pages {
        let f = fs::File::open(root.join(format!("{n}.json")))
            .map_err(|_| err(Code::Internal, "missing index page"))?;
        let entries: SnapshotFiles =
            serde_json::from_reader(f).map_err(|_| err(Code::Internal, "invalid index page"))?;
        files.extend(entries);
    }
    publish_files(files, rev, schema, db, progress).await?;
    Ok(json!({"indexed":true,"revision":rev.revision_id,"chunks":cursor.chunks}))
}
