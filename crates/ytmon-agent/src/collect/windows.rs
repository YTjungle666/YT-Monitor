//! Windows collection through `sysinfo`.
//!
//! There is no cgroup on Windows and the OS APIs already report real values,
//! so this path is much simpler than the Linux one. Metric names are shared
//! where the meaning matches (mem.used_bytes, net.rx_bytes, fs.total_bytes);
//! CPU is a gauge here because Windows reports utilisation, not a cumulative
//! counter, and pretending otherwise would produce nonsense after a diff.

use sysinfo::{Disks, Networks, ProcessesToUpdate, System};
use ytmon_core::Sample;

pub struct Collector {
    sys: System,
    disks: Disks,
    nets: Networks,
}

impl Collector {
    pub fn new(_mounts: Vec<String>) -> Self {
        let mut sys = System::new();
        sys.refresh_memory();
        sys.refresh_cpu_usage();
        Self {
            sys,
            disks: Disks::new_with_refreshed_list(),
            nets: Networks::new_with_refreshed_list(),
        }
    }

    pub fn collect(&mut self, ts: i64) -> Vec<Sample> {
        let mut out = Vec::with_capacity(24);

        self.sys.refresh_memory();
        self.sys.refresh_cpu_usage();
        self.sys.refresh_processes(ProcessesToUpdate::All, true);

        out.push(Sample::new(
            "mem.used_bytes",
            ts,
            self.sys.used_memory() as f64,
        ));
        out.push(Sample::new(
            "mem.total_bytes",
            ts,
            self.sys.total_memory() as f64,
        ));
        // global_cpu_usage() is already a 0-100 machine-wide average, which is
        // the same unit the Linux agent reports (share of the CPUs the guest
        // owns). Do not scale it by core count.
        out.push(Sample::new(
            "cpu.usage_pct",
            ts,
            f64::from(self.sys.global_cpu_usage()),
        ));
        let total_mem = self.sys.total_memory();
        if total_mem > 0 {
            let pct = self.sys.used_memory() as f64 / total_mem as f64 * 100.0;
            out.push(Sample::new("mem.used_pct", ts, pct));
        }
        out.push(Sample::new(
            "proc.count",
            ts,
            self.sys.processes().len() as f64,
        ));
        // `uptime` is an associated function in sysinfo 0.33, not a method.
        out.push(Sample::new("uptime.seconds", ts, System::uptime() as f64));

        self.disks.refresh(true);
        for d in self.disks.list() {
            let total = d.total_space();
            if total == 0 {
                continue;
            }
            let mount = d.mount_point().to_string_lossy().to_string();
            let used = total.saturating_sub(d.available_space());
            out.push(Sample::new("fs.used_bytes", ts, used as f64).label("mount", mount.clone()));
            out.push(Sample::new("fs.total_bytes", ts, total as f64).label("mount", mount.clone()));
            out.push(
                Sample::new("fs.used_pct", ts, used as f64 / total as f64 * 100.0)
                    .label("mount", mount),
            );
        }

        self.nets.refresh(true);
        for (name, data) in self.nets.iter() {
            out.push(
                Sample::new("net.rx_bytes", ts, data.total_received() as f64)
                    .label("iface", name.clone()),
            );
            out.push(
                Sample::new("net.tx_bytes", ts, data.total_transmitted() as f64)
                    .label("iface", name.clone()),
            );
        }

        out
    }
}
