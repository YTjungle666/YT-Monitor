//! Server configuration (`/etc/ytmon/server.toml`).
//!
//! Every field is optional: a missing file yields a server that only ingests
//! and serves, with discovery and probes disabled. That keeps "empty config"
//! a valid, safe state rather than a startup failure.

use crate::probe::ProbeCfg;
use crate::pve;
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize)]
pub struct AgentToken {
    /// Stable identity: `ct:400`, `vm:100`, `host:pve`.
    pub node_id: String,
    /// Human label, refreshed from whatever the agent reports.
    pub name: String,
    pub token: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct FileCfg {
    pub listen: Option<String>,
    pub db: Option<String>,
    /// e.g. https://pve.example.com:8006
    pub pve_base: Option<String>,
    /// e.g. pve
    pub pve_node: Option<String>,
    pub pve_token_file: Option<String>,
    pub pve_interval_secs: Option<u64>,
    pub probe_interval_secs: Option<u64>,
    /// How often rules are evaluated, seconds (default 30).
    pub alert_interval_secs: Option<u64>,
    /// Alert rules file (default /etc/ytmon/rules.toml).
    pub alert_rules_file: Option<String>,
    /// File holding the bot token, one line, mode 600.
    pub telegram_token_file: Option<String>,
    /// Target chat id (private chat or group).
    pub telegram_chat_id: Option<String>,
    /// Local hour (0-23) on the 1st when the monthly traffic report goes out.
    pub monthly_report_hour: Option<u32>,
    /// Set false to switch the monthly report off without touching Telegram settings.
    pub monthly_report_enabled: Option<bool>,
    /// Hosts omitted from the monthly card. NAS lives here by default: its
    /// traffic is mostly LAN backups and says nothing about internet usage.
    pub report_exclude_hosts: Option<Vec<String>>,
    /// Second bind address for the same router, e.g. "0.0.0.0:80" so the
    /// dashboard is reachable without a port while ingest keeps its own.
    pub extra_listen: Option<String>,
    /// One entry per agent. The token is the identity: a rename only changes
    /// `name`, history and alerts stay attached, and revoking one machine means
    /// removing one entry rather than rotating a fleet-wide secret.
    pub agents: Option<Vec<AgentToken>>,
    /// Forget hosts that have not reported for this many days. 0 (the default)
    /// keeps them forever: a guest that is merely switched off should not lose
    /// its history, and a renamed agent is indistinguishable from one that was
    /// turned off until the operator says how long is long enough.
    pub stale_host_days: Option<i64>,
    /// Run hourly/daily downsampling and retention. Off means raw grows without
    /// bound, so this defaults to on.
    pub retention_enabled: Option<bool>,
    /// Count what would be pruned without deleting it.
    pub retention_dry_run: Option<bool>,
    pub retention_interval_secs: Option<u64>,
    /// Host that measures the household uplink (the hypervisor, running the
    /// agent) and the interface facing the optical modem.
    pub report_uplink_host: Option<String>,
    pub report_uplink_iface: Option<String>,
    #[serde(default)]
    pub probes: Vec<ProbeCfg>,
}

impl FileCfg {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(toml::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn listen_or(&self, fallback: &str) -> String {
        self.listen.clone().unwrap_or_else(|| fallback.to_string())
    }

    pub fn db_or(&self, fallback: &str) -> String {
        self.db.clone().unwrap_or_else(|| fallback.to_string())
    }

    /// Build the PVE discovery config, or `None` when no token file is set or
    /// the file is empty/unreadable (logged, not fatal: monitoring the guests
    /// is more important than discovering them).
    pub fn pve_cfg(&self) -> Option<pve::Cfg> {
        let path = self.pve_token_file.as_ref()?;
        match pve::load_token(path) {
            Ok(token) if !token.is_empty() => Some(pve::Cfg {
                base: self
                    .pve_base
                    .clone()
                    .unwrap_or_else(|| "https://pve.example.com:8006".to_string()),
                node: self.pve_node.clone().unwrap_or_else(|| "pve".to_string()),
                token,
                interval_secs: self.pve_interval_secs.unwrap_or(300),
            }),
            Ok(_) => {
                tracing::warn!(path = %path, "pve token file empty, discovery disabled");
                None
            }
            Err(e) => {
                tracing::warn!(path = %path, error = %e, "pve token unreadable, discovery disabled");
                None
            }
        }
    }

    pub fn probe_interval(&self) -> u64 {
        self.probe_interval_secs.unwrap_or(60)
    }

    /// The monthly report reuses the alert bot but is a separate worker: it
    /// must keep running even if the rules file is missing.
    pub fn monthly_report(&self) -> Option<(crate::alert::telegram::Telegram, crate::report::Cfg)> {
        if !self.monthly_report_enabled.unwrap_or(true) {
            return None;
        }
        let token_file = self.telegram_token_file.clone()?;
        let chat_id = self.telegram_chat_id.clone()?;
        match crate::alert::telegram::Telegram::from_file(&token_file, &chat_id) {
            Ok(t) => Some((t, self.report_cfg())),
            Err(e) => {
                tracing::warn!(error = %e, "telegram unavailable; monthly report disabled");
                None
            }
        }
    }

    pub fn retention_cfg(&self) -> Option<crate::retention::Cfg> {
        if !self.retention_enabled.unwrap_or(true) {
            return None;
        }
        Some(crate::retention::Cfg {
            interval_secs: self.retention_interval_secs.unwrap_or(900),
            prune_dry_run: self.retention_dry_run.unwrap_or(false),
            stale_host_days: self.stale_host_days.unwrap_or(0),
        })
    }

    /// `node_id -> human name`, from the token table and the probe list.
    ///
    /// Series keys and alert state stay on the stable identity (`ct:403`), but
    /// every card a person reads addresses the machine by its name
    /// (`YT-Board`). A name the agent reports itself lives in the store and
    /// takes precedence over this: the agent knows its own hostname, the config
    /// only knows what the machine was called when it was enrolled.
    pub fn label_map(&self) -> HashMap<String, String> {
        let mut m = HashMap::new();
        for a in self.agents.iter().flatten() {
            m.insert(a.node_id.clone(), a.name.clone());
        }
        for p in &self.probes {
            if let Some(id) = &p.node_id {
                m.insert(id.clone(), p.name.clone());
            }
        }
        m
    }

    /// Report settings, usable without Telegram (preview/back-fill endpoints).
    pub fn report_cfg(&self) -> crate::report::Cfg {
        crate::report::Cfg {
            hour: self.monthly_report_hour.unwrap_or(9),
            exclude: self.report_exclude_hosts.clone().unwrap_or_default(),
            pve: self.pve_cfg(),
            uplink_host: self.report_uplink_host.clone(),
            uplink_iface: self
                .report_uplink_iface
                .clone()
                .unwrap_or_else(|| "wan".to_string()),
            labels: self.label_map(),
        }
    }

    /// Alerts need a bot token and a chat id; either missing means the
    /// evaluator is not started at all, so an unconfigured server is silent
    /// rather than logging a warning every 30 seconds.
    pub fn alert_cfg(&self) -> Option<crate::alert::Cfg> {
        let token_file = self.telegram_token_file.clone()?;
        let chat_id = self.telegram_chat_id.clone()?;
        if !std::path::Path::new(&token_file).exists() {
            tracing::warn!(path = %token_file, "telegram token file missing, alerts disabled");
            return None;
        }
        Some(crate::alert::Cfg {
            interval_secs: self.alert_interval_secs.unwrap_or(30),
            rules_file: self
                .alert_rules_file
                .clone()
                .unwrap_or_else(|| "/etc/ytmon/rules.toml".to_string()),
            token_file,
            chat_id,
            labels: self.label_map(),
        })
    }
}
