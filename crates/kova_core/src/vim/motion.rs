//! Pure text geometry, motions and text objects for the Vim engine.
//!
//! Everything here operates on a [`BufferSnapshot`] in $O(\log N + \text{line\_len})$
//! time and UTF-8 byte offsets that lie on `char` boundaries. Line terminators
//! (`\n` and `\r\n`) are never split.

#![allow(
    clippy::many_single_char_names,
    clippy::too_many_lines,
    reason = "text-geometry code uses conventional short names (s/e/c/i) and one large motion table"
)]

use std::borrow::Cow;
use std::ops::Range;

use kova_buffer::BufferSnapshot;

/// Last 0-based line index following Vim semantics: a trailing line
/// terminator does not start an additional empty line.
pub(crate) fn last_row(snap: &BufferSnapshot) -> usize {
    let len = snap.len_bytes();
    if len == 0 {
        return 0;
    }
    let rope_lines = snap.len_lines();
    if snap.rope().get_byte(len - 1) == Some(b'\n') {
        rope_lines.saturating_sub(2)
    } else {
        rope_lines.saturating_sub(1)
    }
}

pub(crate) fn row_of(snap: &BufferSnapshot, pos: usize) -> usize {
    let pos = pos.min(snap.len_bytes());
    snap.rope().byte_to_line(pos).min(last_row(snap))
}

pub(crate) fn line_start(snap: &BufferSnapshot, row: usize) -> usize {
    let row = row.min(last_row(snap));
    snap.rope().line_to_byte(row)
}

/// Start of the next line (or text length), i.e. end including terminator.
pub(crate) fn line_end_incl(snap: &BufferSnapshot, row: usize) -> usize {
    let lr = last_row(snap);
    if row < lr {
        snap.rope().line_to_byte(row + 1)
    } else {
        snap.len_bytes()
    }
}

/// End of the line content, excluding the terminator.
pub(crate) fn line_end(snap: &BufferSnapshot, row: usize) -> usize {
    let s = line_start(snap, row);
    let mut e = line_end_incl(snap, row);
    let rope = snap.rope();
    if e > s && rope.get_byte(e - 1) == Some(b'\n') {
        e -= 1;
        if e > s && rope.get_byte(e - 1) == Some(b'\r') {
            e -= 1;
        }
    }
    e
}

/// Content of line `row` (excluding terminator) without allocating when contiguous.
pub(crate) fn line_str(snap: &BufferSnapshot, row: usize) -> Cow<'_, str> {
    let row = row.min(last_row(snap));
    let Ok(slice) = snap.line_slice(row) else {
        return Cow::Borrowed("");
    };
    slice
        .as_str()
        .map_or_else(|| Cow::Owned(slice.to_string()), Cow::Borrowed)
}

pub(crate) fn first_nonblank(snap: &BufferSnapshot, row: usize) -> usize {
    let s = line_start(snap, row);
    let e = line_end(snap, row);
    let line = line_str(snap, row);
    let off = line
        .char_indices()
        .find(|(_, c)| *c != ' ' && *c != '\t')
        .map_or(e, |(i, _)| s + i);
    clamp_normal(snap, off)
}

pub(crate) fn col_chars(snap: &BufferSnapshot, pos: usize) -> usize {
    let row = row_of(snap, pos);
    let s = line_start(snap, row);
    let clamped = pos.max(s).min(snap.len_bytes());
    let rope = snap.rope();
    rope.byte_to_char(clamped)
        .saturating_sub(rope.byte_to_char(s))
}

/// Position of the `col`-th char of `row`, clamped to the last char.
pub(crate) fn pos_at_col(snap: &BufferSnapshot, row: usize, col: usize) -> usize {
    let s = line_start(snap, row);
    let e = line_end(snap, row);
    if e <= s {
        return s;
    }
    let rope = snap.rope();
    let sc = rope.byte_to_char(s);
    let ec = rope.byte_to_char(e);
    if ec <= sc {
        return s;
    }
    let target_ci = (sc.saturating_add(col)).min(ec - 1);
    rope.char_to_byte(target_ci)
}

/// Clamps `pos` so it denotes a char (not a terminator) unless the line is empty.
pub(crate) fn clamp_normal(snap: &BufferSnapshot, pos: usize) -> usize {
    let pos = snap.clamp_byte_to_char_boundary(pos);
    let row = row_of(snap, pos);
    let (s, e) = (line_start(snap, row), line_end(snap, row));
    if e == s || pos < s {
        return s;
    }
    if pos >= e {
        return prev_char(snap, e).unwrap_or(s);
    }
    pos
}

pub(crate) fn char_at(snap: &BufferSnapshot, pos: usize) -> Option<char> {
    if pos >= snap.len_bytes() {
        return None;
    }
    let rope = snap.rope();
    let ci = rope.byte_to_char(pos);
    if rope.char_to_byte(ci) != pos {
        return None;
    }
    rope.get_char(ci)
}

pub(crate) fn next_char(snap: &BufferSnapshot, pos: usize) -> Option<usize> {
    char_at(snap, pos).map(|c| pos + c.len_utf8())
}

pub(crate) fn prev_char(snap: &BufferSnapshot, pos: usize) -> Option<usize> {
    if pos == 0 {
        return None;
    }
    let pos = pos.min(snap.len_bytes());
    let rope = snap.rope();
    let ci = rope.byte_to_char(pos);
    if ci == 0 {
        return None;
    }
    Some(rope.char_to_byte(ci - 1))
}

pub(crate) fn ends_with_crlf_before(snap: &BufferSnapshot, pos: usize) -> bool {
    pos >= 2
        && snap.rope().get_byte(pos - 2) == Some(b'\r')
        && snap.rope().get_byte(pos - 1) == Some(b'\n')
}

pub(crate) fn starts_with_crlf_at(snap: &BufferSnapshot, pos: usize) -> bool {
    snap.rope().get_byte(pos) == Some(b'\r') && snap.rope().get_byte(pos + 1) == Some(b'\n')
}

fn is_eol(c: char) -> bool {
    c == '\n' || c == '\r'
}

/// Vim character class: 0 blank, 1 keyword, 2 punctuation (1 for non-blank when `big`).
fn class(c: char, big: bool) -> u8 {
    if c.is_whitespace() {
        0
    } else if big || c.is_alphanumeric() || c == '_' {
        1
    } else {
        2
    }
}

/// Inclusive/exclusive/linewise nature of a motion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MotionKind {
    Exclusive,
    Inclusive,
    Linewise,
}

/// Result of resolving a motion.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Motion {
    pub(crate) target: usize,
    pub(crate) kind: MotionKind,
    pub(crate) vertical: bool,
    pub(crate) paragraph: bool,
    pub(crate) word: bool,
}

impl Motion {
    pub(crate) const fn new(target: usize, kind: MotionKind) -> Self {
        Self {
            target,
            kind,
            vertical: false,
            paragraph: false,
            word: false,
        }
    }
}

/// Byte range selected by an operator together with its linewise flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpRange {
    pub(crate) range: Range<usize>,
    pub(crate) linewise: bool,
}

/// Converts `(cursor, motion)` into the range acted upon by an operator.
///
/// Backward exclusive motions are right-exclusive with respect to the cursor
/// character (`dFb` on `abcd` with cursor on `d` removes `bc`).
pub(crate) fn operator_range(snap: &BufferSnapshot, cursor: usize, m: Motion) -> OpRange {
    let mut target = m.target;
    if m.word && row_of(snap, target) > row_of(snap, cursor) && target > cursor {
        // `dw` on the last word of a line stops at the end of that word.
        let mut i = target;
        while let Some(p) = prev_char(snap, i) {
            if char_at(snap, p).is_some_and(char::is_whitespace) {
                i = p;
            } else {
                break;
            }
        }
        target = i.max(cursor);
    }
    let (a, b) = if target < cursor {
        (target, cursor)
    } else {
        (cursor, target)
    };
    match m.kind {
        MotionKind::Linewise => {
            let (ra, rb) = (row_of(snap, a), row_of(snap, b));
            OpRange {
                range: line_start(snap, ra)..line_end_incl(snap, rb),
                linewise: true,
            }
        }
        MotionKind::Inclusive => {
            let end = match char_at(snap, b) {
                Some(c) if !is_eol(c) => b + c.len_utf8(),
                _ => b,
            };
            OpRange {
                range: a..end,
                linewise: false,
            }
        }
        MotionKind::Exclusive => {
            let mut end = b;
            let (ra, rb) = (row_of(snap, a), row_of(snap, b));
            if m.paragraph && rb > ra && end == snap.len_bytes() && end != line_start(snap, rb) {
                // `}` stopped at the end of the last line: inclusive of that line.
                if a <= first_nonblank(snap, ra) {
                    return OpRange {
                        range: line_start(snap, ra)..line_end_incl(snap, rb),
                        linewise: true,
                    };
                }
            } else if rb > ra && end == line_start(snap, rb) {
                if m.paragraph && a <= first_nonblank(snap, ra) {
                    return OpRange {
                        range: line_start(snap, ra)..line_end_incl(snap, rb - 1),
                        linewise: true,
                    };
                }
                end = line_end(snap, rb - 1);
            }
            OpRange {
                range: a..end.max(a),
                linewise: false,
            }
        }
    }
}

/// `w`/`W`: start of next word, stopping at empty lines.
pub(crate) fn word_forward(snap: &BufferSnapshot, pos: usize, big: bool) -> usize {
    let len = snap.len_bytes();
    if pos >= len {
        return len;
    }
    let rope = snap.rope();
    let mut chars = rope.chars_at(rope.byte_to_char(pos));
    let Some(c0) = chars.next() else {
        return len;
    };
    let mut i = pos;
    let mut at_line_start = false;
    let k0 = class(c0, big);
    if k0 != 0 {
        i += c0.len_utf8();
        for c in chars.by_ref() {
            if class(c, big) == k0 {
                i += c.len_utf8();
            } else {
                if class(c, big) != 0 {
                    return i;
                }
                if c == '\n' {
                    at_line_start = true;
                }
                i += c.len_utf8();
                break;
            }
        }
    } else {
        if c0 == '\n' {
            at_line_start = true;
        }
        i += c0.len_utf8();
    }
    for c in chars {
        if class(c, big) != 0 {
            return i;
        }
        if at_line_start && (c == '\n' || (c == '\r' && rope.get_byte(i + 1) == Some(b'\n'))) {
            return i;
        }
        at_line_start = c == '\n';
        i += c.len_utf8();
    }
    len
}

/// `b`/`B`: start of previous word.
pub(crate) fn word_backward(snap: &BufferSnapshot, pos: usize, big: bool) -> usize {
    if pos == 0 {
        return 0;
    }
    let rope = snap.rope();
    let mut chars = rope.chars_at(rope.byte_to_char(pos.min(snap.len_bytes())));
    let mut i = pos.min(snap.len_bytes());
    let pos_row = row_of(snap, i);
    let k = loop {
        let Some(c) = chars.prev() else {
            return 0;
        };
        i -= c.len_utf8();
        let kc = class(c, big);
        if kc != 0 {
            break kc;
        }
        if c == '\n' {
            let r = row_of(snap, i);
            if r < pos_row && line_start(snap, r) == line_end(snap, r) {
                return line_start(snap, r);
            }
        }
    };
    while let Some(c) = chars.prev() {
        if class(c, big) == k {
            i -= c.len_utf8();
        } else {
            break;
        }
    }
    i
}

/// `e`/`E`: last char of the current/next word.
pub(crate) fn word_end_forward(snap: &BufferSnapshot, pos: usize, big: bool) -> usize {
    let Some(mut i) = next_char(snap, pos) else {
        return pos;
    };
    let rope = snap.rope();
    let mut chars = rope.chars_at(rope.byte_to_char(i));
    let mut k = 0;
    for c in chars.by_ref() {
        let kc = class(c, big);
        if kc == 0 {
            i += c.len_utf8();
        } else {
            k = kc;
            break;
        }
    }
    if k == 0 {
        return prev_char(snap, snap.len_bytes()).unwrap_or(pos).max(pos);
    }
    for c in chars {
        if class(c, big) == k {
            i += c.len_utf8();
        } else {
            break;
        }
    }
    i
}

/// Target of `cw`/`cW`: end of the current word (not the following blanks).
pub(crate) fn change_word_target(
    snap: &BufferSnapshot,
    pos: usize,
    count: usize,
    big: bool,
) -> usize {
    let mut j = pos;
    for n in 0..count.max(1) {
        let k = char_at(snap, j).map_or(0, |c| class(c, big));
        let at_end = next_char(snap, j)
            .and_then(|nx| char_at(snap, nx))
            .is_none_or(|c| class(c, big) != k);
        j = if n == 0 && !at_end {
            let mut i = j;
            while let Some(nx) = next_char(snap, i) {
                if char_at(snap, nx).is_some_and(|c| class(c, big) == k) {
                    i = nx;
                } else {
                    break;
                }
            }
            i
        } else if n == 0 {
            j
        } else {
            word_end_forward(snap, j, big)
        };
    }
    j
}

/// Whether `pos` is on a non-blank character.
pub(crate) fn on_word(snap: &BufferSnapshot, pos: usize) -> bool {
    char_at(snap, pos).is_some_and(|c| !c.is_whitespace())
}

/// `ge`/`gE`: last char of the previous word.
pub(crate) fn word_end_backward(snap: &BufferSnapshot, pos: usize, big: bool) -> usize {
    let pos = pos.min(snap.len_bytes());
    let rope = snap.rope();
    let mut i = pos;
    let mut chars = rope.chars_at(rope.byte_to_char(i));
    let k = char_at(snap, i).map_or(0, |c| class(c, big));
    if k != 0 {
        while let Some(c) = chars.prev() {
            if class(c, big) == k {
                i -= c.len_utf8();
            } else if class(c, big) != 0 {
                return i - c.len_utf8();
            } else {
                i -= c.len_utf8();
                break;
            }
        }
    }
    while let Some(c) = chars.prev() {
        i -= c.len_utf8();
        if class(c, big) != 0 {
            return i;
        }
    }
    0
}

fn str_next_char(s: &str, pos: usize) -> Option<usize> {
    s.get(pos..)
        .and_then(|t| t.chars().next())
        .map(|c| pos + c.len_utf8())
}

fn str_prev_char(s: &str, pos: usize) -> Option<usize> {
    s.get(..pos)
        .and_then(|t| t.char_indices().next_back().map(|(i, _)| i))
}

fn str_char_at(s: &str, pos: usize) -> Option<char> {
    s.get(pos..).and_then(|t| t.chars().next())
}

/// `f`/`F`/`t`/`T` search on the current line. Returns the motion target.
pub(crate) fn find_char(
    snap: &BufferSnapshot,
    pos: usize,
    kind: char,
    ch: char,
    count: usize,
    repeat: bool,
) -> Option<usize> {
    let row = row_of(snap, pos);
    let s = line_start(snap, row);
    let line = line_str(snap, row);
    let len = line.len();
    let rel_pos = pos.saturating_sub(s).min(len);
    let forward = kind == 'f' || kind == 't';
    let till = kind == 't' || kind == 'T';
    let mut remaining = count.max(1);
    let mut from = rel_pos;
    if repeat && till {
        from = if forward {
            str_next_char(&line, rel_pos)
                .filter(|&n| n < len)
                .unwrap_or(rel_pos)
        } else {
            str_prev_char(&line, rel_pos).unwrap_or(rel_pos)
        };
    }
    loop {
        from = if forward {
            let n = str_next_char(&line, from)?;
            if n >= len {
                return None;
            }
            n
        } else {
            str_prev_char(&line, from)?
        };
        if str_char_at(&line, from) == Some(ch) {
            remaining -= 1;
            if remaining == 0 {
                break;
            }
        }
    }
    let rel_target = if till {
        if forward {
            str_prev_char(&line, from)?
        } else {
            str_next_char(&line, from)?
        }
    } else {
        from
    };
    Some(s + rel_target)
}

/// `}` (forward) / `{` (backward) paragraph motion.
pub(crate) fn paragraph(snap: &BufferSnapshot, pos: usize, forward: bool, count: usize) -> usize {
    let lr = last_row(snap);
    let blank = |r: usize| line_start(snap, r) == line_end(snap, r);
    let mut row = row_of(snap, pos);
    for _ in 0..count.max(1) {
        if forward {
            while row < lr && blank(row) {
                row += 1;
            }
            while row < lr && !blank(row) {
                row += 1;
            }
            if row == lr && !blank(row) {
                return snap.len_bytes();
            }
        } else {
            while row > 0 && blank(row) {
                row -= 1;
            }
            while row > 0 && !blank(row) {
                row -= 1;
            }
            if row == 0 && !blank(0) {
                return 0;
            }
        }
    }
    line_start(snap, row)
}

const fn matching_bracket(b: u8) -> Option<(u8, bool)> {
    match b {
        b'(' => Some((b')', true)),
        b'[' => Some((b']', true)),
        b'{' => Some((b'}', true)),
        b')' => Some((b'(', false)),
        b']' => Some((b'[', false)),
        b'}' => Some((b'{', false)),
        _ => None,
    }
}

/// `%`: finds a bracket on the current line (at or after `pos`) and jumps to
/// its matching partner across lines, respecting nesting.
pub(crate) fn match_bracket(snap: &BufferSnapshot, pos: usize) -> Option<usize> {
    let row = row_of(snap, pos);
    let s = line_start(snap, row);
    let line = line_str(snap, row);
    let rel = pos.saturating_sub(s).min(line.len());
    let (rel_off, open_b, mate, forward) = line[rel..]
        .bytes()
        .enumerate()
        .find_map(|(i, b)| matching_bracket(b).map(|(m, fwd)| (rel + i, b, m, fwd)))?;
    let start_pos = s + rel_off;
    let rope = snap.rope();
    let mut depth = 0usize;
    if forward {
        for (i, b) in (start_pos + 1..).zip(rope.bytes_at(start_pos + 1)) {
            if b == open_b {
                depth += 1;
            } else if b == mate {
                if depth == 0 {
                    return Some(i);
                }
                depth -= 1;
            }
        }
        None
    } else {
        let mut i = start_pos;
        let mut bytes = rope.bytes_at(start_pos);
        while let Some(b) = bytes.prev() {
            i -= 1;
            if b == open_b {
                depth += 1;
            } else if b == mate {
                if depth == 0 {
                    return Some(i);
                }
                depth -= 1;
            }
        }
        None
    }
}

/// `iw` / `aw` / `iW` / `aW`.
pub(crate) fn word_object(
    snap: &BufferSnapshot,
    pos: usize,
    around: bool,
    big: bool,
) -> Option<Range<usize>> {
    let row = row_of(snap, pos);
    let (s, e) = (line_start(snap, row), line_end(snap, row));
    if s == e {
        return Some(pos..pos);
    }
    let line = line_str(snap, row);
    let rel = pos.saturating_sub(s);
    let c = str_char_at(&line, rel).filter(|c| !is_eol(*c))?;
    let k = class(c, big);
    let run = |from: usize, kk: u8| -> Range<usize> {
        let mut a = from;
        while let Some(p) = str_prev_char(&line, a) {
            if str_char_at(&line, p).is_some_and(|ch| class(ch, big) == kk) {
                a = p;
            } else {
                break;
            }
        }
        let mut b = from;
        while let Some(n) = str_next_char(&line, b) {
            if str_char_at(&line, b).is_some_and(|ch| class(ch, big) == kk) {
                b = n;
            } else {
                break;
            }
        }
        a..b
    };
    let r = run(rel, k);
    if !around {
        return Some((s + r.start)..(s + r.end));
    }
    if k != 0 {
        let after = r.end;
        let kk = str_char_at(&line, after).map(|ch| class(ch, big));
        if kk == Some(0) {
            return Some((s + r.start)..(s + run(after, 0).end));
        }
        // No trailing blanks: take leading blanks.
        let mut a = r.start;
        while let Some(p) = str_prev_char(&line, a) {
            if str_char_at(&line, p).is_some_and(char::is_whitespace) {
                a = p;
            } else {
                break;
            }
        }
        Some((s + a)..(s + r.end))
    } else {
        let after = r.end;
        if after < line.len() {
            let kk = str_char_at(&line, after).map_or(0, |ch| class(ch, big));
            return Some((s + r.start)..(s + run(after, kk).end));
        }
        Some((s + r.start)..(s + r.end))
    }
}

/// `i"`/`a"` (also `'` and `` ` ``) on the current line.
pub(crate) fn quote_object(
    snap: &BufferSnapshot,
    pos: usize,
    quote: char,
    around: bool,
) -> Option<Range<usize>> {
    let row = row_of(snap, pos);
    let s = line_start(snap, row);
    let line = line_str(snap, row);
    let mut quotes = Vec::new();
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == quote {
            quotes.push(s + i);
        }
    }
    let pair = quotes
        .chunks_exact(2)
        .find(|p| p[0] <= pos && pos <= p[1])
        .or_else(|| quotes.chunks_exact(2).find(|p| p[0] > pos))?;
    let (open, close) = (pair[0], pair[1]);
    if !around {
        return Some(open + quote.len_utf8()..close);
    }
    let rel_after = (close + quote.len_utf8()).saturating_sub(s);
    let mut rel_end = rel_after;
    while let Some(c) = str_char_at(&line, rel_end) {
        if c == ' ' || c == '\t' {
            rel_end += 1;
        } else {
            break;
        }
    }
    if rel_end > rel_after {
        return Some(open..(s + rel_end));
    }
    let mut rel_start = open.saturating_sub(s);
    while let Some(p) = str_prev_char(&line, rel_start) {
        if str_char_at(&line, p).is_some_and(|c| c == ' ' || c == '\t') {
            rel_start = p;
        } else {
            break;
        }
    }
    Some((s + rel_start)..(s + rel_after))
}

/// `i(`/`a(`/`i{`/… bracket text objects (multi-line, nesting and `[count]` aware).
pub(crate) fn bracket_object(
    snap: &BufferSnapshot,
    pos: usize,
    open: char,
    close: char,
    count: usize,
    around: bool,
) -> Option<Range<usize>> {
    let open_b = u8::try_from(open).ok()?;
    let close_b = u8::try_from(close).ok()?;
    let rope = snap.rope();
    let mut remaining = count.max(1);
    let o = if rope.get_byte(pos) == Some(open_b) && {
        remaining -= 1;
        remaining == 0
    } {
        pos
    } else {
        let mut depth = 0usize;
        let mut i = pos;
        let mut bytes = rope.bytes_at(pos);
        loop {
            let b = bytes.prev()?;
            i -= 1;
            if b == close_b {
                depth += 1;
            } else if b == open_b {
                if depth == 0 {
                    remaining -= 1;
                    if remaining == 0 {
                        break i;
                    }
                } else {
                    depth -= 1;
                }
            }
        }
    };
    let mut depth = 0usize;
    let mut i = o + 1;
    let c = {
        let mut found = None;
        for b in rope.bytes_at(i) {
            if b == open_b {
                depth += 1;
            } else if b == close_b {
                if depth == 0 {
                    found = Some(i);
                    break;
                }
                depth -= 1;
            }
            i += 1;
        }
        found?
    };
    if around {
        return Some(o..c + 1);
    }
    let mut s = o + 1;
    let mut e = c;
    let skipped = if starts_with_crlf_at(snap, s) {
        s += 2;
        true
    } else if rope.get_byte(s) == Some(b'\n') {
        s += 1;
        true
    } else {
        false
    };
    if skipped && s <= e {
        let erow = row_of(snap, e);
        let ls = line_start(snap, erow);
        if ls >= s
            && snap
                .slice_bytes(ls..e)
                .is_ok_and(|tail| tail.chars().all(|ch| ch == ' ' || ch == '\t'))
        {
            e = ls;
        }
    }
    Some(s..e.max(s))
}

#[cfg(test)]
mod tests {
    use kova_buffer::Buffer;

    use super::*;

    fn snap(text: &str) -> BufferSnapshot {
        Buffer::from_text(text).snapshot()
    }

    #[test]
    fn lines_ignore_trailing_terminator() {
        let s = snap("a\nb\n");
        assert_eq!(last_row(&s), 1);
        assert_eq!(line_end(&s, 0), 1);
        assert_eq!(line_end(&snap("a\r\nb"), 0), 1);
        assert_eq!(last_row(&snap("")), 0);
    }

    #[test]
    fn words_forward_backward() {
        let s = snap("foo bar.baz");
        assert_eq!(word_forward(&s, 0, false), 4);
        assert_eq!(word_forward(&s, 4, false), 7);
        assert_eq!(word_forward(&s, 4, true), s.len_bytes());
        assert_eq!(word_backward(&s, 8, false), 7);
        assert_eq!(word_backward(&s, 4, false), 0);
        assert_eq!(word_end_forward(&s, 0, false), 2);
        assert_eq!(word_end_backward(&s, 4, false), 2);
    }

    #[test]
    fn w_stops_on_empty_line() {
        let s = snap("a\n\nb");
        assert_eq!(word_forward(&s, 0, false), 2);
        assert_eq!(word_forward(&s, 2, false), 3);
        assert_eq!(word_backward(&s, 3, false), 2);
    }

    #[test]
    fn find_motions() {
        let s = snap("a,b,c");
        assert_eq!(find_char(&s, 0, 'f', ',', 1, false), Some(1));
        assert_eq!(find_char(&s, 0, 't', ',', 1, false), Some(0));
        assert_eq!(find_char(&s, 0, 'f', ',', 2, false), Some(3));
        assert_eq!(find_char(&s, 4, 'F', ',', 1, false), Some(3));
        assert_eq!(find_char(&s, 4, 'T', ',', 1, false), Some(4));
        assert_eq!(find_char(&s, 0, 'f', 'z', 1, false), None);
    }

    #[test]
    fn backward_exclusive_range() {
        let s = snap("abcd");
        let m = Motion::new(1, MotionKind::Exclusive);
        assert_eq!(operator_range(&s, 3, m).range, 1..3);
    }

    #[test]
    fn bracket_matching_motion() {
        assert_eq!(match_bracket(&snap("f(a(b)c)"), 1), Some(7));
        assert_eq!(match_bracket(&snap("f(a(b)c)"), 0), Some(7));
        assert_eq!(match_bracket(&snap("f(a(b)c)"), 7), Some(1));
        assert_eq!(match_bracket(&snap("f(a(b)c)"), 3), Some(5));
        assert_eq!(match_bracket(&snap("{\n  [x]\n}"), 0), Some(8));
        assert_eq!(match_bracket(&snap("abc"), 0), None);
    }

    #[test]
    fn objects() {
        assert_eq!(word_object(&snap("foo bar"), 1, false, false), Some(0..3));
        assert_eq!(word_object(&snap("foo bar"), 1, true, false), Some(0..4));
        assert_eq!(word_object(&snap("foo bar"), 5, true, false), Some(3..7));
        assert_eq!(quote_object(&snap("x \"hi\" y"), 4, '"', false), Some(3..5));
        assert_eq!(quote_object(&snap("x \"hi\" y"), 4, '"', true), Some(2..7));
        assert_eq!(
            bracket_object(&snap("f(a(b)c)"), 2, '(', ')', 1, false),
            Some(2..7)
        );
        assert_eq!(
            bracket_object(&snap("f(a(b)c)"), 4, '(', ')', 1, true),
            Some(3..6)
        );
        assert_eq!(
            bracket_object(&snap("f(a(b)c)"), 4, '(', ')', 2, false),
            Some(2..7)
        );
        assert_eq!(
            bracket_object(&snap("f {\n  x\n}"), 6, '{', '}', 1, false),
            Some(4..8)
        );
        assert_eq!(bracket_object(&snap("abc"), 1, '(', ')', 1, false), None);
    }
}
