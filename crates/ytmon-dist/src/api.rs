//! HTTP surface: index, manifest, file download.
//!
//! Read-only by construction — there is no upload endpoint. Updates are pushed
//! by putting a file into the content directory on the host; that keeps the
//! attack surface to "can you read these two directories".

use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse},
    routing::get,
    Router,
};

use crate::files::{self, Entry};

#[derive(Clone)]
pub struct AppState {
    pub root: Arc<PathBuf>,
    pub cache: files::Cache,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/version.json", get(version))
        .route("/bin/{name}", get(download_bin))
        .route("/install/{name}", get(download_install))
        .with_state(state)
}

fn not_found(msg: &str) -> (StatusCode, String) {
    (StatusCode::NOT_FOUND, msg.to_string())
}

async fn catalog(state: &AppState) -> Vec<Entry> {
    files::catalog(&state.root, &state.cache).await
}

async fn version(State(state): State<AppState>) -> impl IntoResponse {
    let entries = catalog(&state).await;
    let files: Vec<_> = entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "path": e.path,
                "size": e.size,
                "mtime": e.mtime,
                "sha256": e.sha256,
            })
        })
        .collect();
    axum::Json(serde_json::json!({
        "ok": true,
        "generated_at": now_secs(),
        "files": files,
    }))
    .into_response()
}

/// The page shows copy-paste install commands, so the host in them has to be
/// the host the caller actually used — the same build serves LAN, VPN and
/// reverse-proxied access, and a hardcoded name would be wrong for two of them.
fn base_url(headers: &HeaderMap) -> String {
    headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(|h| format!("http://{h}"))
        .unwrap_or_else(|| "http://<this-host>:8088".to_string())
}

/// The ingest listener is the same host on its own port; deriving it keeps the
/// snippet correct behind a proxy that only forwards the dist port.
fn ingest_url(base: &str) -> String {
    let host = base.rsplit_once("://").map(|(_, r)| r).unwrap_or(base);
    let host = host.split(':').next().unwrap_or(host);
    format!("http://{host}:9180/ingest")
}

async fn index(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let base = base_url(&headers);
    let entries = catalog(&state).await;
    let mut rows = String::new();
    for e in &entries {
        rows.push_str(&format!(
            "<tr><td><a href=\"/{p}\">{p}</a></td><td>{s}</td><td><code>{h}</code></td>             <td>{t}</td></tr>",
            p = e.path,
            s = human_size(e.size),
            h = &e.sha256[..16],
            t = fmt_time(e.mtime),
        ));
    }
    let html = format!(
        "<!doctype html><meta charset=utf-8><title>ytmon-dist</title>\
         <style>body{{font:14px system-ui;margin:2em;background:#111;color:#ddd}}\
         a{{color:#7ab}}table{{border-collapse:collapse}}\
         td,th{{padding:.35em .9em;border-bottom:1px solid #333;text-align:left}}\
         code{{color:#9c9}}pre{{background:#1b1b1b;padding:1em;overflow:auto}}</style>\
         <h2>ytmon 分发包</h2>\
         <p>共 {n} 个文件 · <a href=/version.json>version.json</a></p>\
         <table><tr><th>文件</th><th>大小</th><th>sha256</th><th>更新</th></tr>{rows}</table>\
         <h3>安装 / 更新</h3>\
         <pre>curl -fsSL {base}/install/pve.sh -o pve.sh\nBASE={base} SERVER_URL={ingest} sh pve.sh</pre>\
         <h3>只拿二进制</h3>\
         <pre>curl -fsSL {base}/bin/ytmon-agent -o /usr/local/bin/ytmon-agent</pre>",
        n = entries.len(),
        base = base,
        ingest = ingest_url(&base),
    );
    Html(html).into_response()
}

async fn download_bin(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    serve(&state, "bin", &name, "application/octet-stream").await
}

async fn download_install(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    serve(&state, "install", &name, "text/plain; charset=utf-8").await
}

/// Streams a file from one of the published directories.
///
/// The name is validated *before* it is joined to the root: `..` is rejected by
/// the allow-list, and `Path::file_name` cannot contain a separator, so a
/// request cannot reach outside `root/<dir>`.
async fn serve(
    state: &AppState,
    dir: &str,
    name: &str,
    content_type: &str,
) -> axum::response::Response {
    if !files::safe_name(name) {
        return not_found("bad name").into_response();
    }
    let path = state.root.join(dir).join(name);
    match tokio::fs::read(&path).await {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, content_type.to_string()),
                (header::CACHE_CONTROL, "no-cache".to_string()),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => not_found("no such file").into_response(),
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn human_size(n: u64) -> String {
    const U: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{v:.0} {}", U[i])
    } else {
        format!("{v:.1} {}", U[i])
    }
}

/// Local date-time for the index, from the system timezone.
fn fmt_time(ts: i64) -> String {
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&ts, &mut tm).is_null() {
            return "-".into();
        }
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min
        )
    }
}
