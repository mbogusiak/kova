//! Phase 0 Islands UI shell (`WorkspaceShell`) and startup telemetry (`StartupMetrics`).

use std::fmt;
use std::time::{Duration, Instant};

use gpui::{Context, FontWeight, IntoElement, ParentElement, Render, Styled, Window, div, px};
use kova_core::perf::{
    BudgetKind, budgets, check_duration_budget, check_rss_budget, current_process_rss_bytes,
};

use crate::theme::{KovaTheme, hsla_from_rgba};

/// Canonical Islands UI layout geometry from `KOVA-013` / `KOVA-014` (`editor_ux_guidelines.md` §5).
const SHELL_OUTER_PADDING_PX: f32 = 10.0;
const SHELL_ISLAND_GAP_PX: f32 = 8.0;
const TITLEBAR_HEIGHT_PX: f32 = 36.0;
const EDITOR_ISLAND_RADIUS_PX: f32 = 10.0;
const TAB_BAR_HEIGHT_PX: f32 = 36.0;
const STATUSBAR_HEIGHT_PX: f32 = 28.0;
const STATUSBAR_RADIUS_PX: f32 = 8.0;

#[cfg(target_os = "macos")]
const TITLEBAR_LEFT_PADDING_PX: f32 = 78.0;
#[cfg(not(target_os = "macos"))]
const TITLEBAR_LEFT_PADDING_PX: f32 = 14.0;

/// Diagnostic snapshot of cold-start latency and process memory measured after the first frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupMetrics {
    /// Elapsed time from process entry (`main`) until the first painted frame completed.
    pub cold_start_duration: Option<Duration>,
    /// Whether `cold_start_duration` satisfies `< COLD_START_MAX` (`120 ms`).
    pub cold_start_within_budget: Option<bool>,
    /// Resident Set Size (RSS) of the process sampled after the first frame, in bytes.
    pub rss_bytes: Option<u64>,
    /// Whether `rss_bytes` satisfies `< MAX_PROCESS_RSS_BYTES` (`150 MiB`).
    pub rss_within_budget: Option<bool>,
    /// Active GPU presentation backend name (`"Metal"` on macOS, `"Vulkan"` on Linux).
    pub backend_name: &'static str,
}

impl Default for StartupMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl StartupMetrics {
    /// Creates an unmeasured [`StartupMetrics`] record for the active platform GPU backend.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            cold_start_duration: None,
            cold_start_within_budget: None,
            rss_bytes: None,
            rss_within_budget: None,
            backend_name: Self::active_backend_name(),
        }
    }

    /// Returns the expected GPU backend label for the compiled target OS.
    #[must_use]
    pub const fn active_backend_name() -> &'static str {
        if cfg!(target_os = "macos") {
            "Metal"
        } else if cfg!(target_os = "linux") {
            "Vulkan"
        } else {
            "GPU"
        }
    }

    /// Records first-frame elapsed duration and optional RSS reading against Kova budgets.
    pub fn record_first_frame(&mut self, elapsed: Duration, rss_bytes: Option<u64>) {
        let cold_report =
            check_duration_budget(BudgetKind::ColdStart, elapsed, budgets::COLD_START_MAX);
        self.cold_start_duration = Some(elapsed);
        self.cold_start_within_budget = Some(cold_report.is_within_budget());

        self.rss_bytes = rss_bytes;
        self.rss_within_budget = rss_bytes.map(|bytes| check_rss_budget(bytes).is_within_budget());
    }

    /// Returns `true` when both cold-start latency and RSS memory were measured within budget.
    #[must_use]
    pub const fn is_within_budgets(&self) -> bool {
        matches!(
            (self.cold_start_within_budget, self.rss_within_budget),
            (Some(true), Some(true))
        )
    }

    /// Formats the cold-start duration label for the status bar (e.g., `"Start: 48 ms"`).
    #[must_use]
    pub fn format_start_label(&self) -> String {
        match self.cold_start_duration {
            Some(duration) => format!("Start: {} ms", duration.as_millis()),
            None => "Start: -- ms".to_owned(),
        }
    }

    /// Formats the process RSS footprint in MiB for the status bar (e.g., `"42.1 MiB"`).
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        reason = "process RSS in bytes fits accurately in f64 for MiB status reporting"
    )]
    pub fn format_rss_label(&self) -> String {
        match self.rss_bytes {
            Some(bytes) => {
                let mib = bytes as f64 / (1024.0 * 1024.0);
                format!("{mib:.1} MiB")
            }
            None => "-- MiB".to_owned(),
        }
    }

    /// Formats a compact summary combining start time, RSS footprint, and GPU backend.
    #[must_use]
    pub fn format_summary(&self) -> String {
        format!(
            "{} | {} | {}",
            self.format_start_label(),
            self.format_rss_label(),
            self.backend_name
        )
    }

    /// Formats a structured single-line probe report for automated cold-start benchmarks.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        reason = "sub-second cold-start microseconds convert cleanly to f64 milliseconds"
    )]
    pub fn format_probe_line(&self) -> String {
        let cold_start_ms = self.cold_start_duration.map_or_else(
            || "na".to_owned(),
            |d| format!("{:.2}", d.as_secs_f64() * 1000.0),
        );
        let rss_bytes = self
            .rss_bytes
            .map_or_else(|| "na".to_owned(), |bytes| bytes.to_string());
        format!(
            "cold_start_ms={cold_start_ms} rss_bytes={rss_bytes} backend={} within_budget={}",
            self.backend_name,
            self.is_within_budgets()
        )
    }
}

type FirstFrameCallback = Box<dyn FnOnce(&StartupMetrics)>;

/// Root GPUI view rendering the Phase 0 *Kova Islands Dark* workspace shell.
pub struct WorkspaceShell {
    process_start: Instant,
    metrics: StartupMetrics,
    first_frame_scheduled: bool,
    exit_after_first_frame: bool,
    on_first_frame: Option<FirstFrameCallback>,
}

impl fmt::Debug for WorkspaceShell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkspaceShell")
            .field("metrics", &self.metrics)
            .field("first_frame_scheduled", &self.first_frame_scheduled)
            .field("exit_after_first_frame", &self.exit_after_first_frame)
            .field("has_on_first_frame", &self.on_first_frame.is_some())
            .finish_non_exhaustive()
    }
}

impl WorkspaceShell {
    /// Creates a new [`WorkspaceShell`] anchored to `process_start` for cold-start telemetry.
    #[must_use]
    pub fn new(process_start: Instant) -> Self {
        Self::with_probe(process_start, false, None)
    }

    /// Creates a [`WorkspaceShell`] with optional automated probe exit and completion hook.
    #[must_use]
    pub fn with_probe(
        process_start: Instant,
        exit_after_first_frame: bool,
        on_first_frame: Option<FirstFrameCallback>,
    ) -> Self {
        Self {
            process_start,
            metrics: StartupMetrics::new(),
            first_frame_scheduled: false,
            exit_after_first_frame,
            on_first_frame,
        }
    }

    /// Returns the current [`StartupMetrics`] captured by this shell.
    #[must_use]
    pub const fn metrics(&self) -> &StartupMetrics {
        &self.metrics
    }

    fn complete_first_frame(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.metrics.cold_start_duration.is_some() {
            return;
        }

        let elapsed = self.process_start.elapsed();
        // Sample RSS only after the first frame is submitted so memory probing stays off the critical path.
        let rss_bytes = current_process_rss_bytes().ok();
        self.metrics.record_first_frame(elapsed, rss_bytes);

        if let Some(callback) = self.on_first_frame.take() {
            callback(&self.metrics);
        }

        cx.notify();

        if self.exit_after_first_frame {
            // Defer quitting until after the current frame cycle finishes cleanly.
            window.defer(cx, |_, cx| {
                cx.quit();
            });
        }
    }

    fn render_titlebar(theme: &KovaTheme) -> gpui::Div {
        let text_primary = hsla_from_rgba(theme.text().primary);
        let text_muted = hsla_from_rgba(theme.text().muted);

        div()
            .h(px(TITLEBAR_HEIGHT_PX))
            .w_full()
            .pl(px(TITLEBAR_LEFT_PADDING_PX))
            .pr(px(14.0))
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .text_size(px(12.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(text_primary)
                    .child("Kova"),
            )
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(text_muted)
                    .child(theme.name().to_owned()),
            )
    }

    fn render_editor_island(theme: &KovaTheme) -> gpui::Div {
        let editor_bg = hsla_from_rgba(theme.surfaces().editor);
        let tab_inactive_bg = hsla_from_rgba(theme.surfaces().tab_inactive);
        let tab_active_bg = hsla_from_rgba(theme.surfaces().tab_active);
        let island_border = hsla_from_rgba(theme.borders().island);
        let subtle_border = hsla_from_rgba(theme.borders().subtle);
        let tab_indicator = hsla_from_rgba(theme.borders().tab_indicator);
        let text_primary = hsla_from_rgba(theme.text().primary);
        let text_muted = hsla_from_rgba(theme.text().muted);

        div()
            .flex_1()
            .w_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(px(EDITOR_ISLAND_RADIUS_PX))
            .border_1()
            .border_color(island_border)
            .bg(editor_bg)
            .child(
                div()
                    .h(px(TAB_BAR_HEIGHT_PX))
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .bg(tab_inactive_bg)
                    .border_b_1()
                    .border_color(subtle_border)
                    .child(
                        div()
                            .h_full()
                            .px(px(14.0))
                            .flex()
                            .items_center()
                            .bg(tab_active_bg)
                            .border_b_2()
                            .border_color(tab_indicator)
                            .text_size(px(12.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(text_primary)
                            .child("Welcome"),
                    )
                    .child(
                        div()
                            .pr(px(12.0))
                            .text_size(px(11.0))
                            .text_color(text_muted)
                            .child("Phase 0 · Islands UI"),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .w_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .text_size(px(15.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(text_primary)
                            .child("Kova"),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(text_muted)
                            .child("Editor ready"),
                    ),
            )
    }

    fn render_statusbar(&self, theme: &KovaTheme) -> gpui::Div {
        let statusbar_bg = hsla_from_rgba(theme.surfaces().statusbar);
        let island_border = hsla_from_rgba(theme.borders().island);
        let text_primary = hsla_from_rgba(theme.text().primary);
        let text_secondary = hsla_from_rgba(theme.text().secondary);
        let text_muted = hsla_from_rgba(theme.text().muted);
        let run_icon = hsla_from_rgba(theme.gutter().run_icon);
        let warning_color = hsla_from_rgba(theme.editor().diagnostic_warning);

        let start_color = match self.metrics.cold_start_within_budget {
            Some(true) => run_icon,
            Some(false) => warning_color,
            None => text_muted,
        };

        div()
            .h(px(STATUSBAR_HEIGHT_PX))
            .w_full()
            .px(px(12.0))
            .flex()
            .items_center()
            .justify_between()
            .rounded(px(STATUSBAR_RADIUS_PX))
            .border_1()
            .border_color(island_border)
            .bg(statusbar_bg)
            .text_size(px(11.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(div().size(px(6.0)).rounded_full().bg(run_icon))
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(text_primary)
                            .child(theme.name().to_owned()),
                    )
                    .child(div().text_color(text_secondary).child("Ready")),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(start_color)
                            .child(self.metrics.format_start_label()),
                    )
                    .child(
                        div()
                            .text_color(text_secondary)
                            .child(self.metrics.format_rss_label()),
                    )
                    .child(
                        div()
                            .text_color(text_muted)
                            .child(self.metrics.backend_name),
                    ),
            )
    }
}

impl Render for WorkspaceShell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        if !self.first_frame_scheduled {
            self.first_frame_scheduled = true;
            cx.on_next_frame(window, Self::complete_first_frame);
        }

        let theme = cx.global::<KovaTheme>();
        let canvas_bg = hsla_from_rgba(theme.surfaces().canvas);
        let text_primary = hsla_from_rgba(theme.text().primary);

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(canvas_bg)
            .text_color(text_primary)
            .child(Self::render_titlebar(theme))
            .child(
                div()
                    .flex_1()
                    .w_full()
                    .flex()
                    .flex_col()
                    .px(px(SHELL_OUTER_PADDING_PX))
                    .pb(px(SHELL_OUTER_PADDING_PX))
                    .gap(px(SHELL_ISLAND_GAP_PX))
                    .child(Self::render_editor_island(theme))
                    .child(self.render_statusbar(theme)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_metrics_formats_summary_and_evaluates_budgets() {
        let mut metrics = StartupMetrics::new();
        assert_eq!(metrics.format_start_label(), "Start: -- ms");
        assert_eq!(metrics.format_rss_label(), "-- MiB");
        assert!(!metrics.is_within_budgets());

        metrics.record_first_frame(Duration::from_millis(48), Some(42 * 1024 * 1024));
        assert_eq!(metrics.cold_start_within_budget, Some(true));
        assert_eq!(metrics.rss_within_budget, Some(true));
        assert!(metrics.is_within_budgets());
        assert_eq!(metrics.format_start_label(), "Start: 48 ms");
        assert_eq!(metrics.format_rss_label(), "42.0 MiB");
        assert_eq!(
            metrics.format_summary(),
            format!("Start: 48 ms | 42.0 MiB | {}", metrics.backend_name)
        );
        let probe = metrics.format_probe_line();
        assert!(probe.contains("cold_start_ms=48.00"));
        assert!(probe.contains("rss_bytes=44040192"));
        assert!(probe.contains("within_budget=true"));
    }

    #[test]
    fn startup_metrics_flags_budget_violations() {
        let mut metrics = StartupMetrics::new();
        metrics.record_first_frame(Duration::from_millis(120), Some(150 * 1024 * 1024));
        assert_eq!(metrics.cold_start_within_budget, Some(false));
        assert_eq!(metrics.rss_within_budget, Some(false));
        assert!(!metrics.is_within_budgets());

        let mut missing_rss = StartupMetrics::new();
        missing_rss.record_first_frame(Duration::from_millis(50), None);
        assert_eq!(missing_rss.cold_start_within_budget, Some(true));
        assert_eq!(missing_rss.rss_within_budget, None);
        assert!(!missing_rss.is_within_budgets());
    }
}
