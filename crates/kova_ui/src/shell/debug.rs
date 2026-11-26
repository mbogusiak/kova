//! Debugger integration of the [`WorkspaceShell`] (`KOVA-041`): gutter breakpoints, the
//! `JetBrains` debugger shortcuts, the *Debugger* bottom dock, the execution point in the editor
//! and the *Evaluate Expression* (`Alt+F8`) dialog.
//!
//! The shell turns user actions into [`DebugCommand`]s for the host [`DebugCommandSink`]
//! (`kova_app` → `kova_lsp_dap::dap`) and applies the [`DebugUpdate`]s it reports.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui::{
    AnyElement, Context, Div, IntoElement, ParentElement, Styled, Window, div, px, relative,
};

use super::{Palette, WorkspaceShell};
use crate::debugger::evaluate_modal::{self, EvaluateButton, EvaluateKeyOutcome};
use crate::debugger::{
    DebugCommand, DebugCommandSink, DebugStep, DebugUpdate, DebuggerAction, DebuggerColors,
    DebuggerHandler, DebuggerModel, DebuggerStatus, debugger_panel,
};
use crate::dock::{DockToolId, FocusedRegion};
use crate::document::StatusKind;
use crate::editor::DebugGutter;
use crate::keymap::DebugShellAction;

impl WorkspaceShell {
    /// Installs the host executor of debug sessions (`KOVA-041`).
    pub fn set_debug_sink(&mut self, sink: DebugCommandSink) {
        self.debug_sink = Some(sink);
    }

    /// Debugger state.
    #[must_use]
    pub const fn debugger(&self) -> &DebuggerModel {
        &self.debugger
    }

    /// Absolute path of a document key (project-relative when a project is open).
    pub(super) fn debug_abs_path(&self, key: &Path) -> PathBuf {
        match self.project.root() {
            Some(root) if key.is_relative() => root.join(key),
            _ => key.to_path_buf(),
        }
    }

    /// Document key of an absolute source path reported by the adapter.
    fn debug_doc_key(&self, path: &Path) -> PathBuf {
        self.project
            .root()
            .and_then(|root| path.strip_prefix(root).ok())
            .map_or_else(|| path.to_path_buf(), Path::to_path_buf)
    }

    /// Sends commands to the host; without a host, a start fails visibly.
    pub(super) fn send_debug(&mut self, commands: impl IntoIterator<Item = DebugCommand>) {
        let session_id = self.debugger.session_id();
        for command in commands {
            if let Some(sink) = &self.debug_sink {
                sink(session_id, command);
            } else if matches!(command, DebugCommand::Start(_)) {
                self.debugger.apply(
                    session_id,
                    DebugUpdate::Status(DebuggerStatus::Terminated {
                        message: Some(
                            "No debugger host attached: start Kova with `kova <project-dir>`."
                                .to_owned(),
                        ),
                    }),
                );
            }
        }
    }

    /// Toggles a breakpoint on 0-based buffer `row` of the document `key`.
    pub(super) fn toggle_breakpoint_at(
        &mut self,
        key: &Path,
        row: usize,
        cx: &mut Context<'_, Self>,
    ) {
        let file = self.debug_abs_path(key);
        let line = u32::try_from(row + 1).unwrap_or(u32::MAX);
        let command = self.debugger.toggle_breakpoint(&file, line);
        self.send_debug(command);
        self.refresh_debug_gutters(cx);
        cx.notify();
    }

    /// Pushes breakpoints and the execution point into every open editor.
    pub(super) fn refresh_debug_gutters(&mut self, cx: &mut Context<'_, Self>) {
        let execution = self.debugger.execution_point();
        let mut updates = Vec::new();
        for pane in self.panes.values() {
            for tab in pane.tabs.tabs() {
                let (Some(key), Some(editor)) = (tab.path.as_ref(), pane.editors.get(&tab.id))
                else {
                    continue;
                };
                let file = self.debug_abs_path(key);
                let gutter = DebugGutter {
                    breakpoints: self.debugger.gutter_icons(&file),
                    execution_row: execution
                        .as_ref()
                        .filter(|location| location.path == file)
                        .map(|location| (location.line as usize).saturating_sub(1)),
                };
                updates.push((editor.clone(), gutter));
            }
        }
        for (editor, gutter) in updates {
            editor.update(cx, |editor, cx| editor.set_debug_gutter(gutter, cx));
        }
    }

    /// Runs a keymap debugger command.
    pub(super) fn run_debug_action(
        &mut self,
        action: DebugShellAction,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match action {
            DebugShellAction::ToggleBreakpoint => {
                let target = self.active_document_key().zip(
                    self.active_editor()
                        .map(|editor| editor.read(cx).caret_row()),
                );
                if let Some((key, row)) = target {
                    self.toggle_breakpoint_at(&key, row, cx);
                }
            }
            DebugShellAction::StepOver => self.debug_step(DebugStep::Over, cx),
            DebugShellAction::StepInto => self.debug_step(DebugStep::Into, cx),
            DebugShellAction::StepOut => self.debug_step(DebugStep::Out, cx),
            DebugShellAction::Resume => {
                let command = self.debugger.resume_command();
                self.send_debug(command);
            }
            DebugShellAction::EvaluateExpression => self.open_evaluate(cx),
            DebugShellAction::DebugContext => self.start_debug_session(window, cx),
            DebugShellAction::Stop => self.stop_active_run_action(cx),
        }
        cx.notify();
    }

    fn debug_step(&mut self, kind: DebugStep, cx: &mut Context<'_, Self>) {
        if let Some(command) = self.debugger.step_command(kind) {
            self.send_debug([command]);
        } else {
            self.set_notice(
                StatusKind::Info,
                "Stepping needs a suspended debug session".to_owned(),
                cx,
            );
        }
    }

    fn open_evaluate(&mut self, cx: &mut Context<'_, Self>) {
        let selection = self
            .active_editor()
            .and_then(|editor| editor.read(cx).primary_selection_text());
        self.debugger.open_evaluate(selection.as_deref());
        self.keymap.reset();
    }

    /// Starts debugging the entry point at the caret (`main` or test class / method).
    fn start_debug_session(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) {
        let entry = self
            .active_editor()
            .and_then(|editor| editor.read(cx).debug_entry_point());
        let Some(entry) = entry else {
            self.set_notice(
                StatusKind::Info,
                "Nothing to debug here: place the caret in a `main` method or a test".to_owned(),
                cx,
            );
            return;
        };
        let target = entry.qualified_target();
        let label = target.rsplit('.').next().unwrap_or(&target).to_owned();
        let command = self
            .debugger
            .start(label, target, None, self.project.root_owned());
        self.dock_layout.bottom_open = Some(DockToolId::Debug);
        self.send_debug([command]);
        self.refresh_debug_gutters(cx);
    }

    /// Applies an update reported by the host for `session_id`.
    pub fn apply_debug_update(
        &mut self,
        session_id: u64,
        update: DebugUpdate,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let reveal = matches!(update, DebugUpdate::Stack { .. });
        let suspended = matches!(
            update,
            DebugUpdate::Status(DebuggerStatus::Suspended { .. })
        );
        let follow_up = self.debugger.apply(session_id, update);
        self.send_debug(follow_up);
        if suspended {
            self.dock_layout.bottom_open = Some(DockToolId::Debug);
        }
        if reveal {
            self.reveal_execution_point(window, cx);
        }
        self.refresh_debug_gutters(cx);
        cx.notify();
    }

    fn reveal_execution_point(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(location) = self.debugger.execution_point() else {
            return;
        };
        let key = self.debug_doc_key(&location.path);
        self.open_path(&key, Some((location.line as usize, 1)), window, cx);
    }

    /// Handles a click inside the *Debugger* dock.
    fn handle_debugger_action(
        &mut self,
        action: &DebuggerAction,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.dock_layout.focus(FocusedRegion::BottomPanel);
        match action {
            DebuggerAction::Resume => {
                let command = self.debugger.resume_command();
                self.send_debug(command);
            }
            DebuggerAction::Step(kind) => self.debug_step(*kind, cx),
            DebuggerAction::Stop => self.send_debug([DebugCommand::Stop]),
            DebuggerAction::OpenEvaluate => self.open_evaluate(cx),
            DebuggerAction::SelectThread(index) => {
                let command = self.debugger.select_thread(*index);
                self.send_debug(command);
            }
            DebuggerAction::SelectFrame(index) => {
                let commands = self.debugger.select_frame(*index);
                self.send_debug(commands);
                self.reveal_execution_point(window, cx);
                self.refresh_debug_gutters(cx);
            }
            DebuggerAction::ToggleVariable(index) => {
                let command = self.debugger.toggle_variable(*index);
                self.send_debug(command);
            }
            DebuggerAction::LoadMore(index) => {
                let command = self.debugger.load_more(*index);
                self.send_debug(command);
            }
            DebuggerAction::RemoveWatch(index) => self.debugger.remove_watch(*index),
        }
        cx.notify();
    }

    /// Routes a key to the open *Evaluate Expression* dialog. Returns `false` when closed.
    pub(super) fn handle_evaluate_key(
        &mut self,
        keystroke: &gpui::Keystroke,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let Some(modal) = self.debugger.evaluate_modal_mut() else {
            return false;
        };
        let mods = keystroke.modifiers;
        let text = if mods.control || mods.platform {
            None
        } else {
            keystroke.key_char.as_deref()
        };
        match modal.handle_key(&keystroke.key, text) {
            EvaluateKeyOutcome::Consumed => {}
            EvaluateKeyOutcome::Close => self.debugger.close_evaluate(),
            EvaluateKeyOutcome::Submit => {
                let command = self.debugger.evaluate_command();
                self.send_debug(command);
            }
        }
        cx.notify();
        true
    }

    fn debugger_colors(p: Palette) -> DebuggerColors {
        DebuggerColors::islands_dark(
            p.text_primary,
            p.text_secondary,
            p.text_muted,
            p.hover,
            p.active,
            p.border_subtle,
            p.island,
        )
    }

    fn debugger_handler(cx: &Context<'_, Self>) -> DebuggerHandler {
        Rc::new(cx.listener(|this, action: &DebuggerAction, window, cx| {
            this.handle_debugger_action(action, window, cx);
        }))
    }

    /// Body of the *Debugger* bottom tab.
    pub(super) fn render_debug_body(&self, p: Palette, cx: &Context<'_, Self>) -> AnyElement {
        let colors = Self::debugger_colors(p);
        if matches!(self.debugger.status(), DebuggerStatus::Idle)
            && self.debugger.breakpoints().is_empty()
        {
            return debugger_panel::render_empty(colors).into_any_element();
        }
        let handler = Self::debugger_handler(cx);
        div()
            .flex_1()
            .w_full()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .child(debugger_panel::render_toolbar(
                &self.debugger,
                colors,
                &handler,
            ))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .flex()
                    .flex_row()
                    .child(
                        div()
                            .w(relative(0.32))
                            .h_full()
                            .border_r_1()
                            .border_color(colors.border)
                            .child(debugger_panel::render_threads_and_frames(
                                &self.debugger,
                                colors,
                                &handler,
                            )),
                    )
                    .child(
                        div()
                            .w(relative(0.40))
                            .h_full()
                            .border_r_1()
                            .border_color(colors.border)
                            .child(debugger_panel::render_variables_and_watches(
                                &self.debugger,
                                colors,
                                &handler,
                            )),
                    )
                    .child(
                        div()
                            .flex_1()
                            .h_full()
                            .child(debugger_panel::render_console(&self.debugger, colors)),
                    ),
            )
            .into_any_element()
    }

    /// The *Evaluate Expression* overlay, while open.
    pub(super) fn render_evaluate_overlay(
        &self,
        p: Palette,
        cx: &Context<'_, Self>,
    ) -> Option<Div> {
        let modal = self.debugger.evaluate_modal()?;
        let evaluate = cx.listener(|this, (): &(), _window, cx| {
            let command = this.debugger.evaluate_command();
            this.send_debug(command);
            cx.notify();
        });
        let close = cx.listener(|this, (): &(), _window, cx| {
            this.debugger.close_evaluate();
            cx.notify();
        });
        let on_evaluate: EvaluateButton = Rc::new(move |window, cx| evaluate(&(), window, cx));
        let on_close: EvaluateButton = Rc::new(move |window, cx| close(&(), window, cx));
        Some(evaluate_modal::render(
            modal,
            self.debugger.selected_frame().map(|f| f.name.as_str()),
            Self::debugger_colors(p),
            on_evaluate,
            on_close,
        ))
    }
}
