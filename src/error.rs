//! Error types for the review engine.
//!
//! Errors never carry secret material (tokens, credentials) or raw untrusted
//! repository content. Operational failures are mapped to stable codes so the
//! dispatching API can reason about them without parsing free text.

use std::fmt;

/// Stable machine-readable error codes. These mirror the spirit of the Go
/// engine's `message.Code` values so the dispatching API can treat failures
/// from either engine uniformly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    /// The job request or worker configuration is invalid.
    QueueInvalid,
    /// A GitHub reference, PR snapshot or API response was invalid/inconsistent.
    GitHub,
    /// The pull request changed during preparation.
    GitHubChanged,
    /// A sandbox/container isolation requirement was not met.
    Container,
    /// Repository source could not be read, was too large, or was malformed.
    Source,
    /// The repository's root `.inkspectignore` is invalid, unsupported, or
    /// exceeds its limits. Runs fail closed rather than guess the scope.
    IgnoreInvalid,
    /// A retrieval (embedding / Qdrant / rerank) step failed or returned
    /// something that failed verification.
    Retrieval,
    /// The model call failed, timed out, was truncated, or returned a response
    /// that did not match the required schema.
    Model,
    /// The independent repair verifier could not establish a complete fix.
    FixVerification,
    /// The generator explicitly returned no safe repair, not a provider failure.
    FixNoSafeRepair,
    /// Publishing the review to GitHub failed.
    Publish,
    /// An unexpected internal invariant was violated.
    Internal,
}

impl Code {
    pub fn as_str(self) -> &'static str {
        match self {
            Code::QueueInvalid => "queue.invalid",
            Code::GitHub => "github.invalid_response",
            Code::GitHubChanged => "github.pull_request_changed",
            Code::Container => "container.unavailable",
            Code::Source => "source.unavailable",
            Code::IgnoreInvalid => "source.invalid_ignore",
            Code::Retrieval => "retrieval.invalid",
            Code::Model => "model.failed",
            Code::FixVerification => "fix.verification_failed",
            Code::FixNoSafeRepair => "fix.no_safe_repair",
            Code::Publish => "publish.failed",
            Code::Internal => "internal.error",
        }
    }
}

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A coded engine error. `reason` is a short, non-sensitive diagnostic string
/// that is safe to surface to operators; it must never contain tokens or raw
/// repository bytes.
#[derive(Debug, Clone)]
pub struct EngineError {
    pub code: Code,
    pub reason: String,
}

impl EngineError {
    // Only engine-owned categories reach public diagnostics. Never derive one
    // from provider prose, source text, paths or a model's explanation.
    pub fn diagnostic_code(&self) -> &'static str {
        match self.reason.as_str() {
            "generator could not propose a complete safe repair" => "fix.no_safe_repair",
            "invalid repair response" => "fix.invalid_response",
            "repair anchor is missing or ambiguous" => "fix.anchor_mismatch",
            "repair must address every finding" => "fix.incomplete_findings",
            "repair path outside supplied scope" => "fix.out_of_scope",
            "invalid repaired content" => "fix.invalid_content",
            "invalid repair inability explanation" => "fix.invalid_response",
            "empty or oversized repair" | "empty or oversized cumulative candidate" => "fix.invalid_size",
            "repair repeated a rejected candidate" => "fix.repeated_candidate",
            "repair attempt limit reached without verified closure" => "fix.attempt_limit",
            "repair context exceeds operation budget" | "verification context exceeds operation budget" | "repair loop cannot fit operation budget" => "fix.budget_limit",
            "independent verification did not establish a complete safe fix" => "fix.invalid_verdict",
            "converse call failed" | "provider request failed" => "model.request_failed",
            "provider rejected the request" => "model.request_rejected",
            "model response was truncated or refused" | "provider response incomplete or refused" => "model.incomplete_response",
            "provider response interrupted" => "model.interrupted_response",
            "model response too large" | "provider response too large" => "model.oversized_response",
            "malformed model output" | "unexpected response role" | "invalid provider response" => "model.invalid_response",
            "review context could not be saved" => "audit.write_failed",
            _ => self.code.as_str(),
        }
    }
    pub fn new(code: Code, reason: impl Into<String>) -> Self {
        Self {
            code,
            reason: reason.into(),
        }
    }
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.code, self.reason)
    }
}

impl std::error::Error for EngineError {}

/// Convenience constructors keep call sites terse while forcing a code.
pub fn err(code: Code, reason: impl Into<String>) -> EngineError {
    EngineError::new(code, reason)
}

pub type Result<T> = std::result::Result<T, EngineError>;
