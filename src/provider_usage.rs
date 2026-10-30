//! Capture only billing metadata, never response bodies or authorization headers.
use aws_sdk_bedrockruntime::config::{Intercept, interceptors::BeforeDeserializationInterceptorContextRef};
use aws_smithy_runtime_api::client::runtime_components::RuntimeComponents;
use aws_smithy_types::config_bag::ConfigBag;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Default)]
pub struct ProviderUsage(pub Arc<Mutex<serde_json::Value>>);
impl Intercept for ProviderUsage {
    fn name(&self) -> &'static str { "ProviderUsage" }
    fn read_before_deserialization(&self, ctx: &BeforeDeserializationInterceptorContextRef<'_>, _: &RuntimeComponents, _: &mut ConfigBag) -> std::result::Result<(), aws_smithy_runtime_api::box_error::BoxError> {
        let response = ctx.response();
        let h = response.headers();
        let mut value = serde_json::json!({"http_status":response.status().as_u16()});
        for (header, key) in [("x-amzn-requestid", "request_id"), ("x-amzn-bedrock-input-token-count", "input_tokens"), ("x-amzn-bedrock-output-token-count", "output_tokens")] {
            if let Some(v) = h.get(header) {
                if key == "request_id" { value[key] = v.into(); }
                else if let Ok(n) = v.parse::<u64>() { value[key] = n.into(); }
            }
        }
        *self.0.lock().map_err(|_| "usage lock poisoned")? = value;
        Ok(())
    }
}
impl ProviderUsage {
    fn receipt(&self, kind: &str, id: usize, body: Option<&serde_json::Value>) -> serde_json::Value {
        let mut value = self.0.lock().unwrap().clone();
        if !value.is_object() { value = serde_json::json!({}); }
        value["kind"] = kind.into(); value["id"] = id.into();
        value["measurement_source"] = "bedrock_headers".into();
        if let Some(body) = body {
            for key in ["input_tokens", "search_units"] {
                if let Some(n) = body.pointer(&format!("/meta/billed_units/{key}")).and_then(|v| v.as_u64()) {
                    value[key] = n.into(); value["measurement_source"] = "provider_billed_units".into();
                }
            }
        }
        value
    }
    pub fn record(&self, kind: &str, id: usize, body: Option<&serde_json::Value>) -> crate::error::Result<()> {
        crate::telemetry::record(self.receipt(kind, id, body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn native_bedrock_embed_without_meta_keeps_header_tokens() {
        let usage = ProviderUsage::default();
        *usage.0.lock().unwrap() = json!({"http_status":200,"input_tokens":9,"request_id":"request"});
        let receipt = usage.receipt("embedding.usage", 1, Some(&json!({"embeddings":{"float":[]}})));
        assert_eq!(receipt["input_tokens"], 9);
        assert_eq!(receipt["request_id"], "request");
        assert_eq!(receipt["measurement_source"], "bedrock_headers");
    }
    #[test]
    fn explicit_billed_units_override_headers_without_double_counting() {
        let usage = ProviderUsage::default();
        *usage.0.lock().unwrap() = json!({"http_status":200,"input_tokens":15});
        let receipt = usage.receipt("embedding.usage", 2, Some(&json!({"meta":{"billed_units":{"input_tokens":9}}})));
        assert_eq!(receipt["input_tokens"], 9);
        let rank = usage.receipt("rerank.usage", 3, Some(&json!({"meta":{"billed_units":{"search_units":2}}})));
        assert_eq!(rank["search_units"], 2);
    }
    #[test]
    fn missing_usage_stays_missing_instead_of_zero() {
        let receipt = ProviderUsage::default().receipt("rerank.usage", 4, Some(&json!({"results":[]})));
        assert!(receipt.get("input_tokens").is_none());
        assert!(receipt.get("search_units").is_none());
    }
}
