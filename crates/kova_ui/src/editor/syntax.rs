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
/// Phase 1 only logs it in the *Run* tool window; process execution attaches in Phase 2/3
/// (`kova_toolchain` / `kova_tools`).
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
}
