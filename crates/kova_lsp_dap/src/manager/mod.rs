//! Multi-project `jdt.ls` session management: RSS limits and auto-hibernation (KOVA-038).
//!
//! - [`memory`]: RSS sampling of child processes and the [`MemoryPolicy`] budget,
//! - [`hibernation`]: atomic `hibernated_state.json` persistence in the workspace cache,
//! - [`session_manager`]: the [`JdtlsSessionManager`] state machine, watchdog and single-flight wake.

pub mod hibernation;
pub mod memory;
pub mod session_manager;

pub use hibernation::{
    HIBERNATED_STATE_FILE, HIBERNATION_SCHEMA_VERSION, HibernatedOpenDocument,
    HibernationStateSnapshot, HibernationStore,
};
pub use memory::{
    ChildMemorySample, DEFAULT_HARD_RSS_MULTIPLIER, DEFAULT_SOFT_RSS_MULTIPLIER, MemoryPolicy,
    MemoryPressureLevel, MockProcessMemoryProbe, OsProcessMemoryProbe, ProbeFuture,
    ProcessMemoryProbe, parse_proc_status_vm_rss, parse_ps_rss_kib,
};
pub use session_manager::{
    ActivityKind, DEFAULT_SHUTDOWN_TIMEOUT, DEFAULT_WATCHDOG_INTERVAL, JdtlsLauncher,
    JdtlsSessionManager, LaunchFuture, LaunchedJdtls, OpenBuffer, OpenBufferProvider, ProjectId,
    ProjectSessionConfig, ProjectSessionPhase, SESSION_EVENT_CAPACITY, SessionEvent,
    SessionManagerConfig, SpecJdtlsLauncher, WatchdogHandle, WatchdogReport,
};
