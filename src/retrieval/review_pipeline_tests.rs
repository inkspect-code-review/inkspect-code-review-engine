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
    let results = [json!([{"id":"manifest", "payload":{"kind":"manifest","snapshot":"snapshot","revision":rev}}]),
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
    let out = pipeline.enrich_current(&rev,"Review changes",&changed).unwrap();
    server.join().unwrap();
    assert_eq!(*rank.calls.borrow(),vec![50,2]);
    assert_eq!(out.len(),52);
    assert!(out[..3].iter().any(|r| r.path=="src/caller.ts"));
    assert!(out[..3].iter().any(|r| r.path=="tests/payment.test.ts"));
    assert!(serde_json::to_vec(&out).unwrap().len()<=config.content_bytes);
}

#[tokio::test(flavor="multi_thread", worker_threads=2)]
async fn strict_retrieval_rejects_missing_and_stale_manifests_before_embedding() {
    for result in [json!([]),json!([{"payload":{"kind":"manifest","snapshot":"old",
        "revision":{"tenant_id":"t","provider":"github","provider_instance":"github.com","repository_id":"1","revision_id":"old"}}}])] {
        let listener=std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint=format!("http://{}",listener.local_addr().unwrap());
        let server=std::thread::spawn(move || {
            let (mut stream,_)=listener.accept().unwrap();
            stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
            let mut bytes=Vec::new();let mut buffer=[0;4096];
            let end=loop {let n=stream.read(&mut buffer).unwrap();assert!(n>0);bytes.extend_from_slice(&buffer[..n]);
                if let Some(i)=bytes.windows(4).position(|w|w==b"\r\n\r\n"){break i+4;}};
            let header=String::from_utf8(bytes[..end].to_vec()).unwrap();
            let length=header.lines().find_map(|l|l.to_lowercase().strip_prefix("content-length:").map(|s|s.trim().parse::<usize>().unwrap())).unwrap();
            while bytes.len()<end+length{let n=stream.read(&mut buffer).unwrap();assert!(n>0);bytes.extend_from_slice(&buffer[..n]);}
            let response=json!({"status":"ok","result":result}).to_string();
            write!(stream,"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: application/json\r\n\r\n{}",response.len(),response).unwrap();
        });
        let qdrant=Qdrant{endpoint,api_key:None,client:reqwest::Client::new()};
        let rank=Rank{calls:RefCell::new(Vec::new())};let verifier=Verify{chunks:vec![]};let config=RetrievalConfig::default();
        let pipeline=Pipeline{config:&config,schema:Schema::cohere(1024),embedder:&Embed,reranker:&rank,qdrant:&qdrant,verifier:&verifier,ignore:None};
        let rev=SourceRevision{tenant_id:"t".into(),provider:"github".into(),provider_instance:"github.com".into(),repository_id:"1".into(),revision_id:"current".into()};
        assert!(pipeline.enrich_current(&rev,"Review changes",&[]).is_err());
        assert!(rank.calls.borrow().is_empty());server.join().unwrap();
    }
}

#[tokio::test(flavor="multi_thread", worker_threads=2)]
#[ignore = "requires a disposable local Qdrant server via INKSPECT_TEST_QDRANT_URL"]
async fn temporary_pr_vectors_match_both_sources_and_clean_up() {
    use crate::git::LineEdit;
    use crate::retrieval::pull::{retrieve, DocumentVectors};
    struct Vectors;
    impl DocumentVectors for Vectors {
        async fn documents(&self, texts:&[String]) -> Result<Vec<Vec<f32>>> {
            Ok(vec![vec![1.,0.,0.];texts.len()])
        }
    }
    impl Embedder for Vectors {
        fn embed_query(&self,_:&str)->Result<Vec<f32>> {Ok(vec![1.,0.,0.])}
    }
    struct Ranking(bool);
    impl Reranker for Ranking {
        fn rerank(&self,_:&str,docs:&[String])->Result<Vec<(usize,f64)>> {
            if self.0 {return Err(err(Code::Retrieval,"test failure after upsert"));}
            Ok(docs.iter().enumerate().map(|(i,d)|(i,if d.contains("duration"){0.95}else{0.0})).collect())
        }
    }
    let endpoint = std::env::var("INKSPECT_TEST_QDRANT_URL").unwrap();
    assert!(endpoint.starts_with("http://127.0.0.1:"));
    let db=Qdrant{endpoint,api_key:None,client:reqwest::Client::new()};
    let schema=Schema::cohere(3);
    let job:crate::config::ReviewJob=serde_json::from_str(include_str!("../../examples/job.example.json")).unwrap();
    let ns=Namespace::for_revision(&job.target_revision());
    let mut main=Chunk{path:"docs/runtime.md".into(),content:"duration contract".into(),start_line:1,end_line:1,key:String::new(),file_sha256:String::new()};
    main.key=main.computed_key();
    let snapshot="test-main";
    db.ensure(&schema).await.unwrap();
    db.upsert(&schema,vec![json!({"id":chunk_point_id(&ns,&schema,snapshot,&main.key),"vector":[1,0,0],"payload":{
        "kind":"chunk","namespace":{"tenant":ns.tenant,"repository":ns.repository},"embedding_schema":{"version":1,"model":schema.model,"dimensions":3},
        "snapshot":snapshot,"chunk":{"key":main.key,"path":main.path,"start_line":1,"end_line":1},"content":main.content}})]).await.unwrap();
    let change=|path:&str,code:&str| ChangedFile{path:path.into(),base_path:String::new(),kind:"added".into(),unknown:false,
        edits:vec![LineEdit{base_start:0,base_lines:0,target_start:1,target_lines:1}],excerpt:code.into(),hunk_text:format!("@@ -0,0 +1 @@\n+{code}\n")};
    let files=[change("src/audio.rs","fn duration() {}"),change("docs/audio.md","duration specification"),change("docs/unrelated.md","gardening")];
    let mut cfg=RetrievalConfig::default();
    let verifier=Verify{chunks:vec![main]};
    let manifest=Manifest{snapshot:snapshot.into(),revision:None};
    let route=format!("/collections/{}_review",schema.collection());
    db.ensure_at(&schema,&route).await.unwrap();
    let foreign = "00000000-0000-4000-8000-000000000011";
    let orphan = "00000000-0000-4000-8000-000000000012";
    db.put(&format!("{route}/points?wait=true"), &json!({"points":[
        {"id":foreign,"vector":[1,0,0],"payload":{"scope":"other-active-review","document":true,"expires":4102444800_i64}},
        {"id":orphan,"vector":[1,0,0],"payload":{"scope":"aborted-review","document":true,"expires":1}}]})).await.unwrap();
    for (failure,queries) in [(false,1),(true,1),(false,0)] {
        cfg.queries=queries;
        let rank=Ranking(failure);
        let p=Pipeline{config:&cfg,schema:schema.clone(),embedder:&Vectors,reranker:&rank,qdrant:&db,verifier:&verifier,ignore:None};
        let result=retrieve(&p,&Vectors,&job,&files,Some(&manifest),"test",0).await;
        if failure {assert!(result.is_err());} else {
            let context=result.unwrap().unwrap();
            assert_eq!(context.documents.len(),2);
            let selected=context.select(&files[..1]);
            assert!(selected.len()>=2);
            assert!(selected.iter().any(|v|v["path"]=="docs/audio.md" && v["source"]=="pull_request_diff"));
            assert!(selected.iter().any(|v|v["path"]=="docs/runtime.md" && v["source"]=="repository_head"));
            assert!(context.select(&files[1..]).is_empty());
        }
        let count=db.post(&format!("{route}/points/count"),&json!({"exact":true})).await.unwrap();
        assert_eq!(count["count"],1);
        let remaining=db.post(&format!("{route}/points"),&json!({"ids":[foreign],"with_payload":true})).await.unwrap();
        assert_eq!(remaining[0]["payload"]["scope"],"other-active-review");
        assert_eq!(db.query(&ns,&schema,snapshot,&[1.,0.,0.],4).unwrap().len(),1);
    }
}
