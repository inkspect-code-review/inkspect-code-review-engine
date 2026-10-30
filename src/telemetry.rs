//! Durable receipts survive a malformed model response or a failed child process.
use std::io::Write;
use crate::error::{err, Code, Result};
use std::sync::atomic::{AtomicUsize, Ordering};
static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
pub fn next_id() -> usize { SEQUENCE.fetch_add(1, Ordering::Relaxed) + 1 }

pub fn record(value: serde_json::Value) -> Result<()> {
    let Ok(path) = std::env::var("INKSPECT_USAGE_FILE") else { return Ok(()); };
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)
        .map_err(|_| err(Code::Internal, "usage journal unavailable"))?;
    let mut bytes = serde_json::to_vec(&value).map_err(|_| err(Code::Internal, "usage encoding failed"))?;
    bytes.push(b'\n');
    file.write_all(&bytes).and_then(|_| file.sync_data())
        .map_err(|_| err(Code::Internal, "usage journal write failed"))
}
