//! End-to-end review pipeline: clone -> diff -> retrieve -> single model call
//! -> verify -> (optional) publish inline comments.
//!
//! This is the orchestration layer. Each step lives in its own module; this
//! file wires them in order and owns the one-shot model interaction.

use std::path::Path;

use crate::config::{HostConfig, ReviewJob};
use crate::domain::{IgnoreSnapshot, Report, ReviewExclusions};
use crate::error::{Code, Result};
use crate::git::{self, DiffSummary};
use crate::github::GitHub;
use crate::ignore::{self, IgnorePolicy};
use crate::model::ReviewModel;
use crate::prompts;
use crate::retrieval::cohere::Bedrock;
use crate::retrieval::qdrant::Qdrant;
use crate::retrieval::{Pipeline as Retrieval, Schema, SourceVerifier};
use crate::{review, sandbox};

/// Verifies candidate chunks by reading them from the pinned head in the clone.
struct CloneVerifier<'a> {
    repo_dir: &'a Path,
    head_sha: &'a str,
    token: &'a str,
}

impl SourceVerifier for CloneVerifier<'_> {
    fn read(&self, path: &str, start: u32, end: u32) -> Result<Option<String>> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(git::read_range(
                self.repo_dir,
                self.head_sha,
                path,
                start,
                end,
                self.token,
            ))
        })
    }
}

/// Run the whole review for one job. Returns the final report.
///
/// `repo_dir_override`, when provided, points at an already-prepared bare clone
/// that contains both the base and head commits. It skips the sandbox clone,
/// which is useful for fast local iteration; production runs pass `None`.
pub async fn run(
    host: &HostConfig,
    job: &ReviewJob,
    repo_dir_override: Option<std::path::PathBuf>,
) -> Result<Report> {
    job.validate()?;

    // Resolve credentials. Prefer the GitHub App's own identity (installation
    // token) so clones and PR comments are attributed to the App; fall back to
    // explicit read/write tokens from the environment.
    let (read_token, write_token) =
        match crate::github_app::installation_token(&job.owner, &job.repo).await? {
            Some(app) => {
                tracing::info!("using GitHub App installation token");
                let write = if app.can_write_pulls {
                    Some(app.token.clone())
                } else {
                    None
                };
                (app.token, write)
            }
            None => {
                let read = std::env::var("INKSPECT_GITHUB_READ_TOKEN")
                    .map_err(|_| crate::error::err(Code::GitHub, "missing read token"))?;
                let write = std::env::var("INKSPECT_GITHUB_WRITE_TOKEN").ok();
                (read, write)
            }
        };

    // Re-verify the PR snapshot against GitHub before doing any work.
    let gh = GitHub::with_tokens(read_token.clone(), write_token)?;
    let snapshot = gh.verify_snapshot(job).await?;
    if snapshot.merge_base_sha != job.base_sha || snapshot.head_sha != job.head_sha {
        return Err(crate::error::err(Code::GitHubChanged, "snapshot drifted"));
    }

    // Step 1: clone into the sandbox (or reuse a prepared clone for iteration).
    let token = read_token;
    let _clone_guard;
    let repo_dir = match repo_dir_override {
        Some(dir) => {
            tracing::info!("using prepared clone directory, skipping sandbox clone");
            dir
        }
        None => {
            let clone = sandbox::clone_pr(&host.sandbox, job, &token).await?;
            let dir = clone.path().to_path_buf();
            _clone_guard = clone; // keep the temp dir alive for the whole run
            dir
        }
    };

    // Step 2: diff between merge-base and head, scoped by the repository's
    // `.inkspectignore`. The policy comes from the trusted base, never from the
    // reviewed head, and an invalid one stops the run before any model call.
    let policy = ignore::load(&repo_dir, &job.base_sha, &token).await?;
    let (diff, excluded_files): (DiffSummary, usize) =
        git::diff_with_policy(&repo_dir, &job.base_sha, &job.head_sha, &token, policy.as_ref())
            .await?;
    if excluded_files > 0 {
        tracing::info!(excluded_files, "changed files excluded by .inkspectignore");
    }
    if diff.files.is_empty() {
        tracing::info!("no reviewable changes in diff");
    }

    // Step 3: RAG retrieval + rerank (best-effort; absence is non-fatal).
    let target = job.target_revision();
    let mut base = target.clone();
    base.revision_id = job.base_sha.clone();
    // Invariant: base and target must name the same repository.
    if !base.same_repository(&target) {
        return Err(crate::error::err(Code::Internal, "base/target repo mismatch"));
    }

    let exclusions = policy.as_ref().map(|p| ReviewExclusions {
        excluded_files,
        ignore: IgnoreSnapshot {
            revision: base.clone(),
            digest: p.digest.clone(),
        },
    });
    // The policy excluded every changed file, so nothing is left to review:
    // skip retrieval and the model. The journal entry lets the API settle the
    // run at zero cost.
    if diff.files.is_empty() {
        if let Some(all_excluded) = exclusions.clone().filter(|e| e.excluded_files > 0) {
            crate::telemetry::record(
                serde_json::json!({"kind": "model.skipped", "reason": "all_changes_excluded"}),
            )?;
            let mut report = review::excluded_report(
                &job.run_id,
                &target,
                &base,
                "bedrock",
                &host.model_id,
                all_excluded,
            );
            report.run.review_notes = job.review_notes.clone();
            crate::render::attach(&mut report);
            publish(&gh, job, &report).await;
            return Ok(report);
        }
    }

    // Reserve the diff, instructions and review notes before allocating RAG.
    // This is the existing host byte ceiling, not an estimated token count.
    let nonce = prompts::new_nonce();
    let system = prompts::system_prompt(&nonce);
    let notes_json = serde_json::to_value(&job.review_notes).unwrap_or(serde_json::Value::Null);
    let changed_files: Vec<_> = diff.files.clone();
    let without_context = prompts::user_message(
        &nonce, &snapshot.title, &snapshot.body, &diff, &changed_files, &[], &notes_json,
    );
    let mut retrieval_host = host.clone();
    retrieval_host.retrieval.content_bytes = crate::retrieval::review::available_bytes(
        host.retrieval.content_bytes, host.model_max_request_bytes,
        system.len() + without_context.len() + 4096,
    );
    let retrieved = match retrieve(&retrieval_host, job, &diff, &target, &repo_dir, &token, policy.as_ref())
        .await
    {
        Ok(r) => r,
        Err(e) => {
            // Retrieval problems must not block a review; log and continue.
            tracing::warn!(error = %e, "retrieval skipped");
            Vec::new()
        }
    };

    // Step 4: the single hardened model call.
    let model = ReviewModel::new(&host.model_region, &host.model_id, host.model_max_output_tokens)
        .await?;
    let user = prompts::user_message(
        &nonce,
        &snapshot.title,
        &snapshot.body,
        &diff,
        &changed_files,
        &retrieved,
        &notes_json,
    );

    // Bedrock GPT profiles do not implement CountTokens. Reserve the complete
    // operator-approved input allowance, as the previous engine did.
    if host.budget_nano_usd > 0 {
        let reserved = (host.model_input_allowance as u128) * (host.input_nano_usd as u128)
            + (host.model_max_output_tokens as u128) * (host.output_nano_usd as u128);
        if host.model_input_allowance == 0 || host.model_max_output_tokens <= 0
            || reserved > host.budget_nano_usd as u128
            || system.len() + user.len() + 4096 > host.model_max_request_bytes {
            return Err(crate::error::err(Code::Model, "model budget or request limit exceeded"));
        }
    }
    let raw = model.review(&system, &user, job).await?;

    // Verify + assemble the report.
    let report = review::build_report(
        &job.run_id,
        &target,
        &base,
        model.provider(),
        model.model_id(),
        &diff,
        raw,
    );
    let mut report = review::with_exclusions(report, exclusions);
    report.run.review_notes = job.review_notes.clone();
    crate::render::attach(&mut report);
    // Prompt provenance and the complete input are retained in the private
    // model-context capture, separate from the public report.

    publish(&gh, job, &report).await;
    Ok(report)
}

/// Optional step 5: publish the review to the PR. The summary body is the
/// full Markdown report, including the Mermaid diagram and the change-vs-PR
/// intent line; findings on changed lines also get inline comments. Runs
/// even with zero findings so the PR still gets the summary and diagram.
async fn publish(gh: &GitHub, job: &ReviewJob, report: &Report) {
    if !job.publish_inline_comments {
        return;
    }
    let Some(presentation) = &report.presentation else { return };
    match gh.publish_inline(job, &report.findings, &presentation.github_summary).await {
        Ok(n) => tracing::info!(comments = n, "posted review with {n} inline comment(s)"),
        Err(e) => tracing::warn!(error = %e, "inline publish failed"),
    }
}

/// Build the retrieval pipeline and run it. Separated so its errors can be
/// caught and downgraded to a warning by the caller.
async fn retrieve(
    host: &HostConfig,
    job: &ReviewJob,
    diff: &DiffSummary,
    target: &crate::domain::SourceRevision,
    repo_dir: &Path,
    token: &str,
    policy: Option<&IgnorePolicy>,
) -> Result<Vec<crate::retrieval::Retrieved>> {
    let rc = &host.retrieval;
    if rc.content_bytes <= 2 || rc.max_files == 0 { return Ok(Vec::new()); }
    let bedrock = Bedrock::new(
        &rc.region,
        &rc.embed_model,
        &rc.rerank_region,
        &rc.rerank_model,
        rc.dimensions,
    )
    .await?;
    let qdrant = Qdrant::from_env()?;
    let schema = Schema::cohere(rc.dimensions);
    let namespace = crate::retrieval::Namespace::for_revision(target);

    // If no index exists and automatic review is enabled, index the repository
    // before retrieving so the first review on a repo still gets RAG context.
    if qdrant.manifest(&namespace, &schema)?.is_none() {
        if job.auto_review {
            tracing::info!("no index and auto-review enabled; indexing repository before review");
            let indexed = crate::retrieval::indexer::index_repository(
                repo_dir,
                &job.head_sha,
                target,
                &schema,
                &bedrock,
                &qdrant,
                token,
                policy,
            )
            .await?;
            tracing::info!(chunks = indexed, "repository indexed");
        } else {
            tracing::info!("no index and auto-review disabled; reviewing without RAG");
            return Ok(Vec::new());
        }
    }

    let verifier = CloneVerifier {
        repo_dir,
        head_sha: &job.head_sha,
        token,
    };
    let pipeline = Retrieval {
        config: rc,
        schema,
        embedder: &bedrock,
        reranker: &bedrock,
        qdrant: &qdrant,
        verifier: &verifier,
        ignore: policy,
    };
    let focus = review_focus(job);
    pipeline.enrich(target, &focus, &diff.files)
}

/// A short, bounded focus string built from (untrusted but length-limited)
/// review notes. Used only to steer retrieval queries.
fn review_focus(job: &ReviewJob) -> String {
    let mut s = String::from("Review the pull request changes");
    for n in &job.review_notes {
        s.push('\n');
        s.push_str(&n.body);
    }
    crate::retrieval::bounded(&s, 1800)
}
