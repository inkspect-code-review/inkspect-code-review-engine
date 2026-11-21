//! Review model transport, input counting and strict response parsing.
//!
//! Each inference receives a system prompt and a bounded untrusted payload,
//! and returns a JSON object with findings and presentation data.
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
use aws_smithy_types::error::metadata::ProvideErrorMetadata;
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
    pub resolved_findings: Vec<crate::domain::FindingResolution>,
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
    review_calls: std::cell::Cell<usize>,
}

impl ReviewModel {
    pub async fn new(region: &str, model_id: &str, max_output_tokens: i32) -> Result<ReviewModel> {
        if let Some(provider) = crate::byok::Provider::from_pipe()? {
            return Ok(Self::from_provider(provider, max_output_tokens));
        }
        let cfg = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(region.to_string()))
            // An ambiguous paid request must not be automatically replayed.
            .retry_config(aws_config::retry::RetryConfig::disabled())
            .load()
            .await;
        Ok(ReviewModel {
            client: Some(Client::new(&cfg)),
            review_calls: 0.into(),
            byok: None,
            model_id: model_id.to_string(),
            region: region.to_string(),
            max_output_tokens,
            max_response_bytes: 256 * 1024,
        })
    }

    pub(crate) fn from_provider(provider: crate::byok::Provider, max_output_tokens: i32) -> Self {
        Self {
            client: None,
            model_id: provider.model.clone(),
            region: provider.provider.clone(),
            review_calls: 0.into(),
            max_output_tokens: max_output_tokens.min(provider.max_output_tokens),
            max_response_bytes: 256 * 1024,
            byok: Some(provider),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_test_client(client: Client) -> Self {
        Self { client:Some(client),byok:None,model_id:"global.openai.gpt-6.1-sol".into(),
            region:"us-east-1".into(),max_output_tokens:4096,max_response_bytes:256*1024,review_calls:0.into() }
    }

    pub fn provider(&self) -> &str { self.byok.as_ref().map(|p| p.provider.as_str()).unwrap_or("bedrock") }
    pub fn model_id(&self) -> &str { &self.model_id }

    /// A deployment may expose a model alias without publishing its window. In
    /// that case the provider validates the whole request before inference.
    pub async fn input_limit(&self) -> Result<Option<usize>> {
        let id = self.model_id.split("openai.").last().unwrap_or(&self.model_id);
        let (window, max_input): (usize, usize) = match id {
            "gpt-5.4-mini" => (400_000, 272_000),
            "gpt-5.4" | "gpt-5.5" => (1_050_000, 1_050_000),
            "gpt-6-sol" | "gpt-6-astra" | "gpt-6-luna"
            | "gpt-6.1-sol" | "gpt-6.1-terra" | "gpt-6.2-sol" => (1_050_000, 922_000),
            _ => {
                if let Some(provider) = &self.byok {
                    return provider.input_limit(self.max_output_tokens).await;
                }
                return Ok(None);
            }
        };
        Ok(Some(window.saturating_sub(self.max_output_tokens as usize).min(max_input)))
    }

    pub async fn count_input(&self, system: &str, user: &str) -> Result<Option<usize>> {
        if let Some(provider) = &self.byok { return provider.count_input(system, user).await.map(Some); }
        let message = Message::builder().role(ConversationRole::User)
            .content(ContentBlock::Text(user.into())).build().map_err(|_| err(Code::Model,"invalid count request"))?;
        let input = aws_sdk_bedrockruntime::types::ConverseTokensRequest::builder()
            .system(SystemContentBlock::Text(system.into())).messages(message).build();
        let result = self.client.as_ref().ok_or_else(|| err(Code::Model,"missing token counter"))?
            .count_tokens().model_id(&self.model_id)
            .input(aws_sdk_bedrockruntime::types::CountTokensInput::Converse(input)).send().await;
        match result {
            Ok(out) if out.input_tokens() > 0 => Ok(Some(out.input_tokens() as usize)),
            // Some inference profiles support Converse but have no CountTokens
            // endpoint. The inference endpoint can still validate the exact input.
            Err(e) if matches!(e.raw_response().map(|r| r.status().as_u16()), Some(400 | 403 | 404 | 501)) => Ok(None),
            Err(e) if e.raw_response().is_some_and(|r| r.status().as_u16()==413) => Ok(Some(usize::MAX)),
            _ => Err(err(Code::Model, "model token count unavailable")),
        }
    }

    /// Make a Converse call and return the parsed review.
    pub async fn review(&self, system: &str, user: &str, job: &crate::config::ReviewJob) -> Result<RawReview> {
        parse_review(&self.complete(system, user, job).await?)
    }

    pub async fn complete(&self, system: &str, user: &str, job: &crate::config::ReviewJob) -> Result<String> {
        self.complete_captured(system, user, job, None, false).await
    }

    // A fresh, stateless call: no generator conversation or claimed verdict is
    // forwarded. Reuse only the authenticated transport (including BYOK).
    pub async fn complete_fix(&self, system: &str, user: &str, job: &crate::config::ReviewJob, attempt: usize, verification: bool) -> Result<String> {
        self.complete_captured(system, user, job, Some(attempt), verification).await
    }

    async fn complete_captured(&self, system: &str, user: &str, job: &crate::config::ReviewJob, attempt: Option<usize>, verification: bool) -> Result<String> {
        if let Some(attempt) = attempt { crate::audit::capture_fix_call(job, &self.model_id, &self.region, self.max_output_tokens, system, user, attempt, verification)?; }
        else { let part = self.review_calls.get() + 1; self.review_calls.set(part); crate::audit::capture(part, job, &self.model_id, &self.region, self.max_output_tokens, system, user)?; }
        let receipt = crate::telemetry::next_id();
        crate::telemetry::record(serde_json::json!({"kind":"model.started", "id":receipt, "attempt":attempt, "phase":if verification {"fix_verification"} else {"generation"}}))?;
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
                let _ = crate::audit::capture_model_receipt(receipt, &serde_json::json!({
                    "provider":"bedrock", "attempt":attempt, "phase":if verification {"fix_verification"} else {"generation"},
                    "outcome":"request_failed", "http_status":e.raw_response().map(|r| r.status().as_u16()),
                    "provider_error_code":e.as_service_error().and_then(|s|s.code())
                }));
                let service = e.as_service_error();
                let status = e.raw_response().map(|r| r.status().as_u16());
                if status == Some(413) || (status == Some(400)
                    && service.and_then(|s| s.code()) == Some("ValidationException")
                    && service.and_then(|s|s.message()).is_some_and(is_context_rejection)) {
                    return context_rejection(receipt);
                }
                err(Code::Model, "converse call failed")
            })?;

        if let Some(usage) = out.usage() {
            crate::telemetry::record(serde_json::json!({"kind":"model.usage", "id":receipt, "request_id":out.request_id(), "measurement_source":"bedrock_converse", "input_tokens":i64::from(usage.input_tokens()) + i64::from(usage.cache_read_input_tokens().unwrap_or(0)) + i64::from(usage.cache_write_input_tokens().unwrap_or(0)), "output_tokens":usage.output_tokens(), "cached_input_tokens":usage.cache_read_input_tokens().unwrap_or(0), "cache_write_input_tokens":usage.cache_write_input_tokens().unwrap_or(0), "uncached_input_tokens":usage.input_tokens()}))?;
        }
        // Retain provider text and stop metadata BEFORE any rejection or JSON
        // parsing, including truncated/refused and otherwise malformed replies.
        let message = match out.output() {
            Some(aws_sdk_bedrockruntime::types::ConverseOutput::Message(m)) => Some(m),
            _ => None,
        };
        let mut text = String::new();
        let mut oversized = false;
        if let Some(message) = message {
            for block in message.content() {
                if let ContentBlock::Text(t) = block {
                    if text.len() + t.len() > self.max_response_bytes { oversized = true; break; }
                    text.push_str(t);
                }
                // Reasoning is not part of the repair response and is not retained.
            }
        }
        crate::audit::capture_model_receipt(receipt, &serde_json::json!({
            "provider":"bedrock", "attempt":attempt, "phase":if verification {"fix_verification"} else {"generation"},
            "request_id":out.request_id(), "stop_reason":out.stop_reason().as_str(),
            "output_role":message.map(|m|m.role().as_str()),"response":text,"capture_truncated":oversized,
            "usage":out.usage().map(|u|serde_json::json!({"input_tokens":u.input_tokens(),"output_tokens":u.output_tokens()}))
        }))?;
        if let Some(attempt) = attempt { crate::audit::capture_fix_stop(attempt, verification, out.stop_reason().as_str())?; }
        if out.stop_reason() != &StopReason::EndTurn { return Err(err(Code::Model, "model response was truncated or refused")); }
        let message = message.ok_or_else(||err(Code::Model,"malformed model output"))?;
        if message.role() != &ConversationRole::Assistant { return Err(err(Code::Model,"unexpected response role")); }
        if oversized { return Err(err(Code::Model,"model response too large")); }

        Ok(text)
    }
}

pub(crate) const CONTEXT_LIMIT: &str = "model rejected input length before inference";
pub(crate) fn is_context_rejection(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    ["input is too long", "input too long", "too many input tokens", "input length exceeds",
     "maximum context length", "context length exceeded", "prompt is too long",
     "input token count exceeds", "exceeds the model context", "input is too large",
     "input is longer than", "too many tokens in the input"].iter().any(|s| lower.contains(s))
}
pub(crate) fn context_rejection(receipt: usize) -> crate::error::EngineError {
    if let Err(e) = crate::telemetry::record(serde_json::json!({"kind":"model.input_rejected","id":receipt})) { return e; }
    err(Code::Model, CONTEXT_LIMIT)
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
