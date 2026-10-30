//! Global review-context selection. Relationship hints supplement semantic
//! relevance; they are lexical evidence, not a language-aware call graph.
use super::{chat, Chunk, Retrieved};
use crate::error::{err, Code, Result};
use crate::git::ChangedFile;
use std::collections::{BTreeMap, HashSet};

// A conservative noise floor for semantic-only matches, not a calibrated
// probability. Direct source relationships do not need to clear this floor.
const SEMANTIC_MIN_SCORE: f64 = 0.05;

/// `base_bytes` already includes the empty context array. Replace its two
/// brackets, and retain the host's legacy zero = unconfigured byte ceiling.
pub fn available_bytes(configured: usize, request_limit: usize, base_bytes: usize) -> usize {
    if request_limit == 0 { configured }
    else { configured.min(request_limit.saturating_sub(base_bytes).saturating_add(2)) }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Role { Implementation, Reference, Test, Semantic }

pub struct Policy {
    paths: HashSet<String>,
    stems: HashSet<String>,
    symbols: HashSet<String>,
    dependency_dirs: HashSet<String>,
    explicit_dependencies: bool,
}


impl Policy {
    pub fn new(changed: &[ChangedFile], focus: &str) -> Self {
        let paths: HashSet<_> = changed.iter().flat_map(|f| [&f.path, &f.base_path])
            .filter(|p| !p.is_empty()).cloned().collect();
        let stems = paths.iter().map(|p| module_stem(p)).collect();
        let symbols = changed.iter().flat_map(|f| callable_names(&f.excerpt)).collect();
        let dependency_dirs = paths.iter().filter(|p| is_manifest(p))
            .map(|p| parent(p).to_string()).collect();
        let focus = focus.to_lowercase();
        Self { paths, stems, symbols, dependency_dirs,
            explicit_dependencies: chat::asks_for_lockfile(&focus)
                || ["dependency", "dependencies", "bağımlılık", "bagimlilik"].iter().any(|w| focus.contains(w)) }
    }

    pub fn allows(&self, chunk: &Chunk) -> bool {
        !chat::is_lockfile(&chunk.path) || self.paths.contains(&chunk.path)
            || self.dependency_dirs.contains(parent(&chunk.path)) || self.explicit_dependencies
    }

    fn role(&self, chunk: &Chunk) -> Role {
        let reference = callable_names(&chunk.content).iter().any(|s| self.symbols.contains(s))
            || self.imports_changed_module(chunk);
        let stem = module_stem(&chunk.path);
        let matching_test = is_test(&chunk.path) && !matches!(stem.as_str(), "index"|"main"|"mod"|"lib"|"utils"|"config")
            && self.stems.contains(&stem);
        if is_test(&chunk.path) && (reference || matching_test || self.paths.contains(&chunk.path)) {
            Role::Test
        } else if self.paths.contains(&chunk.path) {
            Role::Implementation
        } else if reference {
            Role::Reference
        } else { Role::Semantic }
    }

    fn imports_changed_module(&self, chunk: &Chunk) -> bool {
        chunk.content.lines().any(|line| {
            let line = line.trim();
            if line.starts_with("//") || !["import ", "from ", "require(", "include ", "include("].iter().any(|s| line.contains(s)) {
                return false;
            }
            line.split(['\'', '"']).enumerate().any(|(i, reference)| {
                if i % 2 == 0 || reference.is_empty() || reference.chars().any(char::is_whitespace) { return false; }
                let path = if reference.starts_with('.') {
                    format!("{}/{}", parent(&chunk.path), reference)
                } else { reference.to_string() };
                let Some(path) = normalized_module(&path) else { return false };
                self.paths.iter().any(|target| normalized_module(target).is_some_and(|target| target == path || target == format!("{path}/index")))
            })
        })
    }
}

fn normalized_module(path: &str) -> Option<String> {
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {},
            ".." => { parts.pop()?; },
            part => parts.push(part),
        }
    }
    let last = parts.pop()?;
    parts.push(last);
    let path = parts.join("/");
    let name = source_stem(last);
    Some(format!("{}{}", &path[..path.len() - last.len()], name))
}

fn parent(path: &str) -> &str { path.rsplit_once('/').map(|(p, _)| p).unwrap_or("") }
fn is_manifest(path: &str) -> bool {
    matches!(path.rsplit('/').next().unwrap_or(path), "package.json" | "Cargo.toml" | "go.mod"
        | "pyproject.toml" | "requirements.txt" | "Gemfile" | "composer.json" | "Pipfile")
}
fn is_test(path: &str) -> bool {
    let p = path.to_lowercase();
    p.split('/').any(|part| matches!(part, "test" | "tests" | "__tests__" | "spec" | "specs"))
        || [".test.", ".spec.", "_test.", "/test_"].iter().any(|s| p.contains(s)) || p.starts_with("test_")
}
fn module_stem(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    let name = source_stem(name);
    let name = name.strip_prefix("test_").unwrap_or(name);
    [".test", ".spec", "_test"].iter().find_map(|s| name.strip_suffix(s)).unwrap_or(name).to_string()
}
fn source_stem(name: &str) -> &str {
    // Only remove known source extensions: dotted module names remain intact.
    match name.rsplit_once('.') {
        Some((stem, "ts"|"tsx"|"js"|"jsx"|"mjs"|"py"|"go"|"rs"|"rb"|"java"|"kt"|"swift"|"c"|"h"|"cpp"|"cs"|"php"|"gd")) => stem,
        _ => name,
    }
}

fn callable_names(text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for line in text.lines() {
        let line = line.trim_start_matches(['+', '-', ' ', '\t']);
        if ["//", "#", "*"].iter().any(|p| line.starts_with(p)) { continue; }
        for (i, _) in line.match_indices('(') {
            let prefix = line[..i].trim_end();
            let name = prefix.rsplit(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$')).next().unwrap_or("");
            if name.len() < 4 || matches!(name, "function"|"return"|"switch"|"while"|"catch"|"super"|"typeof"|"require"|"import"|"print"|"println"|"assert"|"expect"|"describe"|"test"|"Some"|"None"|"Error"|"String"|"Number"|"Boolean"|"Object"|"Array"|"Promise"|"resolve"|"reject"|"push"|"filter"|"reduce"|"forEach"|"includes"|"parseInt"|"stringify"|"parse") { continue; }
            out.insert(name.to_string());
        }
    }
    out
}

pub fn validate_ranking(rows: &[(usize, f64)], count: usize) -> Result<()> {
    let mut seen = HashSet::new();
    if rows.len() != count || rows.iter().any(|(i, score)| *i >= count || !seen.insert(*i) || !(0.0..=1.0).contains(score)) {
        return Err(err(Code::Retrieval, "invalid complete review ranking"));
    }
    Ok(())
}

struct Candidate { chunk: Chunk, score: f64, role: Role }
#[derive(Default)]
pub struct Pool { candidates: BTreeMap<String, Candidate> }

impl Pool {
    pub fn len(&self) -> usize { self.candidates.len() }

    pub fn add(&mut self, chunk: Chunk, score: f64, policy: &Policy) {
        if !policy.allows(&chunk) { return; }
        let role = policy.role(&chunk);
        if role == Role::Semantic && score < SEMANTIC_MIN_SCORE { return; }
        self.candidates.entry(chunk.key.clone())
            .and_modify(|c| c.score = c.score.max(score))
            .or_insert(Candidate { chunk, score, role });
    }

    /// One span per file per round, interleaved by source relationship. Only
    /// after giving other files room do we take deeper non-overlapping spans.
    /// Query order cannot consume the budget before later queries are ranked.
    pub fn select(&self, bytes: usize, max_chunks: usize) -> Vec<Retrieved> {
        let mut files: BTreeMap<&str, Vec<&Candidate>> = BTreeMap::new();
        for c in self.candidates.values() { files.entry(&c.chunk.path).or_default().push(c); }
        for spans in files.values_mut() { spans.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.chunk.start_line.cmp(&b.chunk.start_line))); }
        let rounds = files.values().map(Vec::len).max().unwrap_or(0);
        let mut out: Vec<Retrieved> = Vec::new();
        let mut used = 2; // JSON array brackets, plus metadata/escaping below.
        for round in 0..rounds {
            let mut lanes: BTreeMap<Role, Vec<&Candidate>> = BTreeMap::new();
            for spans in files.values() {
                if let Some(c) = spans.get(round) { lanes.entry(c.role).or_default().push(c); }
            }
            for lane in lanes.values_mut() { lane.sort_by(|a,b| b.score.total_cmp(&a.score).then(a.chunk.path.cmp(&b.chunk.path))); }
            let width = lanes.values().map(Vec::len).max().unwrap_or(0);
            for i in 0..width {
                for lane in lanes.values() {
                    let Some(c) = lane.get(i) else { continue };
                    if out.len() >= max_chunks { return out; }
                    let chunk = &c.chunk;
                    if out.iter().any(|r| r.path == chunk.path && r.start_line <= chunk.end_line && chunk.start_line <= r.end_line) { continue; }
                    let r = Retrieved { path: chunk.path.clone(), start_line: chunk.start_line, end_line: chunk.end_line, content: chunk.content.clone() };
                    let cost = serde_json::to_vec(&r).unwrap().len() + usize::from(!out.is_empty());
                    if cost > bytes.saturating_sub(used) { continue; }
                    used += cost;
                    out.push(r);
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(path: &str, excerpt: &str) -> ChangedFile {
        ChangedFile { kind: "modified".into(), path: path.into(), base_path: path.into(),
            edits: vec![], unknown: false, hunk_text: excerpt.into(), excerpt: excerpt.into() }
    }
    fn chunk(path: &str, start: u32, text: &str) -> Chunk {
        let mut c = Chunk { key: String::new(), path: path.into(), start_line: start,
            end_line: start, file_sha256: String::new(), content: text.into() };
        c.key = c.computed_key(); c
    }
    fn paths(rows: &[Retrieved]) -> Vec<&str> { rows.iter().map(|r| r.path.as_str()).collect() }

    #[test]
    fn budget_can_include_more_than_eight_and_zero_relevant_results() {
        let policy = Policy::new(&[change("src/checkout.ts", "chargeCard(user)")], "");
        let mut pool = Pool::default();
        for i in 0..15 { pool.add(chunk(&format!("src/module{i}.ts"), 1, "relevant source"), 0.8, &policy); }
        assert_eq!(pool.select(100_000, 100).len(), 15);
        assert_eq!(pool.select(100_000, 3).len(), 3); // separate safety cap
        assert!(pool.select(0, 100).is_empty());
        assert!(pool.select(100_000, 0).is_empty());
        let mut empty = Pool::default();
        empty.add(chunk("docs/unrelated.md", 1, "a recipe"), 0.001, &policy);
        assert!(empty.select(100_000, 100).is_empty()); // no minResults guarantee
    }

    #[test]
    fn callers_and_tests_survive_low_semantic_scores_and_generated_file_flood() {
        let policy = Policy::new(&[change("src/payment.ts", "function chargeCard(user) {}")], "");
        let mut pool = Pool::default();
        for i in 1..20 { pool.add(chunk("package-lock.json", i, "chargeCard(user)"), 0.99, &policy); }
        for i in 1..10 { pool.add(chunk("src/payment.ts", i, "implementation"), 0.9, &policy); }
        pool.add(chunk("src/checkout.ts", 1, "chargeCard(user)"), 0.001, &policy);
        pool.add(chunk("tests/payment.test.ts", 1, "chargeCard(user)"), 0.002, &policy);
        let out = pool.select(100_000, 3);
        assert_eq!(paths(&out), ["src/payment.ts", "src/checkout.ts", "tests/payment.test.ts"]);
    }

    #[test]
    fn import_links_and_matching_test_names_are_hints_not_shared_generic_words() {
        let policy = Policy::new(&[change("src/payment.ts", "function chargeCard(user) {}")], "");
        assert_eq!(policy.role(&chunk("src/main.ts", 1, "import payment from './payment';")), Role::Reference);
        assert_eq!(policy.role(&chunk("src/checkout/main.ts", 1, "import payment from '../payment';")), Role::Reference);
        assert_eq!(policy.role(&chunk("other/main.ts", 1, "import payment from './payment';")), Role::Semantic);
        assert_eq!(policy.role(&chunk("src/main.ts", 1, "const name = 'payment';")), Role::Semantic);
        assert_eq!(policy.role(&chunk("tests/payment.spec.ts", 1, "assert(value)")), Role::Test);
        assert_eq!(policy.role(&chunk("tests/unrelated.test.ts", 1, "const user = 1; assert(value);")), Role::Semantic);
        assert_eq!(policy.role(&chunk("docs/readme.md", 1, "// chargeCard(user)")), Role::Semantic);
    }

    #[test]
    fn locks_are_allowed_only_for_changed_lock_manifest_or_dependency_focus() {
        let lock = chunk("apps/web/package-lock.json", 1, "resolved dependencies");
        assert!(!Policy::new(&[change("apps/web/index.ts", "")], "").allows(&lock));
        assert!(Policy::new(&[change("apps/web/package-lock.json", "")], "").allows(&lock));
        assert!(Policy::new(&[change("apps/web/package.json", "")], "").allows(&lock));
        assert!(!Policy::new(&[change("apps/other/package.json", "")], "").allows(&lock));
        assert!(Policy::new(&[], "Check dependency resolution").allows(&lock));
    }

    #[test]
    fn global_selection_is_query_order_independent_and_deduplicates_spans() {
        let policy = Policy::new(&[], "");
        let a = chunk("src/a.ts", 1, "A"); let b = chunk("src/b.ts", 1, "B");
        let mut first = Pool::default(); let mut reversed = Pool::default();
        for (c, s) in [(a.clone(), 0.1), (b.clone(), 0.8), (a.clone(), 0.9)] { first.add(c, s, &policy); }
        for (c, s) in [(a.clone(), 0.9), (b, 0.8), (a, 0.1)] { reversed.add(c, s, &policy); }
        assert_eq!(first.len(), 2);
        assert_eq!(paths(&first.select(1000, 1)), ["src/a.ts"]);
        assert_eq!(serde_json::to_vec(&first.select(1000, 100)).unwrap(), serde_json::to_vec(&reversed.select(1000, 100)).unwrap());
    }

    #[test]
    fn json_budget_accounts_for_unicode_escaping_and_skips_oversized_spans() {
        let policy = Policy::new(&[], "");
        let mut pool = Pool::default();
        pool.add(chunk("src/büyük.ts", 1, &"x".repeat(5000)), 0.99, &policy);
        pool.add(chunk("src/küçük.ts", 1, "\"İstanbul\"\n\\"), 0.8, &policy);
        let full = pool.select(1000, 100);
        assert_eq!(paths(&full), ["src/küçük.ts"]);
        let exact = serde_json::to_vec(&full).unwrap().len();
        assert_eq!(pool.select(exact, 100).len(), 1);
        assert!(pool.select(exact - 1, 100).is_empty());
    }

    #[test]
    fn additional_non_overlapping_spans_fit_after_file_diversity() {
        let policy = Policy::new(&[], ""); let mut pool = Pool::default();
        let mut a = chunk("src/a.ts", 1, "A"); a.end_line = 5; a.key = a.computed_key();
        for (c,s) in [(a,0.9), (chunk("src/a.ts",3,"overlap"),0.8), (chunk("src/a.ts",6,"deeper"),0.7), (chunk("src/b.ts",1,"B"),0.6)] { pool.add(c,s,&policy); }
        let out = pool.select(1000,100);
        assert_eq!(paths(&out), ["src/a.ts","src/b.ts","src/a.ts"]);
        assert_eq!(out[2].start_line,6);
    }

    #[test]
    fn budget_reserves_existing_prompt_and_complete_rankings_are_required() {
        assert_eq!(available_bytes(512_000, 10_000, 9_000), 1002);
        assert_eq!(available_bytes(512_000, 10_000, 11_000), 2);
        assert_eq!(available_bytes(100, 10_000, 9000), 100);
        assert_eq!(available_bytes(100, 0, 9000), 100);
        assert!(validate_ranking(&[(0,0.9),(1,0.1)],2).is_ok());
        for bad in [vec![(0,0.9)], vec![(0,0.9),(0,0.1)], vec![(0,0.9),(2,0.1)], vec![(0, f64::NAN),(1,0.1)]] {
            assert!(validate_ranking(&bad,2).is_err());
        }
    }
}
