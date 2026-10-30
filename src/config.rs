//! Trusted engine configuration and the per-job review request.
//!
//! Two trust tiers, mirroring the Go engine:
//!   * `HostConfig` is operator-owned (sandbox image, resource limits, model
//!     profile, AWS region, Qdrant endpoint). It is never influenced by a job.
//!   * `ReviewJob` is the per-PR request. Repository-derived strings inside it
//!     (owner, repo, review notes) are treated as UNTRUSTED data downstream.
//!
//! Credentials (GitHub installation token, AWS credentials, Qdrant api-key)
//! live in the process environment / AWS credential chain, never in a job
//! payload and never echoed into logs or errors.

use serde::{Deserialize, Serialize};

use crate::error::{err, Code, Result};

/// Operator-owned configuration. Loaded once at startup from a trusted file
/// and/or environment; a job can never override any of it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostConfig {
    /// AWS region for the review model (Bedrock Converse).
    pub model_region: String,
    /// Inference-profile model id for the single review call, e.g.
    /// "global.openai.gpt-6-sol" or "eu.anthropic.claude-sonnet-...".
    pub model_id: String,
    /// Hard cap on output tokens for the single review call.
    pub model_max_output_tokens: i32,
    /// Overall deadline (seconds) for the whole job.
    pub timeout_seconds: u64,
    #[serde(default)]
    pub model_input_allowance: u64,
    #[serde(default)]
    pub model_max_request_bytes: usize,
    #[serde(default)]
    pub input_nano_usd: u64,
    #[serde(default)]
    pub output_nano_usd: u64,
    #[serde(default)]
    pub budget_nano_usd: u64,
    /// Retrieval (RAG) configuration. Matches the Go `retrieval.Config`.
    pub retrieval: RetrievalConfig,
    /// Sandbox resource limits for the clone container.
    pub sandbox: SandboxConfig,
}

/// Retrieval configuration. Field names and defaults mirror the Go
/// `retrieval.Config` so the Rust engine targets the same Qdrant collection
/// and embedding space.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievalConfig {
    pub region: String,
    /// Inference-profile embed model, e.g. "eu.cohere.embed-v4:0". The schema
    /// model stored in Qdrant is always the base id "cohere.embed-v4:0".
    pub embed_model: String,
    pub rerank_region: String,
    pub rerank_model: String,
    pub dimensions: u32,
    /// Qdrant vector-search candidates per query (<= 50 in the Go engine).
    pub candidates: usize,
    /// Chat result limit. Reviews rank all candidates and use the global byte budget.
    pub top_n: usize,
    /// Max number of per-changed-file queries.
    pub queries: usize,
    /// Byte budget for the serialized retrieved-context array (including metadata).
    pub content_bytes: usize,
    /// Max retrieved chunks appended to the prompt.
    pub max_files: usize,
}

impl Default for RetrievalConfig {
    fn default() -> Self {
        // Matches engine DefaultConfig() + review-bedrock-rag.example.json.
        Self {
            region: "eu-west-2".into(),
            embed_model: "eu.cohere.embed-v4:0".into(),
            rerank_region: "eu-central-1".into(),
            rerank_model: "cohere.rerank-v3-5:0".into(),
            dimensions: 1024,
            candidates: 40,
            top_n: 8,
            queries: 8,
            content_bytes: 512_000,
            max_files: 100,
        }
    }
}

/// Sandbox limits for the clone container.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxConfig {
    /// `docker`/`podman` executable path.
    pub runtime: String,
    /// Immutable image id (sha256:...) used to run git inside the sandbox.
    pub image: String,
    pub cpu_millis: u32,
    pub memory_mib: u32,
    pub pids: u32,
    /// Max on-disk clone size in MiB; the host aborts the fetch past this.
    pub clone_mib: u64,
    /// Whether to run the clone inside a container (true) or in a hardened
    /// host-side temp dir (false). Both disable hooks, submodules and non-https
    /// protocols; the container additionally removes ambient network access.
    pub containerize: bool,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            runtime: "docker".into(),
            image: String::new(),
            cpu_millis: 2000,
            memory_mib: 2048,
            pids: 256,
            clone_mib: 500,
            containerize: false,
        }
    }
}

/// Per-job review request. Built by the dispatching API from a webhook or a
/// manual dashboard action and handed to the worker.
///
/// `owner`, `repo` and `review_notes` are untrusted strings. The SHAs are
/// validated to be 40-hex before any git operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewJob {
    /// Review run id; also used as the finding-comment dedup key on GitHub.
    pub run_id: String,
    pub owner: String,
    pub repo: String,
    /// Numeric GitHub repository id, as a decimal string, for namespace hashing.
    pub repository_id: String,
    /// Tenant / organization id for namespace hashing.
    pub tenant_id: String,
    pub pull_number: u64,
    /// Merge-base SHA (review base) — resolved by the API via the compare API.
    pub base_sha: String,
    /// Head SHA (review target).
    pub head_sha: String,
    /// Head branch name, used only for logging/diagnostics (never trusted).
    #[serde(default)]
    pub head_ref: String,
    /// Optional high-level instructions the review should focus on. Untrusted.
    #[serde(default)]
    pub review_notes: Vec<ReviewNote>,
    /// When true, the engine posts inline comments on the PR (the capability
    /// the Go engine lacks). When false it only returns the report.
    #[serde(default)]
    pub publish_inline_comments: bool,
    /// The organization's automatic-review setting. When true and the
    /// repository has no RAG index yet, the engine indexes it before reviewing.
    /// When false, a missing index just means the review runs without RAG.
    #[serde(default)]
    pub auto_review: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewNote {
    pub author: String,
    pub body: String,
}

fn valid_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()) && s == s.to_ascii_lowercase()
}

fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

impl ReviewJob {
    /// Validate the untrusted fields before any side effect. Fails closed.
    pub fn validate(&self) -> Result<()> {
        if self.run_id.is_empty() || self.run_id.len() > 200 {
            return Err(err(Code::QueueInvalid, "invalid run id"));
        }
        if !valid_name(&self.owner) || !valid_name(&self.repo) {
            return Err(err(Code::QueueInvalid, "invalid repository reference"));
        }
        if self.repository_id.is_empty()
            || !self.repository_id.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(err(Code::QueueInvalid, "invalid repository id"));
        }
        if self.tenant_id.is_empty() || self.tenant_id.len() > 200 {
            return Err(err(Code::QueueInvalid, "invalid tenant id"));
        }
        if self.pull_number < 1 || self.pull_number > 2_147_483_647 {
            return Err(err(Code::QueueInvalid, "invalid pull number"));
        }
        if !valid_sha(&self.base_sha) || !valid_sha(&self.head_sha) {
            return Err(err(Code::QueueInvalid, "invalid commit sha"));
        }
        if self.review_notes.len() > 5 {
            return Err(err(Code::QueueInvalid, "too many review notes"));
        }
        let mut note_bytes = 0usize;
        for n in &self.review_notes {
            if !valid_name(&n.author) {
                return Err(err(Code::QueueInvalid, "invalid review note author"));
            }
            note_bytes += n.body.len();
            if n.body.chars().any(|c| c.is_control() && c != '\n' && c != '\t') {
                return Err(err(Code::QueueInvalid, "invalid review note body"));
            }
        }
        if note_bytes > 2000 {
            return Err(err(Code::QueueInvalid, "review notes too long"));
        }
        Ok(())
    }

    /// The immutable review-target revision identity used for namespace and
    /// chunk hashing. Provider/instance literals match the Go engine.
    pub fn target_revision(&self) -> crate::domain::SourceRevision {
        crate::domain::SourceRevision {
            tenant_id: self.tenant_id.clone(),
            provider: "github".into(),
            provider_instance: "github.com".into(),
            repository_id: self.repository_id.clone(),
            revision_id: self.head_sha.clone(),
        }
    }
}
