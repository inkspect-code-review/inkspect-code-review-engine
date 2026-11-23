//! Rolling admission budget for Bedrock document embeddings.
use crate::error::{err, Code, Result};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const WINDOW_MS: u64 = 61_000;
const BUDGET: usize = 370_000;

#[derive(Default, Serialize, Deserialize)]
struct Window {
    // Coalesce reservations from the same millisecond.
    charges: VecDeque<(u64, usize)>,
    blocked_until: u64,
}

impl Window {
    fn reserve(&mut self, now: u64, units: usize) -> u64 {
        self.charges
            .retain(|&(at, _)| at.saturating_add(WINDOW_MS) > now);
        if self.blocked_until > now {
            return self.blocked_until - now;
        }
        let mut total = units + self.charges.iter().map(|&(_, n)| n).sum::<usize>();
        if total > BUDGET {
            for &(at, n) in &self.charges {
                total -= n;
                if total <= BUDGET {
                    return at.saturating_add(WINDOW_MS).saturating_sub(now);
                }
            }
        }
        if let Some((at, n)) = self.charges.back_mut() {
            if *at == now {
                *n += units;
                return 0;
            }
        }
        self.charges.push_back((now, units));
        0
    }
}

pub(super) struct DocumentRate {
    directory: Option<PathBuf>,
    local: Mutex<Window>,
}

impl DocumentRate {
    pub fn new(region: &str, model: &str) -> Self {
        use sha2::{Digest, Sha256};
        let directory = std::env::var_os("INKSPECT_EMBEDDING_RATE_DIR").map(|p| {
            PathBuf::from(p).join(hex::encode(Sha256::digest(format!("{region}\0{model}"))))
        });
        Self {
            directory,
            local: Mutex::default(),
        }
    }

    fn apply(&self, change: impl FnOnce(&mut Window, u64) -> u64) -> Result<u64> {
        let now = || {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|v| v.as_millis() as u64)
                .map_err(io::Error::other)
        };
        let result = match &self.directory {
            Some(dir) => shared_window(dir, |w| Ok(change(w, now()?))),
            None => self
                .local
                .lock()
                .map_err(|_| io::Error::other("lock"))
                .and_then(|mut w| Ok(change(&mut w, now()?))),
        };
        result.map_err(|_| err(Code::Retrieval, "embedding rate state unavailable"))
    }

    pub async fn acquire(&self, bytes: usize) -> Result<()> {
        // Batch validation caps bytes at 120,000. The estimate counts UTF-8 bytes.
        let units = (bytes * 5).div_ceil(8);
        loop {
            let wait = self.apply(|w, now| w.reserve(now, units))?;
            if wait == 0 {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(wait)).await;
        }
    }

    pub fn throttled(&self) -> Result<()> {
        self.apply(|w, now| {
            w.blocked_until = w.blocked_until.max(now + WINDOW_MS);
            0
        })
        .map(|_| ())
    }
}

// The worker gives sibling job processes the same private directory. Hold the
// OS lock only for admission. Atomic replacement survives process termination.
fn shared_window(
    dir: &Path,
    change: impl FnOnce(&mut Window) -> io::Result<u64>,
) -> io::Result<u64> {
    fs::create_dir_all(dir)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("lock"))?;
    lock.lock()?;
    let path = dir.join("window.json");
    let mut window = match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Window::default(),
        Err(e) => return Err(e),
    };
    let wait = change(&mut window)?;
    if wait == 0 {
        let mut temp = tempfile::NamedTempFile::new_in(dir)?;
        serde_json::to_writer(&mut temp, &window)?;
        temp.flush()?;
        temp.persist(path)?;
    }
    Ok(wait)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_until_enough_reservations_expire() {
        let mut w = Window::default();
        assert_eq!(w.reserve(100, 100_000), 0);
        assert_eq!(w.reserve(200, 200_000), 0);
        assert_eq!(w.reserve(300, 170_001), WINDOW_MS - 100);
        assert_eq!(w.reserve(200 + WINDOW_MS, BUDGET), 0);
        assert_eq!(w.reserve(200 + WINDOW_MS, 1), WINDOW_MS);
    }

    #[test]
    fn concurrent_clients_share_budget_and_provider_cooldown() {
        let dir = tempfile::tempdir().unwrap();
        std::thread::scope(|scope| {
            let calls = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        shared_window(dir.path(), |w| Ok(w.reserve(10, 100_000))).unwrap()
                    })
                })
                .collect::<Vec<_>>();
            assert_eq!(
                calls
                    .into_iter()
                    .map(|v| v.join().unwrap())
                    .filter(|v| *v == 0)
                    .count(),
                3
            );
        });
        shared_window(dir.path(), |w| {
            w.blocked_until = 100 + WINDOW_MS;
            Ok(0)
        })
        .unwrap();
        assert_eq!(
            shared_window(dir.path(), |w| Ok(w.reserve(20, 1))).unwrap(),
            WINDOW_MS + 80
        );
    }
}
