//! GitHub REST client: PR snapshot re-verification and inline PR comments.
//!
//! The inline-comment capability is the feature the Go engine lacks: after the
//! review, each eligible finding is posted as a review comment anchored to the
//! changed line via the Pull Request Reviews API.
//!
//! Auth: a short-lived installation token from INKSPECT_GITHUB_READ_TOKEN for
//! reads; publishing requires a token with `pull_requests:write`
//! (INKSPECT_GITHUB_WRITE_TOKEN). The origin is pinned to api.github.com and
//! only `/repos/...` paths are allowed. Redirects are refused. Tokens are never
//! logged.

use serde::Deserialize;

use crate::config::ReviewJob;
use crate::domain::Finding;
use crate::error::{err, Code, Result};

const API_ORIGIN: &str = "https://api.github.com";

pub struct GitHub {
    client: reqwest::Client,
    read_token: String,
    write_token: Option<String>,
}

/// The resolved, pinned PR snapshot.
pub struct PrSnapshot {
    /// The PR's destination branch tip. Retained for diagnostics/parity with
    /// the Go snapshot; the review compares merge_base..head.
    #[allow(dead_code)]
    pub base_sha: String,
    pub head_sha: String,
    pub merge_base_sha: String,
    /// PR title and body as written by the author. UNTRUSTED.
    pub title: String,
    pub body: String,
}

impl GitHub {
    /// Build with explicit read and (optional) write tokens. Used when a
    /// GitHub App installation token has been minted for the run.
    pub fn with_tokens(read_token: String, write_token: Option<String>) -> Result<GitHub> {
        if read_token.is_empty() || read_token.contains(['\r', '\n']) {
            return Err(err(Code::GitHub, "invalid read token"));
        }
        let write_token = write_token.filter(|t| !t.is_empty() && !t.contains(['\r', '\n']));
        let client = reqwest::Client::builder()
            .user_agent("Inkspect-Rust-Engine")
            .timeout(std::time::Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| err(Code::GitHub, "client build failed"))?;
        Ok(GitHub {
            client,
            read_token,
            write_token,
        })
    }

    #[allow(dead_code)]
    pub fn from_env() -> Result<GitHub> {
        let read_token = std::env::var("INKSPECT_GITHUB_READ_TOKEN")
            .map_err(|_| err(Code::GitHub, "missing read token"))?;
        let write_token = std::env::var("INKSPECT_GITHUB_WRITE_TOKEN").ok();
        Self::with_tokens(read_token, write_token)
    }

    fn get_builder(&self, path: &str) -> Result<reqwest::RequestBuilder> {
        if !path.starts_with("/repos/") || path.contains(['\\', '\r', '\n']) {
            return Err(err(Code::GitHub, "disallowed path"));
        }
        Ok(self
            .client
            .get(format!("{API_ORIGIN}{path}"))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .bearer_auth(&self.read_token))
    }

    /// Re-resolve the PR and confirm the pinned snapshot still matches. Fails
    /// with GitHubChanged if base/head moved since the job was queued.
    pub async fn verify_snapshot(&self, job: &ReviewJob) -> Result<PrSnapshot> {
        let pull: Pull = self
            .get_builder(&format!(
                "/repos/{}/{}/pulls/{}",
                job.owner, job.repo, job.pull_number
            ))?
            .send()
            .await
            .map_err(|_| err(Code::GitHub, "pull fetch failed"))?
            .error_for_status()
            .map_err(|_| err(Code::GitHub, "pull not accessible"))?
            .json()
            .await
            .map_err(|_| err(Code::GitHub, "pull decode failed"))?;

        if pull.head.sha != job.head_sha {
            return Err(err(Code::GitHubChanged, "pull head changed"));
        }

        // Resolve the merge base from the compare API, as the Go engine does.
        let cmp: Compare = self
            .get_builder(&format!(
                "/repos/{}/{}/compare/{}...{}?per_page=1&page=1",
                job.owner, job.repo, pull.base.sha, pull.head.sha
            ))?
            .send()
            .await
            .map_err(|_| err(Code::GitHub, "compare fetch failed"))?
            .error_for_status()
            .map_err(|_| err(Code::GitHub, "compare not accessible"))?
            .json()
            .await
            .map_err(|_| err(Code::GitHub, "compare decode failed"))?;

        if cmp.merge_base_commit.sha != job.base_sha {
            return Err(err(Code::GitHubChanged, "merge base changed"));
        }

        Ok(PrSnapshot {
            base_sha: pull.base.sha,
            head_sha: pull.head.sha,
            merge_base_sha: cmp.merge_base_commit.sha,
            title: pull.title.unwrap_or_default(),
            body: pull.body.unwrap_or_default(),
        })
    }

    /// Publish findings as inline review comments on the PR. Only findings
    /// whose location sits on a right-side (added) line of the PR patch are
    /// posted inline; the rest are returned so the caller can include them in
    /// the summary body. Returns the number of inline comments posted.
    pub async fn publish_inline(
        &self,
        job: &ReviewJob,
        findings: &[Finding],
        body_markdown: &str,
    ) -> Result<usize> {
        let write = self
            .write_token
            .as_ref()
            .ok_or_else(|| err(Code::Publish, "no write token configured"))?;

        // Determine which lines are commentable (right side of the diff).
        let patch_lines = self.pr_patch_right_lines(job).await?;

        let mut comments = Vec::new();
        for f in findings {
            let line = f.location.end_line;
            let key = (f.location.path.clone(), line);
            if !patch_lines.contains(&key) {
                continue; // not on a changed right-side line; skip inline
            }
            comments.push(serde_json::json!({
                "path": f.location.path,
                "line": line,
                "side": "RIGHT",
                // Dedup marker keeps re-reviews from duplicating comments.
                "body": format!(
                    "<!-- inkspect:{}:{} -->\n{}",
                    job.run_id,
                    f.id,
                    crate::render::finding_comment(f),
                ),
            }));
        }

        let posted = comments.len();
        // The summary body is the already-rendered Markdown report (including
        // the Mermaid diagram), produced by crate::render. It is engine-authored
        // and GitHub renders the fenced ```mermaid block natively.
        let body = serde_json::json!({
            "commit_id": job.head_sha,
            "event": "COMMENT",
            "body": body_markdown,
            "comments": comments,
        });

        let path = format!(
            "/repos/{}/{}/pulls/{}/reviews",
            job.owner, job.repo, job.pull_number
        );
        self.client
            .post(format!("{API_ORIGIN}{path}"))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .bearer_auth(write)
            .json(&body)
            .send()
            .await
            .map_err(|_| err(Code::Publish, "review post failed"))?
            .error_for_status()
            .map_err(|_| err(Code::Publish, "review rejected"))?;

        Ok(posted)
    }

    /// Fetch the PR's changed files and parse which (path, right-line) pairs
    /// are present in the patch, so inline comments anchor to real diff lines.
    async fn pr_patch_right_lines(
        &self,
        job: &ReviewJob,
    ) -> Result<std::collections::HashSet<(String, u32)>> {
        let files: Vec<PrFile> = self
            .get_builder(&format!(
                "/repos/{}/{}/pulls/{}/files?per_page=100",
                job.owner, job.repo, job.pull_number
            ))?
            .send()
            .await
            .map_err(|_| err(Code::GitHub, "files fetch failed"))?
            .error_for_status()
            .map_err(|_| err(Code::GitHub, "files not accessible"))?
            .json()
            .await
            .map_err(|_| err(Code::GitHub, "files decode failed"))?;

        let mut set = std::collections::HashSet::new();
        for file in files {
            let Some(patch) = file.patch else { continue };
            for (path, line) in right_lines(&file.filename, &patch) {
                set.insert((path, line));
            }
        }
        Ok(set)
    }
}

/// Parse `@@ ... +c,d @@` hunks and yield (path, right_line) for each added or
/// context line on the right side (skipping `-` removals).
fn right_lines(path: &str, patch: &str) -> Vec<(String, u32)> {
    let mut out = Vec::new();
    let mut right = 0u32;
    for line in patch.lines() {
        if let Some(h) = line.strip_prefix("@@ ") {
            if let Some(plus) = h.split('+').nth(1) {
                let start: u32 = plus
                    .split([',', ' '])
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                right = start;
            }
            continue;
        }
        match line.chars().next() {
            Some('+') => {
                out.push((path.to_string(), right));
                right += 1;
            }
            Some('-') | Some('\\') => {}
            _ => {
                right += 1;
            }
        }
    }
    out
}

#[derive(Deserialize)]
struct Pull {
    head: Commitish,
    base: Commitish,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
}

#[derive(Deserialize)]
struct Commitish {
    sha: String,
}

#[derive(Deserialize)]
struct Compare {
    merge_base_commit: Commitish,
}

#[derive(Deserialize)]
struct PrFile {
    filename: String,
    #[serde(default)]
    patch: Option<String>,
}
