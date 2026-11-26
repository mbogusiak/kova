//! Cold-start and first-frame GPUI smoke test (requires an active GPU display server).
//!
//! Launches the compiled `kova` binary as a child process so `NSApplication` / GPUI
//! initializes on the OS main thread rather than a `libtest` worker thread.
//!
//! Run locally on a macOS Metal or Linux Vulkan host with:
//! `cargo test --release -p kova_app --test cold_start_smoke -- --ignored --nocapture`

use std::process::Command;

#[test]
#[ignore = "requires active GPU display server"]
fn opens_gpui_window_and_closes_within_budgets() {
    let bin = env!("CARGO_BIN_EXE_kova");
    // Warm-up run (result ignored): the first launch of a freshly linked binary pays a
    // one-off macOS XProtect / code-signing scan that is not part of Kova's cold start.
    drop(
        Command::new(bin)
            .env("KOVA_STARTUP_PROBE", "1")
            .env_remove("KOVA_ASSERT_BUDGETS")
            .output(),
    );

    let Ok(output) = Command::new(bin)
        .env("KOVA_STARTUP_PROBE", "1")
        .env("KOVA_ASSERT_BUDGETS", "1")
        .output()
    else {
        panic!("failed to execute kova binary at {bin}");
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "kova startup probe failed (status={:?})\nstdout: {stdout}\nstderr: {stderr}",
        output.status.code()
    );
    assert!(
        stdout.contains("within_budget=true"),
        "expected within_budget=true in probe output\nstdout: {stdout}\nstderr: {stderr}"
    );
}
