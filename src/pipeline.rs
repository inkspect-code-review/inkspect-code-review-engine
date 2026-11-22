//! End-to-end review pipeline: clone -> diff -> retrieve -> review
//! -> verify -> (optional) publish inline comments.
//!
//! This is the orchestration layer. Each step lives in its own module; this
//! file selects the request planning policy and publishes the final report.

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
    crate::telemetry::record(serde_json::json!({"kind":"source.snapshot.started"}))?;
    let snapshot = gh.verify_snapshot(job).await?;
    if snapshot.merge_base_sha != job.base_sha || snapshot.head_sha != job.head_sha {
        return Err(crate::error::err(Code::GitHubChanged, "snapshot drifted"));
    }

    crate::telemetry::record(serde_json::json!({"kind":"source.snapshot.completed"}))?;
    crate::telemetry::record(serde_json::json!({"kind":"source.clone.started"}))?;
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

    crate::telemetry::record(serde_json::json!({"kind":"source.clone.completed"}))?;
    // Step 2: diff between merge-base and head, scoped by the repository's
    // `.inkspectignore`. The policy comes from the trusted base, never from the
    // reviewed head, and an invalid one stops the run before any model call.
    let policy = ignore::load(&repo_dir, &job.base_sha, &token).await?;
    // The full diff, JSON envelopes and symbol index coexist while planning.
    // Bound raw source reads using the worker's configured memory allowance.
    let source_bytes = host.token_planned_review.then(|| u64::from(host.sandbox.memory_mib).saturating_mul(1024 * 1024) / 16);
    let (mut diff, excluded_files): (DiffSummary, usize) =
        git::scoped_diff(&repo_dir, &job.base_sha, &job.head_sha, &token, policy.as_ref(), source_bytes)
            .await?;
    crate::telemetry::record(serde_json::json!({
        "kind":"review.scope", "head":job.head_sha, "base":job.base_sha,
        "excluded_files":excluded_files,
        "files":diff.files.iter().map(|f| serde_json::json!({"path":f.path,"base_path":f.base_path,
            "kind":f.kind,"limited":f.unknown,"edits":f.edits.iter().map(|e| serde_json::json!({
            "start_line":e.target_start,"lines":e.target_lines})).collect::<Vec<_>>() })).collect::<Vec<_>>()
    }))?;
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
            crate::telemetry::record(serde_json::json!({"kind":"report.ready"}))?;
            publish(&gh, job, &report).await;
            return Ok(report);
        }
    }

    if host.token_planned_review {
        let retrieved = match retrieve(host, job, &diff, &target, &repo_dir, &token, policy.as_ref()).await {
            Ok(context) => context,
            Err(e) => {
                crate::telemetry::record(serde_json::json!({"kind":"review.retrieval_failed"}))?;
                if host.require_indexed_context { return Err(e); }
                Vec::new()
            }
        };
        crate::telemetry::record(serde_json::json!({"kind":"review.context","revision":job.head_sha,
            "sources": retrieved.iter().map(|r| serde_json::json!({"path":r.path,"start_line":r.start_line,"end_line":r.end_line})).collect::<Vec<_>>() }))?;
        let model = ReviewModel::new(&host.model_region, &host.model_id, host.model_max_output_tokens).await?;
        let report = crate::review_batches::run(&model, host, job, &snapshot.title, &snapshot.body, &diff, &retrieved, &target, &base).await?;
        let mut report = review::with_exclusions(report, exclusions);
        report.run.review_notes = job.review_notes.clone();
        crate::render::attach(&mut report);
        crate::telemetry::record(serde_json::json!({"kind":"report.ready"}))?;
        publish(&gh, job, &report).await;
        return Ok(report);
    }

    // Reserve the diff, instructions and review notes before allocating RAG.
    // This is the existing host byte ceiling, not an estimated token count.
    let nonce = prompts::new_nonce();
    let system = prompts::system_prompt(&nonce);
    let notes_json = serde_json::to_value(&job.review_notes).unwrap_or(serde_json::Value::Null);
    let limit = host.best_effort_review_bytes.unwrap_or(host.model_max_request_bytes);
    let original_files = diff.files.len();
    let make_message = |files: &[git::ChangedFile], context: &[crate::retrieval::Retrieved], prior: &[crate::config::PriorFinding]| {
        prompts::user_message(&nonce, &snapshot.title, &snapshot.body, files, files, context, &notes_json, prior)
    };
    let mut prior = job.prior_findings.as_slice();
    let mut without_context = make_message(&diff.files, &[], prior);
    while !prior.is_empty() && limit > 0 && system.len() + without_context.len() + 4096 > limit {
        prior = &prior[..prior.len() - 1];
        without_context = make_message(&diff.files, &[], prior);
    }
    if host.best_effort_review_bytes.is_some() && system.len() + without_context.len() + 4096 > limit {
        fit_changed_files(&mut diff.files, limit, |files| {
            system.len() + make_message(files, &[], &[]).len() + 4096
        });
        without_context = make_message(&diff.files, &[], prior);
        if original_files > 0 && diff.files.is_empty() {
            return Err(crate::error::err(Code::Model, "no changed file fits the review request"));
        }
    }
    let mut retrieval_host = host.clone();
    retrieval_host.retrieval.content_bytes = crate::retrieval::review::available_bytes(
        host.retrieval.content_bytes, limit, system.len() + without_context.len() + 4096,
    );
    let retrieved = match retrieve(&retrieval_host, job, &diff, &target, &repo_dir, &token, policy.as_ref())
        .await
    {
        Ok(r) => r,
        Err(e) => {
            crate::telemetry::record(serde_json::json!({"kind":"review.retrieval_failed"}))?;
            if host.require_indexed_context { return Err(e); }
            // Optional retrieval preserves the existing best-effort behavior.
            tracing::warn!(error = %e, "retrieval skipped");
            Vec::new()
        }
    };

    crate::telemetry::record(serde_json::json!({"kind":"review.context", "revision":job.head_sha,
        "sources":retrieved.iter().map(|r| serde_json::json!({"path":r.path,"start_line":r.start_line,
            "end_line":r.end_line})).collect::<Vec<_>>() }))?;

    // Step 4: the single hardened model call.
    let model = ReviewModel::new(&host.model_region, &host.model_id, host.model_max_output_tokens)
        .await?;
    let user = make_message(&diff.files, &retrieved, prior);
    let request_bytes = system.len() + user.len() + 4096;
    let reserved = host.model_input_allowance as u128 * host.input_nano_usd as u128
        + host.model_max_output_tokens.max(0) as u128 * host.output_nano_usd as u128;
    if host.best_effort_review_bytes.is_some() {
        crate::telemetry::record(serde_json::json!({
            "kind": "review.estimate", "allowance_cost_nano_usd": reserved.to_string(),
            "budget_nano_usd": host.budget_nano_usd, "request_bytes": request_bytes,
            "request_limit_bytes": limit, "selected_files": diff.files.len(), "total_files": original_files,
        }))?;
        if request_bytes > limit {
            return Err(crate::error::err(Code::Model, "review request limit exceeded"));
        }
    } else if host.budget_nano_usd > 0 && reserved > host.budget_nano_usd as u128 {
        return Err(crate::error::err(Code::ReviewBudget, "review inference budget exceeded"));
    } else if host.budget_nano_usd > 0 && (host.model_input_allowance == 0
        || host.model_max_output_tokens <= 0
        || request_bytes > limit) {
        return Err(crate::error::err(Code::Model, "model budget or request limit exceeded"));
    }
    let raw = model.review(&system, &user, job).await?;
    let resolutions = review::verified_resolutions(prior, &diff, &raw);

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
    if diff.files.len() < original_files {
        report.run.coverage_complete = false;
        if matches!(report.decision, crate::domain::Decision::ReadyForReview) {
            report.decision = crate::domain::Decision::NeedsEvidence;
        }
        report.presentation_notes.push(format!(
            "Reviewed {} of {} changed files within the model request limit.", diff.files.len(), original_files
        ));
    }
    report.resolved_findings = resolutions;
    report.run.review_notes = job.review_notes.clone();
    crate::render::attach(&mut report);
    crate::telemetry::record(serde_json::json!({"kind":"report.ready"}))?;
    // Prompt provenance and the complete input are retained in the private
    // model-context capture, separate from the public report.

    publish(&gh, job, &report).await;
    Ok(report)
}

/// A file adds one entry to each of the changes and source arrays. Account
/// for both separators and keep looking when a large file does not fit.
fn fit_changed_files<T>(files: &mut Vec<T>, limit: usize, request_size: impl Fn(&[T]) -> usize) {
    let empty = request_size(&[]);
    let mut remaining = limit.saturating_sub(empty);
    let mut separators = 0;
    files.retain(|file| {
        let cost = request_size(std::slice::from_ref(file)) - empty + separators;
        if cost > remaining {
            return false;
        }
        remaining -= cost;
        separators = 2;
        true
    });
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
    if rc.content_bytes <= 2 || rc.max_files == 0 {
        if host.require_indexed_context { return Err(crate::error::err(Code::Retrieval, "retrieval is required")); }
        return Ok(Vec::new());
    }
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
    let manifest = qdrant.manifest(&namespace, &schema)?;
    let needs_index = manifest.as_ref().is_none_or(|m| host.require_indexed_context && m.revision.as_ref() != Some(target));
    if needs_index {
        if job.auto_review {
            tracing::info!("no index and auto-review enabled; indexing repository before review");
            let mut progress = crate::index_progress::IndexProgress::new();
            let indexed = crate::retrieval::indexer::index_repository(
                repo_dir,
                &job.head_sha,
                target,
                &schema,
                &bedrock,
                &qdrant,
                token,
                policy,
                &mut progress,
                host.index_max_input_bytes,
            )
            .await?;
            crate::telemetry::record(serde_json::json!({"kind":"review.index", "revision":job.head_sha,
                "reused":false, "chunks":indexed,"scan":progress.scan}))?;
            tracing::info!(chunks = indexed, "repository indexed");
        } else {
            if host.require_indexed_context { return Err(crate::error::err(Code::Retrieval, "current repository index required")); }
            tracing::info!("no index and auto-review disabled; reviewing without RAG");
            return Ok(Vec::new());
        }
    }

    if !needs_index {
        crate::telemetry::record(serde_json::json!({"kind":"review.index", "revision":manifest.as_ref().and_then(|m|m.revision.as_ref()).map(|r|&r.revision_id), "reused":true}))?;
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
    if host.require_indexed_context { pipeline.enrich_current(target, &focus, &diff.files) }
    else { pipeline.enrich(target, &focus, &diff.files) }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_review_keeps_later_files_that_fit_the_serialized_prompt() {
        let file = |path: &str, hunk: String| git::ChangedFile {
            path: path.into(), base_path: path.into(), kind: "modified".into(),
            edits: vec![], unknown: false, hunk_text: hunk, excerpt: String::new(),
        };
        let first = file("first.rs", "\"\\\n".repeat(10));
        let last = file("last.rs", "é".repeat(10));
        let size = |files: &[git::ChangedFile]| {
            prompts::user_message("nonce", "title", "", files, files, &[], &serde_json::json!([]), &[]).len()
        };
        let exact = size(&[first.clone(), last.clone()]);
        let mut files = vec![first, file("huge.rs", "x".repeat(1_500_001)), last];
        fit_changed_files(&mut files, exact, size);
        assert_eq!(files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), ["first.rs", "last.rs"]);
        assert_eq!(size(&files), exact);
        fit_changed_files(&mut files, exact - 1, size);
        assert_eq!(files.len(), 1);
    }
}
