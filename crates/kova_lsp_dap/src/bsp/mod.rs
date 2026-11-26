//! Build Server Protocol client for Gradle / Maven (`KOVA-040`).
//!
//! - [`client`]: `.bsp/*.json` discovery, [`BspClient`] lifecycle (`build/initialize`,
//!   `workspace/buildTargets`, `buildTarget/compile`, `buildTarget/test`, `build/shutdown`),
//! - [`events`]: the [`BspEventMapper`] adapter from `build/task*`, `build/logMessage` and
//!   `build/publishDiagnostics` notifications to [`BspEvent`]s,
//! - [`runner`]: [`run_tests`], choosing BSP or the build tool command line with the
//!   `TEST-*.xml` fallback.

pub mod client;
pub mod events;
pub mod runner;

pub use client::{
    BSP_VERSION, BspClient, BspConnectionDetails, BspTestParams, BuildTarget,
    BuildTargetCapabilities, BuildTargetIdentifier, select_test_targets, test_params_json,
};
pub use events::{BspEvent, BspEventMapper, BspMessageLevel, BspStatusCode};
pub use runner::{
    BuildBackend, BuildRequest, TestBackend, TestRunRequest, module_report_dirs,
    plan_build_backend, plan_test_backend, run_build, run_over_bsp, run_tests, run_with_backend,
};
