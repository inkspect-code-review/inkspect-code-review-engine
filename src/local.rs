//! Read-only local reviews. Never fetch, stage, check out, or execute project code.
use crate::{
    config::{HostConfig, ReviewJob},
    domain::{Report, SourceRevision},
    error::{err, Code, Result},
    git::{ChangedFile, DiffSummary, LineEdit},
    ignore::IgnorePolicy,
    model::ReviewModel,
    prompts,
    retrieval::indexer,
    review,
};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, path::Path};
use tokio::process::Command;

const FILE_LIMIT: usize = 128_000;
const TOTAL_LIMIT: usize = 512_000;
const FILE_COUNT: usize = 200;

fn failure(message: &str) -> crate::error::EngineError {
    err(Code::Source, message)
}
fn git(root: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.kill_on_drop(true)
        .arg("--no-pager")
        .arg("--no-replace-objects")
        .arg("--literal-pathspecs")
        .arg("-C")
        .arg(root)
        // The user explicitly selected this root; trust only it, not all repos.
        .arg("-c")
        .arg(format!("safe.directory={}", root.display()))
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.untrackedCache=false",
            "-c",
            "protocol.allow=never",
            "-c",
            "submodule.recurse=false",
        ])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_ALLOW_PROTOCOL", "");
    cmd
}
async fn command(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = git(root)
        .args(args)
        .output()
        .await
        .map_err(|_| failure("could not run Git; install Git and retry"))?;
    if !out.status.success() {
        return Err(failure(
            "local Git read failed; check the base revision and available objects",
        ));
    }
    if out.stdout.len() > 8 * 1024 * 1024 {
        return Err(failure("local repository listing is too large"));
    }
    Ok(out.stdout)
}
fn safe_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 512
        && !path.contains(['\\', ':'])
        && !path.chars().any(char::is_control)
        && path
            .split('/')
            .all(|p| !p.is_empty() && p != ".." && !p.starts_with('.'))
        && indexer::eligible(path)
}
fn walk(root: &Path, dir: &Path, paths: &mut BTreeSet<String>, visited: &mut usize) -> Result<()> {
    for item in std::fs::read_dir(dir).map_err(|_| failure("cannot read local directory"))? {
        *visited += 1;
        if *visited > 100_000 {
            return Err(failure(
                "local folder is too large; narrow its review scope",
            ));
        }
        let item = item.map_err(|_| failure("cannot read directory entry"))?;
        let path = item.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|_| failure("invalid local path"))?
            .to_str()
            .ok_or_else(|| failure("non-UTF-8 file path is unsupported"))?
            .replace(std::path::MAIN_SEPARATOR, "/");
        let kind = item
            .file_type()
            .map_err(|_| failure("cannot inspect local file"))?;
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            // Reuse generated/sensitive directory filtering before descending.
            if safe_path(&format!("{relative}/scope.rs")) {
                walk(root, &path, paths, visited)?;
            }
        } else if kind.is_file() && safe_path(&relative) {
            paths.insert(relative);
        }
    }
    Ok(())
}
fn regular_file(root: &Path, relative: &str, limit: usize) -> Result<Option<Vec<u8>>> {
    let mut path = root.to_path_buf();
    for component in relative.split('/') {
        path.push(component);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => return Ok(None),
            Ok(_) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(failure("cannot inspect local file")),
        }
    }
    let meta = std::fs::metadata(&path).map_err(|_| failure("cannot inspect local file"))?;
    if !meta.is_file() {
        return Ok(None);
    }
    if meta.len() > limit as u64 {
        return Err(failure(
            "a selected file is too large; exclude it with .inkspectignore",
        ));
    }
    let resolved = path
        .canonicalize()
        .map_err(|_| failure("cannot resolve local file"))?;
    if !resolved.starts_with(root) {
        return Err(failure("local file escaped the review root"));
    }
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(resolved)
        .map_err(|_| failure("cannot open local file"))?
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| failure("cannot read local file"))?;
    if bytes.len() > limit {
        return Err(failure("local file changed beyond the size limit"));
    }
    Ok(Some(bytes))
}
async fn base_file(root: &Path, revision: &str, path: &str) -> Result<Option<Vec<u8>>> {
    let entry = command(root, &["ls-tree", "-z", revision, "--", path]).await?;
    if entry.is_empty() {
        return Ok(None);
    }
    let entry = std::str::from_utf8(&entry).map_err(|_| failure("invalid base tree"))?;
    let metadata = entry.split('\t').next().unwrap_or("");
    let fields: Vec<_> = metadata.split(' ').collect();
    if fields.len() != 3 || !matches!(fields[0], "100644" | "100755") || fields[1] != "blob" {
        return Ok(None);
    }
    let size = command(root, &["cat-file", "-s", fields[2]]).await?;
    let size: usize = std::str::from_utf8(&size)
        .unwrap_or("")
        .trim()
        .parse()
        .map_err(|_| failure("invalid base file size"))?;
    if size > FILE_LIMIT {
        return Err(failure(
            "a base file is too large; exclude it with .inkspectignore",
        ));
    }
    Ok(Some(command(root, &["cat-file", "blob", fields[2]]).await?))
}
fn text(bytes: Option<Vec<u8>>) -> Result<Option<String>> {
    let Some(bytes) = bytes else { return Ok(None) };
    if bytes.contains(&0) {
        return Err(failure(
            "selected source is binary; exclude it with .inkspectignore",
        ));
    }
    let value = String::from_utf8(bytes)
        .map_err(|_| failure("selected source is not UTF-8; exclude it with .inkspectignore"))?;
    if indexer::contains_secret(&value) {
        return Err(failure(
            "selected source contains a possible secret; exclude it before review",
        ));
    }
    Ok(Some(value))
}
async fn change(path: &str, before: Option<&str>, after: Option<&str>) -> Result<ChangedFile> {
    let old = before.unwrap_or("");
    let new = after.unwrap_or("");
    // Diff only captured regular files in a private directory, never user filters.
    let temp = tempfile::tempdir().map_err(|_| failure("cannot create temporary diff"))?;
    std::fs::write(temp.path().join("before"), old)
        .map_err(|_| failure("cannot write temporary diff"))?;
    std::fs::write(temp.path().join("after"), new)
        .map_err(|_| failure("cannot write temporary diff"))?;
    let output = git(temp.path())
        .args([
            "diff",
            "--no-index",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--unified=3",
            "--",
            "before",
            "after",
        ])
        .output()
        .await
        .map_err(|_| failure("cannot diff local source"))?;
    if !matches!(output.status.code(), Some(0 | 1)) {
        return Err(failure("local diff failed"));
    }
    let diff = String::from_utf8(output.stdout).map_err(|_| failure("invalid local diff"))?;
    let mut edits = Vec::new();
    let mut hunk = String::new();
    let mut in_hunk = false;
    for line in diff.lines() {
        if let Some(header) = line.strip_prefix("@@ ") {
            let spec = header.split("@@").next().unwrap_or("");
            let mut ranges = spec.split_whitespace();
            let parse = |value: &str| -> Result<(u32, u32)> {
                let mut parts = value[1..].split(',');
                Ok((
                    parts
                        .next()
                        .unwrap_or("")
                        .parse()
                        .map_err(|_| failure("invalid local hunk"))?,
                    parts
                        .next()
                        .unwrap_or("1")
                        .parse()
                        .map_err(|_| failure("invalid local hunk"))?,
                ))
            };
            let (bs, bl) = parse(ranges.next().ok_or_else(|| failure("invalid local hunk"))?)?;
            let (ts, tl) = parse(ranges.next().ok_or_else(|| failure("invalid local hunk"))?)?;
            edits.push(LineEdit {
                base_start: bs,
                base_lines: bl,
                target_start: ts,
                target_lines: tl,
            });
            in_hunk = true;
        }
        if in_hunk {
            hunk.push_str(line);
            hunk.push('\n');
        }
    }
    Ok(ChangedFile {
        kind: if before.is_none() {
            "added"
        } else if after.is_none() {
            "deleted"
        } else {
            "modified"
        }
        .into(),
        path: path.into(),
        base_path: if before.is_some() {
            path.into()
        } else {
            String::new()
        },
        edits,
        unknown: false,
        excerpt: hunk.clone(),
        hunk_text: hunk,
    })
}
pub struct Snapshot {
    pub diff: DiffSummary,
    pub target: SourceRevision,
    pub base: SourceRevision,
    pub skipped: usize,
}
pub async fn snapshot(path: &Path, base: Option<&str>) -> Result<Snapshot> {
    let root = path
        .canonicalize()
        .map_err(|_| failure("local folder does not exist"))?;
    if !root.is_dir() {
        return Err(failure("--repo must point to a directory"));
    }
    let has_git = root.join(".git").exists();
    if base.is_some() && !has_git {
        return Err(failure("--base requires a Git working-tree root"));
    }
    if root
        .join(".inkspectignore")
        .symlink_metadata()
        .is_ok_and(|m| !m.file_type().is_file())
    {
        return Err(failure(".inkspectignore must be a regular file"));
    }
    let policy = match regular_file(&root, ".inkspectignore", 32 * 1024)? {
        Some(bytes) => Some(IgnorePolicy::parse(&bytes)?),
        None => None,
    };
    // A symlink policy must not silently disable exclusions.
    if root
        .join(".inkspectignore")
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
    {
        return Err(failure(".inkspectignore must be a regular file"));
    }
    let mut paths = BTreeSet::new();
    let revision = if has_git {
        let requested = base.unwrap_or("HEAD");
        let output = git(&root)
            .args([
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{requested}^{{commit}}"),
            ])
            .output()
            .await
            .map_err(|_| failure("cannot resolve local base"))?;
        if output.status.success() {
            let sha = String::from_utf8(output.stdout)
                .map_err(|_| failure("invalid base"))?
                .trim()
                .to_string();
            let names = command(
                &root,
                &[
                    "diff",
                    "--name-only",
                    "-z",
                    "--no-renames",
                    "--no-ext-diff",
                    "--no-textconv",
                    &sha,
                    "--",
                ],
            )
            .await?;
            add_paths(&mut paths, &names)?;
            Some(sha)
        } else if base.is_some() {
            return Err(failure("base must name an existing local commit or branch"));
        } else {
            None
        }
    } else {
        None
    };
    if has_git {
        let args = if revision.is_some() {
            vec!["ls-files", "--others", "--exclude-standard", "-z"]
        } else {
            vec![
                "ls-files",
                "--cached",
                "--others",
                "--exclude-standard",
                "-z",
            ]
        };
        add_paths(&mut paths, &command(&root, &args).await?)?;
    } else {
        walk(&root, &root, &mut paths, &mut 0)?;
    }
    let mut files = Vec::new();
    let mut bytes = 0;
    let mut skipped = 0;
    let mut target_hash = Sha256::new();
    let mut base_hash = Sha256::new();
    for path in paths {
        if !safe_path(&path) || policy.as_ref().is_some_and(|p| p.excluded(&path)) {
            skipped += 1;
            continue;
        }
        // Skip any symlink component, including tracked links and submodules.
        let mut full = root.clone();
        let mut linked = false;
        for c in path.split('/') {
            full.push(c);
            if full
                .symlink_metadata()
                .is_ok_and(|m| m.file_type().is_symlink())
            {
                linked = true;
                break;
            }
        }
        if linked {
            skipped += 1;
            continue;
        }
        let before = text(match &revision {
            Some(sha) => base_file(&root, sha, &path).await?,
            None => None,
        })?;
        let after = text(regular_file(&root, &path, FILE_LIMIT)?)?;
        if before == after {
            continue;
        }
        bytes += before.as_ref().map_or(0, String::len) + after.as_ref().map_or(0, String::len);
        if bytes > TOTAL_LIMIT || files.len() >= FILE_COUNT {
            return Err(failure(
                "local scope is too large; narrow the folder or add .inkspectignore exclusions",
            ));
        }
        for (hash, value) in [(&mut base_hash, &before), (&mut target_hash, &after)] {
            hash.update((path.len() as u64).to_be_bytes());
            hash.update(path.as_bytes());
            hash.update([u8::from(value.is_some())]);
            if let Some(v) = value {
                hash.update((v.len() as u64).to_be_bytes());
                hash.update(v.as_bytes());
            }
        }
        files.push(change(&path, before.as_deref(), after.as_deref()).await?);
    }
    let target = SourceRevision {
        tenant_id: "local".into(),
        provider: "local".into(),
        provider_instance: "filesystem".into(),
        repository_id: hex::encode(Sha256::digest(root.to_string_lossy().as_bytes())),
        revision_id: format!("sha256:{}", hex::encode(target_hash.finalize())),
    };
    let mut base = target.clone();
    base.revision_id =
        revision.unwrap_or_else(|| format!("sha256:{}", hex::encode(base_hash.finalize())));
    Ok(Snapshot {
        diff: DiffSummary { files },
        target,
        base,
        skipped,
    })
}
fn add_paths(paths: &mut BTreeSet<String>, names: &[u8]) -> Result<()> {
    for path in names.split(|b| *b == 0).filter(|b| !b.is_empty()) {
        paths.insert(
            std::str::from_utf8(path)
                .map_err(|_| failure("non-UTF-8 file path is unsupported"))?
                .to_string(),
        );
    }
    Ok(())
}
pub async fn run(host: &HostConfig, path: &Path, base: Option<&str>) -> Result<Report> {
    let snapshot = snapshot(path, base).await?;
    let run_id = format!("L-{}", prompts::new_nonce());
    let mut report = if snapshot.diff.files.is_empty() {
        review::build_report(&run_id,&snapshot.target,&snapshot.base,"none","none",&snapshot.diff,serde_json::from_value(serde_json::json!({"summary":"No reviewable local changes were found.","decision":"ready_for_review","findings":[]})).unwrap())
    } else {
        let nonce = prompts::new_nonce();
        let system=format!("{}\nLOCAL REVIEW: There is no GitHub pull request. Assess only the supplied local snapshot. Files marked added may represent existing files in a folder without Git. Do not invent PR intent; omit change_alignment. Do not claim tests were run.",prompts::system_prompt(&nonce));
        let user = prompts::user_message(
            &nonce,
            "Local code review",
            "No pull request metadata is available.",
            &snapshot.diff,
            &snapshot.diff.files,
            &[],
            &serde_json::json!([]),
        );
        let limit = if host.model_max_request_bytes == 0 {
            1_500_000
        } else {
            host.model_max_request_bytes
        };
        let reserved = host.model_input_allowance as u128 * host.input_nano_usd as u128
            + host.model_max_output_tokens.max(0) as u128 * host.output_nano_usd as u128;
        if system.len() + user.len() + 4096 > limit
            || host.model_max_output_tokens <= 0
            || (host.budget_nano_usd > 0
                && (host.model_input_allowance == 0 || reserved > host.budget_nano_usd as u128))
        {
            return Err(failure("local review exceeds the configured model budget"));
        }
        let job = ReviewJob {
            run_id: run_id.clone(),
            owner: "local".into(),
            repo: "local".into(),
            repository_id: snapshot.target.repository_id.clone(),
            tenant_id: "local".into(),
            pull_number: 0,
            base_sha: snapshot.base.revision_id.clone(),
            head_sha: snapshot.target.revision_id.clone(),
            head_ref: String::new(),
            review_notes: vec![],
            publish_inline_comments: false,
            auto_review: false,
        };
        let model = ReviewModel::new(
            &host.model_region,
            &host.model_id,
            host.model_max_output_tokens,
        )
        .await?;
        let raw = model.review(&system, &user, &job).await?;
        review::build_report(
            &run_id,
            &snapshot.target,
            &snapshot.base,
            model.provider(),
            model.model_id(),
            &snapshot.diff,
            raw,
        )
    };
    report.change_alignment = None;
    report.presentation_notes.push("Local snapshot review. No GitHub access, repository commands, or tests were performed beyond read-only Git inspection. Repository indexing is not used in local mode.".into());
    if snapshot.skipped > 0 {
        report.presentation_notes.push(format!(
            "Skipped {} paths excluded by local scope rules.",
            snapshot.skipped
        ));
    }
    crate::render::attach(&mut report);
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn write(root: &Path, path: &str, value: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, value).unwrap();
    }
    async fn init(root: &Path) {
        command(root, &["init", "--initial-branch=main"])
            .await
            .unwrap();
        command(root, &["config", "user.name", "Fixture"])
            .await
            .unwrap();
        command(root, &["config", "user.email", "fixture@example.invalid"])
            .await
            .unwrap();
    }
    async fn commit(root: &Path) {
        command(root, &["add", "."]).await.unwrap();
        command(
            root,
            &["-c", "commit.gpgsign=false", "commit", "-m", "fixture"],
        )
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn folder_scope_exclusions_and_line_locations() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "src/a file.rs", "fn broken() {\n panic!();\n}\n");
        write(d.path(), "node_modules/pkg/a.js", "hidden");
        write(d.path(), ".env", "hidden");
        write(d.path(), "skip.rs", "skip");
        write(d.path(), ".inkspectignore", "skip.rs\n");
        let s = snapshot(d.path(), None).await.unwrap();
        assert_eq!(s.diff.files.len(), 1);
        assert_eq!(s.diff.files[0].path, "src/a file.rs");
        assert_eq!(s.diff.files[0].edits[0].target_lines, 3);
        assert_eq!(s.target.provider, "local");
        assert!(snapshot(d.path(), Some("main")).await.is_err());
    }
    #[tokio::test]
    async fn git_worktree_includes_staged_unstaged_untracked_and_deletions_without_mutation() {
        let d = tempfile::tempdir().unwrap();
        init(d.path()).await;
        write(d.path(), "a.rs", "fn a() {}\n");
        write(d.path(), "deleted.rs", "fn b() {}\n");
        commit(d.path()).await;
        write(d.path(), "a.rs", "fn a() { panic!(); }\n");
        command(d.path(), &["add", "a.rs"]).await.unwrap();
        write(d.path(), "new.rs", "fn new() {}\n");
        std::fs::remove_file(d.path().join("deleted.rs")).unwrap();
        write(d.path(), ".gitignore", "ignored.rs\n");
        write(d.path(), "ignored.rs", "hidden");
        let index = std::fs::read(d.path().join(".git/index")).unwrap();
        let s = snapshot(d.path(), None).await.unwrap();
        assert_eq!(s.diff.files.len(), 3);
        assert_eq!(index, std::fs::read(d.path().join(".git/index")).unwrap());
        assert!(s.diff.files.iter().any(|f| f.kind == "deleted"));
        assert!(s.diff.files.iter().any(|f| f.path == "new.rs"));
        commit(d.path()).await;
        assert!(snapshot(d.path(), None)
            .await
            .unwrap()
            .diff
            .files
            .is_empty());
        assert!(!snapshot(d.path(), Some("HEAD~1"))
            .await
            .unwrap()
            .diff
            .files
            .is_empty());
        assert!(snapshot(d.path(), Some("--output=/tmp/no")).await.is_err());
    }
    #[tokio::test]
    async fn unborn_git_and_empty_review_need_no_provider() {
        let d = tempfile::tempdir().unwrap();
        init(d.path()).await;
        write(d.path(), "a.rs", "fn a() {}\n");
        assert_eq!(snapshot(d.path(), None).await.unwrap().diff.files.len(), 1);
        std::fs::remove_file(d.path().join("a.rs")).unwrap();
        let h: HostConfig =
            serde_json::from_str(include_str!("../examples/host.example.json")).unwrap();
        let r = run(&h, d.path(), None).await.unwrap();
        assert!(r.findings.is_empty());
        assert_eq!(r.run.model.provider, "none");
        assert!(!r.presentation.unwrap().markdown.contains("/dashboard?"));
    }
    #[tokio::test]
    async fn secrets_and_oversized_files_fail_before_provider() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a.rs", "-----BEGIN PRIVATE KEY-----\nprivate\n");
        assert!(snapshot(d.path(), None).await.is_err());
        write(d.path(), "a.rs", &"a".repeat(FILE_LIMIT + 1));
        assert!(snapshot(d.path(), None).await.is_err());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn symlinks_are_not_followed() {
        let d = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write(outside.path(), "a.rs", "private");
        std::os::unix::fs::symlink(outside.path(), d.path().join("linked")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("a.rs"), d.path().join("a.rs")).unwrap();
        assert!(snapshot(d.path(), None)
            .await
            .unwrap()
            .diff
            .files
            .is_empty());
        std::os::unix::fs::symlink(
            outside.path().join("a.rs"),
            d.path().join(".inkspectignore"),
        )
        .unwrap();
        assert!(snapshot(d.path(), None).await.is_err());
    }
}
