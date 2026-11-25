//! Zero-copy `ropey::Rope` input adapters and coordinate conversions for Tree-sitter.

use std::ops::ControlFlow;

use kova_buffer::{BufferSnapshot, Point};
use ropey::Rope;
use ropey::iter::Chunks;

/// Converts a [`kova_buffer::Point`] into a [`tree_sitter::Point`].
#[must_use]
pub const fn to_ts_point(point: Point) -> tree_sitter::Point {
    tree_sitter::Point::new(point.row, point.column)
}

/// Converts a [`tree_sitter::Point`] into a [`kova_buffer::Point`].
#[must_use]
pub const fn from_ts_point(point: tree_sitter::Point) -> Point {
    Point::new(point.row, point.column)
}

/// Parses `rope` directly from its internal B-tree chunks without allocating a contiguous `String`.
///
/// The `progress` callback receives parser progress states and may return
/// [`ControlFlow::Break`] to abort parsing immediately.
pub fn parse_rope_with(
    parser: &mut tree_sitter::Parser,
    rope: &Rope,
    old_tree: Option<&tree_sitter::Tree>,
    progress: &mut dyn FnMut(&tree_sitter::ParseState) -> ControlFlow<()>,
) -> Option<tree_sitter::Tree> {
    let total_bytes = rope.len_bytes();
    let mut input_callback = |byte_offset: usize, _point: tree_sitter::Point| -> &[u8] {
        if byte_offset >= total_bytes {
            return &[];
        }
        let (mut chunks, chunk_byte_start, _, _) = rope.chunks_at_byte(byte_offset);
        let Some(chunk) = chunks.next() else {
            return &[];
        };
        let rel_offset = byte_offset.saturating_sub(chunk_byte_start);
        chunk.as_bytes().get(rel_offset..).unwrap_or(&[])
    };

    let mut ts_progress = |state: &tree_sitter::ParseState| progress(state);
    let options = tree_sitter::ParseOptions::new().progress_callback(&mut ts_progress);
    parser.parse_with_options(&mut input_callback, old_tree, Some(options))
}

/// Zero-copy [`tree_sitter::TextProvider`] backed by a [`ropey::Rope`].
///
/// Enables Tree-sitter `QueryCursor` predicates (`#eq?`, `#match?`, `#any-of?`) to
/// evaluate directly over `Rope` chunks without materializing the file into a `String`.
#[derive(Debug, Clone, Copy)]
pub struct RopeTextProvider<'a> {
    rope: &'a Rope,
}

impl<'a> RopeTextProvider<'a> {
    /// Creates a [`RopeTextProvider`] reading from `rope`.
    #[must_use]
    pub const fn new(rope: &'a Rope) -> Self {
        Self { rope }
    }

    /// Creates a [`RopeTextProvider`] reading from `snapshot`.
    #[must_use]
    pub fn from_snapshot(snapshot: &'a BufferSnapshot) -> Self {
        Self {
            rope: snapshot.rope(),
        }
    }
}

/// Iterator over byte slices of a [`Rope`] range yielded by [`RopeTextProvider`].
#[derive(Debug)]
pub struct RopeBytesIter<'a> {
    chunks: Chunks<'a>,
    first_offset: usize,
    remaining: usize,
}

impl<'a> RopeBytesIter<'a> {
    fn empty(rope: &'a Rope) -> Self {
        let (chunks, _, _, _) = rope.chunks_at_byte(rope.len_bytes());
        Self {
            chunks,
            first_offset: 0,
            remaining: 0,
        }
    }
}

impl<'a> Iterator for RopeBytesIter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        while self.remaining > 0 {
            let chunk = self.chunks.next()?;
            let bytes = chunk.as_bytes();
            let offset = std::mem::take(&mut self.first_offset);
            let Some(avail) = bytes.get(offset..) else {
                continue;
            };
            if avail.is_empty() {
                continue;
            }
            let take = avail.len().min(self.remaining);
            self.remaining -= take;
            return Some(&avail[..take]);
        }
        None
    }
}

impl<'a> tree_sitter::TextProvider<&'a [u8]> for RopeTextProvider<'a> {
    type I = RopeBytesIter<'a>;

    fn text(&mut self, node: tree_sitter::Node<'_>) -> Self::I {
        let len = self.rope.len_bytes();
        let range = node.byte_range();
        let start = range.start.min(len);
        let end = range.end.min(len);
        if start >= end {
            return RopeBytesIter::empty(self.rope);
        }

        let (chunks, chunk_byte_start, _, _) = self.rope.chunks_at_byte(start);
        RopeBytesIter {
            chunks,
            first_offset: start.saturating_sub(chunk_byte_start),
            remaining: end - start,
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "unit tests assert Tree-sitter parses and query matches"
)]
mod tests {
    use std::fmt::Write as _;

    use kova_buffer::Buffer;
    use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator, TextProvider};

    use super::*;

    #[test]
    fn point_roundtrip() {
        let p = Point::new(14, 37);
        assert_eq!(from_ts_point(to_ts_point(p)), p);
    }

    #[test]
    fn rope_input_matches_flat_str_across_chunks_utf8_emoji_and_crlf() {
        let mut json = String::from("{\r\n  \"entries\": [\r\n");
        // Exceed 16 KiB so `ropey` spans multiple B-tree leaf chunks, including multibyte UTF-8.
        for idx in 0..450 {
            let comma = if idx + 1 < 450 { "," } else { "" };
            let _ = writeln!(
                json,
                "    {{\"id\": {idx}, \"label\": \"zażółć_日本_🚀_{idx}\"}}{comma}\r"
            );
        }
        json.push_str("  ]\r\n}\r\n");
        assert!(json.len() > 16 * 1024);

        let buffer = Buffer::from_text(&json);
        let snapshot = buffer.snapshot();

        let ts_lang: tree_sitter::Language = tree_sitter_json::LANGUAGE.into();
        let mut flat_parser = Parser::new();
        flat_parser.set_language(&ts_lang).unwrap();
        let flat_tree = flat_parser.parse(&json, None).unwrap();

        let mut rope_parser = Parser::new();
        rope_parser.set_language(&ts_lang).unwrap();
        let rope_tree = parse_rope_with(&mut rope_parser, snapshot.rope(), None, &mut |_| {
            ControlFlow::Continue(())
        })
        .unwrap();

        assert!(!rope_tree.root_node().has_error());
        assert_eq!(
            rope_tree.root_node().to_sexp(),
            flat_tree.root_node().to_sexp()
        );
    }

    #[test]
    fn rope_text_provider_evaluates_query_predicates_across_chunks() {
        let mut json = String::from("{\n");
        for idx in 0..500 {
            let _ = writeln!(json, "  \"k_{idx}\": \"val_{idx}\",");
        }
        json.push_str("  \"target_🚀\": \"zażółć_gęślą_jaźń\"\n}");
        assert!(json.len() > 8 * 1024);

        let buffer = Buffer::from_text(&json);
        let snapshot = buffer.snapshot();

        let ts_lang: tree_sitter::Language = tree_sitter_json::LANGUAGE.into();
        let mut parser = Parser::new();
        parser.set_language(&ts_lang).unwrap();
        let tree = parse_rope_with(&mut parser, snapshot.rope(), None, &mut |_| {
            ControlFlow::Continue(())
        })
        .unwrap();

        let query = Query::new(
            &ts_lang,
            r#"((pair key: (string (string_content) @k) value: (string (string_content) @v)) (#eq? @k "target_🚀"))"#,
        )
        .unwrap();

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(
            &query,
            tree.root_node(),
            RopeTextProvider::from_snapshot(&snapshot),
        );

        let mut found_values = Vec::new();
        while let Some(m) = matches.next() {
            for cap in m.captures {
                if cap.index == 1 {
                    let bytes: Vec<u8> = RopeTextProvider::from_snapshot(&snapshot)
                        .text(cap.node)
                        .flat_map(|slice| slice.iter().copied())
                        .collect();
                    found_values.push(String::from_utf8(bytes).unwrap());
                }
            }
        }
        assert_eq!(found_values, vec!["zażółć_gęślą_jaźń".to_owned()]);
    }
}
