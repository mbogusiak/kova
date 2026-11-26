//! GPUI rendering of the *Debugger* dock (`junit_debugger_dock.svg`, right island, *Kova
//! Islands Dark*): the control bar (*Resume* `F9`, *Step Over* `F8`, *Step Into* `F7`,
//! *Step Out* `Shift+F8`, *Stop*, *Evaluate Expression* `Alt+F8`), *Threads & Frames* and
//! *Variables & Watches*, plus the debuggee console.
//!
//! The functions are stateless: the shell owns the [`DebuggerModel`] and passes a
//! [`DebuggerHandler`] routing every [`DebuggerAction`] back to it.

use std::rc::Rc;

use gpui::{
    App, ClickEvent, Div, FontWeight, Hsla, InteractiveElement as _, ParentElement, Stateful,
    StatefulInteractiveElement as _, Styled, Window, div, px, rgb,
};

use crate::debugger::{DebugStep, DebuggerAction, DebuggerModel, VariableRow, WatchItem};

/// Height of one list row.
pub const DEBUG_ROW_HEIGHT_PX: f32 = 22.0;
/// Indentation per *Variables* tree level.
const INDENT_PX: f32 = 14.0;

/// Left padding of a *Variables* row at `depth`.
#[allow(
    clippy::cast_precision_loss,
    reason = "tree depth is tiny and fits cleanly within f32"
)]
fn indent(depth: usize) -> f32 {
    depth as f32 * INDENT_PX
}

/// Callback receiving the dock's actions.
pub type DebuggerHandler = Rc<dyn Fn(&DebuggerAction, &mut Window, &mut App)>;

/// Colours used by the dock, resolved from the active theme by the shell.
#[derive(Debug, Clone, Copy)]
pub struct DebuggerColors {
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
    /// Separators.
    pub border: Hsla,
    /// Island / dialog surface.
    pub surface: Hsla,
    /// Inset surface (console, input field).
    pub inset: Hsla,
    /// Accent (focused input, current frame).
    pub accent: Hsla,
    /// Variable values.
    pub value: Hsla,
    /// Errors and the *Stop* button.
    pub error: Hsla,
    /// *Resume* green.
    pub resume: Hsla,
}

impl DebuggerColors {
    /// Mockup colours on top of theme text / surface tokens.
    #[must_use]
    pub fn islands_dark(
        text_primary: Hsla,
        text_secondary: Hsla,
        text_muted: Hsla,
        hover: Hsla,
        active: Hsla,
        border: Hsla,
        surface: Hsla,
    ) -> Self {
        Self {
            text_primary,
            text_secondary,
            text_muted,
            hover,
            active,
            border,
            surface,
            inset: rgb(0x0014_161B).into(),
            accent: rgb(0x0054_8AF7).into(),
            value: rgb(0x006A_AB73).into(),
            error: rgb(0x00E0_5252).into(),
            resume: rgb(0x0059_A869).into(),
        }
    }
}

fn button(
    id: &'static str,
    label: &'static str,
    enabled: bool,
    color: Hsla,
    colors: DebuggerColors,
    action: DebuggerAction,
    handler: &DebuggerHandler,
) -> Stateful<Div> {
    let el = div()
        .id(id)
        .h(px(22.0))
        .px(px(8.0))
        .flex()
        .items_center()
        .rounded(px(4.0))
        .text_size(px(12.0))
        .child(label);
    if enabled {
        let handler = Rc::clone(handler);
        el.text_color(color)
            .hover(|s| s.bg(colors.hover))
            .on_click(move |_: &ClickEvent, window, cx| handler(&action, window, cx))
    } else {
        el.text_color(colors.text_muted)
    }
}

/// Empty-state body shown before the first session.
#[must_use]
pub fn render_empty(colors: DebuggerColors) -> Div {
    div()
        .flex_1()
        .w_full()
        .p(px(12.0))
        .text_size(px(12.0))
        .text_color(colors.text_muted)
        .child(
            "No debug session. Click a line number to set a breakpoint, then start debugging \
             with Ctrl+Shift+F9 (Ctrl+Shift+D on macOS).",
        )
}

/// Control bar with the `JetBrains` shortcuts.
#[must_use]
pub fn render_toolbar(
    model: &DebuggerModel,
    colors: DebuggerColors,
    handler: &DebuggerHandler,
) -> Div {
    let suspended = model.status().is_suspended();
    let active = model.status().is_active();
    let text = colors.text_primary;
    div()
        .h(px(30.0))
        .w_full()
        .flex()
        .items_center()
        .gap(px(2.0))
        .px(px(6.0))
        .border_b_1()
        .border_color(colors.border)
        .child(button(
            "dbg-resume",
            "▶ Resume (F9)",
            suspended,
            colors.resume,
            colors,
            DebuggerAction::Resume,
            handler,
        ))
        .child(button(
            "dbg-step-over",
            "↷ Step Over (F8)",
            suspended,
            text,
            colors,
            DebuggerAction::Step(DebugStep::Over),
            handler,
        ))
        .child(button(
            "dbg-step-into",
            "↓ Step Into (F7)",
            suspended,
            text,
            colors,
            DebuggerAction::Step(DebugStep::Into),
            handler,
        ))
        .child(button(
            "dbg-step-out",
            "↑ Step Out (⇧F8)",
            suspended,
            text,
            colors,
            DebuggerAction::Step(DebugStep::Out),
            handler,
        ))
        .child(button(
            "dbg-stop",
            "■ Stop",
            active,
            colors.error,
            colors,
            DebuggerAction::Stop,
            handler,
        ))
        .child(button(
            "dbg-evaluate",
            "Evaluate (⌥F8)",
            suspended,
            text,
            colors,
            DebuggerAction::OpenEvaluate,
            handler,
        ))
        .child(div().flex_1())
        .child(
            div()
                .text_size(px(11.0))
                .text_color(colors.text_secondary)
                .child(format!("{} — {}", model.label(), model.status().label())),
        )
}

fn section_header(title: &'static str, colors: DebuggerColors) -> Div {
    div()
        .h(px(DEBUG_ROW_HEIGHT_PX))
        .px(px(8.0))
        .flex()
        .items_center()
        .text_size(px(11.0))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(colors.text_secondary)
        .child(title)
}

fn row(id: (&'static str, usize), selected: bool, colors: DebuggerColors) -> Stateful<Div> {
    let el = div()
        .id(id)
        .h(px(DEBUG_ROW_HEIGHT_PX))
        .px(px(8.0))
        .flex()
        .items_center()
        .gap(px(6.0))
        .text_size(px(12.0))
        .whitespace_nowrap()
        .overflow_hidden()
        .hover(|s| s.bg(colors.hover));
    if selected { el.bg(colors.active) } else { el }
}

/// *Threads & Frames* column.
#[must_use]
pub fn render_threads_and_frames(
    model: &DebuggerModel,
    colors: DebuggerColors,
    handler: &DebuggerHandler,
) -> Stateful<Div> {
    let mut list = div()
        .id("dbg-threads")
        .flex()
        .flex_col()
        .size_full()
        .overflow_y_scroll()
        .child(section_header("Threads & Frames", colors));
    for (index, thread) in model.threads().iter().enumerate() {
        let selected = model.selected_thread() == Some(thread.id);
        let on_thread = Rc::clone(handler);
        list = list.child(
            row(("dbg-thread", index), selected, colors)
                .text_color(colors.text_primary)
                .on_click(move |_: &ClickEvent, window, cx| {
                    on_thread(&DebuggerAction::SelectThread(index), window, cx);
                })
                .child(div().text_color(colors.text_muted).child("⧖"))
                .child(format!("\"{}\"", thread.name)),
        );
        if !selected {
            continue;
        }
        for (frame_index, frame) in model.frames().iter().enumerate() {
            let current = frame_index == model.selected_frame_index();
            let handler = Rc::clone(handler);
            let location = frame
                .path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|n| format!("{}:{}", n.to_string_lossy(), frame.line))
                .unwrap_or_default();
            list = list.child(
                row(("dbg-frame", frame_index), current, colors)
                    .pl(px(8.0 + INDENT_PX))
                    .on_click(move |_: &ClickEvent, window, cx| {
                        handler(&DebuggerAction::SelectFrame(frame_index), window, cx);
                    })
                    .child(
                        div()
                            .text_color(if current {
                                colors.accent
                            } else {
                                colors.text_primary
                            })
                            .child(frame.name.clone()),
                    )
                    .child(div().text_color(colors.text_muted).child(location)),
            );
        }
    }
    list
}

/// *Variables & Watches* column.
#[must_use]
pub fn render_variables_and_watches(
    model: &DebuggerModel,
    colors: DebuggerColors,
    handler: &DebuggerHandler,
) -> Stateful<Div> {
    let mut list = div()
        .id("dbg-variables")
        .flex()
        .flex_col()
        .size_full()
        .overflow_y_scroll()
        .child(section_header("Variables", colors));
    for (index, variable) in model.variable_rows().iter().enumerate() {
        let el = match variable {
            VariableRow::Value {
                depth,
                item,
                expanded,
            } => {
                let chevron = match (item.reference > 0, *expanded) {
                    (false, _) => " ",
                    (true, false) => "›",
                    (true, true) => "⌄",
                };
                let type_name = item
                    .type_name
                    .as_deref()
                    .map(|t| format!("({t})"))
                    .unwrap_or_default();
                let handler = Rc::clone(handler);
                row(("dbg-var", index), false, colors)
                    .pl(px(8.0 + indent(*depth)))
                    .on_click(move |_: &ClickEvent, window, cx| {
                        handler(&DebuggerAction::ToggleVariable(index), window, cx);
                    })
                    .child(
                        div()
                            .w(px(10.0))
                            .text_color(colors.text_muted)
                            .child(chevron),
                    )
                    .child(
                        div()
                            .text_color(colors.text_primary)
                            .child(item.name.clone()),
                    )
                    .child(div().text_color(colors.text_muted).child("="))
                    .child(div().text_color(colors.value).child(item.value.clone()))
                    .child(div().text_color(colors.text_muted).child(type_name))
            }
            VariableRow::Loading { depth } => row(("dbg-var", index), false, colors)
                .pl(px(8.0 + indent(*depth)))
                .text_color(colors.text_muted)
                .child("Collecting data…"),
            VariableRow::More {
                depth,
                start,
                total,
                ..
            } => {
                let handler = Rc::clone(handler);
                row(("dbg-var", index), false, colors)
                    .pl(px(8.0 + indent(*depth)))
                    .text_color(colors.accent)
                    .on_click(move |_: &ClickEvent, window, cx| {
                        handler(&DebuggerAction::LoadMore(index), window, cx);
                    })
                    .child(format!(
                        "Load more… ({start}–{} of {total})",
                        (start + super::VARIABLES_PAGE_SIZE).min(*total)
                    ))
            }
        };
        list = list.child(el);
    }
    list = list.child(section_header("Watches", colors));
    for (index, watch) in model.watches().iter().enumerate() {
        list = list.child(render_watch(index, watch, colors, handler));
    }
    list
}

fn render_watch(
    index: usize,
    watch: &WatchItem,
    colors: DebuggerColors,
    handler: &DebuggerHandler,
) -> Stateful<Div> {
    let (text, color) = match &watch.result {
        None => ("…".to_owned(), colors.text_muted),
        Some(Ok(value)) => (value.clone(), colors.value),
        Some(Err(err)) => (err.clone(), colors.error),
    };
    let handler = Rc::clone(handler);
    row(("dbg-watch", index), false, colors)
        .child(div().text_color(colors.accent).child("◉"))
        .child(
            div()
                .text_color(colors.text_primary)
                .child(watch.expression.clone()),
        )
        .child(div().text_color(colors.text_muted).child("="))
        .child(div().flex_1().text_color(color).child(text))
        .child(
            div()
                .id(("dbg-watch-remove", index))
                .px(px(4.0))
                .text_color(colors.text_muted)
                .hover(|s| s.text_color(colors.error))
                .on_click(move |_: &ClickEvent, window, cx| {
                    handler(&DebuggerAction::RemoveWatch(index), window, cx);
                })
                .child("×"),
        )
}

/// Debuggee console (stdout, stderr and logpoint output).
#[must_use]
pub fn render_console(model: &DebuggerModel, colors: DebuggerColors) -> Stateful<Div> {
    let mut console = div()
        .id("dbg-console")
        .flex()
        .flex_col()
        .size_full()
        .p(px(6.0))
        .bg(colors.inset)
        .overflow_y_scroll()
        .text_size(px(12.0))
        .text_color(colors.text_secondary)
        .child(section_header("Console", colors));
    for line in model.console().iter().rev().take(500).rev() {
        console = console.child(div().whitespace_nowrap().child(line.clone()));
    }
    console
}
