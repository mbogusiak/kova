//! Headless code-completion popup state machine, bounds calculator, and insertion builder (`KOVA-052`).
//!
//! No GPUI windowing calls live here, so navigation, live prefix filtering, `Enter`/`Tab`
//! insertion, `Esc` dismissal, and viewport edge-flipping are 100% unit-tested headless.

use kova_core::completion::{
    CompletionProposal, CompletionProposalKind, CompletionTextEdit, filter_and_rank_proposals,
};

/// Default popup width in logical pixels (wide enough for `name(params)  package  type`).
pub const COMPLETION_POPUP_WIDTH_PX: f32 = 560.0;

/// `IntelliJ` New UI kind-icon tint (`0xRRGGBB`) for `kind` (`KOVA-074`): blue `C` class/record,
/// green `I` interface/annotation, orange `E` enum, red `m` method, purple `f` field, grey otherwise.
#[must_use]
pub const fn completion_icon_rgb(kind: CompletionProposalKind) -> u32 {
    match kind {
        CompletionProposalKind::Class
        | CompletionProposalKind::Record
        | CompletionProposalKind::Struct => 0x0054_8AF7,
        CompletionProposalKind::Interface
        | CompletionProposalKind::Annotation
        | CompletionProposalKind::Trait => 0x005F_B865,
        CompletionProposalKind::Enum => 0x00E0_8855,
        CompletionProposalKind::Method | CompletionProposalKind::Constructor => 0x00F7_5464,
        CompletionProposalKind::Field => 0x00C7_7DBB,
        CompletionProposalKind::Variable => 0x0056_A8F5,
        CompletionProposalKind::Module | CompletionProposalKind::Keyword => 0x0086_8A91,
    }
}
/// Fixed row height in logical pixels (`uniform_list` requirement).
pub const COMPLETION_ROW_HEIGHT_PX: f32 = 24.0;
/// Maximum number of visible rows before scrolling.
pub const COMPLETION_MAX_VISIBLE_ROWS: usize = 10;
/// Vertical padding inside the `uniform_list` container (`top + bottom`).
pub const COMPLETION_LIST_PADDING_Y_PX: f32 = 8.0;
const POPUP_VIEWPORT_MARGIN_PX: f32 = 8.0;
const POPUP_CARET_GAP_PX: f32 = 2.0;

/// Caret anchor rectangle in window logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaretWindowRect {
    /// Caret left edge in window coordinates.
    pub x: f32,
    /// Top of the caret line in window coordinates.
    pub y: f32,
    /// Height of one editor line in logical pixels.
    pub line_height: f32,
}

/// Computed window bounds of the floating completion popup.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PopupBounds {
    /// Left edge in window coordinates.
    pub x: f32,
    /// Top edge in window coordinates.
    pub y: f32,
    /// Popup width in logical pixels.
    pub width: f32,
    /// Popup height in logical pixels.
    pub height: f32,
}

impl PopupBounds {
    /// Returns `true` when window point `(px_x, px_y)` lies inside these bounds.
    #[must_use]
    pub fn contains_point(self, px_x: f32, px_y: f32) -> bool {
        px_x >= self.x
            && px_x <= self.x + self.width
            && px_y >= self.y
            && px_y <= self.y + self.height
    }
}

/// Computes the popup position anchored below `caret`, flipping above the caret line when near
/// the bottom viewport edge and shifting left when near the right viewport edge.
#[must_use]
pub fn compute_completion_popup_bounds(
    caret: CaretWindowRect,
    popup_width: f32,
    popup_height: f32,
    viewport_px: (f32, f32),
) -> PopupBounds {
    let (vw, vh) = viewport_px;
    let width = popup_width.min((vw - POPUP_VIEWPORT_MARGIN_PX * 2.0).max(160.0));
    let max_x = (vw - width - POPUP_VIEWPORT_MARGIN_PX).max(POPUP_VIEWPORT_MARGIN_PX);
    let x = caret.x.clamp(POPUP_VIEWPORT_MARGIN_PX, max_x);

    let below_y = caret.y + caret.line_height + POPUP_CARET_GAP_PX;
    let y = if below_y + popup_height > vh - POPUP_VIEWPORT_MARGIN_PX {
        (caret.y - popup_height - POPUP_CARET_GAP_PX).max(POPUP_VIEWPORT_MARGIN_PX)
    } else {
        below_y.max(POPUP_VIEWPORT_MARGIN_PX)
    };

    PopupBounds {
        x,
        y,
        width,
        height: popup_height,
    }
}

/// One filtered completion entry with its match score and highlighted character indices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilteredCompletionItem {
    /// Underlying completion proposal.
    pub proposal: CompletionProposal,
    /// Ranking score for the current prefix.
    pub score: u32,
    /// Sorted 0-based character indices in `proposal.label` matched by the prefix.
    pub matched_char_indices: Vec<u32>,
}

/// Text edit payload produced when accepting a completion item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionInsertion {
    /// Replacement text inserted over `anchor_byte..caret_byte`.
    pub text: String,
    /// Byte offset inside `text` where the caret should land (`name(|)` vs `name()|`).
    pub cursor_offset_in_text: usize,
    /// Additional edits (e.g. `import ...;` from `jdt.ls`) applied atomically in the same undo step.
    pub additional_text_edits: Vec<CompletionTextEdit>,
}

/// Builds the [`CompletionInsertion`] for `proposal`, respecting callable parentheses and `::`
/// method references (`KOVA-052` §FR-4).
#[must_use]
pub fn build_insertion(
    proposal: &CompletionProposal,
    is_method_reference: bool,
) -> CompletionInsertion {
    let is_ref = is_method_reference || proposal.is_method_reference;
    if proposal.is_callable && !is_ref {
        let base = proposal
            .insert_text
            .strip_suffix("()")
            .unwrap_or(&proposal.insert_text);
        let text = format!("{base}()");
        let cursor_offset_in_text = if proposal.has_params {
            base.len() + 1
        } else {
            base.len() + 2
        };
        CompletionInsertion {
            text,
            cursor_offset_in_text,
            additional_text_edits: proposal.additional_text_edits.clone(),
        }
    } else {
        let text = proposal.insert_text.clone();
        let cursor_offset_in_text = text.len();
        CompletionInsertion {
            text,
            cursor_offset_in_text,
            additional_text_edits: proposal.additional_text_edits.clone(),
        }
    }
}

/// Outcome of handling one keystroke while the completion popup is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKeyOutcome {
    /// Dismiss the popup (`Esc`) and consume the key.
    Close,
    /// Move the active selection by `delta` rows (`Up`/`Down`/`Ctrl+P`/`Ctrl+N`) and consume.
    Move(isize),
    /// Accept the highlighted item (`Enter`/`Tab`) and consume.
    Accept,
    /// Dismiss the popup (`Left`/`Right`/`Home`/`End`) and let the editor handle the key.
    DismissAndPassThrough,
    /// Ignore in the popup controller so the editor / IME receives the key normally.
    PassThrough,
}

/// Classifies a GPUI [`gpui::Keystroke`] for the open code-completion popup (`KOVA-052`).
#[must_use]
pub fn classify_completion_key(keystroke: &gpui::Keystroke) -> CompletionKeyOutcome {
    let mods = &keystroke.modifiers;
    let key = keystroke.key.as_str();
    let plain = !mods.control && !mods.alt && !mods.platform && !mods.shift;

    if key == "escape" && plain {
        return CompletionKeyOutcome::Close;
    }
    if (key == "up" && plain)
        || (key == "p" && mods.control && !mods.alt && !mods.platform && !mods.shift)
    {
        return CompletionKeyOutcome::Move(-1);
    }
    if (key == "down" && plain)
        || (key == "n" && mods.control && !mods.alt && !mods.platform && !mods.shift)
    {
        return CompletionKeyOutcome::Move(1);
    }
    if (key == "enter" || key == "tab") && plain {
        return CompletionKeyOutcome::Accept;
    }
    if matches!(key, "left" | "right" | "home" | "end")
        && !mods.control
        && !mods.alt
        && !mods.platform
    {
        return CompletionKeyOutcome::DismissAndPassThrough;
    }
    CompletionKeyOutcome::PassThrough
}

/// Headless state machine for the floating code-completion popup.
#[derive(Debug, Clone, Default)]
pub struct CompletionPopupState {
    open: bool,
    request_id: u64,
    anchor_byte: usize,
    is_method_reference: bool,
    prefix: String,
    all_proposals: Vec<CompletionProposal>,
    filtered: Vec<FilteredCompletionItem>,
    selected: usize,
    is_incomplete: bool,
}

impl CompletionPopupState {
    /// Creates a closed completion popup state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the popup is currently open.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }

    /// Identifier of the request that populated this popup.
    #[must_use]
    pub const fn request_id(&self) -> u64 {
        self.request_id
    }

    /// Buffer byte offset where replacement starts (`anchor_byte..caret_byte`).
    #[must_use]
    pub const fn anchor_byte(&self) -> usize {
        self.anchor_byte
    }

    /// Current live filter prefix.
    #[must_use]
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// Currently visible filtered & ranked completion items.
    #[must_use]
    pub fn filtered(&self) -> &[FilteredCompletionItem] {
        &self.filtered
    }

    /// Index of the active row in [`Self::filtered`].
    #[must_use]
    pub const fn selected(&self) -> usize {
        self.selected
    }

    /// Whether the host capped the result (re-query on the next typed character, `KOVA-074`).
    #[must_use]
    pub const fn is_incomplete(&self) -> bool {
        self.is_incomplete
    }

    /// Marks the open result as incomplete (capped by the host).
    pub const fn set_incomplete(&mut self, is_incomplete: bool) {
        self.is_incomplete = is_incomplete;
    }

    /// Whether accepting the selected row would change nothing (it equals the typed prefix and has
    /// no extra edits), so `Enter` / `Tab` should fall through to the editor (`KOVA-074`).
    #[must_use]
    pub fn selected_insertion_is_noop(&self) -> bool {
        self.selected_insertion()
            .is_none_or(|ins| ins.text == self.prefix && ins.additional_text_edits.is_empty())
    }

    /// Opens the popup with `proposals` anchored at `anchor_byte` (does not open if `proposals` is empty).
    pub fn open_with(
        &mut self,
        request_id: u64,
        anchor_byte: usize,
        is_method_reference: bool,
        prefix: String,
        proposals: Vec<CompletionProposal>,
    ) {
        if proposals.is_empty() {
            self.close();
            return;
        }
        self.open = true;
        self.request_id = request_id;
        self.anchor_byte = anchor_byte;
        self.is_method_reference = is_method_reference;
        self.prefix = prefix;
        self.all_proposals = proposals;
        self.selected = 0;
        self.is_incomplete = false;
        self.refilter();
    }

    /// Merges asynchronous Level-2 (`jdt.ls`) proposals into the currently open popup for
    /// `request_id`, preserving the user's live `prefix` and active selection when possible.
    pub fn merge_lsp_proposals(&mut self, request_id: u64, lsp_proposals: Vec<CompletionProposal>) {
        if !self.open || self.request_id != request_id || lsp_proposals.is_empty() {
            return;
        }
        let prev_selected_key = self
            .filtered
            .get(self.selected)
            .map(|item| item.proposal.identity_key());

        for incoming in lsp_proposals {
            let incoming_key = incoming.identity_key();
            if let Some(existing) = self
                .all_proposals
                .iter_mut()
                .find(|p| p.identity_key() == incoming_key)
            {
                if !incoming.additional_text_edits.is_empty() {
                    existing.additional_text_edits = incoming.additional_text_edits;
                }
                if existing.return_type.is_none() {
                    existing.return_type = incoming.return_type;
                }
                if existing.params_signature.is_none() {
                    existing.params_signature = incoming.params_signature;
                    existing.has_params = incoming.has_params;
                }
            } else if let Some(existing_by_name) = self.all_proposals.iter_mut().find(|p| {
                !p.kind.is_type()
                    && !incoming.kind.is_type()
                    && p.label == incoming.label
                    && (p.params_signature.is_none() || incoming.params_signature.is_none())
            }) {
                if !incoming.additional_text_edits.is_empty() {
                    existing_by_name.additional_text_edits = incoming.additional_text_edits;
                }
            } else {
                self.all_proposals.push(incoming);
            }
        }

        self.refilter();
        if let Some(key) = prev_selected_key
            && let Some(idx) = self
                .filtered
                .iter()
                .position(|i| i.proposal.identity_key() == key)
        {
            self.selected = idx;
        }
    }

    /// Updates the live filter prefix and resets selection to the top match (`0`).
    pub fn set_prefix(&mut self, prefix: &str) {
        if self.prefix == prefix {
            return;
        }
        self.prefix.clear();
        self.prefix.push_str(prefix);
        self.selected = 0;
        self.refilter();
    }

    /// Moves the selection by `delta` rows with wrap-around.
    pub fn move_selection(&mut self, delta: isize) {
        let len = self.filtered.len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let len_isize = isize::try_from(len).unwrap_or(isize::MAX);
        let cur = isize::try_from(self.selected.min(len - 1)).unwrap_or(0);
        let next = (cur + delta).rem_euclid(len_isize);
        self.selected = usize::try_from(next).unwrap_or(0);
    }

    /// Returns the insertion payload for the currently selected item, if any.
    #[must_use]
    pub fn selected_insertion(&self) -> Option<CompletionInsertion> {
        self.insertion_at(self.selected)
    }

    /// Returns the insertion payload for the item at `index`, if valid.
    #[must_use]
    pub fn insertion_at(&self, index: usize) -> Option<CompletionInsertion> {
        self.filtered
            .get(index)
            .map(|item| build_insertion(&item.proposal, self.is_method_reference))
    }

    /// Closes the popup and clears cached items.
    pub fn close(&mut self) {
        self.open = false;
        self.anchor_byte = 0;
        self.is_method_reference = false;
        self.prefix.clear();
        self.all_proposals.clear();
        self.filtered.clear();
        self.selected = 0;
        self.is_incomplete = false;
    }

    fn refilter(&mut self) {
        self.filtered = filter_and_rank_proposals(&self.all_proposals, &self.prefix)
            .into_iter()
            .map(|r| FilteredCompletionItem {
                proposal: r.proposal,
                score: r.score,
                matched_char_indices: r.matched_char_indices,
            })
            .collect();
        if self.filtered.is_empty() {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(self.filtered.len() - 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use kova_core::completion::{
        CompletionProposalKind, CompletionSource, CompletionTriggerKind,
        extract_completion_context, resolve_local_completion,
    };

    use super::*;
    use crate::editor::state::EditorState;

    fn sample_proposal(
        label: &str,
        kind: CompletionProposalKind,
        params: Option<&str>,
        ret: Option<&str>,
        has_params: bool,
    ) -> CompletionProposal {
        CompletionProposal {
            label: label.to_owned(),
            kind,
            params_signature: params.map(str::to_owned),
            return_type: ret.map(str::to_owned),
            detail_right: None,
            insert_text: label.to_owned(),
            is_callable: matches!(
                kind,
                CompletionProposalKind::Method | CompletionProposalKind::Constructor
            ),
            has_params,
            is_method_reference: false,
            additional_text_edits: Vec::new(),
            source: CompletionSource::JdkBuiltin,
            is_deprecated: false,
            sort_priority: 0,
            is_static: false,
            type_params: None,
            fqcn: None,
        }
    }

    #[test]
    fn popup_navigation_wraps_and_live_filtering_updates_selection() {
        let mut popup = CompletionPopupState::new();
        popup.open_with(
            1,
            10,
            false,
            String::new(),
            vec![
                sample_proposal(
                    "equals",
                    CompletionProposalKind::Method,
                    Some("(Object a, Object b)"),
                    Some("boolean"),
                    true,
                ),
                sample_proposal(
                    "isNull",
                    CompletionProposalKind::Method,
                    Some("(Object obj)"),
                    Some("boolean"),
                    true,
                ),
                sample_proposal(
                    "requireNonNull",
                    CompletionProposalKind::Method,
                    Some("(T obj)"),
                    Some("T"),
                    true,
                ),
            ],
        );
        assert!(popup.is_open());
        assert_eq!(popup.filtered().len(), 3);
        assert_eq!(popup.selected(), 0);

        // Wrap upward from 0 -> last
        popup.move_selection(-1);
        assert_eq!(popup.selected(), 2);
        // Wrap downward from last -> 0
        popup.move_selection(1);
        assert_eq!(popup.selected(), 0);

        // Live filter `req` -> `requireNonNull`
        popup.set_prefix("req");
        assert_eq!(popup.filtered().len(), 1);
        assert_eq!(popup.filtered()[0].proposal.label, "requireNonNull");
        assert_eq!(popup.selected(), 0);

        popup.close();
        assert!(!popup.is_open());
        assert_eq!(popup.filtered(), []);
    }

    #[test]
    fn build_insertion_places_cursor_inside_or_after_parens_and_skips_for_method_ref_and_fields() {
        let with_params = sample_proposal(
            "requireNonNull",
            CompletionProposalKind::Method,
            Some("(T obj)"),
            Some("T"),
            true,
        );
        let ins_params = build_insertion(&with_params, false);
        assert_eq!(ins_params.text, "requireNonNull()");
        assert_eq!(ins_params.cursor_offset_in_text, "requireNonNull(".len());

        let no_params = sample_proposal(
            "toString",
            CompletionProposalKind::Method,
            Some("()"),
            Some("String"),
            false,
        );
        let ins_no_params = build_insertion(&no_params, false);
        assert_eq!(ins_no_params.text, "toString()");
        assert_eq!(ins_no_params.cursor_offset_in_text, "toString()".len());

        let method_ref = build_insertion(&with_params, true);
        assert_eq!(method_ref.text, "requireNonNull");
        assert_eq!(method_ref.cursor_offset_in_text, "requireNonNull".len());

        let field = sample_proposal(
            "out",
            CompletionProposalKind::Field,
            None,
            Some("PrintStream"),
            false,
        );
        let ins_field = build_insertion(&field, false);
        assert_eq!(ins_field.text, "out");
        assert_eq!(ins_field.cursor_offset_in_text, 3);
    }

    #[test]
    fn popup_bounds_flips_above_caret_at_bottom_and_shifts_left_at_right_edge() {
        let normal = compute_completion_popup_bounds(
            CaretWindowRect {
                x: 120.0,
                y: 200.0,
                line_height: 20.0,
            },
            360.0,
            240.0,
            (1440.0, 900.0),
        );
        assert_eq!(normal.x, 120.0);
        assert_eq!(normal.y, 222.0);

        let bottom_edge = compute_completion_popup_bounds(
            CaretWindowRect {
                x: 120.0,
                y: 800.0,
                line_height: 20.0,
            },
            360.0,
            240.0,
            (1440.0, 900.0),
        );
        assert_eq!(bottom_edge.y, 800.0 - 240.0 - 2.0);

        let right_edge = compute_completion_popup_bounds(
            CaretWindowRect {
                x: 1350.0,
                y: 200.0,
                line_height: 20.0,
            },
            360.0,
            240.0,
            (1440.0, 900.0),
        );
        assert_eq!(right_edge.x, 1440.0 - 360.0 - 8.0);
    }

    #[test]
    fn end_to_end_typing_objects_dot_filter_req_and_accept_updates_editor_state() {
        let mut editor = EditorState::from_text("class Demo { void f() { Objects } }\n");
        let caret = editor.snapshot().text().find("Objects }").unwrap() + "Objects".len();
        editor.set_caret(caret);

        // 1. User types '.'
        assert!(editor.insert_text("."));
        let snap = editor.snapshot();
        let path = std::path::PathBuf::from("Demo.java");
        let ctx = extract_completion_context(
            &snap,
            &path,
            editor.primary_head(),
            CompletionTriggerKind::AutoChar('.'),
        )
        .expect("completion context after Objects.");
        let res = resolve_local_completion(&ctx, &snap.text(), &[], None, &[]);

        let mut popup = CompletionPopupState::new();
        popup.open_with(
            1,
            ctx.anchor_byte,
            ctx.is_method_reference,
            ctx.prefix.clone(),
            res.proposals,
        );
        assert!(popup.is_open());
        let labels: Vec<&str> = popup
            .filtered()
            .iter()
            .map(|i| i.proposal.label.as_str())
            .collect();
        assert!(labels.contains(&"requireNonNull"));
        assert!(labels.contains(&"isNull"));
        assert!(labels.contains(&"nonNull"));
        assert!(labels.contains(&"equals"));
        assert!(labels.contains(&"hash"));
        assert!(labels.contains(&"toString"));

        // 2. User types "req" -> filters to requireNonNull*
        assert!(editor.insert_text("req"));
        let snap2 = editor.snapshot();
        let live_prefix = snap2
            .slice_bytes(popup.anchor_byte()..editor.primary_head())
            .unwrap()
            .to_string();
        popup.set_prefix(&live_prefix);
        assert_ne!(popup.filtered(), []);
        assert_eq!(popup.filtered()[0].proposal.label, "requireNonNull");

        // 3. Accept with Enter/Tab -> inserts requireNonNull() with cursor inside `()`
        let insertion = popup.selected_insertion().unwrap();
        let replace_range = popup.anchor_byte()..editor.primary_head();
        let edit = kova_buffer::TextEdit::new(replace_range.clone(), &insertion.text);
        assert!(editor.apply_edits(&[edit], kova_buffer::EditOrigin::Compound));
        editor.set_caret(replace_range.start + insertion.cursor_offset_in_text);
        popup.close();

        assert!(!popup.is_open());
        let final_text = editor.snapshot().text();
        assert!(final_text.contains("Objects.requireNonNull()"));
        let expected_caret = final_text.find("requireNonNull()").unwrap() + "requireNonNull(".len();
        assert_eq!(editor.primary_head(), expected_caret);
    }

    #[test]
    fn lsp_merge_preserves_prefix_and_esc_closes_cleanly() {
        let mut popup = CompletionPopupState::new();
        popup.open_with(
            42,
            10,
            false,
            String::new(),
            vec![sample_proposal(
                "requireNonNull",
                CompletionProposalKind::Method,
                Some("(T obj)"),
                Some("T"),
                true,
            )],
        );
        // User types `req` while LSP is still resolving.
        popup.set_prefix("req");

        // Stale request_id is ignored.
        popup.merge_lsp_proposals(
            41,
            vec![sample_proposal(
                "requireNonNullElse",
                CompletionProposalKind::Method,
                Some("(T obj, T defaultObj)"),
                Some("T"),
                true,
            )],
        );
        assert_eq!(popup.filtered().len(), 1);

        // Matching request_id merges and filters against live `req` prefix.
        let mut lsp_item = sample_proposal(
            "requireNonNullElse",
            CompletionProposalKind::Method,
            Some("(T obj, T defaultObj)"),
            Some("T"),
            true,
        );
        lsp_item.additional_text_edits.push(CompletionTextEdit {
            start_utf16_line: 0,
            start_utf16_col: 0,
            end_utf16_line: 0,
            end_utf16_col: 0,
            new_text: "import java.util.Objects;\n".to_owned(),
        });
        popup.merge_lsp_proposals(42, vec![lsp_item]);
        assert_eq!(popup.filtered().len(), 2);
        assert_eq!(popup.filtered()[0].proposal.label, "requireNonNull");
        assert_eq!(popup.filtered()[1].proposal.label, "requireNonNullElse");

        // Esc closes popup.
        popup.close();
        assert!(!popup.is_open());
        assert!(popup.selected_insertion().is_none());
    }

    fn type_proposal(label: &str, fqcn: &str, kind: CompletionProposalKind) -> CompletionProposal {
        let mut p = sample_proposal(label, kind, None, None, false);
        p.fqcn = Some(fqcn.to_owned());
        p.detail_right = fqcn.rsplit_once('.').map(|(pkg, _)| pkg.to_owned());
        p
    }

    #[test]
    fn enter_passes_through_when_selection_equals_typed_text_and_popup_tracks_incomplete() {
        let mut popup = CompletionPopupState::new();
        let keyword = sample_proposal("else", CompletionProposalKind::Keyword, None, None, false);
        popup.open_with(1, 0, false, "else".to_owned(), vec![keyword]);
        assert!(popup.selected_insertion_is_noop());

        let mut import = type_proposal(
            "Objects",
            "java.util.Objects",
            CompletionProposalKind::Class,
        );
        import.additional_text_edits.push(CompletionTextEdit {
            start_utf16_line: 1,
            start_utf16_col: 0,
            end_utf16_line: 1,
            end_utf16_col: 0,
            new_text: "import java.util.Objects;\n".to_owned(),
        });
        popup.open_with(2, 0, false, "Objects".to_owned(), vec![import]);
        assert!(
            !popup.selected_insertion_is_noop(),
            "accepting still adds the import"
        );
        assert!(!popup.is_incomplete());
        popup.set_incomplete(true);
        assert!(popup.is_incomplete());
        popup.close();
        assert!(!popup.is_incomplete());
    }

    #[test]
    fn lsp_merge_keeps_same_named_types_from_different_packages_apart() {
        let mut popup = CompletionPopupState::new();
        popup.open_with(
            7,
            0,
            false,
            "Obj".to_owned(),
            vec![type_proposal(
                "Objects",
                "java.util.Objects",
                CompletionProposalKind::Class,
            )],
        );
        let mut lsp_same = type_proposal(
            "Objects",
            "java.util.Objects",
            CompletionProposalKind::Class,
        );
        lsp_same.additional_text_edits.push(CompletionTextEdit {
            start_utf16_line: 0,
            start_utf16_col: 0,
            end_utf16_line: 0,
            end_utf16_col: 0,
            new_text: "import java.util.Objects;\n".to_owned(),
        });
        let lsp_other = type_proposal(
            "Objects",
            "com.google.common.base.Objects",
            CompletionProposalKind::Class,
        );
        popup.merge_lsp_proposals(7, vec![lsp_same, lsp_other]);
        let pkgs: Vec<Option<&str>> = popup
            .filtered()
            .iter()
            .map(|i| i.proposal.detail_right.as_deref())
            .collect();
        assert_eq!(pkgs.len(), 2);
        assert!(pkgs.contains(&Some("java.util")));
        assert!(pkgs.contains(&Some("com.google.common.base")));
        let util = popup
            .filtered()
            .iter()
            .find(|i| i.proposal.fqcn.as_deref() == Some("java.util.Objects"))
            .unwrap();
        assert_eq!(util.proposal.additional_text_edits.len(), 1);
    }

    #[test]
    fn kind_icons_follow_intellij_letters_and_tints() {
        assert_eq!(CompletionProposalKind::Class.icon_letter(), 'C');
        assert_eq!(CompletionProposalKind::Interface.icon_letter(), 'I');
        assert_eq!(CompletionProposalKind::Method.icon_letter(), 'm');
        assert_eq!(CompletionProposalKind::Enum.icon_letter(), 'E');
        assert_eq!(
            completion_icon_rgb(CompletionProposalKind::Class),
            0x0054_8AF7
        );
        assert_eq!(
            completion_icon_rgb(CompletionProposalKind::Interface),
            0x005F_B865
        );
        assert_eq!(
            completion_icon_rgb(CompletionProposalKind::Method),
            0x00F7_5464
        );
        let mut m = sample_proposal(
            "checkIndex",
            CompletionProposalKind::Method,
            Some("(int index, int length)"),
            Some("int"),
            true,
        );
        m.is_static = true;
        assert_eq!(
            m.inline_detail().as_deref(),
            Some("(int index, int length)")
        );
        assert_eq!(m.right_label(), Some("int"));
        let mut t = type_proposal(
            "ObjIntConsumer",
            "java.util.function.ObjIntConsumer",
            CompletionProposalKind::Interface,
        );
        t.type_params = Some("<T>".to_owned());
        assert_eq!(t.inline_detail().as_deref(), Some("<T> java.util.function"));
        assert_eq!(t.right_label(), None);
    }
}
