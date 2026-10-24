//! ytmon-dist — package distribution for the fleet.
//!
//! One permanent HTTP service on the monitoring host, so installing or
//! updating an agent is always the same two commands on the target:
//!
//!   curl -fsSL http://monitor.example.com:8088/install/pve.sh | sh
//!
//! Everything is read-only. The service serves whatever is in `root/bin` and
//! `root/install`, and publishes a manifest with sizes and SHA-256 so callers
//! can decide "already current, skip" without downloading the binary.
//!
//!   config.rs   listen address and content root
//!   files.rs    directory scan + hash cache
//!   api.rs      HTTP routes

mod api;
mod config;
mod files;

use std::sync::Arc;
use tokio::sync::RwLock;

use api::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use tracing_subscriber::EnvFilter;
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cfg = config::Config::load("/etc/ytmon/dist.toml")?;
    let root = std::path::PathBuf::from(&cfg.root);
    if !root.is_dir() {
        anyhow::bail!("content root {} does not exist", root.display());
    }

    let state = AppState {
        root: Arc::new(root.clone()),
        cache: Arc::new(RwLock::new(std::collections::HashMap::new())),
    };

    let addr: std::net::SocketAddr = cfg.listen.parse()?;
    tracing::info!(%addr, root = %root.display(), "ytmon-dist listening");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, api::router(state)).await?;
    Ok(())
}
