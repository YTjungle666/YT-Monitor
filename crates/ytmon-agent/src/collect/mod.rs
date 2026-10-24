//! Platform dispatch for metric collection.
//!
//! Both implementations expose the same shape:
//!
//!     Collector::new(mounts) -> Collector
//!     collector.collect(ts) -> Vec<Sample>
//!
//! Linux reads cgroup v2 (see linux.rs for why /proc/meminfo is unusable on
//! this fleet); Windows goes through sysinfo, because there is no cgroup and
//! the Windows API already reports real values.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::Collector;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::Collector;
