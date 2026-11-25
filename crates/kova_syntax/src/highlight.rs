//! Viewport syntax highlight extraction and deterministic span normalization.

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::ops::Range;

use kova_buffer::BufferSnapshot;
use kova_theme::{AetherTheme, SyntaxStyle, SyntaxTokenKind};
use tree_sitter::{QueryCursor, StreamingIterator};

use crate::error::SyntaxError;
use crate::input::RopeTextProvider;
use crate::registry::LanguageRegistry;
use crate::tree::SyntaxTree;

/// A single non-empty, non-overlapping syntax highlight span within a buffer snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HighlightSpan {
    /// UTF-8 byte range in the source buffer snapshot (`range.start < range.end`).
    pub range: Range<usize>,
    /// Semantic Aether token kind resolved from the Tree-sitter capture.
    pub kind: SyntaxTokenKind,
}

impl HighlightSpan {
    /// Creates a new [`HighlightSpan`].
    #[must_use]
    pub const fn new(range: Range<usize>, kind: SyntaxTokenKind) -> Self {
        Self { range, kind }
    }

    /// Resolves the [`SyntaxStyle`] for this span from `theme`.
    #[must_use]
    pub const fn style<'a>(&self, theme: &'a AetherTheme) -> &'a SyntaxStyle {
        theme.tokens.syntax.style(self.kind)
    }
}

/// Stateless-per-query Tree-sitter syntax highlighter producing sorted, disjoint
/// [`HighlightSpan`]s restricted to a visible byte or line viewport.
#[derive(Debug, Clone)]
pub struct Highlighter {
    registry: LanguageRegistry,
}

impl Highlighter {
    /// Creates a [`Highlighter`] backed by `registry`.
    #[must_use]
    pub const fn new(registry: LanguageRegistry) -> Self {
        Self { registry }
    }

    /// Creates a [`Highlighter`] using the process-wide cached [`LanguageRegistry::builtin`].
    ///
    /// # Errors
    ///
    /// Returns [`SyntaxError`] if initializing the built-in registry failed.
    pub fn from_builtin() -> Result<Self, SyntaxError> {
        Ok(Self::new(LanguageRegistry::builtin()?.clone()))
    }

    /// Creates a [`Highlighter`] with a freshly compiled [`LanguageRegistry::with_defaults`].
    ///
    /// # Errors
    ///
    /// Returns [`SyntaxError`] if any built-in grammar or highlight query fails validation.
    pub fn with_defaults() -> Result<Self, SyntaxError> {
        Ok(Self::new(LanguageRegistry::with_defaults()?))
    }

    /// Returns a reference to the backing [`LanguageRegistry`].
    #[must_use]
    pub const fn registry(&self) -> &LanguageRegistry {
        &self.registry
    }

    /// Extracts sorted, strictly disjoint [`HighlightSpan`]s within `byte_range`.
    ///
    /// Nested captures (such as `@string.escape` inside `@string`) split outer container spans
    /// so every returned span has `start < end`, fits inside `byte_range`, and satisfies
    /// `prev.range.end <= next.range.start`.
    ///
    /// # Errors
    ///
    /// Returns [`SyntaxError::InvalidByteRange`] if `byte_range.start > byte_range.end` or
    /// `byte_range.end > snapshot.len_bytes()`.
    pub fn highlight_range(
        &self,
        tree: &SyntaxTree,
        snapshot: &BufferSnapshot,
        byte_range: Range<usize>,
    ) -> Result<Vec<HighlightSpan>, SyntaxError> {
        let total_bytes = snapshot.len_bytes();
        if byte_range.start > byte_range.end || byte_range.end > total_bytes {
            return Err(SyntaxError::InvalidByteRange {
                start: byte_range.start,
                end: byte_range.end,
                len: total_bytes,
            });
        }

        if byte_range.is_empty() {
            return Ok(Vec::new());
        }

        let Some(entry) = self.registry.get(tree.language()) else {
            return Ok(Vec::new());
        };
        let Some(highlight_query) = entry.highlight_query() else {
            return Ok(Vec::new());
        };

        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(byte_range.clone());
        let provider = RopeTextProvider::from_snapshot(snapshot);
        let mut matches = cursor.matches(highlight_query.query(), tree.root_node(), provider);

        let mut candidates = Vec::new();
        while let Some(mat) = matches.next() {
            for cap in mat.captures {
                let Some(kind) = highlight_query.capture_kind(cap.index) else {
                    continue;
                };
                let node_range = cap.node.byte_range();
                let start = node_range.start.max(byte_range.start);
                let end = node_range.end.min(byte_range.end);
                if start >= end {
                    continue;
                }
                candidates.push(CandidateSpan {
                    start,
                    end,
                    orig_len: node_range.end.saturating_sub(node_range.start),
                    pattern_index: mat.pattern_index,
                    capture_index: cap.index,
                    kind,
                });
            }
        }

        Ok(normalize_spans(candidates))
    }

    /// Extracts sorted, strictly disjoint [`HighlightSpan`]s covering the 0-based half-open
    /// line interval `line_range` (`start_line..end_line`), safely clamping bounds to
    /// `snapshot.len_lines()`.
    ///
    /// # Errors
    ///
    /// Propagates any [`SyntaxError`] from [`Self::highlight_range`].
    pub fn highlight_lines(
        &self,
        tree: &SyntaxTree,
        snapshot: &BufferSnapshot,
        line_range: Range<usize>,
    ) -> Result<Vec<HighlightSpan>, SyntaxError> {
        let total_lines = snapshot.len_lines();
        let start_line = line_range.start.min(total_lines);
        let end_line = line_range.end.min(total_lines);
        if start_line >= end_line {
            return Ok(Vec::new());
        }

        let start_byte = snapshot.rope().line_to_byte(start_line);
        let end_byte = if end_line >= total_lines {
            snapshot.len_bytes()
        } else {
            snapshot.rope().line_to_byte(end_line)
        };

        self.highlight_range(tree, snapshot, start_byte..end_byte)
    }
}

#[derive(Debug, Clone, Copy)]
struct CandidateSpan {
    start: usize,
    end: usize,
    orig_len: usize,
    pattern_index: usize,
    capture_index: u32,
    kind: SyntaxTokenKind,
}

/// Resolves overlapping AST captures into sorted, disjoint, coalesced [`HighlightSpan`]s.
///
/// Priority rules:
/// 1. Narrower AST span (`orig_len` ascending) beats wider container spans.
/// 2. For equal AST length, later query pattern (`pattern_index` descending) and later capture
///    (`capture_index` descending) win, matching standard Tree-sitter highlight precedence.
fn normalize_spans(mut candidates: Vec<CandidateSpan>) -> Vec<HighlightSpan> {
    if candidates.is_empty() {
        return Vec::new();
    }

    candidates.sort_unstable_by_key(|c| {
        (
            c.orig_len,
            Reverse(c.pattern_index),
            Reverse(c.capture_index),
            c.start,
            c.end,
        )
    });

    // Maps `start -> (end, kind)` for non-overlapping intervals claimed so far.
    let mut claimed: BTreeMap<usize, (usize, SyntaxTokenKind)> = BTreeMap::new();
    let mut gaps: Vec<(usize, usize)> = Vec::new();

    for candidate in candidates {
        let mut pos = candidate.start;
        if let Some((_, &(prev_end, _))) = claimed.range(..=pos).next_back()
            && prev_end > pos
        {
            pos = prev_end;
        }
        if pos >= candidate.end {
            continue;
        }

        gaps.clear();
        for (&next_start, &(next_end, _)) in claimed.range(pos..candidate.end) {
            if pos < next_start {
                gaps.push((pos, next_start));
            }
            pos = pos.max(next_end);
            if pos >= candidate.end {
                break;
            }
        }
        if pos < candidate.end {
            gaps.push((pos, candidate.end));
        }

        for &(gap_start, gap_end) in &gaps {
            claimed.insert(gap_start, (gap_end, candidate.kind));
        }
    }

    let mut result: Vec<HighlightSpan> = Vec::with_capacity(claimed.len());
    for (start, (end, kind)) in claimed {
        if let Some(last) = result.last_mut()
            && last.range.end == start
            && last.kind == kind
        {
            last.range.end = end;
        } else {
            result.push(HighlightSpan::new(start..end, kind));
        }
    }
    result
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "unit tests assert highlight span normalization and range validation"
)]
mod tests {
    use kova_buffer::Buffer;
    use kova_theme::SyntaxLanguage;

    use super::*;
    use crate::{SyntaxEngine, SyntaxEngineConfig};

    #[test]
    fn normalize_splits_outer_container_around_inner_tokens_and_merges_adjacent() {
        let candidates = vec![
            // Outer string `"hello\n\tworld"` at 0..14
            CandidateSpan {
                start: 0,
                end: 14,
                orig_len: 14,
                pattern_index: 0,
                capture_index: 0,
                kind: SyntaxTokenKind::String,
            },
            // Escape `\n` at 6..8
            CandidateSpan {
                start: 6,
                end: 8,
                orig_len: 2,
                pattern_index: 1,
                capture_index: 0,
                kind: SyntaxTokenKind::StringEscape,
            },
            // Escape `\t` at 8..10 (adjacent same kind -> coalesced with `\n`)
            CandidateSpan {
                start: 8,
                end: 10,
                orig_len: 2,
                pattern_index: 1,
                capture_index: 0,
                kind: SyntaxTokenKind::StringEscape,
            },
        ];

        let spans = normalize_spans(candidates);
        assert_eq!(
            spans,
            vec![
                HighlightSpan::new(0..6, SyntaxTokenKind::String),
                HighlightSpan::new(6..10, SyntaxTokenKind::StringEscape),
                HighlightSpan::new(10..14, SyntaxTokenKind::String),
            ]
        );
    }

    #[test]
    fn normalize_prefers_later_pattern_when_range_is_identical() {
        let candidates = vec![
            CandidateSpan {
                start: 0,
                end: 9,
                orig_len: 9,
                pattern_index: 0,
                capture_index: 0,
                kind: SyntaxTokenKind::Variable,
            },
            CandidateSpan {
                start: 0,
                end: 9,
                orig_len: 9,
                pattern_index: 12,
                capture_index: 0,
                kind: SyntaxTokenKind::Constant,
            },
        ];

        let spans = normalize_spans(candidates);
        assert_eq!(
            spans,
            vec![HighlightSpan::new(0..9, SyntaxTokenKind::Constant)]
        );
    }

    #[test]
    fn highlight_range_validates_bounds_and_handles_empty_slices() {
        let highlighter = Highlighter::from_builtin().unwrap();
        let engine = SyntaxEngine::new(
            highlighter.registry().clone(),
            SyntaxEngineConfig::default(),
        )
        .unwrap();

        let buffer = Buffer::from_text("{\"a\": 1}\n");
        let snap = buffer.snapshot();
        let tree = engine
            .parse_sync(SyntaxLanguage::Json, &snap, None)
            .unwrap();

        assert_eq!(
            highlighter.highlight_range(&tree, &snap, 0..0).unwrap(),
            Vec::new()
        );
        assert_eq!(
            highlighter.highlight_lines(&tree, &snap, 5..5).unwrap(),
            Vec::new()
        );

        let err = highlighter
            .highlight_range(&tree, &snap, 0..999)
            .unwrap_err();
        assert!(matches!(err, SyntaxError::InvalidByteRange { .. }));

        #[allow(
            clippy::reversed_empty_ranges,
            reason = "explicitly testing inverted byte range validation"
        )]
        let inverted = highlighter.highlight_range(&tree, &snap, 4..2).unwrap_err();
        assert!(matches!(inverted, SyntaxError::InvalidByteRange { .. }));
    }
}
