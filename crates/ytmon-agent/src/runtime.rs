//! Agent runtime: the collect → encode → send loop, and the spool replay that
//! keeps samples alive across a server outage.
//!
//! Kept separate from `main` so the send logic can be exercised without a
//! daemon, and so console mode and Windows service mode share one code path.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use ytmon_core::Batch;

use crate::collect::Collector;
use crate::config::Config;
use crate::report;

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// One scrape, stamped with the current second.
pub fn build(col: &mut Collector, host: &str, ctid: Option<u32>, seq: u64, server: &str) -> Batch {
    let ts = now();
    let mut b = Batch::new(host, ctid, seq);
    b.ip = local_ip(server);
    for s in col.collect(ts) {
        b.push(s);
    }
    b
}

/// This host's address on the default route.
///
/// `UdpSocket::connect` sends nothing — it only makes the kernel pick a
/// source address — so this works without any network traffic and without
/// enumerating interfaces (which returns the wrong one on multi-homed hosts).
/// The target is the ingest server itself: a route to it has to exist, since
/// reporting to it is the whole job, and it stays correct on hosts that have no
/// internet access or a single non-default route.
/// Reduce an ingest URL to the `host:port` that `connect` needs.
fn route_target(server: &str) -> String {
    let rest = server.split("://").nth(1).unwrap_or(server);
    let host_port = rest.split('/').next().unwrap_or(rest);
    if host_port.contains(':') {
        host_port.to_string()
    } else {
        format!("{host_port}:80")
    }
}

fn local_ip(server: &str) -> Option<String> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect(route_target(server)).ok()?;
    let addr = sock.local_addr().ok()?;
    let ip = addr.ip();
    if ip.is_loopback() || ip.is_unspecified() {
        return None;
    }
    Some(ip.to_string())
}

/// This host's PVE guest id, when the root disk is named after it.
///
/// PVE's LVM volumes are `vm-<id>-disk-<n>`; systemd escapes the dashes in the
/// device name to `vm--<id>--disk--0`. ZFS uses `subvol-<id>-disk-<n>`. The
/// router image already relies on the LVM form in `/lib/preinit/01_sysinfo`,
/// so this is the same rule plus the ZFS name; a layout that hides the source
/// falls back to server name lookup.
///
/// `shortcut: LVM/ZFS names only. Directory-backed rootfs is not parsed;
/// upgrade when a guest moves to one.`
fn vmid_from_source(src: &str) -> Option<u32> {
    vmid_from_prefixed(src, "vm-").or_else(|| vmid_from_prefixed(src, "subvol-"))
}

fn vmid_from_prefixed(src: &str, prefix: &str) -> Option<u32> {
    let i = src.find(prefix)?;
    let rest = &src[i + prefix.len()..];
    let rest = rest.strip_prefix('-').unwrap_or(rest);
    let end = rest.find(|c: char| !c.is_ascii_digit())?;
    if end == 0 {
        return None;
    }
    let vmid: u32 = rest[..end].parse().ok()?;
    rest[end..]
        .trim_start_matches('-')
        .starts_with("disk")
        .then_some(vmid)
}

fn vmid_from_mounts(text: &str) -> Option<u32> {
    for line in text.lines() {
        let mut f = line.split_whitespace();
        let Some(source) = f.next() else { continue };
        if f.next() != Some("/") {
            continue;
        }
        if let Some(vmid) = vmid_from_source(source) {
            return Some(vmid);
        }
    }
    None
}

pub fn detect_ctid() -> Option<u32> {
    vmid_from_mounts(&std::fs::read_to_string("/proc/mounts").ok()?)
}

/// Run until `stop` is set. On Linux/console only Ctrl-C sets it; under the
/// Windows service dispatcher the SCM stop control sets it.
pub async fn run_until(
    cfg: Config,
    mut col: Collector,
    host: String,
    stop: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let mut seq: u64 = 0;
    let mut tick = tokio::time::interval(Duration::from_secs(cfg.interval_secs.max(5)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let ctid = detect_ctid();
    tracing::info!(ctid = ?ctid, "guest id detected");

    loop {
        if stop.load(Ordering::SeqCst) {
            tracing::info!("stop requested, exiting");
            return Ok(());
        }
        tick.tick().await;
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        seq += 1;

        replay_spool(&cfg, &client).await;

        let batch = build(&mut col, &host, ctid, seq, &cfg.server);
        let n = batch.samples.len();
        match report::encode(&batch) {
            Ok(body) => match report::post(
                &client,
                &cfg.server,
                body.clone(),
                cfg.server_token.as_deref(),
            )
            .await
            {
                // One line per success would be 1440 lines a day of "nothing is
                // wrong" on every host. Liveness is already visible from the
                // server side; keep the per-tick proof behind RUST_LOG=debug.
                Ok(()) => tracing::debug!(seq, samples = n, "sent"),
                Err(e) => {
                    tracing::warn!(seq, error = %e, "send failed, spooling");
                    match report::spool(&cfg.state_dir, seq, &body) {
                        Ok(0) => {}
                        Ok(dropped) => tracing::warn!(
                            seq,
                            dropped,
                            cap = report::SPOOL_MAX_FILES,
                            "spool full, oldest batches dropped"
                        ),
                        Err(e) => tracing::error!(error = %e, "spool write failed, sample lost"),
                    }
                }
            },
            Err(e) => tracing::error!(error = %e, "encode failed"),
        }
    }
}

/// Push a handful of spooled batches before the new one, oldest first. Stops
/// at the first failure so a long outage keeps its ordering and does not turn
/// one cycle into thousands of retries.
async fn replay_spool(cfg: &Config, client: &reqwest::Client) {
    for path in report::spool_files(&cfg.state_dir, 5) {
        let Ok(body) = std::fs::read(&path) else {
            continue;
        };
        match report::post(client, &cfg.server, body, cfg.server_token.as_deref()).await {
            Ok(()) => {
                let _ = std::fs::remove_file(&path);
            }
            Err(e) => {
                tracing::warn!(error = %e, "spool replay failed, keeping backlog");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pve_lvm_names_yield_the_guest_id() {
        // systemd escapes both dashes of `vm-1166-disk-0`.
        assert_eq!(
            vmid_from_source("/dev/mapper/VM-vm--1166--disk--0"),
            Some(1166)
        );
        // Some layouts expose the unescaped PVE name.
        assert_eq!(
            vmid_from_source("/dev/mapper/VM-vm-1044-disk-0"),
            Some(1044)
        );
        // A plain VM disk has no guest id in it.
        assert_eq!(vmid_from_source("/dev/sda2"), None);
        // ZFS names the subvolume after the guest too.
        assert_eq!(
            vmid_from_source("rpool/data/subvol-1233-disk-0"),
            Some(1233)
        );
        // Directory-backed rootfs does not expose the id; server name lookup.
        assert_eq!(vmid_from_source("/dev/mapper/vg0-root"), None);
    }

    #[test]
    fn only_the_root_mount_is_read() {
        let mounts = "\
/dev/sda1 /boot ext4 rw 0 0
/dev/mapper/VM-vm--1022--disk--0 / ext4 rw 0 0
";
        assert_eq!(vmid_from_mounts(mounts), Some(1022));
        assert_eq!(vmid_from_mounts("/dev/sda1 /boot ext4 rw 0 0\n"), None);
    }
}
