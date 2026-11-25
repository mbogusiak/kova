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
    Lines, Motion, MotionKind, bracket_object, change_word_target, char_at, find_char, next_char,
    on_word, operator_range, paragraph, prev_char, quote_object, word_backward, word_end_backward,
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
struct Snap {
    snapshot: BufferSnapshot,
    cursor: usize,
}

/// Headless modal Vim engine bound to a [`Buffer`].
#[derive(Debug)]
pub struct VimEngine {
    buffer: Buffer,
    text: String,
    mode: VimMode,
    cursor: usize,
    anchor: usize,
    want_col: Option<usize>,
    pending: Pending,
    cmd_keys: Vec<VimKey>,
    policy: PolicyTable,
    registers: HashMap<char, Register>,
    last_find: Option<(char, char)>,
    undo: Vec<Snap>,
    redo: Vec<Snap>,
    checkpoint: Option<Snap>,
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
        let text = buffer.snapshot().text();
        Self {
            buffer,
            text,
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
            checkpoint: None,
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
    pub fn text(&self) -> &str {
        &self.text
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
        let mut o = offset.min(self.text.len());
        while !self.text.is_char_boundary(o) {
            o -= 1;
        }
        self.cursor = if self.mode == VimMode::Insert {
            o
        } else {
            Lines::new(&self.text).clamp_normal(o)
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

    // ---------------------------------------------------------------- plumbing

    fn snap(&self) -> Snap {
        Snap {
            snapshot: self.buffer.snapshot(),
            cursor: self.cursor,
        }
    }

    fn replace(&mut self, range: Range<usize>, with: &str) -> bool {
        match self.buffer.apply_edit(range.clone(), with) {
            Ok(_) => {
                self.text.replace_range(range, with);
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
        match self.text.find('\n') {
            Some(i) if i > 0 && self.text.as_bytes()[i - 1] == b'\r' => "\r\n",
            _ => "\n",
        }
    }

    fn clamp(&self, pos: usize) -> usize {
        Lines::new(&self.text).clamp_normal(pos)
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
            if let Some(cp) = self.checkpoint.take() {
                self.undo.push(cp);
                self.redo.clear();
            }
            if self.change_flag && !self.replaying {
                self.last_change = self.cmd_keys.clone();
            }
        }
        self.dirty = false;
        self.change_flag = false;
        self.checkpoint = None;
        self.cmd_keys.clear();
        self.pending = Pending::default();
    }

    fn cancel(&mut self) {
        self.pending = Pending::default();
        self.cmd_keys.clear();
        self.checkpoint = None;
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
            self.checkpoint = Some(self.snap());
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
                    let len = self.text.len();
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
        let lines = Lines::new(&self.text);
        let ra = lines.row_of(self.cursor);
        let rb = (ra + n - 1).min(lines.last_row());
        let range = lines.start(ra)..lines.end_incl(rb);
        drop(lines);
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
        let lines = Lines::new(&self.text);
        let t = self.text.as_str();
        let c = self.cursor;
        let row = lines.row_of(c);
        let big = name.chars().last().is_some_and(|x| x.is_ascii_uppercase()) && name.len() == 1;
        let excl = |target: usize| Some(Motion::new(target, MotionKind::Exclusive));
        let incl = |target: usize| Some(Motion::new(target, MotionKind::Inclusive));
        match name {
            "h" => {
                let s = lines.start(row);
                let mut i = c;
                for _ in 0..n {
                    match prev_char(t, i) {
                        Some(p) if p >= s => i = p,
                        _ => break,
                    }
                }
                (i != c).then_some(())?;
                excl(i)
            }
            "l" => {
                let e = lines.end(row);
                let mut i = c;
                for _ in 0..n {
                    match next_char(t, i) {
                        Some(nx) if i < e => i = nx.min(e),
                        _ => break,
                    }
                }
                (i != c).then_some(())?;
                excl(i)
            }
            "j" | "k" => {
                let new_row = if name == "j" {
                    (row + n).min(lines.last_row())
                } else {
                    row.saturating_sub(n)
                };
                (new_row != row).then_some(())?;
                let col = self.want_col.unwrap_or_else(|| lines.col_chars(c));
                let mut m = Motion::new(lines.pos_at_col(new_row, col), MotionKind::Linewise);
                m.vertical = true;
                Some(m)
            }
            "0" => excl(lines.start(row)),
            "^" => excl(lines.first_nonblank(row)),
            "$" => {
                let r2 = (row + n - 1).min(lines.last_row());
                let e = lines.end(r2);
                incl(if e > lines.start(r2) {
                    prev_char(t, e).unwrap_or(e)
                } else {
                    e
                })
            }
            "w" | "W" => {
                if self.pending.op == Some(Op::Change) && on_word(t, c) {
                    return incl(change_word_target(t, c, n, big));
                }
                let mut i = c;
                for _ in 0..n {
                    i = word_forward(t, i, big);
                }
                let mut m = Motion::new(i, MotionKind::Exclusive);
                m.word = true;
                Some(m)
            }
            "b" | "B" => {
                let mut i = c;
                for _ in 0..n {
                    i = word_backward(t, i, big);
                }
                (i != c).then_some(())?;
                excl(i)
            }
            "e" | "E" => {
                let mut i = c;
                for _ in 0..n {
                    i = word_end_forward(t, i, big);
                }
                incl(i)
            }
            "ge" | "gE" => {
                let mut i = c;
                for _ in 0..n {
                    i = word_end_backward(t, i, name == "gE");
                }
                (i != c).then_some(())?;
                incl(i)
            }
            "gg" | "G" => {
                let r = match (has_count, name) {
                    (true, _) => (n - 1).min(lines.last_row()),
                    (false, "gg") => 0,
                    _ => lines.last_row(),
                };
                Some(Motion::new(lines.first_nonblank(r), MotionKind::Linewise))
            }
            "}" | "{" => {
                let i = paragraph(&lines, c, name == "}", n);
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
            let lines = Lines::new(&self.text);
            let r = operator_range(&lines, self.cursor, m);
            drop(lines);
            self.mark_change(op);
            self.apply_op(op, r.range, r.linewise, 1);
        } else {
            if m.vertical && self.want_col.is_none() {
                self.want_col = Some(Lines::new(&self.text).col_chars(self.cursor));
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
                if c.is_ascii_alphabetic() || c == '"' || c == '_' {
                    self.pending.register = Some(c.to_ascii_lowercase());
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
        let lines = Lines::new(&self.text);
        let target = find_char(&lines, self.cursor, kind, ch, n, repeat);
        drop(lines);
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
        let t = self.text.as_str();
        let pos = self.cursor;
        let range = match c {
            'w' => word_object(t, pos, around, false),
            'W' => word_object(t, pos, around, true),
            '"' | '\'' | '`' => quote_object(t, pos, c, around),
            '(' | ')' | 'b' => bracket_object(t, pos, '(', ')', around),
            '{' | '}' | 'B' => bracket_object(t, pos, '{', '}', around),
            '[' | ']' => bracket_object(t, pos, '[', ']', around),
            '<' | '>' => bracket_object(t, pos, '<', '>', around),
            _ => None,
        };
        let Some(range) = range else {
            return true;
        };
        if self.mode.is_visual() {
            if let Some(last) = prev_char(&self.text, range.end).filter(|l| *l >= range.start) {
                self.anchor = range.start;
                self.cursor = last;
                self.mode = VimMode::Visual;
            }
            return true;
        }
        if let Some(op) = self.pending.op {
            self.mark_change(op);
            self.apply_op(op, range, false, 1);
        }
        true
    }

    // -------------------------------------------------------------- operators

    fn store(&mut self, text: &str, linewise: bool) {
        let mut t = text.replace("\r\n", "\n");
        if linewise && !t.ends_with('\n') {
            t.push('\n');
        }
        let reg = Register { text: t, linewise };
        match self.pending.register {
            Some('_') => return,
            Some(n) if n != '"' => {
                self.registers.insert(n, reg.clone());
            }
            _ => {}
        }
        self.registers.insert('"', reg);
    }

    fn apply_op(&mut self, op: Op, range: Range<usize>, linewise: bool, times: usize) {
        match op {
            Op::Yank => self.op_yank(&range, linewise),
            Op::Delete => {
                if linewise {
                    self.delete_lines(range);
                } else if !range.is_empty() {
                    let s = self.text[range.clone()].to_string();
                    self.store(&s, false);
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
        let s = self.text[range.clone()].to_string();
        self.store(&s, linewise);
        let lines = Lines::new(&self.text);
        let ra = lines.row_of(range.start);
        let new_cursor = if linewise {
            if lines.row_of(self.cursor) == ra {
                self.cursor
            } else {
                lines.pos_at_col(ra, lines.col_chars(self.cursor))
            }
        } else {
            range.start.min(self.cursor)
        };
        drop(lines);
        self.cursor = self.clamp(new_cursor);
    }

    fn op_change(&mut self, range: Range<usize>, linewise: bool) {
        let start = range.start;
        let kill = if linewise {
            let lines = Lines::new(&self.text);
            let rb = lines.row_of(range.end.saturating_sub(1).max(range.start));
            range.start..lines.end(rb)
        } else {
            range.clone()
        };
        if !range.is_empty() {
            let s = self.text[range.clone()].to_string();
            self.store(&s, linewise);
        }
        if !kill.is_empty() {
            self.replace(kill, "");
        }
        self.cursor = start;
        self.enter_insert(1, InsertKind::Plain);
    }

    fn delete_lines(&mut self, range: Range<usize>) {
        let s = self.text[range.clone()].to_string();
        self.store(&s, true);
        let len = self.text.len();
        let mut del = range.clone();
        if range.end == len && range.start > 0 && !s.ends_with('\n') {
            let sep = if self.text[..range.start].ends_with("\r\n") {
                2
            } else {
                1
            };
            del = range.start - sep..len;
        }
        let start = del.start;
        self.replace(del, "");
        let lines = Lines::new(&self.text);
        let row = lines.row_of(start).min(lines.last_row());
        self.cursor = lines.first_nonblank(row);
    }

    fn shift_range(&mut self, range: &Range<usize>, indent: bool, times: usize) {
        let lines = Lines::new(&self.text);
        let ra = lines.row_of(range.start);
        let rb = lines.row_of(range.end.saturating_sub(1).max(range.start));
        let rows: Vec<(usize, usize)> = (ra..=rb).map(|r| (lines.start(r), lines.end(r))).collect();
        drop(lines);
        let amount = SHIFT_WIDTH * times.max(1);
        for (s, e) in rows.into_iter().rev() {
            if indent {
                if e > s {
                    self.replace(s..s, &" ".repeat(amount));
                }
            } else {
                let mut removed = 0;
                let mut end = s;
                for b in self.text[s..e].chars() {
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
        let lines = Lines::new(&self.text);
        self.cursor = lines.first_nonblank(ra.min(lines.last_row()));
    }

    fn case_range(&mut self, op: Op, range: Range<usize>) {
        let start = range.start;
        let new: String = self.text[range.clone()]
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
        let lines = Lines::new(&self.text);
        let row = lines.row_of(self.cursor);
        let (s, e) = (lines.start(row), lines.end(row));
        let first = lines.first_nonblank(row);
        let first_raw = if first < s { s } else { first };
        drop(lines);
        let eol = self.eol();
        let mut kind = InsertKind::Plain;
        match ch {
            'a' => {
                if self.cursor < e {
                    self.cursor = next_char(&self.text, self.cursor).unwrap_or(e).min(e);
                }
            }
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
            let lines = Lines::new(&self.text);
            let row = lines.row_of(self.cursor);
            if row >= lines.last_row() {
                break;
            }
            let e = lines.end(row);
            let next_start = lines.start(row + 1);
            let next_end = lines.end(row + 1);
            let mut ws_end = next_start;
            if spaced {
                ws_end += self.text[next_start..next_end]
                    .chars()
                    .take_while(|c| *c == ' ' || *c == '\t')
                    .map(char::len_utf8)
                    .sum::<usize>();
            }
            let cur_empty = e == lines.start(row);
            let ends_ws = self.text[..e]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
            let next_empty = ws_end >= next_end;
            let starts_paren = char_at(&self.text, ws_end) == Some(')');
            drop(lines);
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
            let new: String = self.text[range.clone()]
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
        let lines = Lines::new(&self.text);
        let row = lines.row_of(self.cursor);
        let e = lines.end(row);
        drop(lines);
        let chars: Vec<usize> = self.text[self.cursor..e]
            .char_indices()
            .map(|(i, _)| i)
            .collect();
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
        let lines = Lines::new(&self.text);
        let row = lines.row_of(self.cursor);
        let e = lines.end(row);
        drop(lines);
        let mut end = self.cursor;
        for _ in 0..n {
            match next_char(&self.text, end) {
                Some(nx) if end < e => end = nx.min(e),
                _ => break,
            }
        }
        if end == self.cursor {
            return;
        }
        let start = self.cursor;
        self.case_range(Op::Toggle, start..end);
        self.cursor = self.clamp(end);
    }

    fn paste(&mut self, after: bool, n: usize) {
        let name = self.pending.register.unwrap_or('"');
        let Some(reg) = self.registers.get(&name).cloned() else {
            return;
        };
        let eol = self.eol();
        let body = reg.text.replace('\n', eol);
        let lines = Lines::new(&self.text);
        let row = lines.row_of(self.cursor);
        let (s, e, ei) = (lines.start(row), lines.end(row), lines.end_incl(row));
        drop(lines);
        if reg.linewise {
            let chunk = body.repeat(n);
            let (at, ins) = if after {
                if ei == self.text.len() && e == ei {
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
            let lines = Lines::new(&self.text);
            self.cursor = lines.first_nonblank(lines.row_of(first_line_at));
        } else {
            let at = if after && self.cursor < e {
                next_char(&self.text, self.cursor).unwrap_or(e)
            } else {
                self.cursor
            };
            let ins = body.repeat(n);
            self.replace(at..at, &ins);
            self.cursor = if ins.contains('\n') {
                at
            } else {
                self.clamp(prev_char(&self.text, at + ins.len()).unwrap_or(at))
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
        let lines = Lines::new(&self.text);
        if self.mode == VimMode::VisualLine {
            return (
                lines.start(lines.row_of(a))..lines.end_incl(lines.row_of(b)),
                true,
            );
        }
        let end = if self.text[b..].starts_with("\r\n") {
            b + 2
        } else {
            next_char(&self.text, b).unwrap_or(b)
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
        let lines = Lines::new(&self.text);
        let (ra, rb) = (
            lines.row_of(range.start),
            lines.row_of(range.end.saturating_sub(1).max(range.start)),
        );
        let start = lines.start(ra);
        drop(lines);
        self.exit_visual();
        self.cursor = start;
        self.pending.count = Some(rb - ra + 1);
        self.pending.op_count = None;
        self.join(true);
    }

    fn visual_paste(&mut self) {
        let name = self.pending.register.unwrap_or('"');
        let Some(reg) = self.registers.get(&name).cloned() else {
            return;
        };
        let (range, line_sel) = self.visual_range();
        let eol = self.eol();
        let mut body = reg.text.replace('\n', eol);
        self.exit_visual();
        let replaced = self.text[range.clone()].to_string();
        if reg.linewise && !line_sel {
            body = format!("{eol}{body}");
        } else if !reg.linewise && line_sel {
            body.push_str(eol);
        }
        let start = range.start;
        self.replace(range, &body);
        self.pending.register = None;
        self.store(&replaced, line_sel);
        let end = start + body.len();
        self.cursor = if reg.linewise || body.contains('\n') {
            self.clamp(start)
        } else {
            self.clamp(prev_char(&self.text, end).unwrap_or(start))
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
                let end = if self.text[at..].starts_with("\r\n") {
                    at + 2
                } else {
                    next_char(&self.text, at).unwrap_or(at)
                };
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
        let start = if self.text[..at].ends_with("\r\n") {
            at - 2
        } else {
            prev_char(&self.text, at).unwrap_or(at)
        };
        if self.replace(start..at, "") {
            self.cursor = start;
            if let Some(ins) = self.insert.as_mut() {
                ins.typed.pop();
            }
        }
    }

    fn insert_delete_word(&mut self) {
        let at = self.cursor;
        let lines = Lines::new(&self.text);
        let ls = lines.start(lines.row_of(at));
        drop(lines);
        let target = if at == ls {
            at.saturating_sub(if self.text[..at].ends_with("\r\n") {
                2
            } else {
                1
            })
        } else {
            word_backward(&self.text, at, false).max(ls)
        };
        if target < at && self.replace(target..at, "") {
            self.cursor = target;
            if let Some(ins) = self.insert.as_mut() {
                ins.typed.clear();
            }
        }
    }

    fn insert_move(&mut self, key: VimKey) {
        let lines = Lines::new(&self.text);
        let row = lines.row_of(self.cursor);
        let (s, e) = (lines.start(row), lines.end(row));
        let new = match key {
            VimKey::Left => prev_char(&self.text, self.cursor).filter(|p| *p >= s),
            VimKey::Right => next_char(&self.text, self.cursor).filter(|_| self.cursor < e),
            VimKey::Home => Some(s),
            VimKey::End => Some(e),
            VimKey::Up | VimKey::Down => {
                let target = if key == VimKey::Up {
                    row.checked_sub(1)
                } else {
                    Some(row + 1).filter(|r| *r <= lines.last_row())
                };
                target.map(|r| {
                    let col = lines.col_chars(self.cursor);
                    let p = lines.pos_at_col(r, col);
                    // `pos_at_col` clamps to the last char; allow end-of-line.
                    if col >= lines.text()[lines.start(r)..lines.end(r)].chars().count() {
                        lines.end(r)
                    } else {
                        p
                    }
                })
            }
            _ => None,
        };
        drop(lines);
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
        let lines = Lines::new(&self.text);
        let ls = lines.start(lines.row_of(self.cursor));
        drop(lines);
        if self.cursor > ls {
            self.cursor = prev_char(&self.text, self.cursor)
                .unwrap_or(self.cursor)
                .max(ls);
        }
        self.cursor = self.clamp(self.cursor);
        self.want_col = None;
        self.commit();
    }

    // ----------------------------------------------------------------- undo/redo

    fn restore(&mut self, snap: &Snap) {
        let new = snap.snapshot.text();
        let len = self.text.len();
        self.replace(0..len, &new);
        self.cursor = self.clamp(snap.cursor);
        self.dirty = false;
    }

    fn undo_once(&mut self) {
        if let Some(prev) = self.undo.pop() {
            let current = Snap {
                snapshot: self.buffer.snapshot(),
                cursor: self.cursor,
            };
            self.redo.push(current);
            self.restore(&prev);
        }
    }

    fn redo_once(&mut self) {
        if let Some(next) = self.redo.pop() {
            let current = Snap {
                snapshot: self.buffer.snapshot(),
                cursor: self.cursor,
            };
            self.undo.push(current);
            self.restore(&next);
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
