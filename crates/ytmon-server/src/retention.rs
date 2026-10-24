//! Downsampling and retention.
//!
//! Minute data is only useful for the recent past, but a year of it is not
//! affordable and not needed. Every hour the closed hour is folded into
//! `hourly`; every day the closed day is folded into `daily`; rows older than
//! each table's window are dropped.
//!
//! Running this on a timer rather than on ingest keeps the write path small and
//! makes catch-up automatic: `rollup:hour` / `rollup:day` in meta record how far
//! we got, so a server that was off for a week fills the gap on the next tick.

use std::time::Duration;

use crate::store::Store;

pub struct Cfg {
    pub interval_secs: u64,
    /// 0 disables stale-host cleanup.
    pub stale_host_days: i64,
    /// When true, pruning only counts what it would delete. The raw table is the
    /// only exact copy of minute data, so the first runs should be observed
    /// before anything is actually removed.
    pub prune_dry_run: bool,
}

pub async fn run(store: Store, cfg: Cfg) {
    // Catch up immediately: the server may have been down across several hour
    // boundaries.
    tick(&store, &cfg);

    let mut ticker = tokio::time::interval(Duration::from_secs(cfg.interval_secs.max(300)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        tick(&store, &cfg);
    }
}

fn tick(store: &Store, cfg: &Cfg) {
    let now = now_secs();

    match store.rollup_hourly(now) {
        Ok(s) if s.rows > 0 => {
            tracing::info!(buckets = s.buckets, rows = s.rows, "hourly rollup");
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "hourly rollup failed"),
    }

    match store.rollup_daily(now) {
        Ok(s) if s.rows > 0 => {
            tracing::info!(buckets = s.buckets, rows = s.rows, "daily rollup");
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "daily rollup failed"),
    }

    sweep_stale_hosts(store, cfg, now);

    match store.prune(now, cfg.prune_dry_run) {
        Ok(s) if s.raw_rows + s.hourly_rows + s.daily_rows > 0 => {
            if cfg.prune_dry_run {
                tracing::info!(
                    raw = s.raw_rows,
                    hourly = s.hourly_rows,
                    daily = s.daily_rows,
                    "prune dry-run: would delete"
                );
            } else {
                tracing::info!(
                    raw = s.raw_rows,
                    hourly = s.hourly_rows,
                    daily = s.daily_rows,
                    "pruned expired rows"
                );
            }
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "prune failed"),
    }
}

/// Drop hosts that stopped reporting long ago (a rename leaves the old name
/// behind as a permanently "offline" card).
///
/// Off by default and capped to one sweep per day: this deletes data, and a
/// guest that is simply switched off looks exactly like a renamed one until
/// enough time has passed that the operator has said so.
fn sweep_stale_hosts(store: &Store, cfg: &Cfg, now: i64) {
    if cfg.stale_host_days <= 0 {
        return;
    }
    let today = now / 86400;
    match store.get_meta("stale_sweep:day") {
        Ok(Some(v)) if v.parse::<i64>().ok() == Some(today) => return,
        _ => {}
    }

    let cutoff = now - cfg.stale_host_days * 86400;
    match store.hosts_stale_before(cutoff) {
        Ok(hosts) => {
            for host in hosts {
                match store.drop_host(&host) {
                    Ok(n) => tracing::info!(
                        host = %host,
                        series = n,
                        days = cfg.stale_host_days,
                        "dropped stale host"
                    ),
                    Err(e) => tracing::warn!(host = %host, error = %e, "drop stale host failed"),
                }
            }
        }
        Err(e) => tracing::warn!(error = %e, "stale host scan failed"),
    }
    let _ = store.set_meta("stale_sweep:day", &today.to_string());
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
