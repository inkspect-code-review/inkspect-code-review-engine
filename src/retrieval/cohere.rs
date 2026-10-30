//! Cohere Embed v4 and Rerank 3.5 via Bedrock InvokeModel.
//!
//! Request and response bodies match the Go engine (engine/internal/retrieval/
//! cohere.go) so vectors land in the same embedding space and the reranker
//! behaves identically:
//!   * embed: {"texts":[..],"input_type":"search_query","embedding_types":
//!            ["float"],"output_dimension":N,"truncate":"NONE"}
//!   * rerank: {"api_version":2,"query":q,"documents":[..],"top_n":N}
//!
//! Client-side L2 normalization is applied to embeddings, matching Go.

use aws_sdk_bedrockruntime::primitives::Blob;
use aws_sdk_bedrockruntime::Client;

use super::{Embedder, Reranker};
use crate::error::{err, Code, Result};

pub struct Bedrock {
    embed_client: Client,
    rerank_client: Client,
    embed_model: String,
    rerank_model: String,
    dimensions: u32,
}

impl Bedrock {
    pub async fn new(
        embed_region: &str,
        embed_model: &str,
        rerank_region: &str,
        rerank_model: &str,
        dimensions: u32,
    ) -> Result<Bedrock> {
        // Each paid attempt gets its own durable receipt. Document retry/backoff
        // happens above embed_batch; hidden SDK retries would lose attribution.
        let embed_cfg = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .retry_config(aws_config::retry::RetryConfig::standard().with_max_attempts(1))
            .region(aws_config::Region::new(embed_region.to_string()))
            .load()
            .await;
        let rerank_cfg = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .retry_config(aws_config::retry::RetryConfig::standard().with_max_attempts(1))
            .region(aws_config::Region::new(rerank_region.to_string()))
            .load()
            .await;
        Ok(Bedrock {
            embed_client: Client::new(&embed_cfg),
            rerank_client: Client::new(&rerank_cfg),
            embed_model: embed_model.to_string(),
            rerank_model: rerank_model.to_string(),
            dimensions,
        })
    }

    /// Embed a batch of texts with the given input_type, applying the same
    /// validation, output_dimension, truncate and client-side L2 normalization
    /// as the Go engine. input_type is "search_query" or "search_document".
    async fn embed_batch(&self, texts: &[String], input_type: &str) -> Result<Vec<Vec<f32>>> {
        // Match Go limits: 1..=96 texts, each 1..=8192 bytes, <=120000 total.
        if texts.is_empty() || texts.len() > 96 {
            return Err(err(Code::Retrieval, "invalid embedding batch size"));
        }
        let mut total = 0usize;
        for t in texts {
            if t.trim().is_empty() || t.len() > 8192 {
                return Err(err(Code::Retrieval, "invalid embedding input"));
            }
            total += t.len();
        }
        if total > 120_000 {
            return Err(err(Code::Retrieval, "embedding batch too large"));
        }
        let body = serde_json::json!({
            "texts": texts,
            "input_type": input_type,
            "embedding_types": ["float"],
            "output_dimension": self.dimensions,
            "truncate": "NONE",
        });
        let receipt = crate::telemetry::next_id();
        crate::telemetry::record(serde_json::json!({"kind":"embedding.started","id":receipt,"model":self.embed_model,"units":texts.len(),"purpose":input_type}))?;
        let usage = crate::provider_usage::ProviderUsage::default();
        let out = self
            .embed_client
            .invoke_model()
            .model_id(&self.embed_model)
            .content_type("application/json")
            .accept("application/json")
            .body(Blob::new(serde_json::to_vec(&body).unwrap()))
            .customize().interceptor(usage.clone())
            .send()
            .await
            .map_err(|e| {
                let _ = usage.record("embedding.failed", receipt, None);
                let detail = aws_smithy_types::error::display::DisplayErrorContext(&e);
                tracing::debug!(texts = texts.len(), "embedding call failed: {detail}");
                err(Code::Retrieval, "embedding call failed")
            })?;

        usage.record("embedding.response", receipt, None)?;
        let raw: serde_json::Value = serde_json::from_slice(out.body().as_ref())
            .map_err(|_| err(Code::Retrieval, "embedding decode failed"))?;
        usage.record("embedding.usage", receipt, Some(&raw))?;
        let resp: EmbedResponse = serde_json::from_value(raw)
            .map_err(|_| err(Code::Retrieval, "embedding decode failed"))?;
        if resp.embeddings.float.len() != texts.len() {
            return Err(err(Code::Retrieval, "embedding count mismatch"));
        }
        let mut out_vecs = Vec::with_capacity(texts.len());
        for mut vector in resp.embeddings.float {
            if vector.len() != self.dimensions as usize {
                return Err(err(Code::Retrieval, "embedding dimension mismatch"));
            }
            let norm: f64 = vector.iter().map(|x| (*x as f64) * (*x as f64)).sum::<f64>().sqrt();
            if norm == 0.0 || !norm.is_finite() {
                return Err(err(Code::Retrieval, "non-finite embedding"));
            }
            for x in &mut vector {
                *x = (*x as f64 / norm) as f32;
            }
            out_vecs.push(vector);
        }
        Ok(out_vecs)
    }

    async fn embed_query_async(&self, text: &str) -> Result<Vec<f32>> {
        let v = self
            .embed_batch(std::slice::from_ref(&text.to_string()), "search_query")
            .await?;
        v.into_iter()
            .next()
            .ok_or_else(|| err(Code::Retrieval, "empty embedding response"))
    }

    /// Embed repository chunks as documents (input_type "search_document"),
    /// retrying on throttling with generous exponential backoff. Bedrock
    /// enforces a tokens-per-minute quota, so backoff starts in the seconds.
    pub async fn embed_documents_async(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut attempt = 0u32;
        loop {
            match self.embed_batch(texts, "search_document").await {
                Ok(v) => return Ok(v),
                Err(e) if attempt < 7 => {
                    attempt += 1;
                    // 4s, 8s, 16s, 32s, 60s, 60s, 60s.
                    let secs = (4u64 << (attempt - 1)).min(60);
                    tracing::debug!(attempt, "throttled; retrying document embedding in {secs}s: {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub async fn rerank_scored_async(
        &self,
        query: &str,
        documents: &[String],
        top_n: usize,
    ) -> Result<Vec<(usize, f64)>> {
        if query.trim().is_empty()
            || query.len() > 6000
            || documents.is_empty()
            || documents.len() > 50
            || top_n < 1
            || top_n > documents.len()
        {
            return Err(err(Code::Retrieval, "invalid rerank request"));
        }
        let body = serde_json::json!({
            "api_version": 2,
            "query": query,
            "documents": documents,
            "top_n": top_n,
        });
        let receipt = crate::telemetry::next_id();
        crate::telemetry::record(serde_json::json!({"kind":"rerank.started","id":receipt,"model":self.rerank_model,"units":documents.len()}))?;
        let usage = crate::provider_usage::ProviderUsage::default();
        let out = self
            .rerank_client
            .invoke_model()
            .model_id(&self.rerank_model)
            .content_type("application/json")
            .accept("application/json")
            .body(Blob::new(serde_json::to_vec(&body).unwrap()))
            .customize().interceptor(usage.clone())
            .send()
            .await
            .map_err(|_| { let _ = usage.record("rerank.failed", receipt, None); err(Code::Retrieval, "rerank call failed") })?;

        usage.record("rerank.response", receipt, None)?;
        let raw: serde_json::Value = serde_json::from_slice(out.body().as_ref())
            .map_err(|_| err(Code::Retrieval, "rerank decode failed"))?;
        usage.record("rerank.usage", receipt, Some(&raw))?;
        let resp: RerankResponse = serde_json::from_value(raw)
            .map_err(|_| err(Code::Retrieval, "rerank decode failed"))?;
        if resp.results.len() != top_n {
            return Err(err(Code::Retrieval, "rerank result count mismatch"));
        }
        let mut seen = std::collections::HashSet::new();
        let mut last = f64::INFINITY;
        let mut order = Vec::with_capacity(top_n);
        for r in resp.results {
            if r.index >= documents.len()
                || !seen.insert(r.index)
                || !(0.0..=1.0).contains(&r.relevance_score)
                || r.relevance_score > last
            {
                return Err(err(Code::Retrieval, "invalid rerank response"));
            }
            last = r.relevance_score;
            order.push((r.index, r.relevance_score));
        }
        Ok(order)
    }
}

impl Embedder for Bedrock {
    fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        block_on(self.embed_query_async(text))
    }
}

impl Reranker for Bedrock {
    fn rerank(&self, query: &str, documents: &[String]) -> Result<Vec<(usize, f64)>> {
        block_on(self.rerank_scored_async(query, documents, documents.len()))
    }
}

#[derive(serde::Deserialize)]
struct EmbedResponse {
    embeddings: EmbedFloats,
}

#[derive(serde::Deserialize)]
struct EmbedFloats {
    #[serde(default)]
    float: Vec<Vec<f32>>,
}

#[derive(serde::Deserialize)]
struct RerankResponse {
    #[serde(default)]
    results: Vec<RerankResult>,
}

#[derive(serde::Deserialize)]
struct RerankResult {
    index: usize,
    relevance_score: f64,
}

fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
}
