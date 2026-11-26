//! GPUI rendering of the `JUnit` *Test Runner* dock (`junit_debugger_dock.svg`, *Kova Islands
//! Dark*): toolbar with *Rerun* / *Rerun Failed* / filters / progress / counters, a virtualized
//! test tree on the left and the output pane with clickable stack frames on the right.
//!
//! The functions here are stateless: the shell owns the [`TestRunnerModel`] and passes a
//! [`TestRunnerHandler`] that routes every [`TestRunnerAction`] back to it.

use std::ops::Range;
use std::rc::Rc;

use gpui::{
    AnyElement, App, ClickEvent, Div, FontWeight, Hsla, InteractiveElement as _, IntoElement,
    ParentElement, SharedString, Stateful, StatefulInteractiveElement as _, Styled, Window, div,
    px, relative, rgb,
};
use kova_tools::junit::{NodeKind, OutputKind, RunState, TestStatus};

use crate::test_runner::{TestRunnerAction, TestRunnerModel};

/// Height of one tree row in logical pixels (fixed for `uniform_list` virtualization).
pub const TEST_ROW_HEIGHT_PX: f32 = 22.0;
/// Height of the toolbar.
const TOOLBAR_HEIGHT_PX: f32 = 30.0;
/// Indentation per tree level.
const TEST_INDENT_PX: f32 = 16.0;

/// Callback receiving the panel's actions.
pub type TestRunnerHandler = Rc<dyn Fn(&TestRunnerAction, &mut Window, &mut App)>;

/// Colours used by the panel, resolved from the active theme by the shell.
#[derive(Debug, Clone, Copy)]
pub struct TestRunnerColors {
    /// Primary text.
    pub text_primary: Hsla,
    /// Secondary text.
    pub text_secondary: Hsla,
    /// Muted text.
    pub text_muted: Hsla,
    /// Hover background.
    pub hover: Hsla,
    /// Selected row background.
    pub active: Hsla,
    /// Separator lines.
    pub border: Hsla,
    /// Inset surface (output pane, progress track).
    pub inset: Hsla,
    /// Passed / success green.
    pub passed: Hsla,
    /// Failed / error red.
    pub failed: Hsla,
    /// Ignored / warning tint.
    pub ignored: Hsla,
    /// Clickable source link blue.
    pub link: Hsla,
}

impl TestRunnerColors {
    /// Mockup colours on top of theme text / surface tokens.
    #[must_use]
    pub fn islands_dark(
        text_primary: Hsla,
        text_secondary: Hsla,
        text_muted: Hsla,
        hover: Hsla,
        active: Hsla,
        border: Hsla,
        ignored: Hsla,
    ) -> Self {
        Self {
            text_primary,
            text_secondary,
            text_muted,
            hover,
            active,
            border,
            inset: rgb(0x0014_161B).into(),
            passed: rgb(0x0059_A869).into(),
            failed: rgb(0x00E0_5252).into(),
            ignored,
            link: rgb(0x0054_8AF7).into(),
        }
    }

    fn status(&self, status: TestStatus) -> (&'static str, Hsla) {
        match status {
            TestStatus::Running => ("◌", self.text_secondary),
            TestStatus::Passed => ("✓", self.passed),
            TestStatus::Failed => ("✗", self.failed),
            TestStatus::Ignored => ("⊘", self.ignored),
        }
    }
}

/// Empty-state body shown before the first run.
#[must_use]
pub fn render_empty(colors: TestRunnerColors) -> Div {
    div()
        .flex_1()
        .w_full()
        .p(px(12.0))
        .text_size(px(12.0))
        .text_color(colors.text_muted)
        .child("No test runs yet. Click ▶ next to a @Test method or class to run it via BSP / Gradle / Maven.")
}

#[allow(
    clippy::too_many_arguments,
    reason = "plain toolbar button builder; a spec struct would only rename the arguments"
)]
fn button(
    id: &'static str,
    label: String,
    enabled: bool,
    active: bool,
    color: Hsla,
    colors: TestRunnerColors,
    action: TestRunnerAction,
    handler: &TestRunnerHandler,
) -> Stateful<Div> {
    let mut el = div()
        .id(id)
        .h(px(22.0))
        .px(px(8.0))
        .flex()
        .items_center()
        .rounded(px(4.0))
        .text_size(px(12.0))
        .child(label);
    if active {
        el = el.bg(colors.active);
    }
    if enabled {
        let handler = Rc::clone(handler);
        el.text_color(color)
            .hover(|s| s.bg(colors.hover))
            .on_click(move |_: &ClickEvent, window, cx| handler(&action, window, cx))
    } else {
        el.text_color(colors.text_muted)
    }
}

/// Toolbar: *Rerun*, *Rerun Failed (n)*, *Passed* / *Failed* filters, *Save to kova.toml*,
/// progress bar and `passed / failed / total · time` counters.
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "declarative GPUI layout of the whole toolbar"
)]
pub fn render_toolbar(
    model: &TestRunnerModel,
    colors: TestRunnerColors,
    can_save: bool,
    handler: &TestRunnerHandler,
) -> Div {
    let Some(run) = model.run() else {
        return div();
    };
    let counts = run.counts();
    let filter = run.filter();
    let running = run.is_running();
    let total_ms: u64 = run
        .roots()
        .iter()
        .filter_map(|r| run.node(*r))
        .map(|n| n.duration_ms)
        .sum();
    let bar_color = if counts.failed > 0 {
        colors.failed
    } else {
        colors.passed
    };
    let progress = if running { counts.progress() } else { 1.0 };
    let status_text = match run.state() {
        RunState::Running => format!("Running… {}/{}", counts.finished(), counts.total),
        RunState::Finished { success: true, .. } => "Tests passed".to_owned(),
        RunState::Finished { success: false, .. } if counts.failed > 0 => "Tests failed".to_owned(),
        RunState::Finished { success: false, .. } => "Run failed".to_owned(),
    };
    div()
        .h(px(TOOLBAR_HEIGHT_PX))
        .flex_none()
        .w_full()
        .px(px(6.0))
        .flex()
        .items_center()
        .gap(px(4.0))
        .border_b_1()
        .border_color(colors.border)
        .child(button(
            "test-rerun",
            "▶ Rerun".to_owned(),
            !running,
            false,
            colors.passed,
            colors,
            TestRunnerAction::RerunAll,
            handler,
        ))
        .child(button(
            "test-rerun-failed",
            format!("↻ Failed ({})", counts.failed),
            !running && counts.failed > 0,
            false,
            colors.failed,
            colors,
            TestRunnerAction::RerunFailed,
            handler,
        ))
        .child(div().w(px(1.0)).h(px(16.0)).bg(colors.border))
        .child(button(
            "test-show-passed",
            "✓ Passed".to_owned(),
            true,
            filter.show_passed,
            colors.passed,
            colors,
            TestRunnerAction::ToggleShowPassed,
            handler,
        ))
        .child(button(
            "test-show-failed",
            "✗ Failed".to_owned(),
            true,
            filter.show_failed,
            colors.failed,
            colors,
            TestRunnerAction::ToggleShowFailed,
            handler,
        ))
        .child(button(
            "test-save-config",
            "＋ kova.toml".to_owned(),
            can_save,
            false,
            colors.text_secondary,
            colors,
            TestRunnerAction::SaveConfiguration,
            handler,
        ))
        .child(
            div()
                .flex_1()
                .mx(px(8.0))
                .h(px(4.0))
                .rounded(px(2.0))
                .bg(colors.inset)
                .child(
                    div()
                        .h_full()
                        .rounded(px(2.0))
                        .w(relative(progress.clamp(0.0, 1.0)))
                        .bg(bar_color),
                ),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .text_size(px(12.0))
                .child(div().text_color(colors.text_secondary).child(status_text))
                .child(
                    div()
                        .text_color(colors.passed)
                        .child(format!("{} passed", counts.passed)),
                )
                .child(
                    div()
                        .text_color(colors.failed)
                        .child(format!("{} failed", counts.failed)),
                )
                .child(
                    div()
                        .text_color(colors.text_muted)
                        .child(format!("{} total · {total_ms}ms", counts.total)),
                ),
        )
}

/// Renders tree rows `range` (called from a `uniform_list` processor).
#[must_use]
pub fn render_rows(
    model: &TestRunnerModel,
    range: Range<usize>,
    colors: TestRunnerColors,
    handler: &TestRunnerHandler,
) -> Vec<Stateful<Div>> {
    let Some(run) = model.run() else {
        return Vec::new();
    };
    let rows = run.rows();
    let end = range.end.min(rows.len());
    let start = range.start.min(end);
    let selected = run.selected();
    rows[start..end]
        .iter()
        .enumerate()
        .filter_map(|(offset, row)| {
            let index = start + offset;
            let node = run.node(row.node)?;
            let (glyph, color) = colors.status(node.status);
            let disclosure = if row.has_children {
                if row.expanded { "▾" } else { "▸" }
            } else {
                ""
            };
            let select = Rc::clone(handler);
            let toggle = Rc::clone(handler);
            let mut el =
                div()
                    .id(("test-row", index))
                    .h(px(TEST_ROW_HEIGHT_PX))
                    .w_full()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .pl(px(6.0
                        + TEST_INDENT_PX
                            * f32::from(
                                u16::try_from(row.depth).unwrap_or(u16::MAX),
                            )))
                    .pr(px(8.0))
                    .text_size(px(12.0))
                    .hover(|s| s.bg(colors.hover))
                    .on_click(move |event: &ClickEvent, window, cx| {
                        if event.click_count() >= 2 {
                            toggle(&TestRunnerAction::ToggleRow(index), window, cx);
                        } else {
                            select(&TestRunnerAction::SelectRow(index), window, cx);
                        }
                    })
                    .child(
                        div()
                            .w(px(10.0))
                            .text_color(colors.text_muted)
                            .child(disclosure),
                    )
                    .child(div().text_color(color).child(glyph))
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .text_color(colors.text_primary)
                            .when_bold(node.kind != NodeKind::Method)
                            .child(node.name.clone()),
                    )
                    .child(div().text_color(colors.text_muted).child(
                        if node.status.is_finished() {
                            format!("{}ms", node.duration_ms)
                        } else {
                            String::new()
                        },
                    ));
            if selected == Some(row.node) {
                el = el.bg(colors.active);
            }
            Some(el)
        })
        .collect()
}

trait WhenBold {
    fn when_bold(self, bold: bool) -> Self;
}

impl WhenBold for Div {
    fn when_bold(self, bold: bool) -> Self {
        if bold {
            self.font_weight(FontWeight::SEMIBOLD)
        } else {
            self
        }
    }
}

/// Output pane: failure message, stack trace (project frames are blue links) and stdout/stderr.
#[must_use]
pub fn render_output(
    model: &TestRunnerModel,
    colors: TestRunnerColors,
    handler: &TestRunnerHandler,
) -> AnyElement {
    let title = model
        .run()
        .and_then(|run| run.selected().and_then(|n| run.node(n)))
        .map_or_else(
            || "Console".to_owned(),
            |node| format!("{} — Output & Stacktrace", node.name),
        );
    let lines = model.output().iter().enumerate().map(|(index, line)| {
        let base = div()
            .id(("test-output-line", index))
            .w_full()
            .text_size(px(12.0))
            .font_family("monospace");
        if model.target_at(index).is_some() {
            let handler = Rc::clone(handler);
            base.text_color(colors.link)
                .cursor_pointer()
                .hover(|s| s.bg(colors.hover))
                .on_click(move |_: &ClickEvent, window, cx| {
                    handler(&TestRunnerAction::OpenFrame(index), window, cx);
                })
                .child(SharedString::from(line.text.clone()))
        } else {
            let color = match line.kind {
                OutputKind::Failure if line.frame.is_some() => colors.text_muted,
                OutputKind::Failure | OutputKind::Stderr => colors.failed,
                OutputKind::Stdout => colors.text_secondary,
            };
            base.text_color(color)
                .child(SharedString::from(line.text.clone()))
        }
    });
    div()
        .flex_1()
        .h_full()
        .flex()
        .flex_col()
        .border_l_1()
        .border_color(colors.border)
        .child(
            div()
                .h(px(24.0))
                .flex_none()
                .px(px(10.0))
                .flex()
                .items_center()
                .text_size(px(12.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(colors.text_primary)
                .child(title),
        )
        .child(
            div()
                .id("test-output")
                .flex_1()
                .px(px(10.0))
                .py(px(4.0))
                .bg(colors.inset)
                .overflow_y_scroll()
                .children(lines),
        )
        .child(
            div()
                .h(px(20.0))
                .flex_none()
                .px(px(10.0))
                .text_size(px(11.0))
                .text_color(colors.text_muted)
                .child("Click any blue frame to jump to source in editor"),
        )
        .into_any_element()
}
