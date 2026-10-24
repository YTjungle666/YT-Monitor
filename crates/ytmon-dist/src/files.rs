//! Content scan and hash cache.
//!
//! Hashing a 3 MB binary on every manifest request would be wasteful, so the
//! result is cached against (path, mtime, size). Replacing the file changes the
//! mtime and invalidates the entry — that is how "push an update" works: drop a
//! new binary in `bin/`, the next `/version.json` reports a new digest.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

/// What we know about one servable file.
#[derive(Clone)]
pub struct Entry {
    /// Path relative to the content root, e.g. `bin/ytmon-agent`.
    pub path: String,
    pub size: u64,
    pub mtime: i64,
    pub sha256: String,
}

/// A cached fingerprint: enough to tell "same file" from "replaced file", so
/// the manifest does not re-hash a 3 MB binary on every request.
#[derive(Clone, Debug)]
pub struct Hashed {
    pub size: u64,
    pub mtime: i64,
    pub sha256: String,
}

pub type Cache = Arc<RwLock<HashMap<PathBuf, Hashed>>>;

/// Directories under the root that are published. Anything else is invisible:
/// the service must never expose an arbitrary path on the host.
pub const PUBLISHED_DIRS: [&str; 2] = ["bin", "install"];

/// Reject anything that could escape the content root.
pub fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn meta(path: &Path) -> Option<(u64, i64)> {
    let md = std::fs::metadata(path).ok()?;
    if !md.is_file() {
        return None;
    }
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Some((md.len(), mtime))
}

async fn digest(path: &Path) -> Option<String> {
    let bytes = tokio::fs::read(path).await.ok()?;
    let mut h = Sha256::new();
    h.update(&bytes);
    Some(format!("{:x}", h.finalize()))
}

/// Hash for `path`, reusing the cache when size and mtime both match.
pub async fn entry(root: &Path, rel: &str, cache: &Cache) -> Option<Entry> {
    let full = root.join(rel);
    let (size, mtime) = meta(&full)?;
    {
        let guard = cache.read().await;
        if let Some(c) = guard.get(&full) {
            if c.size == size && c.mtime == mtime {
                return Some(Entry {
                    path: rel.to_string(),
                    size,
                    mtime,
                    sha256: c.sha256.clone(),
                });
            }
        }
    }
    let sha = digest(&full).await?;
    cache.write().await.insert(
        full,
        Hashed {
            size,
            mtime,
            sha256: sha.clone(),
        },
    );
    Some(Entry {
        path: rel.to_string(),
        size,
        mtime,
        sha256: sha,
    })
}

/// Every published file, sorted by path.
pub async fn catalog(root: &Path, cache: &Cache) -> Vec<Entry> {
    let mut out = Vec::new();
    for dir in PUBLISHED_DIRS {
        let Ok(mut rd) = tokio::fs::read_dir(root.join(dir)).await else {
            continue;
        };
        while let Ok(Some(e)) = rd.next_entry().await {
            let name = e.file_name().to_string_lossy().to_string();
            if !safe_name(&name) {
                continue;
            }
            // Dotfiles are internal bookkeeping (temp files during an upload).
            if name.starts_with('.') {
                continue;
            }
            if let Some(entry) = entry(root, &format!("{dir}/{name}"), cache).await {
                out.push(entry);
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}
