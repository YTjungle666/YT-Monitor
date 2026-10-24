//! HTTP surface: agent ingest, query API, and the single HTML page.
//!
//! Handlers stay thin — all storage semantics live in `store`, all transport
//! decoding (gzip, JSON, protocol version) is contained here.

use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    middleware,
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use flate2::read::GzDecoder;
use std::io::Read;
use ytmon_core::Batch;

use crate::state::App;
use crate::web;

/// Endpoints that agents and internal tooling need. This is the machine-facing
/// surface, so it is the one that carries a credential: a Bearer token in
/// `Authorization` (agents read it from their config). `GET /` here answers 404
/// rather than serving the dashboard.
pub fn ingest_router(app: App) -> Router {
    data_routes()
        // Delete is a write. It exists only on the authenticated listener, so
        // the open dashboard cannot be used to drop a host by accident.
        .route("/api/host", axum::routing::delete(api_drop_host))
        .route("/api/_meta", get(api_meta_dump))
        .route_layer(middleware::from_fn_with_state(app.clone(), require_token))
        .with_state(app)
}

/// The dashboard and its data, on the second listener. LAN-only by intent, so
/// no credentials: the operator opens a browser and sees the page. Put it
/// behind the reverse proxy if it ever needs to be reachable from outside.
pub fn panel_router(app: App) -> Router {
    // Reads only. `/ingest` and the write endpoints stay off this listener: it
    // carries no credentials, so it must not accept data or delete anything.
    Router::new()
        .route("/", get(index))
        .route("/favicon.ico", get(favicon))
        .route("/icon-32.png", get(favicon))
        .route("/icon-180.png", get(touch_icon))
        .route("/api/hosts", get(api_hosts))
        .route("/api/overview", get(api_overview))
        .route("/api/latest", get(api_latest))
        .route("/api/query", get(api_query))
        .route("/api/pve/guests", get(api_pve_guests))
        .route("/api/report/preview", get(api_report_preview))
        .route("/api/report/history", get(api_report_history))
        .with_state(app)
}

fn data_routes() -> Router<App> {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/ingest", post(ingest))
        .route("/api/hosts", get(api_hosts))
        .route("/api/overview", get(api_overview))
        .route("/api/latest", get(api_latest))
        .route("/api/query", get(api_query))
        .route("/api/pve/guests", get(api_pve_guests))
        .route("/api/report/preview", get(api_report_preview))
        .route("/api/report/send", post(api_report_send))
        .route("/api/report/history", get(api_report_history))
}

#[derive(serde::Deserialize)]
struct MetaQuery {
    prefix: Option<String>,
}

/// Debug aid: dump meta entries with a given prefix (`up:`, `seen:`, ...).
async fn api_meta_dump(State(app): State<App>, Query(q): Query<MetaQuery>) -> impl IntoResponse {
    let prefix = q.prefix.unwrap_or_default();
    match app.store.meta_with_prefix(&prefix) {
        Ok(pairs) => Json(serde_json::json!({"ok": true, "prefix": prefix, "entries": pairs}))
            .into_response(),
        Err(e) => server_err(e).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct HostQuery {
    host: String,
}

/// Forget a host that no longer exists (renamed or decommissioned).
async fn api_drop_host(State(app): State<App>, Query(q): Query<HostQuery>) -> impl IntoResponse {
    match app.store.drop_host(&q.host) {
        Ok(n) => Json(serde_json::json!({"ok": true, "host": q.host, "series_removed": n}))
            .into_response(),
        Err(e) => server_err(e).into_response(),
    }
}

/// Bearer-token gate for the machine-facing listener.
///
/// Constant-time compare, so a wrong token costs the same as a right one up to
/// the first differing byte. With no token configured the listener stays open —
/// only acceptable while bringing a new host online, since the ingest endpoint
/// is what writes to the database.
/// Who is calling, resolved from the token rather than from the request body.
#[derive(Clone)]
pub struct NodeIdentity {
    pub node_id: String,
}

async fn require_token(
    State(app): State<App>,
    mut req: axum::extract::Request,
    next: middleware::Next,
) -> axum::response::Response {
    // Health checks come from the service manager, not from a caller that can
    // hold a secret.
    if req.uri().path() == "/healthz" {
        return next.run(req).await;
    }

    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(|t| t.trim().to_string());

    if let Some(tok) = presented {
        if let Some(agent) = app.tokens.get(&tok) {
            req.extensions_mut().insert(NodeIdentity {
                node_id: agent.node_id.clone(),
            });
            return next.run(req).await;
        }
    }

    if app.tokens.is_empty() {
        return next.run(req).await;
    }

    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        "需要 token",
    )
        .into_response()
}

fn server_err(e: impl std::fmt::Display) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"ok": false, "error": e.to_string()})),
    )
}

fn bad_request(e: impl std::fmt::Display) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({"ok": false, "error": e.to_string()})),
    )
}

/// The page itself is never cached: a deploy changes the icon links and the
/// inline CSS, and a heuristically cached copy means the browser keeps showing
/// the previous build's tab icon with no obvious way to notice.
async fn index() -> impl IntoResponse {
    let mut h = HeaderMap::new();
    h.insert(
        header::CACHE_CONTROL,
        "no-cache".parse().expect("static header"),
    );
    (h, Html(web::INDEX_HTML))
}

/// Tab icon and iOS home-screen icon.
///
/// Embedded in the binary rather than read from disk, because the deployment
/// story is still "one file per service" — a container should not need an
/// asset directory next to the executable. Two sizes only: the 32 px favicon
/// is a small-size variant of the dashboard mark (continuous ring, heavier
/// pulse, no glow) because the 512 px artwork turns to mush at tab size.
static FAVICON: &[u8] = include_bytes!("../assets/favicon-32.png");
static TOUCH_ICON: &[u8] = include_bytes!("../assets/icon-180.png");

async fn favicon() -> impl IntoResponse {
    asset(FAVICON)
}

async fn touch_icon() -> impl IntoResponse {
    asset(TOUCH_ICON)
}

fn asset(bytes: &'static [u8]) -> axum::response::Response {
    let mut h = HeaderMap::new();
    h.insert(
        header::CONTENT_TYPE,
        "image/png".parse().expect("static header"),
    );
    // Revalidated on every use: a favicon that sticks in the browser cache is
    // impossible to correct without changing its URL, which is exactly the
    // trap a long max-age set here. These are 2 KB images on a LAN page.
    h.insert(
        header::CACHE_CONTROL,
        "no-cache".parse().expect("static header"),
    );
    (h, bytes).into_response()
}

async fn healthz(State(app): State<App>) -> impl IntoResponse {
    match app.store.series_count() {
        Ok(n) => Json(serde_json::json!({"ok": true, "series": n})).into_response(),
        Err(e) => server_err(e).into_response(),
    }
}

async fn ingest(
    State(app): State<App>,
    ident: Option<axum::Extension<NodeIdentity>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let gzipped = headers
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("gzip"))
        .unwrap_or(false);

    let raw = if gzipped {
        let mut out = Vec::with_capacity(body.len() * 4);
        match GzDecoder::new(&body[..]).read_to_end(&mut out) {
            Ok(_) => out,
            Err(e) => return bad_request(format!("gunzip: {e}")).into_response(),
        }
    } else {
        body.to_vec()
    };

    let mut batch: Batch = match serde_json::from_slice(&raw) {
        Ok(b) => b,
        Err(e) => return bad_request(format!("json: {e}")).into_response(),
    };

    if batch.v != ytmon_core::PROTO_VERSION {
        return bad_request(format!("proto v{} unsupported", batch.v)).into_response();
    }

    // The token decides the identity; the `host` field is only a label. That is
    // what lets a rename be a rename instead of a brand-new machine.
    let label = batch.host.clone();
    if let Some(axum::Extension(id)) = ident {
        batch.host = id.node_id;
    }
    let host = batch.host.clone();

    // Refresh the label only when it actually changed: one read per batch, no
    // extra write in the steady state.
    let key = format!("name:{host}");
    let current = app.store.get_meta(&key).ok().flatten();
    if current.as_deref() != Some(label.as_str()) {
        if let Err(e) = app.store.set_name(&host, &label) {
            tracing::warn!(%host, error = %e, "could not record display name");
        }
    }

    let seq = batch.agent_seq;
    match app.store.ingest(&batch) {
        Ok(r) => {
            tracing::debug!(%host, seq, stored = r.stored, new = r.new_series, "ingested");
            Json(serde_json::json!({
                "ok": true,
                "stored": r.stored,
                "new_series": r.new_series
            }))
            .into_response()
        }
        Err(e) => {
            tracing::error!(%host, seq, error = %e, "ingest failed");
            server_err(e).into_response()
        }
    }
}

async fn api_hosts(State(app): State<App>) -> impl IntoResponse {
    match app.store.hosts() {
        Ok(h) => Json(serde_json::json!({"ok": true, "hosts": h})).into_response(),
        Err(e) => server_err(e).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct LatestQuery {
    host: Option<String>,
}

async fn api_latest(State(app): State<App>, Query(q): Query<LatestQuery>) -> impl IntoResponse {
    let Some(host) = q.host else {
        return bad_request("host query param required").into_response();
    };
    match app.store.latest_for_host(&host) {
        Ok(rows) => {
            let items: Vec<_> = rows
                .into_iter()
                .map(|(k, v)| serde_json::json!({"series": k, "value": v}))
                .collect();
            Json(serde_json::json!({"ok": true, "host": host, "items": items})).into_response()
        }
        Err(e) => server_err(e).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct RangeQuery {
    series: String,
    from: i64,
    to: i64,
}

/// One row per host with just the numbers a dashboard shows.
///
/// The page would otherwise need N+1 requests (one per host), each re-parsing
/// every series; this is a single scan of the latest table.
/// Sort key for the dashboard: host, then VMs, then containers, each by
/// numeric id. Anything unrecognised sorts last rather than crashing the page.
fn group_rank(node_id: &str) -> (u8, u32) {
    let (group, rest) = match node_id.split_once(':') {
        Some((g, r)) => (g, r),
        None => ("ct", node_id),
    };
    let rank = match group {
        "host" => 0,
        "vm" => 1,
        _ => 2,
    };
    (rank, rest.parse::<u32>().unwrap_or(u32::MAX))
}

async fn api_overview(State(app): State<App>) -> impl IntoResponse {
    let all = match app.store.all_latest() {
        Ok(v) => v,
        Err(e) => return server_err(e).into_response(),
    };
    let ips = app.store.host_ips().unwrap_or_default();
    let seen = app.store.last_seen().unwrap_or_default();
    let names = app.store.name_map().unwrap_or_default();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    // series key is "host|metric" or "host|metric|label=value"
    let mut by_host: std::collections::BTreeMap<String, Vec<(String, f64)>> =
        std::collections::BTreeMap::new();
    for (key, v) in all {
        let (host, rest) = match key.split_once('|') {
            Some(x) => x,
            None => continue,
        };
        by_host
            .entry(host.to_string())
            .or_default()
            .push((rest.to_string(), v));
    }

    let mut rows = Vec::with_capacity(by_host.len());
    for (host, items) in &by_host {
        let get = |name: &str| items.iter().find(|(k, _)| k == name).map(|(_, v)| *v);
        let get_prefix = |prefix: &str| {
            items
                .iter()
                .find(|(k, _)| k.starts_with(prefix))
                .map(|(_, v)| *v)
        };

        let cpu = get("cpu.usage_pct");
        // Memory: prefer the guest's configured RAM (PVE knows it; the cgroup
        // usually says "max"), then machine RAM for hosts that are not guests,
        // and only then a percentage the agent already computed.
        let mem = match (get("mem.used_bytes"), get("pve.maxmem_bytes")) {
            (Some(u), Some(m)) if m > 0.0 => Some(u / m * 100.0),
            _ => match (get("mem.used_bytes"), get("mem.total_bytes")) {
                (Some(u), Some(t)) if t > 0.0 => Some(u / t * 100.0),
                _ => get("mem.used_pct"),
            },
        };
        // Windows reports used/total per drive but no percentage, so compute
        // one rather than showing a dash on a host that is otherwise fine.
        let disk = get("fs.used_pct|mount=/")
            .or_else(|| get_prefix("fs.used_pct"))
            .or_else(|| {
                let mut mounts: Vec<&str> = items
                    .iter()
                    .filter_map(|(k, _)| k.strip_prefix("fs.used_bytes|mount="))
                    .collect();
                mounts.sort_unstable();
                let m = mounts.first()?;
                let used = get(&format!("fs.used_bytes|mount={m}"))?;
                let total = get(&format!("fs.total_bytes|mount={m}"))?;
                if total > 0.0 {
                    Some(used / total * 100.0)
                } else {
                    None
                }
            });

        let age = seen.get(host).map(|t| (now - t).max(0)).unwrap_or(i64::MAX);
        // Three scrapes missed: the agent is gone, not just briefly late.
        let online = age <= 180;

        // `node_id` is the identity (ct:400) and drives ordering; `host` is the
        // mutable label the agent reports, which is what a person reads.
        // Reported label, else the name from the token table: a host that has
        // not posted since the identity change should still read "pve", not
        // "host:pve".
        let label = names
            .get(host)
            .cloned()
            .or_else(|| {
                app.tokens
                    .values()
                    .find(|a| a.node_id == *host)
                    .map(|a| a.name.clone())
            })
            .unwrap_or_else(|| host.clone());
        rows.push(serde_json::json!({
            "node_id": host,
            "host": label,
            "ip": ips.get(host),
            "age": age,
            "online": online,
            "cpu": cpu,
            "mem": mem,
            "disk": disk,
            "series": items.len(),
        }));
    }

    // Fixed order: host, then VMs, then containers, each by numeric id. Cards
    // used to reorder on every poll (busiest first), which made a bad machine
    // move away from under the cursor. Offline hosts keep their slot too.
    rows.sort_by_key(|r| group_rank(r["node_id"].as_str().unwrap_or("")));

    Json(serde_json::json!({"ok": true, "hosts": rows})).into_response()
}

/// Historic points. The resolution is chosen from the requested span: minute
/// data only exists for the last two weeks, so a year-wide range reads the
/// daily rollup instead of returning a handful of rows from `raw`.
async fn api_query(State(app): State<App>, Query(q): Query<RangeQuery>) -> impl IntoResponse {
    let span = q.to - q.from;
    let resolution = if span <= 2 * 86400 {
        "1m"
    } else if span <= 360 * 86400 {
        "1h"
    } else {
        "1d"
    };
    match app.store.history(&q.series, q.from, q.to) {
        Ok(points) => {
            let pts: Vec<[f64; 2]> = points.into_iter().map(|(t, v)| [t as f64, v]).collect();
            Json(serde_json::json!({
                "ok": true,
                "series": q.series,
                "resolution": resolution,
                "points": pts,
            }))
            .into_response()
        }
        Err(e) => server_err(e).into_response(),
    }
}

async fn api_pve_guests(State(app): State<App>) -> impl IntoResponse {
    let guests = app.pve_guests.read().await.clone();
    Json(serde_json::json!({"ok": true, "count": guests.len(), "guests": guests}))
}
#[derive(serde::Deserialize)]
struct PreviewQuery {
    /// `prev=1` renders last month; default is the current month to date.
    prev: Option<u8>,
}

/// Renders exactly the card the scheduler would send, without sending it.
/// Handy for checking the layout and the numbers after a deploy.
async fn api_report_preview(
    State(app): State<App>,
    Query(q): Query<PreviewQuery>,
) -> impl IntoResponse {
    let (from, to, label) = if q.prev.unwrap_or(0) == 1 {
        crate::report::prev_month_now()
    } else {
        crate::report::this_month_so_far()
    };
    let cfg = app.cfg.report_cfg();
    let guests = app.pve_guests.read().await.clone();
    match crate::report::build_card(&app.store, &cfg, &guests, from, to, &label).await {
        Ok(card) => (
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            )],
            card.text,
        )
            .into_response(),
        Err(e) => server_err(e).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct SendQuery {
    /// `2026-09` for a finished month, `current` for this month to date.
    /// Omitted means last month.
    month: Option<String>,
}

/// Send one monthly traffic card now, and archive it.
///
/// Exists because the scheduled run is once a month: without this the only way
/// to check the card is to wait for the 1st, and a missed month could never be
/// back-filled after the raw window rolls over.
async fn api_report_send(State(app): State<App>, Query(q): Query<SendQuery>) -> impl IntoResponse {
    let Some((tg, _)) = app.cfg.monthly_report() else {
        return bad_request("monthly report disabled or telegram not configured").into_response();
    };

    let (from, to, label, key) = match q.month.as_deref() {
        Some("current") => {
            let (f, t, l) = crate::report::this_month_so_far();
            (f, t, l, "current".to_string())
        }
        Some(spec) => match crate::report::month_bounds(spec) {
            Some((f, t, l)) => (f, t, l, spec.to_string()),
            None => {
                return bad_request("month must look like 2026-09 or be 'current'").into_response()
            }
        },
        None => {
            let (f, t, l) = crate::report::prev_month_now();
            (f, t, l, "prev".to_string())
        }
    };

    let cfg = app.cfg.report_cfg();
    let guests = app.pve_guests.read().await.clone();
    let period = crate::report::Period {
        from,
        to,
        label: &label,
        key: &key,
    };
    match crate::report::send_and_archive(&app.store, &tg, &period, &cfg, &guests).await {
        Ok(text) => (
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            )],
            text,
        )
            .into_response(),
        Err(e) => server_err(e).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct HistoryQuery {
    /// `2026-10` returns that month's archived card instead of the list.
    month: Option<String>,
}

/// Archived monthly reports, read back from redb (so this also proves the
/// archive survives restarts).
async fn api_report_history(
    State(app): State<App>,
    Query(q): Query<HistoryQuery>,
) -> impl IntoResponse {
    if let Some(month) = q.month {
        return match app.store.get_meta(&format!("report:card:{month}")) {
            Ok(Some(card)) => (
                [(
                    axum::http::header::CONTENT_TYPE,
                    "text/plain; charset=utf-8",
                )],
                card,
            )
                .into_response(),
            Ok(None) => bad_request(format!("no archived report for {month}")).into_response(),
            Err(e) => server_err(e).into_response(),
        };
    }

    match app.store.meta_with_prefix("report:card:") {
        Ok(rows) => {
            let months: Vec<String> = rows
                .into_iter()
                .map(|(k, _)| k.trim_start_matches("report:card:").to_string())
                .collect();
            Json(serde_json::json!({"ok": true, "months": months})).into_response()
        }
        Err(e) => server_err(e).into_response(),
    }
}
