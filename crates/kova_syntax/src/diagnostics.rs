//! Headless extraction of Tree-sitter parse errors (`ERROR` / `MISSING` nodes) as
//! [`SyntaxDiagnostic`]s, so UI crates never touch `tree_sitter` node types directly.

use crate::tree::SyntaxTree;

/// Default upper bound on diagnostics collected for one (possibly badly broken) document.
pub const DEFAULT_MAX_SYNTAX_DIAGNOSTICS: usize = 100;

/// A single syntax error reported by the parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxDiagnostic {
    /// 0-based line of the error start.
    pub line: usize,
    /// 0-based UTF-8 byte column of the error start within its line.
    pub col: usize,
    /// Human-readable message, e.g. "Syntax error: missing `;`" or "Syntax error: unexpected input".
    pub message: String,
}

/// Collects at most `max` syntax diagnostics from `tree` in document order.
///
/// Returns an empty `Vec` in $O(1)$ when the tree has no errors. Only subtrees flagged by
/// `has_error` are descended, so cost is proportional to the broken regions, not the file.
#[must_use]
pub fn collect_syntax_diagnostics(tree: &SyntaxTree, max: usize) -> Vec<SyntaxDiagnostic> {
    let mut diagnostics = Vec::new();
    if !tree.has_error() || max == 0 {
        return diagnostics;
    }
    if let Some(doc) = tree.scanned() {
        diagnostics.extend(doc.diagnostics().iter().take(max).cloned());
        return diagnostics;
    }
    let Some(root) = tree.root_node() else {
        return diagnostics;
    };
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if diagnostics.len() >= max {
            break;
        }
        if node.is_error() || node.is_missing() {
            let pos = node.start_position();
            let message = if node.is_missing() {
                format!("Syntax error: missing `{}`", node.kind())
            } else {
                "Syntax error: unexpected input".to_owned()
            };
            diagnostics.push(SyntaxDiagnostic {
                line: pos.row,
                col: pos.column,
                message,
            });
            continue;
        }
        if node.has_error() {
            for idx in (0..node.child_count()).rev() {
                if let Some(child) = u32::try_from(idx).ok().and_then(|i| node.child(i)) {
                    stack.push(child);
                }
            }
        }
    }
    diagnostics
}

#[cfg(test)]
mod tests {
    use kova_buffer::Buffer;
    use kova_theme::SyntaxLanguage;

    use super::*;
    use crate::{LanguageRegistry, SyntaxEngine, SyntaxEngineConfig};

    fn parse(text: &str) -> SyntaxTree {
        let engine = SyntaxEngine::new(
            LanguageRegistry::with_defaults().unwrap(),
            SyntaxEngineConfig {
                worker_threads: 1,
                ..SyntaxEngineConfig::default()
            },
        )
        .unwrap();
        let snapshot = Buffer::from_text(text).snapshot();
        engine
            .parse_sync(SyntaxLanguage::Java, &snapshot, None)
            .unwrap()
    }

    #[test]
    fn valid_source_has_no_diagnostics() {
        let tree = parse("class A { void m() {} }\n");
        assert_eq!(collect_syntax_diagnostics(&tree, 10), Vec::new());
    }

    #[test]
    fn broken_source_reports_bounded_diagnostics_in_order() {
        let tree = parse("class A {\n  void m() { int x = ; }\n");
        let diagnostics = collect_syntax_diagnostics(&tree, 10);
        assert_ne!(diagnostics, Vec::new());
        assert!(
            diagnostics
                .iter()
                .all(|d| d.message.starts_with("Syntax error"))
        );
        assert!(diagnostics.iter().any(|d| d.line >= 1));
        assert_eq!(collect_syntax_diagnostics(&tree, 0), Vec::new());

        let mut many = String::from("class A {\n");
        for _ in 0..50 {
            many.push_str("  void m() { int x = ; }\n");
        }
        let tree = parse(&many);
        assert!(collect_syntax_diagnostics(&tree, 5).len() <= 5);
    }
}
