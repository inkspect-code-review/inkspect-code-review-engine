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
