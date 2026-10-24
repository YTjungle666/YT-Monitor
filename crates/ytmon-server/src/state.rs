//! Shared handler state.

use crate::config::AgentToken;
use crate::pve::Guest;
use crate::store::Store;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Cheap to clone: the store is an `Arc<Database>` and the guest cache is an
/// `Arc<RwLock<..>>`, so axum can hand a copy to every request.
#[derive(Clone)]
pub struct App {
    pub store: Store,
    /// Last successful PVE discovery result, empty until the first poll.
    pub pve_guests: Arc<RwLock<Vec<Guest>>>,
    /// Config is kept for endpoints that need Telegram settings at request
    /// time (manual monthly-report resend).
    pub cfg: Arc<crate::config::FileCfg>,
    /// token -> (node_id, configured name). Looked up on every ingest, so a
    /// request cannot pick its own identity by changing a field in the body.
    pub tokens: Arc<HashMap<String, AgentToken>>,
}

impl App {
    pub fn token_book(agents: &[AgentToken]) -> Arc<HashMap<String, AgentToken>> {
        Arc::new(
            agents
                .iter()
                .filter(|a| !a.token.is_empty())
                .map(|a| (a.token.clone(), a.clone()))
                .collect(),
        )
    }
}
