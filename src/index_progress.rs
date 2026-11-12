//! Small atomic snapshots for the API worker; never includes source or paths.
use serde::Serialize;
use std::{path::PathBuf, time::{Duration, Instant, SystemTime, UNIX_EPOCH}};

#[derive(Clone, Default, Serialize)]
pub struct Scan {
    pub measured: bool,
    pub stage: String,
    pub total: usize,
    pub processed: usize,
    pub read: usize,
    pub skipped: usize,
    pub chunks: usize,
}

#[derive(Serialize)]
struct Log { key: String, at: u64, message: String }

#[derive(Default, Serialize)]
pub struct IndexProgress {
    pub stage: String,
    pub scan: Scan,
    pub chunks: usize,
    pub embedded: usize,
    pub indexed: bool,
    logs: Vec<Log>,
    #[serde(skip)]
    path: Option<PathBuf>,
    #[serde(skip)]
    last_write: Option<Instant>,
}

impl IndexProgress {
    pub fn new() -> Self {
        Self { path: std::env::var_os("INKSPECT_INDEX_PROGRESS_FILE").map(PathBuf::from), ..Self::default() }
    }

    pub fn stage(&mut self, stage: &str, message: String) {
        self.stage = stage.into();
        self.log(stage, message);
        self.publish(true);
    }

    pub fn reading(&mut self, force: bool) {
        self.scan.chunks = self.chunks;
        self.log("reading", format!("Reading files: {}/{} processed, {} read, {} skipped.",
            self.scan.processed, self.scan.total, self.scan.read, self.scan.skipped));
        self.publish(force);
    }

    pub fn embedding(&mut self) {
        self.log("embedding", format!("Indexing chunks: {}/{} saved.", self.embedded, self.chunks));
        self.publish(true);
    }

    fn log(&mut self, key: &str, message: String) {
        let log = Log { key: format!("progress:{key}"),
            at: SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64, message };
        if let Some(existing) = self.logs.iter_mut().find(|l| l.key == log.key) { *existing = log; }
        else { self.logs.push(log); }
    }

    fn publish(&mut self, force: bool) {
        let Some(path) = &self.path else { return; };
        if !force && self.last_write.is_some_and(|t| t.elapsed() < Duration::from_millis(500)) { return; }
        let temporary = path.with_extension("tmp");
        let result = serde_json::to_vec(self).ok().and_then(|bytes| {
            std::fs::write(&temporary, bytes).ok()?;
            std::fs::rename(&temporary, path).ok()
        });
        if result.is_none() { tracing::warn!("index progress snapshot could not be saved"); }
        self.last_write = Some(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_snapshots_keep_milestones_and_replace_counters() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("progress.json");
        let mut p = IndexProgress { path: Some(path.clone()), ..IndexProgress::default() };
        p.stage("cloning", "Downloading repository.".into());
        p.scan = Scan { measured: true, total: 2, stage: "reading".into(), ..Scan::default() };
        p.stage("reading", "Reading repository files.".into());
        p.scan.processed = 1; p.scan.read = 1; p.chunks = 3; p.reading(true);
        p.scan.processed = 2; p.scan.skipped = 1; p.reading(true);
        let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(saved["scan"]["total"], 2);
        assert_eq!(saved["scan"]["read"], 1);
        assert_eq!(saved["scan"]["processed"], 2);
        assert_eq!(saved["scan"]["chunks"], 3);
        assert_eq!(saved["logs"].as_array().unwrap().len(), 2);
        assert_eq!(saved["indexed"], false);
    }
}
