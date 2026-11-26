//! Modal Vim bridge for [`crate::editor::EditorView`] and caret geometry (`KOVA-026` FR-4).
//!
//! The Vim grammar (motions, operators, text objects, registers) lives in `kova_core`'s
//! [`VimEngine`]. This module only keeps the engine in step with the editor's shared document:
//!
//! - before a key, the engine adopts the current document when a sibling split, the IME or an
//!   IDE action changed it ([`VimEngine::replace_buffer`]) and follows the editor caret;
//! - after a key, the engine's own edits are folded into **one** bounding [`TextEdit`] computed
//!   from the buffer edit log (no full-text diff) and recorded through the document
//!   [`kova_buffer::History`], so splits, the dirty badge and IDE undo stay correct;
//! - `u` / `Ctrl+R` use the document history, so Vim undo and `Cmd+Z` are one timeline;
//! - `:` / `/` / `?` open the status-bar command line owned by the workspace shell.
//!
//! Everything here is headless and unit-tested without a window.

use std::ops::Range;

use kova_buffer::{BufferVersion, EditOrigin, TextEdit, VersionChanges};
use kova_core::keymap::KeyStroke;
use kova_core::vim::{VimCursorShape, VimEngine, VimKey, VimMode, VimOutcome, VimStatus};

use crate::editor::state::EditorState;

/// Height ratio of the operator-pending half block (`row_height * 0.45`, rounded).
const HALF_BLOCK_RATIO: f32 = 0.45;
/// Thickness of the Replace-mode underline caret.
const UNDERLINE_HEIGHT_PX: f32 = 2.5;
/// Width of the Insert / non-Vim bar caret.
const BAR_WIDTH_PX: f32 = 2.0;

/// How the editor caret is painted (`editor_ux_guidelines.md` §4.1, `KOVA-026` FR-4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorRenderKind {
    /// `2 px` vertical bar: Vim `INSERT` or Vim disabled.
    #[default]
    Bar,
    /// Full-cell block with the glyph redrawn in `text.inverse`: `NORMAL` and `VISUAL*`.
    Block,
    /// Bottom `45 %` of the cell: operator pending (`d`, `c`, `y` waiting for a motion).
    HalfBlockBottom,
    /// `2.5 px` underline: Replace (`r{char}` waiting for its character).
    Underline,
}

/// Caret rectangle in logical pixels plus its corner radius.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaretRect {
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
    /// Width.
    pub width: f32,
    /// Height.
    pub height: f32,
    /// Corner radius.
    pub radius: f32,
}

impl CursorRenderKind {
    /// Maps the `kova_core` Vim status (`None` = Vim disabled) onto a caret kind.
    ///
    /// `kova_core` reports an operator-pending caret as [`VimCursorShape::Underline`]; this
    /// shell paints it as [`Self::HalfBlockBottom`] and reserves [`Self::Underline`] for
    /// Replace, keeping the `KOVA-026` visual language.
    #[must_use]
    pub fn from_vim_status(status: Option<&VimStatus>) -> Self {
        let Some(status) = status else {
            return Self::Bar;
        };
        if status.mode == VimMode::Insert {
            return Self::Bar;
        }
        if awaits_replace_char(&status.pending) {
            return Self::Underline;
        }
        match status.cursor_shape {
            VimCursorShape::Bar => Self::Bar,
            VimCursorShape::Underline => Self::HalfBlockBottom,
            VimCursorShape::Block => Self::Block,
        }
    }

    /// Whether the glyph under the caret is redrawn on top of it in `text.inverse`.
    #[must_use]
    pub const fn redraws_glyph(self) -> bool {
        matches!(self, Self::Block)
    }

    /// Computes the caret rectangle for a cell at `(x, y)`.
    ///
    /// `cell_width` is the advance of the glyph under the caret (at least one `char_advance`,
    /// also at the end of a line); `row_height` is the editor line height.
    #[must_use]
    pub fn rect(self, x: f32, y: f32, cell_width: f32, row_height: f32) -> CaretRect {
        let cell_width = cell_width.max(BAR_WIDTH_PX);
        match self {
            Self::Bar => CaretRect {
                x,
                y,
                width: BAR_WIDTH_PX,
                height: row_height,
                radius: 1.0,
            },
            Self::Block => CaretRect {
                x,
                y,
                width: cell_width,
                height: row_height,
                radius: 1.5,
            },
            Self::HalfBlockBottom => {
                let height = (row_height * HALF_BLOCK_RATIO).round();
                CaretRect {
                    x,
                    y: y + row_height - height,
                    width: cell_width,
                    height,
                    radius: 1.0,
                }
            }
            Self::Underline => CaretRect {
                x,
                y: y + row_height - UNDERLINE_HEIGHT_PX,
                width: cell_width,
                height: UNDERLINE_HEIGHT_PX,
                radius: 0.0,
            },
        }
    }
}

/// `true` when the pending Vim keys end in `r` waiting for the replacement character
/// (`r`, `3r`), but not for a register selection (`"r`).
fn awaits_replace_char(pending: &str) -> bool {
    let mut rev = pending.chars().rev();
    rev.next() == Some('r') && rev.next() != Some('"')
}

/// WCAG 2.x contrast ratio between two opaque `0xRRGGBB` colours (`1.0 ..= 21.0`).
#[must_use]
pub fn contrast_ratio(fg_rgb: u32, bg_rgb: u32) -> f32 {
    fn channel(value: u32, shift: u32) -> f32 {
        let c = f32::from(u8::try_from((value >> shift) & 0xFF).unwrap_or(0)) / 255.0;
        if c <= 0.039_28 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    fn luminance(rgb: u32) -> f32 {
        0.2126f32.mul_add(
            channel(rgb, 16),
            0.7152f32.mul_add(channel(rgb, 8), 0.0722 * channel(rgb, 0)),
        )
    }
    let (a, b) = (luminance(fg_rgb), luminance(bg_rgb));
    let (hi, lo) = if a >= b { (a, b) } else { (b, a) };
    (hi + 0.05) / (lo + 0.05)
}

/// Result of feeding one key to the [`VimBridge`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VimKeyResult {
    /// Vim consumed the key; `edited` is `true` when the document changed.
    Handled {
        /// Whether the shared document changed.
        edited: bool,
    },
    /// Vim does not own the key: resolve it against the IDE keymap.
    PassThrough,
    /// `:` / `/` / `?` in `NORMAL` / `VISUAL`: open the status-bar command line.
    OpenCommandLine(char),
    /// `gd` in `NORMAL` mode: jump to the definition of the symbol under the caret.
    GoToDefinition,
}

/// Keeps a [`VimEngine`] in step with one editor's shared document and selections.
#[derive(Debug)]
pub struct VimBridge {
    engine: VimEngine,
    /// Document version whose text the engine buffer holds (`None` forces a resync).
    ///
    /// The engine's forked buffer and the shared document count versions independently,
    /// so the engine's own version number is never compared with the document's.
    synced_doc: Option<BufferVersion>,
}

impl VimBridge {
    /// Creates a bridge in `NORMAL` mode over the current document of `state`, with the
    /// engine caret on the editor's primary caret.
    #[must_use]
    pub fn new(state: &mut EditorState) -> Self {
        let shared = state.shared_buffer();
        let buffer = shared.with_document(|buffer, _| buffer.clone());
        let mut bridge = Self {
            engine: VimEngine::new(buffer),
            synced_doc: Some(shared.version()),
        };
        bridge.engine.set_cursor(state.primary_head());
        bridge.mirror_selection(state);
        bridge
    }

    /// Current Vim mode.
    #[must_use]
    pub fn mode(&self) -> VimMode {
        self.engine.mode()
    }

    /// Status snapshot for the status bar (mode, pending keys, cursor shape).
    #[must_use]
    pub fn status(&self) -> VimStatus {
        self.engine.status_snapshot()
    }

    /// Caret kind for the current mode and pending state.
    #[must_use]
    pub fn cursor_render_kind(&self) -> CursorRenderKind {
        CursorRenderKind::from_vim_status(Some(&self.engine.status_snapshot()))
    }

    /// Whether Vim owns plain keys (every mode but `INSERT`).
    #[must_use]
    pub fn owns_keys(&self) -> bool {
        self.engine.mode() != VimMode::Insert
    }

    /// Feeds one key-down stroke.
    ///
    /// In `INSERT` mode the caller only forwards keys that leave the mode (`Escape`,
    /// `Ctrl+[`, `Ctrl+C`); text keeps flowing through the IME into [`EditorState`].
    pub fn handle_key(&mut self, state: &mut EditorState, stroke: KeyStroke) -> VimKeyResult {
        state.sync_external_edits();
        self.sync_from(state);
        let mode = self.engine.mode();
        let idle = mode != VimMode::Insert && self.engine.status_snapshot().pending.is_empty();
        if idle {
            match VimKey::from_stroke(stroke) {
                Some(VimKey::Char(prefix @ (':' | '/' | '?'))) => {
                    return VimKeyResult::OpenCommandLine(prefix);
                }
                Some(VimKey::Char('u')) if mode == VimMode::Normal => {
                    if state.is_read_only() {
                        return VimKeyResult::Handled { edited: false };
                    }
                    return self.after_history(state, EditorState::undo);
                }
                Some(VimKey::Ctrl('r')) if mode == VimMode::Normal => {
                    if state.is_read_only() {
                        return VimKeyResult::Handled { edited: false };
                    }
                    return self.after_history(state, EditorState::redo);
                }
                _ => {}
            }
        }

        let before = self.engine.buffer().version();
        match self.engine.handle_key(stroke) {
            VimOutcome::PassThrough => return VimKeyResult::PassThrough,
            VimOutcome::GoToDefinition => {
                self.mirror_selection(state);
                return VimKeyResult::GoToDefinition;
            }
            VimOutcome::Handled => {}
        }
        if state.is_read_only() {
            if self.engine.buffer().version() != before {
                let buffer = state
                    .shared_buffer()
                    .with_document(|buffer, _| buffer.clone());
                self.engine.replace_buffer(buffer);
                self.synced_doc = Some(state.shared_buffer().version());
            }
            if self.engine.mode() == VimMode::Insert {
                let _ = self.engine.handle_vim_key(VimKey::Escape);
            }
            self.mirror_selection(state);
            return VimKeyResult::Handled { edited: false };
        }
        let edited = self.write_back(state, before);
        self.mirror_selection(state);
        VimKeyResult::Handled { edited }
    }

    /// Feeds text committed by the IME / keyboard layout while Vim owns the keys
    /// (`fą`, `rż` with `Option`/`AltGr`). Text that completes no pending command is dropped,
    /// so `NORMAL` mode never types into the document.
    pub fn handle_text_input(&mut self, state: &mut EditorState, text: &str) -> VimKeyResult {
        state.sync_external_edits();
        self.sync_from(state);
        let before = self.engine.buffer().version();
        if self.engine.handle_text_input(text) == VimOutcome::PassThrough {
            return VimKeyResult::Handled { edited: false };
        }
        if state.is_read_only() {
            if self.engine.buffer().version() != before {
                let buffer = state
                    .shared_buffer()
                    .with_document(|buffer, _| buffer.clone());
                self.engine.replace_buffer(buffer);
                self.synced_doc = Some(state.shared_buffer().version());
            }
            self.mirror_selection(state);
            return VimKeyResult::Handled { edited: false };
        }
        let edited = self.write_back(state, before);
        self.mirror_selection(state);
        VimKeyResult::Handled { edited }
    }

    /// Returns to `NORMAL` mode (e.g. when the command line closes or Vim is re-enabled).
    pub fn force_normal(&mut self, state: &mut EditorState) {
        state.sync_external_edits();
        self.sync_from(state);
        if self.engine.mode() != VimMode::Normal {
            self.engine.handle_vim_key(VimKey::Escape);
            self.mirror_selection(state);
        }
    }

    fn after_history(
        &mut self,
        state: &mut EditorState,
        op: fn(&mut EditorState) -> bool,
    ) -> VimKeyResult {
        let edited = op(state);
        self.sync_from(state);
        self.mirror_selection(state);
        VimKeyResult::Handled { edited }
    }

    /// Adopts the document when it changed behind the engine and follows the editor caret.
    fn sync_from(&mut self, state: &EditorState) {
        let shared = state.shared_buffer();
        let doc_version = shared.version();
        if self.synced_doc != Some(doc_version) {
            let buffer = shared.with_document(|buffer, _| buffer.clone());
            self.engine.replace_buffer(buffer);
            self.synced_doc = Some(doc_version);
        }
        if !self.engine.mode().is_visual() {
            let head = state.primary_head();
            if head != self.engine.cursor() {
                self.engine.set_cursor(head);
            }
        }
    }

    /// Records the engine's edits since `before` into the document history as one edit.
    fn write_back(&mut self, state: &mut EditorState, before: BufferVersion) -> bool {
        let applied = self.apply_engine_edits(state, before);
        // On success the document now holds the engine text; otherwise resync next key.
        self.synced_doc = applied.then(|| state.shared_buffer().version());
        if !applied && self.engine.buffer().version() == before {
            self.synced_doc = Some(state.shared_buffer().version());
        }
        applied
    }

    fn apply_engine_edits(&self, state: &mut EditorState, before: BufferVersion) -> bool {
        let engine_buffer = self.engine.buffer();
        if engine_buffer.version() == before {
            return false;
        }
        let new_snap = engine_buffer.snapshot();
        let bounds = match engine_buffer.changes_since(before) {
            VersionChanges::Deltas { deltas, .. } => bounding_edit(deltas.iter().flat_map(|d| {
                d.edits.iter().map(|e| {
                    (
                        e.tree_sitter.start_byte,
                        e.tree_sitter.old_end_byte,
                        e.tree_sitter.new_end_byte,
                    )
                })
            })),
            VersionChanges::UpToDate => None,
            VersionChanges::FullResyncRequired { .. } => {
                diff_bounds(&state.snapshot().text(), &new_snap.text())
            }
        };
        let Some((old_range, new_range)) = bounds else {
            return false;
        };
        let replacement: String = new_snap
            .slice_bytes(new_range)
            .map(|s| s.chunks().collect())
            .unwrap_or_default();
        state.apply_edits(
            &[TextEdit::new(old_range, replacement)],
            EditOrigin::Compound,
        )
    }

    /// Mirrors the engine caret (or visual selection) into the editor selections.
    fn mirror_selection(&self, state: &mut EditorState) {
        let cursor = self.engine.cursor();
        match self.engine.selection() {
            Some(range) if cursor <= range.start => state.set_selection(range.end, range.start),
            Some(range) => state.set_selection(range.start, range.end),
            None => state.set_selection(cursor, cursor),
        }
    }
}

/// Folds sequential `(start, old_end, new_end)` byte edits into one edit.
///
/// Every edit is expressed in the coordinates produced by the previous one. Returns the
/// replaced range in the original text and the matching range in the final text.
#[must_use]
pub fn bounding_edit(
    edits: impl IntoIterator<Item = (usize, usize, usize)>,
) -> Option<(Range<usize>, Range<usize>)> {
    // (original start, original end, current start, current end)
    let mut region: Option<(usize, usize, usize, usize)> = None;
    for (start, old_end, new_end) in edits {
        region = Some(match region {
            None => (start, old_end, start, new_end),
            Some((orig_start, orig_end, cur_start, cur_end)) => {
                let orig_start = if start < cur_start {
                    orig_start - (cur_start - start)
                } else {
                    orig_start
                };
                let orig_end = if old_end > cur_end {
                    orig_end + (old_end - cur_end)
                } else {
                    orig_end
                };
                let span_end = cur_end.max(old_end);
                let cur_end = (span_end + new_end).saturating_sub(old_end);
                (orig_start, orig_end, cur_start.min(start), cur_end)
            }
        });
    }
    region.map(|(os, oe, cs, ce)| (os..oe, cs..ce))
}

/// Common-prefix / common-suffix diff of two texts (fallback when the edit log was evicted).
fn diff_bounds(old: &str, new: &str) -> Option<(Range<usize>, Range<usize>)> {
    if old == new {
        return None;
    }
    let mut prefix = old
        .bytes()
        .zip(new.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    while !old.is_char_boundary(prefix) || !new.is_char_boundary(prefix) {
        prefix -= 1;
    }
    let max_suffix = old.len().min(new.len()) - prefix;
    let mut suffix = old
        .bytes()
        .rev()
        .zip(new.bytes().rev())
        .take(max_suffix)
        .take_while(|(a, b)| a == b)
        .count();
    while !old.is_char_boundary(old.len() - suffix) || !new.is_char_boundary(new.len() - suffix) {
        suffix -= 1;
    }
    Some((prefix..old.len() - suffix, prefix..new.len() - suffix))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use kova_core::keymap::{KeyCode, Modifiers};
    use kova_core::perf::budgets::INPUT_LATENCY_MAX;

    use super::*;
    use crate::editor::shared::SharedBuffer;

    fn key(c: char) -> KeyStroke {
        if c.is_ascii_uppercase() {
            KeyStroke::new(Modifiers::SHIFT, KeyCode::Char(c.to_ascii_lowercase()))
        } else {
            KeyStroke::unmodified(KeyCode::Char(c))
        }
    }

    fn esc() -> KeyStroke {
        KeyStroke::unmodified(KeyCode::Escape)
    }

    fn feed(bridge: &mut VimBridge, state: &mut EditorState, keys: &str) {
        for c in keys.chars() {
            let _ = bridge.handle_key(state, key(c));
        }
    }

    #[test]
    fn cursor_kinds_follow_vim_modes() {
        let mut state = EditorState::from_text("hello world");
        let mut bridge = VimBridge::new(&mut state);
        assert_eq!(
            CursorRenderKind::from_vim_status(None),
            CursorRenderKind::Bar
        );
        assert_eq!(bridge.cursor_render_kind(), CursorRenderKind::Block);
        feed(&mut bridge, &mut state, "d");
        assert_eq!(
            bridge.cursor_render_kind(),
            CursorRenderKind::HalfBlockBottom
        );
        let _ = bridge.handle_key(&mut state, esc());
        feed(&mut bridge, &mut state, "r");
        assert_eq!(bridge.cursor_render_kind(), CursorRenderKind::Underline);
        let _ = bridge.handle_key(&mut state, esc());
        feed(&mut bridge, &mut state, "v");
        assert_eq!(bridge.cursor_render_kind(), CursorRenderKind::Block);
        let _ = bridge.handle_key(&mut state, esc());
        feed(&mut bridge, &mut state, "i");
        assert_eq!(bridge.cursor_render_kind(), CursorRenderKind::Bar);
        assert!(!bridge.owns_keys());
        assert!(!awaits_replace_char("\"r"));
    }

    #[test]
    fn caret_geometry_matches_spec() {
        let block = CursorRenderKind::Block.rect(10.0, 20.0, 8.0, 19.0);
        assert_eq!((block.width, block.height, block.radius), (8.0, 19.0, 1.5));
        assert!(CursorRenderKind::Block.redraws_glyph());
        let half = CursorRenderKind::HalfBlockBottom.rect(10.0, 20.0, 8.0, 19.0);
        assert_eq!(half.height, (19.0f32 * 0.45).round());
        assert!((half.y + half.height - 39.0).abs() < f32::EPSILON);
        let under = CursorRenderKind::Underline.rect(10.0, 20.0, 8.0, 19.0);
        assert_eq!((under.height, under.y), (2.5, 36.5));
        let bar = CursorRenderKind::Bar.rect(10.0, 20.0, 8.0, 19.0);
        assert_eq!((bar.width, bar.height), (2.0, 19.0));
        assert!(!CursorRenderKind::Bar.redraws_glyph());
    }

    #[test]
    fn block_cursor_glyph_contrast_meets_nfr3() {
        use kova_theme::{AetherTheme, BuiltinTheme, RgbaColor};
        let rgb = |c: &RgbaColor| (u32::from(c.r) << 16) | (u32::from(c.g) << 8) | u32::from(c.b);
        // Painted colours: `text.inverse` glyph on the `editor.cursor` block, both themes.
        for preset in [BuiltinTheme::KovaIslandsDark, BuiltinTheme::KovaLight] {
            let theme = AetherTheme::builtin(preset);
            let tokens = &theme.tokens;
            let ratio = contrast_ratio(rgb(&tokens.text.inverse), rgb(&tokens.editor.cursor));
            assert!(ratio >= 6.5, "{preset:?}: contrast {ratio:.2} < 6.5");
        }
        // Spec reference pairs: #0B1018 on #6CB6FF (Dark), #FFFFFF on #1D4ED8 (Light).
        assert!(contrast_ratio(0x000B_1018, 0x006C_B6FF) >= 6.5);
        assert!(contrast_ratio(0x00FF_FFFF, 0x001D_4ED8) >= 6.5);
        assert!((contrast_ratio(0x00FF_FFFF, 0x0000_0000) - 21.0).abs() < 0.01);
    }

    #[test]
    fn engine_edits_land_in_the_shared_document_history() {
        let mut state = EditorState::from_text("alpha beta gamma\nsecond line\n");
        let mut bridge = VimBridge::new(&mut state);
        feed(&mut bridge, &mut state, "dw");
        assert_eq!(state.snapshot().text(), "beta gamma\nsecond line\n");
        assert!(state.can_undo());
        feed(&mut bridge, &mut state, "jdd");
        assert_eq!(state.snapshot().text(), "beta gamma\n");
        // `u` walks the document history (same timeline as Cmd+Z).
        feed(&mut bridge, &mut state, "u");
        assert_eq!(state.snapshot().text(), "beta gamma\nsecond line\n");
        feed(&mut bridge, &mut state, "u");
        assert_eq!(state.snapshot().text(), "alpha beta gamma\nsecond line\n");
        let ctrl_r = KeyStroke::new(Modifiers::CTRL, KeyCode::Char('r'));
        assert_eq!(
            bridge.handle_key(&mut state, ctrl_r),
            VimKeyResult::Handled { edited: true }
        );
        assert_eq!(state.snapshot().text(), "beta gamma\nsecond line\n");
    }

    #[test]
    fn sibling_split_edits_and_insert_mode_typing_are_adopted() {
        let doc = SharedBuffer::new(kova_buffer::Buffer::from_text("abc def"));
        let mut left = EditorState::with_shared_buffer(doc.clone());
        let mut right = EditorState::with_shared_buffer(doc);
        let mut bridge = VimBridge::new(&mut left);

        // Insert mode: text goes through EditorState (IME path), Escape returns to Normal.
        feed(&mut bridge, &mut left, "A");
        assert_eq!(bridge.mode(), VimMode::Insert);
        assert!(left.insert_text(" żółć"));
        let _ = bridge.handle_key(&mut left, esc());
        assert_eq!(bridge.mode(), VimMode::Normal);
        assert_eq!(left.snapshot().text(), "abc def żółć");

        // A sibling split edits the shared document; the engine adopts it on the next key.
        right.set_caret(0);
        assert!(right.insert_text(">> "));
        feed(&mut bridge, &mut left, "0x");
        assert_eq!(left.snapshot().text(), "> abc def żółć");
        assert_eq!(right.snapshot().text(), "> abc def żółć");
    }

    #[test]
    fn command_line_keys_and_pass_through() {
        let mut state = EditorState::from_text("x");
        let mut bridge = VimBridge::new(&mut state);
        assert_eq!(
            bridge.handle_key(&mut state, key(':')),
            VimKeyResult::OpenCommandLine(':')
        );
        assert_eq!(
            bridge.handle_key(&mut state, key('/')),
            VimKeyResult::OpenCommandLine('/')
        );
        let ctrl_w = KeyStroke::new(Modifiers::CTRL, KeyCode::Char('w'));
        assert_eq!(
            bridge.handle_key(&mut state, ctrl_w),
            VimKeyResult::PassThrough
        );
        let cmd_s = KeyStroke::new(Modifiers::META, KeyCode::Char('s'));
        assert_eq!(
            bridge.handle_key(&mut state, cmd_s),
            VimKeyResult::PassThrough
        );
        feed(&mut bridge, &mut state, "i");
        bridge.force_normal(&mut state);
        assert_eq!(bridge.mode(), VimMode::Normal);
    }

    #[test]
    fn ime_text_completes_pending_commands_but_never_types_in_normal_mode() {
        let mut state = EditorState::from_text("zażółć");
        let mut bridge = VimBridge::new(&mut state);
        assert_eq!(
            bridge.handle_text_input(&mut state, "ą"),
            VimKeyResult::Handled { edited: false }
        );
        assert_eq!(state.snapshot().text(), "zażółć");
        feed(&mut bridge, &mut state, "r");
        assert_eq!(
            bridge.handle_text_input(&mut state, "ę"),
            VimKeyResult::Handled { edited: true }
        );
        assert_eq!(state.snapshot().text(), "ęażółć");
    }

    #[test]
    fn multi_edit_commands_and_ide_undo_never_desync_the_engine() {
        let mut state = EditorState::from_text("one\ntwo\nthree\nfour\n");
        let mut bridge = VimBridge::new(&mut state);
        // `J` / `>>` may apply several engine edits for one key.
        feed(&mut bridge, &mut state, "J");
        assert_eq!(state.snapshot().text(), "one two\nthree\nfour\n");
        feed(&mut bridge, &mut state, "j2>>");
        assert_eq!(state.snapshot().text(), "one two\n    three\n    four\n");
        // IDE undo (Cmd+Z path) behind the engine's back, then a Vim edit.
        assert!(state.undo());
        assert_eq!(state.snapshot().text(), "one two\nthree\nfour\n");
        // Undo restored the caret onto `thr|ee` (where it was before `2>>`).
        feed(&mut bridge, &mut state, "0x");
        assert_eq!(state.snapshot().text(), "one two\nhree\nfour\n");
        assert!(state.undo());
        assert!(state.undo());
        feed(&mut bridge, &mut state, "ggx");
        assert_eq!(state.snapshot().text(), "ne\ntwo\nthree\nfour\n");
    }

    #[test]
    fn visual_selection_is_mirrored() {
        let mut state = EditorState::from_text("hello world");
        let mut bridge = VimBridge::new(&mut state);
        feed(&mut bridge, &mut state, "vll");
        assert_eq!(state.selections().primary().range(), 0..3);
        feed(&mut bridge, &mut state, "d");
        assert_eq!(state.snapshot().text(), "lo world");
        assert!(state.selections().primary().is_empty());
    }

    #[test]
    fn bounding_edit_folds_sequential_edits() {
        // "hello world" → delete "hello " (0..6) → insert "X" at 5 → "worldX".
        let folded = bounding_edit([(0, 6, 0), (5, 5, 6)]);
        assert_eq!(folded, Some((0..11, 0..6)));
        // Edit before the region extends it on the left.
        assert_eq!(bounding_edit([(5, 6, 5), (1, 2, 4)]), Some((1..6, 1..7)));
        assert_eq!(bounding_edit(std::iter::empty()), None);
        assert_eq!(diff_bounds("abc", "abc"), None);
        assert_eq!(diff_bounds("aźc", "aóc"), Some((1..3, 1..3)));
    }

    #[test]
    fn vim_dispatch_on_100k_lines_stays_within_latency_budget() {
        let mut text = String::with_capacity(100_000 * 24);
        for i in 0..100_000 {
            text.push_str("let value_");
            text.push_str(&i.to_string());
            text.push_str(" = 42;\n");
        }
        let mut state = EditorState::from_text(&text);
        drop(text);
        let mut bridge = VimBridge::new(&mut state);
        feed(&mut bridge, &mut state, "50000j");
        let mut worst = Duration::ZERO;
        for keys in ["w", "dw", "x", "j", "u", "A", "b"] {
            let started = Instant::now();
            feed(&mut bridge, &mut state, keys);
            if bridge.mode() == VimMode::Insert {
                let _ = bridge.handle_key(&mut state, esc());
            }
            worst = worst.max(started.elapsed());
        }
        assert!(
            worst < INPUT_LATENCY_MAX,
            "worst Vim dispatch {worst:?} exceeded {INPUT_LATENCY_MAX:?}"
        );
    }

    #[test]
    fn gd_returns_go_to_definition_and_read_only_blocks_vim_edits() {
        let mut state = EditorState::from_text("OrderService service;\n");
        state.set_read_only(true);
        let mut bridge = VimBridge::new(&mut state);

        // `gd` returns GoToDefinition without editing.
        assert_eq!(
            bridge.handle_key(&mut state, key('g')),
            VimKeyResult::Handled { edited: false }
        );
        assert_eq!(
            bridge.handle_key(&mut state, key('d')),
            VimKeyResult::GoToDefinition
        );
        assert_eq!(state.snapshot().text(), "OrderService service;\n");

        // Mutating commands and Insert mode are blocked on read-only state.
        feed(&mut bridge, &mut state, "dd");
        assert_eq!(state.snapshot().text(), "OrderService service;\n");
        feed(&mut bridge, &mut state, "i");
        assert_eq!(bridge.mode(), VimMode::Normal);
    }
}
