//! Shared protocol and types for ytmon.
//!
//! The wire format is deliberately boring: one JSON object per HTTP POST,
//! gzip-compressed. Counter metrics are reported raw (monotonic totals) and
//! differenced server-side, so a restarted agent can never emit a bogus rate.

use serde::{Deserialize, Serialize};

/// Bumped when the wire format changes incompatibly.
pub const PROTO_VERSION: u32 = 1;

/// One metric observation. `ts` is unix seconds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Sample {
    pub metric: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<(String, String)>,
    pub ts: i64,
    pub value: f64,
}

impl Sample {
    pub fn new(metric: impl Into<String>, ts: i64, value: f64) -> Self {
        Self {
            metric: metric.into(),
            labels: Vec::new(),
            ts,
            value,
        }
    }

    pub fn label(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.labels.push((k.into(), v.into()));
        self
    }

    /// Stable identity of the time series this sample belongs to.
    pub fn series_key(&self, host: &str) -> String {
        series_key(host, &self.metric, &self.labels)
    }
}

/// A batch of samples from one agent, sent as a single POST body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Batch {
    pub v: u32,
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctid: Option<u32>,
    /// Address the agent sees for itself, used in alert cards so a human
    /// reading Telegram knows which box to ssh into without a lookup table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
    /// Monotonic per-agent counter, lets the server spot gaps and replays.
    pub agent_seq: u64,
    pub samples: Vec<Sample>,
}

impl Batch {
    pub fn new(host: impl Into<String>, ctid: Option<u32>, agent_seq: u64) -> Self {
        Self {
            v: PROTO_VERSION,
            host: host.into(),
            ctid,
            ip: None,
            agent_seq,
            samples: Vec::new(),
        }
    }

    pub fn push(&mut self, s: Sample) {
        self.samples.push(s);
    }
}

/// Canonical string identity for a series: `host|metric|k1=v1,k2=v2` with
/// labels sorted, so label order in the request never splits a series.
pub fn series_key(host: &str, metric: &str, labels: &[(String, String)]) -> String {
    let mut pairs: Vec<(&str, &str)> = labels
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    pairs.sort_unstable();
    let mut out = String::with_capacity(host.len() + metric.len() + 24);
    out.push_str(host);
    out.push('|');
    out.push_str(metric);
    for (k, v) in pairs {
        out.push('|');
        out.push_str(k);
        out.push('=');
        out.push_str(v);
    }
    out
}

/// FNV-1a 64. Used to turn a series key into a compact stable id.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

pub fn series_id(host: &str, metric: &str, labels: &[(String, String)]) -> u64 {
    fnv1a64(series_key(host, metric, labels).as_bytes())
}

/// Counter reset detection: a decrease means the guest counter restarted
/// (CT reboot, cgroup recreation) and the delta must be treated as unknown.
pub fn counter_delta(prev: f64, cur: f64) -> Option<f64> {
    if cur >= prev {
        Some(cur - prev)
    } else {
        None
    }
}
