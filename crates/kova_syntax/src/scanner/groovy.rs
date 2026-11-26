//! Deterministic single-pass Groovy / Gradle DSL scanner (highlighting, folding, structure, diagnostics).

use std::ops::Range;

use kova_theme::SyntaxTokenKind;

use crate::diagnostics::SyntaxDiagnostic;
use crate::error::SyntaxError;
use crate::folding::{FoldKind, FoldRange};
use crate::scanner::{
    LineIndex, RegionMarker, ScanBudget, ScannedDocument, SpanBuilder, compact_text,
    finalize_folds, parse_region_keyword, push_diagnostic,
};
use crate::structure::{MAX_STRUCTURE_DEPTH, StructureNode, StructureSymbolKind, SymbolVisibility};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BracketKind {
    Brace,
    Paren,
    Bracket,
}

impl BracketKind {
    const fn open_char(self) -> char {
        match self {
            Self::Brace => '{',
            Self::Paren => '(',
            Self::Bracket => '[',
        }
    }

    const fn from_close_byte(b: u8) -> Option<Self> {
        match b {
            b'}' => Some(Self::Brace),
            b')' => Some(Self::Paren),
            b']' => Some(Self::Bracket),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
enum PendingDecl {
    Type {
        kind: StructureSymbolKind,
        name: String,
        selection_range: Range<usize>,
        start_byte: usize,
        visibility: SymbolVisibility,
        is_abstract: bool,
    },
    Callable {
        name: String,
        selection_range: Range<usize>,
        start_byte: usize,
        visibility: SymbolVisibility,
        is_static: bool,
        is_abstract: bool,
        detail: Option<String>,
    },
    DslSection {
        name: String,
        selection_range: Range<usize>,
        start_byte: usize,
    },
}

#[derive(Debug, Clone)]
struct BracketFrame {
    kind: BracketKind,
    open_byte: usize,
    open_line: usize,
    fold_kind: FoldKind,
    struct_frame: Option<GroovyStructFrame>,
}

#[derive(Debug, Clone)]
struct GroovyStructFrame {
    name: String,
    detail: Option<String>,
    kind: StructureSymbolKind,
    visibility: SymbolVisibility,
    is_static: bool,
    is_abstract: bool,
    start_byte: usize,
    selection_range: Range<usize>,
    children: Vec<StructureNode>,
}

#[derive(Debug, Clone, Copy)]
struct ImportRun {
    start_byte: usize,
    start_line: usize,
    end_byte: usize,
    end_line: usize,
    count: usize,
}

#[derive(Debug, Clone, Copy)]
struct PendingMods {
    visibility: Option<SymbolVisibility>,
    is_static: bool,
    is_abstract: bool,
    start_byte: Option<usize>,
}

impl PendingMods {
    const fn empty() -> Self {
        Self {
            visibility: None,
            is_static: false,
            is_abstract: false,
            start_byte: None,
        }
    }

    fn clear(&mut self) {
        *self = Self::empty();
    }

    fn note_start(&mut self, byte: usize) {
        if self.start_byte.is_none() {
            self.start_byte = Some(byte);
        }
    }
}

#[allow(
    clippy::struct_excessive_bools,
    reason = "orthogonal Groovy lexer state flags"
)]
struct GroovyScanner<'a> {
    source: &'a str,
    bytes: &'a [u8],
    lines: LineIndex,
    spans: SpanBuilder,
    folds: Vec<FoldRange>,
    enclosing_ranges: Vec<Range<usize>>,
    diagnostics: Vec<SyntaxDiagnostic>,
    brackets: Vec<BracketFrame>,
    struct_roots: Vec<StructureNode>,
    region_stack: Vec<(usize, usize)>,
    import_run: Option<ImportRun>,
    can_start_slashy: bool,
    at_statement_start: bool,
    after_dot: bool,
    in_ternary_question: bool,
    mods: PendingMods,
    pending_type_kw: Option<(StructureSymbolKind, usize)>,
    pending_def: Option<usize>,
    pending_decl: Option<PendingDecl>,
    last_ident: Option<(String, Range<usize>)>,
    param_state: Option<(usize, bool)>,
}

/// Scans Groovy / Gradle `source` into a [`ScannedDocument`].
///
/// # Errors
///
/// Propagates cancellation or timeout errors from `budget`.
pub(crate) fn scan(
    source: &str,
    budget: &mut ScanBudget<'_>,
) -> Result<ScannedDocument, SyntaxError> {
    let lines = LineIndex::new(source);
    let mut scanner = GroovyScanner {
        source,
        bytes: source.as_bytes(),
        lines,
        spans: SpanBuilder::with_capacity(source.len() / 5 + 16),
        folds: Vec::new(),
        enclosing_ranges: Vec::new(),
        diagnostics: Vec::new(),
        brackets: Vec::new(),
        struct_roots: Vec::new(),
        region_stack: Vec::new(),
        import_run: None,
        can_start_slashy: true,
        at_statement_start: true,
        after_dot: false,
        in_ternary_question: false,
        mods: PendingMods::empty(),
        pending_type_kw: None,
        pending_def: None,
        pending_decl: None,
        last_ident: None,
        param_state: None,
    };

    scanner.scan_tokens(budget)?;
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

impl GroovyScanner<'_> {
    #[allow(
        clippy::too_many_lines,
        reason = "Cohesive single-pass Groovy lexer loop"
    )]
    fn scan_tokens(&mut self, budget: &mut ScanBudget<'_>) -> Result<(), SyntaxError> {
        let len = self.bytes.len();
        let mut pos = 0usize;

        // Optional shebang on line 0.
        if self.bytes.starts_with(b"#!") {
            let line_end = self.lines.line_content_end(0, self.source);
            self.spans.push(0, line_end, SyntaxTokenKind::Comment);
            pos = line_end;
        }

        while pos < len {
            budget.tick()?;
            let b = self.bytes[pos];

            // Whitespace & newlines.
            if matches!(b, b' ' | b'\t' | b'\r' | b'\n') {
                if b == b'\n' {
                    self.at_statement_start = true;
                    self.in_ternary_question = false;
                    if self.brackets.is_empty()
                        && matches!(self.pending_decl, Some(PendingDecl::DslSection { .. }))
                    {
                        self.pending_decl = None;
                    }
                }
                pos += 1;
                continue;
            }

            // Line comment `// ...`
            if self.bytes[pos..].starts_with(b"//") {
                pos = self.scan_line_comment(pos);
                continue;
            }

            // Block / Groovydoc comment `/* ... */` or `/** ... */`
            if self.bytes[pos..].starts_with(b"/*") {
                pos = self.scan_block_comment(pos);
                continue;
            }

            // Triple-quoted strings `'''` and `"""`
            if self.bytes[pos..].starts_with(b"'''") {
                pos = self.scan_triple_single_string(pos);
                self.after_value_token();
                continue;
            }
            if self.bytes[pos..].starts_with(b"\"\"\"") {
                pos = self.scan_triple_gstring(pos);
                self.after_value_token();
                continue;
            }

            // Dollar-slashy string `$/.../$`
            if self.bytes[pos..].starts_with(b"$/") {
                pos = self.scan_dollar_slashy_string(pos);
                self.after_value_token();
                continue;
            }

            // Single-quoted and double-quoted strings `'...'` and `"..."`
            if b == b'\'' {
                pos = self.scan_single_string(pos);
                self.after_value_token();
                continue;
            }
            if b == b'"' {
                pos = self.scan_double_gstring(pos);
                self.after_value_token();
                continue;
            }

            // Slashy string `/.../` vs division `/` or `/=`
            if b == b'/' && self.can_start_slashy && !self.bytes[pos..].starts_with(b"/=") {
                if let Some(slashy_end) = self.try_scan_slashy_string(pos) {
                    pos = slashy_end;
                    self.after_value_token();
                    continue;
                }
            }

            // Annotations `@Ident` (not `.@` field-access operator).
            if b == b'@' && !self.after_dot && pos + 1 < len && is_ident_start(self.bytes[pos + 1])
            {
                pos = self.scan_annotation(pos);
                continue;
            }

            // Numbers.
            if b.is_ascii_digit() {
                pos = self.scan_number(pos);
                self.after_value_token();
                continue;
            }

            // Identifiers & keywords.
            if is_ident_start(b) {
                pos = self.scan_identifier(pos);
                continue;
            }

            // Brackets.
            if matches!(b, b'{' | b'(' | b'[') {
                pos = self.scan_open_bracket(pos, b);
                continue;
            }
            if matches!(b, b'}' | b')' | b']') {
                pos = self.scan_close_bracket(pos, b);
                continue;
            }

            // Multi-char and single-char operators / punctuation.
            pos = self.scan_operator_or_punctuation(pos);
        }

        Ok(())
    }

    fn after_value_token(&mut self) {
        self.can_start_slashy = false;
        self.at_statement_start = false;
        self.after_dot = false;
    }

    fn scan_line_comment(&mut self, start: usize) -> usize {
        let line = self.lines.line_at(start);
        let line_end = self.lines.line_content_end(line, self.source);
        self.spans.push(start, line_end, SyntaxTokenKind::Comment);

        let body = &self.source[start + 2..line_end];
        match parse_region_keyword(body) {
            Some(RegionMarker::Start) => {
                self.flush_import_run();
                self.region_stack.push((line, line_end));
            }
            Some(RegionMarker::End) => {
                self.flush_import_run();
                if let Some((start_line, open_end)) = self.region_stack.pop()
                    && line > start_line
                    && open_end < line_end
                {
                    self.folds.push(FoldRange {
                        start_line,
                        end_line: line,
                        collapsed_byte_range: open_end..line_end,
                        kind: FoldKind::Region,
                        default_collapsed: false,
                        placeholder: "...",
                    });
                }
            }
            None => {}
        }
        line_end
    }

    fn scan_block_comment(&mut self, start: usize) -> usize {
        let len = self.bytes.len();
        let is_doc =
            self.bytes[start..].starts_with(b"/**") && !self.bytes[start..].starts_with(b"/**/");
        let kind = if is_doc {
            SyntaxTokenKind::CommentDoc
        } else {
            SyntaxTokenKind::Comment
        };

        if let Some(rel_end) = find_subslice(&self.bytes[start + 2..], b"*/") {
            let end = start + 2 + rel_end + 2;
            self.spans.push(start, end, kind);
            self.enclosing_ranges.push(start..end);

            let start_line = self.lines.line_at(start);
            let end_line = self.lines.line_at(end - 1);
            if end_line > start_line {
                self.folds.push(FoldRange {
                    start_line,
                    end_line,
                    collapsed_byte_range: start..end,
                    kind: FoldKind::Comment,
                    default_collapsed: false,
                    placeholder: if is_doc { "/** ... */" } else { "/* ... */" },
                });
            } else {
                let body = &self.source[start + 2..end - 2];
                match parse_region_keyword(body) {
                    Some(RegionMarker::Start) => {
                        let line_end = self
                            .lines
                            .line_content_end(start_line, self.source)
                            .max(end);
                        self.region_stack.push((start_line, line_end));
                    }
                    Some(RegionMarker::End) => {
                        if let Some((reg_start, open_end)) = self.region_stack.pop()
                            && end_line > reg_start
                            && open_end < end
                        {
                            self.folds.push(FoldRange {
                                start_line: reg_start,
                                end_line,
                                collapsed_byte_range: open_end..end,
                                kind: FoldKind::Region,
                                default_collapsed: false,
                                placeholder: "...",
                            });
                        }
                    }
                    None => {}
                }
            }
            end
        } else {
            self.spans.push(start, len, kind);
            push_diagnostic(
                &mut self.diagnostics,
                self.lines.point_at(start),
                String::from("Syntax error: unclosed block comment"),
            );
            len
        }
    }

    fn scan_single_string(&mut self, start: usize) -> usize {
        let len = self.bytes.len();
        let mut seg_start = start;
        let mut pos = start + 1;
        while pos < len {
            let b = self.bytes[pos];
            if b == b'\n' || b == b'\r' {
                if seg_start < pos {
                    self.spans.push(seg_start, pos, SyntaxTokenKind::String);
                }
                push_diagnostic(
                    &mut self.diagnostics,
                    self.lines.point_at(start),
                    String::from("Syntax error: unclosed `'...'` string literal"),
                );
                return pos;
            }
            if b == b'\\' {
                if seg_start < pos {
                    self.spans.push(seg_start, pos, SyntaxTokenKind::String);
                }
                let esc_end = backslash_escape_end(self.bytes, pos, len);
                self.spans.push(pos, esc_end, SyntaxTokenKind::StringEscape);
                seg_start = esc_end;
                pos = esc_end;
                continue;
            }
            if b == b'\'' {
                let end = pos + 1;
                if seg_start < end {
                    self.spans.push(seg_start, end, SyntaxTokenKind::String);
                }
                self.enclosing_ranges.push(start..end);
                return end;
            }
            pos += 1;
        }
        if seg_start < len {
            self.spans.push(seg_start, len, SyntaxTokenKind::String);
        }
        push_diagnostic(
            &mut self.diagnostics,
            self.lines.point_at(start),
            String::from("Syntax error: unclosed `'...'` string literal"),
        );
        len
    }

    fn scan_triple_single_string(&mut self, start: usize) -> usize {
        let len = self.bytes.len();
        let mut seg_start = start;
        let mut pos = start + 3;
        while pos < len {
            if self.bytes[pos] == b'\\' {
                if seg_start < pos {
                    self.spans.push(seg_start, pos, SyntaxTokenKind::String);
                }
                let esc_end = backslash_escape_end(self.bytes, pos, len);
                self.spans.push(pos, esc_end, SyntaxTokenKind::StringEscape);
                seg_start = esc_end;
                pos = esc_end;
                continue;
            }
            if self.bytes[pos..].starts_with(b"'''") {
                let end = pos + 3;
                if seg_start < end {
                    self.spans.push(seg_start, end, SyntaxTokenKind::String);
                }
                self.enclosing_ranges.push(start..end);
                return end;
            }
            pos += 1;
        }
        if seg_start < len {
            self.spans.push(seg_start, len, SyntaxTokenKind::String);
        }
        push_diagnostic(
            &mut self.diagnostics,
            self.lines.point_at(start),
            String::from("Syntax error: unclosed `'''...'''` string literal"),
        );
        len
    }

    fn scan_double_gstring(&mut self, start: usize) -> usize {
        self.scan_gstring_delimited(
            start,
            start + 1,
            b"\"",
            false,
            "unclosed `\"...\"` GString literal",
        )
    }

    fn scan_triple_gstring(&mut self, start: usize) -> usize {
        self.scan_gstring_delimited(
            start,
            start + 3,
            b"\"\"\"",
            true,
            "unclosed `\"\"\"...\"\"\"` GString literal",
        )
    }

    fn scan_dollar_slashy_string(&mut self, start: usize) -> usize {
        let len = self.bytes.len();
        let mut seg_start = start;
        let mut pos = start + 2;
        while pos < len {
            if self.bytes[pos..].starts_with(b"/$") {
                let end = pos + 2;
                if seg_start < end {
                    self.spans.push(seg_start, end, SyntaxTokenKind::String);
                }
                self.enclosing_ranges.push(start..end);
                return end;
            }
            if self.bytes[pos] == b'$' {
                if self.bytes[pos..].starts_with(b"$$") || self.bytes[pos..].starts_with(b"$/") {
                    if seg_start < pos {
                        self.spans.push(seg_start, pos, SyntaxTokenKind::String);
                    }
                    self.spans.push(pos, pos + 2, SyntaxTokenKind::StringEscape);
                    seg_start = pos + 2;
                    pos += 2;
                    continue;
                }
                if let Some(after) = self.try_scan_gstring_interpolation(pos, &mut seg_start, len) {
                    pos = after;
                    continue;
                }
            }
            pos += 1;
        }
        if seg_start < len {
            self.spans.push(seg_start, len, SyntaxTokenKind::String);
        }
        push_diagnostic(
            &mut self.diagnostics,
            self.lines.point_at(start),
            String::from("Syntax error: unclosed `$/.../$` string literal"),
        );
        len
    }

    fn try_scan_slashy_string(&mut self, start: usize) -> Option<usize> {
        let len = self.bytes.len();
        // Verify a closing `/` exists before concluding this is a slashy string rather than `/`.
        let mut probe = start + 1;
        let mut found_close = false;
        while probe < len {
            let b = self.bytes[probe];
            if b == b'\\' {
                probe = (probe + 2).min(len);
                continue;
            }
            if b == b'/' {
                found_close = true;
                break;
            }
            probe += 1;
        }
        if !found_close {
            return None;
        }

        let mut seg_start = start;
        let mut pos = start + 1;
        while pos < len {
            let b = self.bytes[pos];
            if b == b'\\' {
                if seg_start < pos {
                    self.spans.push(seg_start, pos, SyntaxTokenKind::String);
                }
                let esc_end = (pos + 2).min(len);
                self.spans.push(pos, esc_end, SyntaxTokenKind::StringEscape);
                seg_start = esc_end;
                pos = esc_end;
                continue;
            }
            if b == b'$'
                && let Some(after) = self.try_scan_gstring_interpolation(pos, &mut seg_start, len)
            {
                pos = after;
                continue;
            }
            if b == b'/' {
                let end = pos + 1;
                if seg_start < end {
                    self.spans.push(seg_start, end, SyntaxTokenKind::String);
                }
                self.enclosing_ranges.push(start..end);
                return Some(end);
            }
            pos += 1;
        }
        None
    }

    fn scan_gstring_delimited(
        &mut self,
        start: usize,
        mut pos: usize,
        delim: &[u8],
        multiline: bool,
        unclosed_msg: &str,
    ) -> usize {
        let len = self.bytes.len();
        let mut seg_start = start;
        while pos < len {
            let b = self.bytes[pos];
            if !multiline && (b == b'\n' || b == b'\r') {
                if seg_start < pos {
                    self.spans.push(seg_start, pos, SyntaxTokenKind::String);
                }
                push_diagnostic(
                    &mut self.diagnostics,
                    self.lines.point_at(start),
                    format!("Syntax error: {unclosed_msg}"),
                );
                return pos;
            }
            if b == b'\\' {
                if seg_start < pos {
                    self.spans.push(seg_start, pos, SyntaxTokenKind::String);
                }
                let esc_end = backslash_escape_end(self.bytes, pos, len);
                self.spans.push(pos, esc_end, SyntaxTokenKind::StringEscape);
                seg_start = esc_end;
                pos = esc_end;
                continue;
            }
            if b == b'$'
                && let Some(after) = self.try_scan_gstring_interpolation(pos, &mut seg_start, len)
            {
                pos = after;
                continue;
            }
            if self.bytes[pos..].starts_with(delim) {
                let end = pos + delim.len();
                if seg_start < end {
                    self.spans.push(seg_start, end, SyntaxTokenKind::String);
                }
                self.enclosing_ranges.push(start..end);
                return end;
            }
            pos += 1;
        }
        if seg_start < len {
            self.spans.push(seg_start, len, SyntaxTokenKind::String);
        }
        push_diagnostic(
            &mut self.diagnostics,
            self.lines.point_at(start),
            format!("Syntax error: {unclosed_msg}"),
        );
        len
    }

    fn try_scan_gstring_interpolation(
        &mut self,
        dollar_pos: usize,
        seg_start: &mut usize,
        limit: usize,
    ) -> Option<usize> {
        if dollar_pos + 1 >= limit {
            return None;
        }
        let next = self.bytes[dollar_pos + 1];
        if next == b'{' {
            if *seg_start < dollar_pos {
                self.spans
                    .push(*seg_start, dollar_pos, SyntaxTokenKind::String);
            }
            self.spans
                .push(dollar_pos, dollar_pos + 2, SyntaxTokenKind::StringEscape);
            let mut pos = dollar_pos + 2;
            let mut depth = 1usize;
            let expr_start = pos;
            while pos < limit && depth > 0 {
                match self.bytes[pos] {
                    b'{' => depth += 1,
                    b'}' => depth -= 1,
                    b'\'' => {
                        pos += 1;
                        while pos < limit && self.bytes[pos] != b'\'' {
                            if self.bytes[pos] == b'\\' {
                                pos += 1;
                            }
                            pos += 1;
                        }
                    }
                    b'"' => {
                        pos += 1;
                        while pos < limit && self.bytes[pos] != b'"' {
                            if self.bytes[pos] == b'\\' {
                                pos += 1;
                            }
                            pos += 1;
                        }
                    }
                    _ => {}
                }
                if depth > 0 {
                    pos += 1;
                }
            }
            if depth == 0 {
                self.emit_interpolation_expr(expr_start, pos);
                self.spans.push(pos, pos + 1, SyntaxTokenKind::StringEscape);
                let after = pos + 1;
                *seg_start = after;
                return Some(after);
            }
            *seg_start = limit;
            return Some(limit);
        }

        if is_ident_start(next) {
            if *seg_start < dollar_pos {
                self.spans
                    .push(*seg_start, dollar_pos, SyntaxTokenKind::String);
            }
            self.spans
                .push(dollar_pos, dollar_pos + 1, SyntaxTokenKind::StringEscape);
            let mut pos = dollar_pos + 1;
            let first_end = scan_ident_end(self.bytes, pos, limit);
            let first_name = &self.source[pos..first_end];
            let first_kind = if first_name == "it" {
                SyntaxTokenKind::Parameter
            } else {
                SyntaxTokenKind::Variable
            };
            self.spans.push(pos, first_end, first_kind);
            pos = first_end;
            while pos + 1 < limit && self.bytes[pos] == b'.' && is_ident_start(self.bytes[pos + 1])
            {
                self.spans.push(pos, pos + 1, SyntaxTokenKind::Punctuation);
                let prop_start = pos + 1;
                let prop_end = scan_ident_end(self.bytes, prop_start, limit);
                self.spans
                    .push(prop_start, prop_end, SyntaxTokenKind::Property);
                pos = prop_end;
            }
            *seg_start = pos;
            return Some(pos);
        }

        None
    }

    fn emit_interpolation_expr(&mut self, start: usize, end: usize) {
        let mut pos = start;
        let mut after_dot = false;
        while pos < end {
            let b = self.bytes[pos];
            if b.is_ascii_whitespace() {
                pos += 1;
                continue;
            }
            if is_ident_start(b) {
                let ident_end = scan_ident_end(self.bytes, pos, end);
                let ident = &self.source[pos..ident_end];
                let after_ws = skip_inline_ws(self.bytes, ident_end, end);
                let kind = if after_ws < end && self.bytes[after_ws] == b'(' {
                    SyntaxTokenKind::Function
                } else if after_dot {
                    SyntaxTokenKind::Property
                } else if ident == "it" {
                    SyntaxTokenKind::Parameter
                } else if matches!(ident, "this" | "super") {
                    SyntaxTokenKind::Variable
                } else if matches!(ident, "true" | "false" | "null") {
                    SyntaxTokenKind::Boolean
                } else if is_pascal_case(ident) {
                    SyntaxTokenKind::Type
                } else if is_upper_snake(ident) {
                    SyntaxTokenKind::Constant
                } else {
                    SyntaxTokenKind::Variable
                };
                self.spans.push(pos, ident_end, kind);
                after_dot = false;
                pos = ident_end;
                continue;
            }
            if b.is_ascii_digit() {
                let num_end = scan_number_end(self.bytes, pos, end);
                self.spans.push(pos, num_end, SyntaxTokenKind::Number);
                after_dot = false;
                pos = num_end;
                continue;
            }
            if matches!(b, b'.' | b',' | b'(' | b')' | b'[' | b']') {
                self.spans.push(pos, pos + 1, SyntaxTokenKind::Punctuation);
                after_dot = b == b'.';
                pos += 1;
                continue;
            }
            self.spans.push(pos, pos + 1, SyntaxTokenKind::Operator);
            after_dot = false;
            pos += 1;
        }
    }

    fn scan_annotation(&mut self, start: usize) -> usize {
        let len = self.bytes.len();
        let mut pos = scan_ident_end(self.bytes, start + 1, len);
        while pos + 1 < len && self.bytes[pos] == b'.' && is_ident_start(self.bytes[pos + 1]) {
            pos = scan_ident_end(self.bytes, pos + 1, len);
        }
        self.spans.push(start, pos, SyntaxTokenKind::Annotation);
        self.can_start_slashy = false;
        self.at_statement_start = false;
        self.after_dot = false;
        pos
    }

    fn scan_number(&mut self, start: usize) -> usize {
        let end = scan_number_end(self.bytes, start, self.bytes.len());
        self.spans.push(start, end, SyntaxTokenKind::Number);
        end
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Cohesive Groovy keyword, DSL, and identifier classification"
    )]
    fn scan_identifier(&mut self, start: usize) -> usize {
        let len = self.bytes.len();
        let end = scan_ident_end(self.bytes, start, len);
        let ident = &self.source[start..end];
        let line = self.lines.line_at(start);

        // Handle top-level `import` grouping for FoldKind::Imports.
        if ident == "import" && self.brackets.is_empty() && self.at_statement_start {
            let stmt_end = scan_statement_end_on_line(self.bytes, end, len);
            let end_line = self.lines.line_at(stmt_end.saturating_sub(1));
            match &mut self.import_run {
                Some(run) if line <= run.end_line + 2 => {
                    run.end_byte = stmt_end;
                    run.end_line = end_line;
                    run.count += 1;
                }
                _ => {
                    self.flush_import_run();
                    self.import_run = Some(ImportRun {
                        start_byte: start,
                        start_line: line,
                        end_byte: stmt_end,
                        end_line,
                        count: 1,
                    });
                }
            }
        } else if self.brackets.is_empty() && self.at_statement_start && ident != "import" {
            self.flush_import_run();
        }

        // Track modifiers (`public`, `private`, `protected`, `static`, `abstract`).
        match ident {
            "public" => {
                self.mods.visibility = Some(SymbolVisibility::Public);
                self.mods.note_start(start);
            }
            "protected" => {
                self.mods.visibility = Some(SymbolVisibility::Protected);
                self.mods.note_start(start);
            }
            "private" => {
                self.mods.visibility = Some(SymbolVisibility::Private);
                self.mods.note_start(start);
            }
            "static" => {
                self.mods.is_static = true;
                self.mods.note_start(start);
            }
            "abstract" => {
                self.mods.is_abstract = true;
                self.mods.note_start(start);
            }
            "def" | "void" | "final" | "synchronized" | "transient" | "volatile" | "native" => {
                self.mods.note_start(start);
                if ident == "def" {
                    self.pending_def = Some(start);
                }
            }
            _ => {}
        }

        // Check lookahead past horizontal whitespace for `(`, `{`, `:`, or command-arg.
        let after_ws = skip_inline_ws(self.bytes, end, len);
        let next_byte = self.bytes.get(after_ws).copied();
        let next_is_colon = next_byte == Some(b':')
            && self.bytes.get(after_ws + 1).copied() != Some(b':')
            && !self.in_ternary_question;
        let next_is_paren = next_byte == Some(b'(');
        let next_is_brace = next_byte == Some(b'{');
        let is_command_call = !self.after_dot
            && self.at_statement_start
            && !is_any_keyword(ident)
            && !next_is_colon
            && is_groovy_command_arg_ahead(self.bytes, after_ws, len);

        let kind = if let Some(kw_kind) = classify_groovy_keyword(ident) {
            if let Some(type_kind) = match ident {
                "class" | "trait" => Some(StructureSymbolKind::Class),
                "interface" => Some(StructureSymbolKind::Interface),
                "enum" => Some(StructureSymbolKind::Enum),
                _ => None,
            } {
                self.pending_type_kw = Some((type_kind, self.mods.start_byte.unwrap_or(start)));
            } else if !matches!(
                ident,
                "public"
                    | "private"
                    | "protected"
                    | "static"
                    | "final"
                    | "abstract"
                    | "synchronized"
                    | "native"
                    | "strictfp"
                    | "transient"
                    | "volatile"
                    | "def"
                    | "var"
                    | "void"
                    | "boolean"
                    | "byte"
                    | "short"
                    | "int"
                    | "long"
                    | "float"
                    | "double"
                    | "char"
                    | "extends"
                    | "implements"
                    | "throws"
            ) {
                self.pending_decl = None;
            }
            self.can_start_slashy = matches!(
                ident,
                "return" | "throw" | "case" | "assert" | "in" | "else" | "do"
            );
            kw_kind
        } else if next_is_colon {
            self.can_start_slashy = true;
            SyntaxTokenKind::Property
        } else if let Some((type_kind, decl_start)) = self.pending_type_kw.take() {
            let vis = self.mods.visibility.unwrap_or(SymbolVisibility::Public);
            let is_abs = self.mods.is_abstract;
            self.pending_decl = Some(PendingDecl::Type {
                kind: type_kind,
                name: compact_text(ident, 128),
                selection_range: start..end,
                start_byte: decl_start,
                visibility: vis,
                is_abstract: is_abs,
            });
            self.mods.clear();
            self.can_start_slashy = false;
            SyntaxTokenKind::Type
        } else if let Some((pdepth, true)) = self.param_state
            && pdepth == self.brackets.len()
        {
            // Inside method parameter list `(Type param, param2)`:
            // If followed by `,`, `)`, or `=`, this identifier is the parameter name.
            if matches!(next_byte, Some(b',' | b')' | b'=')) {
                self.param_state = Some((pdepth, false));
                self.can_start_slashy = false;
                SyntaxTokenKind::Parameter
            } else if is_pascal_case(ident) {
                self.can_start_slashy = false;
                SyntaxTokenKind::Type
            } else {
                self.can_start_slashy = false;
                SyntaxTokenKind::Variable
            }
        } else if next_is_paren || next_is_brace || is_command_call {
            if next_is_paren {
                if self.pending_def.is_some()
                    || (!self.after_dot
                        && (self.mods.start_byte.is_some() || self.last_ident.is_some())
                        && self.brackets.last().is_none_or(|b| {
                            b.struct_frame.as_ref().is_some_and(|sf| {
                                matches!(
                                    sf.kind,
                                    StructureSymbolKind::Class
                                        | StructureSymbolKind::Interface
                                        | StructureSymbolKind::Enum
                                )
                            })
                        }))
                {
                    let decl_start = self
                        .pending_def
                        .take()
                        .or(self.mods.start_byte)
                        .unwrap_or(start);
                    let vis = self.mods.visibility.unwrap_or(SymbolVisibility::Public);
                    let is_stat = self.mods.is_static;
                    let is_abs = self.mods.is_abstract;
                    self.pending_decl = Some(PendingDecl::Callable {
                        name: compact_text(ident, 128),
                        selection_range: start..end,
                        start_byte: decl_start,
                        visibility: vis,
                        is_static: is_stat,
                        is_abstract: is_abs,
                        detail: None,
                    });
                    self.mods.clear();
                } else if self.brackets.is_empty() {
                    // Top-level DSL call like `tasks.named('test') { ... }`
                    let name = if self.after_dot
                        && let Some((prev_ident, _)) = &self.last_ident
                    {
                        compact_text(&format!("{prev_ident}.{ident}"), 128)
                    } else {
                        compact_text(ident, 128)
                    };
                    self.pending_decl = Some(PendingDecl::DslSection {
                        name,
                        selection_range: start..end,
                        start_byte: start,
                    });
                }
            } else if next_is_brace
                && !self.after_dot
                && self.brackets.is_empty()
                && self.pending_decl.is_none()
            {
                self.pending_decl = Some(PendingDecl::DslSection {
                    name: compact_text(ident, 128),
                    selection_range: start..end,
                    start_byte: start,
                });
            }
            self.can_start_slashy = false;
            SyntaxTokenKind::Function
        } else if self.after_dot {
            self.can_start_slashy = false;
            if is_pascal_case(ident) {
                SyntaxTokenKind::Type
            } else if is_upper_snake(ident) {
                SyntaxTokenKind::Constant
            } else {
                SyntaxTokenKind::Property
            }
        } else {
            // Check for field declaration directly inside a class/interface/enum body.
            self.maybe_emit_class_field(ident, start, end, next_byte);
            self.can_start_slashy = false;
            if is_pascal_case(ident) {
                SyntaxTokenKind::Type
            } else if is_upper_snake(ident) {
                SyntaxTokenKind::Constant
            } else {
                SyntaxTokenKind::Variable
            }
        };

        self.spans.push(start, end, kind);
        self.last_ident = Some((ident.to_owned(), start..end));
        self.at_statement_start = false;
        self.after_dot = false;
        end
    }

    fn maybe_emit_class_field(
        &mut self,
        ident: &str,
        start: usize,
        end: usize,
        next_byte: Option<u8>,
    ) {
        let Some(top_frame) = self.brackets.last_mut() else {
            return;
        };
        let Some(class_sf) = top_frame.struct_frame.as_mut() else {
            return;
        };
        if !matches!(
            class_sf.kind,
            StructureSymbolKind::Class | StructureSymbolKind::Interface | StructureSymbolKind::Enum
        ) {
            return;
        }
        // Only treat as field/enum-constant when terminated by `=`, `;`, `,`, `}`, or newline/EOF
        // (so type names before a field name, like `String name`, are skipped).
        let is_terminated = matches!(
            next_byte,
            None | Some(b'=' | b';' | b',' | b'}' | b'\n' | b'\r')
        );
        if !is_terminated {
            return;
        }
        let has_field_intro = self.mods.start_byte.is_some()
            || self.pending_def.is_some()
            || self.last_ident.is_some();
        let is_enum_const = class_sf.kind == StructureSymbolKind::Enum
            && matches!(next_byte, Some(b',' | b';' | b'}' | b'\n' | b'\r'));
        if !has_field_intro && !is_enum_const {
            return;
        }
        let kind = if is_enum_const && self.mods.start_byte.is_none() && self.pending_def.is_none()
        {
            StructureSymbolKind::EnumConstant
        } else {
            StructureSymbolKind::Field
        };
        let decl_start = self
            .pending_def
            .take()
            .or(self.mods.start_byte)
            .unwrap_or(start);
        let vis = self.mods.visibility.unwrap_or(SymbolVisibility::Public);
        let is_stat = self.mods.is_static || kind == StructureSymbolKind::EnumConstant;
        let is_abs = self.mods.is_abstract;
        self.mods.clear();

        let node = StructureNode {
            name: compact_text(ident, 128),
            detail: None,
            kind,
            visibility: vis,
            is_static: is_stat,
            is_abstract: is_abs,
            full_range: decl_start..end,
            selection_range: start..end,
            start_point: self.lines.point_at(decl_start),
            end_point: self.lines.point_at(end),
            selection_point: self.lines.point_at(start),
            children: Vec::new(),
        };
        class_sf.children.push(node);
    }

    fn scan_open_bracket(&mut self, pos: usize, b: u8) -> usize {
        self.spans.push(pos, pos + 1, SyntaxTokenKind::Punctuation);
        let line = self.lines.line_at(pos);
        match b {
            b'(' => {
                if matches!(self.pending_decl, Some(PendingDecl::Callable { .. })) {
                    self.param_state = Some((self.brackets.len() + 1, true));
                }
                self.brackets.push(BracketFrame {
                    kind: BracketKind::Paren,
                    open_byte: pos,
                    open_line: line,
                    fold_kind: FoldKind::Block,
                    struct_frame: None,
                });
            }
            b'[' => {
                self.brackets.push(BracketFrame {
                    kind: BracketKind::Bracket,
                    open_byte: pos,
                    open_line: line,
                    fold_kind: FoldKind::Block,
                    struct_frame: None,
                });
            }
            b'{' => {
                let (fold_kind, struct_frame) = self.build_brace_struct_frame();
                self.mods.clear();
                self.pending_def = None;
                self.brackets.push(BracketFrame {
                    kind: BracketKind::Brace,
                    open_byte: pos,
                    open_line: line,
                    fold_kind,
                    struct_frame,
                });
            }
            _ => {}
        }

        self.can_start_slashy = true;
        self.at_statement_start = b == b'{';
        self.after_dot = false;
        pos + 1
    }

    fn build_brace_struct_frame(&mut self) -> (FoldKind, Option<GroovyStructFrame>) {
        match self.pending_decl.take() {
            Some(PendingDecl::Type {
                kind,
                name,
                selection_range,
                start_byte,
                visibility,
                is_abstract,
            }) => (
                FoldKind::Type,
                Some(GroovyStructFrame {
                    name,
                    detail: None,
                    kind,
                    visibility,
                    is_static: false,
                    is_abstract,
                    start_byte,
                    selection_range,
                    children: Vec::new(),
                }),
            ),
            Some(PendingDecl::Callable {
                name,
                selection_range,
                start_byte,
                visibility,
                is_static,
                is_abstract,
                detail,
            }) => {
                let in_type = self.brackets.iter().any(|f| {
                    f.struct_frame.as_ref().is_some_and(|sf| {
                        matches!(
                            sf.kind,
                            StructureSymbolKind::Class
                                | StructureSymbolKind::Interface
                                | StructureSymbolKind::Enum
                        )
                    })
                });
                let skind = if in_type {
                    StructureSymbolKind::Method
                } else {
                    StructureSymbolKind::Function
                };
                (
                    FoldKind::Function,
                    Some(GroovyStructFrame {
                        name,
                        detail,
                        kind: skind,
                        visibility,
                        is_static,
                        is_abstract,
                        start_byte,
                        selection_range,
                        children: Vec::new(),
                    }),
                )
            }
            Some(PendingDecl::DslSection {
                name,
                selection_range,
                start_byte,
            }) => (
                FoldKind::Block,
                Some(GroovyStructFrame {
                    name,
                    detail: None,
                    kind: StructureSymbolKind::Section,
                    visibility: SymbolVisibility::Unspecified,
                    is_static: false,
                    is_abstract: false,
                    start_byte,
                    selection_range,
                    children: Vec::new(),
                }),
            ),
            None => (FoldKind::Block, None),
        }
    }

    fn scan_close_bracket(&mut self, pos: usize, b: u8) -> usize {
        self.spans.push(pos, pos + 1, SyntaxTokenKind::Punctuation);
        let close_end = pos + 1;
        let close_line = self.lines.line_at(pos);
        let expected = BracketKind::from_close_byte(b);

        if let Some(expected_kind) = expected {
            if let Some(match_idx) = self.brackets.iter().rposition(|f| f.kind == expected_kind) {
                while self.brackets.len() > match_idx + 1 {
                    if let Some(unclosed) = self.brackets.pop() {
                        push_diagnostic(
                            &mut self.diagnostics,
                            self.lines.point_at(unclosed.open_byte),
                            format!("Syntax error: unclosed `{}`", unclosed.kind.open_char()),
                        );
                        self.finish_struct_frame(unclosed.struct_frame, pos);
                    }
                }
                if let Some(frame) = self.brackets.pop() {
                    self.enclosing_ranges.push(frame.open_byte..close_end);
                    if frame.kind == BracketKind::Paren
                        && self
                            .param_state
                            .is_some_and(|(d, _)| d == self.brackets.len() + 1)
                    {
                        self.param_state = None;
                        if let Some(PendingDecl::Callable { detail, .. }) = &mut self.pending_decl {
                            let raw_params = &self.source[frame.open_byte..close_end];
                            *detail = Some(compact_text(raw_params, 128));
                        }
                    }

                    if close_line > frame.open_line
                        && matches!(frame.kind, BracketKind::Brace | BracketKind::Bracket)
                    {
                        let placeholder = if frame.kind == BracketKind::Bracket {
                            "[...]"
                        } else {
                            "{...}"
                        };
                        self.folds.push(FoldRange {
                            start_line: frame.open_line,
                            end_line: close_line,
                            collapsed_byte_range: frame.open_byte..close_end,
                            kind: frame.fold_kind,
                            default_collapsed: false,
                            placeholder,
                        });
                    }
                    self.finish_struct_frame(frame.struct_frame, close_end);
                }
            } else {
                push_diagnostic(
                    &mut self.diagnostics,
                    self.lines.point_at(pos),
                    format!("Syntax error: unexpected `{}`", char::from(b)),
                );
            }
        }

        self.can_start_slashy = false;
        self.at_statement_start = b == b'}';
        self.after_dot = false;
        close_end
    }

    fn finish_struct_frame(&mut self, sf: Option<GroovyStructFrame>, end_byte: usize) {
        let Some(sf) = sf else {
            return;
        };
        let depth = self
            .brackets
            .iter()
            .filter(|f| f.struct_frame.is_some())
            .count();
        if depth >= MAX_STRUCTURE_DEPTH {
            return;
        }
        let full_end = end_byte.max(sf.start_byte);
        self.enclosing_ranges.push(sf.start_byte..full_end);
        let node = StructureNode {
            name: sf.name,
            detail: sf.detail,
            kind: sf.kind,
            visibility: sf.visibility,
            is_static: sf.is_static,
            is_abstract: sf.is_abstract,
            full_range: sf.start_byte..full_end,
            selection_range: sf.selection_range.clone(),
            start_point: self.lines.point_at(sf.start_byte),
            end_point: self.lines.point_at(full_end),
            selection_point: self.lines.point_at(sf.selection_range.start),
            children: sf.children,
        };
        for frame in self.brackets.iter_mut().rev() {
            if let Some(parent_sf) = &mut frame.struct_frame {
                parent_sf.children.push(node);
                return;
            }
        }
        self.struct_roots.push(node);
    }

    fn scan_operator_or_punctuation(&mut self, pos: usize) -> usize {
        if let Some(end) = self.try_scan_multi_char_op(pos) {
            return end;
        }

        let b = self.bytes[pos];
        if matches!(b, b',' | b'.' | b';' | b':') {
            self.spans.push(pos, pos + 1, SyntaxTokenKind::Punctuation);
            if b == b','
                && let Some((d, _)) = self.param_state
                && d == self.brackets.len()
            {
                self.param_state = Some((d, true));
            }
            if b == b';' {
                self.mods.clear();
                self.pending_def = None;
                self.pending_type_kw = None;
                self.pending_decl = None;
                self.in_ternary_question = false;
            }
            if b == b':' && self.in_ternary_question {
                self.in_ternary_question = false;
            }
            self.can_start_slashy = matches!(b, b',' | b';' | b':');
            self.at_statement_start = b == b';';
            self.after_dot = b == b'.';
            return pos + 1;
        }

        if b == b'?' {
            self.spans.push(pos, pos + 1, SyntaxTokenKind::Operator);
            self.in_ternary_question = true;
            self.can_start_slashy = true;
            self.at_statement_start = false;
            self.after_dot = false;
            return pos + 1;
        }

        if matches!(
            b,
            b'=' | b'+'
                | b'-'
                | b'*'
                | b'/'
                | b'%'
                | b'<'
                | b'>'
                | b'!'
                | b'~'
                | b'&'
                | b'|'
                | b'^'
        ) {
            if b == b'=' {
                self.pending_decl = None;
                self.mods.clear();
                self.pending_def = None;
            }
            self.spans.push(pos, pos + 1, SyntaxTokenKind::Operator);
            self.can_start_slashy = true;
            self.at_statement_start = false;
            self.after_dot = false;
            return pos + 1;
        }

        pos + 1
    }

    fn try_scan_multi_char_op(&mut self, pos: usize) -> Option<usize> {
        const OPS: [&[u8]; 34] = [
            b"==~", b"<=>", b"===", b"!==", b">>>", b"=~", b"?.", b"?:", b"*.", b".&", b".@",
            b"->", b"..<", b"..", b"<<", b">>", b"**", b"++", b"--", b"&&", b"||", b"==", b"!=",
            b"<=", b">=", b"+=", b"-=", b"*=", b"/=", b"%=", b"&=", b"|=", b"^=", b"::",
        ];
        let rest = &self.bytes[pos..];
        let op = OPS.into_iter().find(|op| rest.starts_with(op))?;
        let end = pos + op.len();
        let kind = if op == b"::" {
            SyntaxTokenKind::Punctuation
        } else {
            SyntaxTokenKind::Operator
        };
        self.spans.push(pos, end, kind);
        self.can_start_slashy = !matches!(op, b"++" | b"--");
        self.at_statement_start = false;
        self.after_dot = matches!(op, b"?." | b"*.");
        Some(end)
    }

    fn flush_import_run(&mut self) {
        if let Some(run) = self.import_run.take()
            && run.count >= 2
            && run.end_line > run.start_line
            && run.start_byte < run.end_byte
        {
            self.folds.push(FoldRange {
                start_line: run.start_line,
                end_line: run.end_line,
                collapsed_byte_range: run.start_byte..run.end_byte,
                kind: FoldKind::Imports,
                default_collapsed: true,
                placeholder: "import ...",
            });
        }
    }

    fn finish(&mut self) {
        self.flush_import_run();
        let len = self.bytes.len();
        while let Some(frame) = self.brackets.pop() {
            push_diagnostic(
                &mut self.diagnostics,
                self.lines.point_at(frame.open_byte),
                format!("Syntax error: unclosed `{}`", frame.kind.open_char()),
            );
            self.finish_struct_frame(frame.struct_frame, len);
        }
    }
}

fn skip_inline_ws(bytes: &[u8], mut pos: usize, limit: usize) -> usize {
    while pos < limit && matches!(bytes[pos], b' ' | b'\t') {
        pos += 1;
    }
    pos
}

fn scan_statement_end_on_line(bytes: &[u8], mut pos: usize, limit: usize) -> usize {
    while pos < limit && !matches!(bytes[pos], b'\n' | b'\r' | b';') {
        pos += 1;
    }
    if pos < limit && bytes[pos] == b';' {
        pos += 1;
    }
    pos
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn scan_ident_end(bytes: &[u8], mut pos: usize, limit: usize) -> usize {
    while pos < limit && is_ident_continue(bytes[pos]) {
        pos += 1;
    }
    pos
}

fn scan_number_end(bytes: &[u8], start: usize, limit: usize) -> usize {
    let mut pos = start;
    if bytes[pos..].starts_with(b"0x") || bytes[pos..].starts_with(b"0X") {
        pos += 2;
        while pos < limit && (bytes[pos].is_ascii_hexdigit() || bytes[pos] == b'_') {
            pos += 1;
        }
    } else if bytes[pos..].starts_with(b"0b") || bytes[pos..].starts_with(b"0B") {
        pos += 2;
        while pos < limit && matches!(bytes[pos], b'0' | b'1' | b'_') {
            pos += 1;
        }
    } else {
        while pos < limit && (bytes[pos].is_ascii_digit() || bytes[pos] == b'_') {
            pos += 1;
        }
        if pos + 1 < limit && bytes[pos] == b'.' && bytes[pos + 1].is_ascii_digit() {
            pos += 1;
            while pos < limit && (bytes[pos].is_ascii_digit() || bytes[pos] == b'_') {
                pos += 1;
            }
        }
        if pos < limit && matches!(bytes[pos], b'e' | b'E') {
            let exp_start = pos;
            pos += 1;
            if pos < limit && matches!(bytes[pos], b'+' | b'-') {
                pos += 1;
            }
            if pos < limit && bytes[pos].is_ascii_digit() {
                while pos < limit && (bytes[pos].is_ascii_digit() || bytes[pos] == b'_') {
                    pos += 1;
                }
            } else {
                pos = exp_start;
            }
        }
    }

    if pos < limit
        && matches!(
            bytes[pos],
            b'L' | b'l' | b'G' | b'g' | b'D' | b'd' | b'F' | b'f' | b'I' | b'i'
        )
    {
        pos += 1;
    }
    pos
}

fn backslash_escape_end(bytes: &[u8], backslash_pos: usize, limit: usize) -> usize {
    if backslash_pos + 1 >= limit {
        return limit;
    }
    if bytes[backslash_pos + 1] == b'u' {
        (backslash_pos + 6).min(limit)
    } else {
        backslash_pos + 2
    }
}

fn is_groovy_command_arg_ahead(bytes: &[u8], pos: usize, limit: usize) -> bool {
    if pos >= limit {
        return false;
    }
    let b = bytes[pos];
    if matches!(b, b'\'' | b'"') || b.is_ascii_digit() {
        return true;
    }
    if is_ident_start(b) {
        let end = scan_ident_end(bytes, pos, limit);
        let after = skip_inline_ws(bytes, end, limit);
        return bytes.get(after).copied() == Some(b':');
    }
    false
}

fn classify_groovy_keyword(ident: &str) -> Option<SyntaxTokenKind> {
    match ident {
        "package" | "import" | "as" | "class" | "interface" | "trait" | "enum" | "def" | "var"
        | "public" | "private" | "protected" | "static" | "final" | "abstract" | "synchronized"
        | "native" | "strictfp" | "transient" | "volatile" | "extends" | "implements"
        | "throws" | "new" | "in" | "instanceof" | "const" | "goto" => {
            Some(SyntaxTokenKind::Keyword)
        }
        "if" | "else" | "for" | "while" | "do" | "switch" | "case" | "default" | "break"
        | "continue" | "return" | "try" | "catch" | "finally" | "throw" | "assert" => {
            Some(SyntaxTokenKind::KeywordControl)
        }
        "void" | "boolean" | "byte" | "short" | "int" | "long" | "float" | "double" | "char" => {
            Some(SyntaxTokenKind::TypeBuiltin)
        }
        "true" | "false" | "null" => Some(SyntaxTokenKind::Boolean),
        "this" | "super" => Some(SyntaxTokenKind::Variable),
        "it" => Some(SyntaxTokenKind::Parameter),
        _ => None,
    }
}

fn is_any_keyword(ident: &str) -> bool {
    classify_groovy_keyword(ident).is_some()
}

fn is_pascal_case(ident: &str) -> bool {
    let mut chars = ident.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_uppercase() && chars.any(|c| c.is_ascii_lowercase())
}

fn is_upper_snake(ident: &str) -> bool {
    ident.len() >= 2
        && ident.bytes().any(|b| b.is_ascii_uppercase())
        && ident
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}
