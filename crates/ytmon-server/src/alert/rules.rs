//! Alert rules (`rules.toml`).
//!
//! Two kinds, deliberately only two:
//!
//!   threshold — a metric crosses a number (cpu, memory, disk, probe.up, ...)
//!   stale     — a host stopped reporting samples
//!
//! Anything more expressive (rate(), avg_over_time(), multi-condition joins)
//! belongs in a query language we do not have, and every alert that needs it
//! is usually better served by exporting a derived metric from the agent.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    Gt,
    Lt,
}

/// Severity, ordered so `Warning < Critical` and comparisons work directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Ok,
    Warning,
    Critical,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Warning => "warning",
            Level::Critical => "critical",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Threshold,
    Stale,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Rule {
    pub name: String,
    #[serde(default = "kind_threshold")]
    pub kind: Kind,
    /// Metric name for threshold rules, e.g. `cpu.usage_pct`.
    #[serde(default)]
    pub metric: String,
    /// Optional denominator series, same host. When set, the rule compares
    /// `metric / denominator * 100` instead of the raw value — this is how
    /// "memory above 80%" is expressed for containers that have no cgroup
    /// memory limit (`memory.max = max`), where PVE's configured maxmem is
    /// the only meaningful total.
    #[serde(default)]
    pub denominator_metric: Option<String>,
    /// Human title shown in notifications, e.g. "CPU 占用过高".
    #[serde(default)]
    pub title: Option<String>,
    /// `["*"]` or exact host names. Prefix wildcards (`web-*`) also work.
    #[serde(default = "all_hosts")]
    pub hosts: Vec<String>,
    #[serde(default = "op_gt")]
    pub op: Op,
    #[serde(default)]
    pub warning: Option<f64>,
    #[serde(default)]
    pub critical: Option<f64>,
    /// How long the condition must hold before it fires. For `stale` rules
    /// this is the age at which a host counts as gone.
    #[serde(default = "default_for")]
    pub for_secs: u64,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Shown verbatim in notifications (`%`, `MB`, `ms`, ...).
    #[serde(default)]
    pub unit: Option<String>,
}

fn kind_threshold() -> Kind {
    Kind::Threshold
}
fn all_hosts() -> Vec<String> {
    vec!["*".to_string()]
}
fn op_gt() -> Op {
    Op::Gt
}
fn default_for() -> u64 {
    300
}
fn yes() -> bool {
    true
}

impl Rule {
    /// Does this rule apply to `host`?
    pub fn matches_host(&self, host: &str) -> bool {
        self.hosts.iter().any(|p| {
            p == "*"
                || p == host
                || p.strip_suffix('*')
                    .is_some_and(|prefix| host.starts_with(prefix))
        })
    }

    /// Severity for a raw value. `Ok` means the condition is not met.
    pub fn severity(&self, value: f64) -> Level {
        match self.kind {
            Kind::Stale => Level::Ok, // handled by the caller, which knows age
            Kind::Threshold => {
                let hit = |t: Option<f64>| match (self.op, t) {
                    (Op::Gt, Some(t)) => value > t,
                    (Op::Lt, Some(t)) => value < t,
                    _ => false,
                };
                if hit(self.critical) {
                    Level::Critical
                } else if hit(self.warning) {
                    Level::Warning
                } else {
                    Level::Ok
                }
            }
        }
    }

    /// The threshold that produced `level` is intentionally not shown in
    /// notifications: the operator wants "current value", not the rule's
    /// internals.
    pub fn _threshold_of(&self, level: Level) -> Option<f64> {
        match level {
            Level::Critical => self.critical,
            Level::Warning => self.warning,
            Level::Ok => None,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct RulesFile {
    #[serde(default)]
    pub rules: Vec<Rule>,
}

impl RulesFile {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(toml::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule() -> Rule {
        Rule {
            name: "cpu".into(),
            kind: Kind::Threshold,
            metric: "cpu.usage_pct".into(),
            denominator_metric: None,
            title: Some("CPU 占用过高".into()),
            hosts: vec!["*".into()],
            op: Op::Gt,
            warning: Some(80.0),
            critical: Some(150.0),
            for_secs: 300,
            enabled: true,
            unit: Some("%".into()),
        }
    }

    #[test]
    fn severity_escalates() {
        let r = rule();
        assert_eq!(r.severity(10.0), Level::Ok);
        assert_eq!(r.severity(100.0), Level::Warning);
        assert_eq!(r.severity(200.0), Level::Critical);
    }

    #[test]
    fn lt_rule_inverts() {
        let mut r = rule();
        r.op = Op::Lt;
        r.warning = Some(1.0);
        r.critical = Some(0.5);
        assert_eq!(r.severity(1.0), Level::Ok);
        assert_eq!(r.severity(0.7), Level::Warning);
        assert_eq!(r.severity(0.1), Level::Critical);
    }

    #[test]
    fn host_matching() {
        let mut r = rule();
        assert!(r.matches_host("web-01"));
        r.hosts = vec!["web-*".into()];
        assert!(r.matches_host("web-02"));
        assert!(!r.matches_host("OTHER"));
        r.hosts = vec!["web-01".into()];
        assert!(!r.matches_host("web-01-2"));
    }
}
