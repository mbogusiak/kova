//! Integration tests for KOVA-064: SQL (`.sql`, `.ddl`, `.psql`, `.hql`) detection,
//! highlighting, folding, structure outline, extend selection, syntax diagnostics,
//! `rayon` cancellation, and 120 FPS viewport highlighting performance.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "integration tests assert SQL scanner invariants and parsed results"
)]

use std::cmp::Reverse;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use kova_buffer::{Buffer, BufferId, BufferSnapshot};
use kova_syntax::{
    FoldKind, FoldRange, HighlightSpan, Highlighter, LanguageRegistry, ParseRequest, StructureNode,
    StructureOutline, StructureSymbolKind, SyntaxEngine, SyntaxEngineConfig, SyntaxError,
    SyntaxTree, collect_syntax_diagnostics, detect_entry_points, detect_language, extract_folds,
    extract_structure,
};
use kova_theme::{SyntaxLanguage, SyntaxTokenKind};

fn setup() -> (SyntaxEngine, Highlighter) {
    let registry = LanguageRegistry::builtin().expect("builtin registry initializes");
    let highlighter = Highlighter::new(registry.clone());
    let engine = SyntaxEngine::new(
        registry.clone(),
        SyntaxEngineConfig {
            worker_threads: 2,
            parse_timeout: Some(Duration::from_secs(5)),
            reuse_thread_local_parsers: true,
        },
    )
    .expect("SyntaxEngine initializes");
    (engine, highlighter)
}

fn parse(engine: &SyntaxEngine, source: &str) -> (BufferSnapshot, SyntaxTree) {
    let buffer = Buffer::with_id(BufferId::new(1), source);
    let snapshot = buffer.snapshot();
    let tree = engine
        .parse_sync(SyntaxLanguage::Sql, &snapshot, None)
        .expect("parse_sync succeeds");
    (snapshot, tree)
}

fn assert_span_invariants(
    spans: &[HighlightSpan],
    source: &str,
    expected_bounds: std::ops::Range<usize>,
) {
    for span in spans {
        assert!(
            span.range.start < span.range.end,
            "span must be non-empty: {span:?}"
        );
        assert!(
            span.range.start >= expected_bounds.start && span.range.end <= expected_bounds.end,
            "span {span:?} escaped bounds {expected_bounds:?}"
        );
        assert!(
            source.is_char_boundary(span.range.start) && source.is_char_boundary(span.range.end),
            "span {span:?} is not on UTF-8 char boundaries"
        );
    }
    for pair in spans.windows(2) {
        assert!(
            pair[0].range.end <= pair[1].range.start,
            "spans are not sorted and disjoint: {:?} followed by {:?}",
            pair[0],
            pair[1]
        );
    }
}

fn assert_fold_invariants(folds: &[FoldRange], snapshot: &BufferSnapshot) {
    let len = snapshot.len_bytes();
    for (idx, fold) in folds.iter().enumerate() {
        assert!(
            fold.end_line > fold.start_line,
            "single-line fold: {fold:?}"
        );
        assert!(
            fold.collapsed_byte_range.start <= fold.collapsed_byte_range.end
                && fold.collapsed_byte_range.end <= len,
            "fold range out of bounds: {fold:?}"
        );
        if idx > 0 {
            let prev = &folds[idx - 1];
            assert!(
                (prev.start_line, Reverse(prev.end_line))
                    <= (fold.start_line, Reverse(fold.end_line)),
                "folds not sorted: prev={prev:?} curr={fold:?}"
            );
            assert_ne!(
                (prev.start_line, prev.end_line),
                (fold.start_line, fold.end_line),
                "duplicate fold line pair: {prev:?} vs {fold:?}"
            );
        }
    }
}

fn assert_structure_invariants(outline: &StructureOutline, snapshot: &BufferSnapshot) {
    let len = snapshot.len_bytes();
    let mut stack: Vec<&StructureNode> = outline.roots.iter().collect();
    while let Some(node) = stack.pop() {
        assert!(node.full_range.start <= node.full_range.end && node.full_range.end <= len);
        assert!(
            node.selection_range.start <= node.selection_range.end
                && node.selection_range.end <= len
        );
        stack.extend(node.children.iter());
    }
}

fn assert_tokens(spans: &[HighlightSpan], source: &str, expected: &[(&str, SyntaxTokenKind)]) {
    let actual: Vec<(&str, SyntaxTokenKind)> = spans
        .iter()
        .map(|s| (&source[s.range.clone()], s.kind))
        .collect();
    for &(text, kind) in expected {
        assert!(
            spans
                .iter()
                .any(|s| &source[s.range.clone()] == text && s.kind == kind),
            "expected ({text:?}, {kind:?}) in {actual:?}"
        );
    }
}

#[test]
fn detects_sql_dialects_and_emits_no_entry_points() {
    for path in [
        "schema.sql",
        "V1__init.SQL",
        "tables.ddl",
        "MIGRATIONS.DDL",
        "queries.psql",
        "REPORT.PSQL",
        "analytics.hql",
        "ETL.HQL",
    ] {
        assert_eq!(
            detect_language(Path::new(path)),
            Some(SyntaxLanguage::Sql),
            "expected Sql for {path}"
        );
    }

    let (engine, _) = setup();
    let (snap, tree) = parse(&engine, "SELECT 1;\n");
    assert_eq!(detect_entry_points(&tree, &snap), Vec::new());
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "comprehensive SQL highlight assertion fixture"
)]
fn sql_highlights_keywords_types_functions_strings_params_and_comments() {
    let source = r#"/**
 * Orders analytics query (zażółć).
 */
/* block comment */
/**/
-- line comment
select
  u.id,
  u."full""name",
  `hive``col`,
  COUNT(*),
  SUM(o.total),
  AVG(o.total),
  COALESCE(u.nickname, 'it''s unknown'),
  CURRENT_TIMESTAMP,
  my_schema.calc_total(o.id),
  LEFT(u.email, 4),
  REPLACE(u.email, '@', '_at_'),
  FORMAT('%s', u.id),
  YEAR(o.created_at),
  CASE WHEN o.total > 100.50 THEN TRUE WHEN o.total <= .5 THEN FALSE ELSE NULL END AS tier,
  o.payload::jsonb,
  o.tags ?| ARRAY['vip'],
  o.tags ?& ARRAY['eu'],
  :user_id,
  $1,
  ?,
  ?2,
  E'hello\nworld\x20\u0041\'',
  $fn$SELECT 'nested' FROM dual;$fn$,
  0xFF,
  1_000_000,
  1e-5,
  2.5E+10
FROM public.users u
LEFT JOIN orders o ON o.user_id = u.id
WHERE u.active IS NOT UNKNOWN
GROUP BY u.id
ORDER BY u.id DESC
HAVING COUNT(*) > 0;

CREATE OR REPLACE TABLE items (
  id BIGINT PRIMARY KEY,
  code VARCHAR(255) NOT NULL,
  price NUMERIC(10, 2),
  meta JSONB,
  uid UUID,
  created_at TIMESTAMPTZ,
  is_ok BOOLEAN,
  year_num YEAR
);

INSERT INTO items (id, code) VALUES (1, 'A')
ON CONFLICT (id) DO UPDATE SET code = EXCLUDED.code
RETURNING id;

UPDATE items SET price = 9.99 WHERE id = 1;
DELETE FROM items WHERE id = 2;
"#;

    let (engine, highlighter) = setup();
    let (snap, tree) = parse(&engine, source);
    assert!(
        !tree.has_error(),
        "unexpected diagnostics: {:?}",
        collect_syntax_diagnostics(&tree, 10)
    );

    let spans = highlighter
        .highlight_range(&tree, &snap, 0..snap.len_bytes())
        .expect("highlight_range succeeds");
    assert_span_invariants(&spans, source, 0..snap.len_bytes());

    assert_tokens(
        &spans,
        source,
        &[
            (
                "/**\n * Orders analytics query (zażółć).\n */",
                SyntaxTokenKind::CommentDoc,
            ),
            ("/* block comment */", SyntaxTokenKind::Comment),
            ("/**/", SyntaxTokenKind::Comment),
            ("-- line comment", SyntaxTokenKind::Comment),
            ("select", SyntaxTokenKind::Keyword),
            ("FROM", SyntaxTokenKind::Keyword),
            ("LEFT", SyntaxTokenKind::Keyword),
            ("JOIN", SyntaxTokenKind::Keyword),
            ("WHERE", SyntaxTokenKind::Keyword),
            ("GROUP", SyntaxTokenKind::Keyword),
            ("ORDER", SyntaxTokenKind::Keyword),
            ("HAVING", SyntaxTokenKind::Keyword),
            ("CREATE", SyntaxTokenKind::Keyword),
            ("REPLACE", SyntaxTokenKind::Keyword),
            ("TABLE", SyntaxTokenKind::Keyword),
            ("INSERT", SyntaxTokenKind::Keyword),
            ("CONFLICT", SyntaxTokenKind::Keyword),
            ("RETURNING", SyntaxTokenKind::Keyword),
            ("UPDATE", SyntaxTokenKind::Keyword),
            ("DELETE", SyntaxTokenKind::Keyword),
            ("CASE", SyntaxTokenKind::KeywordControl),
            ("WHEN", SyntaxTokenKind::KeywordControl),
            ("THEN", SyntaxTokenKind::KeywordControl),
            ("ELSE", SyntaxTokenKind::KeywordControl),
            ("END", SyntaxTokenKind::KeywordControl),
            ("TRUE", SyntaxTokenKind::Boolean),
            ("FALSE", SyntaxTokenKind::Boolean),
            ("NULL", SyntaxTokenKind::Boolean),
            ("UNKNOWN", SyntaxTokenKind::Boolean),
            ("BIGINT", SyntaxTokenKind::TypeBuiltin),
            ("VARCHAR", SyntaxTokenKind::TypeBuiltin),
            ("NUMERIC", SyntaxTokenKind::TypeBuiltin),
            ("JSONB", SyntaxTokenKind::TypeBuiltin),
            ("UUID", SyntaxTokenKind::TypeBuiltin),
            ("TIMESTAMPTZ", SyntaxTokenKind::TypeBuiltin),
            ("BOOLEAN", SyntaxTokenKind::TypeBuiltin),
            ("YEAR", SyntaxTokenKind::TypeBuiltin),
            ("COUNT", SyntaxTokenKind::Function),
            ("SUM", SyntaxTokenKind::Function),
            ("AVG", SyntaxTokenKind::Function),
            ("COALESCE", SyntaxTokenKind::Function),
            ("CURRENT_TIMESTAMP", SyntaxTokenKind::Function),
            ("calc_total", SyntaxTokenKind::Function),
            ("LEFT", SyntaxTokenKind::Function),
            ("REPLACE", SyntaxTokenKind::Function),
            ("FORMAT", SyntaxTokenKind::Function),
            ("YEAR", SyntaxTokenKind::Function),
            ("id", SyntaxTokenKind::Property),
            ("users", SyntaxTokenKind::Property),
            ("\"full", SyntaxTokenKind::Property),
            ("name\"", SyntaxTokenKind::Property),
            ("\"\"", SyntaxTokenKind::StringEscape),
            ("`hive", SyntaxTokenKind::Property),
            ("col`", SyntaxTokenKind::Property),
            ("``", SyntaxTokenKind::StringEscape),
            ("''", SyntaxTokenKind::StringEscape),
            ("\\n", SyntaxTokenKind::StringEscape),
            ("\\x20\\u0041\\'", SyntaxTokenKind::StringEscape),
            (
                "$fn$SELECT 'nested' FROM dual;$fn$",
                SyntaxTokenKind::String,
            ),
            (":user_id", SyntaxTokenKind::Parameter),
            ("$1", SyntaxTokenKind::Parameter),
            ("?", SyntaxTokenKind::Parameter),
            ("?2", SyntaxTokenKind::Parameter),
            ("::", SyntaxTokenKind::Operator),
            ("jsonb", SyntaxTokenKind::TypeBuiltin),
            ("?|", SyntaxTokenKind::Operator),
            ("?&", SyntaxTokenKind::Operator),
            ("100.50", SyntaxTokenKind::Number),
            (".5", SyntaxTokenKind::Number),
            ("0xFF", SyntaxTokenKind::Number),
            ("1_000_000", SyntaxTokenKind::Number),
            ("1e-5", SyntaxTokenKind::Number),
            ("2.5E+10", SyntaxTokenKind::Number),
        ],
    );

    // Ensure `::jsonb` was NOT misparsed as parameter `:jsonb`.
    assert!(
        !spans
            .iter()
            .any(|s| &source[s.range.clone()] == ":jsonb" && s.kind == SyntaxTokenKind::Parameter)
    );
}

#[test]
fn sql_folds_blocks_comments_and_regions() {
    let source = "\
-- region Setup
-- First header line
-- Second header line
CREATE TABLE orders (
    id BIGINT PRIMARY KEY,
    status TEXT NOT NULL
);
-- endregion

/**
 * Multi-line doc comment
 */
/*
 * Multi-line block comment
 */
SELECT
    CASE
        WHEN id > 0 THEN 'positive'
        ELSE 'other'
    END
FROM orders;

CREATE PROCEDURE refresh_orders()
BEGIN
    IF TRUE THEN
        SELECT 1;
    END IF;
END;
";
    let (engine, _) = setup();
    let (snap, tree) = parse(&engine, source);
    assert!(
        !tree.has_error(),
        "unexpected diagnostics: {:?}",
        collect_syntax_diagnostics(&tree, 10)
    );

    let folds = extract_folds(&tree, &snap);
    assert_fold_invariants(&folds, &snap);

    assert!(
        folds
            .iter()
            .any(|f| f.kind == FoldKind::Region && f.placeholder == "...")
    );
    assert!(
        folds
            .iter()
            .any(|f| f.kind == FoldKind::Comment && f.placeholder == "-- ...")
    );
    assert!(
        folds
            .iter()
            .any(|f| f.kind == FoldKind::Comment && f.placeholder == "/** ... */")
    );
    assert!(
        folds
            .iter()
            .any(|f| f.kind == FoldKind::Comment && f.placeholder == "/* ... */")
    );
    assert!(
        folds
            .iter()
            .any(|f| f.kind == FoldKind::Block && f.placeholder == "(...)")
    );
    // Both `CASE ... END` and `BEGIN ... END` emit `FoldKind::Block` with `"..."`.
    assert!(
        folds
            .iter()
            .filter(|f| f.kind == FoldKind::Block && f.placeholder == "...")
            .count()
            >= 2
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "comprehensive SQL structure outline assertion fixture"
)]
fn sql_structure_outline_extracts_ddl_columns_and_ctes() {
    let source = "\
CREATE SCHEMA IF NOT EXISTS analytics;

CREATE TYPE order_status AS ENUM ('pending', 'paid');

CREATE SEQUENCE order_seq START 1;

CREATE DOMAIN positive_amount AS NUMERIC(10, 2);

CREATE TABLE IF NOT EXISTS public.orders (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id BIGINT NOT NULL REFERENCES public.users(id),
    amount NUMERIC(10, 2) DEFAULT 0,
    created_at TIMESTAMP WITH TIME ZONE NOT NULL,
    CONSTRAINT pk_orders PRIMARY KEY (id),
    FOREIGN KEY (user_id) REFERENCES public.users(id),
    UNIQUE (id),
    CHECK (amount >= 0),
    EXCLUDE USING btree (id WITH =)
);

CREATE OR REPLACE MATERIALIZED VIEW recent_orders AS
SELECT id, amount FROM public.orders;

CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS idx_orders_created ON public.orders (created_at);

CREATE OR REPLACE FUNCTION calc_tax(amount NUMERIC, rate NUMERIC)
RETURNS NUMERIC AS $$ BEGIN RETURN amount * rate; END; $$ LANGUAGE plpgsql;

CREATE PROCEDURE archive_orders()
BEGIN
    SELECT 1;
END;

CREATE TRIGGER trg_orders_audit AFTER INSERT ON public.orders FOR EACH ROW EXECUTE FUNCTION audit_fn();

WITH RECURSIVE
    seed_orders AS (
        SELECT id FROM public.orders
    ),
    flagged_orders(order_id) AS (
        SELECT id FROM seed_orders
    )
SELECT * FROM flagged_orders;
";
    let (engine, _) = setup();
    let (snap, tree) = parse(&engine, source);
    assert!(
        !tree.has_error(),
        "unexpected diagnostics: {:?}",
        collect_syntax_diagnostics(&tree, 10)
    );

    let outline = extract_structure(&tree, &snap);
    assert_structure_invariants(&outline, &snap);

    assert!(
        outline
            .roots
            .iter()
            .any(|n| n.name == "analytics" && n.detail.as_deref() == Some("schema"))
    );
    assert!(outline.roots.iter().any(|n| {
        n.name == "order_status"
            && n.kind == StructureSymbolKind::Enum
            && n.detail.as_deref() == Some("enum")
    }));
    assert!(
        outline
            .roots
            .iter()
            .any(|n| n.name == "order_seq" && n.detail.as_deref() == Some("sequence"))
    );
    assert!(
        outline
            .roots
            .iter()
            .any(|n| n.name == "positive_amount" && n.detail.as_deref() == Some("domain"))
    );

    let table = outline
        .roots
        .iter()
        .find(|n| n.name == "public.orders" && n.kind == StructureSymbolKind::Class)
        .expect("public.orders table in outline");
    assert_eq!(table.detail.as_deref(), Some("table"));
    let col_names: Vec<&str> = table.children.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(col_names, vec!["id", "user_id", "amount", "created_at"]);
    assert_eq!(table.children[0].kind, StructureSymbolKind::Field);
    assert_eq!(table.children[0].detail.as_deref(), Some("BIGINT"));
    assert_eq!(table.children[2].detail.as_deref(), Some("NUMERIC(10, 2)"));
    assert_eq!(
        table.children[3].detail.as_deref(),
        Some("TIMESTAMP WITH TIME ZONE")
    );

    assert!(
        outline
            .roots
            .iter()
            .any(|n| n.name == "recent_orders" && n.detail.as_deref() == Some("view"))
    );
    assert!(
        outline
            .roots
            .iter()
            .any(|n| n.name == "idx_orders_created" && n.detail.as_deref() == Some("index"))
    );
    assert!(
        outline
            .roots
            .iter()
            .any(|n| n.name == "calc_tax" && n.kind == StructureSymbolKind::Function)
    );
    assert!(
        outline
            .roots
            .iter()
            .any(|n| n.name == "archive_orders" && n.kind == StructureSymbolKind::Function)
    );
    assert!(
        outline
            .roots
            .iter()
            .any(|n| n.name == "trg_orders_audit" && n.detail.as_deref() == Some("trigger"))
    );
    assert!(
        outline
            .roots
            .iter()
            .any(|n| n.name == "seed_orders" && n.detail.as_deref() == Some("cte"))
    );
    assert!(
        outline
            .roots
            .iter()
            .any(|n| n.name == "flagged_orders" && n.detail.as_deref() == Some("cte"))
    );
}

#[test]
fn sql_transactions_do_not_error_and_broken_sql_reports_diagnostics() {
    let valid_tx = "\
BEGIN;
SELECT 1;
COMMIT;

BEGIN TRANSACTION;
UPDATE accounts SET balance = balance - 10;
ROLLBACK;

BEGIN ISOLATION LEVEL SERIALIZABLE;
SELECT 2;
COMMIT;

BEGIN
SELECT 3;
COMMIT;
";
    let (engine, _) = setup();
    let (_, tx_tree) = parse(&engine, valid_tx);
    assert_eq!(
        collect_syntax_diagnostics(&tx_tree, 10),
        Vec::new(),
        "transaction blocks must not report unclosed BEGIN errors"
    );

    let broken = "\
SELECT 'unclosed string
;
SELECT \"unclosed ident
FROM dual;
SELECT `unclosed backtick
FROM dual;
SELECT $$unclosed dollar;
";
    let (broken_snap, broken_tree) = parse(&engine, broken);
    assert!(broken_tree.has_error());
    let diags = collect_syntax_diagnostics(&broken_tree, 20);
    assert_ne!(diags.len(), 0);

    let (_, highlighter) = setup();
    let spans = highlighter
        .highlight_range(&broken_tree, &broken_snap, 0..broken_snap.len_bytes())
        .unwrap();
    assert_span_invariants(&spans, broken, 0..broken_snap.len_bytes());

    let broken_blocks = "CREATE TABLE t (\n  id INT\n;\nBEGIN\n  CASE WHEN 1 = 1 THEN 2\n";
    let (_, block_tree) = parse(&engine, broken_blocks);
    let block_diags = collect_syntax_diagnostics(&block_tree, 20);
    assert!(
        block_diags.len() >= 3,
        "expected unclosed `(`, `BEGIN`, and `CASE` diagnostics: {block_diags:?}"
    );
}

#[test]
fn sql_extend_selection_and_utf8_crlf_resilience() {
    let source = "SELECT (\r\n  'zażółć''🚀'\r\n);\r\n";
    let (engine, highlighter) = setup();
    let (snap, tree) = parse(&engine, source);
    assert!(!tree.has_error());

    let spans = highlighter
        .highlight_range(&tree, &snap, 0..snap.len_bytes())
        .unwrap();
    assert_span_invariants(&spans, source, 0..snap.len_bytes());

    let esc_pos = source.find("''").expect("escape present");
    let r1 = tree
        .enclosing_node_range(esc_pos, esc_pos + 2)
        .expect("expands to string");
    assert_eq!(&source[r1.clone()], "'zażółć''🚀'");

    let r2 = tree
        .enclosing_node_range(r1.start, r1.end)
        .expect("expands to parens");
    assert!(source[r2.clone()].starts_with('(') && source[r2.clone()].ends_with(')'));

    let r3 = tree
        .enclosing_node_range(r2.start, r2.end)
        .expect("expands to statement");
    assert!(source[r3.clone()].starts_with("SELECT") && source[r3].ends_with(';'));
}

#[test]
fn sql_rayon_async_cancellation_and_viewport_budget_10k_lines() {
    let (engine, highlighter) = setup();

    let mut sql = String::from("CREATE SCHEMA bench;\n");
    for idx in 0..2_500 {
        let _ = writeln!(
            sql,
            "SELECT u.id, 'zażółć_{idx}' AS label\nFROM users u\nWHERE u.id = {idx}\nORDER BY u.id;"
        );
    }

    let snap = Buffer::with_id(BufferId::new(99), &sql).snapshot();
    assert!(snap.len_lines() >= 10_000);
    let tree = engine
        .parse_sync(SyntaxLanguage::Sql, &snap, None)
        .expect("10k SQL parse ok");

    let mut best = Duration::MAX;
    for _ in 0..5 {
        let t0 = Instant::now();
        let spans = highlighter
            .highlight_lines(&tree, &snap, 4_960..5_040)
            .expect("viewport highlight ok");
        let elapsed = t0.elapsed();
        assert_ne!(spans.len(), 0);
        assert_span_invariants(
            &spans,
            &sql,
            snap.rope().line_to_byte(4_960)..snap.rope().line_to_byte(5_040),
        );
        best = best.min(elapsed);
    }
    assert!(
        best < Duration::from_millis(2),
        "SQL 80-line viewport highlight took {best:?} (budget < 2ms)"
    );

    // Asynchronous rayon parse + explicit cancellation.
    let (tx, rx) = mpsc::channel();
    let handle = engine.schedule_parse(
        ParseRequest::full(SyntaxLanguage::Sql, snap.clone()),
        move |res| {
            drop(tx.send(res));
        },
    );
    let res = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("async parse finished");
    assert!(
        res.is_ok(),
        "SQL async parse succeeded (handle version {:?})",
        handle.target_version()
    );

    let (tx2, rx2) = mpsc::channel();
    let cancel_handle =
        engine.schedule_parse(ParseRequest::full(SyntaxLanguage::Sql, snap), move |res| {
            drop(tx2.send(res));
        });
    cancel_handle.cancel();
    let res2 = rx2
        .recv_timeout(Duration::from_secs(5))
        .expect("cancelled job returned");
    assert!(matches!(
        res2,
        Ok(_) | Err(SyntaxError::ParseCancelled { .. })
    ));
}
