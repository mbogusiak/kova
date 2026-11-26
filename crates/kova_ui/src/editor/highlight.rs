//! Syntax highlight pipeline: splits one visible editor line into styled segments
//! (`segment_line_styles`) and maps them onto GPUI [`TextRun`]s (`build_line_text_runs`).
//!
//! Highlight spans may come from an older [`kova_syntax`] snapshot than the buffer being
//! painted (*stale syntax fallback* while the background parse catches up). Every offset is
//! therefore clamped to the current line and snapped to a UTF-8 character boundary, so a stale
//! span can only produce slightly wrong colours, never a panic or a split character.

use std::ops::Range;

use gpui::{Font, FontStyle, FontWeight, TextRun, UnderlineStyle, px};
use kova_syntax::HighlightSpan;
use kova_theme::{RgbaColor, SyntaxStyle, SyntaxTokens};

use crate::theme::hsla_from_rgba;

/// One contiguous, uniformly styled byte range of a visible line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyledSegment {
    /// Line-local UTF-8 byte range (always on character boundaries, never empty).
    pub byte_range: Range<usize>,
    /// Resolved style of the segment (`default_fg` without decorations when unhighlighted).
    pub style: SyntaxStyle,
}

/// Returns the largest character boundary of `text` that is `<= byte` (MSRV-safe
/// replacement for `str::floor_char_boundary`).
#[must_use]
pub fn floor_char_boundary(text: &str, byte: usize) -> usize {
    let mut idx = byte.min(text.len());
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// Splits the `visible_byte_window` of `line_str` into styled segments.
///
/// * `line_start_byte` is the absolute buffer offset of the line start in the snapshot the
///   `spans` were computed from (so stale spans stay aligned to their line).
/// * `visible_byte_window` is the line-local window produced by
///   [`ViewportMetrics::visible_utf8_window`](crate::editor::viewport::ViewportMetrics::visible_utf8_window).
/// * `spans` are absolute, ideally sorted and disjoint; unsorted, overlapping or out-of-range
///   spans are tolerated (overlaps are resolved first-come, out-of-range parts are dropped).
///
/// The returned segments tile the (boundary-snapped) window exactly: they are contiguous,
/// non-empty, ordered and their lengths sum to the window length, which is what
/// `shape_line` requires of its runs.
#[must_use]
pub fn segment_line_styles(
    line_str: &str,
    line_start_byte: usize,
    visible_byte_window: Range<usize>,
    spans: &[HighlightSpan],
    tokens: &SyntaxTokens,
    default_fg: RgbaColor,
) -> Vec<StyledSegment> {
    let win_start = floor_char_boundary(line_str, visible_byte_window.start);
    let win_end = floor_char_boundary(line_str, visible_byte_window.end).max(win_start);
    let default_style = SyntaxStyle::new(default_fg);
    let mut segments = Vec::new();
    if win_start == win_end {
        return segments;
    }

    let abs_start = line_start_byte.saturating_add(win_start);
    let abs_end = line_start_byte.saturating_add(win_end);
    // Spans are normally sorted: skip everything that ends before the window in O(log n).
    let first = spans.partition_point(|span| span.range.end <= abs_start);

    let mut cursor = win_start;
    for span in &spans[first..] {
        if span.range.start >= abs_end {
            if span.range.start >= span.range.end {
                continue;
            }
            // Sorted input: nothing further can intersect the window.
            break;
        }
        let local_start = span.range.start.saturating_sub(line_start_byte);
        let local_end = span.range.end.saturating_sub(line_start_byte).min(win_end);
        let start = floor_char_boundary(line_str, local_start.max(cursor));
        let end = floor_char_boundary(line_str, local_end);
        if start < cursor || end <= start {
            continue;
        }
        if start > cursor {
            segments.push(StyledSegment {
                byte_range: cursor..start,
                style: default_style,
            });
        }
        let style = *tokens.style(span.kind);
        push_merged(&mut segments, start..end, style);
        cursor = end;
        if cursor >= win_end {
            break;
        }
    }
    if cursor < win_end {
        push_merged(&mut segments, cursor..win_end, default_style);
    }
    segments
}

/// Appends `range` with `style`, extending the previous segment when both share a style.
fn push_merged(segments: &mut Vec<StyledSegment>, range: Range<usize>, style: SyntaxStyle) {
    if let Some(last) = segments.last_mut()
        && last.style == style
        && last.byte_range.end == range.start
    {
        last.byte_range.end = range.end;
        return;
    }
    segments.push(StyledSegment {
        byte_range: range,
        style,
    });
}

/// Maps styled segments onto GPUI [`TextRun`]s (bold / italic / underline honoured).
///
/// Run lengths sum to the total length of `segments`, so the result can be passed directly to
/// `shape_line` for the text slice the segments were computed over.
#[must_use]
pub fn build_line_text_runs(segments: &[StyledSegment], base_font: &Font) -> Vec<TextRun> {
    segments
        .iter()
        .map(|segment| {
            let style = segment.style;
            let color = hsla_from_rgba(style.fg);
            let mut font = base_font.clone();
            if style.bold {
                font.weight = FontWeight::BOLD;
            }
            if style.italic {
                font.style = FontStyle::Italic;
            }
            TextRun {
                len: segment.byte_range.len(),
                font,
                color,
                background_color: None,
                underline: style.underline.then_some(UnderlineStyle {
                    thickness: px(1.0),
                    color: Some(color),
                    wavy: false,
                }),
                strikethrough: None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use kova_theme::{AetherTheme, BuiltinTheme, SyntaxTokenKind};

    use super::*;

    fn tokens() -> SyntaxTokens {
        AetherTheme::builtin(BuiltinTheme::KovaIslandsDark)
            .tokens
            .syntax
    }

    fn default_fg() -> RgbaColor {
        AetherTheme::builtin(BuiltinTheme::KovaIslandsDark)
            .tokens
            .editor
            .foreground
    }

    fn span(range: Range<usize>, kind: SyntaxTokenKind) -> HighlightSpan {
        HighlightSpan::new(range, kind)
    }

    /// Asserts the tiling invariant required by `shape_line`.
    fn assert_tiles(line: &str, window: Range<usize>, segments: &[StyledSegment]) {
        let start = floor_char_boundary(line, window.start);
        let end = floor_char_boundary(line, window.end).max(start);
        let mut cursor = start;
        for segment in segments {
            assert_eq!(
                segment.byte_range.start, cursor,
                "segments must be contiguous"
            );
            assert!(!segment.byte_range.is_empty(), "segments must be non-empty");
            assert!(line.is_char_boundary(segment.byte_range.start));
            assert!(line.is_char_boundary(segment.byte_range.end));
            cursor = segment.byte_range.end;
        }
        assert_eq!(cursor, end, "segments must cover the whole window");
    }

    fn kinds(line: &str, segments: &[StyledSegment], tokens: &SyntaxTokens) -> Vec<String> {
        segments
            .iter()
            .map(|s| {
                let text = &line[s.byte_range.clone()];
                let fg = if s.style == *tokens.style(SyntaxTokenKind::Keyword) {
                    "kw"
                } else if s.style == *tokens.style(SyntaxTokenKind::String) {
                    "str"
                } else if s.style == *tokens.style(SyntaxTokenKind::Annotation) {
                    "ann"
                } else if s.style == *tokens.style(SyntaxTokenKind::Comment) {
                    "com"
                } else {
                    "plain"
                };
                format!("{fg}:{text}")
            })
            .collect()
    }

    #[test]
    fn java_line_with_multiple_tokens_is_tiled_and_styled() {
        let tokens = tokens();
        let line = "@Service public final class A { String s = \"x\"; } // c";
        let base = 100;
        let at = |needle: &str| line.find(needle).unwrap();
        let spans = vec![
            span(base..base + 8, SyntaxTokenKind::Annotation),
            span(
                base + at("public")..base + at("public") + 6,
                SyntaxTokenKind::Keyword,
            ),
            span(
                base + at("final")..base + at("final") + 5,
                SyntaxTokenKind::Keyword,
            ),
            span(
                base + at("class")..base + at("class") + 5,
                SyntaxTokenKind::Keyword,
            ),
            span(
                base + at("\"x\"")..base + at("\"x\"") + 3,
                SyntaxTokenKind::String,
            ),
            span(
                base + at("// c")..base + line.len(),
                SyntaxTokenKind::Comment,
            ),
        ];
        let segments =
            segment_line_styles(line, base, 0..line.len(), &spans, &tokens, default_fg());
        assert_tiles(line, 0..line.len(), &segments);
        let described = kinds(line, &segments, &tokens);
        assert_eq!(described[0], "ann:@Service");
        assert!(described.contains(&"kw:public".to_owned()));
        assert!(described.contains(&"str:\"x\"".to_owned()));
        assert_eq!(described.last().unwrap(), "com:// c");
        assert!(segments.last().unwrap().style.italic, "comments are italic");
    }

    #[test]
    fn horizontally_scrolled_window_clips_spans() {
        let tokens = tokens();
        let line = "public static void main(String[] args) {";
        let spans = vec![
            span(0..6, SyntaxTokenKind::Keyword),
            span(7..13, SyntaxTokenKind::Keyword),
            span(14..18, SyntaxTokenKind::Keyword),
        ];
        // Window starts mid-way through `static`.
        let segments = segment_line_styles(line, 0, 9..20, &spans, &tokens, default_fg());
        assert_tiles(line, 9..20, &segments);
        assert_eq!(segments[0].byte_range, 9..13);
        assert_eq!(segments[0].style, *tokens.style(SyntaxTokenKind::Keyword));
        assert_eq!(segments.last().unwrap().byte_range.end, 20);
    }

    #[test]
    fn stale_offsets_with_polish_letters_and_emoji_never_split_characters() {
        let tokens = tokens();
        // "zażółć" has 2-byte letters, the emoji is 4 bytes.
        let line = "String s = \"zażółć 🚀 gęślą\";";
        // Spans from an older revision whose boundaries land mid-character and past the end.
        let spans = vec![
            span(3..12, SyntaxTokenKind::Keyword),
            span(14..15, SyntaxTokenKind::String), // inside "ż" (bytes 14..16)
            span(22..26, SyntaxTokenKind::String), // inside the emoji
            span(30..500, SyntaxTokenKind::Comment),
            span(900..950, SyntaxTokenKind::Keyword),
        ];
        let reversed = Range { start: 40, end: 10 };
        for window in [0..line.len(), 13..line.len(), 15..23, 0..1000, reversed] {
            let segments =
                segment_line_styles(line, 0, window.clone(), &spans, &tokens, default_fg());
            assert_tiles(line, window.clone(), &segments);
            let total: usize = segments.iter().map(|s| s.byte_range.len()).sum();
            let start = floor_char_boundary(line, window.start);
            let end = floor_char_boundary(line, window.end).max(start);
            assert_eq!(total, end - start);
        }
    }

    #[test]
    fn unsorted_overlapping_and_empty_spans_do_not_panic() {
        let tokens = tokens();
        let line = "fun main() = println(\"ok\")";
        let spans = vec![
            span(4..8, SyntaxTokenKind::Function),
            span(0..3, SyntaxTokenKind::Keyword),
            span(2..6, SyntaxTokenKind::String),
            span(10..10, SyntaxTokenKind::Keyword),
            span(Range { start: 12, end: 9 }, SyntaxTokenKind::Keyword),
        ];
        let segments = segment_line_styles(line, 0, 0..line.len(), &spans, &tokens, default_fg());
        assert_tiles(line, 0..line.len(), &segments);
    }

    #[test]
    fn line_start_offset_far_past_spans_yields_default_style() {
        let tokens = tokens();
        let line = "plain text";
        let spans = vec![span(0..5, SyntaxTokenKind::Keyword)];
        let segments =
            segment_line_styles(line, 10_000, 0..line.len(), &spans, &tokens, default_fg());
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].style, SyntaxStyle::new(default_fg()));
        assert_eq!(
            segment_line_styles("", 0, 0..0, &spans, &tokens, default_fg()),
            Vec::new()
        );
    }

    #[test]
    fn bold_italic_and_underline_map_onto_text_runs() {
        let tokens = tokens();
        let line = "POST https://x/api";
        let spans = vec![span(0..4, SyntaxTokenKind::HttpMethod)];
        let segments = segment_line_styles(line, 0, 0..line.len(), &spans, &tokens, default_fg());
        let mut styled = segments.clone();
        styled[1].style.italic = true;
        styled[1].style.underline = true;
        let base = Font {
            family: "JetBrains Mono".into(),
            features: gpui::FontFeatures::default(),
            fallbacks: None,
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
        };
        let runs = build_line_text_runs(&styled, &base);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs.iter().map(|r| r.len).sum::<usize>(), line.len());
        assert_eq!(
            runs[0].font.weight,
            if tokens.style(SyntaxTokenKind::HttpMethod).bold {
                FontWeight::BOLD
            } else {
                FontWeight::NORMAL
            }
        );
        assert_eq!(runs[1].font.style, FontStyle::Italic);
        assert!(runs[1].underline.is_some());
    }

    #[test]
    fn floor_char_boundary_snaps_down() {
        let text = "aż🚀";
        assert_eq!(floor_char_boundary(text, 0), 0);
        assert_eq!(floor_char_boundary(text, 2), 1);
        assert_eq!(floor_char_boundary(text, 5), 3);
        assert_eq!(floor_char_boundary(text, 99), text.len());
    }
}
