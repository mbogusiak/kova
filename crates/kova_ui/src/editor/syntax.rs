//! Immutable per-revision syntax data delivered to [`EditorView`](crate::editor::EditorView)
//! from the background `kova_syntax` analysis (`EditorSyntax`), plus the gutter run request
//! (`RunEntryPointRequested`) raised by clicking a `▶` icon.

use std::fmt;
use std::ops::Range;
use std::path::PathBuf;

use kova_buffer::BufferSnapshot;
use kova_syntax::{
    EntryPoint, EntryPointKind, HighlightSpan, Highlighter, SyntaxTree, detect_entry_points,
    extract_folds,
};

use crate::editor::folding::FoldCandidate;

/// Entry points enclosing a queried line inside an [`EditorSyntax`] snapshot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EnclosingEntryPoints<'a> {
    /// Narrowest non-class entry point (`JvmTestMethod`, `JavaMain`, `KotlinMain`, `HttpRequest`)
    /// covering the queried line.
    pub method: Option<&'a EntryPoint>,
    /// Narrowest enclosing `JvmTestClass` covering the queried line.
    pub class: Option<&'a EntryPoint>,
}

/// Syntax snapshot for one buffer revision: the parsed tree, the exact [`BufferSnapshot`] it
/// was parsed from (all byte offsets refer to it), fold candidates and runnable entry points.
///
/// Built off the UI thread by [`EditorSyntax::build`]; the editor only runs cheap viewport
/// highlight queries against it while painting.
pub struct EditorSyntax {
    tree: SyntaxTree,
    snapshot: BufferSnapshot,
    highlighter: Highlighter,
    folds: Vec<FoldCandidate>,
    entry_points: Vec<EntryPoint>,
}

impl fmt::Debug for EditorSyntax {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EditorSyntax")
            .field("language", &self.tree.language())
            .field("version", &self.snapshot.version())
            .field("folds", &self.folds.len())
            .field("entry_points", &self.entry_points.len())
            .finish_non_exhaustive()
    }
}

impl EditorSyntax {
    /// Extracts folds and entry points for `tree` (parsed from `snapshot`).
    ///
    /// Must run on a background executor: it walks the whole syntax tree.
    #[must_use]
    pub fn build(tree: SyntaxTree, snapshot: BufferSnapshot, highlighter: Highlighter) -> Self {
        let mut folds: Vec<FoldCandidate> = extract_folds(&tree, &snapshot)
            .iter()
            .map(FoldCandidate::from)
            .collect();
        folds.sort_unstable_by_key(|f| (f.start_line, f.end_line));
        let mut entry_points = detect_entry_points(&tree, &snapshot);
        entry_points.sort_by_key(|e| (e.line, e.byte_range.start));
        Self {
            tree,
            snapshot,
            highlighter,
            folds,
            entry_points,
        }
    }

    /// Returns the buffer snapshot the tree was parsed from.
    #[must_use]
    pub const fn snapshot(&self) -> &BufferSnapshot {
        &self.snapshot
    }

    /// Returns fold candidates sorted by `(start_line, end_line)`.
    #[must_use]
    pub fn folds(&self) -> &[FoldCandidate] {
        &self.folds
    }

    /// Returns runnable entry points sorted by line.
    #[must_use]
    pub fn entry_points(&self) -> &[EntryPoint] {
        &self.entry_points
    }

    /// Returns `true` when a fold candidate starts at `line`.
    #[must_use]
    pub fn has_fold_at(&self, line: usize) -> bool {
        let idx = self.folds.partition_point(|f| f.start_line < line);
        self.folds.get(idx).is_some_and(|f| f.start_line == line)
    }

    /// Returns the first entry point anchored at `line` (the `▶` gutter icon).
    #[must_use]
    pub fn entry_point_at(&self, line: usize) -> Option<&EntryPoint> {
        let idx = self.entry_points.partition_point(|e| e.line < line);
        self.entry_points.get(idx).filter(|e| e.line == line)
    }

    /// Returns the narrowest runnable method/request and enclosing test class covering `row`
    /// (`start_line <= row && row <= end_line`).
    #[must_use]
    pub fn enclosing_entry_points_at(&self, row: usize) -> EnclosingEntryPoints<'_> {
        let span_len = |ep: &EntryPoint| ep.end_line.saturating_sub(ep.start_line);
        let mut best_method: Option<&EntryPoint> = None;
        let mut best_class: Option<&EntryPoint> = None;
        for ep in &self.entry_points {
            if ep.start_line > row || row > ep.end_line {
                continue;
            }
            if ep.kind == EntryPointKind::JvmTestClass {
                if best_class.is_none_or(|cur| span_len(ep) < span_len(cur)) {
                    best_class = Some(ep);
                }
            } else if best_method.is_none_or(|cur| span_len(ep) < span_len(cur)) {
                best_method = Some(ep);
            }
        }
        EnclosingEntryPoints {
            method: best_method,
            class: best_class,
        }
    }

    /// Returns the grammar language of this syntax snapshot.
    #[must_use]
    pub const fn language(&self) -> kova_theme::SyntaxLanguage {
        self.tree.language()
    }

    /// Returns the smallest enclosing AST node range strictly larger than `start..end`.
    #[must_use]
    pub fn enclosing_node_range(&self, start: usize, end: usize) -> Option<Range<usize>> {
        self.tree.enclosing_node_range(start, end)
    }

    /// Returns the absolute start byte of `line` in [`Self::snapshot`], or `None` past its end.
    #[must_use]
    pub fn line_start_byte(&self, line: usize) -> Option<usize> {
        (line < self.snapshot.len_lines()).then(|| self.snapshot.rope().line_to_byte(line))
    }

    /// Runs a highlight query for the half-open `lines` range against the parsed snapshot,
    /// appending sorted, disjoint spans to `out`. Lines past the parsed snapshot are ignored.
    pub fn highlight_lines_into(&self, lines: Range<usize>, out: &mut Vec<HighlightSpan>) {
        if let Ok(spans) = self
            .highlighter
            .highlight_lines(&self.tree, &self.snapshot, lines)
        {
            out.extend(spans);
        }
    }
}

/// Request to run a gutter entry point (`main`, `@Test` method/class or `.http` request).
///
/// `@Test` methods and classes start a `JUnit` *Test Runner* run (`KOVA-040`); other entry
/// points are logged in the *Run* tool window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunEntryPointRequested {
    /// Project-relative path of the document, when it has one.
    pub file_path: Option<PathBuf>,
    /// Canonical runner target (`dev.kova.Order#test`, `dev.kova.App`, `http#0`).
    pub symbol_name: String,
    /// Kind of the entry point.
    pub kind: EntryPointKind,
    /// 0-based line of the `▶` icon.
    pub line: usize,
    /// Human-readable tooltip label from `kova_syntax`.
    pub label: String,
}

impl RunEntryPointRequested {
    /// Builds a request for `entry` in the document at `file_path`.
    #[must_use]
    pub fn from_entry_point(entry: &EntryPoint, file_path: Option<PathBuf>) -> Self {
        Self {
            file_path,
            symbol_name: entry.qualified_target(),
            kind: entry.kind,
            line: entry.line,
            label: entry.label.clone(),
        }
    }

    /// Formats the *Run* tool window log line for this request.
    #[must_use]
    pub fn log_line(&self) -> String {
        let what = match self.kind {
            EntryPointKind::JavaMain => format!("Java main: {}.main()", self.symbol_name),
            EntryPointKind::KotlinMain => format!("Kotlin main: {}.main()", self.symbol_name),
            EntryPointKind::JvmTestMethod => format!("JUnit test: {}", self.symbol_name),
            EntryPointKind::JvmTestClass => format!("JUnit test class: {}", self.symbol_name),
            EntryPointKind::HttpRequest => format!("HTTP request: {}", self.label),
        };
        let location = self
            .file_path
            .as_ref()
            .map(|path| format!(" ({}:{})", path.display(), self.line + 1))
            .unwrap_or_default();
        format!(
            "[kova_syntax] Ready to run {what}{location} — runtime execution attaches in \
             Phase 2/3 (kova_toolchain / kova_tools)"
        )
    }
}

#[cfg(test)]
mod tests {
    use kova_buffer::Buffer;
    use kova_syntax::{LanguageRegistry, SyntaxEngine, SyntaxEngineConfig};
    use kova_theme::SyntaxLanguage;

    use super::*;

    const JAVA: &str = "\
package dev.kova.orders;

public class OrderService {
    public static void main(String[] args) {
        System.out.println(\"hi\");
    }

    @org.junit.jupiter.api.Test
    void placesOrder() {
        int x = 1;
    }
}
";

    fn build(language: SyntaxLanguage, text: &str) -> EditorSyntax {
        let registry = LanguageRegistry::with_defaults().unwrap();
        let engine = SyntaxEngine::new(
            registry.clone(),
            SyntaxEngineConfig {
                worker_threads: 1,
                ..SyntaxEngineConfig::default()
            },
        )
        .unwrap();
        let snapshot = Buffer::from_text(text).snapshot();
        let tree = engine.parse_sync(language, &snapshot, None).unwrap();
        EditorSyntax::build(tree, snapshot, Highlighter::new(registry))
    }

    #[test]
    fn java_snapshot_exposes_folds_entry_points_and_spans() {
        let syntax = build(SyntaxLanguage::Java, JAVA);
        assert!(syntax.has_fold_at(2), "class body folds");
        assert!(syntax.has_fold_at(3), "main body folds");
        assert!(!syntax.has_fold_at(0));

        let main = syntax.entry_point_at(3).unwrap();
        assert_eq!(main.kind, EntryPointKind::JavaMain);
        assert!(syntax.entry_point_at(4).is_none());
        assert!(
            syntax
                .entry_points()
                .iter()
                .any(|e| e.kind == EntryPointKind::JvmTestMethod)
        );

        let mut spans = Vec::new();
        syntax.highlight_lines_into(0..4, &mut spans);
        assert_ne!(spans, Vec::new());
        assert!(spans.windows(2).all(|w| w[0].range.end <= w[1].range.start));
        // Lines past the parsed snapshot are ignored, not an error.
        let before = spans.len();
        syntax.highlight_lines_into(500..600, &mut spans);
        assert_eq!(spans.len(), before);
        assert_eq!(syntax.line_start_byte(0), Some(0));
        assert_eq!(syntax.line_start_byte(10_000), None);
        assert!(format!("{syntax:?}").contains("EditorSyntax"));
    }

    #[test]
    fn run_request_log_line_names_target_and_phase() {
        let syntax = build(SyntaxLanguage::Java, JAVA);
        let main = syntax.entry_point_at(3).unwrap();
        let request = RunEntryPointRequested::from_entry_point(
            main,
            Some(PathBuf::from("src/OrderService.java")),
        );
        assert_eq!(request.symbol_name, "dev.kova.orders.OrderService");
        let line = request.log_line();
        assert!(line.starts_with(
            "[kova_syntax] Ready to run Java main: dev.kova.orders.OrderService.main()"
        ));
        assert!(line.contains("src/OrderService.java:4"));
        assert!(line.contains("Phase 2/3 (kova_toolchain / kova_tools)"));

        let http = build(SyntaxLanguage::Http, "GET https://example.com/api\n");
        if let Some(entry) = http.entry_points().first() {
            let request = RunEntryPointRequested::from_entry_point(entry, None);
            assert_eq!(request.kind, EntryPointKind::HttpRequest);
            assert!(request.log_line().contains("HTTP request:"));
        }
    }

    #[test]
    fn enclosing_entry_points_at_resolves_method_class_nested_spock_and_outside() {
        let java = "\
package dev.kova;
import org.junit.jupiter.api.Nested;
import org.junit.jupiter.api.Test;

class OuterTest {
    int helperField = 1;

    @Test
    void outerMethod() {
        int a = 1;
    }

    @Nested
    class InnerTest {
        @Test
        void innerMethod() {
            int b = 2;
        }
    }
}
";
        let syntax = build(SyntaxLanguage::Java, java);
        // Outside class
        assert_eq!(
            syntax.enclosing_entry_points_at(0),
            EnclosingEntryPoints::default()
        );
        // Inside class between methods (field line)
        let field_ctx = syntax.enclosing_entry_points_at(5);
        assert!(field_ctx.method.is_none());
        assert_eq!(
            field_ctx.class.map(EntryPoint::qualified_target).as_deref(),
            Some("dev.kova.OuterTest")
        );
        // On @Test annotation line
        let ann_ctx = syntax.enclosing_entry_points_at(7);
        assert_eq!(
            ann_ctx.method.map(EntryPoint::qualified_target).as_deref(),
            Some("dev.kova.OuterTest#outerMethod")
        );
        assert_eq!(
            ann_ctx.class.map(EntryPoint::qualified_target).as_deref(),
            Some("dev.kova.OuterTest")
        );
        // Inside method body
        let body_ctx = syntax.enclosing_entry_points_at(9);
        assert_eq!(
            body_ctx.method.map(EntryPoint::qualified_target).as_deref(),
            Some("dev.kova.OuterTest#outerMethod")
        );
        // Inside @Nested method body picks narrowest class and method
        let nested_ctx = syntax.enclosing_entry_points_at(16);
        assert_eq!(
            nested_ctx
                .method
                .map(EntryPoint::qualified_target)
                .as_deref(),
            Some("dev.kova.OuterTest$InnerTest#innerMethod")
        );
        assert_eq!(
            nested_ctx
                .class
                .map(EntryPoint::qualified_target)
                .as_deref(),
            Some("dev.kova.OuterTest$InnerTest")
        );

        let spock = "\
package com.kova
import spock.lang.Specification
class OrderSpec extends Specification {
    def \"places order\"() {
        expect:
        1 == 1
    }
}
";
        let spock_syntax = build(SyntaxLanguage::Groovy, spock);
        let spock_ctx = spock_syntax.enclosing_entry_points_at(5);
        assert_eq!(
            spock_ctx
                .method
                .map(EntryPoint::qualified_target)
                .as_deref(),
            Some("com.kova.OrderSpec#places order")
        );
        assert_eq!(
            spock_ctx.class.map(EntryPoint::qualified_target).as_deref(),
            Some("com.kova.OrderSpec")
        );
    }
}
