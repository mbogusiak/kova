//! Integration tests for KOVA-022: 5 Tree-sitter grammars, `highlights.scm` capture
//! completeness against `kova_theme`, visible line-range highlighting, and span invariants.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "integration tests assert query completeness and highlight span invariants"
)]

use kova_buffer::{Buffer, BufferSnapshot};
use kova_syntax::{
    HighlightSpan, Highlighter, LanguageRegistry, SyntaxEngine, SyntaxEngineConfig, SyntaxTree,
};
use kova_theme::{AetherTheme, BuiltinTheme, SyntaxLanguage, SyntaxTokenKind, resolve_capture};

const TREE_SITTER_LANGUAGES: [SyntaxLanguage; 5] = [
    SyntaxLanguage::Java,
    SyntaxLanguage::Kotlin,
    SyntaxLanguage::Http,
    SyntaxLanguage::Toml,
    SyntaxLanguage::Json,
];

const SCANNER_LANGUAGES: [SyntaxLanguage; 3] = [
    SyntaxLanguage::Yaml,
    SyntaxLanguage::Xml,
    SyntaxLanguage::Groovy,
];

fn setup() -> (SyntaxEngine, Highlighter) {
    let registry = LanguageRegistry::builtin().expect("builtin registry initializes");
    let highlighter = Highlighter::new(registry.clone());
    let engine = SyntaxEngine::new(registry.clone(), SyntaxEngineConfig::default())
        .expect("SyntaxEngine initializes");
    (engine, highlighter)
}

fn parse(
    engine: &SyntaxEngine,
    language: SyntaxLanguage,
    source: &str,
) -> (BufferSnapshot, SyntaxTree) {
    let buffer = Buffer::from_text(source);
    let snapshot = buffer.snapshot();
    let tree = engine
        .parse_sync(language, &snapshot, None)
        .expect("parse succeeds");
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
            "span {span:?} escaped expected bounds {expected_bounds:?}"
        );
        assert!(
            source.is_char_boundary(span.range.start) && source.is_char_boundary(span.range.end),
            "span {span:?} does not lie on UTF-8 char boundaries"
        );
    }

    for pair in spans.windows(2) {
        assert!(
            pair[0].range.end <= pair[1].range.start,
            "spans are not strictly sorted & disjoint: {:?} followed by {:?}",
            pair[0],
            pair[1]
        );
    }
}

fn span_tokens<'a>(spans: &'a [HighlightSpan], source: &'a str) -> Vec<(&'a str, SyntaxTokenKind)> {
    spans
        .iter()
        .map(|s| (&source[s.range.clone()], s.kind))
        .collect()
}

fn has_token(spans: &[HighlightSpan], source: &str, text: &str, kind: SyntaxTokenKind) -> bool {
    spans
        .iter()
        .any(|s| &source[s.range.clone()] == text && s.kind == kind)
}

fn assert_tokens(spans: &[HighlightSpan], source: &str, expected: &[(&str, SyntaxTokenKind)]) {
    let actual = span_tokens(spans, source);
    for &(text, kind) in expected {
        assert!(
            has_token(spans, source, text, kind),
            "expected ({text:?}, {kind:?}) in {actual:?}"
        );
    }
}

fn assert_kinds(spans: &[HighlightSpan], expected: &[SyntaxTokenKind]) {
    for &kind in expected {
        assert!(
            spans.iter().any(|s| s.kind == kind),
            "expected token kind {kind:?} in spans"
        );
    }
}

#[test]
fn all_default_grammars_and_queries_compile() {
    let registry = LanguageRegistry::with_defaults().expect("with_defaults succeeds");
    assert_eq!(registry.len(), 8);
    for lang in TREE_SITTER_LANGUAGES {
        assert!(registry.contains(lang), "missing {lang:?}");
        let entry = registry.get(lang).expect("entry exists");
        assert_eq!(entry.language(), lang);
        assert!(entry.is_tree_sitter());
        assert!(
            entry.highlight_query().is_some(),
            "missing highlight query for {lang:?}"
        );
    }
    for lang in SCANNER_LANGUAGES {
        assert!(registry.contains(lang), "missing {lang:?}");
        let entry = registry.get(lang).expect("entry exists");
        assert_eq!(entry.language(), lang);
        assert!(!entry.is_tree_sitter());
        assert!(entry.highlight_query().is_none());
    }
}

#[test]
fn all_query_captures_resolve_in_kova_theme() {
    let registry = LanguageRegistry::with_defaults().expect("with_defaults succeeds");
    for lang in TREE_SITTER_LANGUAGES {
        let entry = registry.get(lang).expect("entry exists");
        let hq = entry.highlight_query().expect("highlight query exists");
        let unmapped = hq.unmapped_captures();
        assert!(
            unmapped.is_empty(),
            "language {lang:?} has captures not mapped in kova_theme::resolve_capture: {unmapped:?}"
        );

        for (idx, &name) in hq.query().capture_names().iter().enumerate() {
            if !name.starts_with('_') {
                let resolved = resolve_capture(lang, name);
                assert!(
                    resolved.is_some(),
                    "capture @{name} in {lang:?} must resolve in kova_theme"
                );
                let idx_u32 = u32::try_from(idx).expect("capture count fits u32");
                assert_eq!(hq.capture_kind(idx_u32), resolved);
            }
        }
    }
}

#[test]
fn java_highlights_map_to_aether_tokens_and_viewport_lines() {
    let source = r#"package dev.kova.orders;

import java.util.List;

/**
 * Service handling orders.
 */
@Service
public class OrderService {
    private static final int MAX_RETRIES = 3;

    @Test
    public void processOrders(List<String> items) {
        // Check non-empty
        var count = items.size();
        String banner = """
                Hello\nKova 🚀
                """;
        if (count > 0 && banner != null) {
            items.forEach(item -> System.out.println(item));
            return;
        }
    }
}
"#;
    let (engine, highlighter) = setup();
    let (snap, tree) = parse(&engine, SyntaxLanguage::Java, source);
    assert!(!tree.has_error(), "Java sample should parse cleanly");

    let all_spans = highlighter
        .highlight_range(&tree, &snap, 0..snap.len_bytes())
        .expect("highlight_range succeeds");
    assert_span_invariants(&all_spans, source, 0..snap.len_bytes());
    assert_tokens(
        &all_spans,
        source,
        &[
            ("package", SyntaxTokenKind::Keyword),
            ("import", SyntaxTokenKind::Keyword),
            ("public", SyntaxTokenKind::Keyword),
            ("class", SyntaxTokenKind::Keyword),
            ("var", SyntaxTokenKind::Keyword),
            ("if", SyntaxTokenKind::KeywordControl),
            ("return", SyntaxTokenKind::KeywordControl),
            ("int", SyntaxTokenKind::TypeBuiltin),
            ("void", SyntaxTokenKind::TypeBuiltin),
            ("OrderService", SyntaxTokenKind::Type),
            ("String", SyntaxTokenKind::Type),
            ("@Service", SyntaxTokenKind::Annotation),
            ("@Test", SyntaxTokenKind::Annotation),
            ("processOrders", SyntaxTokenKind::Function),
            ("println", SyntaxTokenKind::Function),
            ("MAX_RETRIES", SyntaxTokenKind::Constant),
            ("items", SyntaxTokenKind::Parameter),
            ("3", SyntaxTokenKind::Number),
            ("null", SyntaxTokenKind::Boolean),
            ("\\n", SyntaxTokenKind::StringEscape),
        ],
    );
    assert_kinds(
        &all_spans,
        &[
            SyntaxTokenKind::CommentDoc,
            SyntaxTokenKind::Comment,
            SyntaxTokenKind::String,
        ],
    );

    // Verify AetherTheme resolution on highlighted spans.
    let dark = AetherTheme::builtin(BuiltinTheme::KovaIslandsDark);
    let doc_span = all_spans
        .iter()
        .find(|s| s.kind == SyntaxTokenKind::CommentDoc)
        .expect("Javadoc span present");
    assert_eq!(doc_span.style(&dark), &dark.tokens.syntax.comment_doc);
    assert!(doc_span.style(&dark).italic);

    // Visible line range: lines 11..19 cover @Test, processOrders, // comment, and text block.
    let line_spans = highlighter
        .highlight_lines(&tree, &snap, 11..19)
        .expect("highlight_lines succeeds");
    let start_b = snap.rope().line_to_byte(11);
    let end_b = snap.rope().line_to_byte(19);
    assert_span_invariants(&line_spans, source, start_b..end_b);
    assert_tokens(
        &line_spans,
        source,
        &[
            ("@Test", SyntaxTokenKind::Annotation),
            ("processOrders", SyntaxTokenKind::Function),
        ],
    );
    assert!(!has_token(
        &line_spans,
        source,
        "OrderService",
        SyntaxTokenKind::Type
    ));
}

#[test]
fn kotlin_highlights_cover_source_and_gradle_dsl_with_interpolation() {
    let source = r#"plugins {
    id("org.jetbrains.kotlin.jvm") version "2.1.0"
}

/**
 * KDoc summary for [Order].
 */
@Suppress("unused")
data class Order(val id: Long, val active: Boolean = true)

suspend fun formatOrder(order: Order?): String {
    // Fallback when null
    val label = order?.let { "Order #${it.id} = $active\n" } ?: "missing"
    return when {
        order == null -> label
        else -> label
    }
}
"#;
    let (engine, highlighter) = setup();
    let (snap, tree) = parse(&engine, SyntaxLanguage::Kotlin, source);
    assert!(!tree.has_error(), "Kotlin sample should parse cleanly");

    let spans = highlighter
        .highlight_range(&tree, &snap, 0..snap.len_bytes())
        .expect("highlight_range succeeds");
    assert_span_invariants(&spans, source, 0..snap.len_bytes());
    assert_tokens(
        &spans,
        source,
        &[
            ("plugins", SyntaxTokenKind::Function),
            ("id", SyntaxTokenKind::Function),
            ("version", SyntaxTokenKind::Function),
            ("let", SyntaxTokenKind::Function),
            ("formatOrder", SyntaxTokenKind::Function),
            ("data", SyntaxTokenKind::Keyword),
            ("class", SyntaxTokenKind::Keyword),
            ("suspend", SyntaxTokenKind::Keyword),
            ("fun", SyntaxTokenKind::Keyword),
            ("val", SyntaxTokenKind::Keyword),
            ("return", SyntaxTokenKind::KeywordControl),
            ("when", SyntaxTokenKind::KeywordControl),
            ("else", SyntaxTokenKind::KeywordControl),
            ("@Suppress", SyntaxTokenKind::Annotation),
            ("Long", SyntaxTokenKind::TypeBuiltin),
            ("Boolean", SyntaxTokenKind::TypeBuiltin),
            ("String", SyntaxTokenKind::TypeBuiltin),
            ("Order", SyntaxTokenKind::Type),
            ("true", SyntaxTokenKind::Boolean),
            ("null", SyntaxTokenKind::Boolean),
            ("${", SyntaxTokenKind::StringEscape),
            ("}", SyntaxTokenKind::StringEscape),
            ("\\n", SyntaxTokenKind::StringEscape),
            ("?.", SyntaxTokenKind::Operator),
            ("?:", SyntaxTokenKind::Operator),
            ("it", SyntaxTokenKind::Parameter),
        ],
    );
    assert_kinds(
        &spans,
        &[SyntaxTokenKind::CommentDoc, SyntaxTokenKind::Comment],
    );

    // Viewport slice around the `formatOrder` body (lines 10..18).
    let line_spans = highlighter
        .highlight_lines(&tree, &snap, 10..18)
        .expect("highlight_lines succeeds");
    let start_b = snap.rope().line_to_byte(10);
    let end_b = snap.rope().line_to_byte(18);
    assert_span_invariants(&line_spans, source, start_b..end_b);
    assert_tokens(
        &line_spans,
        source,
        &[("formatOrder", SyntaxTokenKind::Function)],
    );
    assert!(!has_token(
        &line_spans,
        source,
        "plugins",
        SyntaxTokenKind::Function
    ));
}

#[test]
fn http_highlights_map_method_url_headers_variables_and_status() {
    let source = r#"### Create order request
@baseUrl = https://api.kova.dev

POST {{baseUrl}}/v1/orders HTTP/1.1
Authorization: Bearer {{token}}
Content-Type: application/json

{"item": "coffee", "qty": 2}

### Response preview
HTTP/1.1 201 Created
Content-Type: application/json
"#;
    let (engine, highlighter) = setup();
    let (snap, tree) = parse(&engine, SyntaxLanguage::Http, source);
    assert!(!tree.has_error(), "HTTP sample should parse cleanly");

    let spans = highlighter
        .highlight_range(&tree, &snap, 0..snap.len_bytes())
        .expect("highlight_range succeeds");
    assert_span_invariants(&spans, source, 0..snap.len_bytes());
    assert_kinds(
        &spans,
        &[SyntaxTokenKind::Comment, SyntaxTokenKind::HttpUrl],
    );
    assert_tokens(
        &spans,
        source,
        &[
            ("POST", SyntaxTokenKind::HttpMethod),
            ("Authorization", SyntaxTokenKind::HttpHeader),
            ("Content-Type", SyntaxTokenKind::HttpHeader),
            ("HTTP/1.1", SyntaxTokenKind::HttpStatus),
            ("201", SyntaxTokenKind::HttpStatus),
            ("baseUrl", SyntaxTokenKind::Variable),
            ("{{", SyntaxTokenKind::StringEscape),
            ("}}", SyntaxTokenKind::StringEscape),
        ],
    );

    let dark = AetherTheme::builtin(BuiltinTheme::KovaIslandsDark);
    let post_span = spans
        .iter()
        .find(|s| s.kind == SyntaxTokenKind::HttpMethod)
        .expect("POST span present");
    assert_eq!(post_span.style(&dark), &dark.tokens.syntax.http_method);
    assert!(post_span.style(&dark).bold);

    // Viewport lines 3..6 cover the request line and headers.
    let line_spans = highlighter
        .highlight_lines(&tree, &snap, 3..6)
        .expect("highlight_lines succeeds");
    let start_b = snap.rope().line_to_byte(3);
    let end_b = snap.rope().line_to_byte(6);
    assert_span_invariants(&line_spans, source, start_b..end_b);
    assert_tokens(
        &line_spans,
        source,
        &[("POST", SyntaxTokenKind::HttpMethod)],
    );
    assert!(!has_token(
        &line_spans,
        source,
        "201",
        SyntaxTokenKind::HttpStatus
    ));
}

#[test]
fn toml_and_json_highlights_map_keys_literals_and_escapes() {
    let toml_source = r#"# Kova workspace settings
[editor]
tab_size = 4
font_scale = 1.25
line_numbers = true
greeting = "hello\nworld"
updated = 2026-11-25T16:00:00Z
"#;
    let json_source = r#"{
  "service": "kova-ide",
  "greeting": "zażółć\n🚀",
  "port": 8080,
  "tls": true,
  "proxy": null
}
"#;

    let (engine, highlighter) = setup();

    let (toml_snap, toml_tree) = parse(&engine, SyntaxLanguage::Toml, toml_source);
    assert!(!toml_tree.has_error());
    let toml_spans = highlighter
        .highlight_range(&toml_tree, &toml_snap, 0..toml_snap.len_bytes())
        .expect("toml highlight");
    assert_span_invariants(&toml_spans, toml_source, 0..toml_snap.len_bytes());
    assert_kinds(&toml_spans, &[SyntaxTokenKind::Comment]);
    assert_tokens(
        &toml_spans,
        toml_source,
        &[
            ("editor", SyntaxTokenKind::Property),
            ("tab_size", SyntaxTokenKind::Property),
            ("4", SyntaxTokenKind::Number),
            ("1.25", SyntaxTokenKind::Number),
            ("true", SyntaxTokenKind::Boolean),
            ("\\n", SyntaxTokenKind::StringEscape),
            ("2026-11-25T16:00:00Z", SyntaxTokenKind::String),
        ],
    );

    let (json_snap, json_tree) = parse(&engine, SyntaxLanguage::Json, json_source);
    assert!(!json_tree.has_error());
    let json_spans = highlighter
        .highlight_range(&json_tree, &json_snap, 0..json_snap.len_bytes())
        .expect("json highlight");
    assert_span_invariants(&json_spans, json_source, 0..json_snap.len_bytes());
    assert_tokens(
        &json_spans,
        json_source,
        &[
            ("\"service\"", SyntaxTokenKind::Property),
            ("\"kova-ide\"", SyntaxTokenKind::String),
            ("\\n", SyntaxTokenKind::StringEscape),
            ("8080", SyntaxTokenKind::Number),
            ("true", SyntaxTokenKind::Boolean),
            ("null", SyntaxTokenKind::Boolean),
        ],
    );

    // Verify light theme style lookup too.
    let light = AetherTheme::builtin(BuiltinTheme::KovaLight);
    for span in &json_spans {
        let _style = span.style(&light);
    }
}

#[test]
fn highlight_spans_are_sorted_and_disjoint_with_syntax_errors_utf8_and_arbitrary_viewports() {
    let samples: [(SyntaxLanguage, &str); 5] = [
        (
            SyntaxLanguage::Java,
            "public class Broken {\n  // zażółć 🚀\n  String s = \"a\\n${unclosed\\t\";\n  public void f(int x) {\n    if (x >\n",
        ),
        (
            SyntaxLanguage::Kotlin,
            "fun test(name: String = \"zażółć\\n${1 + 2} 🚀\") {\n  val broken = listOf(1, 2,\n",
        ),
        (
            SyntaxLanguage::Http,
            "### Broken 🚀\nGET https://{{host}}/items?q=zażółć\nX-Test: val\n\n{\"a\": \n",
        ),
        (
            SyntaxLanguage::Toml,
            "[table]\nkey = \"zażółć\\t🚀\"\nbroken = [1, 2,\n",
        ),
        (
            SyntaxLanguage::Json,
            "{\n  \"emoji\": \"🚀\\nzażółć\",\n  \"broken\": [1, true, \n",
        ),
    ];

    let (engine, highlighter) = setup();
    for (lang, src) in samples {
        let (snap, tree) = parse(&engine, lang, src);

        // Full range invariants even when the tree contains syntax errors.
        let full = highlighter
            .highlight_range(&tree, &snap, 0..snap.len_bytes())
            .expect("full range highlight");
        assert_span_invariants(&full, src, 0..snap.len_bytes());
        assert!(
            !full.is_empty(),
            "partial valid tokens must still highlight for {lang:?}"
        );

        // Arbitrary line windows, including clamped bounds past `len_lines()`.
        let total_lines = snap.len_lines();
        for start in 0..=total_lines {
            for end in start..=(total_lines + 2) {
                let spans = highlighter
                    .highlight_lines(&tree, &snap, start..end)
                    .expect("highlight_lines succeeds");
                let clamped_start = start.min(total_lines);
                let clamped_end = end.min(total_lines);
                let start_b = snap.rope().line_to_byte(clamped_start);
                let end_b = if clamped_end >= total_lines {
                    snap.len_bytes()
                } else {
                    snap.rope().line_to_byte(clamped_end)
                };
                assert_span_invariants(&spans, src, start_b..end_b);
            }
        }
    }
}
