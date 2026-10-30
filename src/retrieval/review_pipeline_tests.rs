//! Local HTTP fixture exercises real retrieval orchestration without AWS,
//! repository access, an index write, or a published review.
use super::*;
use crate::{config::RetrievalConfig, domain::SourceRevision, git::ChangedFile,
    ignore::IgnorePolicy, retrieval::{Embedder, Pipeline, Reranker, SourceVerifier}};
use serde_json::json;
use std::{cell::RefCell, io::{Read, Write}};

struct Embed;
impl Embedder for Embed {
    fn embed_query(&self, _: &str) -> Result<Vec<f32>> { Ok(vec![0.1; 1024]) }
}
struct Rank { calls: RefCell<Vec<usize>> }
impl Reranker for Rank {
    fn rerank(&self, _: &str, docs: &[String]) -> Result<Vec<(usize,f64)>> {
        assert!(docs.len() <= 50);
        assert!(docs.iter().all(|d| !["stale.ts", "ignored.ts", "package-lock.json"].iter().any(|p| d.contains(p))));
        self.calls.borrow_mut().push(docs.len());
        Ok(docs.iter().enumerate().map(|(i,d)| (i, if d.contains("chargeCard(user)") {0.001} else {0.8})).collect())
    }
}
struct Verify { chunks: Vec<Chunk> }
impl SourceVerifier for Verify {
    fn read(&self, path: &str, _: u32, _: u32) -> Result<Option<String>> {
        assert!(!["ignored.ts", "package-lock.json"].contains(&path));
        if path == "stale.ts" { return Ok(Some("changed at pinned head".into())); }
        Ok(self.chunks.iter().find(|c| c.path == path).map(|c| c.content.clone()))
    }
}

#[tokio::test(flavor="multi_thread", worker_threads=2)]
async fn review_ranks_all_verified_candidates_in_batches_and_ignores_chat_top_n() {
    let rev = SourceRevision { tenant_id:"tenant".into(), provider:"github".into(),
        provider_instance:"github.com".into(), repository_id:"repo".into(), revision_id:"pinned-head".into() };
    let ns = Namespace::for_revision(&rev); let schema = Schema::cohere(1024);
    let chunks: Vec<_> = (0..55).map(|i| {
        let (path, content) = match i {
            50 => ("src/caller.ts".to_string(), "chargeCard(user)"),
            51 => ("stale.ts".into(), "old code"),
            52 => ("ignored.ts".into(), "excluded code"),
            53 => ("package-lock.json".into(), "generated metadata"),
            54 => ("tests/payment.test.ts".into(), "chargeCard(user)"),
            _ => (format!("src/module{i}.ts"), "implementation"),
        };
        let mut c = Chunk {key:String::new(),path,start_line:1,end_line:1,file_sha256:String::new(),content:content.into()};
        c.key=c.computed_key(); c
    }).collect();
    let points: Vec<_> = chunks.iter().map(|c| json!({"id":chunk_point_id(&ns,&schema,"snapshot",&c.key),
        "payload":{"kind":"chunk","chunk":{"key":c.key,"path":c.path,"start_line":1,"end_line":1},"content":c.content}})).collect();
    let groups: Vec<_> = points[..40].iter().map(|p| json!({"id":p["payload"]["chunk"]["path"],"hits":[p]})).collect();
    let results = [json!([{"id":"manifest", "payload":{"kind":"manifest","snapshot":"snapshot"}}]),
        json!({"groups":groups}), json!({"points":&points[15..]})];
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}",listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        for (i,result) in results.into_iter().enumerate() {
            let (mut stream,_) = listener.accept().unwrap();
            stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
            let mut bytes = Vec::new(); let mut buffer = [0;4096];
            let end = loop {
                let n = stream.read(&mut buffer).unwrap(); assert!(n>0); bytes.extend_from_slice(&buffer[..n]);
                if let Some(i)=bytes.windows(4).position(|w| w==b"\r\n\r\n") { break i+4; }
            };
            let header = String::from_utf8(bytes[..end].to_vec()).unwrap();
            let length = header.lines().find_map(|l| l.to_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap())).unwrap();
            while bytes.len()<end+length { let n=stream.read(&mut buffer).unwrap(); assert!(n>0); bytes.extend_from_slice(&buffer[..n]); }
            let body:serde_json::Value = serde_json::from_slice(&bytes[end..end+length]).unwrap();
            if i>0 {
                assert_eq!(body["limit"],40);
                assert_eq!(body["filter"]["must"].as_array().unwrap().len(),7);
                assert!(header.contains(if i==1 {"/points/query/groups "} else {"/points/query "}));
            }
            let response = json!({"status":"ok","result":result}).to_string();
            write!(stream,"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: application/json\r\n\r\n{}",response.len(),response).unwrap();
        }
    });
    let qdrant = Qdrant { endpoint, api_key:None, client:reqwest::Client::new() };
    let config = RetrievalConfig {top_n:1, ..RetrievalConfig::default()};
    let rank = Rank {calls:RefCell::new(Vec::new())};
    let verifier = Verify {chunks};
    let ignore = IgnorePolicy::parse(b"ignored.ts\n").unwrap();
    let pipeline = Pipeline {config:&config, schema, embedder:&Embed, reranker:&rank,
        qdrant:&qdrant, verifier:&verifier, ignore:Some(&ignore)};
    let changed = [ChangedFile {kind:"modified".into(),path:"src/module0.ts".into(),base_path:String::new(),
        edits:vec![],unknown:false,hunk_text:String::new(),excerpt:"function chargeCard(user) {}".into()}];
    let out = pipeline.enrich(&rev,"Review changes",&changed).unwrap();
    server.join().unwrap();
    assert_eq!(*rank.calls.borrow(),vec![50,2]);
    assert_eq!(out.len(),52);
    assert!(out[..3].iter().any(|r| r.path=="src/caller.ts"));
    assert!(out[..3].iter().any(|r| r.path=="tests/payment.test.ts"));
    assert!(serde_json::to_vec(&out).unwrap().len()<=config.content_bytes);
}
