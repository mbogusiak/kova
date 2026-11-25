//! Integration tests for `kova_syntax::folding` and `kova_syntax::structure` (KOVA-023).

#![allow(
    clippy::expect_used,
    reason = "Integration test helpers panic on setup failure"
)]

use kova_buffer::{Buffer, BufferId, BufferSnapshot};
use kova_syntax::{
    FoldKind, FoldRange, LanguageRegistry, MAX_STRUCTURE_DEPTH, StructureNode, StructureOutline,
    StructureSymbolKind, SymbolVisibility, SyntaxEngine, SyntaxEngineConfig, SyntaxTree,
    extract_folds, extract_structure,
};
use kova_theme::SyntaxLanguage;

fn parse(language: SyntaxLanguage, source: &str) -> (BufferSnapshot, SyntaxTree) {
    let registry = LanguageRegistry::with_defaults().expect("default registry");
    let engine = SyntaxEngine::new(registry, SyntaxEngineConfig::default()).expect("syntax engine");
    let buffer = Buffer::with_id(BufferId::new(1), source);
    let snapshot = buffer.snapshot();
    let tree = engine
        .parse_sync(language, &snapshot, None)
        .expect("parse_sync succeeds");
    (snapshot, tree)
}

fn assert_fold_invariants(folds: &[FoldRange], snapshot: &BufferSnapshot) {
    let len = snapshot.len_bytes();
    for (idx, fold) in folds.iter().enumerate() {
        assert!(
            fold.end_line > fold.start_line,
            "fold {idx} must be multi-line: {fold:?}"
        );
        assert!(
            fold.collapsed_byte_range.start <= fold.collapsed_byte_range.end
                && fold.collapsed_byte_range.end <= len,
            "fold {idx} byte range out of bounds (len={len}): {fold:?}"
        );
        if idx > 0 {
            let prev = &folds[idx - 1];
            assert!(
                (prev.start_line, std::cmp::Reverse(prev.end_line))
                    <= (fold.start_line, std::cmp::Reverse(fold.end_line)),
                "folds not sorted: prev={prev:?}, curr={fold:?}"
            );
            assert_ne!(
                (prev.start_line, prev.end_line),
                (fold.start_line, fold.end_line),
                "duplicate (start_line, end_line): {prev:?} vs {fold:?}"
            );
        }
    }
}

fn assert_structure_invariants(outline: &StructureOutline, snapshot: &BufferSnapshot) {
    let len = snapshot.len_bytes();
    let mut stack: Vec<&StructureNode> = outline.roots.iter().collect();
    while let Some(node) = stack.pop() {
        assert!(
            node.full_range.start <= node.full_range.end && node.full_range.end <= len,
            "full_range out of bounds for {}: {:?}",
            node.name,
            node.full_range
        );
        assert!(
            node.selection_range.start <= node.selection_range.end
                && node.selection_range.end <= len,
            "selection_range out of bounds for {}: {:?}",
            node.name,
            node.selection_range
        );
        stack.extend(node.children.iter());
    }
}

#[test]
fn folding_java_imports_javadoc_regions_and_blocks() {
    let src = "\
package com.example;

import java.util.List;
import java.util.Map;
import java.util.Optional;

/**
 * Order service Javadoc.
 */
public class OrderService {
    // region Fields
    private int count;
    // endregion

    public OrderService(int count) {
        this.count = count;
    }

    public Optional<String> find(Long id, String name) {
        if (id > 0) {
            return Optional.of(name);
        }
        return Optional.empty();
    }
}
";
    let (snapshot, tree) = parse(SyntaxLanguage::Java, src);
    let folds = extract_folds(&tree, &snapshot);
    assert_fold_invariants(&folds, &snapshot);

    assert!(
        folds
            .iter()
            .any(|f| f.kind == FoldKind::Imports && f.default_collapsed),
        "expected default-collapsed Imports fold: {folds:?}"
    );
    assert!(
        folds
            .iter()
            .any(|f| f.kind == FoldKind::Comment && f.placeholder == "/** ... */"),
        "expected Javadoc fold: {folds:?}"
    );
    assert!(
        folds.iter().any(|f| f.kind == FoldKind::Region),
        "expected // region fold: {folds:?}"
    );
    assert!(
        folds.iter().any(|f| f.kind == FoldKind::Type),
        "expected Type fold: {folds:?}"
    );
    assert!(
        folds.iter().any(|f| f.kind == FoldKind::Function),
        "expected Function fold: {folds:?}"
    );
    assert!(
        folds.iter().any(|f| f.kind == FoldKind::Block),
        "expected Block fold: {folds:?}"
    );
}

#[test]
fn folding_kotlin_http_toml_json() {
    let kt = "\
import kotlin.collections.List
import kotlin.collections.Map

/**
 * KDoc comment.
 */
class Greeter {
    fun greet(name: String): String {
        if (name.isNotEmpty()) {
            return \"Hello, $name\"
        }
        return \"Hello\"
    }
}
";
    let (kt_snap, kt_tree) = parse(SyntaxLanguage::Kotlin, kt);
    let kt_folds = extract_folds(&kt_tree, &kt_snap);
    assert_fold_invariants(&kt_folds, &kt_snap);
    assert!(kt_folds.iter().any(|f| f.kind == FoldKind::Imports));
    assert!(kt_folds.iter().any(|f| f.kind == FoldKind::Type));
    assert!(kt_folds.iter().any(|f| f.kind == FoldKind::Function));

    let http = "\
### Get user
GET https://example.com/users/1
Accept: application/json

### Create user
POST https://example.com/users
Content-Type: application/json

{
  \"name\": \"Ada\"
}
";
    let (http_snap, http_tree) = parse(SyntaxLanguage::Http, http);
    let http_folds = extract_folds(&http_tree, &http_snap);
    assert_fold_invariants(&http_folds, &http_snap);
    assert!(http_folds.iter().any(|f| f.kind == FoldKind::Request));

    let toml = "\
[package]
name = \"kova\"
version = \"0.1.0\"

[dependencies]
serde = {
    version = \"1.0\",
    features = [\"derive\"]
}
";
    let (toml_snap, toml_tree) = parse(SyntaxLanguage::Toml, toml);
    let toml_folds = extract_folds(&toml_tree, &toml_snap);
    assert_fold_invariants(&toml_folds, &toml_snap);
    assert_ne!(toml_folds.len(), 0);

    let json = "{\n  \"a\": {\n    \"b\": [\n      1,\n      2\n    ]\n  }\n}\n";
    let (json_snap, json_tree) = parse(SyntaxLanguage::Json, json);
    let json_folds = extract_folds(&json_tree, &json_snap);
    assert_fold_invariants(&json_folds, &json_snap);
    assert!(json_folds.len() >= 2);
}

#[test]
fn structure_java_hierarchy_fields_and_path_at_byte() {
    let src = "\
package com.example;

public class OrderService {
    private int a, b;

    public OrderService() {}

    protected static String format(Long id, String name) {
        return name;
    }

    public enum Status {
        OPEN,
        CLOSED
    }
}
";
    let (snapshot, tree) = parse(SyntaxLanguage::Java, src);
    let outline = extract_structure(&tree, &snapshot);
    assert_structure_invariants(&outline, &snapshot);
    assert!(!outline.is_empty());

    assert_eq!(outline.roots.len(), 1);
    let cls = &outline.roots[0];
    assert_eq!(cls.name, "OrderService");
    assert_eq!(cls.kind, StructureSymbolKind::Class);
    assert_eq!(cls.visibility, SymbolVisibility::Public);

    // Multi-declarator fields `a` and `b`
    let fields: Vec<&StructureNode> = cls
        .children
        .iter()
        .filter(|n| n.kind == StructureSymbolKind::Field)
        .collect();
    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0].name, "a");
    assert_eq!(fields[1].name, "b");
    assert_eq!(fields[0].detail.as_deref(), Some("int"));

    let method = cls
        .children
        .iter()
        .find(|n| n.name == "format")
        .expect("format method");
    assert_eq!(method.kind, StructureSymbolKind::Method);
    assert_eq!(method.visibility, SymbolVisibility::Protected);
    assert!(method.is_static);
    assert_eq!(
        method.detail.as_deref(),
        Some("(Long id, String name): String")
    );

    let status_enum = cls
        .children
        .iter()
        .find(|n| n.name == "Status")
        .expect("Status enum");
    assert_eq!(status_enum.kind, StructureSymbolKind::Enum);
    assert_eq!(status_enum.children.len(), 2);
    assert_eq!(
        status_enum.children[0].kind,
        StructureSymbolKind::EnumConstant
    );

    // Breadcrumb path_at_byte inside `format`
    let byte_in_format = method.selection_range.start;
    let path = outline.path_at_byte(byte_in_format);
    assert_eq!(path.len(), 2);
    assert_eq!(path[0].name, "OrderService");
    assert_eq!(path[1].name, "format");
}

#[test]
fn structure_kotlin_http_toml_json() {
    let kt = "\
class UserRepo {
    private val cacheSize = 10
    internal fun findById(id: Long) {}
    companion object {
        fun create() = UserRepo()
    }
}
";
    let (kt_snap, kt_tree) = parse(SyntaxLanguage::Kotlin, kt);
    let kt_outline = extract_structure(&kt_tree, &kt_snap);
    assert_structure_invariants(&kt_outline, &kt_snap);
    assert_eq!(kt_outline.roots.len(), 1);
    let repo = &kt_outline.roots[0];
    assert_eq!(repo.name, "UserRepo");
    assert!(
        repo.children
            .iter()
            .any(|c| c.name == "cacheSize" && c.visibility == SymbolVisibility::Private)
    );
    assert!(
        repo.children
            .iter()
            .any(|c| c.name == "findById" && c.visibility == SymbolVisibility::Internal)
    );
    assert!(
        repo.children
            .iter()
            .any(|c| c.kind == StructureSymbolKind::Object && c.name == "companion object")
    );

    let http = "GET https://example.com/health\nAccept: */*\n";
    let (http_snap, http_tree) = parse(SyntaxLanguage::Http, http);
    let http_outline = extract_structure(&http_tree, &http_snap);
    assert_structure_invariants(&http_outline, &http_snap);
    assert_eq!(http_outline.roots.len(), 1);
    assert_eq!(http_outline.roots[0].kind, StructureSymbolKind::Section);

    let toml = "[package]\nname = \"kova\"\n";
    let (toml_snap, toml_tree) = parse(SyntaxLanguage::Toml, toml);
    let toml_outline = extract_structure(&toml_tree, &toml_snap);
    assert_structure_invariants(&toml_outline, &toml_snap);
    assert_eq!(toml_outline.roots.len(), 1);
    assert_eq!(toml_outline.roots[0].name, "package");
    assert_eq!(toml_outline.roots[0].children[0].name, "name");

    let json = "{\"server\": {\"port\": 8080}}";
    let (json_snap, json_tree) = parse(SyntaxLanguage::Json, json);
    let json_outline = extract_structure(&json_tree, &json_snap);
    assert_structure_invariants(&json_outline, &json_snap);
    assert_eq!(json_outline.symbol_count(), 2);
}

#[test]
fn resilience_syntax_errors_crlf_utf8_and_deep_json() {
    // Syntax error + CRLF + multi-byte UTF-8
    let broken_java = "public class Zażółć {\r\n    public void broken( {\r\n    }\r\n    public void valid() {\r\n        int x = 1;\r\n    }\r\n}\r\n";
    let (snap, tree) = parse(SyntaxLanguage::Java, broken_java);
    let folds = extract_folds(&tree, &snap);
    assert_fold_invariants(&folds, &snap);
    let outline = extract_structure(&tree, &snap);
    assert_structure_invariants(&outline, &snap);
    assert!(!outline.is_empty());

    // 500-level deep JSON must not stack overflow
    let depth = 500usize;
    let mut deep_json = String::new();
    for _ in 0..depth {
        deep_json.push_str("{\"a\":");
    }
    deep_json.push('1');
    for _ in 0..depth {
        deep_json.push('}');
    }
    let (deep_snap, deep_tree) = parse(SyntaxLanguage::Json, &deep_json);
    let deep_outline = extract_structure(&deep_tree, &deep_snap);
    assert_structure_invariants(&deep_outline, &deep_snap);
    assert!(deep_outline.symbol_count() <= MAX_STRUCTURE_DEPTH);
}
