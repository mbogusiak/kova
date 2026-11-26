//! Deterministic single-pass YAML scanner (highlighting, folding, structure, diagnostics).

use std::ops::Range;

use kova_buffer::Point;
use kova_theme::SyntaxTokenKind;

use crate::diagnostics::SyntaxDiagnostic;
use crate::error::SyntaxError;
use crate::folding::{FoldKind, FoldRange};
use crate::scanner::{
    LineIndex, RegionMarker, ScanBudget, ScannedDocument, SpanBuilder, compact_text,
    finalize_folds, parse_region_keyword, push_diagnostic,
};
use crate::structure::{MAX_STRUCTURE_DEPTH, StructureNode, StructureSymbolKind, SymbolVisibility};

#[derive(Debug, Clone, Copy)]
struct QuotedState {
    quote: u8,
    start_byte: usize,
}

#[derive(Debug, Clone, Copy)]
struct BlockScalarState {
    base_indent: usize,
    content_indent: Option<usize>,
    header_line: usize,
    header_end_byte: usize,
    last_content_line: usize,
    last_content_end: usize,
    start_byte: usize,
}

#[derive(Debug, Clone, Copy)]
struct FlowFrame {
    delim: u8,
    start_byte: usize,
    start_line: usize,
}

#[derive(Debug, Clone)]
struct IndentFoldFrame {
    indent: usize,
    start_line: usize,
    collapse_start_byte: usize,
    last_content_line: usize,
    last_content_end: usize,
}

#[derive(Debug, Clone)]
struct StructFrame {
    indent: usize,
    name: String,
    selection_range: Range<usize>,
    selection_point: Point,
    start_byte: usize,
    start_point: Point,
    last_end_byte: usize,
    children: Vec<StructureNode>,
}

#[derive(Debug, Clone, Copy)]
struct CommentRun {
    indent: usize,
    start_line: usize,
    start_byte: usize,
    last_line: usize,
    last_end: usize,
}

struct YamlScanner<'a> {
    source: &'a str,
    bytes: &'a [u8],
    lines: LineIndex,
    spans: SpanBuilder,
    folds: Vec<FoldRange>,
    enclosing_ranges: Vec<Range<usize>>,
    diagnostics: Vec<SyntaxDiagnostic>,
    quoted: Option<QuotedState>,
    block_scalar: Option<BlockScalarState>,
    flow_stack: Vec<FlowFrame>,
    indent_folds: Vec<IndentFoldFrame>,
    struct_stack: Vec<StructFrame>,
    struct_roots: Vec<StructureNode>,
    comment_run: Option<CommentRun>,
    region_stack: Vec<(usize, usize)>,
}

/// Scans YAML `source` into a [`ScannedDocument`].
///
/// # Errors
///
/// Propagates cancellation or timeout errors from `budget`.
pub(crate) fn scan(
    source: &str,
    budget: &mut ScanBudget<'_>,
) -> Result<ScannedDocument, SyntaxError> {
    let lines = LineIndex::new(source);
    let mut scanner = YamlScanner {
        source,
        bytes: source.as_bytes(),
        lines,
        spans: SpanBuilder::with_capacity(source.len() / 8 + 16),
        folds: Vec::new(),
        enclosing_ranges: Vec::new(),
        diagnostics: Vec::new(),
        quoted: None,
        block_scalar: None,
        flow_stack: Vec::new(),
        indent_folds: Vec::new(),
        struct_stack: Vec::new(),
        struct_roots: Vec::new(),
        comment_run: None,
        region_stack: Vec::new(),
    };

    let line_count = scanner.lines.line_count();
    for line_idx in 0..line_count {
        budget.tick()?;
        scanner.scan_line(line_idx);
    }
    scanner.finish();

    let total_lines = scanner.lines.line_count();
    let total_bytes = scanner.source.len();
    finalize_folds(&mut scanner.folds, total_lines, total_bytes);

    Ok(ScannedDocument {
        source_len: total_bytes,
        spans: scanner.spans.finish(source),
        folds: scanner.folds,
        structure: scanner.struct_roots,
        enclosing_ranges: scanner.enclosing_ranges,
        diagnostics: scanner.diagnostics,
    })
}

impl YamlScanner<'_> {
    fn scan_line(&mut self, line_idx: usize) {
        let raw_range = self.lines.raw_line_range(line_idx, self.bytes.len());
        let line_start = raw_range.start;
        let line_end = self.lines.line_content_end(line_idx, self.source);
        if line_start > line_end {
            return;
        }

        // Continue an active multi-line quoted string if one is open.
        if let Some(qstate) = self.quoted {
            if let Some(pos) = self.scan_quoted_continuation(line_start, line_end, qstate) {
                self.scan_inline_tokens(line_idx, pos, line_end, false);
            }
            return;
        }

        // Measure leading indentation and flag tab characters outside block scalars/quotes.
        let (indent, first_non_ws) = self.measure_indent(line_idx, line_start, line_end);

        // Handle an active block scalar (`|` or `>`).
        if let Some(mut bs) = self.block_scalar {
            if first_non_ws >= line_end {
                return;
            }
            let stays_in_block = match bs.content_indent {
                Some(ci) => indent >= ci,
                None => indent > bs.base_indent,
            };
            if stays_in_block {
                if bs.content_indent.is_none() {
                    bs.content_indent = Some(indent);
                }
                self.spans
                    .push(first_non_ws, line_end, SyntaxTokenKind::String);
                bs.last_content_line = line_idx;
                bs.last_content_end = line_end;
                self.block_scalar = Some(bs);
                self.update_active_indent_blocks(line_idx, line_end);
                return;
            }
            self.close_block_scalar(bs);
        }

        if first_non_ws >= line_end {
            self.flush_comment_run();
            return;
        }

        if self.bytes[first_non_ws] == b'#' {
            self.spans
                .push(first_non_ws, line_end, SyntaxTokenKind::Comment);
            self.handle_comment_line(line_idx, indent, first_non_ws, line_end);
            return;
        }

        self.flush_comment_run();

        if indent == 0
            && self.flow_stack.is_empty()
            && self.try_scan_directive_or_marker(line_idx, first_non_ws, line_end)
        {
            return;
        }

        if self.flow_stack.is_empty() {
            self.close_blocks_above_or_equal(indent);
        }
        self.update_active_indent_blocks(line_idx, line_end);

        self.scan_inline_tokens(line_idx, first_non_ws, line_end, self.flow_stack.is_empty());
    }

    fn measure_indent(
        &mut self,
        line_idx: usize,
        line_start: usize,
        line_end: usize,
    ) -> (usize, usize) {
        let mut pos = line_start;
        let mut indent = 0usize;
        let mut reported_tab = false;
        while pos < line_end {
            match self.bytes[pos] {
                b' ' => {
                    indent += 1;
                    pos += 1;
                }
                b'\t' => {
                    if self.block_scalar.is_none() && !reported_tab {
                        reported_tab = true;
                        push_diagnostic(
                            &mut self.diagnostics,
                            Point::new(line_idx, pos - line_start),
                            String::from(
                                "Syntax error: tab character is not allowed in YAML indentation",
                            ),
                        );
                    }
                    indent += 1;
                    pos += 1;
                }
                _ => break,
            }
        }
        (indent, pos)
    }

    fn try_scan_directive_or_marker(
        &mut self,
        line_idx: usize,
        start: usize,
        line_end: usize,
    ) -> bool {
        let slice = &self.bytes[start..line_end];
        if (slice.starts_with(b"---") || slice.starts_with(b"..."))
            && (slice.len() == 3 || matches!(slice[3], b' ' | b'\t' | b'#'))
        {
            self.close_all_block_frames();
            self.spans.push(start, start + 3, SyntaxTokenKind::Keyword);
            let after = skip_ws(self.bytes, start + 3, line_end);
            if after < line_end {
                self.scan_inline_tokens(line_idx, after, line_end, false);
            }
            return true;
        }
        if self.bytes[start] == b'%' {
            self.close_all_block_frames();
            let mut end = start + 1;
            while end < line_end && !matches!(self.bytes[end], b' ' | b'\t') {
                end += 1;
            }
            self.spans.push(start, end, SyntaxTokenKind::Keyword);
            let after = skip_ws(self.bytes, end, line_end);
            if after < line_end {
                self.scan_value_tokens(line_idx, after, line_end);
            }
            return true;
        }
        false
    }

    fn scan_inline_tokens(
        &mut self,
        line_idx: usize,
        mut pos: usize,
        line_end: usize,
        allow_block_item: bool,
    ) {
        pos = skip_ws(self.bytes, pos, line_end);
        if pos >= line_end {
            return;
        }

        if self.bytes[pos] == b'#' {
            self.spans.push(pos, line_end, SyntaxTokenKind::Comment);
            return;
        }

        if allow_block_item
            && self.bytes[pos] == b'-'
            && (pos + 1 == line_end || matches!(self.bytes[pos + 1], b' ' | b'\t'))
        {
            let line_start = self.lines.raw_line_range(line_idx, self.bytes.len()).start;
            let dash_indent = pos - line_start;
            self.spans.push(pos, pos + 1, SyntaxTokenKind::Punctuation);
            self.indent_folds.push(IndentFoldFrame {
                indent: dash_indent,
                start_line: line_idx,
                collapse_start_byte: line_end,
                last_content_line: line_idx,
                last_content_end: line_end,
            });
            let after_dash = skip_ws(self.bytes, pos + 1, line_end);
            if after_dash < line_end {
                let item_indent = after_dash - line_start;
                self.scan_mapping_or_value(line_idx, after_dash, line_end, item_indent, true);
            }
            return;
        }

        let indent = pos - self.lines.raw_line_range(line_idx, self.bytes.len()).start;
        self.scan_mapping_or_value(
            line_idx,
            pos,
            line_end,
            indent,
            allow_block_item && self.flow_stack.is_empty(),
        );
    }

    fn scan_mapping_or_value(
        &mut self,
        line_idx: usize,
        pos: usize,
        line_end: usize,
        key_indent: usize,
        open_block_frame: bool,
    ) {
        if pos >= line_end {
            return;
        }

        if self.flow_stack.is_empty()
            && !matches!(self.bytes[pos], b'{' | b'[' | b'#' | b'|' | b'>')
            && let Some((key_start, key_end, colon_pos)) =
                find_mapping_key(self.bytes, pos, line_end)
        {
            self.emit_mapping_key(key_start, key_end, colon_pos);
            if open_block_frame {
                self.push_key_frames(line_idx, key_indent, key_start, key_end, line_end);
            }
            let val_pos = skip_ws(self.bytes, colon_pos + 1, line_end);
            if val_pos < line_end {
                self.scan_block_value(line_idx, val_pos, line_end, key_indent);
            }
            return;
        }

        if self.flow_stack.is_empty() {
            self.scan_block_value(line_idx, pos, line_end, key_indent);
        } else {
            self.scan_flow_tokens(line_idx, pos, line_end);
        }
    }

    fn emit_mapping_key(&mut self, key_start: usize, key_end: usize, colon_pos: usize) {
        let kind = if &self.bytes[key_start..key_end] == b"<<" {
            SyntaxTokenKind::Operator
        } else {
            SyntaxTokenKind::Property
        };
        self.spans.push(key_start, key_end, kind);
        self.spans
            .push(colon_pos, colon_pos + 1, SyntaxTokenKind::Punctuation);
    }

    fn push_key_frames(
        &mut self,
        line_idx: usize,
        indent: usize,
        key_start: usize,
        key_end: usize,
        line_end: usize,
    ) {
        self.indent_folds.push(IndentFoldFrame {
            indent,
            start_line: line_idx,
            collapse_start_byte: line_end,
            last_content_line: line_idx,
            last_content_end: line_end,
        });

        let raw_name = &self.source[key_start..key_end];
        let cleaned = raw_name
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .or_else(|| {
                raw_name
                    .strip_prefix('\'')
                    .and_then(|s| s.strip_suffix('\''))
            })
            .unwrap_or(raw_name);
        let name = compact_text(cleaned, 128);
        if !name.is_empty() && self.struct_stack.len() < MAX_STRUCTURE_DEPTH {
            self.struct_stack.push(StructFrame {
                indent,
                name,
                selection_range: key_start..key_end,
                selection_point: self.lines.point_at(key_start),
                start_byte: key_start,
                start_point: self.lines.point_at(key_start),
                last_end_byte: line_end,
                children: Vec::new(),
            });
            self.enclosing_ranges.push(key_start..line_end);
        }
    }

    fn scan_block_value(
        &mut self,
        line_idx: usize,
        mut pos: usize,
        line_end: usize,
        base_indent: usize,
    ) {
        while pos < line_end {
            pos = skip_ws(self.bytes, pos, line_end);
            if pos >= line_end {
                return;
            }
            match self.bytes[pos] {
                b'#' => {
                    self.spans.push(pos, line_end, SyntaxTokenKind::Comment);
                    return;
                }
                b'&' | b'*' => {
                    let end = scan_yaml_identifier_end(self.bytes, pos + 1, line_end, false);
                    if end > pos + 1 {
                        self.spans.push(pos, end, SyntaxTokenKind::Annotation);
                        pos = end;
                    } else {
                        break;
                    }
                }
                b'!' => {
                    let end = scan_yaml_tag_end(self.bytes, pos, line_end);
                    self.spans.push(pos, end, SyntaxTokenKind::Type);
                    pos = end;
                }
                _ => break,
            }
        }

        pos = skip_ws(self.bytes, pos, line_end);
        if pos >= line_end {
            return;
        }

        if matches!(self.bytes[pos], b'|' | b'>')
            && let Some((header_end, comment_start)) =
                parse_block_scalar_header(self.bytes, pos, line_end)
        {
            self.spans.push(pos, header_end, SyntaxTokenKind::Operator);
            if let Some(cstart) = comment_start {
                self.spans.push(cstart, line_end, SyntaxTokenKind::Comment);
            }
            self.block_scalar = Some(BlockScalarState {
                base_indent,
                content_indent: None,
                header_line: line_idx,
                header_end_byte: line_end,
                last_content_line: line_idx,
                last_content_end: line_end,
                start_byte: pos,
            });
            return;
        }

        if matches!(self.bytes[pos], b'{' | b'[') {
            self.scan_flow_tokens(line_idx, pos, line_end);
            return;
        }

        if matches!(self.bytes[pos], b'"' | b'\'') {
            let quote = self.bytes[pos];
            if let Some(after_quote) = self.scan_quoted_span(pos, line_end, quote, false) {
                self.enclosing_ranges.push(pos..after_quote);
                let rest = skip_ws(self.bytes, after_quote, line_end);
                if rest < line_end && self.bytes[rest] == b'#' {
                    self.spans.push(rest, line_end, SyntaxTokenKind::Comment);
                } else if rest < line_end {
                    self.scan_value_tokens(line_idx, rest, line_end);
                }
            }
            return;
        }

        self.scan_plain_scalar_line(pos, line_end);
    }

    fn scan_flow_tokens(&mut self, line_idx: usize, mut pos: usize, line_end: usize) {
        while pos < line_end {
            pos = skip_ws(self.bytes, pos, line_end);
            if pos >= line_end {
                break;
            }
            match self.bytes[pos] {
                b'#' => {
                    self.spans.push(pos, line_end, SyntaxTokenKind::Comment);
                    break;
                }
                b'{' | b'[' => {
                    let delim = self.bytes[pos];
                    self.spans.push(pos, pos + 1, SyntaxTokenKind::Punctuation);
                    self.flow_stack.push(FlowFrame {
                        delim,
                        start_byte: pos,
                        start_line: line_idx,
                    });
                    pos += 1;
                }
                b'}' | b']' => {
                    self.handle_flow_close_bracket(line_idx, pos, self.bytes[pos]);
                    pos += 1;
                }
                b',' | b':' => {
                    self.spans.push(pos, pos + 1, SyntaxTokenKind::Punctuation);
                    pos += 1;
                }
                b'&' | b'*' => {
                    let end = scan_yaml_identifier_end(self.bytes, pos + 1, line_end, true);
                    if end > pos + 1 {
                        self.spans.push(pos, end, SyntaxTokenKind::Annotation);
                        pos = end;
                    } else {
                        self.spans.push(pos, pos + 1, SyntaxTokenKind::Operator);
                        pos += 1;
                    }
                }
                b'!' => {
                    let end = scan_yaml_tag_end(self.bytes, pos, line_end);
                    self.spans.push(pos, end, SyntaxTokenKind::Type);
                    pos = end;
                }
                b'"' | b'\'' => {
                    let quote = self.bytes[pos];
                    let in_map = self
                        .flow_stack
                        .last()
                        .is_some_and(|frame| frame.delim == b'{');
                    if let Some(after) = self.scan_quoted_span(pos, line_end, quote, false) {
                        let next_ws = skip_ws(self.bytes, after, line_end);
                        if in_map && next_ws < line_end && self.bytes[next_ws] == b':' {
                            self.spans.push(pos, after, SyntaxTokenKind::Property);
                        }
                        self.enclosing_ranges.push(pos..after);
                        pos = after;
                    } else {
                        break;
                    }
                }
                _ => {
                    let in_map = self
                        .flow_stack
                        .last()
                        .is_some_and(|frame| frame.delim == b'{');
                    let end = scan_flow_scalar_end(self.bytes, pos, line_end);
                    let trimmed_end = trim_ascii_ws_end(self.bytes, pos, end);
                    if trimmed_end > pos {
                        let next_ws = skip_ws(self.bytes, trimmed_end, line_end);
                        if in_map && next_ws < line_end && self.bytes[next_ws] == b':' {
                            let is_merge = &self.bytes[pos..trimmed_end] == b"<<";
                            let kind = if is_merge {
                                SyntaxTokenKind::Operator
                            } else {
                                SyntaxTokenKind::Property
                            };
                            self.spans.push(pos, trimmed_end, kind);
                        } else {
                            self.emit_scalar_text(pos, trimmed_end);
                        }
                    }
                    pos = end.max(pos + 1);
                }
            }
        }
    }

    fn handle_flow_close_bracket(&mut self, line_idx: usize, pos: usize, close: u8) {
        self.spans.push(pos, pos + 1, SyntaxTokenKind::Punctuation);
        let expected_open = if close == b'}' { b'{' } else { b'[' };
        let col = pos - self.lines.raw_line_range(line_idx, self.bytes.len()).start;
        if let Some(top) = self.flow_stack.last().copied() {
            if top.delim == expected_open {
                self.flow_stack.pop();
                let end_byte = pos + 1;
                self.enclosing_ranges.push(top.start_byte..end_byte);
                if line_idx > top.start_line {
                    self.folds.push(FoldRange {
                        start_line: top.start_line,
                        end_line: line_idx,
                        collapsed_byte_range: top.start_byte..end_byte,
                        kind: FoldKind::Block,
                        default_collapsed: false,
                        placeholder: if expected_open == b'{' {
                            "{...}"
                        } else {
                            "[...]"
                        },
                    });
                }
            } else {
                push_diagnostic(
                    &mut self.diagnostics,
                    Point::new(line_idx, col),
                    format!("Syntax error: mismatched closing `{}`", char::from(close)),
                );
            }
        } else {
            push_diagnostic(
                &mut self.diagnostics,
                Point::new(line_idx, col),
                format!("Syntax error: unexpected `{}`", char::from(close)),
            );
        }
    }

    fn scan_value_tokens(&mut self, line_idx: usize, pos: usize, line_end: usize) {
        if pos >= line_end {
            return;
        }
        if self.bytes[pos] == b'#' {
            self.spans.push(pos, line_end, SyntaxTokenKind::Comment);
        } else if matches!(self.bytes[pos], b'"' | b'\'') {
            let quote = self.bytes[pos];
            let _ = self.scan_quoted_span(pos, line_end, quote, false);
        } else if matches!(self.bytes[pos], b'{' | b'[') {
            self.scan_flow_tokens(line_idx, pos, line_end);
        } else {
            self.scan_plain_scalar_line(pos, line_end);
        }
    }

    fn scan_plain_scalar_line(&mut self, start: usize, line_end: usize) {
        let mut comment_at = None;
        for idx in start..line_end {
            if self.bytes[idx] == b'#'
                && (idx == start || matches!(self.bytes[idx - 1], b' ' | b'\t'))
            {
                comment_at = Some(idx);
                break;
            }
        }
        let scalar_limit = comment_at.unwrap_or(line_end);
        let scalar_end = trim_ascii_ws_end(self.bytes, start, scalar_limit);
        if scalar_end > start {
            self.emit_scalar_text(start, scalar_end);
            self.enclosing_ranges.push(start..scalar_end);
        }
        if let Some(cpos) = comment_at {
            self.spans.push(cpos, line_end, SyntaxTokenKind::Comment);
        }
    }

    fn emit_scalar_text(&mut self, start: usize, end: usize) {
        let raw = &self.source[start..end];
        if is_yaml_bool_or_null(raw) {
            self.spans.push(start, end, SyntaxTokenKind::Boolean);
        } else if is_yaml_number(raw) {
            self.spans.push(start, end, SyntaxTokenKind::Number);
        } else {
            self.emit_string_with_interpolations(start, end);
        }
    }

    fn emit_string_with_interpolations(&mut self, start: usize, end: usize) {
        let mut cursor = start;
        let mut idx = start;
        while idx + 1 < end {
            if (self.bytes[idx] == b'$' || self.bytes[idx] == b'{') && self.bytes[idx + 1] == b'{' {
                let close_seq = if self.bytes[idx] == b'$' {
                    b"}".as_slice()
                } else {
                    b"}}".as_slice()
                };
                if let Some(close_rel) = find_subslice(&self.bytes[idx + 2..end], close_seq) {
                    if cursor < idx {
                        self.spans.push(cursor, idx, SyntaxTokenKind::String);
                    }
                    let esc_end = idx + 2 + close_rel + close_seq.len();
                    self.spans.push(idx, esc_end, SyntaxTokenKind::StringEscape);
                    cursor = esc_end;
                    idx = esc_end;
                    continue;
                }
            }
            idx += 1;
        }
        if cursor < end {
            self.spans.push(cursor, end, SyntaxTokenKind::String);
        }
    }

    fn scan_quoted_continuation(
        &mut self,
        line_start: usize,
        line_end: usize,
        qstate: QuotedState,
    ) -> Option<usize> {
        let after = self.scan_quoted_span(line_start, line_end, qstate.quote, true);
        if let Some(end_pos) = after {
            self.quoted = None;
            self.enclosing_ranges.push(qstate.start_byte..end_pos);
            let start_line = self.lines.line_at(qstate.start_byte);
            let end_line = self.lines.line_at(end_pos.saturating_sub(1));
            if end_line > start_line {
                self.folds.push(FoldRange {
                    start_line,
                    end_line,
                    collapsed_byte_range: qstate.start_byte..end_pos,
                    kind: FoldKind::Block,
                    default_collapsed: false,
                    placeholder: "...",
                });
            }
            return Some(end_pos);
        }
        None
    }

    fn scan_quoted_span(
        &mut self,
        start: usize,
        line_end: usize,
        quote: u8,
        is_continuation: bool,
    ) -> Option<usize> {
        let mut seg_start = start;
        let mut idx = if is_continuation { start } else { start + 1 };

        while idx < line_end {
            let b = self.bytes[idx];
            if quote == b'"' && b == b'\\' {
                if seg_start < idx {
                    self.emit_string_with_interpolations(seg_start, idx);
                }
                let esc_end = double_quote_escape_end(self.bytes, idx, line_end);
                self.spans.push(idx, esc_end, SyntaxTokenKind::StringEscape);
                seg_start = esc_end;
                idx = esc_end;
                continue;
            }
            if quote == b'\'' && b == b'\'' {
                if idx + 1 < line_end && self.bytes[idx + 1] == b'\'' {
                    if seg_start < idx {
                        self.emit_string_with_interpolations(seg_start, idx);
                    }
                    self.spans.push(idx, idx + 2, SyntaxTokenKind::StringEscape);
                    seg_start = idx + 2;
                    idx += 2;
                    continue;
                }
                let end_pos = idx + 1;
                if seg_start < end_pos {
                    self.emit_string_with_interpolations(seg_start, end_pos);
                }
                return Some(end_pos);
            }
            if quote == b'"' && b == b'"' {
                let end_pos = idx + 1;
                if seg_start < end_pos {
                    self.emit_string_with_interpolations(seg_start, end_pos);
                }
                return Some(end_pos);
            }
            idx += 1;
        }

        if seg_start < line_end {
            self.emit_string_with_interpolations(seg_start, line_end);
        }
        if !is_continuation {
            self.quoted = Some(QuotedState {
                quote,
                start_byte: start,
            });
        }
        None
    }

    fn handle_comment_line(
        &mut self,
        line_idx: usize,
        indent: usize,
        comment_start: usize,
        line_end: usize,
    ) {
        let body = &self.source[comment_start + 1..line_end];
        match parse_region_keyword(body) {
            Some(RegionMarker::Start) => {
                self.flush_comment_run();
                self.region_stack.push((line_idx, line_end));
                return;
            }
            Some(RegionMarker::End) => {
                self.flush_comment_run();
                if let Some((start_line, open_end)) = self.region_stack.pop()
                    && line_idx > start_line
                    && open_end < line_end
                {
                    self.folds.push(FoldRange {
                        start_line,
                        end_line: line_idx,
                        collapsed_byte_range: open_end..line_end,
                        kind: FoldKind::Region,
                        default_collapsed: false,
                        placeholder: "...",
                    });
                }
                return;
            }
            None => {}
        }

        match self.comment_run {
            Some(mut run) if run.indent == indent && line_idx == run.last_line + 1 => {
                run.last_line = line_idx;
                run.last_end = line_end;
                self.comment_run = Some(run);
            }
            _ => {
                self.flush_comment_run();
                self.comment_run = Some(CommentRun {
                    indent,
                    start_line: line_idx,
                    start_byte: comment_start,
                    last_line: line_idx,
                    last_end: line_end,
                });
            }
        }
    }

    fn flush_comment_run(&mut self) {
        if let Some(run) = self.comment_run.take()
            && run.last_line > run.start_line
            && run.start_byte < run.last_end
        {
            self.folds.push(FoldRange {
                start_line: run.start_line,
                end_line: run.last_line,
                collapsed_byte_range: run.start_byte..run.last_end,
                kind: FoldKind::Comment,
                default_collapsed: false,
                placeholder: "# ...",
            });
        }
    }

    fn close_block_scalar(&mut self, bs: BlockScalarState) {
        if bs.last_content_line > bs.header_line && bs.header_end_byte < bs.last_content_end {
            self.folds.push(FoldRange {
                start_line: bs.header_line,
                end_line: bs.last_content_line,
                collapsed_byte_range: bs.header_end_byte..bs.last_content_end,
                kind: FoldKind::Block,
                default_collapsed: false,
                placeholder: "...",
            });
            self.enclosing_ranges
                .push(bs.start_byte..bs.last_content_end);
        }
        self.block_scalar = None;
    }

    fn update_active_indent_blocks(&mut self, line_idx: usize, line_end: usize) {
        for frame in &mut self.indent_folds {
            frame.last_content_line = line_idx;
            frame.last_content_end = line_end;
        }
        for sframe in &mut self.struct_stack {
            sframe.last_end_byte = line_end;
        }
    }

    fn close_blocks_above_or_equal(&mut self, indent: usize) {
        while let Some(top) = self.indent_folds.last() {
            if top.indent >= indent {
                if let Some(frame) = self.indent_folds.pop()
                    && frame.last_content_line > frame.start_line
                    && frame.collapse_start_byte < frame.last_content_end
                {
                    self.folds.push(FoldRange {
                        start_line: frame.start_line,
                        end_line: frame.last_content_line,
                        collapsed_byte_range: frame.collapse_start_byte..frame.last_content_end,
                        kind: FoldKind::Block,
                        default_collapsed: false,
                        placeholder: "...",
                    });
                }
            } else {
                break;
            }
        }

        while let Some(top) = self.struct_stack.last() {
            if top.indent >= indent {
                if let Some(frame) = self.struct_stack.pop() {
                    self.emit_struct_frame(frame);
                }
            } else {
                break;
            }
        }
    }

    fn close_all_block_frames(&mut self) {
        self.close_blocks_above_or_equal(0);
    }

    fn emit_struct_frame(&mut self, frame: StructFrame) {
        let kind = if frame.children.is_empty() {
            StructureSymbolKind::Property
        } else {
            StructureSymbolKind::Section
        };
        let end_byte = frame.last_end_byte.max(frame.selection_range.end);
        if end_byte > frame.start_byte {
            self.enclosing_ranges.push(frame.start_byte..end_byte);
        }
        let node = StructureNode {
            name: frame.name,
            detail: None,
            kind,
            visibility: SymbolVisibility::Unspecified,
            is_static: false,
            is_abstract: false,
            full_range: frame.start_byte..end_byte,
            selection_range: frame.selection_range,
            start_point: frame.start_point,
            end_point: self.lines.point_at(end_byte),
            selection_point: frame.selection_point,
            children: frame.children,
        };
        if let Some(parent) = self.struct_stack.last_mut() {
            parent.children.push(node);
        } else {
            self.struct_roots.push(node);
        }
    }

    fn finish(&mut self) {
        self.flush_comment_run();
        if let Some(bs) = self.block_scalar.take() {
            self.close_block_scalar(bs);
        }
        if let Some(q) = self.quoted.take() {
            push_diagnostic(
                &mut self.diagnostics,
                self.lines.point_at(q.start_byte),
                format!(
                    "Syntax error: unclosed `{}` string literal",
                    char::from(q.quote)
                ),
            );
        }
        while let Some(flow) = self.flow_stack.pop() {
            push_diagnostic(
                &mut self.diagnostics,
                self.lines.point_at(flow.start_byte),
                format!("Syntax error: unclosed `{}`", char::from(flow.delim)),
            );
        }
        self.close_all_block_frames();
    }
}

fn skip_ws(bytes: &[u8], mut pos: usize, limit: usize) -> usize {
    while pos < limit && matches!(bytes[pos], b' ' | b'\t') {
        pos += 1;
    }
    pos
}

fn trim_ascii_ws_end(bytes: &[u8], start: usize, mut end: usize) -> usize {
    while end > start && matches!(bytes[end - 1], b' ' | b'\t') {
        end -= 1;
    }
    end
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn double_quote_escape_end(bytes: &[u8], backslash_pos: usize, line_end: usize) -> usize {
    if backslash_pos + 1 >= line_end {
        return line_end;
    }
    match bytes[backslash_pos + 1] {
        b'x' => (backslash_pos + 4).min(line_end),
        b'u' => (backslash_pos + 6).min(line_end),
        b'U' => (backslash_pos + 10).min(line_end),
        _ => backslash_pos + 2,
    }
}

fn find_mapping_key(bytes: &[u8], start: usize, line_end: usize) -> Option<(usize, usize, usize)> {
    if start >= line_end {
        return None;
    }
    if matches!(bytes[start], b'"' | b'\'') {
        let quote = bytes[start];
        let mut idx = start + 1;
        while idx < line_end {
            if quote == b'"' && bytes[idx] == b'\\' {
                idx = (idx + 2).min(line_end);
                continue;
            }
            if quote == b'\'' && bytes[idx] == b'\'' {
                if idx + 1 < line_end && bytes[idx + 1] == b'\'' {
                    idx += 2;
                    continue;
                }
                let key_end = idx + 1;
                let after = skip_ws(bytes, key_end, line_end);
                if after < line_end
                    && bytes[after] == b':'
                    && (after + 1 == line_end || matches!(bytes[after + 1], b' ' | b'\t' | b'#'))
                {
                    return Some((start, key_end, after));
                }
                return None;
            }
            if quote == b'"' && bytes[idx] == b'"' {
                let key_end = idx + 1;
                let after = skip_ws(bytes, key_end, line_end);
                if after < line_end
                    && bytes[after] == b':'
                    && (after + 1 == line_end || matches!(bytes[after + 1], b' ' | b'\t' | b'#'))
                {
                    return Some((start, key_end, after));
                }
                return None;
            }
            idx += 1;
        }
        return None;
    }

    if matches!(bytes[start], b'&' | b'*' | b'!' | b'?' | b',' | b']' | b'}') {
        return None;
    }

    let mut idx = start;
    while idx < line_end {
        match bytes[idx] {
            b'#' if idx > start && matches!(bytes[idx - 1], b' ' | b'\t') => return None,
            b':' if idx + 1 == line_end || matches!(bytes[idx + 1], b' ' | b'\t') => {
                let key_end = trim_ascii_ws_end(bytes, start, idx);
                return (key_end > start).then_some((start, key_end, idx));
            }
            _ => idx += 1,
        }
    }
    None
}

fn scan_yaml_identifier_end(bytes: &[u8], mut pos: usize, limit: usize, in_flow: bool) -> usize {
    while pos < limit {
        let b = bytes[pos];
        if matches!(b, b' ' | b'\t' | b'#')
            || (in_flow && matches!(b, b',' | b'[' | b']' | b'{' | b'}'))
        {
            break;
        }
        pos += 1;
    }
    pos
}

fn scan_yaml_tag_end(bytes: &[u8], start: usize, limit: usize) -> usize {
    if start + 1 < limit && bytes[start + 1] == b'<' {
        let mut pos = start + 2;
        while pos < limit {
            if bytes[pos] == b'>' {
                return pos + 1;
            }
            pos += 1;
        }
        return limit;
    }
    let mut pos = start + 1;
    while pos < limit
        && !matches!(
            bytes[pos],
            b' ' | b'\t' | b',' | b'[' | b']' | b'{' | b'}' | b'#'
        )
    {
        pos += 1;
    }
    pos
}

fn scan_flow_scalar_end(bytes: &[u8], mut pos: usize, limit: usize) -> usize {
    while pos < limit {
        match bytes[pos] {
            b',' | b']' | b'}' | b'{' | b'[' | b'#' => break,
            b':' if pos + 1 == limit
                || matches!(bytes[pos + 1], b' ' | b'\t' | b',' | b']' | b'}') =>
            {
                break;
            }
            _ => pos += 1,
        }
    }
    pos
}

fn parse_block_scalar_header(
    bytes: &[u8],
    start: usize,
    line_end: usize,
) -> Option<(usize, Option<usize>)> {
    let mut pos = start + 1;
    let mut saw_chomp = false;
    let mut saw_indent = false;
    while pos < line_end {
        match bytes[pos] {
            b'+' | b'-' if !saw_chomp => {
                saw_chomp = true;
                pos += 1;
            }
            b'1'..=b'9' if !saw_indent => {
                saw_indent = true;
                pos += 1;
            }
            _ => break,
        }
    }
    let header_end = pos;
    let after = skip_ws(bytes, header_end, line_end);
    if after == line_end {
        Some((header_end, None))
    } else if bytes[after] == b'#' {
        Some((header_end, Some(after)))
    } else {
        None
    }
}

fn is_yaml_bool_or_null(raw: &str) -> bool {
    matches!(
        raw,
        "true" | "True" | "TRUE" | "false" | "False" | "FALSE" | "null" | "Null" | "NULL" | "~"
    )
}

fn is_yaml_number(raw: &str) -> bool {
    if matches!(
        raw,
        ".inf"
            | "+.inf"
            | "-.inf"
            | ".Inf"
            | "+.Inf"
            | "-.Inf"
            | ".INF"
            | "+.INF"
            | "-.INF"
            | ".nan"
            | ".NaN"
            | ".NAN"
    ) {
        return true;
    }
    if let Some(hex) = raw.strip_prefix("0x") {
        return !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit());
    }
    if let Some(oct) = raw.strip_prefix("0o") {
        return !oct.is_empty() && oct.bytes().all(|b| matches!(b, b'0'..=b'7'));
    }

    let s = raw
        .strip_prefix('+')
        .or_else(|| raw.strip_prefix('-'))
        .unwrap_or(raw);
    if s.is_empty() {
        return false;
    }

    let mut saw_digit = false;
    let mut saw_dot = false;
    let mut saw_exp = false;
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'0'..=b'9' => {
                saw_digit = true;
                i += 1;
            }
            b'.' if !saw_dot && !saw_exp => {
                saw_dot = true;
                i += 1;
            }
            b'e' | b'E' if saw_digit && !saw_exp => {
                saw_exp = true;
                i += 1;
                if i < bytes.len() && matches!(bytes[i], b'+' | b'-') {
                    i += 1;
                }
                if i >= bytes.len() || !bytes[i].is_ascii_digit() {
                    return false;
                }
            }
            _ => return false,
        }
    }
    saw_digit
}
