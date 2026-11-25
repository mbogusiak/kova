//! The Vim state machine: key dispatch, operators, registers, undo and insert mode.

#![allow(
    clippy::many_single_char_names,
    clippy::too_many_lines,
    reason = "text-geometry code uses conventional short names (s/e/c/i) and one large motion table"
)]

use std::collections::HashMap;
use std::ops::Range;

use kova_buffer::{Buffer, BufferError, BufferSnapshot, Point};

use super::motion::{
    Motion, MotionKind, bracket_object, change_word_target, char_at, clamp_normal, col_chars,
    ends_with_crlf_before, find_char, first_nonblank, last_row, line_end, line_end_incl,
    line_start, line_str, match_bracket, next_char, on_word, operator_range, paragraph, pos_at_col,
    prev_char, quote_object, row_of, starts_with_crlf_at, word_backward, word_end_backward,
    word_end_forward, word_forward, word_object,
};
use super::{KeyConflictPolicy, PolicyTable, VimCursorShape, VimKey, VimMode, VimOutcome};
use crate::context::FocusContext;
use crate::keymap::KeyStroke;

const MAX_COUNT: usize = 99_999;
const MAX_INSERT_REPEAT: usize = 10_000;
const SHIFT_WIDTH: usize = 4;
const HALF_PAGE: usize = 10;
const FULL_PAGE: usize = 20;

/// Contents of a Vim register.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Register {
    /// Register text (line terminators normalized to `\n`).
    pub text: String,
    /// Whether the text was yanked/deleted linewise.
    pub linewise: bool,
}

/// Lightweight status for status bars (`KOVA-026`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VimStatus {
    /// Current mode.
    pub mode: VimMode,
    /// Cursor shape for the mode / pending state.
    pub cursor_shape: VimCursorShape,
    /// Keys of the incomplete command (`"d2"`, `"g"`, `"ci"`).
    pub pending: String,
    /// Matching focus context for keymap resolution.
    pub focus: FocusContext,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Delete,
    Change,
    Yank,
    Indent,
    Outdent,
    Lower,
    Upper,
    Toggle,
}

impl Op {
    const fn doubled(self) -> char {
        match self {
            Self::Delete => 'd',
            Self::Change => 'c',
            Self::Yank => 'y',
            Self::Indent => '>',
            Self::Outdent => '<',
            Self::Lower => 'u',
            Self::Upper => 'U',
            Self::Toggle => '~',
        }
    }

    const fn changes_text(self) -> bool {
        !matches!(self, Self::Yank)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Await {
    Find(char),
    Replace,
    Object(bool),
    Register,
}

#[derive(Debug, Default, Clone)]
struct Pending {
    count: Option<usize>,
    op_count: Option<usize>,
    op: Option<Op>,
    g: bool,
    awaiting: Option<Await>,
    register: Option<char>,
}

impl Pending {
    fn is_idle(&self) -> bool {
        self.count.is_none()
            && self.op_count.is_none()
            && self.op.is_none()
            && !self.g
            && self.awaiting.is_none()
            && self.register.is_none()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InsertKind {
    Plain,
    Open,
}

#[derive(Debug, Clone)]
struct InsertState {
    count: usize,
    kind: InsertKind,
    typed: String,
}

#[derive(Debug, Clone)]
struct UndoEdit {
    forward_range: Range<usize>,
    forward_text: String,
    inverse_range: Range<usize>,
    inverse_text: String,
}

#[derive(Debug, Clone)]
struct UndoEntry {
    edits: Vec<UndoEdit>,
    cursor_before: usize,
    cursor_after: usize,
}

/// Headless modal Vim engine bound to a [`Buffer`].
#[derive(Debug)]
pub struct VimEngine {
    buffer: Buffer,
    mode: VimMode,
    cursor: usize,
    anchor: usize,
    want_col: Option<usize>,
    pending: Pending,
    cmd_keys: Vec<VimKey>,
    policy: PolicyTable,
    registers: HashMap<char, Register>,
    last_find: Option<(char, char)>,
    undo: Vec<UndoEntry>,
    redo: Vec<UndoEntry>,
    tx_cursor_before: Option<usize>,
    tx_edits: Vec<UndoEdit>,
    dirty: bool,
    change_flag: bool,
    last_change: Vec<VimKey>,
    replaying: bool,
    insert: Option<InsertState>,
    last_visual: Option<(usize, usize, VimMode)>,
    last_error: Option<BufferError>,
}

impl VimEngine {
    /// Creates an engine editing `buffer`, starting in Normal mode at offset 0.
    #[must_use]
    pub fn new(buffer: Buffer) -> Self {
        Self {
            buffer,
            mode: VimMode::Normal,
            cursor: 0,
            anchor: 0,
            want_col: None,
            pending: Pending::default(),
            cmd_keys: Vec::new(),
            policy: PolicyTable::default(),
            registers: HashMap::new(),
            last_find: None,
            undo: Vec::new(),
            redo: Vec::new(),
            tx_cursor_before: None,
            tx_edits: Vec::new(),
            dirty: false,
            change_flag: false,
            last_change: Vec::new(),
            replaying: false,
            insert: None,
            last_visual: None,
            last_error: None,
        }
    }

    /// Creates an engine over a new buffer containing `text`.
    #[must_use]
    pub fn from_text(text: &str) -> Self {
        Self::new(Buffer::from_text(text))
    }

    /// Borrows the edited buffer.
    #[must_use]
    pub fn buffer(&self) -> &Buffer {
        &self.buffer
    }

    /// Consumes the engine and returns its buffer.
    #[must_use]
    pub fn into_buffer(self) -> Buffer {
        self.buffer
    }

    /// Full buffer text.
    #[must_use]
    pub fn text(&self) -> String {
        self.buffer.snapshot().text()
    }

    /// Current mode.
    #[must_use]
    pub fn mode(&self) -> VimMode {
        self.mode
    }

    /// Cursor byte offset (always on a `char` boundary).
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Cursor as a row/byte-column [`Point`].
    #[must_use]
    pub fn cursor_point(&self) -> Option<Point> {
        self.buffer.snapshot().byte_to_point(self.cursor).ok()
    }

    /// Moves the cursor (e.g. after a mouse click), clamping it for the mode.
    pub fn set_cursor(&mut self, offset: usize) {
        let snap = self.buffer.snapshot();
        let o = snap.clamp_byte_to_char_boundary(offset);
        self.cursor = if self.mode == VimMode::Insert {
            o
        } else {
            clamp_normal(&snap, o)
        };
        self.want_col = None;
    }

    /// Active visual selection as a byte range (end exclusive).
    #[must_use]
    pub fn selection(&self) -> Option<Range<usize>> {
        self.mode.is_visual().then(|| self.visual_range().0)
    }

    /// Content of register `name` (`'"'` is the unnamed register).
    #[must_use]
    pub fn register(&self, name: char) -> Option<&Register> {
        self.registers.get(&name)
    }

    /// Last buffer error encountered while applying an edit, if any.
    #[must_use]
    pub fn last_error(&self) -> Option<&BufferError> {
        self.last_error.as_ref()
    }

    /// Cursor shape for the current state.
    #[must_use]
    pub fn cursor_shape(&self) -> VimCursorShape {
        match self.mode {
            VimMode::Insert => VimCursorShape::Bar,
            VimMode::Normal if self.pending.op.is_some() => VimCursorShape::Underline,
            _ => VimCursorShape::Block,
        }
    }

    /// Status snapshot for status bars.
    #[must_use]
    pub fn status_snapshot(&self) -> VimStatus {
        let pending = if self.mode == VimMode::Insert || self.pending.is_idle() {
            String::new()
        } else {
            self.cmd_keys
                .iter()
                .map(|k| match k {
                    VimKey::Char(c) => *c,
                    VimKey::Ctrl(_) => '^',
                    _ => '?',
                })
                .collect()
        };
        VimStatus {
            mode: self.mode,
            cursor_shape: self.cursor_shape(),
            pending,
            focus: self.mode.focus_context(),
        }
    }

    /// Sets the default policy for `Ctrl` chords.
    pub fn set_default_policy(&mut self, policy: KeyConflictPolicy) {
        self.policy.set_default(policy);
    }

    /// Overrides the policy for `Ctrl+<letter>`.
    pub fn set_ctrl_policy(&mut self, letter: char, policy: KeyConflictPolicy) {
        self.policy.set(letter.to_ascii_lowercase(), policy);
    }

    /// Feeds a [`KeyStroke`]; unowned chords yield [`VimOutcome::PassThrough`].
    pub fn handle_key(&mut self, stroke: KeyStroke) -> VimOutcome {
        match VimKey::from_stroke(stroke) {
            Some(key) => self.handle_vim_key(key),
            None => VimOutcome::PassThrough,
        }
    }

    /// Feeds a text script (see [`VimKey::from_char`]) and returns how many keys passed through.
    pub fn feed(&mut self, keys: &str) -> usize {
        keys.chars()
            .filter(|c| self.handle_vim_key(VimKey::from_char(*c)) == VimOutcome::PassThrough)
            .count()
    }

    /// Feeds one normalized key.
    pub fn handle_vim_key(&mut self, key: VimKey) -> VimOutcome {
        if let VimKey::Ctrl(c) = key {
            let supported = if self.mode == VimMode::Insert {
                matches!(c, 'c' | 'h' | 'w')
            } else {
                matches!(c, 'c' | 'r' | 'd' | 'u' | 'f' | 'b')
            };
            if !supported || self.policy.get(c) == KeyConflictPolicy::Ide {
                return VimOutcome::PassThrough;
            }
        }
        if self.mode == VimMode::Insert {
            self.insert_key(key);
        } else {
            self.cmd_key(key);
        }
        VimOutcome::Handled
    }

    /// Feeds committed text from an IME or keyboard layout (including multi-byte
    /// UTF-8 sequences such as Polish diacritics or emoji).
    ///
    /// In [`VimMode::Insert`], inserts `text` at the cursor and records it for
    /// dot-repeat. In [`VimMode::Normal`] or Visual modes, if the engine is
    /// awaiting a character (e.g. `f{char}`, `r{char}`, `"{reg}`), consumes the
    /// first character of `text`.
    pub fn handle_text_input(&mut self, text: &str) -> VimOutcome {
        if text.is_empty() {
            return VimOutcome::Handled;
        }
        if self.mode == VimMode::Insert {
            self.insert_str(text);
            for c in text.chars() {
                self.cmd_keys.push(VimKey::Char(c));
            }
            return VimOutcome::Handled;
        }
        if self.pending.awaiting.is_some()
            && let Some(c) = text.chars().next()
        {
            return self.handle_vim_key(VimKey::Char(c));
        }
        VimOutcome::PassThrough
    }

    // ---------------------------------------------------------------- plumbing

    fn snap(&self) -> BufferSnapshot {
        self.buffer.snapshot()
    }

    fn slice_string(&self, range: Range<usize>) -> String {
        self.buffer
            .snapshot()
            .slice_bytes(range)
            .map(|s| s.to_string())
            .unwrap_or_default()
    }

    fn replace(&mut self, range: Range<usize>, with: &str) -> bool {
        let old_text = self.slice_string(range.clone());
        match self.buffer.apply_edit(range.clone(), with) {
            Ok(_) => {
                let inverse_range = range.start..(range.start + with.len());
                self.tx_edits.push(UndoEdit {
                    forward_range: range,
                    forward_text: with.to_string(),
                    inverse_range,
                    inverse_text: old_text,
                });
                self.dirty = true;
                true
            }
            Err(e) => {
                self.last_error = Some(e);
                false
            }
        }
    }

    fn eol(&self) -> &'static str {
        let snap = self.snap();
        let e = line_end(&snap, 0);
        let ei = line_end_incl(&snap, 0);
        if ei >= e + 2 { "\r\n" } else { "\n" }
    }

    fn clamp(&self, pos: usize) -> usize {
        clamp_normal(&self.snap(), pos)
    }

    fn counts(&self) -> (usize, bool) {
        match (self.pending.count, self.pending.op_count) {
            (None, None) => (1, false),
            (a, b) => (
                a.unwrap_or(1).saturating_mul(b.unwrap_or(1)).min(MAX_COUNT),
                true,
            ),
        }
    }

    fn commit(&mut self) {
        if self.dirty {
            if let Some(cursor_before) = self.tx_cursor_before.take() {
                let edits = std::mem::take(&mut self.tx_edits);
                if !edits.is_empty() {
                    self.undo.push(UndoEntry {
                        edits,
                        cursor_before,
                        cursor_after: self.cursor,
                    });
                    self.redo.clear();
                }
            }
            if self.change_flag && !self.replaying {
                self.last_change = self.cmd_keys.clone();
            }
        }
        self.tx_cursor_before = None;
        self.tx_edits.clear();
        self.dirty = false;
        self.change_flag = false;
        self.cmd_keys.clear();
        self.pending = Pending::default();
    }

    fn cancel(&mut self) {
        self.pending = Pending::default();
        self.cmd_keys.clear();
        self.tx_cursor_before = None;
        self.tx_edits.clear();
        self.change_flag = false;
    }

    fn exit_visual(&mut self) {
        if self.mode.is_visual() {
            self.last_visual = Some((self.anchor, self.cursor, self.mode));
            self.mode = VimMode::Normal;
            self.cursor = self.clamp(self.cursor);
        }
    }

    // ------------------------------------------------------------ normal/visual

    fn cmd_key(&mut self, key: VimKey) {
        if matches!(key, VimKey::Escape | VimKey::Ctrl('c')) {
            self.exit_visual();
            self.cancel();
            return;
        }
        if key == VimKey::Char('.')
            && self.mode == VimMode::Normal
            && self.pending.op.is_none()
            && !self.pending.g
            && self.pending.awaiting.is_none()
        {
            self.cancel();
            self.repeat_last_change();
            return;
        }
        if self.cmd_keys.is_empty() {
            self.tx_cursor_before = Some(self.cursor);
            self.tx_edits.clear();
        }
        self.cmd_keys.push(key);
        if self.process(key) && self.mode != VimMode::Insert {
            self.commit();
        } else if self.mode == VimMode::Insert {
            self.pending = Pending::default();
        }
    }

    fn repeat_last_change(&mut self) {
        let keys = self.last_change.clone();
        self.replaying = true;
        for k in keys {
            self.handle_vim_key(k);
        }
        self.replaying = false;
    }

    fn process(&mut self, key: VimKey) -> bool {
        let raw = match key {
            VimKey::Char(c) => Some(c),
            VimKey::Tab => Some('\t'),
            VimKey::Enter => Some('\n'),
            _ => None,
        };
        if let Some(aw) = self.pending.awaiting.take() {
            return raw.is_none_or(|c| self.handle_await(aw, c));
        }
        let ch = match key {
            VimKey::Char(c) => c,
            VimKey::Ctrl(c) => return self.ctrl_cmd(c),
            VimKey::Left | VimKey::Backspace => 'h',
            VimKey::Right => 'l',
            VimKey::Up => 'k',
            VimKey::Down | VimKey::Enter => 'j',
            VimKey::Home => '0',
            VimKey::End => '$',
            VimKey::Delete => 'x',
            _ => return true,
        };
        if ch.is_ascii_digit() && (ch != '0' || self.count_active()) {
            let d = ch
                .to_digit(10)
                .and_then(|d| usize::try_from(d).ok())
                .unwrap_or(0);
            let slot = if self.pending.op.is_some() {
                &mut self.pending.op_count
            } else {
                &mut self.pending.count
            };
            *slot = Some(
                slot.unwrap_or(0)
                    .saturating_mul(10)
                    .saturating_add(d)
                    .min(MAX_COUNT),
            );
            return false;
        }
        if let Some(op) = self.pending.op {
            if op.doubled() == ch && !self.pending.g {
                return self.linewise_op(op);
            }
        }
        if self.pending.g {
            self.pending.g = false;
            return self.handle_g(ch);
        }
        self.dispatch(ch)
    }

    fn count_active(&self) -> bool {
        if self.pending.op.is_some() {
            self.pending.op_count.is_some()
        } else {
            self.pending.count.is_some()
        }
    }

    fn dispatch(&mut self, ch: char) -> bool {
        if self.mode.is_visual() {
            if let Some(done) = self.visual_cmd(ch) {
                return done;
            }
        }
        match ch {
            '"' => self.await_(Await::Register),
            'g' => {
                self.pending.g = true;
                false
            }
            'd' => self.op_key(Op::Delete),
            'c' => self.op_key(Op::Change),
            'y' => self.op_key(Op::Yank),
            '>' => self.op_key(Op::Indent),
            '<' => self.op_key(Op::Outdent),
            'i' | 'a' if self.pending.op.is_some() => self.await_(Await::Object(ch == 'i')),
            'f' | 't' | 'F' | 'T' => self.await_(Await::Find(ch)),
            ';' | ',' => self.repeat_find(ch == ','),
            _ => {
                if let Some(name) = motion_name(ch) {
                    return self.do_motion(name);
                }
                if self.pending.op.is_some() {
                    return true;
                }
                self.normal_cmd(ch)
            }
        }
    }

    fn await_(&mut self, a: Await) -> bool {
        self.pending.awaiting = Some(a);
        false
    }

    fn ctrl_cmd(&mut self, c: char) -> bool {
        match c {
            'r' if self.mode == VimMode::Normal => {
                let (n, _) = self.counts();
                for _ in 0..n {
                    self.redo_once();
                }
            }
            'd' | 'u' | 'f' | 'b' => {
                let (n, has) = self.counts();
                let page = if matches!(c, 'd' | 'u') {
                    HALF_PAGE
                } else {
                    FULL_PAGE
                };
                let rows = if has { n } else { page };
                let name = if matches!(c, 'd' | 'f') { "j" } else { "k" };
                self.pending.count = Some(rows);
                self.pending.op_count = None;
                self.do_motion(name);
            }
            _ => {}
        }
        true
    }

    fn handle_g(&mut self, ch: char) -> bool {
        match ch {
            'g' => self.do_motion("gg"),
            'e' => self.do_motion("ge"),
            'E' => self.do_motion("gE"),
            'u' | 'U' | '~' => {
                let op = match ch {
                    'u' => Op::Lower,
                    'U' => Op::Upper,
                    _ => Op::Toggle,
                };
                if self.pending.op == Some(op) {
                    self.linewise_op(op)
                } else {
                    self.op_key(op)
                }
            }
            'v' if self.mode == VimMode::Normal && self.pending.op.is_none() => {
                if let Some((a, c, m)) = self.last_visual {
                    let len = self.snap().len_bytes();
                    self.anchor = a.min(len);
                    self.cursor = self.clamp(c);
                    self.mode = m;
                }
                true
            }
            'J' if self.mode == VimMode::Normal && self.pending.op.is_none() => {
                self.join(false);
                true
            }
            _ => true,
        }
    }

    fn op_key(&mut self, op: Op) -> bool {
        if self.mode.is_visual() {
            return self.visual_op(op);
        }
        if self.pending.op.is_some() {
            return true;
        }
        self.pending.op = Some(op);
        false
    }

    fn linewise_op(&mut self, op: Op) -> bool {
        let (n, _) = self.counts();
        let snap = self.snap();
        let ra = row_of(&snap, self.cursor);
        let rb = (ra + n - 1).min(last_row(&snap));
        let range = line_start(&snap, ra)..line_end_incl(&snap, rb);
        drop(snap);
        self.mark_change(op);
        self.apply_op(op, range, true, 1);
        true
    }

    fn mark_change(&mut self, op: Op) {
        if op.changes_text() && !self.mode.is_visual() {
            self.change_flag = true;
        }
    }

    // ---------------------------------------------------------------- motions

    fn resolve_motion(&self, name: &str, n: usize, has_count: bool) -> Option<Motion> {
        let snap = self.snap();
        let c = self.cursor;
        let row = row_of(&snap, c);
        let big = name.chars().last().is_some_and(|x| x.is_ascii_uppercase()) && name.len() == 1;
        let excl = |target: usize| Some(Motion::new(target, MotionKind::Exclusive));
        let incl = |target: usize| Some(Motion::new(target, MotionKind::Inclusive));
        match name {
            "h" => {
                let s = line_start(&snap, row);
                let mut i = c;
                for _ in 0..n {
                    match prev_char(&snap, i) {
                        Some(p) if p >= s => i = p,
                        _ => break,
                    }
                }
                (i != c).then_some(())?;
                excl(i)
            }
            "l" => {
                let e = line_end(&snap, row);
                let mut i = c;
                for _ in 0..n {
                    match next_char(&snap, i) {
                        Some(nx) if i < e => i = nx.min(e),
                        _ => break,
                    }
                }
                (i != c).then_some(())?;
                excl(i)
            }
            "j" | "k" => {
                let new_row = if name == "j" {
                    (row + n).min(last_row(&snap))
                } else {
                    row.saturating_sub(n)
                };
                (new_row != row).then_some(())?;
                let col = self.want_col.unwrap_or_else(|| col_chars(&snap, c));
                let mut m = Motion::new(pos_at_col(&snap, new_row, col), MotionKind::Linewise);
                m.vertical = true;
                Some(m)
            }
            "0" => excl(line_start(&snap, row)),
            "^" => excl(first_nonblank(&snap, row)),
            "$" => {
                let r2 = (row + n - 1).min(last_row(&snap));
                let e = line_end(&snap, r2);
                incl(if e > line_start(&snap, r2) {
                    prev_char(&snap, e).unwrap_or(e)
                } else {
                    e
                })
            }
            "%" => incl(match_bracket(&snap, c)?),
            "w" | "W" => {
                if self.pending.op == Some(Op::Change) && on_word(&snap, c) {
                    return incl(change_word_target(&snap, c, n, big));
                }
                let mut i = c;
                for _ in 0..n {
                    i = word_forward(&snap, i, big);
                }
                let mut m = Motion::new(i, MotionKind::Exclusive);
                m.word = true;
                Some(m)
            }
            "b" | "B" => {
                let mut i = c;
                for _ in 0..n {
                    i = word_backward(&snap, i, big);
                }
                (i != c).then_some(())?;
                excl(i)
            }
            "e" | "E" => {
                let mut i = c;
                for _ in 0..n {
                    i = word_end_forward(&snap, i, big);
                }
                incl(i)
            }
            "ge" | "gE" => {
                let mut i = c;
                for _ in 0..n {
                    i = word_end_backward(&snap, i, name == "gE");
                }
                (i != c).then_some(())?;
                incl(i)
            }
            "gg" | "G" => {
                let r = match (has_count, name) {
                    (true, _) => (n - 1).min(last_row(&snap)),
                    (false, "gg") => 0,
                    _ => last_row(&snap),
                };
                Some(Motion::new(first_nonblank(&snap, r), MotionKind::Linewise))
            }
            "}" | "{" => {
                let i = paragraph(&snap, c, name == "}", n);
                (i != c).then_some(())?;
                let mut m = Motion::new(i, MotionKind::Exclusive);
                m.paragraph = true;
                Some(m)
            }
            _ => None,
        }
    }

    fn do_motion(&mut self, name: &str) -> bool {
        let (n, has) = self.counts();
        if let Some(m) = self.resolve_motion(name, n, has) {
            self.apply_motion(m);
            if name == "$" {
                self.want_col = Some(usize::MAX);
            }
        }
        true
    }

    fn apply_motion(&mut self, m: Motion) {
        if let Some(op) = self.pending.op {
            let snap = self.snap();
            let r = operator_range(&snap, self.cursor, m);
            drop(snap);
            self.mark_change(op);
            self.apply_op(op, r.range, r.linewise, 1);
        } else {
            if m.vertical && self.want_col.is_none() {
                self.want_col = Some(col_chars(&self.snap(), self.cursor));
            }
            self.cursor = self.clamp(m.target);
            if !m.vertical {
                self.want_col = None;
            }
        }
    }

    fn handle_await(&mut self, aw: Await, c: char) -> bool {
        match aw {
            Await::Register => {
                if c.is_ascii_alphabetic()
                    || c.is_ascii_digit()
                    || matches!(c, '"' | '_' | '+' | '*')
                {
                    self.pending.register = Some(c);
                    return false;
                }
                true
            }
            Await::Find(kind) => {
                self.last_find = Some((kind, c));
                self.find(kind, c, false)
            }
            Await::Replace => {
                self.replace_chars(c);
                true
            }
            Await::Object(inner) => self.text_object(inner, c),
        }
    }

    fn repeat_find(&mut self, reverse: bool) -> bool {
        let Some((kind, c)) = self.last_find else {
            return true;
        };
        let kind = if reverse {
            match kind {
                'f' => 'F',
                'F' => 'f',
                't' => 'T',
                _ => 't',
            }
        } else {
            kind
        };
        self.find(kind, c, true)
    }

    fn find(&mut self, kind: char, ch: char, repeat: bool) -> bool {
        let (n, _) = self.counts();
        let snap = self.snap();
        let target = find_char(&snap, self.cursor, kind, ch, n, repeat);
        drop(snap);
        if let Some(t) = target {
            let mk = if matches!(kind, 'f' | 't') {
                MotionKind::Inclusive
            } else {
                MotionKind::Exclusive
            };
            self.apply_motion(Motion::new(t, mk));
        }
        true
    }

    fn text_object(&mut self, inner: bool, c: char) -> bool {
        let around = !inner;
        let (n, _) = self.counts();
        let snap = self.snap();
        let pos = self.cursor;
        let range = match c {
            'w' => word_object(&snap, pos, around, false),
            'W' => word_object(&snap, pos, around, true),
            '"' | '\'' | '`' => quote_object(&snap, pos, c, around),
            '(' | ')' | 'b' => bracket_object(&snap, pos, '(', ')', n, around),
            '{' | '}' | 'B' => bracket_object(&snap, pos, '{', '}', n, around),
            '[' | ']' => bracket_object(&snap, pos, '[', ']', n, around),
            '<' | '>' => bracket_object(&snap, pos, '<', '>', n, around),
            _ => None,
        };
        let Some(range) = range else {
            return true;
        };
        if self.mode.is_visual() {
            if let Some(last) = prev_char(&snap, range.end).filter(|l| *l >= range.start) {
                self.anchor = range.start;
                self.cursor = last;
                self.mode = VimMode::Visual;
            }
            return true;
        }
        drop(snap);
        if let Some(op) = self.pending.op {
            self.mark_change(op);
            self.apply_op(op, range, false, 1);
        }
        true
    }

    // -------------------------------------------------------------- operators

    fn store(&mut self, text: &str, linewise: bool, is_yank: bool) {
        let mut t = text.replace("\r\n", "\n");
        if linewise && !t.ends_with('\n') {
            t.push('\n');
        }
        let reg = Register { text: t, linewise };
        match self.pending.register {
            Some('_') => return,
            Some(c) if c.is_ascii_uppercase() => {
                let lower = c.to_ascii_lowercase();
                let combined = if let Some(existing) = self.registers.get_mut(&lower) {
                    if linewise && !existing.text.ends_with('\n') && !existing.text.is_empty() {
                        existing.text.push('\n');
                    }
                    existing.text.push_str(&reg.text);
                    existing.linewise |= linewise;
                    existing.clone()
                } else {
                    self.registers.insert(lower, reg.clone());
                    reg
                };
                self.registers.insert('"', combined);
                return;
            }
            Some(n) if n != '"' => {
                self.registers.insert(n, reg.clone());
                self.registers.insert('"', reg);
                return;
            }
            _ => {}
        }
        self.registers.insert('"', reg.clone());
        if is_yank {
            self.registers.insert('0', reg);
        } else {
            for d in ('1'..='8').rev() {
                let next = char::from(d as u8 + 1);
                if let Some(prev) = self.registers.remove(&d) {
                    self.registers.insert(next, prev);
                } else {
                    self.registers.remove(&next);
                }
            }
            self.registers.insert('1', reg);
        }
    }

    fn apply_op(&mut self, op: Op, range: Range<usize>, linewise: bool, times: usize) {
        match op {
            Op::Yank => self.op_yank(&range, linewise),
            Op::Delete => {
                if linewise {
                    self.delete_lines(range);
                } else if !range.is_empty() {
                    let s = self.slice_string(range.clone());
                    self.store(&s, false, false);
                    let start = range.start;
                    self.replace(range, "");
                    self.cursor = self.clamp(start);
                }
                self.want_col = None;
            }
            Op::Change => self.op_change(range, linewise),
            Op::Indent | Op::Outdent => self.shift_range(&range, op == Op::Indent, times),
            Op::Lower | Op::Upper | Op::Toggle => self.case_range(op, range),
        }
    }

    fn op_yank(&mut self, range: &Range<usize>, linewise: bool) {
        let s = self.slice_string(range.clone());
        self.store(&s, linewise, true);
        let snap = self.snap();
        let ra = row_of(&snap, range.start);
        let new_cursor = if linewise {
            if row_of(&snap, self.cursor) == ra {
                self.cursor
            } else {
                pos_at_col(&snap, ra, col_chars(&snap, self.cursor))
            }
        } else {
            range.start.min(self.cursor)
        };
        drop(snap);
        self.cursor = self.clamp(new_cursor);
    }

    fn op_change(&mut self, range: Range<usize>, linewise: bool) {
        let start = range.start;
        let kill = if linewise {
            let snap = self.snap();
            let rb = row_of(&snap, range.end.saturating_sub(1).max(range.start));
            range.start..line_end(&snap, rb)
        } else {
            range.clone()
        };
        if !range.is_empty() {
            let s = self.slice_string(range);
            self.store(&s, linewise, false);
        }
        if !kill.is_empty() {
            self.replace(kill, "");
        }
        self.cursor = start;
        self.enter_insert(1, InsertKind::Plain);
    }

    fn delete_lines(&mut self, range: Range<usize>) {
        let s = self.slice_string(range.clone());
        self.store(&s, true, false);
        let snap = self.snap();
        let len = snap.len_bytes();
        let mut del = range.clone();
        if range.end == len && range.start > 0 && !s.ends_with('\n') {
            let sep = if ends_with_crlf_before(&snap, range.start) {
                2
            } else {
                1
            };
            del = range.start - sep..len;
        }
        drop(snap);
        let start = del.start;
        self.replace(del, "");
        let snap = self.snap();
        let row = row_of(&snap, start).min(last_row(&snap));
        self.cursor = first_nonblank(&snap, row);
    }

    fn shift_range(&mut self, range: &Range<usize>, indent: bool, times: usize) {
        let snap = self.snap();
        let ra = row_of(&snap, range.start);
        let rb = row_of(&snap, range.end.saturating_sub(1).max(range.start));
        let rows: Vec<(usize, usize)> = (ra..=rb)
            .map(|r| (line_start(&snap, r), line_end(&snap, r)))
            .collect();
        drop(snap);
        let amount = SHIFT_WIDTH * times.max(1);
        for (s, e) in rows.into_iter().rev() {
            if indent {
                if e > s {
                    self.replace(s..s, &" ".repeat(amount));
                }
            } else {
                let mut removed = 0;
                let mut end = s;
                let line = self.slice_string(s..e);
                for b in line.chars() {
                    if b == ' ' && removed < amount {
                        removed += 1;
                        end += 1;
                    } else if b == '\t' && removed < amount {
                        removed += SHIFT_WIDTH;
                        end += 1;
                    } else {
                        break;
                    }
                }
                self.replace(s..end, "");
            }
        }
        let snap = self.snap();
        self.cursor = first_nonblank(&snap, ra.min(last_row(&snap)));
    }

    fn case_range(&mut self, op: Op, range: Range<usize>) {
        let start = range.start;
        let new: String = self
            .slice_string(range.clone())
            .chars()
            .flat_map(|c| convert_case(op, c))
            .collect();
        self.replace(range, &new);
        self.cursor = self.clamp(start);
    }

    // ----------------------------------------------------------- normal commands

    fn normal_cmd(&mut self, ch: char) -> bool {
        let (n, _) = self.counts();
        match ch {
            'i' | 'a' | 'I' | 'A' | 'o' | 'O' => self.start_insert(ch, n),
            'x' => self.quick_op(Op::Delete, "l"),
            'X' => self.quick_op(Op::Delete, "h"),
            's' => self.quick_op(Op::Change, "l"),
            'D' => self.quick_op(Op::Delete, "$"),
            'C' => self.quick_op(Op::Change, "$"),
            'S' => {
                self.linewise_op(Op::Change);
            }
            'Y' => {
                self.linewise_op(Op::Yank);
            }
            'p' | 'P' => {
                self.change_flag = true;
                self.paste(ch == 'p', n);
            }
            'u' => {
                for _ in 0..n {
                    self.undo_once();
                }
            }
            'J' => self.join(true),
            'r' => return self.await_(Await::Replace),
            '~' => self.tilde(n),
            'v' => self.enter_visual(VimMode::Visual),
            'V' => self.enter_visual(VimMode::VisualLine),
            _ => {}
        }
        true
    }

    fn quick_op(&mut self, op: Op, motion: &str) {
        self.pending.op = Some(op);
        self.change_flag = true;
        self.do_motion(motion);
        if op == Op::Change && self.mode != VimMode::Insert {
            self.enter_insert(1, InsertKind::Plain);
        }
    }

    fn enter_visual(&mut self, mode: VimMode) {
        self.anchor = self.cursor;
        self.mode = mode;
    }

    fn enter_insert(&mut self, count: usize, kind: InsertKind) {
        self.mode = VimMode::Insert;
        self.insert = Some(InsertState {
            count: count.min(MAX_INSERT_REPEAT),
            kind,
            typed: String::new(),
        });
    }

    fn start_insert(&mut self, ch: char, n: usize) {
        self.change_flag = true;
        let snap = self.snap();
        let row = row_of(&snap, self.cursor);
        let (s, e) = (line_start(&snap, row), line_end(&snap, row));
        let first = first_nonblank(&snap, row);
        let first_raw = if first < s { s } else { first };
        let next_pos = if ch == 'a' && self.cursor < e {
            next_char(&snap, self.cursor).unwrap_or(e).min(e)
        } else {
            self.cursor
        };
        drop(snap);
        let eol = self.eol();
        let mut kind = InsertKind::Plain;
        match ch {
            'a' => self.cursor = next_pos,
            'I' => self.cursor = first_raw,
            'A' => self.cursor = e,
            'o' => {
                self.replace(e..e, eol);
                self.cursor = e + eol.len();
                kind = InsertKind::Open;
            }
            'O' => {
                self.replace(s..s, eol);
                self.cursor = s;
                kind = InsertKind::Open;
            }
            _ => {}
        }
        self.enter_insert(n, kind);
    }

    fn join(&mut self, spaced: bool) {
        let (n, _) = self.counts();
        let times = n.max(2) - 1;
        for _ in 0..times {
            let snap = self.snap();
            let row = row_of(&snap, self.cursor);
            if row >= last_row(&snap) {
                break;
            }
            let e = line_end(&snap, row);
            let next_start = line_start(&snap, row + 1);
            let next_end = line_end(&snap, row + 1);
            let mut ws_end = next_start;
            if spaced {
                let next_line = line_str(&snap, row + 1);
                ws_end += next_line
                    .chars()
                    .take_while(|c| *c == ' ' || *c == '\t')
                    .map(char::len_utf8)
                    .sum::<usize>();
            }
            let cur_empty = e == line_start(&snap, row);
            let ends_ws = prev_char(&snap, e)
                .filter(|&p| p >= line_start(&snap, row))
                .and_then(|p| char_at(&snap, p))
                .is_some_and(char::is_whitespace);
            let next_empty = ws_end >= next_end;
            let starts_paren = char_at(&snap, ws_end) == Some(')');
            drop(snap);
            let sep = if spaced && !(cur_empty || ends_ws || next_empty || starts_paren) {
                " "
            } else {
                ""
            };
            self.replace(e..ws_end, sep);
            self.cursor = self.clamp(e);
        }
        self.change_flag = true;
    }

    fn replace_chars(&mut self, c: char) {
        let (n, _) = self.counts();
        if self.mode.is_visual() {
            let (range, _) = self.visual_range();
            let new: String = self
                .slice_string(range.clone())
                .chars()
                .map(|x| if x == '\n' || x == '\r' { x } else { c })
                .collect();
            let start = range.start;
            self.exit_visual();
            self.replace(range, &new);
            self.cursor = self.clamp(start);
            return;
        }
        self.change_flag = true;
        let snap = self.snap();
        let row = row_of(&snap, self.cursor);
        let e = line_end(&snap, row);
        let tail = snap
            .slice_bytes(self.cursor..e)
            .map(|s| s.to_string())
            .unwrap_or_default();
        drop(snap);
        let chars: Vec<usize> = tail.char_indices().map(|(i, _)| i).collect();
        if chars.len() < n || chars.is_empty() {
            return;
        }
        let end = chars.get(n).map_or(e, |off| self.cursor + off);
        let start = self.cursor;
        let (with, last) = if c == '\n' {
            (self.eol().to_string(), None)
        } else {
            let s = c.to_string().repeat(n);
            let last = start + s.len() - c.len_utf8();
            (s, Some(last))
        };
        self.replace(start..end, &with);
        self.cursor = last.unwrap_or(start + with.len());
        self.cursor = self.clamp(self.cursor);
    }

    fn tilde(&mut self, n: usize) {
        self.change_flag = true;
        let snap = self.snap();
        let row = row_of(&snap, self.cursor);
        let e = line_end(&snap, row);
        let mut end = self.cursor;
        for _ in 0..n {
            match next_char(&snap, end) {
                Some(nx) if end < e => end = nx.min(e),
                _ => break,
            }
        }
        drop(snap);
        if end == self.cursor {
            return;
        }
        let start = self.cursor;
        self.case_range(Op::Toggle, start..end);
        self.cursor = self.clamp(end);
    }

    fn paste(&mut self, after: bool, n: usize) {
        let name = self.pending.register.unwrap_or('"');
        let lookup = if name.is_ascii_uppercase() {
            name.to_ascii_lowercase()
        } else {
            name
        };
        let Some(reg) = self.registers.get(&lookup).cloned() else {
            return;
        };
        let eol = self.eol();
        let body = reg.text.replace('\n', eol);
        let snap = self.snap();
        let row = row_of(&snap, self.cursor);
        let (s, e, ei) = (
            line_start(&snap, row),
            line_end(&snap, row),
            line_end_incl(&snap, row),
        );
        let len_bytes = snap.len_bytes();
        let next_after = if after && self.cursor < e {
            next_char(&snap, self.cursor).unwrap_or(e)
        } else {
            self.cursor
        };
        drop(snap);
        if reg.linewise {
            let chunk = body.repeat(n);
            let (at, ins) = if after {
                if ei == len_bytes && e == ei {
                    (ei, format!("{eol}{}", chunk.trim_end_matches(eol)))
                } else {
                    (ei, chunk)
                }
            } else {
                (s, chunk)
            };
            let first_line_at = if after && ins.starts_with(eol) && e == ei {
                at + eol.len()
            } else {
                at
            };
            self.replace(at..at, &ins);
            let snap = self.snap();
            self.cursor = first_nonblank(&snap, row_of(&snap, first_line_at));
        } else {
            let at = next_after;
            let ins = body.repeat(n);
            self.replace(at..at, &ins);
            let snap = self.snap();
            self.cursor = if ins.contains('\n') {
                at
            } else {
                clamp_normal(&snap, prev_char(&snap, at + ins.len()).unwrap_or(at))
            };
        }
        self.want_col = None;
    }

    // -------------------------------------------------------------------- visual

    fn visual_range(&self) -> (Range<usize>, bool) {
        let (a, b) = if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        };
        let snap = self.snap();
        if self.mode == VimMode::VisualLine {
            return (
                line_start(&snap, row_of(&snap, a))..line_end_incl(&snap, row_of(&snap, b)),
                true,
            );
        }
        let end = if starts_with_crlf_at(&snap, b) {
            b + 2
        } else {
            next_char(&snap, b).unwrap_or(b)
        };
        (a..end, false)
    }

    fn visual_cmd(&mut self, ch: char) -> Option<bool> {
        let linewise_mode = self.mode == VimMode::VisualLine;
        Some(match ch {
            'd' | 'x' => self.visual_op(Op::Delete),
            'c' | 's' => self.visual_op(Op::Change),
            'y' => self.visual_op(Op::Yank),
            'D' | 'X' => self.visual_linewise(Op::Delete),
            'C' | 'S' | 'R' => self.visual_linewise(Op::Change),
            'Y' => self.visual_linewise(Op::Yank),
            'u' => self.visual_op(Op::Lower),
            'U' => self.visual_op(Op::Upper),
            '~' => self.visual_op(Op::Toggle),
            'r' => self.await_(Await::Replace),
            'J' => {
                self.visual_join();
                true
            }
            'p' | 'P' => {
                self.visual_paste();
                true
            }
            'o' | 'O' => {
                std::mem::swap(&mut self.anchor, &mut self.cursor);
                true
            }
            'v' => {
                if linewise_mode {
                    self.mode = VimMode::Visual;
                } else {
                    self.exit_visual();
                }
                true
            }
            'V' => {
                if linewise_mode {
                    self.exit_visual();
                } else {
                    self.mode = VimMode::VisualLine;
                }
                true
            }
            'i' | 'a' => self.await_(Await::Object(ch == 'i')),
            _ => return None,
        })
    }

    fn visual_op(&mut self, op: Op) -> bool {
        let (range, linewise) = self.visual_range();
        let times = self.counts().0;
        self.exit_visual();
        self.apply_op(op, range, linewise, times);
        true
    }

    fn visual_linewise(&mut self, op: Op) -> bool {
        self.mode = VimMode::VisualLine;
        self.visual_op(op)
    }

    fn visual_join(&mut self) {
        let (range, _) = self.visual_range();
        let snap = self.snap();
        let (ra, rb) = (
            row_of(&snap, range.start),
            row_of(&snap, range.end.saturating_sub(1).max(range.start)),
        );
        let start = line_start(&snap, ra);
        drop(snap);
        self.exit_visual();
        self.cursor = start;
        self.pending.count = Some(rb - ra + 1);
        self.pending.op_count = None;
        self.join(true);
    }

    fn visual_paste(&mut self) {
        let name = self.pending.register.unwrap_or('"');
        let lookup = if name.is_ascii_uppercase() {
            name.to_ascii_lowercase()
        } else {
            name
        };
        let Some(reg) = self.registers.get(&lookup).cloned() else {
            return;
        };
        let (range, line_sel) = self.visual_range();
        let eol = self.eol();
        let mut body = reg.text.replace('\n', eol);
        self.exit_visual();
        let replaced = self.slice_string(range.clone());
        if reg.linewise && !line_sel {
            body = format!("{eol}{body}");
        } else if !reg.linewise && line_sel {
            body.push_str(eol);
        }
        let start = range.start;
        self.replace(range, &body);
        self.pending.register = None;
        self.store(&replaced, line_sel, false);
        let end = start + body.len();
        let snap = self.snap();
        self.cursor = if reg.linewise || body.contains('\n') {
            clamp_normal(&snap, start)
        } else {
            clamp_normal(&snap, prev_char(&snap, end).unwrap_or(start))
        };
    }

    // -------------------------------------------------------------------- insert

    fn insert_str(&mut self, s: &str) {
        let at = self.cursor;
        if self.replace(at..at, s) {
            self.cursor = at + s.len();
            if let Some(ins) = self.insert.as_mut() {
                ins.typed.push_str(s);
            }
        }
    }

    fn insert_key(&mut self, key: VimKey) {
        self.cmd_keys.push(key);
        match key {
            VimKey::Escape | VimKey::Ctrl('c') => self.leave_insert(),
            VimKey::Char(c) => self.insert_str(c.encode_utf8(&mut [0; 4])),
            VimKey::Tab => self.insert_str("\t"),
            VimKey::Enter => {
                let eol = self.eol();
                self.insert_str(eol);
            }
            VimKey::Backspace | VimKey::Ctrl('h') => self.insert_backspace(),
            VimKey::Ctrl('w') => self.insert_delete_word(),
            VimKey::Delete => {
                let at = self.cursor;
                let snap = self.snap();
                let end = if starts_with_crlf_at(&snap, at) {
                    at + 2
                } else {
                    next_char(&snap, at).unwrap_or(at)
                };
                drop(snap);
                self.replace(at..end, "");
            }
            VimKey::Left
            | VimKey::Right
            | VimKey::Up
            | VimKey::Down
            | VimKey::Home
            | VimKey::End => {
                self.insert_move(key);
            }
            VimKey::Ctrl(_) => {}
        }
    }

    fn insert_backspace(&mut self) {
        let at = self.cursor;
        if at == 0 {
            return;
        }
        let snap = self.snap();
        let start = if ends_with_crlf_before(&snap, at) {
            at - 2
        } else {
            prev_char(&snap, at).unwrap_or(at)
        };
        drop(snap);
        if self.replace(start..at, "") {
            self.cursor = start;
            if let Some(ins) = self.insert.as_mut() {
                ins.typed.pop();
            }
        }
    }

    fn insert_delete_word(&mut self) {
        let at = self.cursor;
        let snap = self.snap();
        let ls = line_start(&snap, row_of(&snap, at));
        let target = if at == ls {
            at.saturating_sub(if ends_with_crlf_before(&snap, at) {
                2
            } else {
                1
            })
        } else {
            word_backward(&snap, at, false).max(ls)
        };
        drop(snap);
        if target < at && self.replace(target..at, "") {
            self.cursor = target;
            if let Some(ins) = self.insert.as_mut() {
                ins.typed.clear();
            }
        }
    }

    fn insert_move(&mut self, key: VimKey) {
        let snap = self.snap();
        let row = row_of(&snap, self.cursor);
        let (s, e) = (line_start(&snap, row), line_end(&snap, row));
        let new = match key {
            VimKey::Left => prev_char(&snap, self.cursor).filter(|p| *p >= s),
            VimKey::Right => next_char(&snap, self.cursor).filter(|_| self.cursor < e),
            VimKey::Home => Some(s),
            VimKey::End => Some(e),
            VimKey::Up | VimKey::Down => {
                let target = if key == VimKey::Up {
                    row.checked_sub(1)
                } else {
                    Some(row + 1).filter(|r| *r <= last_row(&snap))
                };
                target.map(|r| {
                    let col = col_chars(&snap, self.cursor);
                    let p = pos_at_col(&snap, r, col);
                    // `pos_at_col` clamps to the last char; allow end-of-line.
                    if col >= line_str(&snap, r).chars().count() {
                        line_end(&snap, r)
                    } else {
                        p
                    }
                })
            }
            _ => None,
        };
        drop(snap);
        if let Some(p) = new {
            self.cursor = p;
            if let Some(ins) = self.insert.as_mut() {
                ins.typed.clear();
            }
        }
    }

    fn leave_insert(&mut self) {
        if let Some(ins) = self.insert.take() {
            if ins.count > 1 && !ins.typed.is_empty() {
                let eol = self.eol();
                let unit = match ins.kind {
                    InsertKind::Plain => ins.typed.clone(),
                    InsertKind::Open => format!("{eol}{}", ins.typed),
                };
                let rep = unit.repeat(ins.count - 1);
                let at = self.cursor;
                self.replace(at..at, &rep);
                self.cursor = at + rep.len();
            } else if ins.count > 1 && ins.kind == InsertKind::Open {
                let eol = self.eol();
                let rep = eol.repeat(ins.count - 1);
                let at = self.cursor;
                self.replace(at..at, &rep);
                self.cursor = at + rep.len();
            }
        }
        self.mode = VimMode::Normal;
        let snap = self.snap();
        let ls = line_start(&snap, row_of(&snap, self.cursor));
        if self.cursor > ls {
            self.cursor = prev_char(&snap, self.cursor).unwrap_or(self.cursor).max(ls);
        }
        self.cursor = clamp_normal(&snap, self.cursor);
        drop(snap);
        self.want_col = None;
        self.commit();
    }

    // ----------------------------------------------------------------- undo/redo

    fn undo_once(&mut self) {
        if let Some(entry) = self.undo.pop() {
            for edit in entry.edits.iter().rev() {
                if let Err(e) = self
                    .buffer
                    .apply_edit(edit.inverse_range.clone(), &edit.inverse_text)
                {
                    self.last_error = Some(e);
                    break;
                }
            }
            self.cursor = self.clamp(entry.cursor_before);
            self.redo.push(entry);
            self.dirty = false;
        }
    }

    fn redo_once(&mut self) {
        if let Some(entry) = self.redo.pop() {
            for edit in &entry.edits {
                if let Err(e) = self
                    .buffer
                    .apply_edit(edit.forward_range.clone(), &edit.forward_text)
                {
                    self.last_error = Some(e);
                    break;
                }
            }
            self.cursor = self.clamp(entry.cursor_after);
            self.undo.push(entry);
            self.dirty = false;
        }
    }
}

fn motion_name(ch: char) -> Option<&'static str> {
    Some(match ch {
        'h' => "h",
        'l' | ' ' => "l",
        'j' => "j",
        'k' => "k",
        'w' => "w",
        'W' => "W",
        'b' => "b",
        'B' => "B",
        'e' => "e",
        'E' => "E",
        '0' => "0",
        '^' => "^",
        '$' => "$",
        '%' => "%",
        'G' => "G",
        '{' => "{",
        '}' => "}",
        _ => return None,
    })
}

fn convert_case(op: Op, c: char) -> Vec<char> {
    match op {
        Op::Lower => c.to_lowercase().collect(),
        Op::Upper => c.to_uppercase().collect(),
        _ => {
            if c.is_lowercase() {
                c.to_uppercase().collect()
            } else if c.is_uppercase() {
                c.to_lowercase().collect()
            } else {
                vec![c]
            }
        }
    }
}
