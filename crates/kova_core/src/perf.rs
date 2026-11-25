//! Performance budget thresholds, process RSS probing, and budget evaluation.
//!
//! All constants mirror Section 2 of `jvm_rust_ide_plan.md` and live inside the
//! **Headless Core** so both headless tests and the GPUI binary share a single
//! source of truth without pulling any windowing or GPU dependencies.

use std::error::Error;
use std::fmt;
use std::time::Duration;

/// Canonical performance budget thresholds from Section 2 of `jvm_rust_ide_plan.md`.
pub mod budgets {
    use std::time::Duration;

    /// Maximum cold window startup latency until edit-ready (`< 120 ms`).
    pub const COLD_START_MAX: Duration = Duration::from_millis(120);

    /// Maximum resident set size (RSS) of the Kova process in MiB (`< 150 MiB`).
    pub const MAX_PROCESS_RSS_MIB: u64 = 150;

    /// Canonical maximum resident set size (RSS) of the Kova process in bytes (`150 MiB`).
    pub const MAX_PROCESS_RSS_BYTES: u64 = MAX_PROCESS_RSS_MIB * 1024 * 1024;

    /// Decimal (SI) reference threshold for `150 MB` in bytes (`150_000_000 B`).
    pub const MAX_PROCESS_RSS_SI_BYTES: u64 = 150_000_000;

    /// Maximum keystroke input latency on the edit path (`< 5 ms`).
    pub const INPUT_LATENCY_MAX: Duration = Duration::from_millis(5);

    /// Maximum time until *Go to Class* is ready after opening a project (`< 1.5 s`).
    pub const GO_TO_CLASS_READY_MAX: Duration = Duration::from_millis(1_500);

    /// Default inactivity window before auto-hibernating an idle `jdt.ls` process (`15 min`).
    pub const DEFAULT_JVM_HIBERNATION_IDLE: Duration = Duration::from_secs(15 * 60);
}

/// Snapshot of the current process's memory usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessMemorySnapshot {
    /// Current Resident Set Size (RSS) of this process in bytes.
    pub rss_bytes: u64,
    /// Current virtual memory footprint of this process in bytes (informational).
    pub virtual_bytes: u64,
}

impl ProcessMemorySnapshot {
    /// Returns the resident set size converted to MiB (`rss_bytes / 1024^2`).
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        reason = "RSS values in bytes fit well within 53-bit f64 mantissa precision for MiB reporting"
    )]
    pub fn rss_mib(&self) -> f64 {
        self.rss_bytes as f64 / (1024.0 * 1024.0)
    }
}

/// Error returned when querying OS process memory statistics fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryProbeError {
    /// The operating system or sandbox did not provide process memory statistics.
    Unavailable,
    /// The OS reported a zero RSS value, which would produce false-positive budget passes.
    ZeroResidentMemory,
}

impl fmt::Display for MemoryProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => {
                f.write_str("process memory statistics are unavailable on this platform/sandbox")
            }
            Self::ZeroResidentMemory => {
                f.write_str("process memory probe reported zero resident bytes")
            }
        }
    }
}

impl Error for MemoryProbeError {}

/// Samples the current process's physical (RSS) and virtual memory usage.
///
/// Uses `memory-stats` (`mach_task_basic_info` on macOS, `/proc/self/statm` on Linux)
/// without introducing any `unsafe` code in Kova.
///
/// # Errors
///
/// Returns [`MemoryProbeError::Unavailable`] if the underlying OS refuses or
/// cannot provide process memory statistics.
pub fn sample_process_memory() -> Result<ProcessMemorySnapshot, MemoryProbeError> {
    let stats = memory_stats::memory_stats().ok_or(MemoryProbeError::Unavailable)?;
    Ok(ProcessMemorySnapshot {
        rss_bytes: u64::try_from(stats.physical_mem).unwrap_or(u64::MAX),
        virtual_bytes: u64::try_from(stats.virtual_mem).unwrap_or(u64::MAX),
    })
}

/// Returns the current process's Resident Set Size (RSS) in bytes.
///
/// Rejects a `0` byte reading so budget checks cannot pass vacuously.
///
/// # Errors
///
/// Returns [`MemoryProbeError::Unavailable`] if OS statistics cannot be queried,
/// or [`MemoryProbeError::ZeroResidentMemory`] if the reported RSS is `0`.
pub fn current_process_rss_bytes() -> Result<u64, MemoryProbeError> {
    let snapshot = sample_process_memory()?;
    if snapshot.rss_bytes == 0 {
        return Err(MemoryProbeError::ZeroResidentMemory);
    }
    Ok(snapshot.rss_bytes)
}

/// Category of an enforceable Kova performance budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetKind {
    /// Cold window startup until edit-ready (`< 120 ms`).
    ColdStart,
    /// Resident Set Size of the Kova process (`< 150 MiB`).
    ProcessRssBytes,
    /// Keystroke input latency (`< 5 ms`).
    InputLatency,
    /// *Go to Class* readiness after project open (`< 1.5 s`).
    GoToClassReady,
}

impl BudgetKind {
    /// Returns the canonical upper limit in the metric's native unit (`ns` or `bytes`).
    #[must_use]
    pub const fn limit_value(&self) -> u64 {
        match self {
            Self::ColdStart => 120 * 1_000_000,
            Self::ProcessRssBytes => budgets::MAX_PROCESS_RSS_BYTES,
            Self::InputLatency => 5 * 1_000_000,
            Self::GoToClassReady => 1_500 * 1_000_000,
        }
    }

    /// Returns the unit label associated with this budget's scalar values (`"ns"` or `"bytes"`).
    #[must_use]
    pub const fn unit(&self) -> &'static str {
        match self {
            Self::ColdStart | Self::InputLatency | Self::GoToClassReady => "ns",
            Self::ProcessRssBytes => "bytes",
        }
    }
}

impl fmt::Display for BudgetKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::ColdStart => "cold_start",
            Self::ProcessRssBytes => "process_rss",
            Self::InputLatency => "input_latency",
            Self::GoToClassReady => "go_to_class_ready",
        };
        f.write_str(label)
    }
}

/// Outcome of comparing a measured value against a strict `< limit` budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetReport {
    /// Budget category evaluated.
    pub kind: BudgetKind,
    /// Measured value in `kind.unit()`.
    pub measured: u64,
    /// Upper threshold in `kind.unit()` (must be strictly greater than `measured`).
    pub limit: u64,
}

impl BudgetReport {
    /// Returns `true` when `measured` is strictly less than `limit` (`measured < limit`).
    #[must_use]
    pub const fn is_within_budget(&self) -> bool {
        self.measured < self.limit
    }

    /// Returns remaining headroom (`limit - measured`). Values `<= 0` indicate budget exhaustion.
    #[must_use]
    pub const fn margin(&self) -> i128 {
        (self.limit as i128) - (self.measured as i128)
    }
}

/// Builds a [`BudgetReport`] for a time-based [`BudgetKind`].
///
/// # Panics
///
/// Panics if `kind` is [`BudgetKind::ProcessRssBytes`], which must be checked via
/// [`check_rss_budget`].
#[must_use]
pub fn check_duration_budget(kind: BudgetKind, elapsed: Duration, limit: Duration) -> BudgetReport {
    assert!(
        kind != BudgetKind::ProcessRssBytes,
        "check_duration_budget requires a time-based BudgetKind, got ProcessRssBytes"
    );
    BudgetReport {
        kind,
        measured: u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX),
        limit: u64::try_from(limit.as_nanos()).unwrap_or(u64::MAX),
    }
}

/// Builds a [`BudgetReport`] comparing `rss_bytes` against [`budgets::MAX_PROCESS_RSS_BYTES`].
#[must_use]
pub const fn check_rss_budget(rss_bytes: u64) -> BudgetReport {
    BudgetReport {
        kind: BudgetKind::ProcessRssBytes,
        measured: rss_bytes,
        limit: budgets::MAX_PROCESS_RSS_BYTES,
    }
}

/// Asserts that `report` satisfies the strict `measured < limit` budget contract.
///
/// # Panics
///
/// Panics if `!report.is_within_budget()`, formatting the budget kind, measured
/// value, limit, and unit for diagnostics.
pub fn assert_within_budget(report: &BudgetReport) {
    assert!(
        report.is_within_budget(),
        "performance budget exceeded for {}: measured {} {} (limit < {} {})",
        report.kind,
        report.measured,
        report.kind.unit(),
        report.limit,
        report.kind.unit()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_constants_match_architecture_spec() {
        assert_eq!(budgets::COLD_START_MAX, Duration::from_millis(120));
        assert_eq!(budgets::MAX_PROCESS_RSS_MIB, 150);
        assert_eq!(budgets::MAX_PROCESS_RSS_BYTES, 150 * 1024 * 1024);
        assert_eq!(budgets::MAX_PROCESS_RSS_SI_BYTES, 150_000_000);
        assert_eq!(budgets::INPUT_LATENCY_MAX, Duration::from_millis(5));
        assert_eq!(budgets::GO_TO_CLASS_READY_MAX, Duration::from_millis(1_500));
        assert_eq!(
            budgets::DEFAULT_JVM_HIBERNATION_IDLE,
            Duration::from_secs(900)
        );
    }

    #[test]
    fn budget_kind_limits_match_constants() {
        assert_eq!(
            BudgetKind::ColdStart.limit_value(),
            u64::try_from(budgets::COLD_START_MAX.as_nanos()).unwrap()
        );
        assert_eq!(
            BudgetKind::ProcessRssBytes.limit_value(),
            budgets::MAX_PROCESS_RSS_BYTES
        );
        assert_eq!(
            BudgetKind::InputLatency.limit_value(),
            u64::try_from(budgets::INPUT_LATENCY_MAX.as_nanos()).unwrap()
        );
        assert_eq!(
            BudgetKind::GoToClassReady.limit_value(),
            u64::try_from(budgets::GO_TO_CLASS_READY_MAX.as_nanos()).unwrap()
        );
    }

    #[test]
    fn strict_inequality_boundaries_for_budget_report() {
        let limit = budgets::MAX_PROCESS_RSS_BYTES;

        let below = check_rss_budget(limit - 1);
        assert!(below.is_within_budget());
        assert_eq!(below.margin(), 1);

        let exact = check_rss_budget(limit);
        assert!(!exact.is_within_budget());
        assert_eq!(exact.margin(), 0);

        let above = check_rss_budget(limit + 1);
        assert!(!above.is_within_budget());
        assert_eq!(above.margin(), -1);
    }

    #[test]
    fn duration_budget_evaluation_and_assertion() {
        let within = check_duration_budget(
            BudgetKind::InputLatency,
            Duration::from_micros(4_999),
            budgets::INPUT_LATENCY_MAX,
        );
        assert!(within.is_within_budget());
        assert_within_budget(&within);

        let at_limit = check_duration_budget(
            BudgetKind::ColdStart,
            budgets::COLD_START_MAX,
            budgets::COLD_START_MAX,
        );
        assert!(!at_limit.is_within_budget());
    }

    #[test]
    #[should_panic(expected = "check_duration_budget requires a time-based BudgetKind")]
    fn check_duration_budget_rejects_rss_kind() {
        let _ = check_duration_budget(
            BudgetKind::ProcessRssBytes,
            Duration::from_millis(1),
            Duration::from_millis(10),
        );
    }

    #[test]
    #[should_panic(expected = "performance budget exceeded for input_latency")]
    fn assert_within_budget_panics_when_exhausted() {
        let report = check_duration_budget(
            BudgetKind::InputLatency,
            budgets::INPUT_LATENCY_MAX,
            budgets::INPUT_LATENCY_MAX,
        );
        assert_within_budget(&report);
    }

    #[test]
    fn display_formatting_for_kinds_and_errors() {
        assert_eq!(BudgetKind::ColdStart.to_string(), "cold_start");
        assert_eq!(BudgetKind::ProcessRssBytes.to_string(), "process_rss");
        assert_eq!(BudgetKind::InputLatency.to_string(), "input_latency");
        assert_eq!(BudgetKind::GoToClassReady.to_string(), "go_to_class_ready");
        assert_eq!(BudgetKind::ColdStart.unit(), "ns");
        assert_eq!(BudgetKind::ProcessRssBytes.unit(), "bytes");

        assert!(
            MemoryProbeError::Unavailable
                .to_string()
                .contains("unavailable")
        );
        assert!(
            MemoryProbeError::ZeroResidentMemory
                .to_string()
                .contains("zero")
        );
    }

    #[test]
    fn process_rss_probe_returns_positive_bytes() {
        let snapshot = sample_process_memory().expect("OS memory stats should be available");
        assert!(snapshot.rss_bytes > 0);
        assert!(snapshot.rss_mib() > 0.0);

        let rss = current_process_rss_bytes().expect("current RSS should be non-zero");
        assert!(rss > 0);
    }
}
