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

pub struct Cfg {
    /// e.g. https://pve.example.com:8006
    pub base: String,
    /// e.g. pve
    pub node: String,
    /// root@pam!tokenid=uuid
    pub token: String,
    pub interval_secs: u64,
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
                    let node_id = match g.kind.as_str() {
                        "lxc" => format!("ct:{}", g.vmid),
                        _ => format!("vm:{}", g.vmid),
                    };
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
                    if let Err(e) = store.ingest(&b) {
                        failed += 1;
                        tracing::warn!(guest = %g.name, error = %e, "pve ingest failed");
                    }
                }
                tracing::info!(guests = guests.len(), failed, "pve discovery ok");
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
