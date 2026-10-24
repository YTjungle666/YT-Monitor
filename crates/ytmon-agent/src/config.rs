//! Agent configuration (`/etc/ytmon/agent.toml`).

use serde::Deserialize;

fn default_server() -> String {
    "http://127.0.0.1:9180/ingest".to_string()
}
fn default_interval() -> u64 {
    60
}
fn default_mounts() -> Vec<String> {
    vec!["/".to_string()]
}

#[derive(Debug, Deserialize)]
pub struct Config {
    /// Override the reported host name (defaults to /etc/hostname).
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub ctid: Option<u32>,
    #[serde(default = "default_server")]
    pub server: String,
    #[serde(default = "default_interval")]
    pub interval_secs: u64,
    #[serde(default = "default_mounts")]
    pub mounts: Vec<String>,
    /// Local spool directory used when the server is unreachable.
    #[serde(default = "default_state_dir")]
    pub state_dir: String,
    /// Bearer token for the ingest endpoint. Empty means the server does not
    /// ask for one — the field exists so enabling auth is a config change on
    /// both sides, never a rebuild.
    #[serde(default)]
    pub server_token: Option<String>,
}

fn default_state_dir() -> String {
    "/var/lib/ytmon".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: None,
            ctid: None,
            server: default_server(),
            interval_secs: default_interval(),
            mounts: default_mounts(),
            state_dir: default_state_dir(),
            server_token: None,
        }
    }
}

impl Config {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(toml::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn resolved_host(&self) -> String {
        if let Some(h) = &self.host {
            return h.clone();
        }
        std::fs::read_to_string("/etc/hostname")
            .map(|s| s.trim().to_string())
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown".to_string())
    }
}
