//! Active probes for targets that cannot run an agent (Windows VM, NAS with
//! SSH closed, services behind a port).
//!
//! ICMP goes through the system `ping` binary rather than a raw socket: on
//! Alpine the binary is always there, it needs no CAP_NET_RAW in the server,
//! and the failure mode (no ping binary) is visible rather than silent.

use crate::store::Store;
use serde::Deserialize;
use std::time::{Duration, Instant};
use ytmon_core::{Batch, Sample};

fn default_timeout() -> u64 {
    3
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Icmp,
    Tcp,
    Http,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Icmp => "icmp",
            Kind::Tcp => "tcp",
            Kind::Http => "http",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProbeCfg {
    /// Label for this probe. Used as the series host only when `node_id` is
    /// absent, so a probe aimed at a guest can share that guest's identity.
    pub name: String,
    /// Series identity, e.g. `vm:100`. Without it the probe gets its own row in
    /// the dashboard, which is how probe data ended up under a stale name after
    /// the identity migration.
    #[serde(default)]
    pub node_id: Option<String>,
    pub kind: Kind,
    /// host:port or bare host for tcp/icmp
    #[serde(default)]
    pub addr: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// For http: status codes outside 200..300 are still "up" unless this is set.
    #[serde(default)]
    pub expect_status: Option<u16>,
}

pub struct Outcome {
    pub up: bool,
    pub latency_ms: f64,
    pub status: Option<u16>,
    pub detail: String,
}

pub async fn run(store: Store, probes: Vec<ProbeCfg>, interval_secs: u64) {
    if probes.is_empty() {
        tracing::info!("no probes configured");
        return;
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        // The reverse proxy and the PVE API both present self-signed certs.
        .danger_accept_invalid_certs(true)
        .build()
        .expect("http client");

    let mut tick = tokio::time::interval(Duration::from_secs(interval_secs.max(10)));
    loop {
        tick.tick().await;
        let ts = now();
        for p in &probes {
            let o = execute(&client, p).await;
            let kind = p.kind.as_str();
            let target = p.node_id.clone().unwrap_or_else(|| p.name.clone());
            let mut b = Batch::new(target, None, 0);
            b.push(Sample::new("probe.up", ts, if o.up { 1.0 } else { 0.0 }).label("kind", kind));
            b.push(Sample::new("probe.latency_ms", ts, o.latency_ms).label("kind", kind));
            if let Some(s) = o.status {
                b.push(Sample::new("probe.http_status", ts, f64::from(s)).label("kind", kind));
            }
            if !o.up {
                tracing::warn!(probe = %p.name, kind, detail = %o.detail, "probe down");
            }
            if let Err(e) = store.ingest(&b) {
                tracing::error!(probe = %p.name, error = %e, "probe ingest failed");
            }
        }
    }
}

async fn execute(client: &reqwest::Client, p: &ProbeCfg) -> Outcome {
    match p.kind {
        Kind::Icmp => icmp(p).await,
        Kind::Tcp => tcp(p).await,
        Kind::Http => http(client, p).await,
    }
}

async fn icmp(p: &ProbeCfg) -> Outcome {
    let Some(host) = p.addr.as_deref() else {
        return down(0.0, "missing addr");
    };
    let start = Instant::now();
    let out = tokio::process::Command::new("ping")
        .args(["-c", "1", "-W", &p.timeout_secs.to_string(), host])
        .output()
        .await;
    let elapsed = start.elapsed().as_secs_f64() * 1000.0;

    match out {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            let ms = parse_ping_rtt(&text).unwrap_or(elapsed);
            Outcome {
                up: true,
                latency_ms: ms,
                status: None,
                detail: "ok".into(),
            }
        }
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr)
                .lines()
                .next()
                .unwrap_or("")
                .to_string();
            down(
                elapsed,
                if err.is_empty() {
                    "no reply".to_string()
                } else {
                    err
                },
            )
        }
        Err(e) => down(elapsed, format!("ping unavailable: {e}")),
    }
}

/// busybox prints `seq=0 ttl=64 time=0.412 ms`; iputils prints
/// `time=0.412 ms` too, so both are covered.
fn parse_ping_rtt(text: &str) -> Option<f64> {
    for line in text.lines() {
        if let Some(idx) = line.find("time=") {
            let rest = &line[idx + 5..];
            let num: String = rest
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            if let Ok(v) = num.parse::<f64>() {
                return Some(v);
            }
        }
    }
    None
}

async fn tcp(p: &ProbeCfg) -> Outcome {
    let Some(addr) = p.addr.as_deref() else {
        return down(0.0, "missing addr");
    };
    let start = Instant::now();
    let fut = tokio::net::TcpStream::connect(addr);
    match tokio::time::timeout(Duration::from_secs(p.timeout_secs.max(1)), fut).await {
        Ok(Ok(_)) => Outcome {
            up: true,
            latency_ms: start.elapsed().as_secs_f64() * 1000.0,
            status: None,
            detail: "connected".into(),
        },
        Ok(Err(e)) => down(start.elapsed().as_secs_f64() * 1000.0, e.to_string()),
        Err(_) => down(
            p.timeout_secs as f64 * 1000.0,
            format!("timeout {}s", p.timeout_secs),
        ),
    }
}

async fn http(client: &reqwest::Client, p: &ProbeCfg) -> Outcome {
    let Some(url) = p.url.as_deref() else {
        return down(0.0, "missing url");
    };
    let start = Instant::now();
    match client.get(url).send().await {
        Ok(resp) => {
            let code = resp.status().as_u16();
            let up = match p.expect_status {
                Some(want) => code == want,
                None => (200..400).contains(&code),
            };
            Outcome {
                up,
                latency_ms: start.elapsed().as_secs_f64() * 1000.0,
                status: Some(code),
                detail: format!("HTTP {code}"),
            }
        }
        Err(e) => down(start.elapsed().as_secs_f64() * 1000.0, e.to_string()),
    }
}

fn down(latency_ms: f64, detail: impl Into<String>) -> Outcome {
    Outcome {
        up: false,
        latency_ms,
        status: None,
        detail: detail.into(),
    }
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
