//! CLI operations dispatched by the authenticated API worker. No Go runtime.
use crate::{config::{HostConfig, ReviewJob}, error::{err, Code, Result}, retrieval::{self, Embedder}, sandbox, git};
use serde_json::{json, Value};

pub async fn operation(host: &HostConfig, job: &ReviewJob, mode: &str, request: Value) -> Result<Value> {
    job.validate()?;
    if mode == "fix" { return crate::fix::run(host, job, request).await; }
    let token = std::env::var("INKSPECT_GITHUB_READ_TOKEN").map_err(|_| err(Code::GitHub, "missing scoped token"))?;
    let clone = sandbox::clone_pr(&host.sandbox, job, &token).await?;
    if mode == "describe" {
        return Ok(json!({"description_sources": description_sources(clone.path(), &job.head_sha, &token).await?}));
    }
    if mode == "source" {
        let path = request["path"].as_str().ok_or_else(|| err(Code::QueueInvalid, "missing path"))?;
        let start = request["start_line"].as_u64().unwrap_or(0);
        let end = request["end_line"].as_u64().unwrap_or(0);
        let location = crate::domain::Location { path: path.into(), start_line: start as u32, end_line: end as u32 };
        if start > u32::MAX as u64 || end > u32::MAX as u64 || !location.valid() || end - start > 400 {
            return Err(err(Code::QueueInvalid, "invalid source range"));
        }
        let content = git::read_range(clone.path(), &job.head_sha, path, start as u32, end as u32, &token).await?
            .ok_or_else(|| err(Code::Source, "source unavailable"))?;
        return Ok(json!({"revision": job.head_sha,"path":path,"start_line":start,"end_line":end,"content":content}));
    }
    let rc = &host.retrieval;
    let bedrock = retrieval::cohere::Bedrock::new(&rc.region, &rc.embed_model, &rc.rerank_region, &rc.rerank_model, rc.dimensions).await?;
    let qdrant = retrieval::qdrant::Qdrant::from_env()?;
    let schema = retrieval::Schema::cohere(rc.dimensions);
    let rev = job.target_revision();
    if mode == "index" {
        // The policy is read from the indexed commit (the default branch tip),
        // so excluded files never reach the index. An invalid one fails closed.
        let policy = crate::ignore::load(clone.path(), &job.head_sha, &token).await?;
        let chunks = retrieval::indexer::index_repository(clone.path(), &job.head_sha, &rev, &schema, &bedrock, &qdrant, &token, policy.as_ref()).await?;
        return Ok(json!({"revision":job.head_sha,"chunks":chunks,"embedded":chunks,"indexed":true,
            "description_sources":description_sources(clone.path(), &job.head_sha, &token).await?}));
    }
    let question = request["question"].as_str().filter(|q| !q.trim().is_empty() && q.len() <= 6000)
        .ok_or_else(|| err(Code::QueueInvalid, "invalid question"))?;
    let ns = retrieval::Namespace::for_revision(&rev);
    let manifest = qdrant.manifest(&ns, &schema)?.ok_or_else(|| err(Code::Retrieval, "repository index unavailable"))?;
    let vector = bedrock.embed_query(question)?;
    let inspection = match request.get("retrieval").and_then(Value::as_str) {
        None | Some("lookup") => false,
        Some("inspection") => true,
        _ => return Err(err(Code::QueueInvalid, "invalid retrieval mode")),
    };
    let dependency_question = retrieval::chat::asks_for_lockfile(question);
    let candidates = if inspection {
        qdrant.query_files(&ns, &schema, &manifest.snapshot, &vector, rc.candidates.clamp(1,50))?
    } else {
        qdrant.query(&ns, &schema, &manifest.snapshot, &vector, rc.candidates.clamp(1,50))?
    };
    let candidate_count = candidates.len();
    let candidate_files = candidates.iter().map(|c| c.path.clone()).collect::<std::collections::HashSet<_>>().len();
    let mut lockfiles_filtered = 0;
    let mut verified = Vec::new();
    let mut omissions = Vec::new();
    for chunk in candidates {
        if chunk.key != chunk.computed_key() { return Err(err(Code::Retrieval, "candidate integrity failed")); }
        if inspection && !dependency_question && retrieval::chat::is_lockfile(&chunk.path) {
            lockfiles_filtered += 1;
            continue;
        }
        match git::read_range(clone.path(), &job.head_sha, &chunk.path, chunk.start_line, chunk.end_line, &token).await? {
            Some(content) if content == chunk.content => verified.push(chunk),
            _ => omissions.push("stale candidate"),
        }
    }
    let mut snippets = Vec::new();
    if !verified.is_empty() {
        let docs: Vec<String> = verified.iter().map(|c| c.input()).collect();
        // Keep the complete ranked list for inspection selection; truncating to
        // top_n first can discard implementation before file diversity is applied.
        let top = if inspection { docs.len() } else { rc.top_n.min(docs.len()) };
        let ranked = bedrock.rerank_scored_async(question, &docs, top).await?;
        if inspection {
            for (c,score) in retrieval::chat::select(&verified,&ranked,rc.top_n,rc.content_bytes.min(192000),!dependency_question) {
                snippets.push(json!({"path":c.path,"start_line":c.start_line,"end_line":c.end_line,"content":c.content,"score":score}));
            }
        } else {
            let mut remaining = rc.content_bytes.min(192000);
            for (idx, score) in ranked {
                let c = &verified[idx];
                if c.content.len() > remaining { continue; }
                remaining -= c.content.len();
                snippets.push(json!({"path":c.path,"start_line":c.start_line,"end_line":c.end_line,"content":c.content,"score":score}));
            }
        }
    }
    Ok(json!({"revision":rev,"snapshot":manifest.snapshot,"snippets":snippets,"omissions":omissions,
        "retrieval_stats":{"strategy":if inspection {"file_diverse_inspection"} else {"semantic_lookup"},
        "candidate_chunks":candidate_count,"candidate_files":candidate_files,"lockfiles_filtered":lockfiles_filtered,"reranked_chunks":verified.len()}}))
}

async fn description_sources(path: &std::path::Path, head: &str, token: &str) -> Result<Vec<Value>> {
    let mut sources = Vec::new();
    let mut bytes = 0usize;
    for file in ["README.md", "README", "package.json", "Cargo.toml", "go.mod", "pyproject.toml", "requirements.txt", "composer.json", "Gemfile", "project.godot"] {
        if let Some(content) = retrieval::indexer::read_blob(path, head, file, token).await? {
            let content = content.lines().take(120).collect::<Vec<_>>().join("\n");
            if bytes + content.len() > 64000 { continue; }
            bytes += content.len();
            sources.push(json!({"path":file,"content":content}));
        }
    }
    Ok(sources)
}
