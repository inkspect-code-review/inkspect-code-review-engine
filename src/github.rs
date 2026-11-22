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
use std::collections::HashMap;

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

        let patches = self.pr_patches(job).await?;
        let comments: Vec<_> = findings.iter().filter_map(|finding| {
            inline_comment(&job.run_id, finding, patches.get(&finding.location.path)?)
        }).collect();

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

    /// Read GitHub's patches to verify the entire selected range is commentable.
    async fn pr_patches(&self, job: &ReviewJob) -> Result<HashMap<String, String>> {
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

        Ok(files.into_iter().filter_map(|f| Some((f.filename, f.patch?))).collect())
    }
}

fn inline_comment(run_id: &str, finding: &Finding, patch: &str) -> Option<serde_json::Value> {
    let loc = &finding.location;
    crate::git::selected_lines(patch, loc.start_line, loc.end_line)?;
    let mut comment = serde_json::json!({
        "path": loc.path,
        "line": loc.end_line,
        "side": "RIGHT",
        "body": format!("<!-- inkspect:{}:{} -->\n{}", run_id, finding.id, crate::render::finding_comment(finding)),
    });
    if loc.start_line != loc.end_line {
        comment["start_line"] = loc.start_line.into();
        comment["start_side"] = "RIGHT".into();
    }
    Some(comment)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::SourceRevision;
    use crate::git::{ChangedFile, DiffSummary, LineEdit};
    use serde_json::{Value, json};

    const PATCH: &str = "@@ -8,2 +8,3 @@\n keep\n-old\n+    wrong();\n+    finish();\n@@ -20 +21 @@\n-old\n+later\n";

    fn review(start: u32, end: u32, fix: Value) -> crate::domain::Report {
        let diff = DiffSummary { files: vec![ChangedFile {
            kind: "modified".into(), path: "src/lib.rs".into(), base_path: "src/lib.rs".into(),
            edits: vec![LineEdit { base_start: 8, base_lines: 2, target_start: 8, target_lines: 3 }],
            unknown: false, hunk_text: PATCH.into(), excerpt: String::new(),
        }] };
        let mut finding = json!({"title":"Incorrect call","reason":"Incorrect call in this branch",
            "suggestion":"Correct the call","verification":"Check the result","severity":"P2",
            "path":"src/lib.rs","start_line":start,"end_line":end});
        if !fix.is_null() {
            finding["code_fix"] = fix;
        }
        let raw = crate::model::parse_review(&json!({
            "summary":"Review","decision":"needs_changes","findings":[finding]
        }).to_string()).unwrap();
        let rev = SourceRevision::default();
        let mut report = crate::review::build_report("test", &rev, &rev, "test", "test", &diff, raw);
        crate::render::attach(&mut report);
        report
    }

    #[test]
    fn concrete_fix_is_published_on_the_exact_selected_lines() {
        let report = review(9, 10, json!({"original":["    wrong();","    finish();"],
            "replacement":["\tcorrect();","    finish();",""]}));
        let finding = &report.findings[0];
        let presentation = report.presentation.as_ref().unwrap();
        let body = &presentation.github_findings[&finding.id];
        assert!(body.contains("\n\n```suggestion\n\tcorrect();\n    finish();\n\n```\n\n"));
        assert!(!presentation.github_summary.contains("```suggestion"));
        let comment = inline_comment("test", finding, PATCH).unwrap();
        assert_eq!(comment["body"], format!("<!-- inkspect:test:{} -->\n{body}", finding.id));
        assert_eq!(comment["start_line"], 9);
        assert_eq!(comment["line"], 10);
        assert_eq!(comment["start_side"], "RIGHT");
        assert_eq!(comment["side"], "RIGHT");
        assert!(inline_comment("test", finding, "@@ -8,2 +8,3 @@\n keep\n-old\n+    finish();\n").is_none());
    }

    #[test]
    fn single_line_edits_preserve_deletions_and_literal_fences() {
        for (replacement, expected) in [
            (json!([]), "```suggestion\n```"),
            (json!([""]), "```suggestion\n\n```"),
            (json!(["```","@name <tag>"]), "````suggestion\n```\n@name <tag>\n````"),
        ] {
            let report = review(9, 9, json!({"original":["    wrong();"], "replacement":replacement}));
            let comment = inline_comment("test", &report.findings[0], PATCH).unwrap();
            assert!(comment["body"].as_str().unwrap().contains(expected));
            assert_eq!(comment["line"], 9);
            assert!(comment.get("start_line").is_none() && comment.get("start_side").is_none());
        }
    }

    #[test]
    fn invalid_or_absent_edits_preserve_the_prose_finding() {
        for (end, fix) in [
            (9, Value::Null),
            (9, json!({"original":["wrong();"],"replacement":["fixed();"]})),
            (9, json!({"original":["    wrong();"],"replacement":["    wrong();"]})),
            (9, json!({"original":["    wrong();"],"replacement":"prose"})),
            (9, json!({"original":["    wrong();"],"replacement":["a\nb"]})),
            (9, json!({"original":["    wrong();"],"replacement":["a".repeat(58_001)]})),
            (21, json!({"original":["    wrong();","    finish();","later"],"replacement":[]})),
        ] {
            let report = review(9, end, fix);
            let finding = &report.findings[0];
            let body = &report.presentation.as_ref().unwrap().github_findings[&finding.id];
            assert!(!body.contains("```suggestion"));
            assert!(body.contains("**Suggested fix**\nCorrect the call"));
            assert!(finding.replacement_lines.is_none());
        }
    }
}
