//! Deterministic single-pass SQL scanner (`.sql`, `.ddl`, `.psql`, `.hql`):
//! highlighting, folding, structure outline, `Alt+Up` enclosing ranges, and diagnostics.

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

// Strictly ASCII-sorted uppercase keyword/type/function tables for zero-allocation binary search.

const KEYWORDS_CONTROL: &[&str] = &[
    "CASE",
    "CATCH",
    "CONTINUE",
    "ELSE",
    "ELSEIF",
    "ELSIF",
    "END",
    "EXCEPTION",
    "EXIT",
    "FOR",
    "IF",
    "LOOP",
    "RAISE",
    "RETURN",
    "THEN",
    "THROW",
    "TRY",
    "WHEN",
    "WHILE",
];

const KEYWORDS_BOOLEAN: &[&str] = &["FALSE", "NULL", "TRUE", "UNKNOWN"];

const KEYWORDS: &[&str] = &[
    "ADD",
    "ALL",
    "ALTER",
    "ALWAYS",
    "ANALYZE",
    "AND",
    "ANY",
    "AS",
    "ASC",
    "AUTO_INCREMENT",
    "BEGIN",
    "BETWEEN",
    "BY",
    "CASCADE",
    "CAST",
    "CHECK",
    "CLOSE",
    "COLLATE",
    "COLUMN",
    "COMMIT",
    "CONCURRENTLY",
    "CONFLICT",
    "CONSTRAINT",
    "CREATE",
    "CROSS",
    "CURRENT",
    "CURSOR",
    "DATABASE",
    "DEALLOCATE",
    "DECLARE",
    "DEFAULT",
    "DEFINER",
    "DELETE",
    "DELIMITED",
    "DESC",
    "DESCRIBE",
    "DISTINCT",
    "DO",
    "DOMAIN",
    "DROP",
    "ENUM",
    "ESCAPE",
    "EXCEPT",
    "EXCLUDE",
    "EXCLUDED",
    "EXECUTE",
    "EXISTS",
    "EXPLAIN",
    "EXTENSION",
    "EXTERNAL",
    "FETCH",
    "FIELDS",
    "FILTER",
    "FIRST",
    "FOLLOWING",
    "FOREIGN",
    "FORMAT",
    "FROM",
    "FULL",
    "FUNCTION",
    "GENERATED",
    "GRANT",
    "GROUP",
    "HAVING",
    "IDENTITY",
    "ILIKE",
    "IMMUTABLE",
    "IN",
    "INDEX",
    "INNER",
    "INSERT",
    "INTERSECT",
    "INTO",
    "INVOKER",
    "IS",
    "ISOLATION",
    "JOIN",
    "KEY",
    "LANGUAGE",
    "LAST",
    "LATERAL",
    "LEFT",
    "LIMIT",
    "LOCATION",
    "LOCK",
    "MATERIALIZED",
    "MERGE",
    "NATURAL",
    "NEXT",
    "NOT",
    "NOTHING",
    "NULLS",
    "OFFSET",
    "ON",
    "ONLY",
    "OPEN",
    "OR",
    "ORDER",
    "OUTER",
    "OVER",
    "PARTITION",
    "PARTITIONED",
    "PERFORM",
    "PRECEDING",
    "PREPARE",
    "PRIMARY",
    "PRIVILEGES",
    "PROCEDURE",
    "RANGE",
    "RECURSIVE",
    "REFERENCES",
    "RELEASE",
    "RENAME",
    "REPLACE",
    "RESTRICT",
    "RETURNING",
    "RETURNS",
    "REVOKE",
    "RIGHT",
    "ROLLBACK",
    "ROW",
    "ROWS",
    "SAVEPOINT",
    "SCHEMA",
    "SECURITY",
    "SELECT",
    "SEQUENCE",
    "SERDE",
    "SET",
    "SHOW",
    "SIMILAR",
    "SOME",
    "STABLE",
    "START",
    "STORED",
    "TABLE",
    "TEMP",
    "TEMPORARY",
    "TERMINATED",
    "TO",
    "TRANSACTION",
    "TRIGGER",
    "TRUNCATE",
    "TYPE",
    "UNBOUNDED",
    "UNION",
    "UNIQUE",
    "UPDATE",
    "USE",
    "USING",
    "VACUUM",
    "VALUES",
    "VIEW",
    "VOLATILE",
    "WHERE",
    "WINDOW",
    "WITH",
    "WITHIN",
    "WORK",
];

const TYPE_BUILTINS: &[&str] = &[
    "ARRAY",
    "BIGINT",
    "BIGSERIAL",
    "BINARY",
    "BIT",
    "BLOB",
    "BOOL",
    "BOOLEAN",
    "BYTEA",
    "CHAR",
    "CHARACTER",
    "CIDR",
    "CLOB",
    "DATE",
    "DATETIME",
    "DEC",
    "DECIMAL",
    "DOUBLE",
    "FLOAT",
    "FLOAT4",
    "FLOAT8",
    "INET",
    "INT",
    "INT2",
    "INT4",
    "INT8",
    "INTEGER",
    "INTERVAL",
    "JSON",
    "JSONB",
    "MACADDR",
    "MAP",
    "MEDIUMINT",
    "MONEY",
    "NCHAR",
    "NUMBER",
    "NUMERIC",
    "NVARCHAR",
    "OID",
    "PRECISION",
    "REAL",
    "RECORD",
    "SERIAL",
    "SETOF",
    "SMALLINT",
    "SMALLSERIAL",
    "STRING",
    "STRUCT",
    "TEXT",
    "TIME",
    "TIMESTAMP",
    "TIMESTAMPTZ",
    "TIMETZ",
    "TINYINT",
    "UUID",
    "VARBINARY",
    "VARCHAR",
    "VARYING",
    "VOID",
    "XML",
    "YEAR",
];

const NO_PAREN_FUNCTIONS: &[&str] = &[
    "CURRENT_DATE",
    "CURRENT_TIME",
    "CURRENT_TIMESTAMP",
    "LOCALTIME",
    "LOCALTIMESTAMP",
];

const TABLE_CONSTRAINTS: &[&str] = &[
    "CHECK",
    "CONSTRAINT",
    "EXCLUDE",
    "FOREIGN",
    "FULLTEXT",
    "INDEX",
    "KEY",
    "LIKE",
    "PERIOD",
    "PRIMARY",
    "SPATIAL",
    "UNIQUE",
];

const COLUMN_TYPE_STOPS: &[&str] = &[
    "AS",
    "AUTO_INCREMENT",
    "CHECK",
    "COLLATE",
    "COMMENT",
    "CONSTRAINT",
    "DEFAULT",
    "GENERATED",
    "IDENTITY",
    "NOT",
    "NULL",
    "ON",
    "PRIMARY",
    "REFERENCES",
    "UNIQUE",
];

const MULTI_CHAR_OPS: &[&[u8]] = &[
    b"#>>", b"->>", b"!~*", b"!~~", b"<>", b"!=", b"<=", b">=", b"||", b"::", b":=", b"->", b"#>",
    b"@>", b"<@", b"?|", b"?&", b"~*", b"!~", b"~~", b"<<", b">>",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Begin,
    Case,
}

impl BlockKind {
    const fn label(self) -> &'static str {
        match self {
            Self::Begin => "BEGIN",
            Self::Case => "CASE",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct BlockFrame {
    kind: BlockKind,
    start_byte: usize,
    open_line: usize,
    header_end_byte: usize,
}

#[derive(Debug, Clone, Copy)]
struct ParenFrame {
    open_byte: usize,
    open_line: usize,
}

#[derive(Debug, Clone, Copy)]
struct LineCommentRun {
    start_line: usize,
    start_byte: usize,
    last_line: usize,
    last_end: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CreateObjectKind {
    Table,
    View,
    Index,
    Function,
    Procedure,
    Trigger,
    Type,
    Domain,
    Schema,
    Sequence,
}

#[derive(Debug, Clone)]
struct ColumnItem {
    is_constraint: bool,
    name: Option<String>,
    selection_range: Option<Range<usize>>,
    start_byte: usize,
    type_start: Option<usize>,
    type_end: usize,
    type_done: bool,
    end_byte: usize,
}

impl ColumnItem {
    const fn empty(start_byte: usize) -> Self {
        Self {
            is_constraint: false,
            name: None,
            selection_range: None,
            start_byte,
            type_start: None,
            type_end: start_byte,
            type_done: false,
            end_byte: start_byte,
        }
    }
}

#[derive(Debug, Clone)]
struct TableBodyCollector {
    paren_depth: usize,
    current: ColumnItem,
    columns: Vec<StructureNode>,
}

impl TableBodyCollector {
    fn new(paren_depth: usize, open_end: usize) -> Self {
        Self {
            paren_depth,
            current: ColumnItem::empty(open_end),
            columns: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
enum StmtState {
    Start,
    Other,
    Create {
        start_byte: usize,
    },
    CreateObject {
        start_byte: usize,
        kind: CreateObjectKind,
        if_not_exists_stage: u8,
        skip_concurrently: bool,
        saw_index_on: bool,
        name: Option<String>,
        selection_range: Option<Range<usize>>,
        expect_dot_part: bool,
        table_body: Option<TableBodyCollector>,
        params_detail: Option<String>,
        params_depth: Option<usize>,
        enum_stage: u8,
    },
    With {
        expect_cte_name: bool,
        cte_name: Option<String>,
        cte_sel: Option<Range<usize>>,
        cte_start: usize,
        col_list_depth: Option<usize>,
        saw_as: bool,
        body_depth: Option<usize>,
    },
}

struct SqlScanner<'a> {
    source: &'a str,
    bytes: &'a [u8],
    lines: LineIndex,
    spans: SpanBuilder,
    folds: Vec<FoldRange>,
    enclosing_ranges: Vec<Range<usize>>,
    diagnostics: Vec<SyntaxDiagnostic>,
    parens: Vec<ParenFrame>,
    blocks: Vec<BlockFrame>,
    stmt_starts: Vec<Option<usize>>,
    region_stack: Vec<(usize, usize)>,
    comment_run: Option<LineCommentRun>,
    struct_roots: Vec<StructureNode>,
    stmt_state: StmtState,
    after_dot: bool,
    dot_value_prev: bool,
    last_token_end: usize,
}

/// Scans SQL / DDL / `PostgreSQL` / `HiveQL` `source` into a [`ScannedDocument`].
///
/// # Errors
///
/// Propagates cancellation or timeout errors from `budget`.
pub(crate) fn scan(
    source: &str,
    budget: &mut ScanBudget<'_>,
) -> Result<ScannedDocument, SyntaxError> {
    let lines = LineIndex::new(source);
    let mut scanner = SqlScanner {
        source,
        bytes: source.as_bytes(),
        lines,
        spans: SpanBuilder::with_capacity(source.len() / 5 + 16),
        folds: Vec::new(),
        enclosing_ranges: Vec::new(),
        diagnostics: Vec::new(),
        parens: Vec::new(),
        blocks: Vec::new(),
        stmt_starts: vec![None],
        region_stack: Vec::new(),
        comment_run: None,
        struct_roots: Vec::new(),
        stmt_state: StmtState::Start,
        after_dot: false,
        dot_value_prev: false,
        last_token_end: 0,
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

impl SqlScanner<'_> {
    #[allow(clippy::too_many_lines, reason = "Cohesive single-pass SQL lexer loop")]
    fn scan_tokens(&mut self, budget: &mut ScanBudget<'_>) -> Result<(), SyntaxError> {
        let len = self.bytes.len();
        let mut pos = 0usize;
        let mut blank_line_seen = false;

        while pos < len {
            budget.tick()?;
            let b = self.bytes[pos];

            if matches!(b, b' ' | b'\t' | b'\r' | b'\n') {
                if b == b'\n' {
                    if blank_line_seen {
                        self.flush_comment_run();
                    }
                    blank_line_seen = true;
                }
                pos += 1;
                continue;
            }

            // Line comment `-- ...`
            if self.bytes[pos..].starts_with(b"--") {
                let only_on_line = is_line_prefix_ws(self.bytes, &self.lines, pos);
                pos = self.scan_line_comment(pos, only_on_line);
                blank_line_seen = false;
                continue;
            }

            // Block / Doc comment `/* ... */` or `/** ... */`
            if self.bytes[pos..].starts_with(b"/*") {
                self.flush_comment_run();
                blank_line_seen = false;
                pos = self.scan_block_comment(pos);
                continue;
            }

            // Any non-comment token breaks consecutive `--` comment folding runs.
            self.flush_comment_run();
            blank_line_seen = false;
            self.note_stmt_token(pos);

            // PostgreSQL escape string `E'...'` / `e'...'`
            if (b == b'E' || b == b'e') && self.bytes.get(pos + 1) == Some(&b'\'') {
                pos = self.scan_escape_string(pos);
                self.after_dot = false;
                self.dot_value_prev = true;
                continue;
            }

            // Standard string `'...'`
            if b == b'\'' {
                pos = self.scan_single_string(pos);
                self.after_dot = false;
                self.dot_value_prev = true;
                continue;
            }

            // Quoted identifier `"..."` or HiveQL/MySQL `` `...` ``
            if b == b'"' || b == b'`' {
                pos = self.scan_quoted_identifier(pos, b);
                self.after_dot = false;
                self.dot_value_prev = true;
                continue;
            }

            // Dollar-quoted string `$tag$...$tag$` / `$$...$$` or positional `$1`
            if b == b'$' {
                pos = self.scan_dollar(pos);
                continue;
            }

            // Named parameter `:name` vs `::` / `:=`
            if b == b':' {
                pos = self.scan_colon(pos);
                continue;
            }

            // Positional parameter `?` / `?1` vs JSONB `?|` / `?&`
            if b == b'?' {
                pos = self.scan_question(pos);
                continue;
            }

            // Leading-dot fractional number `.5`
            if b == b'.'
                && !self.dot_value_prev
                && pos + 1 < len
                && self.bytes[pos + 1].is_ascii_digit()
            {
                pos = self.scan_number(pos);
                self.after_dot = false;
                self.dot_value_prev = true;
                continue;
            }

            // Numbers
            if b.is_ascii_digit() {
                pos = self.scan_number(pos);
                self.after_dot = false;
                self.dot_value_prev = true;
                continue;
            }

            // Identifiers, keywords, types, functions
            if is_ident_start_byte(self.source, pos) {
                pos = self.scan_identifier(pos);
                continue;
            }

            // Parentheses and brackets
            if b == b'(' {
                pos = self.scan_open_paren(pos);
                continue;
            }
            if b == b')' {
                pos = self.scan_close_paren(pos);
                continue;
            }
            if matches!(b, b'[' | b']') {
                self.spans.push(pos, pos + 1, SyntaxTokenKind::Punctuation);
                self.after_dot = false;
                self.dot_value_prev = b == b']';
                self.last_token_end = pos + 1;
                pos += 1;
                continue;
            }

            // Operators & punctuation
            if let Some(end) = self.try_scan_multi_char_op(pos) {
                pos = end;
                continue;
            }
            if matches!(b, b',' | b';' | b'.') {
                pos = self.scan_punctuation(pos, b);
                continue;
            }
            if matches!(
                b,
                b'=' | b'<' | b'>' | b'+' | b'-' | b'*' | b'/' | b'%' | b'~' | b'&' | b'|' | b'^'
            ) {
                self.spans.push(pos, pos + 1, SyntaxTokenKind::Operator);
                self.after_dot = false;
                self.dot_value_prev = false;
                self.last_token_end = pos + 1;
                pos += 1;
                continue;
            }

            // Advance safely over any unrecognized ASCII or UTF-8 scalar.
            let ch_len = utf8_char_len(self.source, pos);
            self.after_dot = false;
            self.dot_value_prev = false;
            pos += ch_len;
        }

        Ok(())
    }

    fn note_stmt_token(&mut self, pos: usize) {
        if self.parens.is_empty()
            && let Some(slot) = self.stmt_starts.last_mut()
            && slot.is_none()
        {
            *slot = Some(pos);
        }
    }

    fn scan_line_comment(&mut self, start: usize, only_on_line: bool) -> usize {
        let line = self.lines.line_at(start);
        let line_end = self.lines.line_content_end(line, self.source);
        self.spans.push(start, line_end, SyntaxTokenKind::Comment);

        let body = &self.source[start + 2..line_end];
        match parse_region_keyword(body) {
            Some(RegionMarker::Start) => {
                self.flush_comment_run();
                self.region_stack.push((line, line_end));
            }
            Some(RegionMarker::End) => {
                self.flush_comment_run();
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
            None => {
                if only_on_line {
                    match self.comment_run {
                        Some(mut run) if run.last_line + 1 == line => {
                            run.last_line = line;
                            run.last_end = line_end;
                            self.comment_run = Some(run);
                        }
                        _ => {
                            self.flush_comment_run();
                            self.comment_run = Some(LineCommentRun {
                                start_line: line,
                                start_byte: start,
                                last_line: line,
                                last_end: line_end,
                            });
                        }
                    }
                } else {
                    self.flush_comment_run();
                }
            }
        }

        line_end
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
                placeholder: "-- ...",
            });
        }
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
            if self.bytes[pos] == b'\'' {
                if pos + 1 < len && self.bytes[pos + 1] == b'\'' {
                    if seg_start < pos {
                        self.spans.push(seg_start, pos, SyntaxTokenKind::String);
                    }
                    self.spans.push(pos, pos + 2, SyntaxTokenKind::StringEscape);
                    pos += 2;
                    seg_start = pos;
                    continue;
                }
                let end = pos + 1;
                if seg_start < end {
                    self.spans.push(seg_start, end, SyntaxTokenKind::String);
                }
                self.enclosing_ranges.push(start..end);
                self.last_token_end = end;
                self.feed_table_body_span(start, end);
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
        self.last_token_end = len;
        len
    }

    fn scan_escape_string(&mut self, start: usize) -> usize {
        let len = self.bytes.len();
        let mut seg_start = start;
        let mut pos = start + 2; // skip `E'` / `e'`

        while pos < len {
            let b = self.bytes[pos];
            if b == b'\'' {
                if pos + 1 < len && self.bytes[pos + 1] == b'\'' {
                    if seg_start < pos {
                        self.spans.push(seg_start, pos, SyntaxTokenKind::String);
                    }
                    self.spans.push(pos, pos + 2, SyntaxTokenKind::StringEscape);
                    pos += 2;
                    seg_start = pos;
                    continue;
                }
                let end = pos + 1;
                if seg_start < end {
                    self.spans.push(seg_start, end, SyntaxTokenKind::String);
                }
                self.enclosing_ranges.push(start..end);
                self.last_token_end = end;
                self.feed_table_body_span(start, end);
                return end;
            }
            if b == b'\\' {
                if seg_start < pos {
                    self.spans.push(seg_start, pos, SyntaxTokenKind::String);
                }
                let esc_end = postgres_escape_end(self.source, pos);
                self.spans.push(pos, esc_end, SyntaxTokenKind::StringEscape);
                pos = esc_end;
                seg_start = pos;
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
            String::from("Syntax error: unclosed `E'...'` string literal"),
        );
        self.last_token_end = len;
        len
    }

    fn scan_quoted_identifier(&mut self, start: usize, quote: u8) -> usize {
        let len = self.bytes.len();
        let mut pos = start + 1;

        while pos < len {
            let b = self.bytes[pos];
            if b == b'\n' || b == b'\r' {
                self.spans.push(start, pos, SyntaxTokenKind::Property);
                push_diagnostic(
                    &mut self.diagnostics,
                    self.lines.point_at(start),
                    format!(
                        "Syntax error: unclosed `{}` quoted identifier",
                        char::from(quote)
                    ),
                );
                self.last_token_end = pos;
                return pos;
            }
            if b == quote {
                if pos + 1 < len && self.bytes[pos + 1] == quote {
                    self.spans.push(pos, pos + 2, SyntaxTokenKind::StringEscape);
                    pos += 2;
                    continue;
                }
                let end = pos + 1;
                // SpanBuilder coalesces/prefers narrower StringEscape inside Property.
                self.spans.push(start, end, SyntaxTokenKind::Property);
                self.enclosing_ranges.push(start..end);
                self.last_token_end = end;
                let unquoted = unquote_ident(&self.source[start + 1..pos], quote);
                self.feed_structure_ident(unquoted, start, end);
                return end;
            }
            pos += 1;
        }

        self.spans.push(start, len, SyntaxTokenKind::Property);
        push_diagnostic(
            &mut self.diagnostics,
            self.lines.point_at(start),
            format!(
                "Syntax error: unclosed `{}` quoted identifier",
                char::from(quote)
            ),
        );
        self.last_token_end = len;
        len
    }

    fn scan_dollar(&mut self, start: usize) -> usize {
        let len = self.bytes.len();
        if let Some(delim_end) = parse_dollar_tag_end(self.bytes, start) {
            let delim = &self.bytes[start..delim_end];
            if let Some(rel) = find_subslice(&self.bytes[delim_end..], delim) {
                let end = delim_end + rel + delim.len();
                self.spans.push(start, end, SyntaxTokenKind::String);
                self.enclosing_ranges.push(start..end);
                self.after_dot = false;
                self.dot_value_prev = true;
                self.last_token_end = end;
                return end;
            }
            self.spans.push(start, len, SyntaxTokenKind::String);
            push_diagnostic(
                &mut self.diagnostics,
                self.lines.point_at(start),
                String::from("Syntax error: unclosed dollar-quoted string"),
            );
            self.after_dot = false;
            self.dot_value_prev = true;
            self.last_token_end = len;
            return len;
        }

        if start + 1 < len && self.bytes[start + 1].is_ascii_digit() {
            let mut end = start + 2;
            while end < len && self.bytes[end].is_ascii_digit() {
                end += 1;
            }
            self.spans.push(start, end, SyntaxTokenKind::Parameter);
            self.after_dot = false;
            self.dot_value_prev = true;
            self.last_token_end = end;
            return end;
        }

        self.spans
            .push(start, start + 1, SyntaxTokenKind::Punctuation);
        self.after_dot = false;
        self.dot_value_prev = false;
        self.last_token_end = start + 1;
        start + 1
    }

    fn scan_colon(&mut self, start: usize) -> usize {
        let len = self.bytes.len();
        if self.bytes[start..].starts_with(b"::") || self.bytes[start..].starts_with(b":=") {
            let end = start + 2;
            self.spans.push(start, end, SyntaxTokenKind::Operator);
            self.after_dot = false;
            self.dot_value_prev = false;
            self.last_token_end = end;
            return end;
        }

        let prev_is_colon = start > 0 && self.bytes[start - 1] == b':';
        if !prev_is_colon
            && start + 1 < len
            && (self.bytes[start + 1].is_ascii_alphabetic() || self.bytes[start + 1] == b'_')
        {
            let mut end = start + 2;
            while end < len && (self.bytes[end].is_ascii_alphanumeric() || self.bytes[end] == b'_')
            {
                end += 1;
            }
            self.spans.push(start, end, SyntaxTokenKind::Parameter);
            self.after_dot = false;
            self.dot_value_prev = true;
            self.last_token_end = end;
            return end;
        }

        self.spans
            .push(start, start + 1, SyntaxTokenKind::Punctuation);
        self.after_dot = false;
        self.dot_value_prev = false;
        self.last_token_end = start + 1;
        start + 1
    }

    fn scan_question(&mut self, start: usize) -> usize {
        if self.bytes[start..].starts_with(b"?|") || self.bytes[start..].starts_with(b"?&") {
            let end = start + 2;
            self.spans.push(start, end, SyntaxTokenKind::Operator);
            self.after_dot = false;
            self.dot_value_prev = false;
            self.last_token_end = end;
            return end;
        }

        let mut end = start + 1;
        while end < self.bytes.len() && self.bytes[end].is_ascii_digit() {
            end += 1;
        }
        self.spans.push(start, end, SyntaxTokenKind::Parameter);
        self.after_dot = false;
        self.dot_value_prev = true;
        self.last_token_end = end;
        end
    }

    fn scan_number(&mut self, start: usize) -> usize {
        let len = self.bytes.len();
        let mut pos = start;

        if self.bytes[pos..].starts_with(b"0x") || self.bytes[pos..].starts_with(b"0X") {
            pos += 2;
            while pos < len && (self.bytes[pos].is_ascii_hexdigit() || self.bytes[pos] == b'_') {
                pos += 1;
            }
        } else {
            if self.bytes[pos] == b'.' {
                pos += 1;
                while pos < len && (self.bytes[pos].is_ascii_digit() || self.bytes[pos] == b'_') {
                    pos += 1;
                }
            } else {
                while pos < len && (self.bytes[pos].is_ascii_digit() || self.bytes[pos] == b'_') {
                    pos += 1;
                }
                if pos + 1 < len && self.bytes[pos] == b'.' && self.bytes[pos + 1].is_ascii_digit()
                {
                    pos += 1;
                    while pos < len && (self.bytes[pos].is_ascii_digit() || self.bytes[pos] == b'_')
                    {
                        pos += 1;
                    }
                }
            }
            if pos < len && matches!(self.bytes[pos], b'e' | b'E') {
                let exp_start = pos;
                pos += 1;
                if pos < len && matches!(self.bytes[pos], b'+' | b'-') {
                    pos += 1;
                }
                if pos < len && self.bytes[pos].is_ascii_digit() {
                    while pos < len && (self.bytes[pos].is_ascii_digit() || self.bytes[pos] == b'_')
                    {
                        pos += 1;
                    }
                } else {
                    pos = exp_start;
                }
            }
        }

        self.spans.push(start, pos, SyntaxTokenKind::Number);
        self.last_token_end = pos;
        self.feed_table_body_span(start, pos);
        pos
    }

    fn scan_identifier(&mut self, start: usize) -> usize {
        let end = scan_ident_end(self.source, start);
        let raw = &self.source[start..end];
        let followed_by_paren = next_non_ws_byte(self.bytes, end) == Some(b'(');

        let mut upper_buf = [0u8; 64];
        let upper = if raw.is_ascii() && raw.len() <= upper_buf.len() {
            for (dst, &src) in upper_buf.iter_mut().zip(raw.as_bytes()) {
                *dst = src.to_ascii_uppercase();
            }
            // SAFETY: ASCII bytes uppercased remain valid UTF-8.
            std::str::from_utf8(&upper_buf[..raw.len()]).ok()
        } else {
            None
        };

        let kind = if self.after_dot {
            if followed_by_paren {
                SyntaxTokenKind::Function
            } else {
                SyntaxTokenKind::Property
            }
        } else if let Some(u) = upper {
            classify_uppercase_ident(u, followed_by_paren)
        } else if followed_by_paren {
            SyntaxTokenKind::Function
        } else {
            SyntaxTokenKind::Variable
        };

        self.spans.push(start, end, kind);

        if !self.after_dot
            && let Some(u) = upper
        {
            self.handle_block_keyword(u, start, end);
        }

        self.feed_structure_word(raw, upper, start, end);

        self.after_dot = false;
        self.dot_value_prev = true;
        self.last_token_end = end;
        end
    }

    fn handle_block_keyword(&mut self, upper: &str, start: usize, end: usize) {
        match upper {
            "BEGIN" => {
                let next = peek_next_significant_upper(self.source, self.bytes, end);
                let is_tx = matches!(
                    next.as_deref(),
                    None | Some(
                        ";" | "TRANSACTION"
                            | "WORK"
                            | "ISOLATION"
                            | "READ"
                            | "DEFERRED"
                            | "IMMEDIATE"
                            | "EXCLUSIVE"
                    )
                );
                if !is_tx {
                    let line = self.lines.line_at(start);
                    self.blocks.push(BlockFrame {
                        kind: BlockKind::Begin,
                        start_byte: start,
                        open_line: line,
                        header_end_byte: self.lines.line_content_end(line, self.source),
                    });
                    self.stmt_starts.push(None);
                }
            }
            "CASE" => {
                let line = self.lines.line_at(start);
                self.blocks.push(BlockFrame {
                    kind: BlockKind::Case,
                    start_byte: start,
                    open_line: line,
                    header_end_byte: self.lines.line_content_end(line, self.source),
                });
                self.stmt_starts.push(None);
            }
            "END" => self.handle_end_keyword(start, end),
            "COMMIT" => {
                if self
                    .blocks
                    .last()
                    .is_some_and(|frame| frame.kind == BlockKind::Begin)
                {
                    self.blocks.pop();
                    self.pop_stmt_start();
                }
            }
            "ROLLBACK" => {
                let next = peek_next_significant_upper(self.source, self.bytes, end);
                if next.as_deref() != Some("TO")
                    && self
                        .blocks
                        .last()
                        .is_some_and(|frame| frame.kind == BlockKind::Begin)
                {
                    self.blocks.pop();
                    self.pop_stmt_start();
                }
            }
            _ => {}
        }
    }

    fn pop_stmt_start(&mut self) {
        if self.stmt_starts.len() > 1 {
            self.stmt_starts.pop();
        }
    }

    fn handle_end_keyword(&mut self, start: usize, end: usize) {
        let next = peek_next_significant_upper(self.source, self.bytes, end);
        if matches!(next.as_deref(), Some("IF" | "LOOP" | "WHILE")) {
            // `END IF` / `END LOOP` / `END WHILE` closes an unstacked PL/pgSQL control
            // construct; leave any enclosing `BEGIN` or `CASE` frame intact.
            return;
        }
        if next.as_deref() == Some("CASE") {
            if let Some(idx) = self
                .blocks
                .iter()
                .rposition(|frame| frame.kind == BlockKind::Case)
            {
                while self.blocks.len() > idx + 1 {
                    if let Some(unclosed) = self.blocks.pop() {
                        self.pop_stmt_start();
                        push_diagnostic(
                            &mut self.diagnostics,
                            self.lines.point_at(unclosed.start_byte),
                            format!("Syntax error: unclosed `{}` block", unclosed.kind.label()),
                        );
                    }
                }
                if let Some(frame) = self.blocks.pop() {
                    self.pop_stmt_start();
                    self.close_block_frame(frame, end);
                }
            }
            return;
        }
        if let Some(frame) = self.blocks.pop() {
            self.pop_stmt_start();
            self.close_block_frame(frame, end);
        } else {
            push_diagnostic(
                &mut self.diagnostics,
                self.lines.point_at(start),
                String::from("Syntax error: unexpected `END`"),
            );
        }
    }

    fn close_block_frame(&mut self, frame: BlockFrame, close_end: usize) {
        self.enclosing_ranges.push(frame.start_byte..close_end);
        let close_line = self.lines.line_at(close_end.saturating_sub(1));
        if close_line > frame.open_line && frame.header_end_byte < close_end {
            self.folds.push(FoldRange {
                start_line: frame.open_line,
                end_line: close_line,
                collapsed_byte_range: frame.header_end_byte..close_end,
                kind: FoldKind::Block,
                default_collapsed: false,
                placeholder: "...",
            });
        }
    }

    fn scan_open_paren(&mut self, pos: usize) -> usize {
        self.spans.push(pos, pos + 1, SyntaxTokenKind::Punctuation);
        let line = self.lines.line_at(pos);
        self.parens.push(ParenFrame {
            open_byte: pos,
            open_line: line,
        });
        let depth = self.parens.len();

        match &mut self.stmt_state {
            StmtState::CreateObject {
                kind,
                table_body,
                params_depth,
                ..
            } => {
                if *kind == CreateObjectKind::Table && table_body.is_none() && depth == 1 {
                    *table_body = Some(TableBodyCollector::new(depth, pos + 1));
                } else if matches!(
                    kind,
                    CreateObjectKind::Function | CreateObjectKind::Procedure
                ) && params_depth.is_none()
                    && depth == 1
                {
                    *params_depth = Some(depth);
                } else if let Some(tb) = table_body
                    && depth > tb.paren_depth
                {
                    tb.current.end_byte = pos + 1;
                    if !tb.current.type_done && tb.current.type_start.is_some() {
                        tb.current.type_end = pos + 1;
                    }
                }
            }
            StmtState::With {
                cte_name,
                col_list_depth,
                saw_as,
                body_depth,
                ..
            } if depth == 1 => {
                if *saw_as && body_depth.is_none() {
                    *body_depth = Some(depth);
                } else if cte_name.is_some() && !*saw_as && col_list_depth.is_none() {
                    *col_list_depth = Some(depth);
                }
            }
            _ => {}
        }

        self.after_dot = false;
        self.dot_value_prev = false;
        self.last_token_end = pos + 1;
        pos + 1
    }

    fn scan_close_paren(&mut self, pos: usize) -> usize {
        self.spans.push(pos, pos + 1, SyntaxTokenKind::Punctuation);
        let close_end = pos + 1;
        let close_line = self.lines.line_at(pos);

        if let Some(frame) = self.parens.pop() {
            let depth_after_pop = self.parens.len();
            self.enclosing_ranges.push(frame.open_byte..close_end);
            if close_line > frame.open_line {
                self.folds.push(FoldRange {
                    start_line: frame.open_line,
                    end_line: close_line,
                    collapsed_byte_range: frame.open_byte..close_end,
                    kind: FoldKind::Block,
                    default_collapsed: false,
                    placeholder: "(...)",
                });
            }

            let closed_depth = depth_after_pop + 1;
            self.handle_close_paren_structure(frame.open_byte, close_end, closed_depth, pos);
        } else {
            push_diagnostic(
                &mut self.diagnostics,
                self.lines.point_at(pos),
                String::from("Syntax error: unexpected `)`"),
            );
        }

        self.after_dot = false;
        self.dot_value_prev = true;
        self.last_token_end = close_end;
        close_end
    }

    fn handle_close_paren_structure(
        &mut self,
        open_byte: usize,
        close_end: usize,
        closed_depth: usize,
        close_pos: usize,
    ) {
        enum Transition {
            None,
            FinishTable(StructureNode),
            ClearWithCte(StructureNode),
        }

        let mut transition = Transition::None;
        match &mut self.stmt_state {
            StmtState::CreateObject {
                start_byte,
                name,
                selection_range,
                table_body,
                params_depth,
                params_detail,
                ..
            } => {
                if let Some(tb) = table_body {
                    if closed_depth == tb.paren_depth {
                        let mut taken = table_body.take();
                        if let Some(ref mut collector) = taken {
                            collector.current.end_byte = close_pos;
                            flush_column_item(self.source, &self.lines, collector);
                            if let (Some(tbl_name), Some(sel)) =
                                (name.take(), selection_range.take())
                            {
                                let children = std::mem::take(&mut collector.columns);
                                let node = make_structure_node(
                                    &tbl_name,
                                    Some(String::from("table")),
                                    StructureSymbolKind::Class,
                                    *start_byte..close_end,
                                    sel,
                                    children,
                                    &self.lines,
                                );
                                transition = Transition::FinishTable(node);
                            }
                        }
                    } else if closed_depth > tb.paren_depth {
                        tb.current.end_byte = close_end;
                        if !tb.current.type_done && tb.current.type_start.is_some() {
                            tb.current.type_end = close_end;
                        }
                    }
                } else if *params_depth == Some(closed_depth) {
                    *params_depth = None;
                    if params_detail.is_none() {
                        *params_detail =
                            Some(compact_text(&self.source[open_byte..close_end], 128));
                    }
                }
            }
            StmtState::With {
                cte_name,
                cte_sel,
                cte_start,
                col_list_depth,
                saw_as,
                body_depth,
                ..
            } => {
                if *col_list_depth == Some(closed_depth) {
                    *col_list_depth = None;
                } else if *body_depth == Some(closed_depth) {
                    *body_depth = None;
                    *saw_as = false;
                    if let (Some(name), Some(sel)) = (cte_name.take(), cte_sel.take()) {
                        let node = make_structure_node(
                            &name,
                            Some(String::from("cte")),
                            StructureSymbolKind::Section,
                            *cte_start..close_end,
                            sel,
                            Vec::new(),
                            &self.lines,
                        );
                        transition = Transition::ClearWithCte(node);
                    }
                }
            }
            _ => {}
        }

        match transition {
            Transition::None => {}
            Transition::FinishTable(node) => {
                self.push_structure_root(node);
                self.stmt_state = StmtState::Other;
            }
            Transition::ClearWithCte(node) => {
                self.push_structure_root(node);
            }
        }
    }

    fn try_scan_multi_char_op(&mut self, pos: usize) -> Option<usize> {
        let rest = &self.bytes[pos..];
        let op = MULTI_CHAR_OPS
            .iter()
            .copied()
            .find(|op| rest.starts_with(op))?;
        let end = pos + op.len();
        self.spans.push(pos, end, SyntaxTokenKind::Operator);
        self.after_dot = false;
        self.dot_value_prev = false;
        self.last_token_end = end;
        Some(end)
    }

    fn scan_punctuation(&mut self, pos: usize, b: u8) -> usize {
        let end = pos + 1;
        self.spans.push(pos, end, SyntaxTokenKind::Punctuation);

        match b {
            b'.' => {
                self.after_dot = true;
                self.dot_value_prev = false;
            }
            b',' => {
                self.after_dot = false;
                self.dot_value_prev = false;
                let depth = self.parens.len();
                match &mut self.stmt_state {
                    StmtState::CreateObject {
                        table_body: Some(tb),
                        ..
                    } if depth == tb.paren_depth => {
                        tb.current.end_byte = pos;
                        flush_column_item(self.source, &self.lines, tb);
                        tb.current = ColumnItem::empty(end);
                    }
                    StmtState::With {
                        expect_cte_name,
                        saw_as,
                        body_depth,
                        col_list_depth,
                        ..
                    } if depth == 0
                        && !*saw_as
                        && body_depth.is_none()
                        && col_list_depth.is_none() =>
                    {
                        *expect_cte_name = true;
                    }
                    _ => {}
                }
            }
            b';' => {
                self.after_dot = false;
                self.dot_value_prev = false;
                if self.parens.is_empty() {
                    if let Some(start) = self.stmt_starts.last_mut().and_then(Option::take)
                        && start < end
                    {
                        self.enclosing_ranges.push(start..end);
                    }
                    if self.blocks.is_empty() {
                        self.finalize_pending_create(end);
                        self.stmt_state = StmtState::Start;
                    } else if matches!(self.stmt_state, StmtState::With { .. }) {
                        self.stmt_state = StmtState::Start;
                    }
                }
            }
            _ => {}
        }

        self.last_token_end = end;
        end
    }

    fn feed_table_body_span(&mut self, start: usize, end: usize) {
        let depth = self.parens.len();
        if let StmtState::CreateObject {
            table_body: Some(tb),
            ..
        } = &mut self.stmt_state
            && depth >= tb.paren_depth
        {
            if tb.current.name.is_none() && !tb.current.is_constraint {
                tb.current.start_byte = start;
            }
            tb.current.end_byte = end;
            if depth == tb.paren_depth && tb.current.name.is_some() && !tb.current.type_done {
                if tb.current.type_start.is_none() {
                    tb.current.type_start = Some(start);
                }
                tb.current.type_end = end;
            }
        }
    }

    fn feed_structure_ident(&mut self, ident: String, start: usize, end: usize) {
        let depth = self.parens.len();
        match &mut self.stmt_state {
            StmtState::CreateObject {
                if_not_exists_stage,
                skip_concurrently,
                saw_index_on,
                name,
                selection_range,
                expect_dot_part,
                table_body,
                ..
            } if depth == 0 && table_body.is_none() => {
                *skip_concurrently = false;
                if *if_not_exists_stage > 0 && *if_not_exists_stage < 3 {
                    *if_not_exists_stage = 3;
                }
                if self.after_dot && *expect_dot_part {
                    if let Some(existing) = name {
                        existing.push('.');
                        existing.push_str(&ident);
                    }
                    if let Some(sel) = selection_range {
                        sel.end = end;
                    }
                } else if name.is_none() || *saw_index_on {
                    if *saw_index_on {
                        *saw_index_on = false;
                        *name = Some(format!("on {ident}"));
                    } else {
                        *name = Some(ident);
                    }
                    *selection_range = Some(start..end);
                    *expect_dot_part = true;
                }
            }
            StmtState::CreateObject {
                table_body: Some(tb),
                ..
            } if depth == tb.paren_depth => {
                if !tb.current.is_constraint && tb.current.name.is_none() {
                    tb.current.name = Some(ident);
                    tb.current.selection_range = Some(start..end);
                    tb.current.start_byte = start;
                    tb.current.end_byte = end;
                } else if !tb.current.is_constraint && !tb.current.type_done {
                    if tb.current.type_start.is_none() {
                        tb.current.type_start = Some(start);
                    }
                    tb.current.type_end = end;
                    tb.current.end_byte = end;
                }
            }
            StmtState::With {
                expect_cte_name,
                cte_name,
                cte_sel,
                cte_start,
                ..
            } if depth == 0 && *expect_cte_name => {
                *cte_name = Some(ident);
                *cte_sel = Some(start..end);
                *cte_start = start;
                *expect_cte_name = false;
            }
            _ => {}
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Cohesive SQL DDL/CTE header state machine"
    )]
    fn feed_structure_word(&mut self, raw: &str, upper: Option<&str>, start: usize, end: usize) {
        let depth = self.parens.len();
        let u = upper.unwrap_or("");

        // Handle table column items first when inside CREATE TABLE (...).
        if let StmtState::CreateObject {
            table_body: Some(tb),
            ..
        } = &mut self.stmt_state
        {
            if depth == tb.paren_depth {
                if !tb.current.is_constraint && tb.current.name.is_none() {
                    if TABLE_CONSTRAINTS.binary_search(&u).is_ok() {
                        tb.current.is_constraint = true;
                    } else {
                        tb.current.name = Some(raw.to_owned());
                        tb.current.selection_range = Some(start..end);
                        tb.current.start_byte = start;
                        tb.current.end_byte = end;
                    }
                } else if !tb.current.is_constraint {
                    tb.current.end_byte = end;
                    if !tb.current.type_done {
                        if COLUMN_TYPE_STOPS.binary_search(&u).is_ok() {
                            tb.current.type_done = true;
                        } else {
                            if tb.current.type_start.is_none() {
                                tb.current.type_start = Some(start);
                            }
                            tb.current.type_end = end;
                        }
                    }
                }
            } else if depth > tb.paren_depth && !tb.current.is_constraint {
                tb.current.end_byte = end;
                if !tb.current.type_done && tb.current.type_start.is_some() {
                    tb.current.type_end = end;
                }
            }
            return;
        }

        if depth > 0 {
            return;
        }

        match &mut self.stmt_state {
            StmtState::Start => {
                if !self.after_dot {
                    match u {
                        "CREATE" => {
                            self.stmt_state = StmtState::Create { start_byte: start };
                        }
                        "WITH" => {
                            self.stmt_state = StmtState::With {
                                expect_cte_name: true,
                                cte_name: None,
                                cte_sel: None,
                                cte_start: start,
                                col_list_depth: None,
                                saw_as: false,
                                body_depth: None,
                            };
                        }
                        _ => {
                            self.stmt_state = StmtState::Other;
                        }
                    }
                }
            }
            StmtState::Create { start_byte } => {
                if matches!(
                    u,
                    "OR" | "REPLACE" | "TEMP" | "TEMPORARY" | "UNIQUE" | "MATERIALIZED"
                ) {
                    return;
                }
                let kind = match u {
                    "TABLE" => Some(CreateObjectKind::Table),
                    "VIEW" => Some(CreateObjectKind::View),
                    "INDEX" => Some(CreateObjectKind::Index),
                    "FUNCTION" => Some(CreateObjectKind::Function),
                    "PROCEDURE" => Some(CreateObjectKind::Procedure),
                    "TRIGGER" => Some(CreateObjectKind::Trigger),
                    "TYPE" => Some(CreateObjectKind::Type),
                    "DOMAIN" => Some(CreateObjectKind::Domain),
                    "SCHEMA" => Some(CreateObjectKind::Schema),
                    "SEQUENCE" => Some(CreateObjectKind::Sequence),
                    _ => None,
                };
                if let Some(kind) = kind {
                    self.stmt_state = StmtState::CreateObject {
                        start_byte: *start_byte,
                        kind,
                        if_not_exists_stage: 0,
                        skip_concurrently: kind == CreateObjectKind::Index,
                        saw_index_on: false,
                        name: None,
                        selection_range: None,
                        expect_dot_part: false,
                        table_body: None,
                        params_detail: None,
                        params_depth: None,
                        enum_stage: 0,
                    };
                } else {
                    self.stmt_state = StmtState::Other;
                }
            }
            StmtState::CreateObject {
                kind,
                if_not_exists_stage,
                skip_concurrently,
                saw_index_on,
                name,
                selection_range,
                expect_dot_part,
                enum_stage,
                ..
            } => {
                if name.is_none() {
                    if *skip_concurrently && u == "CONCURRENTLY" {
                        *skip_concurrently = false;
                        return;
                    }
                    *skip_concurrently = false;
                    match (*if_not_exists_stage, u) {
                        (0, "IF") => {
                            *if_not_exists_stage = 1;
                            return;
                        }
                        (1, "NOT") => {
                            *if_not_exists_stage = 2;
                            return;
                        }
                        (2, "EXISTS") => {
                            *if_not_exists_stage = 3;
                            return;
                        }
                        _ => {
                            *if_not_exists_stage = 3;
                        }
                    }
                    if *kind == CreateObjectKind::Index && u == "ON" && !*saw_index_on {
                        *saw_index_on = true;
                        return;
                    }
                    if *saw_index_on {
                        *saw_index_on = false;
                        *name = Some(format!("on {raw}"));
                    } else {
                        *name = Some(raw.to_owned());
                    }
                    *selection_range = Some(start..end);
                    *expect_dot_part = true;
                } else if self.after_dot && *expect_dot_part {
                    if let Some(existing) = name {
                        existing.push('.');
                        existing.push_str(raw);
                    }
                    if let Some(sel) = selection_range {
                        sel.end = end;
                    }
                } else {
                    *expect_dot_part = false;
                    if *kind == CreateObjectKind::Type {
                        match (*enum_stage, u) {
                            (0, "AS") => *enum_stage = 1,
                            (1, "ENUM") => *enum_stage = 2,
                            _ => {}
                        }
                    }
                }
            }
            StmtState::With {
                expect_cte_name,
                cte_name,
                cte_sel,
                cte_start,
                saw_as,
                body_depth,
                col_list_depth,
            } => {
                if *expect_cte_name {
                    if cte_name.is_none() && u == "RECURSIVE" {
                        return;
                    }
                    *cte_name = Some(raw.to_owned());
                    *cte_sel = Some(start..end);
                    *cte_start = start;
                    *expect_cte_name = false;
                } else if cte_name.is_some() && !*saw_as {
                    if u == "AS" {
                        *saw_as = true;
                    } else if col_list_depth.is_none() {
                        self.stmt_state = StmtState::Other;
                    }
                } else if *saw_as && body_depth.is_none() {
                    if !matches!(u, "MATERIALIZED" | "NOT") {
                        self.stmt_state = StmtState::Other;
                    }
                } else if !*saw_as && body_depth.is_none() {
                    self.stmt_state = StmtState::Other;
                }
            }
            StmtState::Other => {}
        }
    }

    fn finalize_pending_create(&mut self, stmt_end: usize) {
        let state = std::mem::replace(&mut self.stmt_state, StmtState::Start);
        let StmtState::CreateObject {
            start_byte,
            kind,
            name: Some(name),
            selection_range: Some(sel),
            table_body,
            params_detail,
            enum_stage,
            ..
        } = state
        else {
            return;
        };

        let (sym_kind, detail, children) = match kind {
            CreateObjectKind::Table => {
                let children = table_body.map_or_else(Vec::new, |mut tb| {
                    flush_column_item(self.source, &self.lines, &mut tb);
                    tb.columns
                });
                (
                    StructureSymbolKind::Class,
                    Some(String::from("table")),
                    children,
                )
            }
            CreateObjectKind::View => (
                StructureSymbolKind::Section,
                Some(String::from("view")),
                Vec::new(),
            ),
            CreateObjectKind::Index => (
                StructureSymbolKind::Section,
                Some(String::from("index")),
                Vec::new(),
            ),
            CreateObjectKind::Function => (
                StructureSymbolKind::Function,
                params_detail.or_else(|| Some(String::from("function"))),
                Vec::new(),
            ),
            CreateObjectKind::Procedure => (
                StructureSymbolKind::Function,
                params_detail.or_else(|| Some(String::from("procedure"))),
                Vec::new(),
            ),
            CreateObjectKind::Trigger => (
                StructureSymbolKind::Section,
                Some(String::from("trigger")),
                Vec::new(),
            ),
            CreateObjectKind::Type => {
                if enum_stage == 2 {
                    (
                        StructureSymbolKind::Enum,
                        Some(String::from("enum")),
                        Vec::new(),
                    )
                } else {
                    (
                        StructureSymbolKind::Section,
                        Some(String::from("type")),
                        Vec::new(),
                    )
                }
            }
            CreateObjectKind::Domain => (
                StructureSymbolKind::Section,
                Some(String::from("domain")),
                Vec::new(),
            ),
            CreateObjectKind::Schema => (
                StructureSymbolKind::Section,
                Some(String::from("schema")),
                Vec::new(),
            ),
            CreateObjectKind::Sequence => (
                StructureSymbolKind::Section,
                Some(String::from("sequence")),
                Vec::new(),
            ),
        };

        let full_end = stmt_end.max(start_byte);
        let node = make_structure_node(
            &name,
            detail,
            sym_kind,
            start_byte..full_end,
            sel,
            children,
            &self.lines,
        );
        self.push_structure_root(node);
    }

    fn push_structure_root(&mut self, node: StructureNode) {
        if MAX_STRUCTURE_DEPTH >= 1 {
            self.struct_roots.push(node);
        }
    }

    fn finish(&mut self) {
        self.flush_comment_run();
        let len = self.bytes.len();
        let stmt_end = self.last_token_end.min(len);
        self.finalize_pending_create(stmt_end);

        if let Some(start) = self.stmt_starts.first_mut().and_then(Option::take)
            && start < stmt_end
        {
            self.enclosing_ranges.push(start..stmt_end);
        }

        while let Some(frame) = self.parens.pop() {
            push_diagnostic(
                &mut self.diagnostics,
                self.lines.point_at(frame.open_byte),
                String::from("Syntax error: unclosed `(`"),
            );
        }

        while let Some(frame) = self.blocks.pop() {
            push_diagnostic(
                &mut self.diagnostics,
                self.lines.point_at(frame.start_byte),
                format!("Syntax error: unclosed `{}` block", frame.kind.label()),
            );
        }
    }
}

fn flush_column_item(source: &str, lines: &LineIndex, tb: &mut TableBodyCollector) {
    if tb.current.is_constraint {
        return;
    }
    let Some(name) = tb.current.name.take() else {
        return;
    };
    let Some(sel) = tb.current.selection_range.take() else {
        return;
    };
    let detail = tb.current.type_start.and_then(|ts| {
        let te = tb.current.type_end.min(source.len());
        (ts < te).then(|| compact_text(&source[ts..te], 64))
    });
    let full_start = tb.current.start_byte.min(sel.start);
    let full_end = tb.current.end_byte.max(sel.end).min(source.len());
    tb.columns.push(make_structure_node(
        &name,
        detail,
        StructureSymbolKind::Field,
        full_start..full_end,
        sel,
        Vec::new(),
        lines,
    ));
}

fn make_structure_node(
    name: &str,
    detail: Option<String>,
    kind: StructureSymbolKind,
    full_range: Range<usize>,
    selection_range: Range<usize>,
    children: Vec<StructureNode>,
    lines: &LineIndex,
) -> StructureNode {
    StructureNode {
        name: compact_text(name, 128),
        detail,
        kind,
        visibility: SymbolVisibility::Unspecified,
        is_static: false,
        is_abstract: false,
        start_point: lines.point_at(full_range.start),
        end_point: lines.point_at(full_range.end),
        selection_point: lines.point_at(selection_range.start),
        full_range,
        selection_range,
        children,
    }
}

fn classify_uppercase_ident(upper: &str, followed_by_paren: bool) -> SyntaxTokenKind {
    // Contextual words that serve as either clauses/types or built-in functions when invoked.
    if matches!(upper, "LEFT" | "RIGHT" | "REPLACE" | "FORMAT" | "YEAR") {
        if followed_by_paren {
            return SyntaxTokenKind::Function;
        }
        return if upper == "YEAR" {
            SyntaxTokenKind::TypeBuiltin
        } else {
            SyntaxTokenKind::Keyword
        };
    }

    if KEYWORDS_CONTROL.binary_search(&upper).is_ok() {
        return SyntaxTokenKind::KeywordControl;
    }
    if KEYWORDS_BOOLEAN.binary_search(&upper).is_ok() {
        return SyntaxTokenKind::Boolean;
    }
    if KEYWORDS.binary_search(&upper).is_ok() {
        return SyntaxTokenKind::Keyword;
    }
    if TYPE_BUILTINS.binary_search(&upper).is_ok() {
        return SyntaxTokenKind::TypeBuiltin;
    }
    if followed_by_paren || NO_PAREN_FUNCTIONS.binary_search(&upper).is_ok() {
        return SyntaxTokenKind::Function;
    }
    SyntaxTokenKind::Variable
}

fn is_line_prefix_ws(bytes: &[u8], lines: &LineIndex, pos: usize) -> bool {
    let line = lines.line_at(pos);
    let line_start = lines.raw_line_range(line, bytes.len()).start;
    bytes[line_start..pos]
        .iter()
        .all(|b| matches!(b, b' ' | b'\t'))
}

fn is_ident_start_byte(source: &str, pos: usize) -> bool {
    let b = source.as_bytes()[pos];
    if b.is_ascii_alphabetic() || b == b'_' {
        return true;
    }
    if b >= 0x80 {
        return source[pos..]
            .chars()
            .next()
            .is_some_and(char::is_alphabetic);
    }
    false
}

fn scan_ident_end(source: &str, start: usize) -> usize {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut pos = start;
    while pos < len {
        let b = bytes[pos];
        if b.is_ascii_alphanumeric() || b == b'_' || b == b'$' {
            pos += 1;
        } else if b >= 0x80 {
            if let Some(ch) = source[pos..].chars().next()
                && (ch.is_alphanumeric() || ch == '_')
            {
                pos += ch.len_utf8();
            } else {
                break;
            }
        } else {
            break;
        }
    }
    pos
}

fn utf8_char_len(source: &str, pos: usize) -> usize {
    source[pos..].chars().next().map_or(1, char::len_utf8)
}

fn next_non_ws_byte(bytes: &[u8], mut pos: usize) -> Option<u8> {
    while pos < bytes.len() && matches!(bytes[pos], b' ' | b'\t' | b'\r' | b'\n') {
        pos += 1;
    }
    bytes.get(pos).copied()
}

fn postgres_escape_end(source: &str, backslash_pos: usize) -> usize {
    let bytes = source.as_bytes();
    let len = bytes.len();
    if backslash_pos + 1 >= len {
        return len;
    }
    match bytes[backslash_pos + 1] {
        b'x' => {
            let mut end = backslash_pos + 2;
            let mut digits = 0;
            while end < len && digits < 2 && bytes[end].is_ascii_hexdigit() {
                end += 1;
                digits += 1;
            }
            end
        }
        b'u' => {
            let mut end = backslash_pos + 2;
            let mut digits = 0;
            while end < len && digits < 4 && bytes[end].is_ascii_hexdigit() {
                end += 1;
                digits += 1;
            }
            end
        }
        b'U' => {
            let mut end = backslash_pos + 2;
            let mut digits = 0;
            while end < len && digits < 8 && bytes[end].is_ascii_hexdigit() {
                end += 1;
                digits += 1;
            }
            end
        }
        _ => backslash_pos + 1 + utf8_char_len(source, backslash_pos + 1),
    }
}

fn parse_dollar_tag_end(bytes: &[u8], start: usize) -> Option<usize> {
    let len = bytes.len();
    if start + 1 >= len || bytes[start] != b'$' {
        return None;
    }
    if bytes[start + 1] == b'$' {
        return Some(start + 2);
    }
    if !(bytes[start + 1].is_ascii_alphabetic() || bytes[start + 1] == b'_') {
        return None;
    }
    let mut pos = start + 2;
    while pos < len && (bytes[pos].is_ascii_alphanumeric() || bytes[pos] == b'_') {
        pos += 1;
    }
    (pos < len && bytes[pos] == b'$').then_some(pos + 1)
}

fn unquote_ident(inner: &str, quote: u8) -> String {
    let ch = char::from(quote);
    let doubled = match quote {
        b'"' => "\"\"",
        b'`' => "``",
        _ => return inner.to_owned(),
    };
    if inner.contains(doubled) {
        let mut single = [0u8; 1];
        single[0] = quote;
        let replacement = std::str::from_utf8(&single).unwrap_or("");
        inner.replace(doubled, replacement)
    } else {
        let _ = ch;
        inner.to_owned()
    }
}

fn peek_next_significant_upper(source: &str, bytes: &[u8], mut pos: usize) -> Option<String> {
    let len = bytes.len();
    loop {
        while pos < len && matches!(bytes[pos], b' ' | b'\t' | b'\r' | b'\n') {
            pos += 1;
        }
        if pos >= len {
            return None;
        }
        if bytes[pos..].starts_with(b"--") {
            while pos < len && bytes[pos] != b'\n' {
                pos += 1;
            }
            continue;
        }
        if bytes[pos..].starts_with(b"/*") {
            if let Some(rel) = find_subslice(&bytes[pos + 2..], b"*/") {
                pos = pos + 2 + rel + 2;
                continue;
            }
            return None;
        }
        if bytes[pos] == b';' {
            return Some(String::from(";"));
        }
        if is_ident_start_byte(source, pos) {
            let end = scan_ident_end(source, pos);
            return Some(source[pos..end].to_ascii_uppercase());
        }
        return Some(source[pos..pos + utf8_char_len(source, pos)].to_owned());
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_strictly_sorted(slice: &[&str], name: &str) {
        for pair in slice.windows(2) {
            assert!(
                pair[0] < pair[1],
                "{name} is not strictly sorted: {:?} >= {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn lookup_tables_are_strictly_sorted() {
        assert_strictly_sorted(KEYWORDS_CONTROL, "KEYWORDS_CONTROL");
        assert_strictly_sorted(KEYWORDS_BOOLEAN, "KEYWORDS_BOOLEAN");
        assert_strictly_sorted(KEYWORDS, "KEYWORDS");
        assert_strictly_sorted(TYPE_BUILTINS, "TYPE_BUILTINS");
        assert_strictly_sorted(NO_PAREN_FUNCTIONS, "NO_PAREN_FUNCTIONS");
        assert_strictly_sorted(TABLE_CONSTRAINTS, "TABLE_CONSTRAINTS");
        assert_strictly_sorted(COLUMN_TYPE_STOPS, "COLUMN_TYPE_STOPS");
    }
}
