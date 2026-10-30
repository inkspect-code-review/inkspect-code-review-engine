//! Review output schema.
//!
//! These types are serialized to JSON that stays compatible with the existing
//! Go engine's `domain.Report` contract (see engine/internal/domain/types.go)
//! and the frontend's `ReviewReport` TypeScript type
//! (app/src/lib/review-types.ts).
//!
//! Compatibility rules enforced here:
//!   * `findings`, `evidence`, `verification_gaps`, `assessments` serialize as
//!     `[]`, never `null` (the API iterates them without null checks).
//!   * `severity` is one of "P0".."P3" and findings sort P0 first (lexical).
//!   * `rules`/`advisory` are omitted when empty/false; `diagram` is omitted
//!     when absent. `evidence_ids`/`targets` are always present.
//!   * Diagram node IDs rendered into Mermaid are engine-generated (`n{i}`),
//!     never model-supplied, and labels are sanitized (see `crate::review`).

use serde::{Deserialize, Serialize};

/// Immutable source revision identity. Field order and tags match the Go
/// `domain.SourceRevision` exactly so hashes computed over it agree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SourceRevision {
    pub tenant_id: String,
    pub provider: String,
    pub provider_instance: String,
    pub repository_id: String,
    pub revision_id: String,
}

impl SourceRevision {
    /// Two revisions describe the same repository when everything but the
    /// commit id matches.
    pub fn same_repository(&self, other: &SourceRevision) -> bool {
        self.tenant_id == other.tenant_id
            && self.provider == other.provider
            && self.provider_instance == other.provider_instance
            && self.repository_id == other.repository_id
    }
}

/// A repository-relative source location, 1-based and inclusive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
}

impl Location {
    pub fn valid(&self) -> bool {
        !self.path.is_empty()
            && self.path != "."
            && self.path != ".."
            && !self.path.starts_with('/')
            && !self.path.contains("../")
            && !self.path.contains('\\')
            && !self.path.contains(':')
            && !self.path.chars().any(|c| c.is_control())
            && self.start_line >= 1
            && self.end_line >= self.start_line
    }
}

/// Finding severity. Serialized as P0..P3; P0 is the most severe and sorts
/// first by lexical comparison, matching the Go engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Severity {
    #[serde(rename = "P0")]
    Critical,
    #[serde(rename = "P1")]
    High,
    #[serde(rename = "P2")]
    Medium,
    #[serde(rename = "P3")]
    Low,
}

/// Final, verified finding. Shape matches the Go `domain.Finding`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advisory: Option<bool>,
    pub id: String,
    pub title: String,
    pub severity: Severity,
    pub reason: String,
    pub suggestion: String,
    pub verification: String,
    pub revision: SourceRevision,
    pub location: Location,
    pub evidence_ids: Vec<String>,
}

/// A reason the review could not fully verify something. Blocking gaps prevent
/// a "ready_for_human" decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationGap {
    pub reason: String,
    pub blocking: bool,
}

/// A diagram node. `id` is the model's logical id; when rendering Mermaid the
/// engine substitutes a generated `n{index}` so model text can never become
/// diagram syntax.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagramNode {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_ids: Vec<String>,
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub issue: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagramEdge {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_ids: Vec<String>,
    pub from: String,
    pub to: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub missing: bool,
}

/// Bounded structured diagram data. Renderers never accept raw Mermaid.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagram {
    pub title: String,
    /// "current" or "proposed".
    pub view: String,
    pub nodes: Vec<DiagramNode>,
    pub edges: Vec<DiagramEdge>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_ids: Vec<String>,
}

/// Top-level decision. The accepting value is reviewer-agnostic: the next
/// reviewer may be a human or another agent, so it is "ready_for_review"
/// rather than "ready_for_human".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    NeedsChanges,
    NeedsEvidence,
    #[serde(alias = "ready_for_human")]
    ReadyForReview,
}

/// One scored dimension (1 to 5) with its justification and a path to 5/5.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Score {
    pub score: u8,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub rationale: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub to_improve: String,
}

/// Quality scores for the changed code.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scores {
    pub code_quality: Score,
    pub security: Score,
}

/// The review run envelope the frontend reads. Only the fields the app/API
/// actually consume are modeled; unknown fields are not required because we
/// own the producer side.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSelection {
    pub provider: String,
    pub model_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewRun {
    pub id: String,
    pub status: String,
    /// "current" or "stale".
    pub freshness: String,
    /// Complete for the reviewed scope, which excludes `exclusions` paths.
    pub coverage_complete: bool,
    pub target: SourceRevision,
    pub base: SourceRevision,
    pub model: ModelSelection,
    /// Present whenever the base revision has a `.inkspectignore`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclusions: Option<ReviewExclusions>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub review_notes: Vec<crate::config::ReviewNote>,
}

/// Trusted review-scope metadata from the repository's `.inkspectignore`,
/// never a model claim. Field names follow the earlier engine's
/// `comparison.excluded_files` and `comparison.ignore`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewExclusions {
    /// Changed files left out of the review.
    pub excluded_files: usize,
    pub ignore: IgnoreSnapshot,
}

/// The pinned policy: the revision it was read from and its content digest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IgnoreSnapshot {
    pub revision: SourceRevision,
    /// `sha256:<hex>` over the policy file bytes.
    pub digest: String,
}

impl ReviewExclusions {
    /// Engine-authored one-liner, e.g. "3 changed files excluded by
    /// .inkspectignore at base 1a2b3c4d5e6f."
    pub fn note(&self) -> String {
        let files = if self.excluded_files == 1 { "file" } else { "files" };
        let base: String = self.ignore.revision.revision_id.chars().take(12).collect();
        format!(
            "{} changed {files} excluded by .inkspectignore at base {base}.",
            self.excluded_files
        )
    }
}

/// How well the change delivers the PR's stated intent. `level` is one of
/// "aligned" | "partial" | "diverging" | "unclear".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeAlignment {
    pub level: String,
    pub rationale: String,
}

/// The persisted and published review report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presentation: Option<Presentation>,
    pub schema_version: String,
    pub run: ReviewRun,
    pub summary: String,
    pub decision: Decision,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_alignment: Option<ChangeAlignment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scores: Option<Scores>,
    pub findings: Vec<Finding>,
    pub verification_gaps: Vec<VerificationGap>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagram: Option<Diagram>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub presentation_notes: Vec<String>,
}

/// All review prose is generated by the engine; consumers only deliver it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Presentation {
    pub version: u32,
    pub markdown: String,
    pub github_summary: String,
    pub github_findings: std::collections::BTreeMap<String, String>,
    pub finding_prompts: std::collections::BTreeMap<String, String>,
    pub fix_prompt: String,
}

fn is_false(b: &bool) -> bool {
    !*b
}
