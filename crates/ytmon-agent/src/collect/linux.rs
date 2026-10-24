//! Linux collection from cgroup v2 and /proc.
//!
//! Deliberately does **not** use `/proc/meminfo` or trust `/proc/loadavg`:
//! on this fleet every CT runs without lxcfs meminfo virtualisation, so those
//! files report the hypervisor's numbers (61 GB on a 512 MB guest). cgroup v2
//! files are per-container and are the only correct source for memory/CPU/IO.
//!
//! All counters are reported raw and monotonic; differencing happens in the
//! server so an agent restart cannot fabricate a rate.

use std::collections::HashMap;
use ytmon_core::Sample;

const CGROUP_ROOT: &str = "/sys/fs/cgroup";

pub struct Collector {
    pub mounts: Vec<String>,
    /// Previous (ts, usage_usec) so we can derive a utilisation percentage.
    /// Agents are long-lived daemons, so keeping this here beats making the
    /// server difference counters for every alert rule.
    prev_cpu: Option<(i64, u64)>,
}

impl Collector {
    pub fn new(mounts: Vec<String>) -> Self {
        Self {
            mounts,
            prev_cpu: None,
        }
    }

    pub fn collect(&mut self, ts: i64) -> Vec<Sample> {
        let mut out = Vec::with_capacity(28);
        self.collect_memory(ts, &mut out);
        self.collect_cpu(ts, &mut out);
        self.collect_pids(ts, &mut out);
        self.collect_io(ts, &mut out);
        self.collect_net(ts, &mut out);
        self.collect_fs(ts, &mut out);
        self.collect_facts(ts, &mut out);
        out
    }

    /// Memory truth: `anon` is process memory, `file` is page cache. Reporting
    /// `memory.current` alone makes a database look like it leaks.
    fn collect_memory(&self, ts: i64, out: &mut Vec<Sample>) {
        let stat = format!("{CGROUP_ROOT}/memory.stat");
        if let Some(text) = read(&stat) {
            let kv = parse_kv(&text);
            if let Some(v) = kv.get("anon") {
                out.push(Sample::new("mem.anon_bytes", ts, *v as f64));
                // Cross-platform name for "memory actually held by
                // processes". Windows reports mem.used_bytes from the OS, and
                // alert rules use one metric for both; page cache (`file`) is
                // deliberately excluded because the kernel can reclaim it and
                // counting it makes every NAS look like it is out of memory.
                out.push(Sample::new("mem.used_bytes", ts, *v as f64));
            }
            if let Some(v) = kv.get("file") {
                out.push(Sample::new("mem.file_bytes", ts, *v as f64));
            }
            if let Some(v) = kv.get("file_dirty") {
                out.push(Sample::new("mem.dirty_bytes", ts, *v as f64));
            }
        }
        // Machine RAM, for hosts that are not guests of anything: the
        // hypervisor itself and standalone boxes report a real MemTotal, while
        // inside a container this is the *host's* number and useless — that is
        // why the panel prefers pve.maxmem_bytes when it exists.
        if let Some(text) = read("/proc/meminfo") {
            for line in text.lines() {
                if let Some(rest) = line.strip_prefix("MemTotal:") {
                    if let Some(kb) = rest.split_whitespace().next() {
                        if let Ok(kb) = kb.parse::<u64>() {
                            out.push(Sample::new("mem.total_bytes", ts, (kb * 1024) as f64));
                        }
                    }
                    break;
                }
            }
        }
        if let Some(text) = read(&format!("{CGROUP_ROOT}/memory.current")) {
            if let Ok(v) = text.trim().parse::<u64>() {
                out.push(Sample::new("mem.current_bytes", ts, v as f64));
            }
        }
        // "max" means the guest has no memory limit (this fleet's default);
        // emit nothing rather than invent a denominator.
        if let Some(text) = read(&format!("{CGROUP_ROOT}/memory.max")) {
            if let Ok(v) = text.trim().parse::<u64>() {
                out.push(Sample::new("mem.limit_bytes", ts, v as f64));
            }
        }
        if let Some(text) = read(&format!("{CGROUP_ROOT}/memory.events")) {
            let kv = parse_kv(&text);
            if let Some(v) = kv.get("oom_kill") {
                out.push(Sample::new("mem.oom_kill_total", ts, *v as f64));
            }
        }
    }

    /// CPU truth: cumulative microseconds consumed by this cgroup.
    ///
    /// Also derives `cpu.usage_pct`: utilisation of the CPU the guest actually
    /// owns, where 100% means "every core this container may use is busy".
    /// The denominator is the cpuset (PVE `cores=4` shows up as a 4-CPU
    /// cpuset), falling back to CFS quota and finally to nproc. A single-core
    /// denominator would report 400% for a 4-core guest, which reads as a
    /// broken number to anyone watching a dashboard.
    fn collect_cpu(&mut self, ts: i64, out: &mut Vec<Sample>) {
        if let Some(text) = read(&format!("{CGROUP_ROOT}/cpu.stat")) {
            let kv = parse_kv(&text);
            if let Some(v) = kv.get("usage_usec") {
                out.push(Sample::new("cpu.usage_usec", ts, *v as f64));

                if let Some((prev_ts, prev_usec)) = self.prev_cpu {
                    let dt = ts - prev_ts;
                    // A counter going backwards means the cgroup was recreated
                    // (container restart); skip this interval rather than emit
                    // a negative or absurd rate.
                    if dt > 0 && *v >= prev_usec {
                        let cores = cpu_share();
                        let used_usec = (*v - prev_usec) as f64;
                        let pct = used_usec / (dt as f64 * 1_000_000.0) / cores * 100.0;
                        out.push(Sample::new("cpu.usage_pct", ts, pct));
                    }
                }
                self.prev_cpu = Some((ts, *v));
            }
            if let Some(v) = kv.get("nr_throttled") {
                out.push(Sample::new("cpu.throttled_total", ts, *v as f64));
            }
        }
    }

    fn collect_pids(&self, ts: i64, out: &mut Vec<Sample>) {
        if let Some(text) = read(&format!("{CGROUP_ROOT}/pids.current")) {
            if let Ok(v) = text.trim().parse::<u64>() {
                out.push(Sample::new("proc.count", ts, v as f64));
            }
        }
    }

    /// Block IO truth: `/proc/diskstats` inside a container shows the host's
    /// devices, so use the cgroup's own accounting and sum across devices.
    fn collect_io(&self, ts: i64, out: &mut Vec<Sample>) {
        let Some(text) = read(&format!("{CGROUP_ROOT}/io.stat")) else {
            return;
        };
        let (mut r, mut w) = (0u64, 0u64);
        for line in text.lines() {
            for field in line.split_whitespace().skip(1) {
                let Some((k, v)) = field.split_once('=') else {
                    continue;
                };
                let Ok(n) = v.parse::<u64>() else { continue };
                match k {
                    "rbytes" => r = r.saturating_add(n),
                    "wbytes" => w = w.saturating_add(n),
                    _ => {}
                }
            }
        }
        out.push(Sample::new("io.read_bytes", ts, r as f64));
        out.push(Sample::new("io.write_bytes", ts, w as f64));
    }

    /// Interface counters are per-network-namespace, so this one is safe.
    ///
    /// Virtual interfaces are skipped: a bridge and its physical slave carry
    /// the *same* bytes, and so do docker0/veth pairs or a tun device and the
    /// uplink it tunnels over. Summing them double-counts. On an OpenWrt router, br-lan
    /// and eth0 both read ~16 GB for one month of the same traffic.
    fn collect_net(&self, ts: i64, out: &mut Vec<Sample>) {
        let Some(text) = read("/proc/net/dev") else {
            return;
        };
        for line in text.lines().skip(2) {
            let Some((name, rest)) = line.split_once(':') else {
                continue;
            };
            let name = name.trim();
            if !is_uplink_iface(name) {
                continue;
            }
            let f: Vec<&str> = rest.split_whitespace().collect();
            if f.len() < 9 {
                continue;
            }
            if let Ok(rx) = f[0].parse::<u64>() {
                out.push(Sample::new("net.rx_bytes", ts, rx as f64).label("iface", name));
            }
            if let Ok(tx) = f[8].parse::<u64>() {
                out.push(Sample::new("net.tx_bytes", ts, tx as f64).label("iface", name));
            }
        }
    }

    fn collect_fs(&self, ts: i64, out: &mut Vec<Sample>) {
        for m in &self.mounts {
            if let Some((used, total)) = statvfs(m) {
                out.push(Sample::new("fs.used_bytes", ts, used as f64).label("mount", m.clone()));
                out.push(Sample::new("fs.total_bytes", ts, total as f64).label("mount", m.clone()));
                if total > 0 {
                    let pct = used as f64 / total as f64 * 100.0;
                    out.push(Sample::new("fs.used_pct", ts, pct).label("mount", m.clone()));
                }
            }
        }
    }

    fn collect_facts(&self, ts: i64, out: &mut Vec<Sample>) {
        if let Some(text) = read("/proc/uptime") {
            if let Some(first) = text.split_whitespace().next() {
                if let Ok(v) = first.parse::<f64>() {
                    out.push(Sample::new("uptime.seconds", ts, v));
                }
            }
        }
        if let Some(text) = read("/proc/loadavg") {
            if let Some(first) = text.split_whitespace().next() {
                if let Ok(v) = first.parse::<f64>() {
                    // NOTE: lxcfs may not virtualise this; label kept honest by
                    // only ever showing it as advisory in the UI.
                    out.push(Sample::new("load.1m", ts, v));
                }
            }
        }
    }
}

fn read(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// How many CPUs this cgroup may actually use.
///
/// Order matters: PVE implements `cores=N` as a cpuset, so the effective set
/// is the honest denominator; a CFS quota (cpulimit) can only shrink it
/// further; nproc is the last resort for cgroups without either.
fn cpu_share() -> f64 {
    if let Some(text) = read(&format!("{CGROUP_ROOT}/cpuset.cpus.effective")) {
        if let Some(n) = count_cpus(text.trim()) {
            return n as f64;
        }
    }

    if let Some(text) = read(&format!("{CGROUP_ROOT}/cpu.max")) {
        let mut it = text.split_whitespace();
        let quota = it.next().unwrap_or("max");
        let period = it
            .next()
            .and_then(|p| p.parse::<f64>().ok())
            .unwrap_or(100_000.0);
        if let Ok(q) = quota.parse::<f64>() {
            if q > 0.0 && period > 0.0 {
                return (q / period).max(1.0);
            }
        }
    }

    read("/proc/cpuinfo")
        .map(|t| t.lines().filter(|l| l.starts_with("processor")).count())
        .filter(|n| *n > 0)
        .map(|n| n as f64)
        .unwrap_or(1.0)
}

/// Whether an interface's counters represent traffic leaving (or entering)
/// this guest, rather than a local duplicate of the same bytes.
fn is_uplink_iface(name: &str) -> bool {
    const VIRTUAL: [&str; 12] = [
        "lo", "docker", "veth", "br-", "vmbr", "virbr", "tun", "tap", "wg", "zt", "cni", "podman",
    ];
    if VIRTUAL.iter().any(|p| name.starts_with(p)) {
        return false;
    }
    !name.is_empty()
}

/// Kernel cpuset syntax: `"0-2,10,12"` is five CPUs.
fn count_cpus(spec: &str) -> Option<usize> {
    if spec.is_empty() || spec == "max" {
        return None;
    }
    let mut n = 0usize;
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part.split_once('-') {
            Some((a, b)) => {
                let a: usize = a.trim().parse().ok()?;
                let b: usize = b.trim().parse().ok()?;
                if b < a {
                    return None;
                }
                n += b - a + 1;
            }
            None => {
                part.parse::<usize>().ok()?;
                n += 1;
            }
        }
    }
    if n == 0 {
        None
    } else {
        Some(n)
    }
}

/// `key value` per line, the shape of cgroup v2 stat files.
fn parse_kv(text: &str) -> HashMap<String, u64> {
    text.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let k = it.next()?;
            let v = it.next()?.parse::<u64>().ok()?;
            Some((k.to_string(), v))
        })
        .collect()
}

fn statvfs(path: &str) -> Option<(u64, u64)> {
    let c = std::ffi::CString::new(path).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let frsize = st.f_frsize as u64;
    let total = (st.f_blocks as u64).saturating_mul(frsize);
    // f_bfree (not f_bavail) so `used` matches `df`'s Used column; the
    // difference is the root-reserved blocks df also counts as used.
    let free = (st.f_bfree as u64).saturating_mul(frsize);
    Some((total.saturating_sub(free), total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kv_parses_cgroup_stat() {
        let kv = parse_kv("anon 157286400\nfile 125829120\ninactive_file 1\n");
        assert_eq!(kv.get("anon"), Some(&157_286_400));
        assert_eq!(kv.get("file"), Some(&125_829_120));
    }

    #[test]
    fn net_dev_skips_loopback() {
        let mut c = Collector::new(vec![]);
        let s = c.collect(1000);
        // whatever the host has, lo must never appear
        assert!(!s
            .iter()
            .any(|x| x.labels.iter().any(|(k, v)| k == "iface" && v == "lo")));
    }

    #[test]
    fn cpuset_spec_counts_cpus() {
        assert_eq!(count_cpus("0-2,10,12"), Some(5));
        assert_eq!(count_cpus("6-8,11"), Some(4));
        assert_eq!(count_cpus("3"), Some(1));
        assert_eq!(count_cpus(""), None);
        assert_eq!(count_cpus("max"), None);
        assert_eq!(count_cpus("5-2"), None);
    }

    #[test]
    fn cpu_share_is_plausible() {
        // Real hosts always give something sane; this also guards the
        // division by zero that would poison every cpu.usage_pct sample.
        let s = cpu_share();
        assert!((1.0..=512.0).contains(&s), "cpu_share() = {s}");
    }
}
