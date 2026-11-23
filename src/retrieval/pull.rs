//! Review-local vector search. Qdrant stores vectors and opaque identifiers.
//! Excerpts always come from the pinned local diff or verified repository head.
use super::{
    chat,
    cohere::Bedrock,
    hash::{point_id, GoJson},
    indexer,
    qdrant::Manifest,
    Namespace, Pipeline,
};
use crate::{
    config::ReviewJob,
    error::{err, Code, Result},
    git::{ChangedFile, LineEdit},
};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

const FRAGMENT_BYTES: usize = 4800;
const CONTEXT_BYTES: usize = 32 * 1024;
const PER_QUERY: usize = 6;

fn documentation(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    path.ends_with(".md")
        || path.ends_with(".rst")
        || path.ends_with(".txt") && path.starts_with("docs/")
        || path.ends_with(".json") && path.starts_with("docs/validation/")
}

pub fn mixed_review(files: &[ChangedFile]) -> bool {
    files
        .iter()
        .any(|f| !f.unknown && chat::implementation(&f.path))
        && files.iter().any(|f| documentation(&f.path))
}

struct Match {
    path: String,
    edit: LineEdit,
    evidence: Vec<String>,
}

#[derive(Default)]
pub struct Context {
    pub documents: HashSet<String>,
    evidence: HashMap<String, Value>,
    matches: Vec<Match>,
    bytes: usize,
    limit: usize,
}

impl Context {
    pub fn source_ranges(&self) -> Vec<Value> {
        let mut ranges: Vec<_> = self.evidence.iter().collect();
        ranges.sort_by_key(|(id, _)| *id);
        ranges.into_iter().map(|(_,v)| json!({"path":v["path"],"start_line":v["start_line"],"end_line":v["end_line"],"source":v["source"]})).collect()
    }

    /// Round robin over code fragments so one long implementation cannot fill
    /// the whole batch with its first hit list.
    pub fn select(&self, files: &[ChangedFile]) -> Vec<&Value> {
        let mut ranges = HashMap::<&str, Vec<&LineEdit>>::new();
        for f in files {
            ranges.entry(&f.path).or_default().extend(&f.edits);
        }
        let matching: Vec<_> = self
            .matches
            .iter()
            .filter(|m| {
                ranges
                    .get(m.path.as_str())
                    .is_some_and(|edits| edits.iter().any(|e| overlaps(e, &m.edit)))
            })
            .collect();
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut bytes = 2;
        for rank in 0..matching.iter().map(|m| m.evidence.len()).max().unwrap_or(0) {
            for m in &matching {
                let Some(key) = m.evidence.get(rank) else {
                    continue;
                };
                if !seen.insert(key) {
                    continue;
                }
                let v = &self.evidence[key];
                let size = serde_json::to_vec(v).unwrap().len() + usize::from(!out.is_empty());
                if bytes + size <= self.bytes && out.len() < self.limit {
                    out.push(v);
                    bytes += size;
                }
            }
        }
        out
    }
}

fn overlaps(a: &LineEdit, b: &LineEdit) -> bool {
    let range = |x: u32, n: u32, y: u32, m: u32| {
        n > 0 && m > 0 && x < y.saturating_add(m) && y < x.saturating_add(n)
    };
    range(
        a.target_start,
        a.target_lines,
        b.target_start,
        b.target_lines,
    ) || range(a.base_start, a.base_lines, b.base_start, b.base_lines)
}

struct Part {
    id: String,
    file: ChangedFile,
    input: String,
    doc: bool,
}

fn collect(
    files: &[ChangedFile],
    scope: &str,
    context: &mut Context,
    job: &ReviewJob,
    limit: usize,
) -> Result<Vec<Part>> {
    let mut parts = Vec::new();
    let mut bytes = 0usize;
    for file in files {
        if file.unknown
            || file.edits.is_empty()
            || !indexer::eligible(&file.path)
            || (!file.base_path.is_empty() && !indexer::eligible(&file.base_path))
            || indexer::contains_secret(&file.hunk_text)
        {
            continue;
        }
        let doc = documentation(&file.path)
            && (file.base_path.is_empty() || documentation(&file.base_path));
        let start = parts.len();
        let mut complete = true;
        for fragment in crate::review_batches::hunk_fragments(file, FRAGMENT_BYTES) {
            let input = format!("File: {}\n{}", fragment.path, fragment.hunk_text);
            if input.len() > 6400 || fragment.edits.len() != 1 {
                complete = false;
                continue;
            }
            bytes += input.len();
            if (limit > 0 && bytes > limit) || parts.len() >= 30_000 {
                return Err(err(Code::Retrieval, "PR index exceeds input allowance"));
            }
            let id = point_id(&GoJson::Array(vec![
                GoJson::s(scope),
                GoJson::Int(parts.len() as i64),
            ]));
            parts.push(Part {
                id,
                file: fragment,
                input,
                doc,
            });
        }
        if doc
            && complete
            && parts.len() > start
            && !job.prior_findings.iter().any(|p| p.path == file.path)
        {
            context.documents.insert(file.path.clone());
        }
    }
    Ok(parts)
}

fn completed(value: Value) -> Result<()> {
    if value["status"] == "completed" {
        Ok(())
    } else {
        Err(err(Code::Retrieval, "PR index write incomplete"))
    }
}

pub trait DocumentVectors {
    async fn documents(&self, input: &[String]) -> Result<Vec<Vec<f32>>>;
}
impl DocumentVectors for Bedrock {
    async fn documents(&self, input: &[String]) -> Result<Vec<Vec<f32>>> {
        self.embed_documents_async(input).await
    }
}

pub async fn retrieve(
    p: &Pipeline<'_>,
    documents: &impl DocumentVectors,
    job: &ReviewJob,
    files: &[ChangedFile],
    manifest: Option<&Manifest>,
    focus: &str,
    limit: usize,
) -> Result<Option<Context>> {
    let rev = job.target_revision();
    let ns = Namespace::for_revision(&rev);
    let scope = super::hash::digest(&GoJson::Array(vec![
        ns.go_json(),
        GoJson::s(&job.run_id),
        GoJson::s(&job.head_sha),
        GoJson::s(rand::random::<u128>().to_string()),
    ]));
    let mut context = Context {
        bytes: p.config.content_bytes.min(CONTEXT_BYTES),
        limit: p.config.max_files,
        ..Default::default()
    };
    let parts = collect(files, &scope, &mut context, job, limit)?;
    if !parts.iter().any(|v| chat::implementation(&v.file.path)) {
        return Ok(None);
    }
    let route = format!("/collections/{}_review", p.schema.collection());
    p.qdrant.ensure_at(&p.schema, &route).await?;
    let delete = format!("{route}/points/delete?wait=true");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| err(Code::Internal, "invalid system time"))?
        .as_secs();
    // Recover orphaned vectors after a killed worker. Unexpired attempts are isolated.
    let expired = json!({"filter":{"must":[{"key":"expires","range":{"lt":now}}]}});
    completed(p.qdrant.post(&delete, &expired).await?)?;
    let result = match_parts(
        p,
        documents,
        &ns,
        &scope,
        &route,
        now,
        &parts,
        manifest,
        focus,
        &mut context,
    )
    .await;
    let cleanup = p
        .qdrant
        .post(
            &delete,
            &json!({"filter":{"must":[{"key":"scope","match":{"value":scope}}]}}),
        )
        .await
        .and_then(completed);
    cleanup?;
    result?;
    Ok(Some(context))
}

#[allow(clippy::too_many_arguments)]
async fn match_parts(
    p: &Pipeline<'_>,
    documents: &impl DocumentVectors,
    ns: &Namespace,
    scope: &str,
    route: &str,
    now: u64,
    parts: &[Part],
    manifest: Option<&Manifest>,
    focus: &str,
    context: &mut Context,
) -> Result<()> {
    let mut code_vectors = Vec::new();
    for batch in parts.chunks(16) {
        let inputs = batch
            .iter()
            .map(|part| part.input.clone())
            .collect::<Vec<_>>();
        let vectors = documents.documents(&inputs).await?;
        if vectors.len() != batch.len() {
            return Err(err(Code::Retrieval, "PR embedding count mismatch"));
        }
        let points = batch
            .iter()
            .zip(vectors)
            .map(|(v, vector)| {
                if chat::implementation(&v.file.path) {
                    code_vectors.push((v, vector.clone()));
                }
                json!({"id":v.id,"vector":vector,
                "payload":{"scope":scope,"document":v.doc,"expires":now+86400}})
            })
            .collect::<Vec<_>>();
        completed(
            p.qdrant
                .put(
                    &format!("{route}/points?wait=true"),
                    &json!({"points":points}),
                )
                .await?,
        )?;
    }
    let by_id: HashMap<_, _> = parts.iter().map(|v| (v.id.as_str(), v)).collect();
    let mut verified = HashMap::new();
    let code_count = code_vectors.len();
    let query_limit = p.config.queries.min(code_count);
    for (n, (part, document_vector)) in code_vectors.into_iter().enumerate() {
        // Spread the configured paid query/rerank calls across the change.
        // Every other hunk still searches both indexes with its existing vector.
        let rerank = (n + 1) * query_limit / code_count > n * query_limit / code_count;
        let query = super::bounded(
            &format!(
                "{focus}\nFind related documentation, contracts and callers for this change.\n{}",
                part.input
            ),
            6000,
        );
        let vector = if rerank {
            p.embedder.embed_query(&query)?
        } else {
            document_vector
        };
        let mut candidates = Vec::new();
        let mut seen = HashSet::new();
        // Reserve recall for documentation even when identical code is nearest.
        for docs_only in [true, false] {
            let mut must = vec![json!({"key":"scope","match":{"value":scope}})];
            if docs_only {
                must.push(json!({"key":"document","match":{"value":true}}));
            }
            let response=p.qdrant.post(&format!("{route}/points/query"),&json!({"query":vector,
                    "limit":p.config.candidates.min(12),"filter":{"must":must,"must_not":[{"has_id":[part.id]}]},
                    "with_payload":false,"with_vector":false})).await?;
            for hit in response["points"]
                .as_array()
                .ok_or_else(|| err(Code::Retrieval, "invalid PR search response"))?
            {
                let key = hit["id"].as_str().unwrap_or("");
                let v = by_id
                    .get(key)
                    .ok_or_else(|| err(Code::Retrieval, "unexpected PR index point"))?;
                if v.file.path == part.file.path || !seen.insert(key.to_string()) {
                    continue;
                }
                let edit = &v.file.edits[0];
                let value = json!({"source":"pull_request_diff","path":v.file.path,"start_line":edit.target_start,
                    "end_line":edit.target_start.saturating_add(edit.target_lines.saturating_sub(1)),"hunk":v.file.hunk_text});
                let score = hit["score"]
                    .as_f64()
                    .ok_or_else(|| err(Code::Retrieval, "missing PR search score"))?;
                candidates.push((key.to_string(), v.input.clone(), value, v.doc, score));
            }
        }
        if let Some(main) = manifest {
            for (c, score) in p.qdrant.query_inner(
                ns,
                &p.schema,
                &main.snapshot,
                &vector,
                p.config.candidates.min(12),
                true,
            )? {
                if c.key != c.computed_key() {
                    return Err(err(Code::Retrieval, "repository candidate key mismatch"));
                }
                if c.path == part.file.path
                    || p.ignore.is_some_and(|i| i.excluded(&c.path))
                    || !indexer::eligible(&c.path)
                    || !seen.insert(c.key.clone())
                {
                    continue;
                }
                let matches_head = match verified.entry(c.key.clone()) {
                    std::collections::hash_map::Entry::Occupied(e) => *e.get(),
                    std::collections::hash_map::Entry::Vacant(e) => *e.insert(
                        p.verifier.read(&c.path, c.start_line, c.end_line)?.as_ref()
                            == Some(&c.content),
                    ),
                };
                if !matches_head {
                    continue;
                }
                candidates.push((
                    c.key.clone(),
                    c.input(),
                    json!({"source":"repository_head","path":c.path,"start_line":c.start_line,
                        "end_line":c.end_line,"content":c.content}),
                    documentation(&c.path),
                    score,
                ));
            }
        }
        if candidates.is_empty() {
            continue;
        }
        let relevant: Vec<_> = if rerank {
            let docs = candidates.iter().map(|c| c.1.clone()).collect::<Vec<_>>();
            let ranked = p.reranker.rerank(&query, &docs)?;
            super::review::validate_ranking(&ranked, docs.len())?;
            ranked
                .into_iter()
                .filter(|(_, score)| *score >= 0.05)
                .collect()
        } else {
            let mut nearest: Vec<_> = candidates
                .iter()
                .enumerate()
                .map(|(i, c)| (i, c.4))
                .filter(|(_, cosine)| *cosine > 0.0)
                .collect();
            nearest.sort_by(|a, b| b.1.total_cmp(&a.1));
            nearest
        };
        let best_doc = relevant.iter().find(|(i, _)| candidates[*i].3);
        let mut evidence = Vec::new();
        for &(idx, _) in best_doc.into_iter().chain(&relevant) {
            let (key, _, value, _, _) = &candidates[idx];
            if evidence.contains(key) {
                continue;
            }
            context
                .evidence
                .entry(key.clone())
                .or_insert_with(|| value.clone());
            evidence.push(key.clone());
            if evidence.len() == PER_QUERY {
                break;
            }
        }
        context.matches.push(Match {
            path: part.file.path.clone(),
            edit: part.file.edits[0].clone(),
            evidence,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, text: &str) -> ChangedFile {
        ChangedFile {
            path: path.into(),
            base_path: String::new(),
            kind: "added".into(),
            unknown: false,
            excerpt: String::new(),
            hunk_text: format!("@@ -0,0 +1 @@\n+{text}\n"),
            edits: vec![LineEdit {
                base_start: 0,
                base_lines: 0,
                target_start: 1,
                target_lines: 1,
            }],
        }
    }
    #[test]
    fn code_under_docs_is_primary_and_unindexed_or_prior_documents_stay_in_scope() {
        let mut job: ReviewJob =
            serde_json::from_str(include_str!("../../examples/job.example.json")).unwrap();
        job.prior_findings.push(crate::config::PriorFinding {
            path: "docs/prior.md".into(),
            title: "prior".into(),
            thread: "1".into(),
            reason: String::new(),
            suggestion: String::new(),
        });
        let files = [
            file("docs/run.py", "print(1)"),
            file("docs/normal.md", "read this"),
            file("docs/prior.md", "prior"),
            file("docs/long.md", &"x".repeat(9000)),
            file("config.json", "{}"),
            file("docs/validation/output.json", "{}"),
        ];
        let mut context = Context::default();
        let parts = collect(&files, "a", &mut context, &job, 0).unwrap();
        assert_eq!(
            context.documents,
            HashSet::from([
                "docs/normal.md".into(),
                "docs/validation/output.json".into()
            ])
        );
        assert!(parts.iter().any(|p| p.file.path == "docs/run.py" && !p.doc));
        assert!(collect(&files, "b", &mut context, &job, 10).is_err());
    }
}
