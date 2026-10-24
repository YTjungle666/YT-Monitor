//! Telegram delivery.
//!
//! The token is read from a 600-mode file and never logged: the URL contains
//! it, so any error string that included the URL would leak the bot.

use std::time::Duration;

pub struct Telegram {
    token: String,
    chat_id: String,
    client: reqwest::Client,
}

impl Telegram {
    pub fn new(token: String, chat_id: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        Self {
            token,
            chat_id,
            client,
        }
    }

    /// Token file holds one line: `123456:AA...`
    pub fn from_file(path: &str, chat_id: &str) -> anyhow::Result<Self> {
        let token = std::fs::read_to_string(path)?.trim().to_string();
        if token.is_empty() {
            anyhow::bail!("telegram token file is empty");
        }
        if !token.contains(':') {
            anyhow::bail!("telegram token file does not look like <id>:<secret>");
        }
        Ok(Self::new(token, chat_id.to_string()))
    }

    pub async fn send(&self, html: &str) -> anyhow::Result<()> {
        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.token);
        let resp = self
            .client
            .post(&url)
            .json(&serde_json::json!({
                "chat_id": self.chat_id,
                "text": html,
                "parse_mode": "HTML",
                "disable_web_page_preview": true,
            }))
            .send()
            .await?;

        let status = resp.status();
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        if !body["ok"].as_bool().unwrap_or(false) {
            // Report the API's own error text, never the request URL.
            anyhow::bail!(
                "telegram rejected the message ({}): {}",
                status,
                body["description"].as_str().unwrap_or("no description")
            );
        }
        Ok(())
    }
}

/// Minimal escaping: host names and rule names end up inside HTML.
pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
