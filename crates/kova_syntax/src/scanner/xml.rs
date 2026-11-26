//! Deterministic single-pass XML scanner (highlighting, folding, structure, diagnostics).

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

#[derive(Debug, Clone)]
struct TagFrame {
    name: String,
    start_byte: usize,
    start_line: usize,
    open_end_byte: usize,
    selection_range: Range<usize>,
    selection_point: Point,
    detail: Option<String>,
    children: Vec<StructureNode>,
}

struct XmlScanner<'a> {
    source: &'a str,
    bytes: &'a [u8],
    lines: LineIndex,
    spans: SpanBuilder,
    folds: Vec<FoldRange>,
    enclosing_ranges: Vec<Range<usize>>,
    diagnostics: Vec<SyntaxDiagnostic>,
    tag_stack: Vec<TagFrame>,
    struct_roots: Vec<StructureNode>,
    region_stack: Vec<(usize, usize)>,
}

/// Scans XML `source` into a [`ScannedDocument`].
///
/// # Errors
///
/// Propagates cancellation or timeout errors from `budget`.
pub(crate) fn scan(
    source: &str,
    budget: &mut ScanBudget<'_>,
) -> Result<ScannedDocument, SyntaxError> {
    let lines = LineIndex::new(source);
    let mut scanner = XmlScanner {
        source,
        bytes: source.as_bytes(),
        lines,
        spans: SpanBuilder::with_capacity(source.len() / 6 + 16),
        folds: Vec::new(),
        enclosing_ranges: Vec::new(),
        diagnostics: Vec::new(),
        tag_stack: Vec::new(),
        struct_roots: Vec::new(),
        region_stack: Vec::new(),
    };

    scanner.scan_all(budget)?;
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

impl<'a> XmlScanner<'a> {
    fn scan_all(&mut self, budget: &mut ScanBudget<'_>) -> Result<(), SyntaxError> {
        let len = self.bytes.len();
        let mut pos = 0usize;

        while pos < len {
            budget.tick()?;
            if self.bytes[pos] == b'<' {
                if self.bytes[pos..].starts_with(b"<!--") {
                    pos = self.scan_comment(pos);
                } else if self.bytes[pos..].starts_with(b"<![CDATA[") {
                    pos = self.scan_cdata(pos);
                } else if self.bytes[pos..].starts_with(b"<?") {
                    pos = self.scan_processing_instruction(pos);
                } else if self.bytes[pos..].starts_with(b"<!") {
                    pos = self.scan_declaration(pos);
                } else if self.bytes[pos..].starts_with(b"</") {
                    pos = self.scan_closing_tag(pos);
                } else {
                    pos = self.scan_opening_tag(pos);
                }
            } else if self.bytes[pos] == b'&' {
                if let Some(ent_end) = scan_entity_end(self.bytes, pos, len) {
                    self.spans.push(pos, ent_end, SyntaxTokenKind::StringEscape);
                    pos = ent_end;
                } else {
                    pos += 1;
                }
            } else {
                pos += 1;
            }
        }
        Ok(())
    }

    fn scan_comment(&mut self, start: usize) -> usize {
        let len = self.bytes.len();
        let body_start = start + 4;
        if let Some(rel_end) = find_subslice(&self.bytes[body_start..], b"-->") {
            let body_end = body_start + rel_end;
            let end = body_end + 3;
            self.spans.push(start, end, SyntaxTokenKind::Comment);
            self.enclosing_ranges.push(start..end);

            let start_line = self.lines.line_at(start);
            let end_line = self.lines.line_at(end - 1);
            let body_text = &self.source[body_start..body_end];
            if !body_text.contains('\n') {
                match parse_region_keyword(body_text) {
                    Some(RegionMarker::Start) => {
                        let line_end = self
                            .lines
                            .line_content_end(start_line, self.source)
                            .max(end);
                        self.region_stack.push((start_line, line_end));
                        return end;
                    }
                    Some(RegionMarker::End) => {
                        if let Some((reg_start_line, open_end)) = self.region_stack.pop()
                            && end_line > reg_start_line
                            && open_end < end
                        {
                            self.folds.push(FoldRange {
                                start_line: reg_start_line,
                                end_line,
                                collapsed_byte_range: open_end..end,
                                kind: FoldKind::Region,
                                default_collapsed: false,
                                placeholder: "...",
                            });
                        }
                        return end;
                    }
                    None => {}
                }
            }

            if end_line > start_line {
                self.folds.push(FoldRange {
                    start_line,
                    end_line,
                    collapsed_byte_range: start..end,
                    kind: FoldKind::Comment,
                    default_collapsed: false,
                    placeholder: "<!-- ... -->",
                });
            }
            end
        } else {
            self.spans.push(start, len, SyntaxTokenKind::Comment);
            push_diagnostic(
                &mut self.diagnostics,
                self.lines.point_at(start),
                String::from("Syntax error: unclosed XML comment"),
            );
            len
        }
    }

    fn scan_cdata(&mut self, start: usize) -> usize {
        let len = self.bytes.len();
        let open_end = start + 9; // "<![CDATA["
        self.spans.push(start, open_end, SyntaxTokenKind::Keyword);

        if let Some(rel_end) = find_subslice(&self.bytes[open_end..], b"]]>") {
            let content_end = open_end + rel_end;
            let end = content_end + 3;
            self.spans
                .push(open_end, content_end, SyntaxTokenKind::String);
            self.spans.push(content_end, end, SyntaxTokenKind::Keyword);
            self.enclosing_ranges.push(start..end);

            let start_line = self.lines.line_at(start);
            let end_line = self.lines.line_at(end - 1);
            if end_line > start_line {
                self.folds.push(FoldRange {
                    start_line,
                    end_line,
                    collapsed_byte_range: start..end,
                    kind: FoldKind::Block,
                    default_collapsed: false,
                    placeholder: "<![CDATA[ ... ]]>",
                });
            }
            end
        } else {
            self.spans.push(open_end, len, SyntaxTokenKind::String);
            push_diagnostic(
                &mut self.diagnostics,
                self.lines.point_at(start),
                String::from("Syntax error: unclosed CDATA section"),
            );
            len
        }
    }

    fn scan_processing_instruction(&mut self, start: usize) -> usize {
        let len = self.bytes.len();
        self.spans
            .push(start, start + 2, SyntaxTokenKind::Punctuation);
        let mut pos = skip_xml_ws(self.bytes, start + 2, len);
        let name_start = pos;
        while pos < len && is_xml_name_byte(self.bytes[pos]) {
            pos += 1;
        }
        if pos > name_start {
            self.spans.push(name_start, pos, SyntaxTokenKind::Keyword);
        }

        while pos < len {
            pos = skip_xml_ws(self.bytes, pos, len);
            if pos >= len {
                break;
            }
            if self.bytes[pos..].starts_with(b"?>") {
                self.spans.push(pos, pos + 2, SyntaxTokenKind::Punctuation);
                self.enclosing_ranges.push(start..pos + 2);
                return pos + 2;
            }
            if self.bytes[pos] == b'<' {
                push_diagnostic(
                    &mut self.diagnostics,
                    self.lines.point_at(start),
                    String::from("Syntax error: unclosed processing instruction"),
                );
                return pos;
            }
            if is_xml_name_start_byte(self.bytes[pos]) {
                let (next_pos, _, _) = self.scan_attribute(pos);
                pos = next_pos;
            } else {
                pos += 1;
            }
        }

        push_diagnostic(
            &mut self.diagnostics,
            self.lines.point_at(start),
            String::from("Syntax error: unclosed processing instruction"),
        );
        len
    }

    fn scan_declaration(&mut self, start: usize) -> usize {
        let len = self.bytes.len();
        self.spans
            .push(start, start + 2, SyntaxTokenKind::Punctuation);
        let mut pos = start + 2;
        let kw_start = pos;
        while pos < len && is_xml_name_byte(self.bytes[pos]) {
            pos += 1;
        }
        if pos > kw_start {
            self.spans.push(kw_start, pos, SyntaxTokenKind::Keyword);
        }

        let mut bracket_depth = 0usize;
        while pos < len {
            match self.bytes[pos] {
                b'"' | b'\'' => {
                    let quote = self.bytes[pos];
                    pos = self.scan_quoted_value(pos, quote);
                }
                b'[' => {
                    self.spans.push(pos, pos + 1, SyntaxTokenKind::Punctuation);
                    bracket_depth += 1;
                    pos += 1;
                }
                b']' => {
                    self.spans.push(pos, pos + 1, SyntaxTokenKind::Punctuation);
                    bracket_depth = bracket_depth.saturating_sub(1);
                    pos += 1;
                }
                b'>' if bracket_depth == 0 => {
                    self.spans.push(pos, pos + 1, SyntaxTokenKind::Punctuation);
                    let end = pos + 1;
                    self.enclosing_ranges.push(start..end);
                    let start_line = self.lines.line_at(start);
                    let end_line = self.lines.line_at(end - 1);
                    if end_line > start_line {
                        let collapse_start = self
                            .lines
                            .line_content_end(start_line, self.source)
                            .max(start);
                        if collapse_start < end {
                            self.folds.push(FoldRange {
                                start_line,
                                end_line,
                                collapsed_byte_range: collapse_start..end,
                                kind: FoldKind::Block,
                                default_collapsed: false,
                                placeholder: "...",
                            });
                        }
                    }
                    return end;
                }
                b'<' if bracket_depth > 0 && self.bytes[pos..].starts_with(b"<!--") => {
                    pos = self.scan_comment(pos);
                }
                b'<' if bracket_depth > 0 && self.bytes[pos..].starts_with(b"<!") => {
                    pos = self.scan_declaration(pos);
                }
                _ => pos += 1,
            }
        }

        push_diagnostic(
            &mut self.diagnostics,
            self.lines.point_at(start),
            String::from("Syntax error: unclosed XML declaration"),
        );
        len
    }

    fn scan_opening_tag(&mut self, start: usize) -> usize {
        let len = self.bytes.len();
        let after_lt = start + 1;
        if after_lt >= len || !is_xml_name_start_byte(self.bytes[after_lt]) {
            self.spans
                .push(start, after_lt, SyntaxTokenKind::Punctuation);
            push_diagnostic(
                &mut self.diagnostics,
                self.lines.point_at(start),
                String::from("Syntax error: unexpected `<` in XML"),
            );
            return after_lt;
        }

        self.spans
            .push(start, after_lt, SyntaxTokenKind::Punctuation);
        let (name_end, tag_name) = self.scan_qname(after_lt);
        let selection_range = after_lt..name_end;
        let selection_point = self.lines.point_at(after_lt);

        let mut pos = name_end;
        let mut detail: Option<String> = None;

        while pos < len {
            pos = skip_xml_ws(self.bytes, pos, len);
            if pos >= len {
                break;
            }
            if self.bytes[pos..].starts_with(b"/>") {
                return self.finish_self_closing_tag(
                    start,
                    pos,
                    tag_name,
                    selection_range,
                    selection_point,
                    detail,
                );
            }
            if self.bytes[pos] == b'>' {
                self.spans.push(pos, pos + 1, SyntaxTokenKind::Punctuation);
                let open_end = pos + 1;
                let start_line = self.lines.line_at(start);
                self.tag_stack.push(TagFrame {
                    name: tag_name,
                    start_byte: start,
                    start_line,
                    open_end_byte: open_end,
                    selection_range,
                    selection_point,
                    detail,
                    children: Vec::new(),
                });
                return open_end;
            }
            if self.bytes[pos] == b'<' {
                // Recovery while typing an incomplete opening tag (e.g. `<dep\n<other/>`).
                push_diagnostic(
                    &mut self.diagnostics,
                    self.lines.point_at(start),
                    format!("Syntax error: unclosed `<{tag_name}` tag"),
                );
                return pos;
            }
            if is_xml_name_start_byte(self.bytes[pos]) {
                let (next_pos, attr_name, attr_val) = self.scan_attribute(pos);
                if let (Some(aname), Some(aval)) = (attr_name, attr_val)
                    && !aval.is_empty()
                {
                    let should_set = match aname {
                        "id" => true,
                        "name" | "key" => detail.is_none(),
                        _ => false,
                    };
                    if should_set {
                        detail = Some(compact_text(&aval, 64));
                    }
                }
                pos = next_pos;
            } else {
                pos += 1;
            }
        }

        push_diagnostic(
            &mut self.diagnostics,
            self.lines.point_at(start),
            format!("Syntax error: unclosed `<{tag_name}` tag"),
        );
        len
    }

    fn finish_self_closing_tag(
        &mut self,
        start: usize,
        slash_pos: usize,
        tag_name: String,
        selection_range: Range<usize>,
        selection_point: Point,
        detail: Option<String>,
    ) -> usize {
        self.spans
            .push(slash_pos, slash_pos + 2, SyntaxTokenKind::Punctuation);
        let end = slash_pos + 2;
        let start_line = self.lines.line_at(start);
        let end_line = self.lines.line_at(end - 1);
        self.enclosing_ranges.push(start..end);
        if end_line > start_line {
            let collapse_start = self
                .lines
                .line_content_end(start_line, self.source)
                .max(start);
            if collapse_start < end {
                self.folds.push(FoldRange {
                    start_line,
                    end_line,
                    collapsed_byte_range: collapse_start..end,
                    kind: FoldKind::Block,
                    default_collapsed: false,
                    placeholder: "...",
                });
            }
        }
        self.push_completed_element(
            TagFrame {
                name: tag_name,
                start_byte: start,
                start_line,
                open_end_byte: end,
                selection_range,
                selection_point,
                detail,
                children: Vec::new(),
            },
            end,
        );
        end
    }

    fn scan_closing_tag(&mut self, start: usize) -> usize {
        let len = self.bytes.len();
        self.spans
            .push(start, start + 2, SyntaxTokenKind::Punctuation);
        let mut pos = skip_xml_ws(self.bytes, start + 2, len);
        let (name_end, close_name) = self.scan_qname(pos);
        pos = skip_xml_ws(self.bytes, name_end, len);

        let close_end = if pos < len && self.bytes[pos] == b'>' {
            self.spans.push(pos, pos + 1, SyntaxTokenKind::Punctuation);
            pos + 1
        } else {
            push_diagnostic(
                &mut self.diagnostics,
                self.lines.point_at(start),
                format!("Syntax error: unclosed closing tag `</{close_name}`"),
            );
            pos
        };

        if close_name.is_empty() {
            return close_end;
        }

        if let Some(match_idx) = self.tag_stack.iter().rposition(|f| f.name == close_name) {
            while self.tag_stack.len() > match_idx + 1 {
                if let Some(unclosed) = self.tag_stack.pop() {
                    push_diagnostic(
                        &mut self.diagnostics,
                        self.lines.point_at(unclosed.start_byte),
                        format!("Syntax error: unclosed `<{}>` element", unclosed.name),
                    );
                    let fallback_end = start.max(unclosed.open_end_byte);
                    self.push_completed_element(unclosed, fallback_end);
                }
            }
            if let Some(frame) = self.tag_stack.pop() {
                let end_line = self.lines.line_at(close_end.saturating_sub(1));
                self.enclosing_ranges.push(frame.start_byte..close_end);
                if end_line > frame.start_line {
                    let collapse_start = self
                        .lines
                        .line_content_end(frame.start_line, self.source)
                        .max(frame.open_end_byte);
                    if collapse_start < close_end {
                        self.folds.push(FoldRange {
                            start_line: frame.start_line,
                            end_line,
                            collapsed_byte_range: collapse_start..close_end,
                            kind: FoldKind::Block,
                            default_collapsed: false,
                            placeholder: "...",
                        });
                    }
                }
                self.push_completed_element(frame, close_end);
            }
        } else {
            push_diagnostic(
                &mut self.diagnostics,
                self.lines.point_at(start),
                format!("Syntax error: unexpected closing tag `</{close_name}>`"),
            );
        }

        close_end
    }

    fn scan_qname(&mut self, start: usize) -> (usize, String) {
        let len = self.bytes.len();
        let mut pos = start;
        while pos < len && is_xml_name_byte(self.bytes[pos]) {
            pos += 1;
        }
        if pos == start {
            return (start, String::new());
        }
        let full = &self.source[start..pos];
        if let Some(colon_rel) = full.find(':') {
            let colon_abs = start + colon_rel;
            if colon_abs > start {
                self.spans.push(start, colon_abs, SyntaxTokenKind::Type);
            }
            self.spans
                .push(colon_abs, colon_abs + 1, SyntaxTokenKind::Punctuation);
            if colon_abs + 1 < pos {
                self.spans
                    .push(colon_abs + 1, pos, SyntaxTokenKind::Keyword);
            }
        } else {
            self.spans.push(start, pos, SyntaxTokenKind::Keyword);
        }
        (pos, full.to_owned())
    }

    fn scan_attribute(&mut self, start: usize) -> (usize, Option<&'a str>, Option<String>) {
        let len = self.bytes.len();
        let attr_start = start;
        let mut pos = start;
        while pos < len && is_xml_name_byte(self.bytes[pos]) {
            pos += 1;
        }
        if pos == attr_start {
            return (start + 1, None, None);
        }
        let attr_name = &self.source[attr_start..pos];
        self.spans.push(attr_start, pos, SyntaxTokenKind::Property);

        pos = skip_xml_ws(self.bytes, pos, len);
        if pos >= len || self.bytes[pos] != b'=' {
            self.enclosing_ranges.push(attr_start..pos);
            return (pos, Some(attr_name), None);
        }
        self.spans.push(pos, pos + 1, SyntaxTokenKind::Punctuation);
        pos = skip_xml_ws(self.bytes, pos + 1, len);

        if pos < len && matches!(self.bytes[pos], b'"' | b'\'') {
            let quote = self.bytes[pos];
            let val_start = pos;
            let val_end = self.scan_quoted_value(val_start, quote);
            self.enclosing_ranges.push(attr_start..val_end);
            self.enclosing_ranges.push(val_start..val_end);
            let inner_end = if val_end > val_start + 1 && self.bytes[val_end - 1] == quote {
                val_end - 1
            } else {
                val_end
            };
            let val = self.source[val_start + 1..inner_end].to_owned();
            (val_end, Some(attr_name), Some(val))
        } else {
            self.enclosing_ranges.push(attr_start..pos);
            (pos, Some(attr_name), None)
        }
    }

    fn scan_quoted_value(&mut self, start: usize, quote: u8) -> usize {
        let len = self.bytes.len();
        let mut seg_start = start;
        let mut pos = start + 1;
        while pos < len {
            if self.bytes[pos] == quote {
                let end = pos + 1;
                if seg_start < end {
                    self.spans.push(seg_start, end, SyntaxTokenKind::String);
                }
                return end;
            }
            if self.bytes[pos] == b'&'
                && let Some(ent_end) = scan_entity_end(self.bytes, pos, len)
            {
                if seg_start < pos {
                    self.spans.push(seg_start, pos, SyntaxTokenKind::String);
                }
                self.spans.push(pos, ent_end, SyntaxTokenKind::StringEscape);
                seg_start = ent_end;
                pos = ent_end;
                continue;
            }
            pos += 1;
        }
        if seg_start < len {
            self.spans.push(seg_start, len, SyntaxTokenKind::String);
        }
        push_diagnostic(
            &mut self.diagnostics,
            self.lines.point_at(start),
            format!(
                "Syntax error: unclosed XML `{}` attribute value",
                char::from(quote)
            ),
        );
        len
    }

    fn push_completed_element(&mut self, frame: TagFrame, end_byte: usize) {
        let depth = self.tag_stack.len();
        if depth >= MAX_STRUCTURE_DEPTH {
            return;
        }
        let kind = if frame.children.is_empty() {
            StructureSymbolKind::Property
        } else {
            StructureSymbolKind::Section
        };
        let node = StructureNode {
            name: compact_text(&frame.name, 128),
            detail: frame.detail,
            kind,
            visibility: SymbolVisibility::Unspecified,
            is_static: false,
            is_abstract: false,
            full_range: frame.start_byte..end_byte.max(frame.start_byte),
            selection_range: frame.selection_range,
            start_point: self.lines.point_at(frame.start_byte),
            end_point: self.lines.point_at(end_byte.max(frame.start_byte)),
            selection_point: frame.selection_point,
            children: frame.children,
        };
        if let Some(parent) = self.tag_stack.last_mut() {
            parent.children.push(node);
        } else {
            self.struct_roots.push(node);
        }
    }

    fn finish(&mut self) {
        let len = self.bytes.len();
        while let Some(frame) = self.tag_stack.pop() {
            push_diagnostic(
                &mut self.diagnostics,
                self.lines.point_at(frame.start_byte),
                format!("Syntax error: unclosed `<{}>` element", frame.name),
            );
            let fallback_end = len.max(frame.open_end_byte);
            self.push_completed_element(frame, fallback_end);
        }
    }
}

fn skip_xml_ws(bytes: &[u8], mut pos: usize, limit: usize) -> usize {
    while pos < limit && matches!(bytes[pos], b' ' | b'\t' | b'\r' | b'\n') {
        pos += 1;
    }
    pos
}

fn is_xml_name_start_byte(b: u8) -> bool {
    b.is_ascii_alphabetic() || matches!(b, b'_' | b':') || b >= 0x80
}

fn is_xml_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':') || b >= 0x80
}

fn scan_entity_end(bytes: &[u8], start: usize, limit: usize) -> Option<usize> {
    if start + 2 >= limit || bytes[start] != b'&' {
        return None;
    }
    let mut pos = start + 1;
    let max_entity_len = (start + 32).min(limit);
    if bytes[pos] == b'#' {
        pos += 1;
        if pos >= max_entity_len {
            return None;
        }
        if matches!(bytes[pos], b'x' | b'X') {
            pos += 1;
            let digits_start = pos;
            while pos < max_entity_len && bytes[pos].is_ascii_hexdigit() {
                pos += 1;
            }
            return (pos > digits_start && pos < limit && bytes[pos] == b';').then_some(pos + 1);
        }
        let digits_start = pos;
        while pos < max_entity_len && bytes[pos].is_ascii_digit() {
            pos += 1;
        }
        return (pos > digits_start && pos < limit && bytes[pos] == b';').then_some(pos + 1);
    }

    if !bytes[pos].is_ascii_alphabetic() && bytes[pos] != b'_' {
        return None;
    }
    while pos < max_entity_len
        && (bytes[pos].is_ascii_alphanumeric() || matches!(bytes[pos], b'_' | b'-' | b'.'))
    {
        pos += 1;
    }
    (pos < limit && bytes[pos] == b';').then_some(pos + 1)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}
