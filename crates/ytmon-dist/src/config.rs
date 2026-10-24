//! Service configuration (`/etc/ytmon/dist.toml`).
//!
//! Every field has a default: an empty config file yields the standard
//! monitoring-host layout, so the service never fails to start over a typo.

use serde::Deserialize;

fn default_listen() -> String {
    "0.0.0.0:8088".to_string()
}

fn default_root() -> String {
    "/srv/ytmon-dist".to_string()
}

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default = "default_root")]
    pub root: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            root: default_root(),
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
}
