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
    /// Household uplink total; only known when PVE RRD answered.
    pub uplink: Option<Traffic>,
}

/// Collect and render one period.
///
/// Preferred source is PVE's own RRD: it is measured on the hypervisor side,
/// it survives the guest being reinstalled, and — for the uplink figure — the
/// node interface is the one place every byte leaving the house must pass
/// through, while guest-to-guest traffic on the same bridge never reaches it.
/// The agent counters remain as a fallback for when PVE is unreachable.
pub async fn build_card(
    store: &Store,
    cfg: &Cfg,
    guests: &[pve::Guest],
    from: i64,
    to: i64,
    label: &str,
) -> anyhow::Result<Card> {
    if let Some(pcfg) = &cfg.pve {
        match pve::period_traffic(pcfg, guests, from, to).await {
            Ok(t) => {
                let uplink = uplink_total(store, cfg, from, to);
                let rows: Vec<(String, Traffic)> = t
                    .guests
                    .into_iter()
                    .filter(|g| !cfg.exclude.iter().any(|x| x == &g.name))
                    .filter(|g| g.rx + g.tx > 0.0)
                    .map(|g| (g.name, Traffic { rx: g.rx, tx: g.tx }))
                    .collect();
                return Ok(Card {
                    text: render_rows(&rows, label, uplink.as_ref()),
                    rows,
                    uplink,
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "pve rrd unavailable, falling back to agent counters")
            }
        }
    }

    let rows: Vec<(String, Traffic)> = collect(store, from, to)?
        .into_iter()
        .filter(|(h, _)| !cfg.exclude.iter().any(|x| x == h))
        .collect();
    Ok(Card {
        text: render_rows(&rows, label, None),
        rows,
        uplink: None,
    })
}

/// Household uplink from the agent-side counters of the modem-facing nic.
///
/// Returns `None` when that host has not been reporting for long enough to
/// cover the window — better to show no total than a number that silently
/// means "since the agent was installed".
fn uplink_total(store: &Store, cfg: &Cfg, from: i64, to: i64) -> Option<Traffic> {
    let host = cfg.uplink_host.as_ref()?;
    let rx_key = format!("{host}|net.rx_bytes|iface={}", cfg.uplink_iface);
    let tx_key = format!("{host}|net.tx_bytes|iface={}", cfg.uplink_iface);

    // An hour of slack absorbs the 60s scrape interval and clock skew. More
    // than that and the series starts mid-window, which would report "traffic
    // since the agent was installed" as if it were the month's total.
    const COVERAGE_GRACE: i64 = 3600;
    let first = store.range(&rx_key, from, to).ok()?.first()?.0;
    if first - from > COVERAGE_GRACE {
        tracing::info!(
            host,
            iface = %cfg.uplink_iface,
            from,
            first,
            "uplink series starts inside the window; omitting the total"
        );
        return None;
    }

    let rx = store.counter_delta(&rx_key, from, to).ok()?;
    let tx = store.counter_delta(&tx_key, from, to).ok()?;
    if rx <= 0.0 && tx <= 0.0 {
        return None;
    }
    Some(Traffic { rx, tx })
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
        "uplink": card.uplink,
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
fn render_rows(rows: &[(String, Traffic)], label: &str, uplink: Option<&Traffic>) -> String {
    if rows.is_empty() && uplink.is_none() {
        return format!("📊 <b>月度流量汇总</b>\n{label}\n\n本期没有采样数据。");
    }

    let mut s = format!("📊 <b>月度流量汇总</b>\n{label}\n\n");
    if let Some(u) = uplink {
        s.push_str(&format!(
            "外网出口　↓ <b>{}</b>　↑ <b>{}</b>\n\n",
            human(u.rx),
            human(u.tx)
        ));
    }

    let shown = rows.len().min(12);
    let (head, tail) = rows.split_at(shown);
    for (host, t) in head {
        s.push_str(&format!(
            "{}　↓ {}　↑ {}\n",
            esc(host),
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
    let rx: f64 = rows.iter().map(|(_, t)| t.rx).sum();
    let tx: f64 = rows.iter().map(|(_, t)| t.tx).sum();
    s.push_str(&format!(
        "\n各主机合计（{} 台）　↓ {}　↑ {}",
        rows.len(),
        human(rx),
        human(tx)
    ));
    if uplink.is_some() {
        // Without this note the two totals look contradictory: per-guest sums
        // include backups and media sync between guests, which never touch the
        // uplink.
        s.push_str("\n（各主机合计含内网互传；网关出口是路由器转发量）");
    }
    s
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

    #[test]
    fn bytes_are_human() {
        assert_eq!(human(0.0), "0 B");
        assert_eq!(human(999.0), "999 B");
        assert_eq!(human(1024.0), "1.0 KB");
        assert_eq!(human(1536.0), "1.5 KB");
        assert_eq!(human(1024.0 * 1024.0 * 1024.0 * 5.5), "5.5 GB");
        assert_eq!(human(1024.0_f64.powi(4) * 2.0), "2.0 TB");
    }

    #[test]
    fn previous_month_crosses_the_year_boundary() {
        let (from, to, label) = prev_month(2026, 1);
        assert!(to > from);
        // December is 31 days.
        assert_eq!(to - from, 31 * 86400, "local zone has no DST here");
        assert_eq!(label, "2025 年 12 月");
    }

    #[test]
    fn previous_month_is_a_full_month() {
        let (from, to, label) = prev_month(2026, 3);
        assert_eq!(to - from, 28 * 86400, "February 2026");
        assert_eq!(label, "2026 年 2 月");
    }
}
