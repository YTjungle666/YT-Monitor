//! ytmon-dist — package distribution for the fleet.
//!
//! One HTTP surface on the monitoring host, so installing or updating an agent
//! is always the same two commands on the target:
//!
//!   curl -fsSL http://monitor.example.com/update/install/pve.sh | sh
//!
//! The same routes are mounted by `ytmon-server` under `/update`; this binary
//! exists for a host that wants to publish without running the dashboard.
//!
//! Everything is read-only. The service serves whatever is in `root/bin` and
//! `root/install`, and publishes a manifest with sizes and SHA-256 so callers
//! can decide "already current, skip" without downloading the binary.
//!
//!   config.rs   listen address, content root and path prefix
//!   files.rs    directory scan + hash cache
//!   api.rs      HTTP routes

use ytmon_dist::api::{self, AppState};
use ytmon_dist::config;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use tracing_subscriber::EnvFilter;
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cfg = config::Config::load("/etc/ytmon/dist.toml")?;
    if !std::path::Path::new(&cfg.root).is_dir() {
        anyhow::bail!("content root {} does not exist", cfg.root);
    }

    let state = AppState::new(&cfg.root, &cfg.prefix);

    let addr: std::net::SocketAddr = cfg.listen.parse()?;
    tracing::info!(%addr, root = %cfg.root, prefix = %cfg.prefix, "ytmon-dist listening");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, api::router(state)).await?;
    Ok(())
}
