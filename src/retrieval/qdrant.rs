//! Minimal Qdrant REST client, read-only against the shared repository index.
//!
//! Endpoint and api-key come from the environment (INKSPECT_QDRANT_URL,
//! INKSPECT_QDRANT_API_KEY), never from a job. Only the two reads the review
//! path needs are implemented: manifest lookup and vector search. Writes are
//! owned by the Go repository-index worker.

use serde::Deserialize;

use super::hash::GoJson;
use super::{chunk_point_id, Chunk, Namespace, Schema};
use crate::error::{err, Code, Result};

const MANIFEST_PREFIX: &str = "manifest";

#[cfg(test)]
#[path = "review_pipeline_tests.rs"]
mod review_pipeline_tests;

/// Resolved manifest describing the current index snapshot for a repository.
pub struct Manifest {
    pub snapshot: String,
}

pub struct Qdrant {
    endpoint: String,
    api_key: Option<String>,
    client: reqwest::Client,
}

impl Qdrant {
    /// Build from environment. Enforces the same endpoint rules as the Go
    /// engine: https, or http only for loopback hosts.
    pub fn from_env() -> Result<Qdrant> {
        let endpoint =
            std::env::var("INKSPECT_QDRANT_URL").unwrap_or_else(|_| "http://127.0.0.1:6333".into());
        let parsed = reqwest::Url::parse(&endpoint)
            .map_err(|_| err(Code::Retrieval, "invalid qdrant endpoint"))?;
        let host = parsed.host_str().unwrap_or("");
        let loopback = matches!(host, "localhost" | "127.0.0.1" | "::1");
        let ok = match parsed.scheme() {
            "https" => true,
            "http" => loopback,
            _ => false,
        };
        if !ok || parsed.query().is_some() {
            return Err(err(Code::Retrieval, "unsupported qdrant endpoint"));
        }
        let api_key = std::env::var("INKSPECT_QDRANT_API_KEY").ok().filter(|k| !k.is_empty());
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| err(Code::Retrieval, "qdrant client build failed"))?;
        Ok(Qdrant {
            endpoint: endpoint.trim_end_matches('/').to_string(),
            api_key,
            client,
        })
    }

    fn collection_route(&self, schema: &Schema) -> String {
        format!("/collections/{}", schema.collection())
    }

    async fn post<B: serde::Serialize>(&self, route: &str, body: &B) -> Result<serde_json::Value> {
        let mut req = self
            .client
            .post(format!("{}{}", self.endpoint, route))
            .json(body);
        if let Some(key) = &self.api_key {
            req = req.header("api-key", key);
        }
        let res = req
            .send()
            .await
            .map_err(|_| err(Code::Retrieval, "qdrant unavailable"))?;
        let status = res.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(serde_json::Value::Null);
        }
        if !status.is_success() {
            let detail = res.text().await.unwrap_or_default();
            tracing::debug!(route, %status, "qdrant request failed: {}", detail.chars().take(400).collect::<String>());
            return Err(err(Code::Retrieval, "qdrant request failed"));
        }
        let envelope: QdrantEnvelope = res
            .json()
            .await
            .map_err(|_| err(Code::Retrieval, "qdrant response decode failed"))?;
        if envelope.status != "ok" {
            return Err(err(Code::Retrieval, "qdrant status not ok"));
        }
        Ok(envelope.result)
    }

    /// Fetch the repository manifest point. Returns None when no index exists.
    pub fn manifest(&self, ns: &Namespace, schema: &Schema) -> Result<Option<Manifest>> {
        let id = super::hash::point_id(&GoJson::Array(vec![
            GoJson::s(MANIFEST_PREFIX),
            ns.go_json(),
            schema.go_json(),
        ]));
        let route = format!("{}/points", self.collection_route(schema));
        let body = serde_json::json!({
            "ids": [id],
            "with_payload": true,
            "with_vector": false,
        });
        let result = block_on(self.post(&route, &body))?;
        if result.is_null() {
            return Ok(None);
        }
        let points: Vec<RawPoint> = serde_json::from_value(result)
            .map_err(|_| err(Code::Retrieval, "manifest decode failed"))?;
        let point = match points.into_iter().next() {
            Some(p) => p,
            None => return Ok(None),
        };
        let kind = point.payload.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        let snapshot = point
            .payload
            .get("snapshot")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if kind != "manifest" || snapshot.is_empty() {
            return Err(err(Code::Retrieval, "invalid manifest payload"));
        }
        Ok(Some(Manifest {
            snapshot: snapshot.to_string(),
        }))
    }

    /// Vector search within the pinned snapshot. Enforces the full scope filter
    /// (kind, namespace, schema, snapshot) exactly like the Go engine.
    pub fn query(
        &self,
        ns: &Namespace,
        schema: &Schema,
        snapshot: &str,
        vector: &[f32],
        limit: usize,
    ) -> Result<Vec<Chunk>> {
        self.query_inner(ns, schema, snapshot, vector, limit, false)
    }

    /// Inspection samples files, so a long generated file cannot occupy the
    /// entire candidate budget. Uses the existing payload; no re-index needed.
    pub fn query_files(
        &self, ns: &Namespace, schema: &Schema, snapshot: &str,
        vector: &[f32], limit: usize,
    ) -> Result<Vec<Chunk>> {
        self.query_inner(ns, schema, snapshot, vector, limit, true)
    }

    fn query_inner(
        &self, ns: &Namespace, schema: &Schema, snapshot: &str,
        vector: &[f32], limit: usize, grouped: bool,
    ) -> Result<Vec<Chunk>> {
        let filter = serde_json::json!({
            "must": [
                {"key": "kind", "match": {"value": "chunk"}},
                {"key": "namespace.tenant", "match": {"value": ns.tenant}},
                {"key": "namespace.repository", "match": {"value": ns.repository}},
                {"key": "embedding_schema.model", "match": {"value": schema.model}},
                {"key": "embedding_schema.version", "match": {"value": schema.version}},
                {"key": "embedding_schema.dimensions", "match": {"value": schema.dimensions}},
                {"key": "snapshot", "match": {"value": snapshot}},
            ]
        });
        let route = format!("{}/points/query{}", self.collection_route(schema), if grouped { "/groups" } else { "" });
        let mut body = serde_json::json!({
            "query": vector,
            "filter": filter,
            "limit": limit,
            "with_payload": true,
            "with_vector": false,
        });
        if grouped {
            body["group_by"] = serde_json::json!("chunk.path");
            body["group_size"] = serde_json::json!(1);
        }
        let result = block_on(self.post(&route, &body))?;
        if result.is_null() {
            return Ok(Vec::new());
        }
        let points = query_points(result, grouped, limit)?;
        if points.len() > limit {
            return Err(err(Code::Retrieval, "query returned too many points"));
        }

        let mut chunks = Vec::with_capacity(points.len());
        for p in points {
            let payload = p.payload;
            if payload.get("kind").and_then(|v| v.as_str()) != Some("chunk") {
                return Err(err(Code::Retrieval, "non-chunk in query scope"));
            }
            let chunk_obj = payload
                .get("chunk")
                .ok_or_else(|| err(Code::Retrieval, "payload missing chunk"))?;
            let content = payload
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let chunk = Chunk {
                key: field_str(chunk_obj, "key")?,
                path: field_str(chunk_obj, "path")?,
                start_line: field_u32(chunk_obj, "start_line")?,
                end_line: field_u32(chunk_obj, "end_line")?,
                file_sha256: chunk_obj
                    .get("file_sha256")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                content,
            };
            // Validate the id matches the Go-derived chunk point id.
            let expected = chunk_point_id(ns, schema, snapshot, &chunk.key);
            if p.id != expected {
                return Err(err(Code::Retrieval, "chunk point id mismatch"));
            }
            chunks.push(chunk);
        }
        Ok(chunks)
    }

    // ---- Write path (repository indexing) --------------------------------
    //
    // These mirror the Go engine's Ensure / upsert / cleanup so the index the
    // Rust engine writes is byte-for-byte acceptable to the Go read path (and
    // to this engine's own reader above).

    async fn put(&self, route: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
        let mut req = self
            .client
            .put(format!("{}{}", self.endpoint, route))
            .json(body);
        if let Some(key) = &self.api_key {
            req = req.header("api-key", key);
        }
        let res = req
            .send()
            .await
            .map_err(|_| err(Code::Retrieval, "qdrant unavailable"))?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(serde_json::Value::Null);
        }
        if !res.status().is_success() {
            return Err(err(Code::Retrieval, "qdrant put failed"));
        }
        let envelope: QdrantEnvelope = res
            .json()
            .await
            .map_err(|_| err(Code::Retrieval, "qdrant response decode failed"))?;
        if envelope.status != "ok" {
            return Err(err(Code::Retrieval, "qdrant status not ok"));
        }
        Ok(envelope.result)
    }

    async fn get_collection(&self, schema: &Schema) -> Result<Option<serde_json::Value>> {
        let mut req = self
            .client
            .get(format!("{}{}", self.endpoint, self.collection_route(schema)));
        if let Some(key) = &self.api_key {
            req = req.header("api-key", key);
        }
        let res = req
            .send()
            .await
            .map_err(|_| err(Code::Retrieval, "qdrant unavailable"))?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !res.status().is_success() {
            return Err(err(Code::Retrieval, "qdrant collection read failed"));
        }
        let envelope: QdrantEnvelope = res
            .json()
            .await
            .map_err(|_| err(Code::Retrieval, "qdrant response decode failed"))?;
        Ok(Some(envelope.result))
    }

    /// Ensure the collection exists with the exact size/distance/metadata the Go
    /// engine requires. Creates it if missing (create=true).
    pub async fn ensure(&self, schema: &Schema) -> Result<()> {
        if let Some(cfg) = self.get_collection(schema).await? {
            let size = cfg
                .pointer("/config/params/vectors/size")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let distance = cfg
                .pointer("/config/params/vectors/distance")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if size != schema.dimensions || distance != "Cosine" {
                return Err(err(Code::Retrieval, "collection schema mismatch"));
            }
            return Ok(());
        }
        // Create, then re-check.
        let body = serde_json::json!({
            "vectors": {"size": schema.dimensions, "distance": "Cosine"},
            "metadata": {"inkspect_embedding": {
                "version": schema.version,
                "model": schema.model,
                "dimensions": schema.dimensions,
                "normalize": schema.normalize,
            }},
        });
        self.put(&self.collection_route(schema), &body).await?;
        Ok(())
    }

    /// Upsert points (chunk or manifest). Each `point` is a full JSON object
    /// `{id, vector, payload}`. Waits for completion.
    pub async fn upsert(&self, schema: &Schema, points: Vec<serde_json::Value>) -> Result<()> {
        // Point upsert is PUT; POST /points is the "retrieve by ids" endpoint.
        let route = format!("{}/points?wait=true", self.collection_route(schema));
        let body = serde_json::json!({ "points": points });
        let result = self.put(&route, &body).await?;
        if result.get("status").and_then(|v| v.as_str()) != Some("completed") {
            return Err(err(Code::Retrieval, "qdrant write incomplete"));
        }
        Ok(())
    }

    /// Delete chunk points of every snapshot except the current one, matching
    /// the Go cleanup filter (must = scope minus snapshot, must_not = snapshot).
    pub async fn delete_old_snapshots(
        &self,
        ns: &Namespace,
        schema: &Schema,
        snapshot: &str,
    ) -> Result<()> {
        let filter = serde_json::json!({
            "must": [
                {"key": "kind", "match": {"value": "chunk"}},
                {"key": "namespace.tenant", "match": {"value": ns.tenant}},
                {"key": "namespace.repository", "match": {"value": ns.repository}},
                {"key": "embedding_schema.model", "match": {"value": schema.model}},
                {"key": "embedding_schema.version", "match": {"value": schema.version}},
                {"key": "embedding_schema.dimensions", "match": {"value": schema.dimensions}},
            ],
            "must_not": [
                {"key": "snapshot", "match": {"value": snapshot}}
            ],
        });
        let route = format!("{}/points/delete?wait=true", self.collection_route(schema));
        let body = serde_json::json!({ "filter": filter });
        let result = self.post(&route, &body).await?;
        if result.get("status").and_then(|v| v.as_str()) != Some("completed") {
            return Err(err(Code::Retrieval, "qdrant delete incomplete"));
        }
        Ok(())
    }

    /// The manifest point id, matching Go `pointID(["manifest", ns, schema])`.
    pub fn manifest_id(ns: &Namespace, schema: &Schema) -> String {
        super::hash::point_id(&GoJson::Array(vec![
            GoJson::s(MANIFEST_PREFIX),
            ns.go_json(),
            schema.go_json(),
        ]))
    }
}

fn field_str(v: &serde_json::Value, key: &str) -> Result<String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| err(Code::Retrieval, "chunk field missing"))
}

fn field_u32(v: &serde_json::Value, key: &str) -> Result<u32> {
    v.get(key)
        .and_then(|x| x.as_u64())
        .map(|n| n as u32)
        .ok_or_else(|| err(Code::Retrieval, "chunk field missing"))
}

#[derive(Deserialize)]
struct QdrantEnvelope {
    #[serde(default)]
    status: String,
    #[serde(default)]
    result: serde_json::Value,
}

#[derive(Deserialize)]
struct RawPoint {
    #[serde(default)]
    id: String,
    #[serde(default)]
    payload: serde_json::Value,
}

#[derive(Deserialize)]
struct QueryResult {
    #[serde(default)]
    points: Vec<RawPoint>,
}

/// Bridge the async reqwest calls into the sync trait surface. The pipeline
/// runs inside a Tokio runtime; `block_on` uses a handle to the current one.
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
}

// Bound and validate grouped replies before using the existing scope/id checks.
fn query_points(result: serde_json::Value, grouped: bool, limit: usize) -> Result<Vec<RawPoint>> {
    if !grouped {
        let result: QueryResult = serde_json::from_value(result)
            .map_err(|_| err(Code::Retrieval, "query decode failed"))?;
        return Ok(result.points);
    }
    let result: GroupResult = serde_json::from_value(result)
        .map_err(|_| err(Code::Retrieval, "group query decode failed"))?;
    if result.groups.len() > limit {
        return Err(err(Code::Retrieval, "too many candidate files"));
    }
    let mut seen = std::collections::HashSet::new();
    let mut points = Vec::new();
    for group in result.groups {
        if group.hits.len() != 1 || !seen.insert(group.id.clone())
            || group.hits[0].payload.pointer("/chunk/path").and_then(|v| v.as_str()) != Some(group.id.as_str()) {
            return Err(err(Code::Retrieval, "invalid candidate file group"));
        }
        points.extend(group.hits);
    }
    Ok(points)
}

#[derive(Deserialize)]
struct GroupResult { groups: Vec<PointGroup> }
#[derive(Deserialize)]
struct PointGroup { id: String, hits: Vec<RawPoint> }

#[cfg(test)]
mod group_tests {
    use super::*;
    use serde_json::json;
    fn group(path: &str) -> serde_json::Value {
        json!({"id":path,"hits":[{"id":"point","payload":{"chunk":{"path":path}}}]})
    }
    #[tokio::test(flavor="multi_thread", worker_threads=2)]
    async fn grouped_http_query_keeps_repository_snapshot_scope() {
        use std::io::{Read,Write};
        let listener=std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint=format!("http://{}",listener.local_addr().unwrap());
        let ns=Namespace{tenant:"tenant-hash".into(),repository:"repo-hash".into()};
        let schema=Schema::cohere(1024);
        let mut chunk=Chunk{key:"".into(),path:"src/main.ts".into(),start_line:1,end_line:1,file_sha256:"".into(),content:"export const n=1;".into()};
        chunk.key=chunk.computed_key();
        let point=json!({"id":chunk_point_id(&ns,&schema,"snapshot",&chunk.key),"payload":{"kind":"chunk","chunk":{"key":chunk.key,"path":chunk.path,"start_line":1,"end_line":1},"content":chunk.content}});
        let response=json!({"status":"ok","result":{"groups":[{"id":"src/main.ts","hits":[point]}]}}).to_string();
        let server=std::thread::spawn(move||{
            let (mut stream,_)=listener.accept().unwrap();
            stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
            let mut bytes=Vec::new();let mut buffer=[0u8;4096];
            let header_end=loop {
                let n=stream.read(&mut buffer).unwrap();assert!(n>0);bytes.extend_from_slice(&buffer[..n]);
                if let Some(i)=bytes.windows(4).position(|w|w==b"\r\n\r\n"){break i+4;}
            };
            let header=String::from_utf8(bytes[..header_end].to_vec()).unwrap();
            assert!(header.starts_with("POST /collections/inkspect_cohere_embed_v4_1024_v1/points/query/groups "));
            let length=header.lines().find_map(|line|line.to_lowercase().strip_prefix("content-length:").map(|v|v.trim().parse::<usize>().unwrap())).unwrap();
            while bytes.len()<header_end+length {let n=stream.read(&mut buffer).unwrap();assert!(n>0);bytes.extend_from_slice(&buffer[..n]);}
            let body:serde_json::Value=serde_json::from_slice(&bytes[header_end..header_end+length]).unwrap();
            assert_eq!(body["group_by"],"chunk.path");assert_eq!(body["group_size"],1);assert_eq!(body["limit"],40);
            let filters=body["filter"]["must"].as_array().unwrap();
            assert_eq!(filters.len(),7);
            for (key,value) in [("namespace.tenant",json!("tenant-hash")),("namespace.repository",json!("repo-hash")),("snapshot",json!("snapshot")),("kind",json!("chunk")),("embedding_schema.model",json!("cohere.embed-v4:0")),("embedding_schema.version",json!(1)),("embedding_schema.dimensions",json!(1024))] {
                assert!(filters.contains(&json!({"key":key,"match":{"value":value}})));
            }
            write!(stream,"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: application/json\r\n\r\n{}",response.len(),response).unwrap();
        });
        let client=Qdrant{endpoint,api_key:None,client:reqwest::Client::new()};
        let result=client.query_files(&ns,&schema,"snapshot",&[0.1,0.2],40).unwrap();
        assert_eq!(result.len(),1);assert_eq!(result[0].path,"src/main.ts");
        server.join().unwrap();
    }

    #[test]
    fn groups_are_bounded_and_match_the_payload_path() {
        assert_eq!(query_points(json!({"groups":[group("src/main.ts"),group("package-lock.json")]}),true,2).unwrap().len(),2);
        assert!(query_points(json!({"groups":[group("a"),group("a")]}),true,2).is_err());
        assert!(query_points(json!({"groups":[group("a"),group("b")]}),true,1).is_err());
        let mut bad=group("a"); bad["hits"][0]["payload"]["chunk"]["path"]=json!("b");
        assert!(query_points(json!({"groups":[bad]}),true,2).is_err());
        let mut bad=group("a"); bad["hits"]=json!([]);
        assert!(query_points(json!({"groups":[bad]}),true,2).is_err());
    }
}
