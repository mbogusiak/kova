//! Cold-start and first-frame GPUI smoke test (requires an active GPU display server).
//!
//! Run locally on a macOS Metal or Linux Vulkan host with:
//! `cargo test -p kova_app --test cold_start_smoke -- --ignored --nocapture`

use std::process::ExitCode;
use std::time::Instant;

use kova_app::{AppLaunchOptions, run_with_options};

#[test]
#[ignore = "requires active GPU display server"]
fn opens_gpui_window_and_closes_within_budgets() {
    let start = Instant::now();
    let code = run_with_options(
        start,
        AppLaunchOptions {
            exit_after_first_frame: true,
            assert_budgets: true,
        },
    );
    assert_eq!(code, ExitCode::SUCCESS);
}
