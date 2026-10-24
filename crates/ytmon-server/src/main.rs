//! ytmon-server — assembly only: load config, open the store, start workers,
//! serve HTTP. Every behaviour lives in its own module.
//!
//!   config.rs  /etc/ytmon/server.toml
//!   state.rs   shared handler state
//!   api.rs     HTTP routes + handlers
//!   web.rs     the single HTML page
//!   store.rs   redb schema, ingest, queries
//!   probe.rs   ICMP / TCP / HTTP active probes
//!   pve.rs     read-only PVE discovery
//!   alert/     rules, state machine, Telegram delivery

mod alert;
mod api;
mod config;
mod probe;
mod pve;
mod report;
mod retention;
mod state;
mod store;
mod web;

use clap::Parser;
use std::sync::Arc;
use tokio::sync::RwLock;

use config::FileCfg;
use state::App;
use store::Store;

#[derive(Parser, Debug)]
#[command(name = "ytmon-server", version, about = "ytmon ingest + query server")]
struct Args {
    /// Path to server.toml (missing file = defaults, discovery/probes off)
    #[arg(long, default_value = "/etc/ytmon/server.toml")]
    config: String,
    /// Overridden by `listen` in the config file
    #[arg(long, default_value = "0.0.0.0:9180")]
    listen: String,
    /// Overridden by `db` in the config file
    #[arg(long, default_value = "/var/lib/ytmon/ytmon.db")]
    db: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use tracing_subscriber::EnvFilter;
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let args = Args::parse();
    let cfg = Arc::new(FileCfg::load(&args.config)?);

    let listen = cfg.listen_or(&args.listen);
    let db_path = cfg.db_or(&args.db);
    if let Some(dir) = std::path::Path::new(&db_path).parent() {
        std::fs::create_dir_all(dir)?;
    }

    let store = Store::open(&db_path)?;
    tracing::info!(db = %db_path, series = store.series_count()?, "store opened");

    let pve_guests: Arc<RwLock<Vec<pve::Guest>>> = Arc::new(RwLock::new(Vec::new()));

    if let Some(pcfg) = cfg.pve_cfg() {
        tracing::info!(node = %pcfg.node, base = %pcfg.base, "pve discovery enabled");
        tokio::spawn(pve::run(store.clone(), pcfg, pve_guests.clone()));
    }

    if !cfg.probes.is_empty() {
        tracing::info!(count = cfg.probes.len(), "probes enabled");
        tokio::spawn(probe::run(
            store.clone(),
            cfg.probes.clone(),
            cfg.probe_interval(),
        ));
    }

    if let Some(acfg) = cfg.alert_cfg() {
        tracing::info!(
            rules = %acfg.rules_file,
            interval = acfg.interval_secs,
            "alerting enabled"
        );
        tokio::spawn(alert::run(store.clone(), acfg));
    }

    if let Some(rcfg) = cfg.retention_cfg() {
        tracing::info!(
            interval = rcfg.interval_secs,
            dry_run = rcfg.prune_dry_run,
            "retention worker enabled"
        );
        tokio::spawn(retention::run(store.clone(), rcfg));
    }

    if let Some((tg, rcfg)) = cfg.monthly_report() {
        tracing::info!(
            hour = rcfg.hour,
            source = if rcfg.pve.is_some() { "pve-rrd" } else { "agent" },
            exclude = ?rcfg.exclude,
            "monthly traffic report enabled"
        );
        tokio::spawn(report::run(store.clone(), tg, rcfg, pve_guests.clone()));
    }

    // One-time identity migration: series were keyed by host name, so a rename
    // used to strand the history under the old name. Rewrite them onto node ids
    // (ct:400, vm:100, host:pve) before anything starts writing again.
    let agents = cfg.agents.clone().unwrap_or_default();
    if store.get_meta("identity:v2")?.is_none() {
        let map: std::collections::HashMap<String, String> = agents
            .iter()
            .map(|a| (a.name.clone(), a.node_id.clone()))
            .collect();
        match store.migrate_identity(&map) {
            Ok(n) => {
                tracing::info!(series = n, "identity migration: host names -> node ids");
                store.set_meta("identity:v2", "1")?;
            }
            Err(e) => tracing::error!(error = %e, "identity migration failed, will retry"),
        }
    }

    let state = App {
        store,
        pve_guests,
        cfg: cfg.clone(),
        tokens: App::token_book(&agents),
    };
    // Two routers from one state: the ingest API (no page, no credentials) and
    // the dashboard (Basic auth). Keeping them separate means `GET /` on the
    // agent port is a 404, not an unauthenticated page.
    let ingest = api::ingest_router(state.clone());
    let panel = api::panel_router(state);

    // An optional second listener serves the dashboard, so it can sit on :80
    // while agents keep posting to the original port — changing the ingest URL
    // would mean touching every agent config, including the hypervisor's.
    if let Some(extra) = &cfg.extra_listen {
        let addr: std::net::SocketAddr = extra.parse()?;
        let second = tokio::net::TcpListener::bind(addr).await?;
        tracing::info!(%addr, "dashboard listening");
        tokio::spawn(async move {
            if let Err(e) = axum::serve(second, panel).await {
                tracing::error!(error = %e, "dashboard listener stopped");
            }
        });
    }

    let addr: std::net::SocketAddr = listen.parse()?;
    tracing::info!(%addr, "ingest listening");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, ingest).await?;
    Ok(())
}
