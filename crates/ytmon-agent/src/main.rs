//! ytmon-agent — assembly only.
//!
//!   config.rs    agent.toml
//!   collect/     platform collectors (linux = cgroup v2, windows = sysinfo)
//!   report.rs    gzip encode, POST, disk spool
//!   runtime.rs   the collect → send loop
//!   service.rs   Windows SCM integration (cfg windows)

mod collect;
mod config;
mod report;
mod runtime;
#[cfg(windows)]
mod service;

use anyhow::Result;
use clap::Parser;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use collect::Collector;
use config::Config;

#[derive(Parser, Debug)]
#[command(name = "ytmon-agent", version, about = "ytmon host metrics agent")]
struct Args {
    /// Path to agent.toml (missing file falls back to defaults)
    #[arg(long, default_value = "/etc/ytmon/agent.toml")]
    config: String,
    /// Collect once, print the batch as JSON, do not send
    #[arg(long)]
    once: bool,
    /// Windows: hand control to the Service Control Manager
    #[arg(long)]
    service: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    #[cfg(windows)]
    if args.service {
        service::CONFIG_PATH.set(args.config.clone()).ok();
        init_logging(true);
        return service::run().map_err(|e| anyhow::anyhow!("service dispatcher failed: {e}"));
    }

    init_logging(false);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(run_console(args))
}

async fn run_console(args: Args) -> Result<()> {
    let cfg = Config::load(&args.config)?;
    let host = cfg.resolved_host();
    let mut col = Collector::new(cfg.mounts.clone());

    if args.once {
        let b = runtime::build(&mut col, &host, cfg.ctid, 0, &cfg.server);
        println!("{}", serde_json::to_string_pretty(&b)?);
        return Ok(());
    }

    tracing::info!(
        host = %host,
        server = %cfg.server,
        interval = cfg.interval_secs,
        "agent starting"
    );

    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_signal = stop.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            stop_for_signal.store(true, Ordering::SeqCst);
        }
    });

    runtime::run_until(cfg, col, host, stop).await
}

/// Service mode has no console, so trace output goes to a file next to the
/// state directory. Console mode keeps the default writer.
fn init_logging(to_file: bool) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());

    if to_file {
        let dir = std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".to_string());
        let dir = std::path::PathBuf::from(dir).join("ytmon");
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("agent.log"))
        {
            let writer =
                tracing_subscriber::fmt::writer::BoxMakeWriter::new(std::sync::Mutex::new(file));
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(writer)
                .init();
            return;
        }
    }

    tracing_subscriber::fmt().with_env_filter(filter).init();
}
