//! redb-backed time-series store.
//!
//! Layout is intentionally flat and boring:
//!   series : series_id(u64)  -> canonical series key ("host|metric|k=v")
//!   raw    : [id_be(8) | ts_be(8)] -> f64
//!   latest : series_id(u64)  -> f64
//!   meta   : "hosts"         -> JSON array of host names
//!   hourly : [id_be(8) | bucket_be(8)] -> mean over that hour (365d)
//!   daily  : [id_be(8) | bucket_be(8)] -> mean over that day  (730d)
//!
//! One row per series per scrape is plenty at household scale (238 series ×
//! 1/min ≈ 343k rows/day). Two rollup tables keep a year of history without
//! keeping a year of minute samples:
//!
//!   raw    : 1 minute, 14 days   (the recent window, exact)
//!   hourly : 1 hour,   365 days  (trends)
//!   daily  : 1 day,    730 days  (long-term; bounded, not forever)
//!
//! Rollups store the mean of the bucket. That is right for gauges (cpu, mem,
//! disk) and good enough for the trend charts; monthly traffic totals come from
//! PVE's own RRD, not from these counters, so a counter rolled up to a mean is
//! never used for arithmetic that would be wrong.

use anyhow::Result;
use redb::{Database, ReadableTable, TableDefinition};
use std::sync::Arc;
use ytmon_core::{fnv1a64, series_key, Batch};

const T_SERIES: TableDefinition<u64, &str> = TableDefinition::new("series");
const T_RAW: TableDefinition<[u8; 16], f64> = TableDefinition::new("raw");
const T_LATEST: TableDefinition<u64, f64> = TableDefinition::new("latest");
const T_META: TableDefinition<&str, &str> = TableDefinition::new("meta");
/// Same key layout as `raw`, one row per series per bucket.
const T_HOURLY: TableDefinition<[u8; 16], f64> = TableDefinition::new("hourly");
const T_DAILY: TableDefinition<[u8; 16], f64> = TableDefinition::new("daily");

/// How long each resolution is kept, in days. Every table is bounded — an
/// unlimited one is a slow disk leak. Two years of daily means costs ~10 MB, so
/// the long table gets two years rather than one and needs no second thought.
pub const RAW_RETENTION_DAYS: i64 = 14;
pub const HOURLY_RETENTION_DAYS: i64 = 365;
pub const DAILY_RETENTION_DAYS: i64 = 730;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct RollupStats {
    pub buckets: usize,
    pub rows: usize,
}

/// Which table a query reads.
#[derive(Debug, Clone, Copy)]
pub enum Resolution {
    Raw,
    Hourly,
    Daily,
}

#[derive(Debug, Default)]
pub struct PruneStats {
    pub raw_rows: usize,
    pub hourly_rows: usize,
    pub daily_rows: usize,
}

#[derive(Clone)]
pub struct Store {
    db: Arc<Database>,
}

pub struct IngestResult {
    pub stored: usize,
    pub new_series: usize,
}

fn pack(id: u64, ts: i64) -> [u8; 16] {
    let mut k = [0u8; 16];
    k[..8].copy_from_slice(&id.to_be_bytes());
    k[8..].copy_from_slice(&(ts as u64).to_be_bytes());
    k
}

fn unpack_ts(k: [u8; 16]) -> i64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&k[8..]);
    i64::from_be_bytes(b)
}

impl Store {
    pub fn open(path: &str) -> Result<Self> {
        let db = Database::create(path)?;
        let w = db.begin_write()?;
        {
            w.open_table(T_SERIES)?;
            w.open_table(T_RAW)?;
            w.open_table(T_LATEST)?;
            w.open_table(T_META)?;
            w.open_table(T_HOURLY)?;
            w.open_table(T_DAILY)?;
        }
        w.commit()?;
        Ok(Self { db: Arc::new(db) })
    }

    /// Store a batch sent by the host itself. Reaching this path is what
    /// makes a host "alive": the request came from the machine.
    pub fn ingest(&self, batch: &Batch) -> Result<IngestResult> {
        self.ingest_inner(batch, true)
    }

    /// Store a batch that observes a host without proving it answers: PVE
    /// discovery (the hypervisor says "running", which is inventory, not
    /// reachability) and a probe that just failed. Such a batch writes data but
    /// must not touch `up:`, otherwise a replaced or hung machine stays green
    /// forever — CT401 did, for as long as PVE kept reporting it running.
    pub fn ingest_passive(&self, batch: &Batch) -> Result<IngestResult> {
        self.ingest_inner(batch, false)
    }

    fn ingest_inner(&self, batch: &Batch, marks_alive: bool) -> Result<IngestResult> {
        let w = self.db.begin_write()?;
        let mut stored = 0usize;
        let mut new_series = 0usize;
        {
            let mut series = w.open_table(T_SERIES)?;
            let mut raw = w.open_table(T_RAW)?;
            let mut latest = w.open_table(T_LATEST)?;

            // Newest timestamp we actually persisted. Samples with NaN/Inf are
            // skipped below, and they must not count as data or as liveness.
            let mut newest_stored: Option<i64> = None;

            for s in &batch.samples {
                if !s.value.is_finite() {
                    continue;
                }
                let key = series_key(&batch.host, &s.metric, &s.labels);
                let id = fnv1a64(key.as_bytes());
                if series.get(id)?.is_none() {
                    series.insert(id, key.as_str())?;
                    new_series += 1;
                }
                raw.insert(pack(id, s.ts), s.value)?;
                latest.insert(id, s.value)?;
                stored += 1;
                newest_stored = Some(newest_stored.map_or(s.ts, |m: i64| m.max(s.ts)));
            }

            let mut meta = w.open_table(T_META)?;
            let prev = meta
                .get("hosts")?
                .map(|g| g.value().to_string())
                .unwrap_or_default();
            let mut hosts: Vec<String> = serde_json::from_str(&prev).unwrap_or_else(|_| Vec::new());
            if !hosts.iter().any(|h| h == &batch.host) {
                hosts.push(batch.host.clone());
                hosts.sort();
                let json = serde_json::to_string(&hosts)?;
                meta.insert("hosts", json.as_str())?;
            }

            // Liveness for the "agent stopped reporting" rule, recorded from
            // the server's clock: an agent whose clock runs fast would other-
            // wise always look fresh, and a spooled batch replayed after an
            // outage would look like it just arrived.
            //
            // Only a batch that actually stored something counts. A collector
            // that returns NaN for every metric still connects and still
            // posts, and it has no data to give — marking that agent "up"
            // would show a green card with frozen numbers next to it.
            if marks_alive && stored > 0 {
                let up_key = format!("up:{}", batch.host);
                let up_val = now_secs().to_string();
                meta.insert(up_key.as_str(), up_val.as_str())?;
            }

            // Data freshness, kept separately: the newest sample timestamp
            // tells us whether what we store is current, which is not the same
            // question as whether the agent is alive.
            if let Some(ts) = newest_stored {
                let key = format!("seen:{}", batch.host);
                let val = ts.to_string();
                meta.insert(key.as_str(), val.as_str())?;
            }

            if let Some(ip) = &batch.ip {
                let key = format!("ip:{}", batch.host);
                meta.insert(key.as_str(), ip.as_str())?;
            }
        }
        w.commit()?;
        Ok(IngestResult { stored, new_series })
    }

    pub fn hosts(&self) -> Result<Vec<String>> {
        let r = self.db.begin_read()?;
        let meta = r.open_table(T_META)?;
        let json = meta
            .get("hosts")?
            .map(|g| g.value().to_string())
            .unwrap_or_default();
        Ok(serde_json::from_str(&json).unwrap_or_default())
    }

    /// All series keys for one host, with their latest value.
    pub fn latest_for_host(&self, host: &str) -> Result<Vec<(String, f64)>> {
        let r = self.db.begin_read()?;
        let series = r.open_table(T_SERIES)?;
        let latest = r.open_table(T_LATEST)?;
        let prefix = format!("{host}|");
        let mut out = Vec::new();
        for item in series.iter()? {
            let (k, v) = item?;
            let key = v.value().to_string();
            if !key.starts_with(&prefix) {
                continue;
            }
            if let Some(val) = latest.get(k.value())? {
                out.push((key, val.value()));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// Raw samples for one series key inside a time window, oldest first.
    pub fn range(&self, key: &str, from: i64, to: i64) -> Result<Vec<(i64, f64)>> {
        let id = fnv1a64(key.as_bytes());
        let r = self.db.begin_read()?;
        let raw = r.open_table(T_RAW)?;
        let mut out = Vec::new();
        for item in raw.range(pack(id, from)..pack(id, to))? {
            let (k, v) = item?;
            out.push((unpack_ts(k.value()), v.value()));
        }
        Ok(out)
    }

    pub fn series_count(&self) -> Result<usize> {
        let r = self.db.begin_read()?;
        let t = r.open_table(T_SERIES)?;
        Ok(t.iter()?.filter(|i| i.is_ok()).count())
    }

    /// Bytes moved by a cumulative counter inside `[from, to)`.
    ///
    /// A container restart recreates the cgroup and its veth, so the counter
    /// drops back to zero; `last - first` would then be negative or wildly
    /// wrong. Sum the per-sample deltas instead, and treat a backwards step as
    /// "restarted — everything the counter now shows happened inside this
    /// window".
    ///
    /// One sample is taken from before the window so the first interval is not
    /// silently dropped.
    pub fn counter_delta(&self, key: &str, from: i64, to: i64) -> Result<f64> {
        const BASELINE_LOOKBACK: i64 = 600;
        let rows = self.range(key, from - BASELINE_LOOKBACK, to)?;
        let mut total = 0.0;
        let mut prev: Option<f64> = None;
        for (ts, v) in rows {
            if let Some(p) = prev {
                if ts >= from {
                    let d = v - p;
                    total += if d >= 0.0 { d } else { v };
                }
            }
            prev = Some(v);
        }
        Ok(total)
    }

    /// When each host last delivered a batch, by the server's clock:
    /// `up:<host>` -> ts. Falls back to the sample timestamp for rows written
    /// before `up:` existed, so an upgrade does not make every host look dead.
    pub fn last_seen(&self) -> Result<std::collections::HashMap<String, i64>> {
        let r = self.db.begin_read()?;
        let meta = r.open_table(T_META)?;
        let mut out = std::collections::HashMap::new();
        for item in meta.iter()? {
            let (k, v) = item?;
            let key = k.value();
            if let Some(host) = key.strip_prefix("up:") {
                if let Ok(ts) = v.value().parse::<i64>() {
                    out.insert(host.to_string(), ts);
                }
            }
        }
        // Backfill from the data clock is deliberately gone: `seen:` is also
        // written by PVE discovery, so a guest with no agent would inherit a
        // fresh timestamp from the hypervisor poll and read as alive. Liveness
        // has exactly one source now, the `up:` row written by an agent or a
        // probe.
        Ok(out)
    }

    /// Reported address per host: `ip:<host>` -> "192.0.2.10".
    pub fn host_ips(&self) -> Result<std::collections::HashMap<String, String>> {
        let r = self.db.begin_read()?;
        let meta = r.open_table(T_META)?;
        let mut out = std::collections::HashMap::new();
        for item in meta.iter()? {
            let (k, v) = item?;
            if let Some(host) = k.value().strip_prefix("ip:") {
                out.insert(host.to_string(), v.value().to_string());
            }
        }
        Ok(out)
    }

    /// Every series key with its latest value, in one pass. The alert loop
    /// needs all of them each tick; calling `latest_for_host` per host would
    /// rescan the whole series table 17 times.
    pub fn all_latest(&self) -> Result<Vec<(String, f64)>> {
        let r = self.db.begin_read()?;
        let series = r.open_table(T_SERIES)?;
        let latest = r.open_table(T_LATEST)?;
        let mut out = Vec::new();
        for item in series.iter()? {
            let (k, v) = item?;
            if let Some(val) = latest.get(k.value())? {
                out.push((v.value().to_string(), val.value()));
            }
        }
        Ok(out)
    }

    /// Alert state persists here so a server restart neither re-fires an
    /// active alert nor loses track of one that was already notified.
    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let r = self.db.begin_read()?;
        let meta = r.open_table(T_META)?;
        Ok(meta.get(key)?.map(|g| g.value().to_string()))
    }

    /// All metadata entries whose key starts with `prefix`, sorted by key.
    /// Used to list archived monthly reports without knowing the months.
    pub fn meta_with_prefix(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        let r = self.db.begin_read()?;
        let meta = r.open_table(T_META)?;
        let mut out = Vec::new();
        for item in meta.iter()? {
            let (k, v) = item?;
            let key = k.value();
            if key.starts_with(prefix) {
                out.push((key.to_string(), v.value().to_string()));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        let w = self.db.begin_write()?;
        {
            let mut meta = w.open_table(T_META)?;
            meta.insert(key, value)?;
        }
        w.commit()?;
        Ok(())
    }

    // ---- rollups and retention ------------------------------------------

    fn series_ids(&self) -> Result<Vec<u64>> {
        let r = self.db.begin_read()?;
        let t = r.open_table(T_SERIES)?;
        let mut out = Vec::new();
        for item in t.iter()? {
            out.push(item?.0.value());
        }
        Ok(out)
    }

    /// Fold every complete hour since the last run into `hourly`.
    ///
    /// Progress is recorded in meta, so a restart — or a server that was off for
    /// three days — catches up instead of silently leaving a hole in the data.
    /// Re-running over a bucket is harmless: the values are means, not sums.
    pub fn rollup_hourly(&self, now: i64) -> Result<RollupStats> {
        let hour_end = now - now.rem_euclid(3600);
        let last: Option<i64> = self.get_meta("rollup:hour")?.and_then(|s| s.parse().ok());
        // `rollup:hour` is "processed up to here", so the next run resumes
        // exactly there. Adding another hour would skip one bucket for every
        // restart — the gap this marker exists to prevent.
        let start = match last {
            None => hour_end - 3600,
            Some(l) => {
                if l >= hour_end {
                    return Ok(RollupStats::default());
                }
                l
            }
        };

        let ids = self.series_ids()?;
        let mut stats = RollupStats::default();
        let w = self.db.begin_write()?;
        {
            let raw = w.open_table(T_RAW)?;
            let mut hourly = w.open_table(T_HOURLY)?;
            let mut bucket = start;
            while bucket < hour_end {
                stats.buckets += 1;
                for id in &ids {
                    let (mut sum, mut n) = (0.0f64, 0usize);
                    for item in raw.range(pack(*id, bucket)..pack(*id, bucket + 3600))? {
                        let (_, v) = item?;
                        sum += v.value();
                        n += 1;
                    }
                    if n > 0 {
                        hourly.insert(pack(*id, bucket), sum / n as f64)?;
                        stats.rows += 1;
                    }
                }
                bucket += 3600;
            }
            let mut meta = w.open_table(T_META)?;
            meta.insert("rollup:hour", hour_end.to_string().as_str())?;
        }
        w.commit()?;
        Ok(stats)
    }

    /// Fold every complete day into `daily`, sourced from the hourly table.
    pub fn rollup_daily(&self, now: i64) -> Result<RollupStats> {
        let day_end = now - now.rem_euclid(86400);
        let last: Option<i64> = self.get_meta("rollup:day")?.and_then(|s| s.parse().ok());
        let start = match last {
            None => day_end - 86400,
            Some(l) => {
                if l >= day_end {
                    return Ok(RollupStats::default());
                }
                l
            }
        };

        let ids = self.series_ids()?;
        let mut stats = RollupStats::default();
        let w = self.db.begin_write()?;
        {
            let hourly = w.open_table(T_HOURLY)?;
            let mut daily = w.open_table(T_DAILY)?;
            let mut bucket = start;
            while bucket < day_end {
                stats.buckets += 1;
                for id in &ids {
                    let (mut sum, mut n) = (0.0f64, 0usize);
                    for item in hourly.range(pack(*id, bucket)..pack(*id, bucket + 86400))? {
                        let (_, v) = item?;
                        sum += v.value();
                        n += 1;
                    }
                    if n > 0 {
                        daily.insert(pack(*id, bucket), sum / n as f64)?;
                        stats.rows += 1;
                    }
                }
                bucket += 86400;
            }
            let mut meta = w.open_table(T_META)?;
            meta.insert("rollup:day", day_end.to_string().as_str())?;
        }
        w.commit()?;
        Ok(stats)
    }

    /// Delete rows older than each table's retention window.
    ///
    /// `dry_run` only counts, so the first run can be inspected before anything
    /// is dropped — the raw window is the only source of exact minute data, and
    /// deleting past it is not reversible.
    pub fn prune(&self, now: i64, dry_run: bool) -> Result<PruneStats> {
        let raw_before = now - RAW_RETENTION_DAYS * 86400;
        let hourly_before = now - HOURLY_RETENTION_DAYS * 86400;
        let daily_before = now - DAILY_RETENTION_DAYS * 86400;
        let ids = self.series_ids()?;
        let mut stats = PruneStats::default();

        let w = self.db.begin_write()?;
        {
            let mut raw = w.open_table(T_RAW)?;
            let mut hourly = w.open_table(T_HOURLY)?;
            let mut daily = w.open_table(T_DAILY)?;
            for id in &ids {
                // Collect first: removing while iterating the same range would
                // invalidate the iterator.
                let mut victims = Vec::new();
                for item in raw.range(pack(*id, 0)..pack(*id, raw_before))? {
                    victims.push(item?.0.value());
                }
                stats.raw_rows += victims.len();
                if !dry_run {
                    for k in victims {
                        raw.remove(k)?;
                    }
                }

                let mut victims = Vec::new();
                for item in hourly.range(pack(*id, 0)..pack(*id, hourly_before))? {
                    victims.push(item?.0.value());
                }
                stats.hourly_rows += victims.len();
                if !dry_run {
                    for k in victims {
                        hourly.remove(k)?;
                    }
                }

                let mut victims = Vec::new();
                for item in daily.range(pack(*id, 0)..pack(*id, daily_before))? {
                    victims.push(item?.0.value());
                }
                stats.daily_rows += victims.len();
                if !dry_run {
                    for k in victims {
                        daily.remove(k)?;
                    }
                }
            }
        }
        if dry_run {
            w.abort()?;
        } else {
            w.commit()?;
        }
        Ok(stats)
    }

    /// Display label per node id (`name:<node_id>` -> `web-01`).
    pub fn name_map(&self) -> Result<std::collections::HashMap<String, String>> {
        let r = self.db.begin_read()?;
        let meta = r.open_table(T_META)?;
        let mut out = std::collections::HashMap::new();
        for item in meta.iter()? {
            let (k, v) = item?;
            if let Some(id) = k.value().strip_prefix("name:") {
                out.insert(id.to_string(), v.value().to_string());
            }
        }
        Ok(out)
    }

    /// Record the label the agent reports. Called on every batch, so a rename
    /// shows up within a minute and nothing else has to move.
    pub fn set_name(&self, node_id: &str, name: &str) -> Result<()> {
        let w = self.db.begin_write()?;
        {
            let mut meta = w.open_table(T_META)?;
            let key = format!("name:{node_id}");
            meta.insert(key.as_str(), name)?;
        }
        w.commit()?;
        Ok(())
    }

    /// Rewrite series keys from the old `host name | metric` scheme to node ids.
    ///
    /// `web-01|cpu.usage_pct` becomes `ct:400|cpu.usage_pct`, which is what
    /// makes a later rename a label change instead of a new machine. Series ids
    /// are hashes of the key, so the id changes too: `latest` and the three
    /// point tables have to move with it. Guarded by `identity:v2` in meta so a
    /// restart cannot run it twice.
    pub fn migrate_identity(
        &self,
        map: &std::collections::HashMap<String, String>,
    ) -> Result<usize> {
        let w = self.db.begin_write()?;
        let mut moved = 0usize;
        let mut renames: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();
        {
            let mut series = w.open_table(T_SERIES)?;
            let mut victims: Vec<(u64, String)> = Vec::new();
            for item in series.iter()? {
                let (k, v) = item?;
                let key = v.value();
                let Some((host, rest)) = key.split_once('|') else {
                    continue;
                };
                if let Some(node) = map.get(host) {
                    victims.push((k.value(), format!("{node}|{rest}")));
                }
            }
            for (old_id, new_key) in victims {
                let new_id = fnv1a64(new_key.as_bytes());
                series.insert(new_id, new_key.as_str())?;
                series.remove(old_id)?;
                renames.insert(old_id, new_id);
                moved += 1;
            }
        }
        if !renames.is_empty() {
            let mut latest = w.open_table(T_LATEST)?;
            for (old_id, new_id) in &renames {
                // Take the value out first: the guard borrows `latest`.
                let val = latest.remove(*old_id)?.map(|g| g.value());
                if let Some(v) = val {
                    latest.insert(*new_id, v)?;
                }
            }
        }
        // The point tables key on (series id, ts): collect first, then rewrite,
        // because redb will not let a cursor and a write share a table.
        macro_rules! move_points {
            ($table:expr) => {{
                let mut tbl = w.open_table($table)?;
                let mut moves: Vec<([u8; 16], f64, [u8; 16])> = Vec::new();
                for item in tbl.iter()? {
                    let (k, v) = item?;
                    let kb = k.value();
                    let mut idb = [0u8; 8];
                    idb.copy_from_slice(&kb[..8]);
                    let id = u64::from_be_bytes(idb);
                    if let Some(new_id) = renames.get(&id) {
                        moves.push((kb, v.value(), pack(*new_id, unpack_ts(kb))));
                    }
                }
                for (old_k, val, new_k) in moves {
                    tbl.remove(old_k)?;
                    tbl.insert(new_k, val)?;
                }
            }};
        }
        if !renames.is_empty() {
            move_points!(T_RAW);
            move_points!(T_HOURLY);
            move_points!(T_DAILY);
        }
        w.commit()?;
        Ok(moved)
    }

    /// Hosts whose last batch is older than `cutoff` (server clock).
    pub fn hosts_stale_before(&self, cutoff: i64) -> Result<Vec<String>> {
        let seen = self.last_seen()?;
        let mut out: Vec<String> = seen
            .into_iter()
            .filter(|(_, ts)| *ts < cutoff)
            .map(|(h, _)| h)
            .collect();
        out.sort();
        Ok(out)
    }

    /// Remove every registry trace of a host that no longer exists.
    ///
    /// Series ids are opaque hashes of the key, so the only way to find a
    /// host's rows is to walk the registry. Historical buckets are left alone:
    /// they are keyed by id and age out of their tables on their own, and
    /// scanning every id's range to reclaim them is not worth the risk of a
    /// long write transaction on the live database.
    pub fn drop_host(&self, host: &str) -> Result<usize> {
        let prefix = format!("{host}|");
        let w = self.db.begin_write()?;
        let mut removed = 0usize;
        {
            let mut series = w.open_table(T_SERIES)?;
            let mut latest = w.open_table(T_LATEST)?;
            let mut meta = w.open_table(T_META)?;

            let mut ids = Vec::new();
            for item in series.iter()? {
                let (k, v) = item?;
                if v.value().starts_with(&prefix) {
                    ids.push(k.value());
                }
            }
            for id in ids {
                series.remove(id)?;
                latest.remove(id)?;
                removed += 1;
            }

            // Every meta row keyed by the node: data clock, liveness, address,
            // and the display label.
            for prefix in ["seen:", "up:", "ip:", "name:"] {
                meta.remove(format!("{prefix}{host}").as_str())?;
            }

            // Read into an owned String first: the guard borrows `meta`, and the
            // insert below needs it mutably.
            let hosts_json: Option<String> = {
                let guard = meta.get("hosts")?;
                guard.map(|v| v.value().to_string())
            };
            if let Some(json) = hosts_json {
                if let Ok(mut list) = serde_json::from_str::<Vec<String>>(&json) {
                    list.retain(|h| h != host);
                    let out = serde_json::to_string(&list)?;
                    meta.insert("hosts", out.as_str())?;
                }
            }
        }
        w.commit()?;
        Ok(removed)
    }

    /// Points from one specific table.
    pub fn points(
        &self,
        key: &str,
        from: i64,
        to: i64,
        res: Resolution,
    ) -> Result<Vec<(i64, f64)>> {
        let id = fnv1a64(key.as_bytes());
        let r = self.db.begin_read()?;
        let mut out = Vec::new();
        match res {
            Resolution::Raw => {
                let t = r.open_table(T_RAW)?;
                for item in t.range(pack(id, from)..pack(id, to))? {
                    let (k, v) = item?;
                    out.push((unpack_ts(k.value()), v.value()));
                }
            }
            Resolution::Hourly => {
                let t = r.open_table(T_HOURLY)?;
                for item in t.range(pack(id, from)..pack(id, to))? {
                    let (k, v) = item?;
                    out.push((unpack_ts(k.value()), v.value()));
                }
            }
            Resolution::Daily => {
                let t = r.open_table(T_DAILY)?;
                for item in t.range(pack(id, from)..pack(id, to))? {
                    let (k, v) = item?;
                    out.push((unpack_ts(k.value()), v.value()));
                }
            }
        }
        Ok(out)
    }

    /// Points for a chart, from the coarsest table that still shows the shape.
    ///
    /// Minute data only exists for the last two weeks, so asking for a year out
    /// of `raw` would return almost nothing; picking by span keeps the response
    /// small and honest.
    pub fn history(&self, key: &str, from: i64, to: i64) -> Result<Vec<(i64, f64)>> {
        let span = to - from;
        let res = if span <= 2 * 86400 {
            Resolution::Raw
        } else if span <= (HOURLY_RETENTION_DAYS - 5) * 86400 {
            Resolution::Hourly
        } else {
            Resolution::Daily
        };
        self.points(key, from, to, res)
    }
}
/// Wall-clock seconds. Used for liveness marks, which must not inherit an
/// agent's clock.
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
