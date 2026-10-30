//! Step 1: clone the repository under review into an isolated sandbox.
//!
//! Two modes, both hardened:
//!   * Host mode (`containerize = false`): a fresh 0700 temp dir, a bare clone
//!     of exactly the two pinned SHAs, no checkout, no hooks, no submodules,
//!     https-only, LFS smudge disabled, credential passed via an in-process
//!     `http.extraHeader` config value (never in argv or on disk), and a disk
//!     watchdog that aborts past the configured size.
//!   * Container mode (`containerize = true`): the same clone performed inside
//!     a `--network=none --read-only --cap-drop=ALL` container whose only
//!     network path is a host-mediated relay to github.com. (Relay wiring is
//!     out of scope for this skeleton; host mode is the default and is fully
//!     implemented here.)
//!
//! The installation token is read from the environment
//! (INKSPECT_GITHUB_READ_TOKEN) and is never logged, placed in argv, or written
//! to a config file. It is injected through a `GIT_CONFIG_*` env var as an
//! `Authorization: Basic` header scoped to https://github.com/.

use base64::Engine;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::{ReviewJob, SandboxConfig};
use crate::error::{err, Code, Result};

/// A live clone with its working directory. Dropped => directory removed.
pub struct Clone {
    pub dir: PathBuf,
    _guard: tempfile::TempDir,
}

impl Clone {
    pub fn path(&self) -> &Path {
        &self.dir
    }
}

/// Clone the base and head commits of the PR into an isolated bare repo.
pub async fn clone_pr(cfg: &SandboxConfig, job: &ReviewJob, token: &str) -> Result<Clone> {
    if cfg.containerize {
        // Container mode requires the relay and an operator-provided image id.
        // Not wired in this skeleton; surface a clear, coded error so an
        // operator does not silently fall back to host mode.
        return Err(err(
            Code::Container,
            "container clone mode is not yet implemented in the rust engine",
        ));
    }

    let guard = tempfile::Builder::new()
        .prefix("inkspect-rust-clone-")
        .tempdir()
        .map_err(|_| err(Code::Source, "could not create clone dir"))?;
    let dir = guard.path().to_path_buf();

    // Credential: Authorization: Basic base64("x-access-token:<token>"), scoped
    // to github.com via http.<url>.extraHeader, delivered through env only.
    let auth = base64::engine::general_purpose::STANDARD
        .encode(format!("x-access-token:{token}"));

    let base_args: Vec<String> = vec![
        "--no-pager".into(),
        "--no-replace-objects".into(),
        "-c".into(),
        "core.hooksPath=/dev/null".into(),
        "-c".into(),
        "credential.helper=".into(),
        "-c".into(),
        "http.followRedirects=false".into(),
        "-c".into(),
        "protocol.allow=never".into(),
        "-c".into(),
        "protocol.https.allow=always".into(),
        "-c".into(),
        "submodule.recurse=false".into(),
        "-c".into(),
        "gc.auto=0".into(),
    ];

    run_git(&dir, &auth, &base_args, &["init", "--bare", "--template=", "."]).await?;

    // A named `origin` remote is required so the partial clone records a
    // promisor remote and can lazy-fetch omitted blobs later.
    let remote = format!("https://github.com/{}/{}.git", job.owner, job.repo);
    run_git(&dir, &auth, &base_args, &["remote", "add", "origin", &remote]).await?;

    // Disk watchdog: abort if the on-disk size exceeds the configured cap.
    let watch_dir = dir.clone();
    let cap_bytes = cfg.clone_mib * 1024 * 1024;
    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
    let watcher = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut stop_rx => return Ok(()),
                _ = tokio::time::sleep(Duration::from_millis(250)) => {
                    if dir_size(&watch_dir) > cap_bytes {
                        return Err(());
                    }
                }
            }
        }
    });

    let fetch = async {
        // Partial clone: fetch commits + trees but omit blobs. Blobs are
        // lazy-fetched on demand (diff hunks, indexed files), so huge binary
        // assets are never downloaded unless a text file actually needs them.
        let fetch_args = [
            "fetch",
            "--quiet",
            "--depth=1",
            "--no-tags",
            "--no-recurse-submodules",
            "--filter=blob:none",
            "origin",
            &job.base_sha,
            &job.head_sha,
        ];
        run_git(&dir, &auth, &base_args, &fetch_args).await
    };

    let fetch_res = tokio::time::timeout(Duration::from_secs(900), fetch).await;
    let _ = stop_tx.send(());
    if let Ok(Err(())) = watcher.await {
        return Err(err(Code::Source, "clone exceeded size limit"));
    }
    match fetch_res {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err(err(Code::Source, "clone timed out")),
    }

    Ok(Clone {
        dir: guard.path().to_path_buf(),
        _guard: guard,
    })
}

/// Run git with the token injected via GIT_CONFIG_* env (never argv/disk).
async fn run_git(dir: &Path, auth: &str, base_args: &[String], args: &[&str]) -> Result<()> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.kill_on_drop(true);
    cmd.current_dir(dir);
    for a in base_args {
        cmd.arg(a);
    }
    for a in args {
        cmd.arg(a);
    }
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()))
        .env("HOME", dir)
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ALLOW_PROTOCOL", "https")
        .env("GIT_LFS_SKIP_SMUDGE", "1")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "http.https://github.com/.extraHeader")
        .env("GIT_CONFIG_VALUE_0", format!("Authorization: Basic {auth}"));
    // Discard output so a failing fetch cannot echo the token.
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());

    let status = cmd
        .status()
        .await
        .map_err(|_| err(Code::Source, "git invocation failed"))?;
    if !status.success() {
        return Err(err(Code::Source, "git clone step failed"));
    }
    Ok(())
}

fn dir_size(root: &Path) -> u64 {
    fn walk(p: &Path, acc: &mut u64) {
        if let Ok(rd) = std::fs::read_dir(p) {
            for entry in rd.flatten() {
                let path = entry.path();
                match entry.file_type() {
                    Ok(ft) if ft.is_dir() => walk(&path, acc),
                    Ok(ft) if ft.is_file() => {
                        if let Ok(m) = entry.metadata() {
                            *acc += m.len();
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    let mut total = 0;
    walk(root, &mut total);
    total
}
