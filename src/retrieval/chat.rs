//! Candidate and result policy for broad repository inspections.
use super::Chunk;
use std::collections::HashSet;

const LOCKFILES: &[&str] = &[
    "package-lock.json", "npm-shrinkwrap.json", "yarn.lock", "pnpm-lock.yaml",
    "bun.lock", "bun.lockb", "cargo.lock", "composer.lock", "gemfile.lock",
    "poetry.lock", "uv.lock", "pipfile.lock", "go.sum", "packages.lock.json",
];

pub fn is_lockfile(path: &str) -> bool {
    LOCKFILES.contains(&path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase().as_str())
}

pub fn asks_for_lockfile(question: &str) -> bool {
    let q = question.to_lowercase();
    LOCKFILES.iter().any(|name| q.contains(name))
        || ["lockfile", "lock file", "lock-file", "kilit dosya", "dependency resolution", "resolved dependencies"].iter().any(|word| q.contains(word))
}

fn implementation(path: &str) -> bool {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    matches!(ext.as_str(), "ts"|"tsx"|"js"|"jsx"|"mjs"|"cjs"|"py"|"go"|"rs"|"rb"|"java"|"kt"|"kts"|"swift"|"c"|"h"|"cpp"|"hpp"|"cs"|"php"|"gd"|"vue"|"svelte"|"sql"|"sh")
}

/// Reserve half the inspection slots for implementation where available, then
/// fill by rerank order. One file per slot; scores remain Cohere's real scores.
/// A dependency-specific inspection instead follows its rerank order directly.
pub fn select<'a>(chunks: &'a [Chunk], ranked: &[(usize,f64)], limit: usize, bytes: usize, code_first: bool) -> Vec<(&'a Chunk,f64)> {
    if limit == 0 {return Vec::new();}
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut remaining = bytes;
    if code_first {
        for &(idx,score) in ranked {
            let c=&chunks[idx];
            if !implementation(&c.path) || c.content.len()>remaining || !seen.insert(c.path.clone()) {continue;}
            remaining-=c.content.len();out.push((c,score));
            if out.len()>=limit.div_ceil(2) {break;}
        }
    }
    for &(idx,score) in ranked {
        if out.len()>=limit {break;}
        let c=&chunks[idx];
        if c.content.len()>remaining || !seen.insert(c.path.clone()) {continue;}
        remaining-=c.content.len();out.push((c,score));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    fn chunk(path: &str) -> Chunk { Chunk {key:"".into(),path:path.into(),start_line:1,end_line:1,file_sha256:"".into(),content:"code".into()} }
    #[test]
    fn generated_dependency_files_do_not_hide_nested_lockfiles() {
        assert!(is_lockfile("apps/web/package-lock.json"));
        assert!(is_lockfile("Cargo.lock"));
        assert!(!is_lockfile("package.json"));
        assert!(asks_for_lockfile("Review package-lock.json dependency changes"));
        assert!(!asks_for_lockfile("Project implementation dependencies and tests"));
    }
    #[test]
    fn code_gets_space_without_inventing_scores_or_exceeding_budgets() {
        let chunks=vec![chunk("README.md"),chunk("package.json"),chunk("tsconfig.json"),chunk("src/a.ts"),chunk("src/b.ts"),chunk("src/a.ts")];
        let ranks=vec![(0,0.9),(1,0.8),(2,0.7),(3,0.03),(4,0.02),(5,0.01)];
        let selected=select(&chunks,&ranks,4,100,true);
        assert_eq!(selected.iter().map(|(c,_)|c.path.as_str()).collect::<Vec<_>>(),vec!["src/a.ts","src/b.ts","README.md","package.json"]);
        assert_eq!(selected[0].1,0.03);
        assert_eq!(select(&chunks,&ranks,4,8,true).len(),2);
        assert_eq!(select(&chunks,&ranks,2,100,false)[0].0.path,"README.md");
    }
}
