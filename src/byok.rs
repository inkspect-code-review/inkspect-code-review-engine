//! Fixed-host BYOK transports. Credentials arrive on a private pipe, never in
//! jobs, journals, arguments, environment variables or provider error logs.
use crate::error::{err, Code, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::Read;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub provider: String,
    pub model: String,
    api_key: String,
    pub max_output_tokens: i32,
}
impl Provider {
    pub fn from_pipe() -> Result<Option<Self>> {
        if std::env::var("INKSPECT_BYOK_STDIN").ok().as_deref() != Some("1") { return Ok(None); }
        let mut bytes = Vec::new();
        std::io::stdin().take(8193).read_to_end(&mut bytes).map_err(|_| err(Code::Model,"credential pipe unavailable"))?;
        let parsed = serde_json::from_slice::<Self>(&bytes);
        let bounded = bytes.len() <= 8192; bytes.fill(0);
        let p = parsed.map_err(|_| err(Code::Model,"invalid provider credentials"))?;
        if !bounded || !["openai","anthropic","google"].contains(&p.provider.as_str()) || p.model.is_empty() || p.model.len()>100
            || !p.model.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b))
            || !(16..=4096).contains(&p.api_key.len()) || !p.api_key.bytes().all(|b| (33..=126).contains(&b))
            || !(1..=16384).contains(&p.max_output_tokens) {return Err(err(Code::Model,"invalid provider credentials"));}
        Ok(Some(p))
    }
    fn payload(&self, system: &str, user: &str, max: i32) -> (&'static str, Value) {
        match self.provider.as_str() {
            "openai" => ("https://api.openai.com/v1/responses", json!({"model":self.model,"instructions":system,"input":user,"max_output_tokens":max,"store":false})),
            "anthropic" => ("https://api.anthropic.com/v1/messages", json!({"model":self.model,"system":system,"messages":[{"role":"user","content":user}],"max_tokens":max})),
            _ => ("https://generativelanguage.googleapis.com", json!({"systemInstruction":{"parts":[{"text":system}]},"contents":[{"role":"user","parts":[{"text":user}]}],"generationConfig":{"maxOutputTokens":max,"responseMimeType":"application/json"}})),
        }
    }
    pub async fn complete(&self, system: &str, user: &str, max: i32, receipt: usize) -> Result<String> {
        let (host, body) = self.payload(system,user,max);
        let url = if self.provider=="google" {format!("{host}/v1beta/models/{}:generateContent",self.model)} else {host.into()};
        let client=reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).retry(reqwest::retry::never())
            .timeout(std::time::Duration::from_secs(180)).build().map_err(|_|err(Code::Model,"provider client unavailable"))?;
        let (header, secret) = match self.provider.as_str() {"openai"=>("authorization",format!("Bearer {}",self.api_key)),"anthropic"=>("x-api-key",self.api_key.clone()),_=>("x-goog-api-key",self.api_key.clone())};
        let mut value=reqwest::header::HeaderValue::from_str(&secret).map_err(|_|err(Code::Model,"invalid provider key"))?; value.set_sensitive(true);
        let mut request=client.post(url).header(header,value).json(&body);
        if self.provider=="anthropic" {request=request.header("anthropic-version","2023-06-01");}
        let mut response=request.send().await.map_err(|e| {
            let _=crate::audit::capture_model_receipt(receipt,&json!({"provider":self.provider,"outcome":"request_failed",
                "error_class":if e.is_timeout(){"timeout"}else if e.is_connect(){"connect"}else{"request"}}));
            err(Code::Model,"provider request failed")
        })?;
        let status=response.status().as_u16();
        let request_id=["x-request-id","request-id","x-goog-request-id"].iter()
            .find_map(|h|response.headers().get(*h).and_then(|v|v.to_str().ok())).map(str::to_owned);
        let mut bytes=Vec::new();
        loop {
            match response.chunk().await {
                Ok(None)=>break,
                Ok(Some(chunk))=>{
                    if bytes.len()+chunk.len()>1024*1024 {
                        crate::audit::capture_model_receipt(receipt,&response_receipt(&self.provider,status,request_id.as_deref(),"oversized",&bytes,&self.api_key))?;
                        return Err(err(Code::Model,"provider response too large"));
                    }
                    bytes.extend_from_slice(&chunk);
                },
                Err(_)=>{
                    let _=crate::audit::capture_model_receipt(receipt,&response_receipt(&self.provider,status,request_id.as_deref(),"interrupted",&bytes,&self.api_key));
                    return Err(err(Code::Model,"provider response interrupted"));
                }
            }
        }
        let parsed=serde_json::from_slice::<Value>(&bytes);
        // Usage is durable before diagnostic storage/schema validation can fail.
        if let Ok(result)=&parsed {
            if let Some((input,output,cached))=usage(&self.provider,result) {
                crate::telemetry::record(json!({"kind":"model.usage","id":receipt,"input_tokens":input,"output_tokens":output,"cached_input_tokens":cached,"measurement_source":self.provider,"provider":self.provider,"model":self.model}))?;
            }
        }
        crate::audit::capture_model_receipt(receipt,&response_receipt(&self.provider,status,request_id.as_deref(),"received",&bytes,&self.api_key))?;
        if !(200..300).contains(&status) {return Err(err(Code::Model,"provider rejected the request"));}
        let result=parsed.map_err(|_|err(Code::Model,"invalid provider response"))?;

        text(&self.provider,&result)
    }
}
fn response_receipt(provider:&str,status:u16,request_id:Option<&str>,outcome:&str,bytes:&[u8],key:&str)->Value {
    let redacted=String::from_utf8_lossy(bytes).replace(key,"[REDACTED]");
    json!({"provider":provider,"http_status":status,"request_id":request_id,"outcome":outcome,"body":redacted})
}

fn usage(provider:&str, v:&Value) -> Option<(u64,u64,u64)> {
    let counts=match provider {
        "openai" => (v["usage"]["input_tokens"].as_u64()?,v["usage"]["output_tokens"].as_u64()?,v["usage"]["input_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0)),
        "anthropic" => {
            let u=&v["usage"]; let cached=u["cache_read_input_tokens"].as_u64().unwrap_or(0);
            (u["input_tokens"].as_u64()?.checked_add(cached)?.checked_add(u["cache_creation_input_tokens"].as_u64().unwrap_or(0))?,u["output_tokens"].as_u64()?,cached)
        },
        _ => {let u=&v["usageMetadata"];let input=u["promptTokenCount"].as_u64()?;
            (input,u["totalTokenCount"].as_u64()?.checked_sub(input)?,u["cachedContentTokenCount"].as_u64().unwrap_or(0))},
    };
    if counts.0>100_000_000 || counts.1>100_000_000 || counts.2>counts.0 {return None;} Some(counts)
}
fn text(provider:&str,v:&Value) -> Result<String> {
    let failure=||err(Code::Model,"provider response incomplete or refused");
    let mut out=String::new();
    match provider {
        "openai" => {
            if v["status"]!="completed" {return Err(failure());}
            for item in v["output"].as_array().ok_or_else(failure)? {
                if item["type"]=="message" && item["role"]=="assistant" {
                    for part in item["content"].as_array().ok_or_else(failure)? {
                        if part["type"]!="output_text" {return Err(failure());} out.push_str(part["text"].as_str().ok_or_else(failure)?);
                    }
                }
            }
        },
        "anthropic" => {
            if v["stop_reason"]!="end_turn" {return Err(failure());}
            for part in v["content"].as_array().ok_or_else(failure)? { if part["type"]=="text" {out.push_str(part["text"].as_str().ok_or_else(failure)?);} }
        },
        _ => {
            let candidates=v["candidates"].as_array().ok_or_else(failure)?;
            if candidates.len()!=1 || candidates[0]["finishReason"]!="STOP" {return Err(failure());}
            for part in candidates[0]["content"]["parts"].as_array().ok_or_else(failure)? {
                if part["thought"]==true {continue;} out.push_str(part["text"].as_str().ok_or_else(failure)?);
            }
        }
    }
    if out.is_empty() || out.len()>256*1024 {return Err(failure());} Ok(out)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn all_provider_responses_are_strict_and_usage_includes_reasoning_and_cache() {
        let o=json!({"status":"completed","output":[{"type":"reasoning"},{"type":"message","role":"assistant","content":[{"type":"output_text","text":"{}"}]}],"usage":{"input_tokens":12,"output_tokens":8}});
        assert_eq!(text("openai",&o).unwrap(),"{}"); assert_eq!(usage("openai",&o),Some((12,8,0)));
        let a=json!({"stop_reason":"end_turn","content":[{"type":"text","text":"{}"}],"usage":{"input_tokens":10,"cache_read_input_tokens":5,"cache_creation_input_tokens":3,"output_tokens":4}});
        assert_eq!(text("anthropic",&a).unwrap(),"{}"); assert_eq!(usage("anthropic",&a),Some((18,4,5)));
        let g=json!({"candidates":[{"finishReason":"STOP","content":{"parts":[{"text":"hidden","thought":true},{"text":"{}"}]}}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":4,"thoughtsTokenCount":8,"totalTokenCount":22}});
        assert_eq!(text("google",&g).unwrap(),"{}"); assert_eq!(usage("google",&g),Some((10,12,0)));
        for p in ["openai","anthropic","google"] {assert!(text(p,&json!({})).is_err());assert!(usage(p,&json!({})).is_none());}
        let mut truncated=o;truncated["status"]=json!("incomplete");assert!(text("openai",&truncated).is_err());
    }
    #[test] fn failure_receipts_keep_evidence_without_the_credential() {
        let v=response_receipt("openai",429,Some("req-test"),"received",br#"{"error":{"code":"rate_limit","message":"key private-credential"}}"#,"private-credential");
        assert_eq!(v["http_status"],429);assert_eq!(v["request_id"],"req-test");
        assert!(v["body"].as_str().unwrap().contains("rate_limit"));assert!(!v.to_string().contains("private-credential"));
    }
    #[test] fn request_envelopes_keep_instructions_outside_untrusted_input() {
        for provider in ["openai","anthropic","google"] {
            let p=Provider{provider:provider.into(),model:"test-model".into(),api_key:"secret".into(),max_output_tokens:1024};
            let (url,v)=p.payload("trusted","untrusted",1024);
            assert!(url.starts_with("https://")); assert!(!v.to_string().contains("secret"));
            if provider=="openai" {assert_eq!(v["store"],false);assert_eq!(v["instructions"],"trusted");}
        }
    }
}
