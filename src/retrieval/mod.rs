//! RAG retrieval for review.
//!
//! Step 3 of the engine: given the PR changes, find other code related to the
//! change and rank it, then hand the top snippets to the single model call.
//!
//! Compatibility: this reads the SAME Qdrant index the Go engine writes. The
//! namespace, collection name, point payloads, and the chunk-key/point-id
//! hashes are all derived with the Go-compatible encoder in `hash.rs`. We only
//! READ the shared repository namespace; we never write it.
//!
//! Security: every candidate returned by vector search is re-verified against
//! the pinned PR head (byte-for-byte) before it is allowed into the reranker
//! or the model prompt. A stale or tampered index can never inject content
//! that does not exist at the reviewed commit.

pub mod cohere;
pub mod chat;
pub mod hash;
pub mod indexer;
pub mod qdrant;
pub mod review;

use crate::config::RetrievalConfig;
use crate::domain::SourceRevision;
use crate::error::{err, Code, Result};
use crate::git::ChangedFile;
use hash::{digest, point_id, GoJson};

/// Embedding schema pinned into the Qdrant collection. Must match Go's
/// `retrieval.Schema` field order and values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    pub version: i64,
    pub model: String,
    pub dimensions: i64,
    pub normalize: bool,
}

impl Schema {
    pub fn cohere(dimensions: u32) -> Self {
        Self {
            version: 1,
            model: "cohere.embed-v4:0".into(),
            dimensions: dimensions as i64,
            normalize: true,
        }
    }

    /// Qdrant collection name, matching Go `Schema.Collection()`.
    pub fn collection(&self) -> String {
        format!(
            "inkspect_cohere_embed_v4_{}_v{}",
            self.dimensions, self.version
        )
    }

    pub fn go_json(&self) -> GoJson {
        GoJson::Object(vec![
            ("version", GoJson::Int(self.version)),
            ("model", GoJson::s(self.model.clone())),
            ("dimensions", GoJson::Int(self.dimensions)),
            ("normalize", GoJson::Bool(self.normalize)),
        ])
    }
}

/// Tenant/repository namespace. Matches Go `retrieval.Namespace`.
#[derive(Debug, Clone)]
pub struct Namespace {
    pub tenant: String,
    pub repository: String,
}

impl Namespace {
    /// Derive the shared repository namespace for a revision, exactly as the Go
    /// engine does: tenant = digest(tenant_id string), repository =
    /// digest(revision-with-empty-revision_id).
    pub fn for_revision(rev: &SourceRevision) -> Namespace {
        let tenant = digest(&GoJson::s(rev.tenant_id.clone()));
        // identity = rev with revision_id cleared, in Go field order.
        let identity = GoJson::Object(vec![
            ("tenant_id", GoJson::s(rev.tenant_id.clone())),
            ("provider", GoJson::s(rev.provider.clone())),
            ("provider_instance", GoJson::s(rev.provider_instance.clone())),
            ("repository_id", GoJson::s(rev.repository_id.clone())),
            ("revision_id", GoJson::s(String::new())),
        ]);
        Namespace {
            tenant,
            repository: digest(&identity),
        }
    }

    pub fn go_json(&self) -> GoJson {
        GoJson::Object(vec![
            ("tenant", GoJson::s(self.tenant.clone())),
            ("repository", GoJson::s(self.repository.clone())),
        ])
    }
}

/// A chunk of repository source stored in Qdrant. Matches Go `retrieval.Chunk`
/// (Content is carried separately in the payload `content` field).
#[derive(Debug, Clone)]
pub struct Chunk {
    pub key: String,
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    /// Whole-file SHA-256 recorded by the indexer. Carried for parity with the
    /// Go payload; verification re-reads the pinned head directly.
    #[allow(dead_code)]
    pub file_sha256: String,
    pub content: String,
}

impl Chunk {
    /// The stable chunk input string, matching Go `Chunk.Input()`:
    /// "File: {path}\nLines: {a}-{b}\n\n{content}".
    pub fn input(&self) -> String {
        format!(
            "File: {}\nLines: {}-{}\n\n{}",
            self.path, self.start_line, self.end_line, self.content
        )
    }

    /// The content key is digest(Input()), hashed as a JSON string.
    pub fn computed_key(&self) -> String {
        hash::digest_string(&self.input())
    }
}

/// A retrieved, verified snippet ready for the model prompt.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Retrieved {
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub content: String,
}

/// Trait abstraction so retrieval can be tested without live AWS/Qdrant.
pub trait Embedder {
    /// Embed a single query string (input_type = "search_query"), returning a
    /// unit-normalized vector of `dimensions` floats.
    fn embed_query(&self, text: &str) -> Result<Vec<f32>>;
}

pub trait Reranker {
    /// Return every supplied document with its relevance score, in descending order.
    /// Provider batches are bounded separately from the final context budget.
    fn rerank(&self, query: &str, documents: &[String]) -> Result<Vec<(usize, f64)>>;
}

/// Verifies that a candidate chunk's bytes exist unchanged at the pinned head.
pub trait SourceVerifier {
    /// Read `path` lines [start,end] at the PR head and return the content, or
    /// None if the file/range cannot be read (treated as an omission).
    fn read(&self, path: &str, start: u32, end: u32) -> Result<Option<String>>;
}

/// The retrieval pipeline. Generic over providers for testability.
pub struct Pipeline<'a> {
    pub config: &'a RetrievalConfig,
    pub schema: Schema,
    pub embedder: &'a dyn Embedder,
    pub reranker: &'a dyn Reranker,
    pub qdrant: &'a qdrant::Qdrant,
    pub verifier: &'a dyn SourceVerifier,
    /// The review's `.inkspectignore`; excluded paths never become context.
    pub ignore: Option<&'a crate::ignore::IgnorePolicy>,
}

impl<'a> Pipeline<'a> {
    /// Build one query per changed file from its changed excerpt plus review
    /// focus, matching the Go engine's per-changed-file query strategy.
    fn build_queries(&self, prefix: &str, changed: &[ChangedFile]) -> Vec<String> {
        let mut queries = Vec::new();
        for file in changed {
            if file.excerpt.trim().is_empty() {
                continue;
            }
            let q = format!(
                "{prefix}\nFind related implementation, callers, tests and contracts for {}\n{}",
                file.path, file.excerpt
            );
            queries.push(bounded(&q, 6000));
            if queries.len() >= self.config.queries {
                break;
            }
        }
        if queries.is_empty() {
            queries.push(bounded(
                &format!("{prefix}\nFind implementation and tests relevant to this review."),
                6000,
            ));
        }
        queries
    }

    /// Run retrieval and return verified snippets packed within the byte budget.
    pub fn enrich(
        &self,
        rev: &SourceRevision,
        focus: &str,
        changed: &[ChangedFile],
    ) -> Result<Vec<Retrieved>> {
        let namespace = Namespace::for_revision(rev);

        // Read the manifest. If no shared index exists we simply skip RAG; the
        // single model call still runs on the diff and changed-file context.
        let manifest = match self.qdrant.manifest(&namespace, &self.schema)? {
            Some(m) => m,
            None => {
                tracing::info!("no repository index; proceeding without RAG context");
                return Ok(Vec::new());
            }
        };

        let prefix = bounded(focus, 1800);
        let queries = self.build_queries(&prefix, changed);

        let policy = review::Policy::new(changed, focus);
        let mut pool = review::Pool::default();

        for query in queries {
            let vector = self.embedder.embed_query(&query)?;
            if vector.len() != self.schema.dimensions as usize {
                return Err(err(Code::Retrieval, "embedding dimension mismatch"));
            }

            // File-grouped recall prevents a long generated file from consuming
            // every candidate. Ordinary recall also retains useful deeper spans.
            let mut points = self.qdrant.query_files(
                &namespace,
                &self.schema,
                &manifest.snapshot,
                &vector,
                self.config.candidates,
            )?;
            points.extend(self.qdrant.query(
                &namespace, &self.schema, &manifest.snapshot, &vector, self.config.candidates,
            )?);
            let mut seen = std::collections::HashSet::new();

            // Verify every candidate against the pinned head before ranking.
            let mut candidates: Vec<Chunk> = Vec::new();
            let mut documents: Vec<String> = Vec::new();
            for chunk in points {
                if chunk.key != chunk.computed_key() {
                    return Err(err(Code::Retrieval, "candidate key mismatch"));
                }
                if !seen.insert(chunk.key.clone()) || !policy.allows(&chunk) {
                    continue;
                }
                // The shared index may predate the policy, so filter on read
                // too (before the verifier fetches the blob).
                if self.ignore.is_some_and(|policy| policy.excluded(&chunk.path)) {
                    continue;
                }
                match self.verifier.read(&chunk.path, chunk.start_line, chunk.end_line)? {
                    Some(body) if body == chunk.content => {
                        documents.push(chunk.input());
                        candidates.push(chunk);
                    }
                    // Omission: the file/range changed or is unreadable at head.
                    _ => continue,
                }
            }
            if documents.is_empty() {
                continue;
            }

            // Rerank all verified candidates, never the first configured top_n.
            // Cohere accepts at most 50 documents in each provider request.
            for (batch, docs) in documents.chunks(50).enumerate() {
                let ranked = self.reranker.rerank(&query, docs)?;
                review::validate_ranking(&ranked, docs.len())?;
                for (idx, score) in ranked {
                    pool.add(candidates[batch * 50 + idx].clone(), score, &policy);
                }
            }
        }

        let out = pool.select(self.config.content_bytes, self.config.max_files);
        tracing::info!(candidates = pool.len(), selected_chunks = out.len(),
            selected_files = out.iter().map(|c| &c.path).collect::<std::collections::HashSet<_>>().len(),
            context_bytes = serde_json::to_vec(&out).unwrap().len(),
            "review context selected across all queries");

        Ok(out)
    }
}

/// Point id for a chunk, matching Go `pointID(["chunk", ns, schema, snapshot, key])`.
pub fn chunk_point_id(ns: &Namespace, schema: &Schema, snapshot: &str, key: &str) -> String {
    point_id(&GoJson::Array(vec![
        GoJson::s("chunk"),
        ns.go_json(),
        schema.go_json(),
        GoJson::s(snapshot.to_string()),
        GoJson::s(key.to_string()),
    ]))
}

/// Byte-bounded truncation on a UTF-8 boundary (matches Go `boundedText`).
pub fn bounded(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}
