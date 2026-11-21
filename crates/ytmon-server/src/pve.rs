//! PVE read-only discovery: every CT and VM the node knows about, whether or
//! not it can run an agent. This is what makes "all CT and VM" true rather
//! than "all the ones we installed something on".
//!
//! Uses a PVEAuditor token — GET only. If the token ever gains write rights
//! that is a PVE-side mistake, not something this code does.

use crate::probe::now;
use crate::store::Store;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use ytmon_core::{Batch, Sample};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Guest {
    pub vmid: u32,
    pub name: String,
    /// "lxc" or "qemu"
    pub kind: String,
    pub status: String,
    pub maxmem: u64,
    pub maxdisk: u64,
    pub uptime: u64,
}

/// Series identity for a discovered guest.
pub fn identity(g: &Guest) -> String {
    match g.kind.as_str() {
        "lxc" => format!("ct:{}", g.vmid),
        _ => format!("vm:{}", g.vmid),
    }
}

pub struct Cfg {
    /// e.g. https://pve.example.com:8006
    pub base: String,
    /// e.g. pve
    pub node: String,
    /// root@pam!tokenid=uuid
    pub token: String,
    pub interval_secs: u64,
    /// Seconds of confirmed absence before a vanished guest is dropped; 0
    /// disables pruning. A switched-off guest is still listed by PVE and is
    /// never pruned, so this only fires after an operator deletes it.
    pub prune_absent_secs: u64,
    /// Identities referenced by probes: those keep their row even when the
    /// guest is gone, otherwise the probe would recreate it every minute.
    pub protected: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Resp<T> {
    data: T,
}

#[derive(Debug, Deserialize)]
struct Entry {
    vmid: u32,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    maxmem: Option<u64>,
    #[serde(default)]
    maxdisk: Option<u64>,
    #[serde(default)]
    uptime: Option<u64>,
}

pub fn load_token(path: &str) -> anyhow::Result<String> {
    let raw = std::fs::read_to_string(path)?;
    Ok(raw.trim().to_string())
}

/// One RRD sample: `netin`/`netout` are byte-per-second averages.
#[derive(Debug, Deserialize)]
struct RrdPoint {
    time: i64,
    #[serde(default)]
    netin: Option<f64>,
    #[serde(default)]
    netout: Option<f64>,
}

/// Traffic a single guest moved during a period.
#[derive(Debug, Clone, Serialize)]
pub struct GuestTraffic {
    pub vmid: u32,
    pub name: String,
    pub kind: String,
    /// Bytes delivered *to* the guest — downstream from the household's view.
    pub rx: f64,
    /// Bytes the guest sent out — upstream.
    pub tx: f64,
}

/// Per-guest traffic for a period.
///
/// The household total is *not* derived here: PVE's node RRD sums every
/// non-loopback interface, and it sums wan+lan on this host (measured:
/// netin 1047.9 GB == wan 201 GB + lan 846.7 GB), so the LAN side would swamp
/// the number. The uplink is measured by the agent running on the hypervisor,
/// which reports `wan` and `lan` as separate series.
pub struct PeriodTraffic {
    pub guests: Vec<GuestTraffic>,
}

fn client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(30))
        .build()?)
}

async fn rrd(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    path: &str,
) -> anyhow::Result<Vec<RrdPoint>> {
    let url = format!(
        "{}/api2/json{path}/rrddata?timeframe=year&cf=AVERAGE",
        base.trim_end_matches('/')
    );
    let resp = client
        .get(&url)
        .header("Authorization", format!("PVEAPIToken={token}"))
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("{path} -> {status}");
    }
    let body: Resp<Vec<RrdPoint>> = resp.json().await?;
    Ok(body.data)
}

/// Integrate RRD rates over `[from, to)`.
///
/// Each sample is the average rate since the previous one, so the value is
/// multiplied by the interval it covers rather than a fixed step: PVE uses a
/// 6-hour step on the year view, and a guest that was off contributes holes
/// that must not be counted as zero-rate intervals.
fn integrate(points: &[RrdPoint], from: i64, to: i64, out: bool) -> f64 {
    let mut total = 0.0;
    for w in points.windows(2) {
        let (p, next) = (&w[0], &w[1]);
        let Some(rate) = (if out { p.netout } else { p.netin }) else {
            continue;
        };
        let start = p.time.max(from);
        let end = next.time.min(to);
        if end > start {
            total += rate * (end - start) as f64;
        }
    }
    total
}

/// Pull one month of traffic for every guest, plus the node uplink.
pub async fn period_traffic(
    cfg: &Cfg,
    guests: &[Guest],
    from: i64,
    to: i64,
) -> anyhow::Result<PeriodTraffic> {
    let client = client()?;
    let base = cfg.base.trim_end_matches('/');

    let mut out = Vec::new();
    for g in guests {
        let path = format!("/nodes/{}/{}/{}", cfg.node, g.kind, g.vmid);
        match rrd(&client, base, &cfg.token, &path).await {
            Ok(points) => out.push(GuestTraffic {
                vmid: g.vmid,
                name: g.name.clone(),
                kind: g.kind.clone(),
                rx: integrate(&points, from, to, false),
                tx: integrate(&points, from, to, true),
            }),
            Err(e) => tracing::warn!(vmid = g.vmid, name = %g.name, error = %e, "rrd fetch failed"),
        }
    }
    out.sort_by(|a, b| {
        (b.rx + b.tx)
            .partial_cmp(&(a.rx + a.tx))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(PeriodTraffic { guests: out })
}

pub async fn run(store: Store, cfg: Cfg, cache: Arc<RwLock<Vec<Guest>>>) {
    let client = match reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "pve client build failed");
            return;
        }
    };

    let mut tick = tokio::time::interval(Duration::from_secs(cfg.interval_secs.max(30)));
    loop {
        tick.tick().await;
        match fetch(&client, &cfg).await {
            Ok(guests) => {
                let ts = now();
                let mut failed = 0usize;
                for g in &guests {
                    // Same identity the agents use: a PVE-side rename must not
                    // fork the series, and the guest name is the authority for
                    // the label (it is what the operator sees in the PVE UI).
                    let node_id = identity(g);
                    let name_key = format!("name:{node_id}");
                    let current = store.get_meta(&name_key).ok().flatten();
                    if current.as_deref() != Some(g.name.as_str()) {
                        if let Err(e) = store.set_name(&node_id, &g.name) {
                            tracing::warn!(guest = %g.name, error = %e, "pve name update failed");
                        }
                    }
                    let mut b = Batch::new(node_id, Some(g.vmid), 0);
                    b.push(
                        Sample::new(
                            "pve.status",
                            ts,
                            if g.status == "running" { 1.0 } else { 0.0 },
                        )
                        .label("vmtype", g.kind.clone()),
                    );
                    if g.maxmem > 0 {
                        b.push(Sample::new("pve.maxmem_bytes", ts, g.maxmem as f64));
                    }
                    if g.maxdisk > 0 {
                        b.push(Sample::new("pve.maxdisk_bytes", ts, g.maxdisk as f64));
                    }
                    b.push(
                        Sample::new("pve.uptime_seconds", ts, g.uptime as f64)
                            .label("vmtype", g.kind.clone()),
                    );
                    if let Err(e) = store.ingest_passive(&b) {
                        failed += 1;
                        tracing::warn!(guest = %g.name, error = %e, "pve ingest failed");
                    }
                }
                tracing::info!(guests = guests.len(), failed, "pve discovery ok");
                reap(&store, &guests, &cfg);
                *cache.write().await = guests;
            }
            Err(e) => tracing::warn!(error = %e, "pve discovery failed"),
        }
    }
}

async fn fetch(client: &reqwest::Client, cfg: &Cfg) -> anyhow::Result<Vec<Guest>> {
    let mut out = Vec::new();
    for (path, kind) in [("lxc", "lxc"), ("qemu", "qemu")] {
        let url = format!(
            "{}/api2/json/nodes/{}/{}",
            cfg.base.trim_end_matches('/'),
            cfg.node,
            path
        );
        let resp = client
            .get(&url)
            .header("Authorization", format!("PVEAPIToken={}", cfg.token))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("{url} -> {status}");
        }
        let body: Resp<Vec<Entry>> = resp.json().await?;
        for e in body.data {
            out.push(Guest {
                vmid: e.vmid,
                name: e.name.unwrap_or_else(|| format!("{kind}-{}", e.vmid)),
                kind: kind.to_string(),
                status: e.status.unwrap_or_else(|| "unknown".into()),
                maxmem: e.maxmem.unwrap_or(0),
                maxdisk: e.maxdisk.unwrap_or(0),
                uptime: e.uptime.unwrap_or(0),
            });
        }
    }
    out.sort_by_key(|g| g.vmid);
    Ok(out)
}

/// Resolve a guest by name: a single running match wins, then a single match
/// when the guest is off. Two same-named `running` guests are ambiguous — the
/// retired-number duplicates that still sit in PVE are all stopped, so the
/// running guest is the live one.
pub fn resolve_name(guests: &[Guest], name: &str) -> Option<String> {
    let matches: Vec<&Guest> = guests
        .iter()
        .filter(|g| g.name.eq_ignore_ascii_case(name))
        .collect();
    let running: Vec<&&Guest> = matches.iter().filter(|g| g.status == "running").collect();
    match running.len() {
        1 => Some(identity(running[0])),
        0 if matches.len() == 1 => Some(identity(matches[0])),
        _ => None,
    }
}

/// Decide which series identity an agent's batch belongs to.
///
/// The token is the trust anchor and its configured `name` is the join key; the
/// ctid the agent reports only disambiguates. A reported id is accepted only
/// when that guest is running and carries the token's name, so a stale ctid
/// from before a renumbering (it points at a stopped retired twin) falls
/// through to the name lookup instead of writing into the old container.
pub fn identify(
    guests: &[Guest],
    name: &str,
    explicit: Option<&str>,
    reported_ctid: Option<u32>,
) -> Result<(String, &'static str), String> {
    if let Some(id) = explicit.filter(|s| !s.is_empty()) {
        return Ok((id.to_string(), "config"));
    }
    if let Some(vmid) = reported_ctid {
        if let Some(g) = guests.iter().find(|g| g.vmid == vmid) {
            if g.status == "running" && g.name.eq_ignore_ascii_case(name) {
                return Ok((identity(g), "ctid"));
            }
        }
    }
    match resolve_name(guests, name) {
        Some(id) => Ok((id, "name")),
        None => Err(format!(
            "token name {name} matches no unique PVE guest (reported ctid {reported_ctid:?})"
        )),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Absence {
    /// The agent reported inside the grace window; keep the data and reset.
    Alive,
    /// First confirmed miss: start the clock.
    Starts,
    /// Still inside the grace window.
    Waits,
    /// Gone long enough to drop.
    Drops,
}

fn absence_action(since: Option<i64>, now: i64, grace: i64, last_up: Option<i64>) -> Absence {
    if let Some(up) = last_up {
        if up + grace > now {
            return Absence::Alive;
        }
    }
    match since {
        None => Absence::Starts,
        Some(t) if now - t >= grace => Absence::Drops,
        Some(_) => Absence::Waits,
    }
}

/// Follow PVE deletions: a guest that stops appearing in a complete discovery
/// is dropped from the store after the grace window. Only `ct:`/`vm:` rows are
/// considered — `host:pve` and probe-only rows have no PVE guest to miss.
fn reap(store: &Store, guests: &[Guest], cfg: &Cfg) {
    if cfg.prune_absent_secs == 0 || guests.is_empty() {
        // An empty list is a broken API/permission result, not "every guest
        // was deleted"; never mass-prune on it.
        return;
    }
    let live: std::collections::HashSet<String> = guests.iter().map(identity).collect();
    let now = now();
    let grace = cfg.prune_absent_secs as i64;
    let Ok(hosts) = store.hosts() else {
        return;
    };
    for host in hosts {
        if !(host.starts_with("ct:") || host.starts_with("vm:")) {
            continue;
        }
        if cfg.protected.iter().any(|p| p == &host) {
            continue;
        }
        let absent_key = format!("pve:absent:{host}");
        if live.contains(&host) {
            if store.get_meta(&absent_key).ok().flatten().is_some() {
                let _ = store.remove_meta(&absent_key);
            }
            continue;
        }
        let since = store
            .get_meta(&absent_key)
            .ok()
            .flatten()
            .and_then(|s| s.parse::<i64>().ok());
        let last_up = store
            .get_meta(&format!("up:{host}"))
            .ok()
            .flatten()
            .and_then(|s| s.parse::<i64>().ok());
        match absence_action(since, now, grace, last_up) {
            Absence::Alive => {
                let _ = store.remove_meta(&absent_key);
            }
            Absence::Starts => {
                let _ = store.set_meta(&absent_key, &now.to_string());
                tracing::info!(
                    host = %host,
                    grace_secs = cfg.prune_absent_secs,
                    "pve guest missing, grace timer started"
                );
            }
            Absence::Waits => {}
            Absence::Drops => {
                let name = store
                    .get_meta(&format!("name:{host}"))
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                match store.drop_host(&host) {
                    Ok(series) => {
                        tracing::info!(host = %host, name = %name, series, "pve guest gone, dropped")
                    }
                    Err(e) => tracing::warn!(host = %host, error = %e, "pve guest drop failed"),
                }
                let _ = store.remove_meta(&absent_key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guest(vmid: u32, name: &str, kind: &str, status: &str) -> Guest {
        Guest {
            vmid,
            name: name.into(),
            kind: kind.into(),
            status: status.into(),
            maxmem: 0,
            maxdisk: 0,
            uptime: 0,
        }
    }

    fn fleet() -> Vec<Guest> {
        vec![
            guest(413, "YT-Monitor", "lxc", "stopped"),
            guest(1166, "YT-Monitor", "lxc", "running"),
            guest(200, "YT-FnOS", "qemu", "stopped"),
            guest(1233, "YT-FnOS", "qemu", "running"),
            guest(1144, "YT-WIN10", "qemu", "stopped"),
        ]
    }

    #[test]
    fn reported_ctid_wins_when_running_and_named() {
        let (id, source) = identify(&fleet(), "YT-Monitor", None, Some(1166)).unwrap();
        assert_eq!(id, "ct:1166");
        assert_eq!(source, "ctid");
    }

    #[test]
    fn stale_ctid_falls_through_to_the_running_twin() {
        // What every pre-0.1.17 agent still reports after the renumbering.
        let (id, source) = identify(&fleet(), "YT-Monitor", None, Some(413)).unwrap();
        assert_eq!(id, "ct:1166");
        assert_eq!(source, "name");
    }

    #[test]
    fn a_ctid_with_another_name_is_ignored() {
        let (id, source) = identify(&fleet(), "YT-Monitor", None, Some(1233)).unwrap();
        assert_eq!(id, "ct:1166");
        assert_eq!(source, "name");
    }

    #[test]
    fn no_ctid_resolves_by_unique_name_including_a_stopped_vm() {
        let (id, _) = identify(&fleet(), "YT-FnOS", None, None).unwrap();
        assert_eq!(id, "vm:1233");
        let (id, _) = identify(&fleet(), "YT-WIN10", None, None).unwrap();
        assert_eq!(id, "vm:1144");
    }

    #[test]
    fn explicit_node_id_is_the_escape_hatch() {
        let (id, source) = identify(&fleet(), "pve", Some("host:pve"), None).unwrap();
        assert_eq!(id, "host:pve");
        assert_eq!(source, "config");
    }

    #[test]
    fn duplicate_running_names_are_ambiguous() {
        let mut f = fleet();
        f.push(guest(2166, "YT-Monitor", "lxc", "running"));
        assert!(identify(&f, "YT-Monitor", None, None).is_err());
    }

    #[test]
    fn absence_needs_the_grace_window_and_no_recent_report() {
        let now = 1_000;
        assert_eq!(absence_action(None, now, 600, None), Absence::Starts);
        assert_eq!(absence_action(Some(500), now, 600, None), Absence::Waits);
        assert_eq!(absence_action(Some(300), now, 600, None), Absence::Drops);
        // An agent that reported 10 minutes ago keeps its history.
        assert_eq!(
            absence_action(Some(300), now, 600, Some(900)),
            Absence::Alive
        );
    }

    #[test]
    fn reap_drops_a_guest_after_the_grace_window() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("ytmon-reap-{stamp}.db"));
        let store = Store::open(path.to_str().unwrap()).unwrap();

        let mut b = Batch::new("ct:413", Some(413), 1);
        b.push(ytmon_core::Sample::new("pve.status", 1_000, 1.0));
        store.ingest_passive(&b).unwrap();
        store.set_name("ct:413", "YT-Monitor").unwrap();

        let cfg = Cfg {
            base: String::new(),
            node: String::new(),
            token: String::new(),
            interval_secs: 300,
            prune_absent_secs: 600,
            protected: Vec::new(),
        };
        let guests = vec![guest(1166, "YT-Monitor", "lxc", "running")];

        // First complete discovery without 413: start the clock, keep the data.
        reap(&store, &guests, &cfg);
        assert!(store.get_meta("pve:absent:ct:413").unwrap().is_some());
        assert!(store.hosts().unwrap().contains(&"ct:413".to_string()));

        // Pretend the grace window elapsed; the next discovery drops it.
        store
            .set_meta("pve:absent:ct:413", &(now() - 601).to_string())
            .unwrap();
        reap(&store, &guests, &cfg);
        assert!(!store.hosts().unwrap().contains(&"ct:413".to_string()));

        drop(store);
        let _ = std::fs::remove_file(&path);
    }
}
