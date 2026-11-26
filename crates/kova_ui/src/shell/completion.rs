//! Floating code-completion popup controller and overlay renderer for [`WorkspaceShell`] (`KOVA-052`).

use std::ops::Range;

use gpui::{
    ClickEvent, Context, Div, FontWeight, HighlightStyle, Hsla, InteractiveElement as _,
    MouseButton, MouseDownEvent, ParentElement as _, ScrollStrategy, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled, StyledText, Window, div, px, uniform_list,
};
use kova_core::completion::{
    CompletionProposalKind, CompletionTriggerKind, extract_completion_context,
    resolve_local_completion,
};

use super::WorkspaceShell;
use crate::controls::ControlPalette;
use crate::document::{CompletionOutcome, CompletionRequest};
use crate::editor::EditorView;
use crate::editor::completion::{
    COMPLETION_LIST_PADDING_Y_PX, COMPLETION_MAX_VISIBLE_ROWS, COMPLETION_POPUP_WIDTH_PX,
    COMPLETION_ROW_HEIGHT_PX, CompletionKeyOutcome, FilteredCompletionItem, PopupBounds,
    classify_completion_key, compute_completion_popup_bounds,
};
use crate::search::char_indices_to_byte_ranges;
use crate::theme::{KovaTheme, hsla_from_rgba};

impl WorkspaceShell {
    /// Applies an asynchronous or immediate code-completion result from the host (`KOVA-052`).
    pub fn apply_completion_result(
        &mut self,
        request_id: u64,
        outcome: CompletionOutcome,
        cx: &mut Context<'_, Self>,
    ) {
        if self.pending_completion_request_id != Some(request_id)
            && !(self.completion_popup.is_open()
                && self.completion_popup.request_id() == request_id)
        {
            return;
        }

        match outcome {
            CompletionOutcome::Ready {
                anchor_byte,
                is_method_reference,
                initial_prefix,
                proposals,
                is_final,
            } => {
                if self.completion_popup.is_open()
                    && self.completion_popup.request_id() == request_id
                {
                    self.completion_popup
                        .merge_lsp_proposals(request_id, proposals);
                    self.completion_scroll
                        .scroll_to_item(self.completion_popup.selected(), ScrollStrategy::Top);
                } else if !proposals.is_empty() {
                    let live_prefix = if self.active_editor().is_some() {
                        let Some(prefix) =
                            self.live_completion_prefix_from_active_editor(anchor_byte, cx)
                        else {
                            if is_final {
                                self.pending_completion_request_id = None;
                            }
                            return;
                        };
                        prefix
                    } else {
                        initial_prefix
                    };
                    self.completion_popup.open_with(
                        request_id,
                        anchor_byte,
                        is_method_reference,
                        live_prefix,
                        proposals,
                    );
                    self.completion_scroll
                        .scroll_to_item(0, ScrollStrategy::Top);
                }
                if is_final {
                    self.pending_completion_request_id = None;
                }
            }
            CompletionOutcome::Empty { is_final } => {
                if is_final {
                    self.pending_completion_request_id = None;
                }
            }
        }
        cx.notify();
    }

    pub(super) fn trigger_completion(
        &mut self,
        trigger_kind: CompletionTriggerKind,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(source_path) = self.active_document_key() else {
            return;
        };
        let Some(active_tab_id) = self
            .panes
            .get(&self.active_pane)
            .and_then(|pane| pane.tabs.active_tab_id())
        else {
            return;
        };
        let Some(editor) = self.active_editor().cloned() else {
            return;
        };

        let (snapshot, cursor_byte, can_complete) = {
            let ed = editor.read(cx);
            let editable =
                !ed.state().is_read_only() && !ed.vim_owns_keys() && ed.cursor_count() == 1;
            (ed.snapshot(), ed.state().primary_head(), editable)
        };
        if !can_complete {
            return;
        }

        let request_id = self.next_completion_request_id;
        self.next_completion_request_id = self.next_completion_request_id.wrapping_add(1);
        self.pending_completion_request_id = Some(request_id);
        self.completion_origin = Some((self.active_pane, active_tab_id));

        let open_buffers: Vec<(std::path::PathBuf, String)> = self
            .documents
            .iter()
            .map(|(path, buf)| (path.clone(), buf.snapshot().text()))
            .collect();
        let workspace_files: Vec<std::path::PathBuf> = self
            .project_tree
            .file_paths()
            .map(std::path::Path::to_path_buf)
            .collect();
        let source_text = snapshot.text();

        if let Some(sink) = self.completion_sink.clone() {
            sink(CompletionRequest {
                request_id,
                project_root: self.project.root_owned(),
                source_path,
                source_text,
                cursor_byte,
                trigger_kind,
                open_buffers,
                workspace_files,
            });
            return;
        }

        // Standalone shell fallback without a host sink: run Level-1 locally.
        let outcome =
            extract_completion_context(&snapshot, &source_path, cursor_byte, trigger_kind).map_or(
                CompletionOutcome::Empty { is_final: true },
                |ctx| {
                    let local = resolve_local_completion(
                        &ctx,
                        &source_text,
                        &open_buffers,
                        self.project.root(),
                        &workspace_files,
                    );
                    if local.proposals.is_empty() {
                        CompletionOutcome::Empty { is_final: true }
                    } else {
                        CompletionOutcome::Ready {
                            anchor_byte: ctx.anchor_byte,
                            is_method_reference: ctx.is_method_reference,
                            initial_prefix: ctx.prefix,
                            proposals: local.proposals,
                            is_final: true,
                        }
                    }
                },
            );
        self.apply_completion_result(request_id, outcome, cx);
    }

    pub(super) fn close_completion_popup(&mut self, cx: &mut Context<'_, Self>) {
        if self.completion_popup.is_open()
            || self.pending_completion_request_id.is_some()
            || self.completion_origin.is_some()
        {
            self.completion_popup.close();
            self.pending_completion_request_id = None;
            self.completion_origin = None;
            cx.notify();
        }
    }

    pub(super) fn sync_completion_popup_after_edit(
        &mut self,
        editor: &gpui::Entity<EditorView>,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.completion_popup.is_open() {
            return;
        }
        let anchor = self.completion_popup.anchor_byte();
        let (snapshot, caret, cursor_count, read_only, vim_owns) = {
            let ed = editor.read(cx);
            (
                ed.snapshot(),
                ed.state().primary_head(),
                ed.cursor_count(),
                ed.state().is_read_only(),
                ed.vim_owns_keys(),
            )
        };
        if read_only || vim_owns || cursor_count != 1 || caret < anchor {
            self.close_completion_popup(cx);
            return;
        }
        let Some(slice) = snapshot
            .slice_bytes(anchor..caret)
            .ok()
            .map(|s| s.to_string())
        else {
            self.close_completion_popup(cx);
            return;
        };
        if slice.contains('\n') || !slice.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            self.close_completion_popup(cx);
            return;
        }
        self.completion_popup.set_prefix(&slice);
        self.completion_scroll
            .scroll_to_item(self.completion_popup.selected(), ScrollStrategy::Top);
        cx.notify();
    }

    pub(super) fn handle_completion_key_capture(
        &mut self,
        keystroke: &gpui::Keystroke,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        if !self.completion_popup.is_open() {
            return false;
        }
        match classify_completion_key(keystroke) {
            CompletionKeyOutcome::Close => {
                self.close_completion_popup(cx);
                cx.stop_propagation();
                true
            }
            CompletionKeyOutcome::Move(delta) => {
                self.completion_popup.move_selection(delta);
                self.completion_scroll
                    .scroll_to_item(self.completion_popup.selected(), ScrollStrategy::Top);
                cx.stop_propagation();
                cx.notify();
                true
            }
            CompletionKeyOutcome::Accept => {
                if self.completion_popup.selected_insertion().is_some() {
                    self.accept_completion_selection(window, cx);
                    cx.stop_propagation();
                    cx.notify();
                    return true;
                }
                self.close_completion_popup(cx);
                false
            }
            CompletionKeyOutcome::DismissAndPassThrough => {
                self.close_completion_popup(cx);
                false
            }
            CompletionKeyOutcome::PassThrough => false,
        }
    }

    pub(super) fn accept_completion_selection(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let selected = self.completion_popup.selected();
        self.accept_completion_at(selected, window, cx);
    }

    pub(super) fn accept_completion_at(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(insertion) = self.completion_popup.insertion_at(index) else {
            self.close_completion_popup(cx);
            return;
        };
        let anchor_byte = self.completion_popup.anchor_byte();
        self.close_completion_popup(cx);

        let Some(editor) = self.active_editor().cloned() else {
            return;
        };
        let caret_byte = editor.read(cx).state().primary_head();
        let replace_range = anchor_byte.min(caret_byte)..caret_byte;
        editor.update(cx, |ed, cx| {
            ed.apply_completion_insertion(replace_range, &insertion, window, cx);
        });
        window.focus(editor.read(cx).focus_handle());
        cx.notify();
    }

    pub(super) fn handle_completion_outside_click(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.completion_popup.is_open() {
            return;
        }
        let inside = self
            .current_completion_popup_bounds(cx)
            .is_some_and(|bounds| {
                bounds.contains_point(f32::from(event.position.x), f32::from(event.position.y))
            });
        if !inside {
            self.close_completion_popup(cx);
        }
    }

    fn live_completion_prefix_from_active_editor(
        &self,
        anchor_byte: usize,
        cx: &Context<'_, Self>,
    ) -> Option<String> {
        let editor = self.active_editor()?.read(cx);
        let caret = editor.state().primary_head();
        if caret < anchor_byte {
            return None;
        }
        let slice = editor
            .snapshot()
            .slice_bytes(anchor_byte..caret)
            .ok()?
            .to_string();
        (!slice.contains('\n') && slice.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
            .then_some(slice)
    }

    #[allow(
        clippy::cast_precision_loss,
        reason = "visible completion row count (<= 10) fits cleanly in f32"
    )]
    fn current_completion_popup_bounds(&self, cx: &Context<'_, Self>) -> Option<PopupBounds> {
        if !self.completion_popup.is_open() {
            return None;
        }
        let editor = self.active_editor()?.read(cx);
        let caret = editor
            .caret_window_rect_at(self.completion_popup.anchor_byte())
            .or_else(|| editor.primary_caret_window_rect())?;
        let rows = self
            .completion_popup
            .filtered()
            .len()
            .clamp(1, COMPLETION_MAX_VISIBLE_ROWS);
        let list_height = (rows as f32) * COMPLETION_ROW_HEIGHT_PX + COMPLETION_LIST_PADDING_Y_PX;
        Some(compute_completion_popup_bounds(
            caret,
            COMPLETION_POPUP_WIDTH_PX,
            list_height,
            self.viewport_px,
        ))
    }

    pub(super) fn render_completion_overlay(
        &self,
        cx: &Context<'_, Self>,
    ) -> Option<Stateful<Div>> {
        let bounds = self.current_completion_popup_bounds(cx)?;
        let theme = cx.global::<KovaTheme>();
        let elevated = hsla_from_rgba(theme.surfaces().elevated);
        let border_strong = hsla_from_rgba(theme.borders().strong);
        let text_muted = hsla_from_rgba(theme.text().muted);
        let count = self.completion_popup.filtered().len();

        let body = if count == 0 {
            div()
                .h(px(bounds.height))
                .w_full()
                .px(px(10.0))
                .flex()
                .items_center()
                .text_size(px(12.0))
                .text_color(text_muted)
                .child("No suggestions")
        } else {
            div().h(px(bounds.height)).w_full().child(
                uniform_list(
                    "code-completion-list",
                    count,
                    cx.processor(Self::render_completion_rows),
                )
                .track_scroll(self.completion_scroll.clone())
                .size_full()
                .py(px(4.0)),
            )
        };

        Some(
            div()
                .id("code-completion-popup")
                .absolute()
                .left(px(bounds.x))
                .top(px(bounds.y))
                .w(px(bounds.width))
                .rounded(px(8.0))
                .border_1()
                .border_color(border_strong)
                .bg(elevated)
                .overflow_hidden()
                .shadow_lg()
                .occlude()
                .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                })
                .child(body),
        )
    }

    fn render_completion_rows(
        &mut self,
        range: Range<usize>,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Vec<Stateful<Div>> {
        let theme = cx.global::<KovaTheme>().clone();
        let selected = self.completion_popup.selected();
        let items = self.completion_popup.filtered();
        range
            .filter_map(|row| {
                items
                    .get(row)
                    .map(|item| render_completion_row(row, item, row == selected, &theme, cx))
            })
            .collect()
    }
}

fn badge_color(kind: CompletionProposalKind, p: &ControlPalette) -> Hsla {
    match kind {
        CompletionProposalKind::Method | CompletionProposalKind::Constructor => p.primary.subtle_fg,
        CompletionProposalKind::Field | CompletionProposalKind::Variable => p.warning.subtle_fg,
        CompletionProposalKind::Class
        | CompletionProposalKind::Interface
        | CompletionProposalKind::Enum
        | CompletionProposalKind::Struct
        | CompletionProposalKind::Trait
        | CompletionProposalKind::Module => p.accent.subtle_fg,
        CompletionProposalKind::Keyword => p.muted.subtle_fg,
    }
}

fn render_completion_row(
    row: usize,
    item: &FilteredCompletionItem,
    selected: bool,
    theme: &KovaTheme,
    cx: &Context<'_, WorkspaceShell>,
) -> Stateful<Div> {
    let p = ControlPalette::from_theme(theme);
    let active_bg = hsla_from_rgba(theme.surfaces().active);
    let tab_indicator = hsla_from_rgba(theme.borders().tab_indicator);
    let (left_el, right_el) = render_completion_row_content(item, &p);

    let mut el = div()
        .id(("completion-row", row))
        .relative()
        .h(px(COMPLETION_ROW_HEIGHT_PX))
        .w_full()
        .px(px(8.0))
        .flex()
        .items_center()
        .justify_between()
        .gap(px(6.0))
        .cursor_pointer()
        .hover(|s| s.bg(p.hover))
        .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, cx| {
            cx.stop_propagation();
        })
        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            cx.stop_propagation();
            this.accept_completion_at(row, window, cx);
        }));

    if selected {
        el = el.bg(active_bg).child(
            div()
                .absolute()
                .left(px(0.0))
                .top(px(3.0))
                .bottom(px(3.0))
                .w(px(2.5))
                .rounded(px(1.5))
                .bg(tab_indicator),
        );
    }

    el.child(left_el).children(right_el)
}

fn render_completion_row_content(
    item: &FilteredCompletionItem,
    p: &ControlPalette,
) -> (Div, Option<Div>) {
    let proposal = &item.proposal;
    let badge = proposal.kind.badge().to_string();
    let badge_fg = badge_color(proposal.kind, p);

    let highlights: Vec<_> =
        char_indices_to_byte_ranges(&proposal.label, &item.matched_char_indices)
            .into_iter()
            .map(|range| {
                (
                    range,
                    HighlightStyle {
                        color: Some(p.primary.subtle_fg),
                        font_weight: Some(FontWeight::BOLD),
                        ..HighlightStyle::default()
                    },
                )
            })
            .collect();

    let mut label_el = div()
        .text_size(px(12.5))
        .text_color(if proposal.is_deprecated {
            p.text_muted
        } else {
            p.text
        })
        .whitespace_nowrap()
        .child(StyledText::new(proposal.label.clone()).with_highlights(highlights));
    if proposal.is_deprecated {
        label_el = label_el.line_through();
    }

    let params_el = proposal.params_signature.as_deref().map(|params| {
        div()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_size(px(12.0))
            .text_color(p.text_muted)
            .child(SharedString::from(params.to_owned()))
    });

    let right_el = proposal.right_label().map(|right| {
        div()
            .flex_none()
            .pl(px(8.0))
            .text_size(px(11.0))
            .text_color(p.text_muted)
            .whitespace_nowrap()
            .child(SharedString::from(right.to_owned()))
    });

    let left_el = div()
        .flex_1()
        .min_w(px(0.0))
        .flex()
        .items_center()
        .gap(px(6.0))
        .overflow_hidden()
        .child(
            div()
                .w(px(17.0))
                .h(px(17.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.0))
                .bg(p.input)
                .text_size(px(10.0))
                .font_weight(FontWeight::BOLD)
                .text_color(badge_fg)
                .child(badge),
        )
        .child(label_el)
        .children(params_el);

    (left_el, right_el)
}
