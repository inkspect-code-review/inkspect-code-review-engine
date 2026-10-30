//! PR branch checkout and diff computation (step 2).
//!
//! Operates on the pinned bare clone produced by `crate::sandbox`. It computes
//! the change between the merge-base (`base_sha`) and head (`head_sha`) using
//! hardened, read-only git plumbing: no hooks, no network, no filters,
//! `--no-renames` disabled lazy fetch, literal pathspecs.
//!
//! Output is a structured `DiffSummary` of line-range edits (like the Go
//! engine's ChangeSet) plus, per changed file, a bounded hunk excerpt used both
//! as the model prompt context and to build RAG queries. No raw file bytes
//! leave this module except the diff hunks themselves.

use crate::error::{err, Code, Result};
use crate::ignore::IgnorePolicy;
use std::ffi::{OsStr, OsString};
use std::path::Path;

/// A single edited line range on both sides. 1-based; a zero `*_lines` marks an
/// insertion/deletion point. Matches Go `domain.LineEdit`.
#[derive(Debug, Clone)]
pub struct LineEdit {
    pub base_start: u32,
    pub base_lines: u32,
    pub target_start: u32,
    pub target_lines: u32,
}

/// A changed file. Matches Go `domain.FileChange` plus a rendered hunk excerpt.
#[derive(Debug, Clone)]
pub struct ChangedFile {
    /// modified | added | deleted | renamed.
    pub kind: String,
    /// Target path (empty for deleted files).
    pub path: String,
    /// Base path (for renames/deletes).
    pub base_path: String,
    pub edits: Vec<LineEdit>,
    /// Whether the whole file counts as changed (binary/oversized fallback).
    pub unknown: bool,
    /// Bounded unified-diff hunk text for the prompt. Untrusted.
    pub hunk_text: String,
    /// Bounded excerpt of the changed+nearby lines, used to build RAG queries.
    pub excerpt: String,
}

#[derive(Debug, Clone)]
pub struct DiffSummary {
    pub files: Vec<ChangedFile>,
}

const MAX_FILE_HUNK_BYTES: usize = 64_000;
const MAX_DIFF_FILES: usize = 500;

/// Git for the indexer. Like the diff reader but permits https lazy-fetch of
/// missing blobs from the promisor remote (the clone is `blob:none`), using the
/// installation token from INKSPECT_GITHUB_READ_TOKEN via http.extraHeader.
/// Hooks, submodules and non-https protocols stay disabled. Indexing only ever
/// reads immutable objects of the pinned commit.
pub(crate) async fn index_git(repo_dir: &Path, args: &[&str], token: &str) -> Result<Vec<u8>> {
    index_git_args(repo_dir, args, token).await
}

/// `index_git` for arguments that need not be UTF-8, such as repository paths
/// passed as pathspecs.
async fn index_git_args<S>(repo_dir: &Path, args: &[S], token: &str) -> Result<Vec<u8>>
where
    S: AsRef<OsStr> + std::fmt::Debug,
{
    use base64::Engine;
    let mut cmd = tokio::process::Command::new("git");
    cmd.kill_on_drop(true);
    cmd.arg("--no-pager")
        .arg("--no-replace-objects")
        .arg("-C")
        .arg(repo_dir)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "protocol.allow=never",
            "-c",
            "protocol.https.allow=always",
            "-c",
            "submodule.recurse=false",
        ])
        .args(args);
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()))
        .env("HOME", repo_dir)
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ALLOW_PROTOCOL", "https")
        .env("GIT_LFS_SKIP_SMUDGE", "1");

    // Supply the token for lazy blob fetches, scoped to github.com, via env
    // config only (never argv/disk), matching the clone path.
    if !token.is_empty() {
        let auth = base64::engine::general_purpose::STANDARD
            .encode(format!("x-access-token:{token}"));
        cmd.env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.https://github.com/.extraHeader")
            .env("GIT_CONFIG_VALUE_0", format!("Authorization: Basic {auth}"));
    }

    let out = cmd
        .output()
        .await
        .map_err(|_| err(Code::Source, "git invocation failed"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        tracing::debug!(args = ?args, "index git failed: {}", stderr.trim());
        return Err(err(Code::Source, "git command returned an error"));
    }
    Ok(out.stdout)
}

/// Compute the diff between base and head. Both commits/trees are present in
/// the partial clone; the changed-file blobs are lazy-fetched from the promisor
/// remote via `index_git` (token-scoped https), since the clone omits blobs.
pub async fn diff(repo_dir: &Path, base_sha: &str, head_sha: &str, token: &str) -> Result<DiffSummary> {
    let raw = index_git(
        repo_dir,
        &[
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--find-renames",
            "--unified=3",
            base_sha,
            head_sha,
        ],
        token,
    )
    .await?;
    let text = String::from_utf8_lossy(&raw);
    parse_unified_diff(&text)
}

/// Upper bound on pathspec bytes passed to git, far below the OS argv limit.
const MAX_PATHSPEC_BYTES: usize = 256 * 1024;

/// The review diff with the repository's exclusion policy applied. Returns the
/// diff of the kept paths and how many changed paths the policy excluded.
/// Without a policy, or when it excludes nothing, this is exactly `diff`.
///
/// Changed paths are listed from the trees alone (`--no-renames` reads no
/// blob), filtered, and only the kept paths are content-diffed. Excluded files
/// are therefore never fetched and never count toward `MAX_DIFF_FILES`. Rename
/// detection still pairs kept paths; a rename across the policy boundary shows
/// up as an addition or deletion of the kept side.
pub async fn diff_with_policy(
    repo_dir: &Path,
    base_sha: &str,
    head_sha: &str,
    token: &str,
    policy: Option<&IgnorePolicy>,
) -> Result<(DiffSummary, usize)> {
    let Some(policy) = policy else {
        return Ok((diff(repo_dir, base_sha, head_sha, token).await?, 0));
    };
    let names = index_git(
        repo_dir,
        &["diff", "--name-only", "-z", "--no-renames", "--no-ext-diff", base_sha, head_sha],
        token,
    )
    .await?;
    let mut kept: Vec<&[u8]> = Vec::new();
    let mut excluded = 0;
    for path in names.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        if policy.excluded(&String::from_utf8_lossy(path)) {
            excluded += 1;
        } else {
            kept.push(path);
        }
    }
    if excluded == 0 {
        return Ok((diff(repo_dir, base_sha, head_sha, token).await?, 0));
    }
    // Never run the content diff without a pathspec here: that would diff
    // everything, excluded files included.
    if kept.is_empty() {
        return Ok((DiffSummary { files: Vec::new() }, excluded));
    }
    let mut args: Vec<OsString> = [
        "--literal-pathspecs",
        "diff",
        "--no-color",
        "--no-ext-diff",
        "--find-renames",
        "--unified=3",
        base_sha,
        head_sha,
        "--",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    let mut bytes = 0;
    for path in kept.iter().take(MAX_DIFF_FILES) {
        bytes += path.len() + 1;
        if bytes > MAX_PATHSPEC_BYTES {
            tracing::warn!("review scope truncated at the pathspec size limit");
            break;
        }
        args.push(os_path(path));
    }
    let raw = index_git_args(repo_dir, &args, token).await?;
    Ok((parse_unified_diff(&String::from_utf8_lossy(&raw))?, excluded))
}

/// A repository path as git reported it, byte for byte.
#[cfg(unix)]
fn os_path(path: &[u8]) -> OsString {
    use std::os::unix::ffi::OsStringExt;
    OsString::from_vec(path.to_vec())
}

#[cfg(not(unix))]
fn os_path(path: &[u8]) -> OsString {
    OsString::from(String::from_utf8_lossy(path).into_owned())
}

/// Parse a unified diff into structured changed files. Binary files and
/// oversized hunks are marked `unknown` with an empty excerpt.
fn parse_unified_diff(text: &str) -> Result<DiffSummary> {
    let mut files: Vec<ChangedFile> = Vec::new();
    let mut current: Option<ChangedFile> = None;
    let mut hunk_buf = String::new();

    let flush = |current: &mut Option<ChangedFile>, hunk_buf: &mut String| {
        if let Some(mut f) = current.take() {
            f.hunk_text = std::mem::take(hunk_buf);
            if f.hunk_text.len() > MAX_FILE_HUNK_BYTES {
                f.hunk_text.truncate(cut_boundary(&f.hunk_text, MAX_FILE_HUNK_BYTES));
                f.unknown = true;
            }
            f.excerpt = build_excerpt(&f.hunk_text);
            f
        } else {
            return;
        };
    };

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            // Finalize the previous file.
            if let Some(mut f) = current.take() {
                f.hunk_text = std::mem::take(&mut hunk_buf);
                f.excerpt = build_excerpt(&f.hunk_text);
                files.push(f);
                if files.len() >= MAX_DIFF_FILES {
                    break;
                }
            }
            let (a, b) = parse_git_header_paths(rest);
            current = Some(ChangedFile {
                kind: "modified".into(),
                path: b,
                base_path: a,
                edits: Vec::new(),
                unknown: false,
                hunk_text: String::new(),
                excerpt: String::new(),
            });
            hunk_buf.clear();
            continue;
        }
        let Some(f) = current.as_mut() else { continue };

        if line.starts_with("new file mode") {
            f.kind = "added".into();
            f.base_path.clear();
        } else if line.starts_with("deleted file mode") {
            f.kind = "deleted".into();
            f.path = f.base_path.clone();
        } else if line.starts_with("rename from ") || line.starts_with("rename to ") {
            f.kind = "renamed".into();
        } else if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
            f.unknown = true;
        } else if let Some(h) = line.strip_prefix("@@ ") {
            if let Some(edit) = parse_hunk_header(h) {
                f.edits.push(edit);
            }
            push_bounded(&mut hunk_buf, line);
        } else if line.starts_with("+++") || line.starts_with("---") {
            // header lines; keep in hunk text for readability but not required
            push_bounded(&mut hunk_buf, line);
        } else if line.starts_with('+') || line.starts_with('-') || line.starts_with(' ') {
            push_bounded(&mut hunk_buf, line);
        }
    }
    // Finalize the last file.
    if let Some(mut f) = current.take() {
        f.hunk_text = hunk_buf;
        if f.hunk_text.len() > MAX_FILE_HUNK_BYTES {
            let n = cut_boundary(&f.hunk_text, MAX_FILE_HUNK_BYTES);
            f.hunk_text.truncate(n);
            f.unknown = true;
        }
        f.excerpt = build_excerpt(&f.hunk_text);
        if files.len() < MAX_DIFF_FILES {
            files.push(f);
        }
    }
    let _ = flush; // silence unused in case of future refactor

    Ok(DiffSummary { files })
}

/// Parse the `a/path b/path` portion of a `diff --git` header.
fn parse_git_header_paths(rest: &str) -> (String, String) {
    // Format: a/old b/new (quoting aside). Best-effort split on " b/".
    if let Some(idx) = rest.find(" b/") {
        let a = rest[..idx].trim_start_matches("a/").to_string();
        let b = rest[idx + 3..].to_string();
        return (a, b);
    }
    (String::new(), rest.to_string())
}

/// Parse `@@ -a,b +c,d @@` into a LineEdit. Counts default to 1 when omitted.
fn parse_hunk_header(h: &str) -> Option<LineEdit> {
    let end = h.find("@@")?;
    let spec = &h[..end];
    let mut parts = spec.split_whitespace();
    let minus = parts.next()?; // -a,b
    let plus = parts.next()?; // +c,d
    let (bs, bl) = parse_range(minus.strip_prefix('-')?);
    let (ts, tl) = parse_range(plus.strip_prefix('+')?);
    Some(LineEdit {
        base_start: bs,
        base_lines: bl,
        target_start: ts,
        target_lines: tl,
    })
}

fn parse_range(s: &str) -> (u32, u32) {
    let mut it = s.split(',');
    let start = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let lines = it.next().and_then(|v| v.parse().ok()).unwrap_or(1);
    (start, lines)
}

fn push_bounded(buf: &mut String, line: &str) {
    if buf.len() + line.len() + 1 <= MAX_FILE_HUNK_BYTES {
        buf.push_str(line);
        buf.push('\n');
    }
}

/// Build a query excerpt: added/removed lines without the +/- markers, bounded.
fn build_excerpt(hunk: &str) -> String {
    let mut out = String::new();
    for line in hunk.lines() {
        if line.starts_with("+++") || line.starts_with("---") || line.starts_with("@@") {
            continue;
        }
        if let Some(rest) = line.strip_prefix('+').or_else(|| line.strip_prefix('-')) {
            if out.len() + rest.len() + 1 > 6000 {
                break;
            }
            out.push_str(rest);
            out.push('\n');
        }
    }
    out
}

fn cut_boundary(s: &str, max: usize) -> usize {
    let mut n = max.min(s.len());
    while n > 0 && !s.is_char_boundary(n) {
        n -= 1;
    }
    n
}

/// Read a line range of a file at a pinned commit (used by the RAG verifier).
pub async fn read_range(
    repo_dir: &Path,
    sha: &str,
    path: &str,
    start: u32,
    end: u32,
    token: &str,
) -> Result<Option<String>> {
    // `git show sha:path` then slice lines. Fails closed on any error. Uses the
    // promisor-aware reader so the blob can be lazy-fetched if omitted.
    let content = match crate::retrieval::indexer::read_blob(repo_dir, sha, path, token).await? {
        Some(c) => c,
        None => return Ok(None),
    };
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    if start < 1 || (end as usize) > lines.len() || end < start {
        return Ok(None);
    }
    let slice: String = lines[(start as usize - 1)..(end as usize)].concat();
    Ok(Some(slice))
}
