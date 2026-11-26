//! Integration tests for KOVA-056: YAML, XML, and Groovy detection, highlighting,
//! folding, structure outline, extend selection, syntax diagnostics, `rayon` cancellation,
//! and 120 FPS viewport highlighting performance.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "integration tests assert scanner invariants and parsed results"
)]

use std::cmp::Reverse;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use kova_buffer::{Buffer, BufferId, BufferSnapshot};
use kova_syntax::{
    FoldKind, FoldRange, HighlightSpan, Highlighter, LanguageRegistry, MAX_STRUCTURE_DEPTH,
    ParseRequest, StructureNode, StructureOutline, StructureSymbolKind, SyntaxEngine,
    SyntaxEngineConfig, SyntaxError, SyntaxTree, collect_syntax_diagnostics, detect_entry_points,
    detect_language, extract_folds, extract_structure,
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

fn parse(
    engine: &SyntaxEngine,
    language: SyntaxLanguage,
    source: &str,
) -> (BufferSnapshot, SyntaxTree) {
    let buffer = Buffer::with_id(BufferId::new(1), source);
    let snapshot = buffer.snapshot();
    let tree = engine
        .parse_sync(language, &snapshot, None)
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
fn detects_yaml_xml_groovy_and_preserves_gradle_kts() {
    assert_eq!(
        detect_language(Path::new("application.yaml")),
        Some(SyntaxLanguage::Yaml)
    );
    assert_eq!(
        detect_language(Path::new("compose.YML")),
        Some(SyntaxLanguage::Yaml)
    );
    assert_eq!(
        detect_language(Path::new("pom.xml")),
        Some(SyntaxLanguage::Xml)
    );
    assert_eq!(
        detect_language(Path::new("artifact.POM")),
        Some(SyntaxLanguage::Xml)
    );
    assert_eq!(
        detect_language(Path::new("schema.Xsd")),
        Some(SyntaxLanguage::Xml)
    );
    assert_eq!(
        detect_language(Path::new("Script.groovy")),
        Some(SyntaxLanguage::Groovy)
    );
    assert_eq!(
        detect_language(Path::new("build.gradle")),
        Some(SyntaxLanguage::Groovy)
    );
    assert_eq!(
        detect_language(Path::new("settings.GRADLE")),
        Some(SyntaxLanguage::Groovy)
    );
    assert_eq!(
        detect_language(Path::new("build.gradle.kts")),
        Some(SyntaxLanguage::Kotlin)
    );

    let (engine, _) = setup();
    for lang in [
        SyntaxLanguage::Yaml,
        SyntaxLanguage::Xml,
        SyntaxLanguage::Groovy,
    ] {
        let (snap, tree) = parse(&engine, lang, "example\n");
        assert!(tree.tree().is_none());
        assert!(tree.root_node().is_none());
        assert_eq!(detect_entry_points(&tree, &snap).len(), 0);
    }
}

#[test]
fn yaml_highlights_folds_structure_selection_and_diagnostics() {
    let source = "\
%YAML 1.2
---
# region Server Settings
# First comment line
# Second comment line
defaults: &base
  host: \"localhost\\n ${ENV}\"
  url: https://example.com:8080/api#frag
  note: 'it''s {{ok}}'
server:
  <<: *base
  port: 8080
  ratio: -3.14
  hex: 0xFF
  oct: 0o755
  limit: .inf
  enabled: true
  legacy: null
  tagged: !!str zażółć 🚀
  script: |-
    echo \"hello\"
    # not a comment inside block scalar
  flow_map: {
    alpha: 1,
    beta: false
  }
  items: [
    \"one\",
    \"two\"
  ]
# endregion
...
";
    let (engine, highlighter) = setup();
    let (snap, tree) = parse(&engine, SyntaxLanguage::Yaml, source);
    assert!(!tree.has_error(), "valid YAML has no syntax errors");

    let spans = highlighter
        .highlight_range(&tree, &snap, 0..snap.len_bytes())
        .expect("highlight_range succeeds");
    assert_span_invariants(&spans, source, 0..snap.len_bytes());
    assert_tokens(
        &spans,
        source,
        &[
            ("%YAML", SyntaxTokenKind::Keyword),
            ("---", SyntaxTokenKind::Keyword),
            ("...", SyntaxTokenKind::Keyword),
            ("defaults", SyntaxTokenKind::Property),
            ("&base", SyntaxTokenKind::Annotation),
            ("*base", SyntaxTokenKind::Annotation),
            ("<<", SyntaxTokenKind::Operator),
            ("!!str", SyntaxTokenKind::Type),
            ("|-", SyntaxTokenKind::Operator),
            ("8080", SyntaxTokenKind::Number),
            ("-3.14", SyntaxTokenKind::Number),
            ("0xFF", SyntaxTokenKind::Number),
            ("0o755", SyntaxTokenKind::Number),
            (".inf", SyntaxTokenKind::Number),
            ("true", SyntaxTokenKind::Boolean),
            ("null", SyntaxTokenKind::Boolean),
            ("\\n", SyntaxTokenKind::StringEscape),
            ("${ENV}", SyntaxTokenKind::StringEscape),
            ("''", SyntaxTokenKind::StringEscape),
            ("{{ok}}", SyntaxTokenKind::StringEscape),
            ("https://example.com:8080/api#frag", SyntaxTokenKind::String),
            ("# First comment line", SyntaxTokenKind::Comment),
            ("alpha", SyntaxTokenKind::Property),
            ("{", SyntaxTokenKind::Punctuation),
        ],
    );

    let folds = extract_folds(&tree, &snap);
    assert_fold_invariants(&folds, &snap);
    assert!(folds.iter().any(|f| f.kind == FoldKind::Region));
    assert!(
        folds
            .iter()
            .any(|f| f.kind == FoldKind::Comment && f.placeholder == "# ...")
    );
    assert!(
        folds
            .iter()
            .any(|f| f.kind == FoldKind::Block && f.placeholder == "...")
    );
    assert!(
        folds
            .iter()
            .any(|f| f.kind == FoldKind::Block && f.placeholder == "{...}")
    );
    assert!(
        folds
            .iter()
            .any(|f| f.kind == FoldKind::Block && f.placeholder == "[...]")
    );
}

#[test]
fn yaml_structure_extend_selection_and_diagnostics() {
    let source = "\
defaults: &base
  host: \"localhost\"
server:
  <<: *base
  port: 8080
";
    let (engine, _) = setup();
    let (snap, tree) = parse(&engine, SyntaxLanguage::Yaml, source);
    let outline = extract_structure(&tree, &snap);
    assert_structure_invariants(&outline, &snap);
    let server = outline
        .roots
        .iter()
        .find(|n| n.name == "server")
        .expect("server root present");
    assert_eq!(server.kind, StructureSymbolKind::Section);
    let port = server
        .children
        .iter()
        .find(|n| n.name == "port")
        .expect("port child present");
    assert_eq!(port.kind, StructureSymbolKind::Property);
    assert_eq!(outline.path_at_byte(port.selection_range.start).len(), 2);

    // Alt+Up Extend Selection climbs from scalar token -> entry -> server block -> document.
    let port_val_pos = source.find("8080\n").unwrap();
    let r1 = tree
        .enclosing_node_range(port_val_pos, port_val_pos)
        .unwrap();
    assert_eq!(&source[r1.clone()], "8080");
    let r2 = tree.enclosing_node_range(r1.start, r1.end).unwrap();
    assert!(r2.len() > r1.len());

    // Broken YAML diagnostics (tab indent + unclosed quote + unclosed flow).
    let broken = "server:\n\tport: 80\nitems: [1, 2,\n\"unclosed\n";
    let (_, broken_tree) = parse(&engine, SyntaxLanguage::Yaml, broken);
    assert!(broken_tree.has_error());
    let diags = collect_syntax_diagnostics(&broken_tree, 10);
    assert!(diags.len() >= 3);
}

#[test]
fn xml_highlights_folds_structure_resilience_and_diagnostics() {
    let source = "\
<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<!DOCTYPE project>
<!-- region Dependencies -->
<!-- Multi-line
     XML comment -->
<project xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">
  <dependencies>
    <dependency id=\"core\">
      <groupId>dev.kova</groupId>
      <artifactId>kova-core &amp; &#x1F680;</artifactId>
    </dependency>
    <xs:element
      name=\"order\"
      type=\"xs:string\"/>
  </dependencies>
  <script><![CDATA[
    if (a < b && c > d) {}
  ]]></script>
</project>
<!-- endregion -->
";
    let (engine, highlighter) = setup();
    let (snap, tree) = parse(&engine, SyntaxLanguage::Xml, source);
    assert!(!tree.has_error(), "valid XML has no diagnostics");

    let spans = highlighter
        .highlight_range(&tree, &snap, 0..snap.len_bytes())
        .expect("highlight_range succeeds");
    assert_span_invariants(&spans, source, 0..snap.len_bytes());
    assert_tokens(
        &spans,
        source,
        &[
            ("<?", SyntaxTokenKind::Punctuation),
            ("xml", SyntaxTokenKind::Keyword),
            ("version", SyntaxTokenKind::Property),
            ("DOCTYPE", SyntaxTokenKind::Keyword),
            ("project", SyntaxTokenKind::Keyword),
            ("xmlns:xsi", SyntaxTokenKind::Property),
            ("xs", SyntaxTokenKind::Type),
            ("element", SyntaxTokenKind::Keyword),
            ("&amp;", SyntaxTokenKind::StringEscape),
            ("&#x1F680;", SyntaxTokenKind::StringEscape),
            ("<![CDATA[", SyntaxTokenKind::Keyword),
            ("]]>", SyntaxTokenKind::Keyword),
        ],
    );

    let folds = extract_folds(&tree, &snap);
    assert_fold_invariants(&folds, &snap);
    assert!(folds.iter().any(|f| f.kind == FoldKind::Region));
    assert!(
        folds
            .iter()
            .any(|f| f.kind == FoldKind::Comment && f.placeholder == "<!-- ... -->")
    );
    assert!(
        folds
            .iter()
            .any(|f| f.kind == FoldKind::Block && f.placeholder == "<![CDATA[ ... ]]>")
    );
    assert!(
        folds
            .iter()
            .any(|f| f.kind == FoldKind::Block && f.placeholder == "...")
    );

    let outline = extract_structure(&tree, &snap);
    assert_structure_invariants(&outline, &snap);
    let project = &outline.roots[0];
    assert_eq!(project.name, "project");
    assert_eq!(project.kind, StructureSymbolKind::Section);
    let deps = &project.children[0];
    assert_eq!(deps.name, "dependencies");
    assert_eq!(deps.children[0].name, "dependency");
    assert_eq!(deps.children[0].detail.as_deref(), Some("core"));
    assert_eq!(deps.children[1].name, "xs:element");
    assert_eq!(deps.children[1].detail.as_deref(), Some("order"));
    assert_eq!(deps.children[1].kind, StructureSymbolKind::Property);

    // Partial/incomplete XML during editing must not panic or suppress highlighting of later tags.
    let partial = "<dependencies>\n  <dependency\n  <artifactId>kova</artifactId>\n</wrong>\n";
    let (partial_snap, partial_tree) = parse(&engine, SyntaxLanguage::Xml, partial);
    assert!(partial_tree.has_error());
    let partial_spans = highlighter
        .highlight_range(&partial_tree, &partial_snap, 0..partial_snap.len_bytes())
        .expect("partial XML highlights");
    assert_span_invariants(&partial_spans, partial, 0..partial_snap.len_bytes());
    assert_tokens(
        &partial_spans,
        partial,
        &[("artifactId", SyntaxTokenKind::Keyword)],
    );
    assert_ne!(collect_syntax_diagnostics(&partial_tree, 10).len(), 0);
}

const GROOVY_SAMPLE: &str = "\
#!/usr/bin/env groovy
package dev.kova.build

import groovy.transform.CompileStatic
import java.util.List

/**
 * Groovydoc comment.
 */
// region Gradle DSL
plugins {
    id 'java'
}

tasks.named('test') {
    useJUnitPlatform()
}
// endregion

@CompileStatic
class OrderService extends BaseService {
    private static final int MAX_RETRIES = 3
    String label = \"zażółć\\n\"

    def greet(String name) {
        /* block
           comment */
        def msg = \"Hello ${name.toUpperCase()} $label\"
        def multi = '''line1\\nline2'''
        def regex = /order-\\d+/
        def slashy = $/path/$
        def ratio = 10 / 2
        def map = [group: 'dev.kova', version: 1_000L, pi: 3.14G, mask: 0xFF, bits: 0b1010]
        if (name =~ regex && ratio > 0 ?: false) {
            return [1, 2, 3].collect { it * 2 }
        }
        return []
    }
}
";

#[test]
fn groovy_highlights_and_folds() {
    let source = GROOVY_SAMPLE;
    let (engine, highlighter) = setup();
    let (snap, tree) = parse(&engine, SyntaxLanguage::Groovy, source);
    assert!(
        !tree.has_error(),
        "valid Groovy has no diagnostics: {:?}",
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
            ("#!/usr/bin/env groovy", SyntaxTokenKind::Comment),
            ("package", SyntaxTokenKind::Keyword),
            ("import", SyntaxTokenKind::Keyword),
            ("class", SyntaxTokenKind::Keyword),
            ("def", SyntaxTokenKind::Keyword),
            ("extends", SyntaxTokenKind::Keyword),
            ("if", SyntaxTokenKind::KeywordControl),
            ("return", SyntaxTokenKind::KeywordControl),
            ("int", SyntaxTokenKind::TypeBuiltin),
            ("@CompileStatic", SyntaxTokenKind::Annotation),
            ("OrderService", SyntaxTokenKind::Type),
            ("MAX_RETRIES", SyntaxTokenKind::Constant),
            ("plugins", SyntaxTokenKind::Function),
            ("id", SyntaxTokenKind::Function),
            ("greet", SyntaxTokenKind::Function),
            ("name", SyntaxTokenKind::Parameter),
            ("it", SyntaxTokenKind::Parameter),
            ("group", SyntaxTokenKind::Property),
            ("false", SyntaxTokenKind::Boolean),
            ("${", SyntaxTokenKind::StringEscape),
            ("$", SyntaxTokenKind::StringEscape),
            ("\\n", SyntaxTokenKind::StringEscape),
            ("1_000L", SyntaxTokenKind::Number),
            ("3.14G", SyntaxTokenKind::Number),
            ("0xFF", SyntaxTokenKind::Number),
            ("0b1010", SyntaxTokenKind::Number),
            ("=~", SyntaxTokenKind::Operator),
            ("?:", SyntaxTokenKind::Operator),
        ],
    );
    assert!(spans.iter().any(|s| s.kind == SyntaxTokenKind::CommentDoc));

    let folds = extract_folds(&tree, &snap);
    assert_fold_invariants(&folds, &snap);
    assert!(
        folds
            .iter()
            .any(|f| f.kind == FoldKind::Imports && f.default_collapsed)
    );
    assert!(
        folds
            .iter()
            .any(|f| f.kind == FoldKind::Comment && f.placeholder == "/** ... */")
    );
    assert!(folds.iter().any(|f| f.kind == FoldKind::Region));
    assert!(
        folds
            .iter()
            .any(|f| f.kind == FoldKind::Type && f.placeholder == "{...}")
    );
    assert!(
        folds
            .iter()
            .any(|f| f.kind == FoldKind::Function && f.placeholder == "{...}")
    );
    assert!(
        folds
            .iter()
            .any(|f| f.kind == FoldKind::Block && f.placeholder == "{...}")
    );
}

#[test]
fn groovy_structure_and_diagnostics() {
    let source = "\
plugins {
    id 'java'
}

tasks.named('test') {
    useJUnitPlatform()
}

class OrderService extends BaseService {
    private static final int MAX_RETRIES = 3

    def greet(String name) {
        return name
    }
}
";
    let (engine, highlighter) = setup();
    let (snap, tree) = parse(&engine, SyntaxLanguage::Groovy, source);
    let outline = extract_structure(&tree, &snap);
    assert_structure_invariants(&outline, &snap);
    assert!(
        outline
            .roots
            .iter()
            .any(|n| n.name == "plugins" && n.kind == StructureSymbolKind::Section)
    );
    assert!(
        outline
            .roots
            .iter()
            .any(|n| n.name == "tasks.named" && n.kind == StructureSymbolKind::Section)
    );
    let cls = outline
        .roots
        .iter()
        .find(|n| n.name == "OrderService")
        .expect("OrderService class in outline");
    assert_eq!(cls.kind, StructureSymbolKind::Class);
    assert!(
        cls.children
            .iter()
            .any(|c| c.name == "MAX_RETRIES" && c.kind == StructureSymbolKind::Field)
    );
    assert!(
        cls.children
            .iter()
            .any(|c| c.name == "greet" && c.kind == StructureSymbolKind::Method)
    );

    // Broken Groovy diagnostics & UTF-8 CRLF resilience.
    let broken = "class Broken {\r\n  def s = \"zażółć 🚀\r\n  /* unclosed\r\n";
    let (broken_snap, broken_tree) = parse(&engine, SyntaxLanguage::Groovy, broken);
    assert!(broken_tree.has_error());
    assert_ne!(collect_syntax_diagnostics(&broken_tree, 10).len(), 0);
    let b_spans = highlighter
        .highlight_range(&broken_tree, &broken_snap, 0..broken_snap.len_bytes())
        .unwrap();
    assert_span_invariants(&b_spans, broken, 0..broken_snap.len_bytes());
}

#[test]
fn groovy_entry_points_detect_junit_tests_and_ignore_gradle_scripts() {
    let (engine, _) = setup();

    // Gradle DSL scripts must emit zero entry points.
    let gradle_src = "\
plugins {
    id 'java'
}

tasks.named('test') {
    useJUnitPlatform()
}
";
    let (gradle_snap, gradle_tree) = parse(&engine, SyntaxLanguage::Groovy, gradle_src);
    assert_eq!(detect_entry_points(&gradle_tree, &gradle_snap).len(), 0);

    // Groovy JUnit 5 test class with @Test, @ParameterizedTest, @Nested, and comments.
    let test_src = "\
package com.kova.orders

import org.junit.jupiter.api.Nested
import org.junit.jupiter.api.Test

class OrderServiceTest {
    // @Test void commentedOut() {}

    @Test
    void shouldCreateOrder() {
        assert true
    }

    @org.junit.jupiter.api.ParameterizedTest
    def \"should reject negative amount\"(int amount) {
        assert amount < 0
    }

    static void main(String[] args) {
        println 'main'
    }

    @Nested
    class NestedTests {
        @Test
        void innerCheck() {
            assert 1 == 1
        }
    }
}
";
    let (test_snap, test_tree) = parse(&engine, SyntaxLanguage::Groovy, test_src);
    let eps = detect_entry_points(&test_tree, &test_snap);
    let targets: Vec<(&str, kova_syntax::EntryPointKind)> =
        eps.iter().map(|ep| (ep.id.as_str(), ep.kind)).collect();
    assert!(targets.contains(&(
        "com.kova.orders.OrderServiceTest",
        kova_syntax::EntryPointKind::JvmTestClass
    )));
    assert!(targets.contains(&(
        "com.kova.orders.OrderServiceTest#shouldCreateOrder",
        kova_syntax::EntryPointKind::JvmTestMethod
    )));
    assert!(targets.contains(&(
        "com.kova.orders.OrderServiceTest#should reject negative amount",
        kova_syntax::EntryPointKind::JvmTestMethod
    )));
    assert!(targets.contains(&(
        "com.kova.orders.OrderServiceTest#main",
        kova_syntax::EntryPointKind::JavaMain
    )));
    assert!(targets.contains(&(
        "com.kova.orders.OrderServiceTest$NestedTests",
        kova_syntax::EntryPointKind::JvmTestClass
    )));
    assert!(targets.contains(&(
        "com.kova.orders.OrderServiceTest$NestedTests#innerCheck",
        kova_syntax::EntryPointKind::JvmTestMethod
    )));
    assert_eq!(eps.len(), 6);
}

#[test]
fn deep_nesting_respects_max_structure_depth() {
    let (engine, _) = setup();
    let mut xml = String::new();
    for i in 0..200 {
        let _ = write!(xml, "<n{i}>");
    }
    for i in (0..200).rev() {
        let _ = write!(xml, "</n{i}>");
    }
    let (snap, tree) = parse(&engine, SyntaxLanguage::Xml, &xml);
    let outline = extract_structure(&tree, &snap);
    assert_structure_invariants(&outline, &snap);
    assert!(outline.symbol_count() <= MAX_STRUCTURE_DEPTH);
}

#[test]
fn rayon_async_cancellation_and_viewport_budget_10k_lines() {
    let (engine, highlighter) = setup();

    // Build 10k+ lines for YAML, XML, and Groovy and verify viewport highlighting < 2 ms.
    let mut yaml = String::from("services:\n");
    for idx in 0..2_500 {
        let _ = writeln!(
            yaml,
            "  svc_{idx}:\n    port: {idx}\n    enabled: true\n    name: \"zażółć_{idx}\""
        );
    }
    let mut xml = String::from("<root>\n");
    for idx in 0..2_500 {
        let _ = writeln!(
            xml,
            "  <item id=\"{idx}\">\n    <name>v_{idx}</name>\n    <ok>true</ok>\n  </item>"
        );
    }
    xml.push_str("</root>\n");
    let mut groovy = String::from("class BigBuild {\n");
    for idx in 0..2_500 {
        let _ = writeln!(
            groovy,
            "    def task{idx}() {{\n        println \"step_${{idx}}\"\n        return {idx}\n    }}"
        );
    }
    groovy.push_str("}\n");

    for (lang, text) in [
        (SyntaxLanguage::Yaml, yaml),
        (SyntaxLanguage::Xml, xml),
        (SyntaxLanguage::Groovy, groovy),
    ] {
        let snap = Buffer::with_id(BufferId::new(50), &text).snapshot();
        assert!(snap.len_lines() >= 10_000);
        let tree = engine.parse_sync(lang, &snap, None).expect("10k parse ok");

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
                &text,
                snap.rope().line_to_byte(4_960)..snap.rope().line_to_byte(5_040),
            );
            best = best.min(elapsed);
        }
        assert!(
            best < Duration::from_millis(2),
            "{lang:?} 80-line viewport highlight took {best:?} (budget < 2ms)"
        );

        // Asynchronous rayon parse + explicit cancellation.
        let (tx, rx) = mpsc::channel();
        let handle = engine.schedule_parse(ParseRequest::full(lang, snap.clone()), move |res| {
            drop(tx.send(res));
        });
        let res = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("async parse finished");
        assert!(
            res.is_ok(),
            "{lang:?} async parse succeeded (handle version {:?})",
            handle.target_version()
        );

        let (tx2, rx2) = mpsc::channel();
        let cancel_handle = engine.schedule_parse(ParseRequest::full(lang, snap), move |res| {
            drop(tx2.send(res));
        });
        cancel_handle.cancel();
        let res2 = rx2
            .recv_timeout(Duration::from_secs(5))
            .expect("cancelled job returned");
        // Either cancelled in flight or already completed if faster than cancel store.
        assert!(matches!(
            res2,
            Ok(_) | Err(SyntaxError::ParseCancelled { .. })
        ));
    }
}
