//! Agent runtime: the collect → encode → send loop, and the spool replay that
//! keeps samples alive across a server outage.
//!
//! Kept separate from `main` so the send logic can be exercised without a
//! daemon, and so console mode and Windows service mode share one code path.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use ytmon_core::Batch;

use crate::collect::Collector;
use crate::config::Config;
use crate::report;

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// One scrape, stamped with the current second.
pub fn build(col: &mut Collector, host: &str, ctid: Option<u32>, seq: u64, server: &str) -> Batch {
    let ts = now();
    let mut b = Batch::new(host, ctid, seq);
    b.ip = local_ip(server);
    for s in col.collect(ts) {
        b.push(s);
    }
    b
}

/// This host's address on the default route.
///
/// `UdpSocket::connect` sends nothing — it only makes the kernel pick a
/// source address — so this works without any network traffic and without
/// enumerating interfaces (which returns the wrong one on multi-homed hosts).
/// The target is the ingest server itself: a route to it has to exist, since
/// reporting to it is the whole job, and it stays correct on hosts that have no
/// internet access or a single non-default route.
/// Reduce an ingest URL to the `host:port` that `connect` needs.
fn route_target(server: &str) -> String {
    let rest = server.split("://").nth(1).unwrap_or(server);
    let host_port = rest.split('/').next().unwrap_or(rest);
    if host_port.contains(':') {
        host_port.to_string()
    } else {
        format!("{host_port}:80")
    }
}

fn local_ip(server: &str) -> Option<String> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect(route_target(server)).ok()?;
    let addr = sock.local_addr().ok()?;
    let ip = addr.ip();
    if ip.is_loopback() || ip.is_unspecified() {
        return None;
    }
    Some(ip.to_string())
}

/// Run until `stop` is set. On Linux/console only Ctrl-C sets it; under the
/// Windows service dispatcher the SCM stop control sets it.
pub async fn run_until(
    cfg: Config,
    mut col: Collector,
    host: String,
    stop: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let mut seq: u64 = 0;
    let mut tick = tokio::time::interval(Duration::from_secs(cfg.interval_secs.max(5)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        if stop.load(Ordering::SeqCst) {
            tracing::info!("stop requested, exiting");
            return Ok(());
        }
        tick.tick().await;
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        seq += 1;

        replay_spool(&cfg, &client).await;

        let batch = build(&mut col, &host, cfg.ctid, seq, &cfg.server);
        let n = batch.samples.len();
        match report::encode(&batch) {
            Ok(body) => match report::post(
                &client,
                &cfg.server,
                body.clone(),
                cfg.server_token.as_deref(),
            )
            .await
            {
                // One line per success would be 1440 lines a day of "nothing is
                // wrong" on every host. Liveness is already visible from the
                // server side; keep the per-tick proof behind RUST_LOG=debug.
                Ok(()) => tracing::debug!(seq, samples = n, "sent"),
                Err(e) => {
                    tracing::warn!(seq, error = %e, "send failed, spooling");
                    match report::spool(&cfg.state_dir, seq, &body) {
                        Ok(0) => {}
                        Ok(dropped) => tracing::warn!(
                            seq,
                            dropped,
                            cap = report::SPOOL_MAX_FILES,
                            "spool full, oldest batches dropped"
                        ),
                        Err(e) => tracing::error!(error = %e, "spool write failed, sample lost"),
                    }
                }
            },
            Err(e) => tracing::error!(error = %e, "encode failed"),
        }
    }
}

/// Push a handful of spooled batches before the new one, oldest first. Stops
/// at the first failure so a long outage keeps its ordering and does not turn
/// one cycle into thousands of retries.
async fn replay_spool(cfg: &Config, client: &reqwest::Client) {
    for path in report::spool_files(&cfg.state_dir, 5) {
        let Ok(body) = std::fs::read(&path) else {
            continue;
        };
        match report::post(client, &cfg.server, body, cfg.server_token.as_deref()).await {
            Ok(()) => {
                let _ = std::fs::remove_file(&path);
            }
            Err(e) => {
                tracing::warn!(error = %e, "spool replay failed, keeping backlog");
                break;
            }
        }
    }
}
