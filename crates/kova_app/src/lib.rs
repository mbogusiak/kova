//! `kova_app` — startup binary, GPUI initialization and event loop for Kova IDE.
//!
//! This crate is the application entry point and one of the only two crates
//! allowed to depend on GPU windowing (`kova_ui` and `kova_app`).

use std::io::Write as _;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use gpui::{
    App, AppContext as _, Application, Bounds, TitlebarOptions, WindowBackgroundAppearance,
    WindowBounds, WindowOptions, point, px, size,
};
use kova_ui::{KovaTheme, StartupMetrics, WorkspaceShell};

/// Name of this crate as declared in its `Cargo.toml`.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

const DEFAULT_WINDOW_WIDTH_PX: f32 = 1280.0;
const DEFAULT_WINDOW_HEIGHT_PX: f32 = 800.0;
const MIN_WINDOW_WIDTH_PX: f32 = 640.0;
const MIN_WINDOW_HEIGHT_PX: f32 = 400.0;

/// Launch configuration options controlling startup probing and budget enforcement.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AppLaunchOptions {
    /// Exit cleanly right after the first painted frame has been recorded.
    pub exit_after_first_frame: bool,
    /// Return a failing [`ExitCode`] if cold-start or RSS budgets are exceeded.
    pub assert_budgets: bool,
}

impl AppLaunchOptions {
    /// Parses [`AppLaunchOptions`] from process environment variables and CLI arguments.
    ///
    /// Supported triggers:
    /// - `KOVA_STARTUP_PROBE=1` or `--probe-startup`: sets `exit_after_first_frame = true`
    /// - `KOVA_ASSERT_BUDGETS=1` or `--assert-startup-budget`: sets `assert_budgets = true`
    #[must_use]
    pub fn from_env_and_args() -> Self {
        let probe_env = env_flag_enabled("KOVA_STARTUP_PROBE");
        let assert_env = env_flag_enabled("KOVA_ASSERT_BUDGETS");

        let mut probe_arg = false;
        let mut assert_arg = false;
        for arg in std::env::args().skip(1) {
            if arg == "--probe-startup" {
                probe_arg = true;
            } else if arg == "--assert-startup-budget" {
                assert_arg = true;
            }
        }

        Self {
            exit_after_first_frame: probe_env || probe_arg,
            assert_budgets: assert_env || assert_arg,
        }
    }
}

fn env_flag_enabled(key: &str) -> bool {
    std::env::var(key).is_ok_and(|value| {
        let trimmed = value.trim();
        trimmed == "1" || trimmed.eq_ignore_ascii_case("true")
    })
}

/// Starts the Kova IDE desktop application using process environment and CLI options.
pub fn run() -> ExitCode {
    run_with_options(Instant::now(), AppLaunchOptions::from_env_and_args())
}

/// Starts the Kova IDE GPUI application using a caller-supplied `process_start` timestamp.
pub fn run_with_options(process_start: Instant, options: AppLaunchOptions) -> ExitCode {
    let report_cell = Arc::new(OnceLock::<StartupMetrics>::new());
    let report_for_shell = Arc::clone(&report_cell);

    let probe_printed = Arc::new(AtomicBool::new(false));
    let probe_printed_for_shell = Arc::clone(&probe_printed);

    let open_failed = Arc::new(AtomicBool::new(false));
    let open_failed_for_app = Arc::clone(&open_failed);

    Application::new().run(move |cx: &mut App| {
        cx.set_global(KovaTheme::islands_dark());

        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let open_result = cx.open_window(default_window_options(cx), move |_, cx| {
            cx.activate(true);
            cx.new(move |_| {
                WorkspaceShell::with_probe(
                    process_start,
                    options.exit_after_first_frame,
                    Some(Box::new(move |metrics| {
                        handle_first_frame_report(
                            metrics,
                            options,
                            &report_for_shell,
                            &probe_printed_for_shell,
                        );
                    })),
                )
            })
        });

        if let Err(err) = open_result {
            open_failed_for_app.store(true, Ordering::Release);
            write_stderr(&format!("failed to open Kova window: {err}"));
            #[cfg(target_os = "macos")]
            {
                #[allow(
                    clippy::exit,
                    reason = "NSApplication::terminate on macOS exits with 0 and never returns to run_with_options"
                )]
                std::process::exit(1);
            }
            #[cfg(not(target_os = "macos"))]
            cx.quit();
        }
    });

    finalize_launch(
        options,
        open_failed.load(Ordering::Acquire),
        report_cell.get(),
        &probe_printed,
    )
}

fn default_window_options(cx: &App) -> WindowOptions {
    let default_size = size(px(DEFAULT_WINDOW_WIDTH_PX), px(DEFAULT_WINDOW_HEIGHT_PX));
    let bounds = Bounds::centered(None, default_size, cx);

    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some("Kova".into()),
            appears_transparent: true,
            traffic_light_position: Some(point(px(12.0), px(10.0))),
        }),
        focus: true,
        show: true,
        window_background: WindowBackgroundAppearance::Opaque,
        app_id: Some("dev.kova.ide".to_owned()),
        window_min_size: Some(size(px(MIN_WINDOW_WIDTH_PX), px(MIN_WINDOW_HEIGHT_PX))),
        ..Default::default()
    }
}

fn handle_first_frame_report(
    metrics: &StartupMetrics,
    options: AppLaunchOptions,
    report_cell: &OnceLock<StartupMetrics>,
    probe_printed: &AtomicBool,
) {
    drop(report_cell.set(metrics.clone()));
    if options.exit_after_first_frame && !probe_printed.swap(true, Ordering::AcqRel) {
        write_stdout(&metrics.format_probe_line());
    }
    if options.assert_budgets && !metrics.is_within_budgets() {
        write_stderr(&format!(
            "performance budget check failed: {}",
            metrics.format_probe_line()
        ));
        #[cfg(target_os = "macos")]
        if options.exit_after_first_frame {
            #[allow(
                clippy::exit,
                reason = "NSApplication::terminate on macOS exits with 0 and never returns to run_with_options"
            )]
            std::process::exit(1);
        }
    }
}

fn finalize_launch(
    options: AppLaunchOptions,
    open_failed: bool,
    report: Option<&StartupMetrics>,
    probe_printed: &AtomicBool,
) -> ExitCode {
    if open_failed {
        return ExitCode::FAILURE;
    }

    if options.exit_after_first_frame {
        if let Some(metrics) = report {
            if !probe_printed.swap(true, Ordering::AcqRel) {
                write_stdout(&metrics.format_probe_line());
            }
        } else {
            write_stderr("startup probe exited before first frame completed");
            return ExitCode::FAILURE;
        }
    }

    if options.assert_budgets && !report.is_some_and(StartupMetrics::is_within_budgets) {
        if let Some(metrics) = report {
            write_stderr(&format!(
                "performance budget check failed: {}",
                metrics.format_probe_line()
            ));
        }
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

fn write_stdout(line: &str) {
    let mut stdout = std::io::stdout().lock();
    drop(writeln!(stdout, "{line}"));
    drop(stdout.flush());
}

fn write_stderr(line: &str) {
    let mut stderr = std::io::stderr().lock();
    drop(writeln!(stderr, "{line}"));
    drop(stderr.flush());
}

#[cfg(test)]
mod tests {
    use std::process::ExitCode;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    use kova_ui::StartupMetrics;

    use super::{AppLaunchOptions, CRATE_NAME, finalize_launch};

    #[test]
    fn crate_name_matches_manifest() {
        assert_eq!(CRATE_NAME, "kova_app");
    }

    #[test]
    fn inherits_workspace_license() {
        assert_eq!(env!("CARGO_PKG_LICENSE"), "MIT OR Apache-2.0");
    }

    #[test]
    fn launch_options_default_to_interactive_mode() {
        let defaults = AppLaunchOptions::default();
        assert!(!defaults.exit_after_first_frame);
        assert!(!defaults.assert_budgets);
    }

    #[test]
    fn finalize_launch_enforces_probe_and_budget_contracts() {
        let printed = AtomicBool::new(true);
        let mut within = StartupMetrics::new();
        within.record_first_frame(Duration::from_millis(45), Some(48 * 1024 * 1024));

        let probe_assert = AppLaunchOptions {
            exit_after_first_frame: true,
            assert_budgets: true,
        };
        assert_eq!(
            finalize_launch(probe_assert, false, Some(&within), &printed),
            ExitCode::SUCCESS
        );
        assert_eq!(
            finalize_launch(probe_assert, true, Some(&within), &printed),
            ExitCode::FAILURE
        );
        assert_eq!(
            finalize_launch(probe_assert, false, None, &printed),
            ExitCode::FAILURE
        );

        let mut over_budget = StartupMetrics::new();
        over_budget.record_first_frame(Duration::from_millis(140), Some(48 * 1024 * 1024));
        assert_eq!(
            finalize_launch(probe_assert, false, Some(&over_budget), &printed),
            ExitCode::FAILURE
        );
    }
}
