//! Lightweight, deterministic, single-pass syntax scanners for YAML, XML, Groovy, and SQL.
//!
//! These languages do not ship external Tree-sitter C grammars in `Cargo.lock`, so `kova_syntax`
//! parses them on the same `rayon` worker pool into a [`ScannedDocument`] containing normalized
//! highlight spans, fold regions, hierarchical structure nodes, `Alt+Up` enclosing ranges, and
//! bounded syntax diagnostics.

pub(crate) mod groovy;
pub(crate) mod sql;
pub(crate) mod xml;
pub(crate) mod yaml;

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::ops::Range;
use std::time::{Duration, Instant};

use kova_buffer::{BufferSnapshot, BufferVersion, Point};
use kova_theme::{SyntaxLanguage, SyntaxTokenKind};

use crate::diagnostics::SyntaxDiagnostic;
use crate::error::SyntaxError;
use crate::folding::FoldRange;
use crate::highlight::HighlightSpan;
use crate::structure::StructureNode;

/// Upper bound on syntax diagnostics retained in a single [`ScannedDocument`].
pub(crate) const MAX_SCANNER_DIAGNOSTICS: usize = 256;

/// Precomputed output of a single-pass scanner parse, stored inside [`crate::SyntaxTree`].
#[derive(Debug, Clone)]
pub(crate) struct ScannedDocument {
    source_len: usize,
    spans: Vec<HighlightSpan>,
    folds: Vec<FoldRange>,
    structure: Vec<StructureNode>,
    enclosing_ranges: Vec<Range<usize>>,
    diagnostics: Vec<SyntaxDiagnostic>,
}

impl ScannedDocument {
    /// Returns all sorted, disjoint [`HighlightSpan`]s for the document.
    #[must_use]
    pub(crate) fn spans(&self) -> &[HighlightSpan] {
        &self.spans
    }

    /// Returns all sorted, deduplicated [`FoldRange`]s for the document.
    #[must_use]
    pub(crate) fn folds(&self) -> &[FoldRange] {
        &self.folds
    }

    /// Returns the top-level [`StructureNode`]s for the document.
    #[must_use]
    pub(crate) fn structure(&self) -> &[StructureNode] {
        &self.structure
    }

    /// Returns bounded syntax diagnostics collected during scanning.
    #[must_use]
    pub(crate) fn diagnostics(&self) -> &[SyntaxDiagnostic] {
        &self.diagnostics
    }

    /// Returns `true` if any syntax errors were detected during scanning.
    #[must_use]
    pub(crate) fn has_error(&self) -> bool {
        !self.diagnostics.is_empty()
    }

    /// Returns the smallest byte range that strictly encloses `start..end`.
    #[must_use]
    pub(crate) fn enclosing_node_range(&self, start: usize, end: usize) -> Option<Range<usize>> {
        if self.source_len == 0 {
            return None;
        }
        let clamped_start = start.min(self.source_len);
        let clamped_end = end.max(clamped_start).min(self.source_len);

        let mut best: Option<Range<usize>> = None;
        let mut consider = |r: Range<usize>| {
            if r.start <= clamped_start
                && r.end >= clamped_end
                && (r.start < clamped_start || r.end > clamped_end)
            {
                let replace = best
                    .as_ref()
                    .is_none_or(|b| (r.end - r.start) < (b.end - b.start));
                if replace {
                    best = Some(r);
                }
            }
        };

        // Check leaf highlight spans via binary search first.
        let span_idx = self
            .spans
            .partition_point(|s| s.range.start <= clamped_start);
        if let Some(prev_idx) = span_idx.checked_sub(1)
            && let Some(span) = self.spans.get(prev_idx)
        {
            consider(span.range.clone());
        }
        if let Some(span) = self.spans.get(span_idx) {
            consider(span.range.clone());
        }

        for range in &self.enclosing_ranges {
            consider(range.clone());
        }

        consider(0..self.source_len);
        best
    }
}

/// Fast UTF-8 byte-offset-to-line/column index built in $O(N)$.
#[derive(Debug, Clone)]
pub(crate) struct LineIndex {
    line_starts: Vec<usize>,
}

impl LineIndex {
    /// Builds a [`LineIndex`] for `source`.
    #[must_use]
    pub(crate) fn new(source: &str) -> Self {
        let mut line_starts = Vec::with_capacity(source.len() / 32 + 1);
        line_starts.push(0);
        for (idx, &byte) in source.as_bytes().iter().enumerate() {
            if byte == b'\n' {
                line_starts.push(idx + 1);
            }
        }
        Self { line_starts }
    }

    /// Returns the number of lines in the index (at least 1).
    #[must_use]
    pub(crate) fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    /// Returns the 0-based line index containing `byte`.
    #[must_use]
    pub(crate) fn line_at(&self, byte: usize) -> usize {
        self.line_starts
            .partition_point(|&start| start <= byte)
            .saturating_sub(1)
    }

    /// Converts `byte` to a 0-based `(row, utf8_byte_col)` [`Point`].
    #[must_use]
    pub(crate) fn point_at(&self, byte: usize) -> Point {
        let row = self.line_at(byte);
        let col = byte.saturating_sub(self.line_starts[row]);
        Point::new(row, col)
    }

    /// Returns the raw byte range of `line` (including any trailing `\r\n` or `\n`).
    #[must_use]
    pub(crate) fn raw_line_range(&self, line: usize, total_len: usize) -> Range<usize> {
        let start = self.line_starts.get(line).copied().unwrap_or(total_len);
        let end = self.line_starts.get(line + 1).copied().unwrap_or(total_len);
        start..end
    }

    /// Returns the end byte offset of `line` excluding trailing `\r` or `\n`.
    #[must_use]
    pub(crate) fn line_content_end(&self, line: usize, source: &str) -> usize {
        let bytes = source.as_bytes();
        let start = self.line_starts.get(line).copied().unwrap_or(bytes.len());
        let mut end = self
            .line_starts
            .get(line + 1)
            .copied()
            .unwrap_or(bytes.len());
        if end > start && bytes[end - 1] == b'\n' {
            end -= 1;
        }
        if end > start && bytes[end - 1] == b'\r' {
            end -= 1;
        }
        end
    }
}

/// Cooperative cancellation and timeout tracker polled periodically by scanners.
pub(crate) struct ScanBudget<'a> {
    started: Instant,
    timeout: Option<Duration>,
    cancel: Option<&'a dyn Fn() -> Option<BufferVersion>>,
    snapshot_version: BufferVersion,
    ticks: usize,
}

impl<'a> ScanBudget<'a> {
    /// Creates a new [`ScanBudget`].
    #[must_use]
    pub(crate) fn new(
        started: Instant,
        timeout: Option<Duration>,
        cancel: Option<&'a dyn Fn() -> Option<BufferVersion>>,
        snapshot_version: BufferVersion,
    ) -> Self {
        Self {
            started,
            timeout,
            cancel,
            snapshot_version,
            ticks: 0,
        }
    }

    /// Advances the tick counter and checks cancellation/timeout every 256 iterations.
    ///
    /// # Errors
    ///
    /// Returns [`SyntaxError::ParseCancelled`] or [`SyntaxError::ParseTimedOut`] if aborted.
    pub(crate) fn tick(&mut self) -> Result<(), SyntaxError> {
        self.ticks = self.ticks.wrapping_add(1);
        if self.ticks.trailing_zeros() >= 8 {
            self.check_now()?;
        }
        Ok(())
    }

    /// Immediately checks cancellation and timeout conditions.
    ///
    /// # Errors
    ///
    /// Returns [`SyntaxError::ParseCancelled`] or [`SyntaxError::ParseTimedOut`] if aborted.
    pub(crate) fn check_now(&self) -> Result<(), SyntaxError> {
        if let Some(cancel_fn) = self.cancel
            && let Some(latest) = cancel_fn()
        {
            return Err(SyntaxError::ParseCancelled {
                requested: self.snapshot_version,
                latest,
            });
        }
        if let Some(timeout) = self.timeout
            && self.started.elapsed() >= timeout
        {
            let elapsed_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
            return Err(SyntaxError::ParseTimedOut { elapsed_ms });
        }
        Ok(())
    }
}

/// Accumulates highlight spans and guarantees strict sortedness, UTF-8 char boundaries,
/// non-emptiness, and disjointness.
#[derive(Debug, Default)]
pub(crate) struct SpanBuilder {
    entries: Vec<(usize, usize, SyntaxTokenKind)>,
}

impl SpanBuilder {
    /// Creates a new [`SpanBuilder`] with preallocated capacity.
    #[must_use]
    pub(crate) fn with_capacity(cap: usize) -> Self {
        Self {
            entries: Vec::with_capacity(cap),
        }
    }

    /// Records a highlight span `[start..end)`. Empty or inverted ranges are ignored.
    pub(crate) fn push(&mut self, start: usize, end: usize, kind: SyntaxTokenKind) {
        if start < end {
            self.entries.push((start, end, kind));
        }
    }

    /// Normalizes all pushed spans into sorted, disjoint, coalesced [`HighlightSpan`]s on valid
    /// UTF-8 char boundaries.
    #[must_use]
    pub(crate) fn finish(self, source: &str) -> Vec<HighlightSpan> {
        let len = source.len();
        if self.entries.is_empty() || len == 0 {
            return Vec::new();
        }

        let mut is_fast_disjoint = true;
        let mut prev_end = 0usize;
        for &(start, end, _) in &self.entries {
            if end > len
                || start >= end
                || start < prev_end
                || !source.is_char_boundary(start)
                || !source.is_char_boundary(end)
            {
                is_fast_disjoint = false;
                break;
            }
            prev_end = end;
        }

        if is_fast_disjoint {
            let mut out: Vec<HighlightSpan> = Vec::with_capacity(self.entries.len());
            for (start, end, kind) in self.entries {
                if let Some(last) = out.last_mut()
                    && last.range.end == start
                    && last.kind == kind
                {
                    last.range.end = end;
                } else {
                    out.push(HighlightSpan::new(start..end, kind));
                }
            }
            return out;
        }

        // General normalization fallback: clamp to UTF-8 boundaries, prefer narrower spans,
        // then later spans for ties.
        let mut indexed: Vec<(usize, usize, usize, usize, SyntaxTokenKind)> = self
            .entries
            .into_iter()
            .enumerate()
            .filter_map(|(order, (s, e, kind))| {
                let start = floor_char_boundary(source, s.min(len));
                let end = ceil_char_boundary(source, e.min(len));
                (start < end).then_some((end - start, order, start, end, kind))
            })
            .collect();

        indexed.sort_unstable_by_key(|&(span_len, order, start, end, _)| {
            (span_len, Reverse(order), start, end)
        });

        let mut claimed: BTreeMap<usize, (usize, SyntaxTokenKind)> = BTreeMap::new();
        let mut gaps: Vec<(usize, usize)> = Vec::new();
        for (_, _, start, end, kind) in indexed {
            let mut pos = start;
            if let Some((_, &(prev_end, _))) = claimed.range(..=pos).next_back()
                && prev_end > pos
            {
                pos = prev_end;
            }
            if pos >= end {
                continue;
            }

            gaps.clear();
            for (&next_start, &(next_end, _)) in claimed.range(pos..end) {
                if pos < next_start {
                    gaps.push((pos, next_start));
                }
                pos = pos.max(next_end);
                if pos >= end {
                    break;
                }
            }
            if pos < end {
                gaps.push((pos, end));
            }
            for &(gap_start, gap_end) in &gaps {
                claimed.insert(gap_start, (gap_end, kind));
            }
        }

        let mut out: Vec<HighlightSpan> = Vec::with_capacity(claimed.len());
        for (start, (end, kind)) in claimed {
            if let Some(last) = out.last_mut()
                && last.range.end == start
                && last.kind == kind
            {
                last.range.end = end;
            } else {
                out.push(HighlightSpan::new(start..end, kind));
            }
        }
        out
    }
}

fn floor_char_boundary(source: &str, mut index: usize) -> usize {
    index = index.min(source.len());
    while index > 0 && !source.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(source: &str, mut index: usize) -> usize {
    index = index.min(source.len());
    while index < source.len() && !source.is_char_boundary(index) {
        index += 1;
    }
    index
}

/// Region comment marker parsed from `# region`, `// region`, or `<!-- region -->` comments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegionMarker {
    Start,
    End,
}

/// Parses a region fold marker from the inner text of a comment (after `//`, `#`, or `<!--`).
#[must_use]
pub(crate) fn parse_region_keyword(body: &str) -> Option<RegionMarker> {
    let lower = body.trim().to_ascii_lowercase();
    let is_word_boundary = |s: &str, prefix: &str| -> bool {
        s.strip_prefix(prefix).is_some_and(|rest| {
            rest.is_empty() || rest.starts_with(char::is_whitespace) || rest.starts_with(':')
        })
    };

    if is_word_boundary(&lower, "region") || is_word_boundary(&lower, "#region") {
        Some(RegionMarker::Start)
    } else if is_word_boundary(&lower, "endregion") || is_word_boundary(&lower, "#endregion") {
        Some(RegionMarker::End)
    } else {
        None
    }
}

/// Collapses runs of whitespace and truncates to `max_chars` Unicode scalar values.
#[must_use]
pub(crate) fn compact_text(text: &str, max_chars: usize) -> String {
    crate::input::compact_whitespace_and_truncate(text, max_chars)
}

/// Pushes a diagnostic up to [`MAX_SCANNER_DIAGNOSTICS`].
pub(crate) fn push_diagnostic(
    diagnostics: &mut Vec<SyntaxDiagnostic>,
    point: Point,
    message: String,
) {
    if diagnostics.len() < MAX_SCANNER_DIAGNOSTICS {
        diagnostics.push(SyntaxDiagnostic {
            line: point.row,
            col: point.column,
            message,
        });
    }
}

/// Finalizes folds using the canonical deduplication and sorting order.
pub(crate) fn finalize_folds(folds: &mut Vec<FoldRange>, total_lines: usize, total_bytes: usize) {
    folds.retain(|f| {
        f.start_line < f.end_line
            && f.end_line < total_lines
            && f.collapsed_byte_range.start <= f.collapsed_byte_range.end
            && f.collapsed_byte_range.end <= total_bytes
    });
    crate::folding::dedup_and_sort_folds(folds);
}

/// Parses `snapshot` using the built-in deterministic scanner for `language`.
///
/// # Errors
///
/// Returns [`SyntaxError::UnsupportedLanguage`] if `language` is a Tree-sitter grammar, or
/// [`SyntaxError::ParseCancelled`] / [`SyntaxError::ParseTimedOut`] if aborted.
pub(crate) fn parse_snapshot(
    language: SyntaxLanguage,
    snapshot: &BufferSnapshot,
    budget: &mut ScanBudget<'_>,
) -> Result<ScannedDocument, SyntaxError> {
    budget.check_now()?;
    let source = snapshot.text();
    let doc = match language {
        SyntaxLanguage::Yaml => yaml::scan(&source, budget)?,
        SyntaxLanguage::Xml => xml::scan(&source, budget)?,
        SyntaxLanguage::Groovy => groovy::scan(&source, budget)?,
        SyntaxLanguage::Sql => sql::scan(&source, budget)?,
        SyntaxLanguage::Java
        | SyntaxLanguage::Kotlin
        | SyntaxLanguage::Http
        | SyntaxLanguage::Toml
        | SyntaxLanguage::Json => {
            return Err(SyntaxError::UnsupportedLanguage { language });
        }
    };
    budget.check_now()?;
    Ok(doc)
}
