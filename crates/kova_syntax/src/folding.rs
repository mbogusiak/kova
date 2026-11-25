//! Code folding region extraction (`FoldKind`, `FoldRange`, `extract_folds`).

use std::cmp::Reverse;
use std::ops::Range;

use kova_buffer::BufferSnapshot;
use kova_theme::SyntaxLanguage;
use tree_sitter::{QueryCursor, StreamingIterator};

use crate::input::{RopeTextProvider, read_byte_range_text, trimmed_end_byte_and_line};
use crate::registry::{FoldQuery, LanguageRegistry};
use crate::tree::SyntaxTree;

/// Semantic category of a foldable source code region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FoldKind {
    /// Grouped consecutive `import` declarations (Java / Kotlin).
    Imports,
    /// Multi-line block comment, Javadoc, or `KDoc`.
    Comment,
    /// Explicit user region (`// region ... // endregion` or `#region ... #endregion`).
    Region,
    /// Type declaration body (`class`, `interface`, `record`, `enum`, `annotation`, `object`).
    Type,
    /// Function, method, constructor, or initializer body.
    Function,
    /// Control-flow block, lambda, switch/when block, array, JSON/TOML container, or TOML section.
    Block,
    /// Single HTTP request block inside a `.http` file.
    Request,
}

impl FoldKind {
    /// Returns the deduplication priority when two AST nodes span the exact same `(start_line, end_line)`.
    #[must_use]
    const fn priority(self) -> u8 {
        match self {
            Self::Type => 7,
            Self::Function => 6,
            Self::Request => 5,
            Self::Imports => 4,
            Self::Comment => 3,
            Self::Region => 2,
            Self::Block => 1,
        }
    }
}

/// A single multi-line foldable region in a buffer snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FoldRange {
    /// Zero-based line index displaying the fold chevron in the editor gutter.
    pub start_line: usize,
    /// Zero-based inclusive end line index of the folded block (`end_line > start_line`).
    pub end_line: usize,
    /// UTF-8 byte range hidden when the fold is collapsed.
    pub collapsed_byte_range: Range<usize>,
    /// Semantic kind of the foldable region.
    pub kind: FoldKind,
    /// Whether this region starts collapsed by default (for example grouped imports).
    pub default_collapsed: bool,
    /// Placeholder text rendered inline when collapsed (for example `"{...}"` or `"/** ... */"`).
    pub placeholder: &'static str,
}

/// Extracts deterministically sorted, deduplicated [`FoldRange`]s from `tree` and `snapshot`
/// using the built-in [`LanguageRegistry`].
///
/// Guarantees:
/// - No single-line ranges (`start_line < end_line` for every entry).
/// - No duplicate `(start_line, end_line)` pairs (highest-priority [`FoldKind`] wins).
/// - Sorted by `(start_line, Reverse(end_line))`.
/// - All byte offsets lie inside `0..=snapshot.len_bytes()` on valid UTF-8 boundaries.
#[must_use]
pub fn extract_folds(tree: &SyntaxTree, snapshot: &BufferSnapshot) -> Vec<FoldRange> {
    match LanguageRegistry::builtin() {
        Ok(registry) => extract_folds_with_registry(tree, snapshot, registry),
        Err(_) => Vec::new(),
    }
}

/// Extracts [`FoldRange`]s using a caller-provided [`LanguageRegistry`].
#[must_use]
pub fn extract_folds_with_registry(
    tree: &SyntaxTree,
    snapshot: &BufferSnapshot,
    registry: &LanguageRegistry,
) -> Vec<FoldRange> {
    if snapshot.is_empty() || snapshot.len_lines() < 2 {
        return Vec::new();
    }

    let mut folds = Vec::new();

    if let Some(fold_query) = registry
        .get(tree.language())
        .and_then(|entry| entry.fold_query())
    {
        collect_query_folds(tree, snapshot, fold_query, &mut folds);
    }

    collect_import_folds(tree, snapshot, &mut folds);
    collect_region_folds(tree, snapshot, &mut folds);

    dedup_and_sort_folds(&mut folds);
    folds
}

fn collect_query_folds(
    tree: &SyntaxTree,
    snapshot: &BufferSnapshot,
    fold_query: &FoldQuery,
    out: &mut Vec<FoldRange>,
) {
    let total_bytes = snapshot.len_bytes();
    let rope = snapshot.rope();
    let language = tree.language();

    let mut cursor = QueryCursor::new();
    let provider = RopeTextProvider::from_snapshot(snapshot);
    let mut matches = cursor.matches(fold_query.query(), tree.root_node(), provider);

    while let Some(mat) = matches.next() {
        for cap in mat.captures {
            let Some(kind) = fold_query.capture_kind(cap.index) else {
                continue;
            };
            let node = cap.node;
            let start_byte = node.start_byte().min(total_bytes);
            let raw_end_byte = node.end_byte().min(total_bytes);
            if start_byte >= raw_end_byte {
                continue;
            }

            let Some((end_byte, end_line)) =
                trimmed_end_byte_and_line(start_byte..raw_end_byte, snapshot)
            else {
                continue;
            };
            let start_line = rope.byte_to_line(start_byte);
            if start_line >= end_line {
                continue;
            }

            let Some((collapsed_byte_range, placeholder)) = compute_collapsed_range(
                language, kind, node, start_byte, end_byte, start_line, snapshot,
            ) else {
                continue;
            };

            out.push(FoldRange {
                start_line,
                end_line,
                collapsed_byte_range,
                kind,
                default_collapsed: kind == FoldKind::Imports,
                placeholder,
            });
        }
    }
}

fn compute_collapsed_range(
    language: SyntaxLanguage,
    kind: FoldKind,
    node: tree_sitter::Node<'_>,
    start_byte: usize,
    end_byte: usize,
    start_line: usize,
    snapshot: &BufferSnapshot,
) -> Option<(Range<usize>, &'static str)> {
    let rope = snapshot.rope();
    match kind {
        FoldKind::Comment => {
            let prefix_end = (start_byte + 3).min(end_byte);
            let prefix = read_byte_range_text(start_byte..prefix_end, snapshot);
            let placeholder = if prefix == "/**" {
                "/** ... */"
            } else {
                "/* ... */"
            };
            (start_byte < end_byte).then_some((start_byte..end_byte, placeholder))
        }
        FoldKind::Type => {
            let placeholder = if rope.get_byte(start_byte) == Some(b'{') {
                "{...}"
            } else {
                "..."
            };
            (start_byte < end_byte).then_some((start_byte..end_byte, placeholder))
        }
        FoldKind::Function => {
            if let Some(body_start) = find_brace_body_start(node, snapshot)
                && body_start < end_byte
            {
                return Some((body_start..end_byte, "{...}"));
            }
            let collapse_start = line_content_end_byte(start_line, snapshot).max(start_byte);
            (collapse_start < end_byte).then_some((collapse_start..end_byte, "..."))
        }
        FoldKind::Block => {
            let node_kind = node.kind();
            if language == SyntaxLanguage::Toml
                && (node_kind == "table" || node_kind == "table_array_element")
            {
                let collapse_start = line_content_end_byte(start_line, snapshot).max(start_byte);
                return (collapse_start < end_byte).then_some((collapse_start..end_byte, "..."));
            }

            match rope.get_byte(start_byte) {
                Some(b'{') => (start_byte < end_byte).then_some((start_byte..end_byte, "{...}")),
                Some(b'[') => (start_byte < end_byte).then_some((start_byte..end_byte, "[...]")),
                _ => {
                    if let Some(brace_start) = find_brace_body_start(node, snapshot)
                        && brace_start < end_byte
                    {
                        return Some((brace_start..end_byte, "{...}"));
                    }
                    let collapse_start =
                        line_content_end_byte(start_line, snapshot).max(start_byte);
                    (collapse_start < end_byte).then_some((collapse_start..end_byte, "..."))
                }
            }
        }
        FoldKind::Request | FoldKind::Region => {
            let collapse_start = line_content_end_byte(start_line, snapshot).max(start_byte);
            (collapse_start < end_byte).then_some((collapse_start..end_byte, "..."))
        }
        FoldKind::Imports => {
            (start_byte < end_byte).then_some((start_byte..end_byte, "import ..."))
        }
    }
}

fn find_brace_body_start(node: tree_sitter::Node<'_>, snapshot: &BufferSnapshot) -> Option<usize> {
    let rope = snapshot.rope();
    let total_bytes = snapshot.len_bytes();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let child_start = child.start_byte().min(total_bytes);
        if rope.get_byte(child_start) == Some(b'{') {
            return Some(child_start);
        }
        if matches!(
            child.kind(),
            "block" | "constructor_body" | "function_body" | "class_body" | "switch_block"
        ) {
            let mut inner = child.walk();
            for grandchild in child.children(&mut inner) {
                let gc_start = grandchild.start_byte().min(total_bytes);
                if rope.get_byte(gc_start) == Some(b'{') {
                    return Some(gc_start);
                }
            }
        }
    }
    None
}

fn line_content_end_byte(line: usize, snapshot: &BufferSnapshot) -> usize {
    let rope = snapshot.rope();
    if line >= rope.len_lines() {
        return snapshot.len_bytes();
    }
    let line_start = rope.line_to_byte(line);
    let content_len = snapshot.line_len_bytes(line).unwrap_or(0);
    (line_start + content_len).min(snapshot.len_bytes())
}

fn collect_import_folds(tree: &SyntaxTree, snapshot: &BufferSnapshot, out: &mut Vec<FoldRange>) {
    let import_kind = match tree.language() {
        SyntaxLanguage::Java => "import_declaration",
        SyntaxLanguage::Kotlin => "import",
        _ => return,
    };

    let root = tree.root_node();
    let mut cursor = root.walk();
    let total_bytes = snapshot.len_bytes();
    let rope = snapshot.rope();

    let mut run_start: Option<(usize, usize)> = None;
    let mut run_end: Option<(usize, usize)> = None;
    let mut import_count: usize = 0;

    let flush = |start: &mut Option<(usize, usize)>,
                 end: &mut Option<(usize, usize)>,
                 count: &mut usize,
                 out: &mut Vec<FoldRange>| {
        if let (Some((start_byte, start_line)), Some((end_byte, end_line))) = (*start, *end)
            && *count >= 2
            && end_line > start_line
            && start_byte < end_byte
        {
            out.push(FoldRange {
                start_line,
                end_line,
                collapsed_byte_range: start_byte..end_byte,
                kind: FoldKind::Imports,
                default_collapsed: true,
                placeholder: "import ...",
            });
        }
        *start = None;
        *end = None;
        *count = 0;
    };

    for child in root.children(&mut cursor) {
        let kind = child.kind();
        if kind == import_kind {
            let start_byte = child.start_byte().min(total_bytes);
            let raw_end = child.end_byte().min(total_bytes);
            if let Some((end_byte, end_line)) =
                trimmed_end_byte_and_line(start_byte..raw_end, snapshot)
            {
                let start_line = rope.byte_to_line(start_byte);
                if run_start.is_none() {
                    run_start = Some((start_byte, start_line));
                }
                run_end = Some((end_byte, end_line));
                import_count += 1;
            }
        } else if run_start.is_some()
            && kind == "line_comment"
            && parse_region_marker(&read_byte_range_text(child.byte_range(), snapshot)).is_none()
        {
            // Non-region line comments between import statements do not split an import group,
            // nor do they extend `run_end` past the last actual import node.
        } else {
            flush(&mut run_start, &mut run_end, &mut import_count, out);
        }
    }

    flush(&mut run_start, &mut run_end, &mut import_count, out);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegionMarker {
    Start,
    End,
}

fn parse_region_marker(comment_text: &str) -> Option<RegionMarker> {
    let trimmed = comment_text.trim();
    let body = if let Some(rest) = trimmed.strip_prefix("//") {
        rest.trim_start()
    } else if let Some(rest) = trimmed.strip_prefix('#') {
        rest.trim_start()
    } else {
        let rest = trimmed
            .strip_prefix("/*")
            .and_then(|s| s.strip_suffix("*/"))?;
        if rest.contains('\n') {
            return None;
        }
        rest.trim()
    };

    let lower = body.to_ascii_lowercase();
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

fn collect_region_folds(tree: &SyntaxTree, snapshot: &BufferSnapshot, out: &mut Vec<FoldRange>) {
    let total_bytes = snapshot.len_bytes();
    let rope = snapshot.rope();
    let mut stack: Vec<(usize, usize)> = Vec::new();

    // Non-recursive TreeCursor DFS over the entire AST so deeply nested files never overflow.
    let mut cursor = tree.root_node().walk();
    loop {
        let node = cursor.node();
        let kind = node.kind();
        if matches!(kind, "line_comment" | "block_comment" | "comment") {
            let start_byte = node.start_byte().min(total_bytes);
            let raw_end = node.end_byte().min(total_bytes);
            if let Some((end_byte, end_line)) =
                trimmed_end_byte_and_line(start_byte..raw_end, snapshot)
            {
                let text = read_byte_range_text(start_byte..end_byte, snapshot);
                match parse_region_marker(&text) {
                    Some(RegionMarker::Start) => {
                        let start_line = rope.byte_to_line(start_byte);
                        let line_end = line_content_end_byte(start_line, snapshot).max(end_byte);
                        stack.push((start_line, line_end));
                    }
                    Some(RegionMarker::End) => {
                        if let Some((start_line, open_end_byte)) = stack.pop()
                            && end_line > start_line
                            && open_end_byte < end_byte
                        {
                            out.push(FoldRange {
                                start_line,
                                end_line,
                                collapsed_byte_range: open_end_byte..end_byte,
                                kind: FoldKind::Region,
                                default_collapsed: false,
                                placeholder: "...",
                            });
                        }
                    }
                    None => {}
                }
            }
        }

        if cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return;
            }
        }
    }
}

fn dedup_and_sort_folds(folds: &mut Vec<FoldRange>) {
    folds.sort_unstable_by_key(|f| {
        (
            f.start_line,
            Reverse(f.end_line),
            Reverse(f.kind.priority()),
        )
    });
    folds.dedup_by(|later, earlier| {
        earlier.start_line == later.start_line && earlier.end_line == later.end_line
    });
}
