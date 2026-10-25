//! Transport: gzip JSON POST plus a disk spool so a server outage or a
//! network blip does not silently drop samples.

use anyhow::{Context, Result};
use flate2::{write::GzEncoder, Compression};
use std::io::Write;
use std::path::{Path, PathBuf};
use ytmon_core::Batch;

pub fn encode(batch: &Batch) -> Result<Vec<u8>> {
    let json = serde_json::to_vec(batch)?;
    let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
    enc.write_all(&json)?;
    Ok(enc.finish()?)
}

pub async fn post(
    client: &reqwest::Client,
    url: &str,
    body: Vec<u8>,
    token: Option<&str>,
) -> Result<()> {
    let mut req = client
        .post(url)
        .header("content-type", "application/json")
        .header("content-encoding", "gzip");
    if let Some(t) = token.filter(|t| !t.is_empty()) {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = req.body(body).send().await.context("POST to server")?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("server returned {status}: {text}");
    }
    Ok(())
}

/// Hard cap on spooled batches per agent.
///
/// A batch is ~0.5 KB compressed, so 200 batches is ~100 KB — enough to cover
/// a couple of hours of outage without letting a permanently broken server
/// address fill the guest's disk. `spool_files` bounds only how many files one
/// cycle replays; without a cap here the directory itself grew forever.
pub const SPOOL_MAX_FILES: usize = 200;

/// Write a failed batch to `<state_dir>/spool/<seq>.json.gz`.
///
/// Returns how many of the oldest batches had to be dropped to stay under
/// `SPOOL_MAX_FILES`, so the caller can say out loud that data was lost.
pub fn spool(dir: &str, seq: u64, body: &[u8]) -> Result<usize> {
    let d = Path::new(dir).join("spool");
    std::fs::create_dir_all(&d)?;
    let path = d.join(format!("{seq:012}.json.gz"));
    std::fs::write(&path, body).with_context(|| format!("spool {}", path.display()))?;
    Ok(trim_spool(&d, SPOOL_MAX_FILES))
}

/// Delete the oldest batches until at most `max` remain. File names sort by
/// sequence number, so lexical order is age order. Returns the drop count;
/// a failed unlink is counted as kept, not as removed.
fn trim_spool(dir: &Path, max: usize) -> usize {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut v: Vec<PathBuf> = rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|e| e == "gz").unwrap_or(false))
        .collect();
    if v.len() <= max {
        return 0;
    }
    v.sort();
    let excess = v.len() - max;
    let mut dropped = 0;
    for p in v.iter().take(excess) {
        if std::fs::remove_file(p).is_ok() {
            dropped += 1;
        }
    }
    dropped
}

/// Oldest-first list of spooled batches, capped so a long outage cannot
/// make one cycle try to replay thousands of files.
pub fn spool_files(dir: &str, limit: usize) -> Vec<PathBuf> {
    let d = Path::new(dir).join("spool");
    let Ok(rd) = std::fs::read_dir(&d) else {
        return Vec::new();
    };
    let mut v: Vec<PathBuf> = rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|e| e == "gz").unwrap_or(false))
        .collect();
    v.sort();
    v.truncate(limit);
    v
}
