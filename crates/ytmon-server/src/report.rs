//! Monthly traffic report.
//!
//! Once a month (1st day, after a configured local hour) the previous month's
//! per-guest traffic is summarised into one Telegram card.
//!
//! Data source is the network counters each agent already reports
//! (`net.rx_bytes` / `net.tx_bytes`), so this covers "how much did each CT/VM
//! move". Per-domain inbound traffic needs the SNI gateway (P4) — until that
//! exists there is no byte dimension per Host, and inventing one from the
//! Lucky logs is impossible because those records carry no byte field.

use std::collections::HashMap;
use std::time::Duration;

use crate::alert::telegram::{esc, Telegram};
use crate::pve;
use crate::store::Store;

/// One host's traffic for the period, in bytes.
#[derive(Default, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct Traffic {
    pub rx: f64,
    pub tx: f64,
}

impl Traffic {
    fn total(&self) -> f64 {
        self.rx + self.tx
    }
}

pub struct Cfg {
    /// Local hour after which the report goes out (0-23).
    pub hour: u32,
    /// Hosts left out of the card. The NAS is excluded on purpose: almost all
    /// of its traffic is internal (backups, media sync), so it says nothing
    /// about what the household spends on the internet.
    pub exclude: Vec<String>,
    /// PVE API config. When set, traffic comes from PVE's own RRD and the card
    /// reports the household uplink; otherwise it falls back to the agent's
    /// interface counters.
    pub pve: Option<pve::Cfg>,
    /// Where the household uplink is measured: a host that reports the
    /// nic facing the optical modem, plus the interface name. On this setup the
    /// hypervisor itself runs the agent and `wan` is that interface — it is the
    /// only place every byte entering or leaving the house passes through.
    ///
    /// PVE's own node RRD cannot be used for this: it reports wan+lan summed
    /// (measured: 1047.9 GB netin == wan 201 GB + lan 846.7 GB), so the LAN
    /// side would swamp the number.
    pub uplink_host: Option<String>,
    pub uplink_iface: String,
    /// The household LAN port on the same host. Reported as a reference line:
    /// it carries device-to-device traffic that never reaches the internet.
    pub internal_iface: String,
    /// `node_id -> human name`, so the card lists `YT-Board` rather than
    /// `ct:403`. The PVE path already carries guest names; this is what the
    /// agent-counter fallback needs.
    pub labels: HashMap<String, String>,
}

pub async fn run(
    store: Store,
    tg: Telegram,
    cfg: Cfg,
    guests: std::sync::Arc<tokio::sync::RwLock<Vec<pve::Guest>>>,
) {
    // Checking every 15 minutes is cheap (two meta reads) and means a restart
    // around the send hour still delivers the report the same day.
    let mut ticker = tokio::time::interval(Duration::from_secs(900));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let snapshot = guests.read().await.clone();
        if let Err(e) = tick(&store, &tg, &cfg, &snapshot).await {
            tracing::warn!(error = %e, "monthly report failed");
        }
    }
}

async fn tick(
    store: &Store,
    tg: &Telegram,
    cfg: &Cfg,
    guests: &[pve::Guest],
) -> anyhow::Result<()> {
    let (year, month, day, hour) = local_now();
    if day != 1 || hour < cfg.hour {
        return Ok(());
    }

    // Guard against re-sending after a restart, and against the 15-minute
    // ticker firing again inside the same hour.
    let key = format!("report:{year:04}-{month:02}");
    let done = store.get_meta("report:last_month")?;
    if done.as_deref() == Some(key.as_str()) {
        return Ok(());
    }

    let (from, to, label) = prev_month(year, month);
    let period = Period {
        from,
        to,
        label: &label,
        key: &key,
    };
    send_and_archive(store, tg, &period, cfg, guests).await?;
    store.set_meta("report:last_month", &key)?;
    tracing::info!(month = %key, "monthly traffic report sent");
    Ok(())
}

/// A rendered card plus the numbers behind it, so the archive can hold both.
pub struct Card {
    pub text: String,
    pub rows: Vec<(String, Traffic)>,
    /// Household internet total, measured on the modem-facing port.
    pub external: Option<Traffic>,
    /// Household LAN-port total; includes device-to-device transfers.
    pub internal: Option<Traffic>,
}

/// Totals measured on the two physical ports, plus where their series start.
///
/// The ports are the only honest baseline: guest counters count both ends of an
/// internal transfer and never see what a phone or the TV pulled, so a card
/// built from them alone reads as "the house moved 4 TB" when the internet
/// actually carried a tenth of that.
#[derive(Clone, Copy)]
struct Ports {
    external: Option<Traffic>,
    internal: Option<Traffic>,
    /// Earliest instant both ports cover; `None` when neither has data.
    since: Option<i64>,
}

/// Collect and render one period.
///
/// Preferred source for the per-guest rows is PVE's own RRD: it is measured on
/// the hypervisor side and survives the guest being reinstalled. The agent
/// counters remain as a fallback for when PVE is unreachable.
pub async fn build_card(
    store: &Store,
    cfg: &Cfg,
    guests: &[pve::Guest],
    from: i64,
    to: i64,
    label: &str,
) -> anyhow::Result<Card> {
    let ports = ports(store, cfg, from, to);
    if let Some(pcfg) = &cfg.pve {
        match pve::period_traffic(pcfg, guests, from, to).await {
            Ok(t) => {
                let rows: Vec<(String, Traffic)> = t
                    .guests
                    .into_iter()
                    .filter(|g| !cfg.exclude.iter().any(|x| x == &g.name))
                    .filter(|g| g.rx + g.tx > 0.0)
                    .map(|g| (g.name, Traffic { rx: g.rx, tx: g.tx }))
                    .collect();
                return Ok(Card {
                    text: render_rows(&rows, label, ports, from, &cfg.labels),
                    rows,
                    external: ports.external,
                    internal: ports.internal,
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "pve rrd unavailable, falling back to agent counters")
            }
        }
    }

    // Agent counters are keyed by node id, so the names have to come from the
    // label maps: the store's copy (what each agent calls itself) wins over the
    // token table.
    let mut labels = cfg.labels.clone();
    if let Ok(reported) = store.name_map() {
        labels.extend(reported);
    }
    let rows: Vec<(String, Traffic)> = collect(store, from, to)?
        .into_iter()
        .filter(|(h, _)| {
            !cfg.exclude
                .iter()
                .any(|x| x == h || Some(x) == labels.get(h))
        })
        .collect();
    Ok(Card {
        text: render_rows(&rows, label, ports, from, &labels),
        rows,
        external: ports.external,
        internal: ports.internal,
    })
}

/// Both physical-port totals for the window.
fn ports(store: &Store, cfg: &Cfg, from: i64, to: i64) -> Ports {
    let external = port_total(store, cfg, &cfg.uplink_iface, from, to);
    let internal = port_total(store, cfg, &cfg.internal_iface, from, to);
    // The note has to describe the worst case, so the coverage start is the
    // later of the two ports.
    let since = [external.map(|p| p.1), internal.map(|p| p.1)]
        .into_iter()
        .flatten()
        .max();
    Ports {
        external: external.map(|p| p.0),
        internal: internal.map(|p| p.0),
        since,
    }
}

/// Traffic on one physical interface, plus the timestamp of its first sample.
///
/// A partially covered window is still reported — with the port's own start
/// time, so the card can say which days are missing. Refusing to report it
/// (the previous behaviour) left the guest sums alone on the card, which read
/// as if the household had pulled 4 TB from the internet in October.
fn port_total(store: &Store, cfg: &Cfg, iface: &str, from: i64, to: i64) -> Option<(Traffic, i64)> {
    let host = cfg.uplink_host.as_ref()?;
    let rx_key = format!("{host}|net.rx_bytes|iface={iface}");
    let tx_key = format!("{host}|net.tx_bytes|iface={iface}");
    let rx_start = store.history(&rx_key, from, to).ok()?.first()?.0;
    let tx_start = store.history(&tx_key, from, to).ok()?.first()?.0;
    let rx = store.counter_delta(&rx_key, from, to).ok()?;
    let tx = store.counter_delta(&tx_key, from, to).ok()?;
    if rx <= 0.0 && tx <= 0.0 {
        return None;
    }
    Some((Traffic { rx, tx }, rx_start.max(tx_start)))
}

/// Render, send, and archive one period. Archiving both the numbers and the
/// exact card means a past month can be re-read (or re-sent) without rescanning
/// raw samples, and survives independently of the raw table.
pub struct Period<'a> {
    pub from: i64,
    pub to: i64,
    /// Human label for the card heading, e.g. "2026-09".
    pub label: &'a str,
    /// Meta key suffix, e.g. "2026-09" or "prev".
    pub key: &'a str,
}

pub async fn send_and_archive(
    store: &Store,
    tg: &Telegram,
    period: &Period<'_>,
    cfg: &Cfg,
    guests: &[pve::Guest],
) -> anyhow::Result<String> {
    let card = build_card(store, cfg, guests, period.from, period.to, period.label).await?;
    tg.send(&card.text).await?;
    // Keep the uplink next to the per-host rows: without it, the archived
    // numbers cannot reproduce the card's headline line.
    let archive = serde_json::json!({
        "external": card.external,
        "internal": card.internal,
        "hosts": card.rows,
    });
    store.set_meta(&format!("report:data:{}", period.key), &archive.to_string())?;
    store.set_meta(&format!("report:card:{}", period.key), &card.text)?;
    Ok(card.text)
}

/// Parse "2026-09" into `[from, to)` plus a display label.
pub fn month_bounds(spec: &str) -> Option<(i64, i64, String)> {
    let (y, m) = spec.split_once('-')?;
    let year: i32 = y.parse().ok()?;
    let month: u32 = m.parse().ok()?;
    if !(1..=12).contains(&month) {
        return None;
    }
    let from = local_midnight(year, month, 1);
    let (ny, nm) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    Some((
        from,
        local_midnight(ny, nm, 1),
        format!("{year} 年 {month} 月"),
    ))
}

/// Per-host totals for a window, biggest first, zero-traffic hosts dropped.
pub fn collect(store: &Store, from: i64, to: i64) -> anyhow::Result<Vec<(String, Traffic)>> {
    let mut per_host: HashMap<String, Traffic> = HashMap::new();

    for (key, _) in store.all_latest()? {
        let mut parts = key.split('|');
        let Some(host) = parts.next() else { continue };
        let Some(metric) = parts.next() else { continue };

        let is_rx = metric == "net.rx_bytes";
        let is_tx = metric == "net.tx_bytes";
        if !is_rx && !is_tx {
            continue;
        }
        // Several interfaces per host (eth0, wlan0, ...) all fold into the
        // host total; that is what "this container moved N GB" means.
        let delta = store.counter_delta(&key, from, to)?;
        let entry = per_host.entry(host.to_string()).or_default();
        if is_rx {
            entry.rx += delta;
        } else {
            entry.tx += delta;
        }
    }

    let mut rows: Vec<(String, Traffic)> = per_host
        .into_iter()
        .filter(|(_, t)| t.total() > 0.0)
        .collect();
    rows.sort_by(|a, b| {
        b.1.total()
            .partial_cmp(&a.1.total())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(rows)
}

/// Compose the card from already-collected totals (kept separate so the
/// archived copy and a live render cannot drift apart).
///
/// Shape: the two physical-port lines are the headline, the per-guest rows sit
/// under a divider as reference only. There is deliberately no summed total of
/// the rows — that number counts both ends of every internal transfer and reads
/// as an internet figure it never was.
fn render_rows(
    rows: &[(String, Traffic)],
    label: &str,
    ports: Ports,
    from: i64,
    labels: &HashMap<String, String>,
) -> String {
    if rows.is_empty() && ports.external.is_none() && ports.internal.is_none() {
        return format!("📊 <b>月度流量汇总</b>\n{label}\n\n本期没有采样数据。");
    }

    let mut s = format!("📊 <b>月度流量汇总</b>\n{label}\n\n");
    match ports.external {
        Some(u) => s.push_str(&format!(
            "外网　↓ <b>{}</b>　↑ <b>{}</b>\n",
            human(u.rx),
            human(u.tx)
        )),
        None => s.push_str("外网　无数据\n"),
    }
    if let Some(u) = ports.internal {
        s.push_str(&format!("内网合计　↓ {}　↑ {}\n", human(u.rx), human(u.tx)));
    }
    // Say which days the ports do not cover. An hour of slack absorbs the 60s
    // scrape interval and clock skew; anything more means the series starts
    // mid-window, and hiding that is what made October look like 4 TB.
    const COVERAGE_GRACE: i64 = 3600;
    if let Some(since) = ports.since {
        if since - from > COVERAGE_GRACE {
            s.push_str(&format!(
                "\n⚠️ 外网/内网合计只有 {} 起的数据（缺前 {} 天），明细为整月\n",
                local_stamp(since),
                (since - from) / 86400
            ));
        }
    }

    if !rows.is_empty() {
        s.push_str("\n—— 以下明细含内网互传，两端各计一次，仅作参考 ——\n\n");
        let shown = rows.len().min(12);
        let (head, tail) = rows.split_at(shown);
        for (host, t) in head {
            s.push_str(&format!(
                "{}　↓ {}　↑ {}\n",
                esc(labels.get(host).map(String::as_str).unwrap_or(host)),
                human(t.rx),
                human(t.tx)
            ));
        }
        if !tail.is_empty() {
            let rx: f64 = tail.iter().map(|(_, t)| t.rx).sum();
            let tx: f64 = tail.iter().map(|(_, t)| t.tx).sum();
            s.push_str(&format!(
                "其余 {} 台　↓ {}　↑ {}\n",
                tail.len(),
                human(rx),
                human(tx)
            ));
        }
    }
    s.trim_end().to_string()
}

fn human(v: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = v.max(0.0);
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    // Bytes stay integral (nobody needs "1.4 B"); everything above gets one
    // decimal, otherwise 1.5 KB would round to a bare "2 KB".
    if i == 0 {
        format!("{v:.0} {}", UNITS[i])
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// Local wall clock, from the system timezone (all guests here are +0800).
fn local_now() -> (i32, u32, u32, u32) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&ts, &mut tm).is_null() {
            return (1970, 1, 1, 0);
        }
        (
            tm.tm_year + 1900,
            (tm.tm_mon + 1) as u32,
            tm.tm_mday as u32,
            tm.tm_hour as u32,
        )
    }
}

/// `MM-DD HH:MM` in local time, for the coverage note.
fn local_stamp(ts: i64) -> String {
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&ts, &mut tm).is_null() {
            return "?".to_string();
        }
        format!(
            "{:02}-{:02} {:02}:{:02}",
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min
        )
    }
}

/// Midnight local time of the given year/month/day, as unix seconds.
fn local_midnight(year: i32, month: u32, day: u32) -> i64 {
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        tm.tm_year = year - 1900;
        tm.tm_mon = month as i32 - 1;
        tm.tm_mday = day as i32;
        tm.tm_isdst = -1;
        libc::mktime(&mut tm) as i64
    }
}

/// Previous calendar month as `[from, to)` plus a display label.
pub fn prev_month(year: i32, month: u32) -> (i64, i64, String) {
    let (py, pm) = if month == 1 {
        (year - 1, 12)
    } else {
        (year, month - 1)
    };
    (
        local_midnight(py, pm, 1),
        local_midnight(year, month, 1),
        format!("{py} 年 {pm} 月"),
    )
}

/// Previous month relative to the local clock.
pub fn prev_month_now() -> (i64, i64, String) {
    let (year, month, _, _) = local_now();
    prev_month(year, month)
}

/// Current month as `[from, to_up_to_now)` — used by the preview endpoint so a
/// dry run shows something non-empty on the 15th instead of a blank card.
pub fn this_month_so_far() -> (i64, i64, String) {
    let (year, month, _, _) = local_now();
    let start = local_midnight(year, month, 1);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(start);
    (start, now, format!("{year} 年 {month} 月（截至今天）"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(since: Option<i64>) -> String {
        let from = 1_790_000_000;
        let ports = Ports {
            external: Some(Traffic {
                rx: 257e9,
                tx: 26e9,
            }),
            internal: Some(Traffic {
                rx: 1.1e12,
                tx: 1.5e12,
            }),
            since,
        };
        let rows = vec![(
            "ct:411".to_string(),
            Traffic {
                rx: 1.9e12,
                tx: 2.2e12,
            },
        )];
        render_rows(&rows, "2026 年 10 月", ports, from, &HashMap::new())
    }

    #[test]
    fn ports_are_the_headline_and_rows_are_reference_only() {
        let full = card(Some(1_790_000_000));
        assert!(
            full.contains("外网　↓ <b>239.3 GB</b>　↑ <b>24.2 GB</b>"),
            "{full}"
        );
        assert!(full.contains("内网合计"), "{full}");
        assert!(full.contains("仅作参考"), "{full}");
        // The summed row total is what made October read as 4 TB of internet.
        assert!(!full.contains("各主机合计"), "{full}");
        assert!(!full.contains("缺前"), "{full}");
    }

    #[test]
    fn partial_coverage_says_which_days_are_missing() {
        let late = card(Some(1_790_000_000 + 22 * 86400));
        assert!(late.contains("缺前 22 天"), "{late}");
        assert!(late.contains("明细为整月"), "{late}");
        // A port line is never dropped for being partial.
        assert!(late.contains("外网　↓"), "{late}");
    }
}
