//! Pure text geometry, motions and text objects for the Vim engine.
//!
//! Everything here operates on a `&str` and UTF-8 byte offsets that lie on
//! `char` boundaries. Line terminators (`\n` and `\r\n`) are never split.

#![allow(
    clippy::many_single_char_names,
    clippy::too_many_lines,
    reason = "text-geometry code uses conventional short names (s/e/c/i) and one large motion table"
)]

use std::ops::Range;

/// Line index of a text, following Vim semantics: a trailing line terminator
/// does not start an additional line.
#[derive(Debug)]
pub(crate) struct Lines<'a> {
    text: &'a str,
    starts: Vec<usize>,
}

impl<'a> Lines<'a> {
    pub(crate) fn new(text: &'a str) -> Self {
        let mut starts = vec![0];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' && i + 1 < text.len() {
                starts.push(i + 1);
            }
        }
        Self { text, starts }
    }

    pub(crate) fn text(&self) -> &'a str {
        self.text
    }

    #[cfg(test)]
    pub(crate) fn count(&self) -> usize {
        self.starts.len()
    }

    pub(crate) fn last_row(&self) -> usize {
        self.starts.len() - 1
    }

    pub(crate) fn start(&self, row: usize) -> usize {
        self.starts[row.min(self.last_row())]
    }

    /// Start of the next line (or text length), i.e. end including terminator.
    pub(crate) fn end_incl(&self, row: usize) -> usize {
        if row < self.last_row() {
            self.starts[row + 1]
        } else {
            self.text.len()
        }
    }

    /// End of the line content, excluding the terminator.
    pub(crate) fn end(&self, row: usize) -> usize {
        let s = self.start(row);
        let mut e = self.end_incl(row);
        if e > s && self.text.as_bytes()[e - 1] == b'\n' {
            e -= 1;
            if e > s && self.text.as_bytes()[e - 1] == b'\r' {
                e -= 1;
            }
        }
        e
    }

    pub(crate) fn row_of(&self, pos: usize) -> usize {
        self.starts
            .partition_point(|&s| s <= pos)
            .saturating_sub(1)
            .min(self.last_row())
    }

    pub(crate) fn first_nonblank(&self, row: usize) -> usize {
        let s = self.start(row);
        let e = self.end(row);
        let off = self.text[s..e]
            .char_indices()
            .find(|(_, c)| *c != ' ' && *c != '\t')
            .map_or(e, |(i, _)| s + i);
        self.clamp_normal(off)
    }

    pub(crate) fn col_chars(&self, pos: usize) -> usize {
        let row = self.row_of(pos);
        self.text[self.start(row)..pos.max(self.start(row))]
            .chars()
            .count()
    }

    /// Position of the `col`-th char of `row`, clamped to the last char.
    pub(crate) fn pos_at_col(&self, row: usize, col: usize) -> usize {
        let s = self.start(row);
        let e = self.end(row);
        let mut last = s;
        for (n, (i, _)) in self.text[s..e].char_indices().enumerate() {
            last = s + i;
            if n == col {
                return last;
            }
        }
        last
    }

    /// Clamps `pos` so it denotes a char (not a terminator) unless the line is empty.
    pub(crate) fn clamp_normal(&self, pos: usize) -> usize {
        let pos = pos.min(self.text.len());
        let row = self.row_of(pos);
        let (s, e) = (self.start(row), self.end(row));
        if e == s || pos < s {
            return s;
        }
        if pos >= e {
            return prev_char(self.text, e).unwrap_or(s);
        }
        pos
    }
}

pub(crate) fn char_at(text: &str, pos: usize) -> Option<char> {
    text.get(pos..).and_then(|t| t.chars().next())
}

pub(crate) fn next_char(text: &str, pos: usize) -> Option<usize> {
    char_at(text, pos).map(|c| pos + c.len_utf8())
}

pub(crate) fn prev_char(text: &str, pos: usize) -> Option<usize> {
    text.get(..pos)
        .and_then(|t| t.char_indices().next_back().map(|(i, _)| i))
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
pub(crate) fn operator_range(lines: &Lines<'_>, cursor: usize, m: Motion) -> OpRange {
    let text = lines.text();
    let mut target = m.target;
    if m.word && lines.row_of(target) > lines.row_of(cursor) && target > cursor {
        // `dw` on the last word of a line stops at the end of that word.
        let mut i = target;
        while let Some(p) = prev_char(text, i) {
            if char_at(text, p).is_some_and(char::is_whitespace) {
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
            let (ra, rb) = (lines.row_of(a), lines.row_of(b));
            OpRange {
                range: lines.start(ra)..lines.end_incl(rb),
                linewise: true,
            }
        }
        MotionKind::Inclusive => {
            let end = match char_at(text, b) {
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
            let (ra, rb) = (lines.row_of(a), lines.row_of(b));
            if m.paragraph && rb > ra && end == text.len() && end != lines.start(rb) {
                // `}` stopped at the end of the last line: inclusive of that line.
                if a <= lines.first_nonblank(ra) {
                    return OpRange {
                        range: lines.start(ra)..lines.end_incl(rb),
                        linewise: true,
                    };
                }
            } else if rb > ra && end == lines.start(rb) {
                if m.paragraph && a <= lines.first_nonblank(ra) {
                    return OpRange {
                        range: lines.start(ra)..lines.end_incl(rb - 1),
                        linewise: true,
                    };
                }
                end = lines.end(rb - 1);
            }
            OpRange {
                range: a..end.max(a),
                linewise: false,
            }
        }
    }
}

/// `w`/`W`: start of next word, stopping at empty lines.
pub(crate) fn word_forward(text: &str, pos: usize, big: bool) -> usize {
    let lines = Lines::new(text);
    let mut i = pos;
    let Some(c0) = char_at(text, i) else {
        return text.len();
    };
    let k0 = class(c0, big);
    if k0 != 0 {
        while let Some(c) = char_at(text, i) {
            if class(c, big) == k0 {
                i += c.len_utf8();
            } else {
                break;
            }
        }
    }
    let pos_row = lines.row_of(pos);
    while let Some(c) = char_at(text, i) {
        if class(c, big) != 0 {
            return i;
        }
        let row = lines.row_of(i);
        if row > pos_row && lines.start(row) == lines.end(row) && i == lines.start(row) {
            return i;
        }
        i += c.len_utf8();
    }
    text.len()
}

/// `b`/`B`: start of previous word.
pub(crate) fn word_backward(text: &str, pos: usize, big: bool) -> usize {
    let lines = Lines::new(text);
    let pos_row = lines.row_of(pos);
    let mut i = pos;
    loop {
        let Some(p) = prev_char(text, i) else {
            return 0;
        };
        i = p;
        let c = char_at(text, i).unwrap_or(' ');
        if class(c, big) != 0 {
            break;
        }
        let row = lines.row_of(i);
        if row < pos_row && lines.start(row) == lines.end(row) {
            return lines.start(row);
        }
    }
    let k = char_at(text, i).map_or(0, |c| class(c, big));
    while let Some(p) = prev_char(text, i) {
        if char_at(text, p).is_some_and(|c| class(c, big) == k) {
            i = p;
        } else {
            break;
        }
    }
    i
}

/// `e`/`E`: last char of the current/next word.
pub(crate) fn word_end_forward(text: &str, pos: usize, big: bool) -> usize {
    let Some(mut i) = next_char(text, pos) else {
        return pos;
    };
    while let Some(c) = char_at(text, i) {
        if class(c, big) == 0 {
            i += c.len_utf8();
        } else {
            break;
        }
    }
    let Some(c) = char_at(text, i) else {
        return prev_char(text, text.len()).unwrap_or(pos).max(pos);
    };
    let k = class(c, big);
    while let Some(n) = next_char(text, i) {
        if char_at(text, n).is_some_and(|c| class(c, big) == k) {
            i = n;
        } else {
            break;
        }
    }
    i
}

/// Target of `cw`/`cW`: end of the current word (not the following blanks).
pub(crate) fn change_word_target(text: &str, pos: usize, count: usize, big: bool) -> usize {
    let mut j = pos;
    for n in 0..count.max(1) {
        let k = char_at(text, j).map_or(0, |c| class(c, big));
        let at_end = next_char(text, j)
            .and_then(|nx| char_at(text, nx))
            .is_none_or(|c| class(c, big) != k);
        j = if n == 0 && !at_end {
            let mut i = j;
            while let Some(nx) = next_char(text, i) {
                if char_at(text, nx).is_some_and(|c| class(c, big) == k) {
                    i = nx;
                } else {
                    break;
                }
            }
            i
        } else if n == 0 {
            j
        } else {
            word_end_forward(text, j, big)
        };
    }
    j
}

/// Whether `pos` is on a non-blank character.
pub(crate) fn on_word(text: &str, pos: usize) -> bool {
    char_at(text, pos).is_some_and(|c| !c.is_whitespace())
}

/// `ge`/`gE`: last char of the previous word.
pub(crate) fn word_end_backward(text: &str, pos: usize, big: bool) -> usize {
    let mut i = pos;
    let k = char_at(text, i).map_or(0, |c| class(c, big));
    if k != 0 {
        while let Some(p) = prev_char(text, i) {
            if char_at(text, p).is_some_and(|c| class(c, big) == k) {
                i = p;
            } else {
                break;
            }
        }
    }
    loop {
        let Some(p) = prev_char(text, i) else {
            return 0;
        };
        i = p;
        if char_at(text, i).is_some_and(|c| class(c, big) != 0) {
            return i;
        }
    }
}

/// `f`/`F`/`t`/`T` search on the current line. Returns the motion target.
pub(crate) fn find_char(
    lines: &Lines<'_>,
    pos: usize,
    kind: char,
    ch: char,
    count: usize,
    repeat: bool,
) -> Option<usize> {
    let text = lines.text();
    let row = lines.row_of(pos);
    let (s, e) = (lines.start(row), lines.end(row));
    let forward = kind == 'f' || kind == 't';
    let till = kind == 't' || kind == 'T';
    let mut remaining = count.max(1);
    let mut from = pos;
    if repeat && till {
        from = if forward {
            next_char(text, pos).filter(|&n| n < e).unwrap_or(pos)
        } else {
            prev_char(text, pos).filter(|&p| p >= s).unwrap_or(pos)
        };
    }
    loop {
        from = if forward {
            let n = next_char(text, from)?;
            if n >= e {
                return None;
            }
            n
        } else {
            let p = prev_char(text, from)?;
            if p < s {
                return None;
            }
            p
        };
        if char_at(text, from) == Some(ch) {
            remaining -= 1;
            if remaining == 0 {
                break;
            }
        }
    }
    if till {
        if forward {
            prev_char(text, from)
        } else {
            next_char(text, from)
        }
    } else {
        Some(from)
    }
}

/// `}` (forward) / `{` (backward) paragraph motion.
pub(crate) fn paragraph(lines: &Lines<'_>, pos: usize, forward: bool, count: usize) -> usize {
    let blank = |r: usize| lines.start(r) == lines.end(r);
    let mut row = lines.row_of(pos);
    for _ in 0..count.max(1) {
        if forward {
            while row < lines.last_row() && blank(row) {
                row += 1;
            }
            while row < lines.last_row() && !blank(row) {
                row += 1;
            }
            if row == lines.last_row() && !blank(row) {
                return lines.text().len();
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
    lines.start(row)
}

/// `iw` / `aw` / `iW` / `aW`.
pub(crate) fn word_object(text: &str, pos: usize, around: bool, big: bool) -> Option<Range<usize>> {
    let lines = Lines::new(text);
    let row = lines.row_of(pos);
    let (s, e) = (lines.start(row), lines.end(row));
    if s == e {
        return Some(pos..pos);
    }
    let c = char_at(text, pos).filter(|c| !is_eol(*c))?;
    let k = class(c, big);
    let run = |from: usize, kk: u8| -> Range<usize> {
        let mut a = from;
        while let Some(p) = prev_char(text, a) {
            if p >= s && char_at(text, p).is_some_and(|c| class(c, big) == kk) {
                a = p;
            } else {
                break;
            }
        }
        let mut b = from;
        while let Some(n) = next_char(text, b) {
            if b < e && char_at(text, b).is_some_and(|c| class(c, big) == kk) {
                b = n;
            } else {
                break;
            }
        }
        a..b
    };
    let r = run(pos, k);
    if !around {
        return Some(r);
    }
    let extend_run = |from: usize, kk: u8| -> usize { run(from, kk).end };
    if k != 0 {
        let after = r.end;
        let kk = char_at(text, after)
            .filter(|_| after < e)
            .map(|c| class(c, big));
        if kk == Some(0) {
            return Some(r.start..extend_run(after, 0));
        }
        // No trailing blanks: take leading blanks.
        let mut a = r.start;
        while let Some(p) = prev_char(text, a) {
            if p >= s && char_at(text, p).is_some_and(char::is_whitespace) {
                a = p;
            } else {
                break;
            }
        }
        Some(a..r.end)
    } else {
        let after = r.end;
        if after < e {
            let kk = char_at(text, after).map_or(0, |c| class(c, big));
            return Some(r.start..extend_run(after, kk));
        }
        Some(r)
    }
}

/// `i"`/`a"` (also `'` and `` ` ``) on the current line.
pub(crate) fn quote_object(
    text: &str,
    pos: usize,
    quote: char,
    around: bool,
) -> Option<Range<usize>> {
    let lines = Lines::new(text);
    let row = lines.row_of(pos);
    let (s, e) = (lines.start(row), lines.end(row));
    let mut quotes = Vec::new();
    let mut escaped = false;
    for (i, c) in text[s..e].char_indices() {
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
    let after = close + quote.len_utf8();
    let mut end = after;
    while end < e && char_at(text, end).is_some_and(|c| c == ' ' || c == '\t') {
        end += 1;
    }
    if end > after {
        return Some(open..end);
    }
    let mut start = open;
    while let Some(p) = prev_char(text, start) {
        if p >= s && char_at(text, p).is_some_and(|c| c == ' ' || c == '\t') {
            start = p;
        } else {
            break;
        }
    }
    Some(start..after)
}

/// `i(`/`a(`/`i{`/… bracket text objects (multi-line, nesting aware).
pub(crate) fn bracket_object(
    text: &str,
    pos: usize,
    open: char,
    close: char,
    around: bool,
) -> Option<Range<usize>> {
    let o = if char_at(text, pos) == Some(open) {
        pos
    } else {
        let mut depth = 0usize;
        let mut i = pos;
        loop {
            let p = prev_char(text, i)?;
            i = p;
            let c = char_at(text, i)?;
            if c == close {
                depth += 1;
            } else if c == open {
                if depth == 0 {
                    break i;
                }
                depth -= 1;
            }
        }
    };
    let mut depth = 0usize;
    let mut i = o + open.len_utf8();
    let c = loop {
        let ch = char_at(text, i)?;
        if ch == open {
            depth += 1;
        } else if ch == close {
            if depth == 0 {
                break i;
            }
            depth -= 1;
        }
        i += ch.len_utf8();
    };
    if around {
        return Some(o..c + close.len_utf8());
    }
    let mut s = o + open.len_utf8();
    let mut e = c;
    let skipped = if text[s..].starts_with("\r\n") {
        s += 2;
        true
    } else if text[s..].starts_with('\n') {
        s += 1;
        true
    } else {
        false
    };
    if skipped && s <= e {
        let ls = text[..e].rfind('\n').map_or(0, |i| i + 1);
        if ls >= s && text[ls..e].chars().all(|c| c == ' ' || c == '\t') {
            e = ls;
        }
    }
    Some(s..e.max(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_ignore_trailing_terminator() {
        let l = Lines::new("a\nb\n");
        assert_eq!(l.count(), 2);
        assert_eq!(l.end(0), 1);
        assert_eq!(Lines::new("a\r\nb").end(0), 1);
        assert_eq!(Lines::new("").count(), 1);
    }

    #[test]
    fn words_forward_backward() {
        let t = "foo bar.baz";
        assert_eq!(word_forward(t, 0, false), 4);
        assert_eq!(word_forward(t, 4, false), 7);
        assert_eq!(word_forward(t, 4, true), t.len());
        assert_eq!(word_backward(t, 8, false), 7);
        assert_eq!(word_backward(t, 4, false), 0);
        assert_eq!(word_end_forward(t, 0, false), 2);
        assert_eq!(word_end_backward(t, 4, false), 2);
    }

    #[test]
    fn w_stops_on_empty_line() {
        let t = "a\n\nb";
        assert_eq!(word_forward(t, 0, false), 2);
        assert_eq!(word_forward(t, 2, false), 3);
        assert_eq!(word_backward(t, 3, false), 2);
    }

    #[test]
    fn find_motions() {
        let l = Lines::new("a,b,c");
        assert_eq!(find_char(&l, 0, 'f', ',', 1, false), Some(1));
        assert_eq!(find_char(&l, 0, 't', ',', 1, false), Some(0));
        assert_eq!(find_char(&l, 0, 'f', ',', 2, false), Some(3));
        assert_eq!(find_char(&l, 4, 'F', ',', 1, false), Some(3));
        assert_eq!(find_char(&l, 4, 'T', ',', 1, false), Some(4));
        assert_eq!(find_char(&l, 0, 'f', 'z', 1, false), None);
    }

    #[test]
    fn backward_exclusive_range() {
        let l = Lines::new("abcd");
        let m = Motion::new(1, MotionKind::Exclusive);
        assert_eq!(operator_range(&l, 3, m).range, 1..3);
    }

    #[test]
    fn objects() {
        assert_eq!(word_object("foo bar", 1, false, false), Some(0..3));
        assert_eq!(word_object("foo bar", 1, true, false), Some(0..4));
        assert_eq!(word_object("foo bar", 5, true, false), Some(3..7));
        assert_eq!(quote_object("x \"hi\" y", 4, '"', false), Some(3..5));
        assert_eq!(quote_object("x \"hi\" y", 4, '"', true), Some(2..7));
        assert_eq!(bracket_object("f(a(b)c)", 2, '(', ')', false), Some(2..7));
        assert_eq!(bracket_object("f(a(b)c)", 4, '(', ')', true), Some(3..6));
        assert_eq!(
            bracket_object("f {\n  x\n}", 6, '{', '}', false),
            Some(4..8)
        );
        assert_eq!(bracket_object("abc", 1, '(', ')', false), None);
    }
}
