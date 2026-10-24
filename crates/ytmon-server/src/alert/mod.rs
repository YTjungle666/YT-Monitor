//! Alert evaluation loop.
//!
//! Every `interval_secs` we look at the latest value of every series, run each
//! rule against every matching host, and let the state machine decide whether
//! something is worth a Telegram message.
//!
//! Deliberately stateless on the query side: one full pass over `latest` is
//! ~300 rows for this fleet, far cheaper than subscribing to the ingest path
//! and having to handle out-of-order replays.

pub mod engine;
pub mod rules;
pub mod telegram;

use std::collections::HashMap;
use std::time::Duration;

use crate::store::Store;
use engine::State;
use rules::{Kind, Level, Rule, RulesFile};
use telegram::{esc, Telegram};

pub struct Cfg {
    pub interval_secs: u64,
    pub rules_file: String,
    pub token_file: String,
    pub chat_id: String,
}

pub async fn run(store: Store, cfg: Cfg) {
    let tg = match Telegram::from_file(&cfg.token_file, &cfg.chat_id) {
        Ok(t) => {
            tracing::info!("telegram alerts enabled");
            Some(t)
        }
        Err(e) => {
            tracing::warn!(error = %e, "telegram unavailable; alerts will be logged only");
            None
        }
    };

    // State cache: hot path avoids a redb read per rule per host per tick.
    let mut states: HashMap<String, State> = HashMap::new();

    let mut ticker = tokio::time::interval(Duration::from_secs(cfg.interval_secs.max(10)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;
        if let Err(e) = evaluate(&store, &cfg.rules_file, tg.as_ref(), &mut states).await {
            tracing::warn!(error = %e, "alert evaluation failed");
        }
    }
}

async fn evaluate(
    store: &Store,
    rules_file: &str,
    tg: Option<&Telegram>,
    states: &mut HashMap<String, State>,
) -> anyhow::Result<()> {
    // Reloaded every tick so editing rules.toml takes effect without a restart.
    let rules = RulesFile::load(rules_file)?;
    if rules.rules.is_empty() {
        return Ok(());
    }

    let now = now_ts();
    let hosts = store.hosts()?;
    let seen = store.last_seen()?;
    let ips = store.host_ips()?;
    let series = store.all_latest()?;

    for rule in rules.rules.iter().filter(|r| r.enabled) {
        for host in hosts.iter().filter(|h| rule.matches_host(h)) {
            // `series_key` is for the log line; the notification uses the
            // human title, not the internal series name. (Do not call this
            // `key` — it would be shadowed by the alert state key below.)
            let (sev, value, series_key) = match rule.kind {
                Kind::Threshold => {
                    let Some((key, raw)) = pick_series(&series, host, &rule.metric) else {
                        // Metric missing is not an alert condition: a host
                        // without a disk mount should not page anyone.
                        continue;
                    };
                    let value = match &rule.denominator_metric {
                        Some(dm) => match pick_series(&series, host, dm) {
                            // No denominator yet (first PVE poll still
                            // pending) means we cannot judge a ratio.
                            Some((_, den)) if den > 0.0 => raw / den * 100.0,
                            _ => continue,
                        },
                        None => raw,
                    };
                    (rule.severity(value), value, key)
                }
                Kind::Stale => {
                    let age = seen.get(host).map(|ts| (now - ts).max(0)).unwrap_or(now);
                    let sev = if age as u64 >= rule.for_secs {
                        Level::Critical
                    } else {
                        Level::Ok
                    };
                    (sev, age as f64, String::new())
                }
            };

            let key = format!("alert:{}|{}", rule.name, host);
            let mut st = states
                .get(&key)
                .cloned()
                .unwrap_or_else(|| load_state(store, &key));

            match st.update(sev, value, now, rule.for_secs) {
                Some(tr) => {
                    let msg = format_msg(rule, host, ips.get(host).map(|s| s.as_str()), &tr, value);
                    let delivered = match tg {
                        Some(t) => match t.send(&msg).await {
                            Ok(()) => {
                                tracing::info!(
                                    rule = %rule.name,
                                    host = %host,
                                    series = %series_key,
                                    level = tr.to.as_str(),
                                    held_secs = tr.held_secs,
                                    "alert sent"
                                );
                                true
                            }
                            Err(e) => {
                                tracing::warn!(
                                    rule = %rule.name, host = %host, error = %e,
                                    "alert delivery failed; will retry next tick"
                                );
                                false
                            }
                        },
                        None => {
                            tracing::warn!(rule = %rule.name, host = %host, "{msg}");
                            true
                        }
                    };
                    if delivered {
                        states.insert(key.clone(), st.clone());
                        save_state(store, &key, &st)?;
                    }
                    // On delivery failure the previous state is kept, so the
                    // transition is retried rather than silently dropped.
                }
                None => {
                    states.insert(key, st);
                }
            }
        }
    }
    Ok(())
}

/// Highest value among the series belonging to `host` + `metric`. A label
/// split (fs.used_pct per mount) should alert on the worst mount, not the
/// first one redb happens to return.
fn pick_series(series: &[(String, f64)], host: &str, metric: &str) -> Option<(String, f64)> {
    let base = format!("{host}|{metric}");
    series
        .iter()
        .filter(|(k, _)| k == &base || k.starts_with(&format!("{base}|")))
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .cloned()
}

fn load_state(store: &Store, key: &str) -> State {
    store
        .get_meta(key)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_state(store: &Store, key: &str, st: &State) -> anyhow::Result<()> {
    store.set_meta(key, &serde_json::to_string(st)?)
}

fn format_msg(
    rule: &Rule,
    host: &str,
    ip: Option<&str>,
    tr: &engine::Transition,
    value: f64,
) -> String {
    let title = rule.title.as_deref().unwrap_or(&rule.name);
    let unit = rule.unit.as_deref().unwrap_or("");
    let addr = ip.unwrap_or("未知");

    if tr.to == Level::Ok {
        let mut s = format!(
            "🟢 <b>已恢复 · {}</b>\n\n主机　{}\n地址　{}\n",
            esc(title),
            esc(host),
            esc(addr)
        );
        if rule.kind != Kind::Stale {
            s.push_str(&format!("当前　<b>{}{}</b>\n", fmt_num(value), unit));
        }
        s.push_str(&format!("异常持续　{}", fmt_dur_cn(tr.held_secs)));
        return s;
    }

    let (icon, level) = if tr.to == Level::Critical {
        ("🔴", "严重")
    } else {
        ("🟡", "警告")
    };

    let mut s = format!(
        "{icon} <b>{level} · {}</b>\n\n主机　{}\n地址　{}\n",
        esc(title),
        esc(host),
        esc(addr)
    );

    if rule.kind == Kind::Stale {
        s.push_str(&format!(
            "已　<b>{}</b> 无数据上报",
            fmt_dur_cn(value as i64)
        ));
        return s;
    }

    s.push_str(&format!("当前　<b>{}{}</b>\n", fmt_num(value), unit));
    s.push_str(&format!("持续　{}", fmt_dur_cn(tr.held_secs)));

    if tr.from != Level::Ok && tr.from != tr.to {
        s.push_str(&format!("\n由{}升级", level_cn(tr.from)));
    }
    s
}

fn level_cn(l: Level) -> &'static str {
    match l {
        Level::Critical => "严重",
        Level::Warning => "警告",
        Level::Ok => "正常",
    }
}

fn fmt_num(v: f64) -> String {
    if (v.fract()).abs() < 0.05 {
        format!("{}", v.round() as i64)
    } else {
        format!("{v:.1}")
    }
}

fn fmt_dur_cn(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s} 秒")
    } else if s < 3600 {
        format!("{} 分 {} 秒", s / 60, s % 60)
    } else if s < 86400 {
        format!("{} 小时 {} 分", s / 3600, (s % 3600) / 60)
    } else {
        format!("{} 天 {} 小时", s / 86400, (s % 86400) / 3600)
    }
}

fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_worst_label_not_first() {
        let series = vec![
            ("h|fs.used_pct|mount=/".to_string(), 10.0),
            ("h|fs.used_pct|mount=/var".to_string(), 95.0),
            ("h|fs.used_total|mount=/".to_string(), 1.0),
        ];
        let (k, v) = pick_series(&series, "h", "fs.used_pct").unwrap();
        assert_eq!(v, 95.0);
        assert_eq!(k, "h|fs.used_pct|mount=/var");
    }

    #[test]
    fn metric_prefix_does_not_overmatch() {
        let series = vec![("h|cpu.usage_pct_total".to_string(), 99.0)];
        assert!(pick_series(&series, "h", "cpu.usage_pct").is_none());
    }

    #[test]
    fn duration_formatting() {
        assert_eq!(fmt_dur_cn(12), "12 秒");
        assert_eq!(fmt_dur_cn(312), "5 分 12 秒");
        assert_eq!(fmt_dur_cn(3780), "1 小时 3 分");
        assert_eq!(fmt_dur_cn(90000), "1 天 1 小时");
    }

    fn sample_rule() -> Rule {
        toml::from_str(
            r#"
            name = "cpu_high"
            title = "CPU 占用过高"
            metric = "cpu.usage_pct"
            hosts = ["*"]
            op = "gt"
            warning = 70
            critical = 90
            for_secs = 300
            unit = "%"
        "#,
        )
        .expect("rule parses")
    }

    #[test]
    fn card_has_host_and_ip() {
        let rule = sample_rule();
        let tr = engine::Transition {
            from: Level::Ok,
            to: Level::Warning,
            held_secs: 312,
        };
        let msg = format_msg(&rule, "web-01", Some("192.0.2.10"), &tr, 78.2);
        assert!(msg.contains("web-01"), "{msg}");
        assert!(msg.contains("192.0.2.10"), "{msg}");
        assert!(msg.contains("警告"), "{msg}");
        assert!(msg.contains("78.2%"), "{msg}");
        assert!(msg.contains("5 分 12 秒"), "{msg}");
        // no internal identifiers leak into the card
        assert!(!msg.contains("cpu.usage_pct"), "{msg}");
        assert!(!msg.contains("cpu_high"), "{msg}");
    }

    #[test]
    fn recovery_card_says_recovered() {
        let rule = sample_rule();
        let tr = engine::Transition {
            from: Level::Warning,
            to: Level::Ok,
            held_secs: 723,
        };
        let msg = format_msg(&rule, "web-01", Some("192.0.2.10"), &tr, 12.3);
        assert!(msg.contains("已恢复"), "{msg}");
        assert!(msg.contains("12 分 3 秒"), "{msg}");
    }

    #[test]
    fn missing_ip_does_not_break_the_card() {
        let rule = sample_rule();
        let tr = engine::Transition {
            from: Level::Ok,
            to: Level::Critical,
            held_secs: 400,
        };
        let msg = format_msg(&rule, "web-01", None, &tr, 96.0);
        assert!(msg.contains("未知"), "{msg}");
    }
}
