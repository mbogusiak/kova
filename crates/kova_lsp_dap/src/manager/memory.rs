//! RSS monitoring of `jdt.ls` child processes (KOVA-038, FR-1).
//!
//! [`OsProcessMemoryProbe`] measures the resident set size of a process by PID without `unsafe`
//! and without new dependencies: `VmRSS:` from `/proc/<pid>/status` on Linux (independent of the
//! kernel page size) and `ps -o rss= -p <pid>` on macOS. [`MockProcessMemoryProbe`] feeds
//! deterministic values to tests. [`MemoryPolicy`] maps a sample to a [`MemoryPressureLevel`]
//! relative to the configured `-Xmx` (`jdtls_max_ram_mb`).

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Mutex, PoisonError};

use kova_config::EffectiveConfig;
use tokio::time::Instant;

use crate::error::LspDapError;

/// Bytes in one MiB.
const MIB: u64 = 1024 * 1024;

/// Default soft multiplier of `-Xmx` (metaspace / GC / native overhead above the heap).
pub const DEFAULT_SOFT_RSS_MULTIPLIER: f64 = 1.25;
/// Default hard multiplier of `-Xmx`; above it the process is considered over budget.
pub const DEFAULT_HARD_RSS_MULTIPLIER: f64 = 1.50;

/// One RSS measurement of a child process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChildMemorySample {
    /// OS process id.
    pub pid: u32,
    /// Resident set size in bytes.
    pub rss_bytes: u64,
    /// When the sample was taken (Tokio clock, pausable in tests).
    pub sampled_at: Instant,
}

/// Memory pressure of a `jdt.ls` process relative to its budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MemoryPressureLevel {
    /// Below the soft limit.
    Normal,
    /// Between the soft and the hard limit.
    Warning,
    /// Above the hard limit.
    Exceeded,
}

/// RSS budget derived from `jdtls_max_ram_mb`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MemoryPolicy {
    /// Configured `-Xmx` of `jdt.ls` in MiB.
    pub configured_xmx_mb: u32,
    /// Multiplier of `-Xmx` above which the level is [`MemoryPressureLevel::Warning`].
    pub soft_rss_multiplier: f64,
    /// Multiplier of `-Xmx` above which the level is [`MemoryPressureLevel::Exceeded`].
    pub hard_rss_multiplier: f64,
}

impl Default for MemoryPolicy {
    fn default() -> Self {
        Self::new(kova_config::JvmSettings::default().jdtls_max_ram_mb)
    }
}

impl MemoryPolicy {
    /// Policy with the default multipliers for the given `-Xmx` (MiB).
    #[must_use]
    pub const fn new(configured_xmx_mb: u32) -> Self {
        Self {
            configured_xmx_mb,
            soft_rss_multiplier: DEFAULT_SOFT_RSS_MULTIPLIER,
            hard_rss_multiplier: DEFAULT_HARD_RSS_MULTIPLIER,
        }
    }

    /// Policy for `settings.jvm.jdtls_max_ram_mb` of the effective configuration.
    #[must_use]
    pub fn from_effective_config(effective: &EffectiveConfig) -> Self {
        Self::new(effective.settings.jvm.jdtls_max_ram_mb)
    }

    /// Configured heap in bytes.
    #[must_use]
    pub fn xmx_bytes(&self) -> u64 {
        u64::from(self.configured_xmx_mb) * MIB
    }

    /// RSS (bytes) above which the level is [`MemoryPressureLevel::Warning`].
    #[must_use]
    pub fn soft_limit_bytes(&self) -> u64 {
        scale_bytes(self.xmx_bytes(), self.soft_rss_multiplier)
    }

    /// RSS (bytes) above which the level is [`MemoryPressureLevel::Exceeded`].
    #[must_use]
    pub fn hard_limit_bytes(&self) -> u64 {
        scale_bytes(self.xmx_bytes(), self.hard_rss_multiplier).max(self.soft_limit_bytes())
    }

    /// Classifies an RSS measurement.
    #[must_use]
    pub fn evaluate_rss(&self, rss_bytes: u64) -> MemoryPressureLevel {
        if rss_bytes > self.hard_limit_bytes() {
            MemoryPressureLevel::Exceeded
        } else if rss_bytes > self.soft_limit_bytes() {
            MemoryPressureLevel::Warning
        } else {
            MemoryPressureLevel::Normal
        }
    }
}

/// `bytes * multiplier`, saturating; non-finite or non-positive multipliers fall back to `1.0`.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "byte budgets are far below 2^53; the float -> int cast saturates and the \
              multiplier is validated to be finite and positive"
)]
fn scale_bytes(bytes: u64, multiplier: f64) -> u64 {
    if !multiplier.is_finite() || multiplier <= 0.0 {
        return bytes;
    }
    (bytes as f64 * multiplier) as u64
}

/// Boxed future returned by [`ProcessMemoryProbe::sample_pid_rss`].
pub type ProbeFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<u64>, LspDapError>> + Send + 'a>>;

/// Measures the RSS of a process by PID.
pub trait ProcessMemoryProbe: Send + Sync + fmt::Debug {
    /// RSS of `pid` in bytes; `Ok(None)` when the process no longer exists.
    fn sample_pid_rss(&self, pid: u32) -> ProbeFuture<'_>;
}

/// Parses `VmRSS:` (KiB) from the contents of `/proc/<pid>/status` into bytes.
#[must_use]
pub fn parse_proc_status_vm_rss(status: &str) -> Option<u64> {
    status.lines().find_map(|line| {
        let rest = line.strip_prefix("VmRSS:")?;
        let kib: u64 = rest.split_whitespace().next()?.parse().ok()?;
        Some(kib.saturating_mul(1024))
    })
}

/// Parses the output of `ps -o rss= -p <pid>` (KiB) into bytes.
#[must_use]
pub fn parse_ps_rss_kib(output: &str) -> Option<u64> {
    let kib: u64 = output.split_whitespace().next()?.parse().ok()?;
    Some(kib.saturating_mul(1024))
}

/// OS-backed probe: `/proc/<pid>/status` on Linux, `ps` on macOS, `Ok(None)` elsewhere.
#[derive(Debug, Clone, Copy, Default)]
pub struct OsProcessMemoryProbe;

impl OsProcessMemoryProbe {
    /// Creates the probe.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl ProcessMemoryProbe for OsProcessMemoryProbe {
    fn sample_pid_rss(&self, pid: u32) -> ProbeFuture<'_> {
        Box::pin(sample_os_rss(pid))
    }
}

#[cfg(target_os = "linux")]
async fn sample_os_rss(pid: u32) -> Result<Option<u64>, LspDapError> {
    let path = format!("/proc/{pid}/status");
    match tokio::fs::read_to_string(&path).await {
        Ok(status) => Ok(parse_proc_status_vm_rss(&status)),
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidInput
            ) =>
        {
            Ok(None)
        }
        Err(err) => Err(LspDapError::io(path, &err)),
    }
}

#[cfg(target_os = "macos")]
async fn sample_os_rss(pid: u32) -> Result<Option<u64>, LspDapError> {
    let output = tokio::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|err| LspDapError::SpawnFailed {
            program: "ps".to_owned(),
            reason: err.to_string(),
        })?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(parse_ps_rss_kib(&String::from_utf8_lossy(&output.stdout)))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
async fn sample_os_rss(_pid: u32) -> Result<Option<u64>, LspDapError> {
    Ok(None)
}

/// In-memory probe for deterministic tests: returns the RSS set per PID (`None` when unset).
#[derive(Debug, Default)]
pub struct MockProcessMemoryProbe {
    values: Mutex<HashMap<u32, u64>>,
}

impl MockProcessMemoryProbe {
    /// Creates an empty probe.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the RSS reported for `pid`.
    pub fn set_rss(&self, pid: u32, rss_bytes: u64) {
        self.values
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(pid, rss_bytes);
    }

    /// Simulates the exit of `pid` (subsequent samples return `Ok(None)`).
    pub fn remove(&self, pid: u32) {
        self.values
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&pid);
    }
}

impl ProcessMemoryProbe for MockProcessMemoryProbe {
    fn sample_pid_rss(&self, pid: u32) -> ProbeFuture<'_> {
        let value = self
            .values
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&pid)
            .copied();
        Box::pin(async move { Ok(value) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_limits_follow_multipliers() {
        let policy = MemoryPolicy::new(2048);
        assert_eq!(policy.xmx_bytes(), 2048 * MIB);
        assert_eq!(policy.soft_limit_bytes(), 2560 * MIB);
        assert_eq!(policy.hard_limit_bytes(), 3072 * MIB);
        assert_eq!(policy.evaluate_rss(2048 * MIB), MemoryPressureLevel::Normal);
        assert_eq!(
            policy.evaluate_rss(2600 * MIB),
            MemoryPressureLevel::Warning
        );
        assert_eq!(
            policy.evaluate_rss(3073 * MIB),
            MemoryPressureLevel::Exceeded
        );
    }

    #[test]
    fn invalid_multipliers_fall_back_to_xmx() {
        let policy = MemoryPolicy {
            configured_xmx_mb: 256,
            soft_rss_multiplier: f64::NAN,
            hard_rss_multiplier: -1.0,
        };
        assert_eq!(policy.soft_limit_bytes(), 256 * MIB);
        assert_eq!(policy.hard_limit_bytes(), 256 * MIB);
    }

    #[test]
    fn policy_reads_effective_config() {
        let mut effective = EffectiveConfig::merge(&kova_config::KovaSettings::default(), None);
        effective.settings.jvm.jdtls_max_ram_mb = 512;
        assert_eq!(
            MemoryPolicy::from_effective_config(&effective).configured_xmx_mb,
            512
        );
        assert_eq!(MemoryPolicy::default().configured_xmx_mb, 2048);
    }

    #[test]
    fn parses_proc_status_and_ps_output() {
        let status = "Name:\tjava\nVmPeak:\t  999 kB\nVmRSS:\t  123456 kB\nThreads:\t42\n";
        assert_eq!(parse_proc_status_vm_rss(status), Some(123_456 * 1024));
        assert_eq!(parse_proc_status_vm_rss("Name:\tzombie\n"), None);
        assert_eq!(parse_ps_rss_kib("  20480\n"), Some(20_480 * 1024));
        assert_eq!(parse_ps_rss_kib(""), None);
        assert_eq!(parse_ps_rss_kib("RSS\n"), None);
    }

    #[tokio::test]
    async fn mock_probe_reports_and_forgets() {
        let probe = MockProcessMemoryProbe::new();
        assert_eq!(probe.sample_pid_rss(7).await, Ok(None));
        probe.set_rss(7, 42);
        assert_eq!(probe.sample_pid_rss(7).await, Ok(Some(42)));
        probe.remove(7);
        assert_eq!(probe.sample_pid_rss(7).await, Ok(None));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn os_probe_measures_current_process() {
        let rss = OsProcessMemoryProbe::new()
            .sample_pid_rss(std::process::id())
            .await;
        assert!(matches!(rss, Ok(Some(bytes)) if bytes > 0), "{rss:?}");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn os_probe_returns_none_for_missing_process() {
        // PIDs are bounded well below `u32::MAX / 2` on Linux and macOS.
        let rss = OsProcessMemoryProbe::new()
            .sample_pid_rss(u32::MAX / 2)
            .await;
        assert_eq!(rss, Ok(None));
    }
}
