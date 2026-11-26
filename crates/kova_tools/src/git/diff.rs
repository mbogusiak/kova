//! Line diff (Myers) of a live buffer against its `HEAD` version, as gutter hunks and
//! unified preview lines.
//!
//! The functions here are pure and allocation-light: they work on `&str` slices and are
//! fast enough to run on every edit from a background thread.

use std::collections::HashMap;

/// Upper bound on the edit distance explored by Myers; beyond it the changed middle
/// section is reported as one modified block (keeps memory at `O(MAX_EDIT_DISTANCE²)`).
const MAX_EDIT_DISTANCE: usize = 1024;

/// Kind of change shown on the editor gutter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LineChangeKind {
    /// Lines that do not exist in `HEAD` (green bar).
    Added,
    /// Lines that replace different `HEAD` lines (blue bar).
    Modified,
    /// `HEAD` lines removed before `start_line` (red marker, no buffer lines).
    Deleted,
}

/// One contiguous gutter change, in 0-based buffer line coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GutterHunk {
    /// Kind of change.
    pub kind: LineChangeKind,
    /// First affected buffer line; for [`LineChangeKind::Deleted`] the line the removal
    /// happened before (may equal the buffer line count for a removal at the end).
    pub start_line: usize,
    /// Number of buffer lines covered (`0` for [`LineChangeKind::Deleted`]).
    pub line_count: usize,
    /// First affected `HEAD` line.
    pub base_start: usize,
    /// Number of `HEAD` lines removed or replaced.
    pub base_count: usize,
}

impl GutterHunk {
    /// Returns `true` when buffer line `line` is painted by this hunk (a deleted marker
    /// is attributed to the line it sits on top of).
    #[must_use]
    pub const fn covers(&self, line: usize) -> bool {
        match self.kind {
            LineChangeKind::Deleted => line == self.start_line,
            LineChangeKind::Added | LineChangeKind::Modified => {
                line >= self.start_line && line < self.start_line + self.line_count
            }
        }
    }
}

/// Kind of a unified diff preview line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiffLineKind {
    /// Unchanged line.
    Context,
    /// Line present only in the new text.
    Added,
    /// Line present only in the old text.
    Removed,
}

/// One line of a unified diff preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    /// Kind of line.
    pub kind: DiffLineKind,
    /// Line text without its terminator.
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Equal,
    Delete,
    Insert,
}

/// Splits text into lines the way the editor numbers them (no terminators, `\r` stripped).
fn split_lines(text: &str) -> Vec<&str> {
    text.lines().collect()
}

/// Computes gutter hunks for `new` (the live buffer) against `old` (the `HEAD` blob).
#[must_use]
pub fn gutter_hunks(old: &str, new: &str) -> Vec<GutterHunk> {
    let old_lines = split_lines(old);
    let new_lines = split_lines(new);
    let ops = line_ops(&old_lines, &new_lines);

    let mut hunks = Vec::new();
    let (mut old_pos, mut new_pos) = (0_usize, 0_usize);
    let mut idx = 0;
    while idx < ops.len() {
        if ops[idx] == Op::Equal {
            old_pos += 1;
            new_pos += 1;
            idx += 1;
            continue;
        }
        let (base_start, start_line) = (old_pos, new_pos);
        let (mut removed, mut added) = (0, 0);
        while idx < ops.len() && ops[idx] != Op::Equal {
            if ops[idx] == Op::Delete {
                removed += 1;
            } else {
                added += 1;
            }
            idx += 1;
        }
        old_pos += removed;
        new_pos += added;
        let kind = match (removed, added) {
            (0, _) => LineChangeKind::Added,
            (_, 0) => LineChangeKind::Deleted,
            _ => LineChangeKind::Modified,
        };
        hunks.push(GutterHunk {
            kind,
            start_line,
            line_count: added,
            base_start,
            base_count: removed,
        });
    }
    hunks
}

/// Builds a full unified preview (every line, with context) of `old` → `new`.
#[must_use]
pub fn unified_lines(old: &str, new: &str) -> Vec<DiffLine> {
    let old_lines = split_lines(old);
    let new_lines = split_lines(new);
    let ops = line_ops(&old_lines, &new_lines);
    let (mut oi, mut ni) = (0, 0);
    ops.iter()
        .map(|op| {
            let (kind, text) = match op {
                Op::Equal => {
                    oi += 1;
                    ni += 1;
                    (DiffLineKind::Context, new_lines[ni - 1])
                }
                Op::Delete => {
                    oi += 1;
                    (DiffLineKind::Removed, old_lines[oi - 1])
                }
                Op::Insert => {
                    ni += 1;
                    (DiffLineKind::Added, new_lines[ni - 1])
                }
            };
            DiffLine {
                kind,
                text: text.to_owned(),
            }
        })
        .collect()
}

/// Maps a buffer line to the `HEAD` line it corresponds to, or `None` when the line is
/// added or modified (uncommitted) according to `hunks`.
#[must_use]
pub fn base_line_for(hunks: &[GutterHunk], line: usize) -> Option<usize> {
    let mut delta: isize = 0;
    for hunk in hunks {
        if hunk.kind != LineChangeKind::Deleted && hunk.covers(line) {
            return None;
        }
        if hunk.start_line + hunk.line_count <= line {
            delta += signed(hunk.base_count) - signed(hunk.line_count);
        } else {
            break;
        }
    }
    usize::try_from(signed(line) + delta).ok()
}

fn signed(value: usize) -> isize {
    isize::try_from(value).unwrap_or(isize::MAX)
}

/// Interns lines to integer ids so Myers compares `u32`s instead of strings.
fn intern<'a>(lines: &[&'a str], table: &mut HashMap<&'a str, u32>) -> Vec<u32> {
    lines
        .iter()
        .map(|line| {
            let next = u32::try_from(table.len()).unwrap_or(u32::MAX);
            *table.entry(line).or_insert(next)
        })
        .collect()
}

/// Returns the edit script turning `old` into `new`, one [`Op`] per line.
fn line_ops(old: &[&str], new: &[&str]) -> Vec<Op> {
    let mut table = HashMap::with_capacity(old.len() + new.len());
    let a = intern(old, &mut table);
    let b = intern(new, &mut table);

    let prefix = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();

    let mut ops = vec![Op::Equal; prefix];
    let a_mid = &a[prefix..a.len() - suffix];
    let b_mid = &b[prefix..b.len() - suffix];
    if let Some(mid) = myers(a_mid, b_mid) {
        ops.extend(mid);
    } else {
        ops.extend(std::iter::repeat_n(Op::Delete, a_mid.len()));
        ops.extend(std::iter::repeat_n(Op::Insert, b_mid.len()));
    }
    ops.extend(std::iter::repeat_n(Op::Equal, suffix));
    ops
}

/// Myers O(ND) diff; `None` when the edit distance exceeds [`MAX_EDIT_DISTANCE`].
#[allow(
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::many_single_char_names,
    reason = "diagonal indices are bounded by MAX_EDIT_DISTANCE and line counts; names follow the Myers paper"
)]
fn myers(a: &[u32], b: &[u32]) -> Option<Vec<Op>> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    if n == 0 || m == 0 {
        let mut ops = vec![Op::Delete; a.len()];
        ops.extend(std::iter::repeat_n(Op::Insert, b.len()));
        return Some(ops);
    }
    let d_max = (a.len() + b.len()).min(MAX_EDIT_DISTANCE) as isize;
    let offset = d_max + 1;
    let mut v = vec![0_isize; (2 * offset + 1) as usize];
    // trace[d] holds V for diagonals -(d+1)..=(d+1) *before* step d.
    let mut trace: Vec<Vec<isize>> = Vec::new();
    let at = |k: isize| (k + offset) as usize;

    let mut found = None;
    'outer: for d in 0..=d_max {
        trace.push(v[at(-d - 1)..=at(d + 1)].to_vec());
        let mut k = -d;
        while k <= d {
            let mut x = if k == -d || (k != d && v[at(k - 1)] < v[at(k + 1)]) {
                v[at(k + 1)]
            } else {
                v[at(k - 1)] + 1
            };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[at(k)] = x;
            if x >= n && y >= m {
                found = Some(d);
                break 'outer;
            }
            k += 2;
        }
    }
    found?;

    let mut rev = Vec::with_capacity(a.len() + b.len());
    let (mut x, mut y) = (n, m);
    for d in (0..trace.len() as isize).rev() {
        let snap = &trace[d as usize];
        let get = |k: isize| snap[(k + d + 1) as usize];
        let k = x - y;
        let prev_k = if k == -d || (k != d && get(k - 1) < get(k + 1)) {
            k + 1
        } else {
            k - 1
        };
        let prev_x = if d == 0 { 0 } else { get(prev_k) };
        let prev_y = if d == 0 { 0 } else { prev_x - prev_k };
        while x > prev_x && y > prev_y {
            rev.push(Op::Equal);
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            if x == prev_x {
                rev.push(Op::Insert);
            } else {
                rev.push(Op::Delete);
            }
        }
        x = prev_x;
        y = prev_y;
    }
    rev.reverse();
    Some(rev)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunk(kind: LineChangeKind, start: usize, count: usize) -> (LineChangeKind, usize, usize) {
        (kind, start, count)
    }

    fn simple(hunks: &[GutterHunk]) -> Vec<(LineChangeKind, usize, usize)> {
        hunks
            .iter()
            .map(|h| (h.kind, h.start_line, h.line_count))
            .collect()
    }

    #[test]
    fn identical_text_has_no_hunks() {
        assert_eq!(gutter_hunks("a\nb\nc\n", "a\nb\nc\n"), Vec::new());
        assert_eq!(gutter_hunks("", ""), Vec::new());
    }

    #[test]
    fn detects_added_modified_and_deleted_lines() {
        let old = "one\ntwo\nthree\nfour\nfive\n";
        let new = "one\nTWO\nthree\nfive\nsix\nseven\n";
        assert_eq!(
            simple(&gutter_hunks(old, new)),
            vec![
                hunk(LineChangeKind::Modified, 1, 1),
                hunk(LineChangeKind::Deleted, 3, 0),
                hunk(LineChangeKind::Added, 4, 2),
            ]
        );
    }

    #[test]
    fn new_file_is_all_added_and_cleared_file_is_deleted() {
        assert_eq!(
            simple(&gutter_hunks("", "a\nb\n")),
            vec![hunk(LineChangeKind::Added, 0, 2)]
        );
        assert_eq!(
            simple(&gutter_hunks("a\nb\n", "")),
            vec![hunk(LineChangeKind::Deleted, 0, 0)]
        );
    }

    #[test]
    fn unified_preview_interleaves_removed_and_added() {
        let lines = unified_lines("a\nb\nc\n", "a\nB\nc\n");
        let kinds: Vec<_> = lines.iter().map(|l| (l.kind, l.text.as_str())).collect();
        assert_eq!(
            kinds,
            vec![
                (DiffLineKind::Context, "a"),
                (DiffLineKind::Removed, "b"),
                (DiffLineKind::Added, "B"),
                (DiffLineKind::Context, "c"),
            ]
        );
    }

    #[test]
    fn maps_buffer_lines_back_to_head_lines() {
        let old = "a\nb\nc\nd\n";
        let new = "x\na\nB\nd\n";
        let hunks = gutter_hunks(old, new);
        assert_eq!(base_line_for(&hunks, 0), None);
        assert_eq!(base_line_for(&hunks, 1), Some(0));
        assert_eq!(base_line_for(&hunks, 2), None);
        assert_eq!(base_line_for(&hunks, 3), Some(3));
    }

    #[test]
    fn myers_script_reproduces_both_sides() {
        let old: Vec<String> = (0..300).map(|i| format!("l{}", i % 17)).collect();
        let new: Vec<String> = (0..280).map(|i| format!("l{}", (i * 7) % 19)).collect();
        let old_text = old.join("\n");
        let new_text = new.join("\n");
        let preview = unified_lines(&old_text, &new_text);
        let rebuilt_old: Vec<&str> = preview
            .iter()
            .filter(|l| l.kind != DiffLineKind::Added)
            .map(|l| l.text.as_str())
            .collect();
        let rebuilt_new: Vec<&str> = preview
            .iter()
            .filter(|l| l.kind != DiffLineKind::Removed)
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(
            rebuilt_old,
            old.iter().map(String::as_str).collect::<Vec<_>>()
        );
        assert_eq!(
            rebuilt_new,
            new.iter().map(String::as_str).collect::<Vec<_>>()
        );
    }

    #[test]
    fn huge_rewrite_falls_back_to_one_modified_block() {
        let old = (0..5000)
            .map(|i| format!("old {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let new = (0..5000)
            .map(|i| format!("new {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            simple(&gutter_hunks(&old, &new)),
            vec![hunk(LineChangeKind::Modified, 0, 5000)]
        );
    }
}
