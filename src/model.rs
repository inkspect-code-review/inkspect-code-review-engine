//! The single review model call (Bedrock Converse) and strict response parsing.
//!
//! Unlike the Go engine, which makes one call per obligation plus separate
//! judge and diagram calls, this makes ONE Converse call: system prompt +
//! one untrusted user payload in, one JSON object (findings + diagram) out.
//!
//! The system message carries all instructions; the user message carries all
//! untrusted data wrapped in a per-call nonce (see `crate::prompts`). The
//! response is decoded strictly: unknown fields are rejected, the shape is
//! validated, and a malformed response is NOT re-fed to the model (no repair
//! loop that could amplify an injection).

use aws_sdk_bedrockruntime::operation::RequestId;
use aws_sdk_bedrockruntime::types::{
    ContentBlock, ConversationRole, InferenceConfiguration, Message, StopReason,
    SystemContentBlock,
};
use aws_sdk_bedrockruntime::Client;
use serde::Deserialize;

use crate::domain::Severity;
use crate::error::{err, Code, Result};

/// A raw finding exactly as the model emits it (before engine verification).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawFinding {
    pub title: String,
    pub severity: Severity,
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub reason: String,
    pub suggestion: String,
    pub verification: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawDiagram {
    pub meaningful: bool,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub view: Option<String>,
    #[serde(default)]
    pub nodes: Vec<RawNode>,
    #[serde(default)]
    pub edges: Vec<RawEdge>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawNode {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub issue: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawEdge {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub missing: bool,
}

/// How well the diff matches the PR's stated intent.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawAlignment {
    pub level: String,
    pub rationale: String,
}

/// One scored dimension: a 1-5 score, why, and how to reach 5/5.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawScore {
    pub score: u8,
    #[serde(default)]
    pub rationale: String,
    #[serde(default)]
    pub to_improve: String,
}

/// Quality scores for the changed code.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawScores {
    pub code_quality: RawScore,
    pub security: RawScore,
}

/// The single model response.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawReview {
    pub summary: String,
    /// The model's suggested decision. Advisory only: the engine derives the
    /// authoritative decision from verified findings, so a prompt-injected
    /// value here cannot change the outcome.
    #[allow(dead_code)]
    pub decision: String,
    /// How well the change delivers the PR's stated intent. Optional so an
    /// older prompt/model still parses.
    #[serde(default)]
    pub change_alignment: Option<RawAlignment>,
    /// Code-quality and security scores (1-5) for the changed code.
    #[serde(default)]
    pub scores: Option<RawScores>,
    pub findings: Vec<RawFinding>,
    #[serde(default)]
    pub diagram: Option<RawDiagram>,
}

pub struct ReviewModel {
    client: Option<Client>,
    byok: Option<crate::byok::Provider>,
    model_id: String,
    region: String,
    max_output_tokens: i32,
    max_response_bytes: usize,
}

impl ReviewModel {
    pub async fn new(region: &str, model_id: &str, max_output_tokens: i32) -> Result<ReviewModel> {
        if let Some(provider) = crate::byok::Provider::from_pipe()? {
            return Ok(ReviewModel { client:None, model_id:provider.model.clone(), region:provider.provider.clone(),
                max_output_tokens:max_output_tokens.min(provider.max_output_tokens), max_response_bytes:256*1024, byok:Some(provider) });
        }
        let cfg = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(region.to_string()))
            // An ambiguous paid request must not be automatically replayed.
            .retry_config(aws_config::retry::RetryConfig::disabled())
            .load()
            .await;
        Ok(ReviewModel {
            client: Some(Client::new(&cfg)),
            byok: None,
            model_id: model_id.to_string(),
            region: region.to_string(),
            max_output_tokens,
            max_response_bytes: 256 * 1024,
        })
    }

    pub fn provider(&self) -> &str { self.byok.as_ref().map(|p| p.provider.as_str()).unwrap_or("bedrock") }
    pub fn model_id(&self) -> &str { &self.model_id }

    /// Make the single Converse call and return the parsed review.
    pub async fn review(&self, system: &str, user: &str, job: &crate::config::ReviewJob) -> Result<RawReview> {
        parse_review(&self.complete(system, user, job).await?)
    }

    pub async fn complete(&self, system: &str, user: &str, job: &crate::config::ReviewJob) -> Result<String> {
        crate::audit::capture(job, &self.model_id, &self.region, self.max_output_tokens, system, user)?;
        let receipt = crate::telemetry::next_id();
        crate::telemetry::record(serde_json::json!({"kind":"model.started", "id":receipt}))?;
        if let Some(provider) = &self.byok { return provider.complete(system,user,self.max_output_tokens,receipt).await; }
        let out = self
            .client.as_ref().ok_or_else(||err(Code::Model,"model client unavailable"))?
            .converse()
            .model_id(&self.model_id)
            .system(SystemContentBlock::Text(system.to_string()))
            .messages(
                Message::builder()
                    .role(ConversationRole::User)
                    .content(ContentBlock::Text(user.to_string()))
                    .build()
                    .map_err(|_| err(Code::Model, "message build failed"))?,
            )
            .inference_config(
                // Only max_tokens is set. Some Bedrock models (e.g. the GPT
                // inference profiles) reject the temperature field, and the Go
                // engine likewise sends only MaxTokens.
                InferenceConfiguration::builder()
                    .max_tokens(self.max_output_tokens)
                    .build(),
            )
            .send()
            .await
            .map_err(|e| {
                // Surface the full SDK error (service message, not secrets) to
                // debug logs so model-access/auth problems are diagnosable.
                let detail = aws_smithy_types::error::display::DisplayErrorContext(&e);
                tracing::debug!("converse call failed: {detail}");
                err(Code::Model, "converse call failed")
            })?;

        if let Some(usage) = out.usage() {
            crate::telemetry::record(serde_json::json!({"kind":"model.usage", "id":receipt, "request_id":out.request_id(), "measurement_source":"bedrock_converse", "input_tokens":i64::from(usage.input_tokens()) + i64::from(usage.cache_read_input_tokens().unwrap_or(0)) + i64::from(usage.cache_write_input_tokens().unwrap_or(0)), "output_tokens":usage.output_tokens(), "cached_input_tokens":usage.cache_read_input_tokens().unwrap_or(0), "cache_write_input_tokens":usage.cache_write_input_tokens().unwrap_or(0), "uncached_input_tokens":usage.input_tokens()}))?;
        }
        if out.stop_reason() != &StopReason::EndTurn {
            return Err(err(Code::Model, "model response was truncated or refused"));
        }

        let message = match out.output() {
            Some(aws_sdk_bedrockruntime::types::ConverseOutput::Message(m)) => m,
            _ => return Err(err(Code::Model, "malformed model output")),
        };
        if message.role() != &ConversationRole::Assistant {
            return Err(err(Code::Model, "unexpected response role"));
        }

        let mut text = String::new();
        for block in message.content() {
            if let ContentBlock::Text(t) = block {
                if text.len() + t.len() > self.max_response_bytes {
                    return Err(err(Code::Model, "model response too large"));
                }
                text.push_str(t);
            }
            // Reasoning and any other block kinds are ignored, never parsed.
        }

        Ok(text)
    }
}

/// Strictly decode the single JSON object. Tolerates a single ```json fence
/// (some models wrap JSON), but nothing else. Unknown fields are rejected.
pub fn parse_review(text: &str) -> Result<RawReview> {
    let trimmed = unwrap_fence(text.trim());
    let review: RawReview = serde_json::from_str(trimmed)
        .map_err(|_| err(Code::Model, "response did not match review schema"))?;
    // Ensure there is no trailing garbage after the object.
    let mut de = serde_json::Deserializer::from_str(trimmed);
    serde::de::IgnoredAny::deserialize(&mut de)
        .map_err(|_| err(Code::Model, "invalid JSON"))?;
    de.end()
        .map_err(|_| err(Code::Model, "trailing data after JSON object"))?;
    Ok(review)
}

/// Unwrap exactly one ```json ... ``` or ``` ... ``` fence if the whole string
/// is fenced; otherwise return it unchanged.
pub(crate) fn unwrap_fence(raw: &str) -> &str {
    for prefix in ["```json\n", "```\n"] {
        if let Some(rest) = raw.strip_prefix(prefix) {
            if let Some(inner) = rest.strip_suffix("\n```") {
                let inner = inner.trim();
                if serde_json::from_str::<serde::de::IgnoredAny>(inner).is_ok() {
                    return inner;
                }
            }
        }
    }
    raw
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_fields() {
        let bad = r#"{"summary":"s","decision":"ready_for_human","findings":[],"extra":1}"#;
        assert!(parse_review(bad).is_err());
    }

    #[test]
    fn accepts_minimal_valid() {
        let ok = r#"{"summary":"s","decision":"ready_for_human","findings":[]}"#;
        let r = parse_review(ok).unwrap();
        assert_eq!(r.decision, "ready_for_human");
        assert!(r.findings.is_empty());
    }

    #[test]
    fn unwraps_json_fence() {
        let fenced = "```json\n{\"summary\":\"s\",\"decision\":\"ready_for_human\",\"findings\":[]}\n```";
        assert!(parse_review(fenced).is_ok());
    }

    #[test]
    fn rejects_trailing_data() {
        let bad = r#"{"summary":"s","decision":"ready_for_human","findings":[]} trailing"#;
        assert!(parse_review(bad).is_err());
    }
}
