//! `kova_app` — startup binary, GPUI initialization and event loop for Kova IDE.
//!
//! This crate is the application entry point and one of the only two crates
//! allowed to depend on GPU windowing (`kova_ui` and `kova_app`).
//!
//! Phase 0 integration (`KOVA-016`): CLI file opening ([`cli`]), non-blocking load and
//! atomic `Cmd+S` / `Ctrl+S` saves on a 2-worker `tokio` runtime ([`io`]), and live theme
//! reload from `settings.toml` through `kova_config::ConfigWatcher` ([`theme_watch`]).
//! All background work starts only after the first frame, keeping cold start `< 120 ms`.

pub mod cli;
pub mod debug_sessions;
pub mod definition;
pub mod io;
pub mod menus;
pub mod test_runs;
pub mod theme_watch;

use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use gpui::{
    App, AppContext as _, Application, AsyncApp, Bounds, TitlebarOptions,
    WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowOptions, point, px, size,
};
use kova_ui::settings::{SettingsWriteRequest, SettingsWriteSink};
use kova_ui::{KovaTheme, SaveRequest, SaveSink, StartupMetrics, StatusKind, WorkspaceShell};
use tokio::runtime::{Handle, Runtime};
use tokio::sync::{mpsc, oneshot};

use crate::cli::FileTarget;
use crate::io::{FileLoadError, LoadedFile};
use crate::theme_watch::ThemeWatchEvent;

/// Name of this crate as declared in its `Cargo.toml`.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

/// Number of `tokio` worker threads (file I/O + config watcher) kept small for the RAM budget.
pub const IO_WORKER_THREADS: usize = 2;

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

/// What the window should open with, as parsed from the command line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchRequest {
    /// File to open (`None` keeps the built-in welcome document).
    pub target: Option<FileTarget>,
    /// Project directory shown in the *Project* dock (`kova <project-dir>`, `KOVA-025`).
    pub project_root: Option<PathBuf>,
    /// Configuration directory override (`--config-dir`).
    pub config_dir_override: Option<PathBuf>,
}

impl LaunchRequest {
    /// Builds the request from the parsed positional argument: an existing directory becomes
    /// the project root (`kova <project-dir>`), anything else the file to open.
    #[must_use]
    pub fn from_cli(target: Option<FileTarget>, config_dir_override: Option<PathBuf>) -> Self {
        match target {
            Some(dir) if dir.path.is_dir() => Self {
                target: None,
                project_root: Some(dir.path),
                config_dir_override,
            },
            target => Self {
                target,
                project_root: None,
                config_dir_override,
            },
        }
    }
}

/// Results delivered from `tokio` tasks to the GPUI foreground thread.
#[derive(Debug)]
pub enum AppEvent {
    /// The CLI file finished loading (or failed to load).
    FileLoaded {
        /// The requested file and cursor position.
        target: FileTarget,
        /// Load outcome.
        result: Result<LoadedFile, FileLoadError>,
    },
    /// A background save finished.
    SaveFinished {
        /// Ticket of the save request.
        ticket: kova_ui::SaveTicket,
        /// `Err` carries a human-readable I/O error.
        result: Result<(), String>,
    },
    /// Theme or configuration update from the config watcher.
    Theme(ThemeWatchEvent),
    /// A background `settings.toml` write from the Settings view finished (`KOVA-029`).
    SettingsWritten(Result<(), String>),
    /// The configuration watcher could not be started.
    ConfigUnavailable(String),
    /// A batch of `JUnit` *Test Runner* events for run `run_id` (`KOVA-040`).
    TestEvents {
        /// Run identifier from the shell's request.
        run_id: u64,
        /// Events in arrival order.
        events: Vec<kova_tools::junit::TestEvent>,
    },
    /// Compile diagnostics of one file reported by the build server (`KOVA-040`).
    BuildProblems {
        /// Project-relative file.
        file: String,
        /// Problems (empty clears the file).
        problems: Vec<kova_ui::ProblemEntry>,
    },
    /// The `Recent Projects` list changed or finished loading (`KOVA-049`).
    RecentProjects(Result<Vec<kova_core::RecentProject>, String>),
    /// An update of JVM debug session `session_id` (`KOVA-041`).
    Debug {
        /// Session identifier from the shell's start request.
        session_id: u64,
        /// Update for the *Debugger* dock.
        update: kova_ui::DebugUpdate,
    },
    /// *Go to Definition* resolution completed (`KOVA-051`).
    DefinitionResolved {
        /// Request identifier for stale-reply filtering.
        request_id: u64,
        /// Resolution outcome.
        outcome: kova_ui::DefinitionOutcome,
    },
}

/// Builds the `tokio` runtime used for editor I/O ([`IO_WORKER_THREADS`] workers).
///
/// # Errors
///
/// Returns the OS error when worker threads cannot be spawned.
pub fn build_io_runtime() -> std::io::Result<Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(IO_WORKER_THREADS)
        .max_blocking_threads(IO_WORKER_THREADS * 2)
        .thread_name("kova-io")
        .enable_all()
        .build()
}

/// Creates the [`SaveSink`] that writes snapshots atomically on `runtime` and reports
/// [`AppEvent::SaveFinished`] through `events`.
#[must_use]
pub fn save_sink(runtime: Handle, events: mpsc::UnboundedSender<AppEvent>) -> SaveSink {
    Arc::new(move |request: SaveRequest| {
        let events = events.clone();
        runtime.spawn(async move {
            let SaveRequest {
                path,
                snapshot,
                ticket,
            } = request;
            let result = io::atomic_write_snapshot(&path, snapshot)
                .await
                .map_err(|err| err.to_string());
            drop(events.send(AppEvent::SaveFinished { ticket, result }));
        });
    })
}

/// Creates the [`SettingsWriteSink`] used by the Settings view (`KOVA-029`).
///
/// Writes are serialized on one `tokio` task that always persists the latest request
/// (older queued edits are superseded), so `settings.toml` can never end up with an older
/// document than the GUI shows. Each write is atomic (`SettingsDocument::save_to_file`) and
/// runs on a blocking thread; the result comes back as [`AppEvent::SettingsWritten`].
#[must_use]
pub fn settings_write_sink(
    runtime: &Handle,
    events: mpsc::UnboundedSender<AppEvent>,
) -> SettingsWriteSink {
    let (tx, mut rx) = mpsc::unbounded_channel::<SettingsWriteRequest>();
    runtime.spawn(async move {
        while let Some(mut request) = rx.recv().await {
            while let Ok(newer) = rx.try_recv() {
                request = newer;
            }
            let result = tokio::task::spawn_blocking(move || {
                request
                    .document
                    .save_to_file(&request.path)
                    .map_err(|err| err.to_string())
            })
            .await
            .unwrap_or_else(|err| Err(err.to_string()));
            if events.send(AppEvent::SettingsWritten(result)).is_err() {
                break;
            }
        }
    });
    Arc::new(move |request: SettingsWriteRequest| {
        drop(tx.send(request));
    })
}

/// Starts the post-first-frame background work: loading the CLI file and the
/// configuration watcher bridge. Never blocks the caller.
pub fn spawn_background_work(
    runtime: &Handle,
    request: LaunchRequest,
    events: &mpsc::UnboundedSender<AppEvent>,
) {
    let recent = request.project_root.clone().map_or(
        menus::RecentProjectsUpdate::Load,
        menus::RecentProjectsUpdate::Record,
    );
    menus::spawn_recent_update(runtime, events, recent);

    if let Some(target) = request.target {
        let events = events.clone();
        runtime.spawn(async move {
            let result = io::load_file_for_editor(&target.path).await;
            drop(events.send(AppEvent::FileLoaded { target, result }));
        });
    }

    let events = events.clone();
    let config_dir = request.config_dir_override;
    runtime.spawn(async move {
        let paths = match theme_watch::resolve_paths(config_dir.as_deref()) {
            Ok(paths) => paths,
            Err(err) => {
                drop(events.send(AppEvent::ConfigUnavailable(err.to_string())));
                return;
            }
        };
        let (theme_tx, mut theme_rx) = mpsc::unbounded_channel();
        // The watcher's initial scan is synchronous disk I/O: keep it off the async workers.
        let started =
            tokio::task::spawn_blocking(move || theme_watch::start(paths, theme_tx)).await;
        let watch = match started {
            Ok(Ok(watch)) => watch,
            Ok(Err(err)) => {
                drop(events.send(AppEvent::ConfigUnavailable(err.to_string())));
                return;
            }
            Err(err) => {
                drop(events.send(AppEvent::ConfigUnavailable(err.to_string())));
                return;
            }
        };
        // Keeps `watch` alive for as long as the app listens for events.
        while let Some(event) = theme_rx.recv().await {
            if events.send(AppEvent::Theme(event)).is_err() {
                break;
            }
        }
        drop(watch);
    });
}

/// Applies one [`AppEvent`] to the workspace shell on the GPUI thread.
pub fn handle_app_event(
    shell: &mut WorkspaceShell,
    event: AppEvent,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<'_, WorkspaceShell>,
) {
    match event {
        AppEvent::FileLoaded { target, result } => match result {
            Ok(LoadedFile::Existing { path, buffer }) => {
                shell.open_document(path, Some(buffer), target.line, target.column, window, cx);
            }
            Ok(LoadedFile::NewFile { path }) => {
                shell.open_document(path, None, target.line, target.column, window, cx);
            }
            Err(err) => shell.report_open_error(err.to_string(), cx),
        },
        AppEvent::SaveFinished { ticket, result } => shell.finish_save(ticket, result, cx),
        AppEvent::Theme(ThemeWatchEvent::ThemeChanged(theme)) => {
            kova_ui::apply_live_theme(theme, cx);
            shell.clear_notice(cx);
        }
        AppEvent::Theme(ThemeWatchEvent::ConfigReloaded) => shell.clear_notice(cx),
        AppEvent::Theme(ThemeWatchEvent::Warning(message)) => {
            shell.set_notice(StatusKind::Warning, message, cx);
        }
        AppEvent::Theme(ThemeWatchEvent::Settings(source)) => {
            shell.update_settings_source(source, cx);
        }
        AppEvent::Theme(ThemeWatchEvent::SettingsInvalid(message)) => {
            shell.report_settings_error(message, cx);
        }
        AppEvent::SettingsWritten(result) => shell.finish_settings_write(result, cx),
        AppEvent::ConfigUnavailable(message) => shell.set_notice(
            StatusKind::Warning,
            format!("Live config reload unavailable: {message}"),
            cx,
        ),
        AppEvent::TestEvents { run_id, events } => shell.apply_test_events(run_id, events, cx),
        AppEvent::BuildProblems { file, problems } => {
            shell.set_build_problems(&file, problems, cx);
        }
        AppEvent::Debug { session_id, update } => {
            shell.apply_debug_update(session_id, update, window, cx);
        }
        AppEvent::RecentProjects(Ok(recent)) => menus::refresh_menus(&recent, cx),
        AppEvent::RecentProjects(Err(message)) => shell.set_notice(
            StatusKind::Warning,
            format!("Recent Projects unavailable: {message}"),
            cx,
        ),
        AppEvent::DefinitionResolved {
            request_id,
            outcome,
        } => shell.apply_definition_result(request_id, outcome, window, cx),
    }
}

async fn pump_app_events(
    window: WindowHandle<WorkspaceShell>,
    first_frame: oneshot::Receiver<()>,
    runtime: Handle,
    request: LaunchRequest,
    events_tx: mpsc::UnboundedSender<AppEvent>,
    mut events_rx: mpsc::UnboundedReceiver<AppEvent>,
    cx: &mut AsyncApp,
) {
    if first_frame.await.is_err() {
        return;
    }
    spawn_background_work(&runtime, request, &events_tx);
    drop(events_tx);
    while let Some(event) = events_rx.recv().await {
        let applied = window.update(cx, |shell, window, cx| {
            handle_app_event(shell, event, window, cx);
        });
        if applied.is_err() {
            break;
        }
    }
}

/// Starts the Kova IDE GPUI application using a caller-supplied `process_start` timestamp.
pub fn run_with_options(process_start: Instant, options: AppLaunchOptions) -> ExitCode {
    run_cli(process_start, options, LaunchRequest::default())
}

/// Starts Kova with an explicit [`LaunchRequest`] parsed from the command line.
///
/// The window opens immediately with the built-in theme and an empty buffer; the CLI
/// file and `settings.toml` are loaded on the `tokio` runtime after the first frame.
pub fn run_cli(
    process_start: Instant,
    options: AppLaunchOptions,
    request: LaunchRequest,
) -> ExitCode {
    let runtime = match build_io_runtime() {
        Ok(runtime) => runtime,
        Err(err) => {
            write_stderr(&format!("failed to start Kova I/O runtime: {err}"));
            return ExitCode::FAILURE;
        }
    };
    let runtime_handle = runtime.handle().clone();
    let (events_tx, events_rx) = mpsc::unbounded_channel::<AppEvent>();
    let (first_frame_tx, first_frame_rx) = oneshot::channel::<()>();

    let report_cell = Arc::new(OnceLock::<StartupMetrics>::new());
    let report_for_shell = Arc::clone(&report_cell);

    let probe_printed = Arc::new(AtomicBool::new(false));
    let probe_printed_for_shell = Arc::clone(&probe_printed);

    let open_failed = Arc::new(AtomicBool::new(false));
    let open_failed_for_app = Arc::clone(&open_failed);

    let project_root = request.project_root.clone();

    Application::new().run(move |cx: &mut App| {
        cx.set_global(KovaTheme::islands_dark());
        kova_ui::editor::actions::bind_default_keys(cx);
        kova_ui::git::bind_default_keys(cx);

        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let runtime_for_shell = runtime_handle.clone();
        let events_for_shell = events_tx.clone();
        let open_result = cx.open_window(default_window_options(cx), move |_, cx| {
            cx.activate(true);
            cx.new(move |cx| {
                let mut shell = WorkspaceShell::with_probe(
                    process_start,
                    options.exit_after_first_frame,
                    Some(Box::new(move |metrics| {
                        handle_first_frame_report(
                            metrics,
                            options,
                            &report_for_shell,
                            &probe_printed_for_shell,
                        );
                        let _ = first_frame_tx.send(());
                    })),
                    cx,
                );
                install_shell_sinks(&mut shell, &runtime_for_shell, &events_for_shell);
                match project_root {
                    Some(root) => shell.with_project_root(root),
                    None => shell,
                }
            })
        });

        match open_result {
            Ok(window) => {
                menus::install(window, &runtime_handle, &events_tx, cx);
                cx.spawn(async move |cx| {
                    pump_app_events(
                        window,
                        first_frame_rx,
                        runtime_handle,
                        request,
                        events_tx,
                        events_rx,
                        cx,
                    )
                    .await;
                })
                .detach();
            }
            Err(err) => {
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
        }
    });

    // Background tasks are cancelled; pending writes finish on their own blocking threads.
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));

    finalize_launch(
        options,
        open_failed.load(Ordering::Acquire),
        report_cell.get(),
        &probe_printed,
    )
}

fn install_shell_sinks(
    shell: &mut WorkspaceShell,
    runtime: &Handle,
    events: &mpsc::UnboundedSender<AppEvent>,
) {
    shell.set_save_sink(save_sink(runtime.clone(), events.clone()));
    shell.set_settings_sink(settings_write_sink(runtime, events.clone()));
    shell.set_test_run_sink(test_runs::test_run_sink(runtime.clone(), events.clone()));
    shell.set_debug_sink(debug_sessions::debug_command_sink(
        runtime.clone(),
        events.clone(),
    ));
    shell.set_definition_sink(definition::definition_sink(runtime.clone(), events.clone()));
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

    use kova_config::{SettingUpdate, SettingsDocument};
    use kova_ui::settings::SettingsWriteRequest;
    use tokio::sync::mpsc;

    use super::{
        AppEvent, AppLaunchOptions, CRATE_NAME, LaunchRequest, build_io_runtime, finalize_launch,
        settings_write_sink,
    };
    use crate::cli::FileTarget;

    #[test]
    fn settings_write_sink_persists_the_latest_document_with_comments() {
        let runtime = build_io_runtime().expect("runtime");
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = settings_write_sink(runtime.handle(), tx);

        let mut doc = SettingsDocument::from_toml_str("# keep\n[editor]\ntab_size = 4 # tabs\n")
            .expect("valid settings");
        for size in 2..=6_u8 {
            doc.set_setting(SettingUpdate::EditorTabSize(size))
                .expect("valid tab size");
            sink(SettingsWriteRequest {
                path: path.clone(),
                document: doc.clone(),
            });
        }

        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(10), async {
                while let Some(event) = rx.recv().await {
                    if let AppEvent::SettingsWritten(result) = event {
                        result.expect("write succeeds");
                        let text = std::fs::read_to_string(&path).expect("written");
                        if text.contains("tab_size = 6") {
                            break;
                        }
                    }
                }
            })
            .await
            .expect("latest write lands");
        });
        assert_eq!(
            std::fs::read_to_string(&path).expect("written"),
            "# keep\n[editor]\ntab_size = 6 # tabs\n"
        );
    }

    #[test]
    fn crate_name_matches_manifest() {
        assert_eq!(CRATE_NAME, "kova_app");
    }

    #[test]
    fn inherits_workspace_license() {
        assert_eq!(env!("CARGO_PKG_LICENSE"), "MIT OR Apache-2.0");
    }

    #[test]
    fn launch_request_maps_a_directory_to_the_project_root() {
        let target = |path: &str| FileTarget {
            path: path.into(),
            line: None,
            column: None,
        };
        let dir = env!("CARGO_MANIFEST_DIR");
        let request = LaunchRequest::from_cli(Some(target(dir)), Some("/cfg".into()));
        assert_eq!(request.target, None);
        assert_eq!(request.project_root, Some(dir.into()));
        assert_eq!(request.config_dir_override, Some("/cfg".into()));

        let file = target("/definitely/not/a/kova/File.java");
        let request = LaunchRequest::from_cli(Some(file.clone()), None);
        assert_eq!(request.target, Some(file));
        assert_eq!(request.project_root, None);
        assert_eq!(
            LaunchRequest::from_cli(None, None),
            LaunchRequest::default()
        );
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
