//! AST-based entry-point detection (`main`, `@Test*`, and `.http` requests) for gutter `▶` icons.
//!
//! Operates strictly headlessly on a [`SyntaxTree`] and [`BufferSnapshot`] without spawning
//! processes, opening network sockets, or depending on any UI crate.

use std::collections::HashMap;
use std::ops::Range;

use kova_buffer::BufferSnapshot;
use kova_theme::SyntaxLanguage;
use tree_sitter::{Node, QueryCursor, StreamingIterator};

use crate::input::{RopeTextProvider, read_node_text};
use crate::registry::LanguageRegistry;
use crate::tree::SyntaxTree;

/// Canonical kind of runnable entry point detected in an editor buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntryPointKind {
    /// Java entry-point method (`public static void main` or JEP 445 `void main`).
    JavaMain,
    /// Kotlin entry-point function (`fun main` top-level or `@JvmStatic` inside `object` / `companion object`).
    KotlinMain,
    /// JVM test class containing at least one test method or `@Nested` test class.
    JvmTestClass,
    /// Single JVM test method (`@Test`, `@ParameterizedTest`, `@RepeatedTest`, `@TestFactory`, `@TestTemplate`).
    JvmTestMethod,
    /// REST request in an `.http` file.
    HttpRequest,
}

/// Visual role of the gutter `▶` icon (`kova_theme::GutterTokens::run_icon`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GutterIconRole {
    /// Single runnable entry point (`main`, single `@Test`, or single `.http` request).
    Run,
    /// Group run target (`JvmTestClass` — executes all tests in the class).
    RunAll,
}

/// Metadata describing a runnable entry point anchored to a source line for gutter rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryPoint {
    /// Stable file-local target identifier, e.g. `com.example.OrderTest#shouldCreate` or `http#0`.
    pub id: String,
    /// Semantic kind of this entry point.
    pub kind: EntryPointKind,
    /// Gutter play icon role (`Run` vs `RunAll`).
    pub icon_role: GutterIconRole,
    /// Human-readable UI tooltip label, e.g. `"Run 'OrderTest.shouldCreate()'"` or `"Send 'POST /api/orders'"`.
    pub label: String,
    /// 0-based line number where the gutter renders the `▶` icon (signature/name line).
    pub line: usize,
    /// Inclusive 0-based start line of the full declaration or HTTP request.
    pub start_line: usize,
    /// Inclusive 0-based end line of the full declaration or HTTP request.
    pub end_line: usize,
    /// Clamped UTF-8 byte range covering the full declaration or HTTP request.
    pub byte_range: Range<usize>,
    /// JVM package name (`None` for default package or `.http` files).
    pub package_name: Option<String>,
    /// Enclosing JVM class/object name using `$` for nested types (`None` for top-level Kotlin or `.http`).
    pub class_name: Option<String>,
    /// JVM method/function name (`None` for `JvmTestClass` and `.http`).
    pub method_name: Option<String>,
    /// 0-based request index within an `.http` file (`None` for JVM).
    pub request_index: Option<usize>,
    /// Normalized uppercase HTTP method (`None` for JVM).
    pub http_method: Option<String>,
    /// Target URL/URI extracted from an `.http` request line (`None` for JVM).
    pub http_url: Option<String>,
}

impl EntryPoint {
    /// Returns the canonical qualified target identifier for runners (e.g.
    /// `com.example.OrderServiceTest#shouldPlaceOrder` for `JUnit` or `com.example.Application` for `JavaMain`).
    #[must_use]
    pub fn qualified_target(&self) -> String {
        match self.kind {
            EntryPointKind::JavaMain | EntryPointKind::KotlinMain => {
                if let Some(fqcn) =
                    build_fqcn(self.package_name.as_deref(), self.class_name.as_deref())
                {
                    fqcn
                } else {
                    self.id.clone()
                }
            }
            EntryPointKind::JvmTestClass => {
                build_fqcn(self.package_name.as_deref(), self.class_name.as_deref())
                    .unwrap_or_else(|| self.id.clone())
            }
            EntryPointKind::JvmTestMethod => {
                match (
                    build_fqcn(self.package_name.as_deref(), self.class_name.as_deref()),
                    self.method_name.as_deref(),
                ) {
                    (Some(fqcn), Some(method)) => format!("{fqcn}#{method}"),
                    _ => self.id.clone(),
                }
            }
            EntryPointKind::HttpRequest => self.id.clone(),
        }
    }
}

/// Detects all runnable entry points (`main`, `@Test*`, test classes, and `.http` requests)
/// in `tree` against `snapshot`.
///
/// Returns an empty `Vec` in $O(1)$ time for [`SyntaxLanguage::Toml`], [`SyntaxLanguage::Json`],
/// [`SyntaxLanguage::Yaml`], [`SyntaxLanguage::Xml`], and [`SyntaxLanguage::Groovy`].
/// Output is sorted ascending by `(line, byte_range.start)` and preserves `start_line <= line <= end_line`
/// and `byte_range.end <= snapshot.len_bytes()`.
#[must_use]
pub fn detect_entry_points(tree: &SyntaxTree, snapshot: &BufferSnapshot) -> Vec<EntryPoint> {
    let mut entry_points = match tree.language() {
        SyntaxLanguage::Java => detect_java_entry_points(tree, snapshot),
        SyntaxLanguage::Kotlin => detect_kotlin_entry_points(tree, snapshot),
        SyntaxLanguage::Http => detect_http_entry_points(tree, snapshot),
        SyntaxLanguage::Toml
        | SyntaxLanguage::Json
        | SyntaxLanguage::Yaml
        | SyntaxLanguage::Xml
        | SyntaxLanguage::Groovy => return Vec::new(),
    };

    entry_points.sort_by_key(|ep| (ep.line, ep.byte_range.start));
    entry_points
}

fn builtin_entry_points_query(language: SyntaxLanguage) -> Option<&'static tree_sitter::Query> {
    LanguageRegistry::builtin()
        .ok()?
        .get(language)?
        .entry_points_query()
}

fn is_test_annotation_name(raw_name: &str) -> bool {
    let trimmed = raw_name.trim().trim_start_matches('@');
    let simple = trimmed
        .rsplit_once('.')
        .map_or(trimmed, |(_, tail)| tail)
        .trim();
    matches!(
        simple,
        "Test" | "ParameterizedTest" | "RepeatedTest" | "TestFactory" | "TestTemplate"
    )
}

fn is_nested_annotation_name(raw_name: &str) -> bool {
    let trimmed = raw_name.trim().trim_start_matches('@');
    let simple = trimmed
        .rsplit_once('.')
        .map_or(trimmed, |(_, tail)| tail)
        .trim();
    simple == "Nested"
}

fn is_jvm_static_annotation_name(raw_name: &str) -> bool {
    let trimmed = raw_name.trim().trim_start_matches('@');
    trimmed == "JvmStatic" || trimmed == "kotlin.jvm.JvmStatic"
}

fn build_fqcn(package_name: Option<&str>, class_name: Option<&str>) -> Option<String> {
    let cls = class_name.filter(|s| !s.is_empty())?;
    match package_name.filter(|s| !s.is_empty()) {
        Some(pkg) => Some(format!("{pkg}.{cls}")),
        None => Some(cls.to_owned()),
    }
}

fn clamp_byte_range(range: Range<usize>, len_bytes: usize) -> Range<usize> {
    let start = range.start.min(len_bytes);
    let end = range.end.min(len_bytes).max(start);
    start..end
}

fn effective_end_line(node: Node<'_>, line: usize) -> usize {
    let end = node.end_position();
    let raw = if end.column == 0 && end.row > node.start_position().row {
        end.row - 1
    } else {
        end.row
    };
    raw.max(line)
}

#[derive(Debug, Clone)]
struct TestClassRecord {
    line: usize,
    start_line: usize,
    end_line: usize,
    byte_range: Range<usize>,
    class_name: String,
    has_tests: bool,
    is_nested: bool,
    parent_class_id: Option<usize>,
}

#[derive(Debug, Default)]
struct TestClassTracker {
    classes: HashMap<usize, TestClassRecord>,
    order: Vec<usize>,
}

impl TestClassTracker {
    fn ensure_chain(
        &mut self,
        chain: &[ClassChainItem],
        snapshot: &BufferSnapshot,
        mark_innermost_has_tests: bool,
    ) {
        let len_bytes = snapshot.len_bytes();
        let mut parent_id: Option<usize> = None;
        let mut qualified_parts: Vec<String> = Vec::with_capacity(chain.len());

        for (idx, item) in chain.iter().enumerate() {
            qualified_parts.push(item.name.clone());
            let is_last = idx + 1 == chain.len();
            let entry = self.classes.entry(item.node_id).or_insert_with(|| {
                self.order.push(item.node_id);
                TestClassRecord {
                    line: item.line,
                    start_line: item.start_line,
                    end_line: item.end_line,
                    byte_range: clamp_byte_range(item.byte_range.clone(), len_bytes),
                    class_name: qualified_parts.join("$"),
                    has_tests: false,
                    is_nested: item.is_nested,
                    parent_class_id: parent_id,
                }
            });
            if is_last && mark_innermost_has_tests {
                entry.has_tests = true;
            }
            parent_id = Some(item.node_id);
        }
    }

    fn emit(mut self, package_name: Option<&str>, out: &mut Vec<EntryPoint>) {
        // Propagate test presence upward through `@Nested` test classes so outer test suites get RunAll.
        loop {
            let mut changed = false;
            let mut parents_to_mark = Vec::new();
            for record in self.classes.values() {
                if record.has_tests
                    && record.is_nested
                    && let Some(parent_id) = record.parent_class_id
                {
                    parents_to_mark.push(parent_id);
                }
            }
            for parent_id in parents_to_mark {
                if let Some(parent) = self.classes.get_mut(&parent_id)
                    && !parent.has_tests
                {
                    parent.has_tests = true;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        for id in self.order {
            let Some(record) = self.classes.get(&id) else {
                continue;
            };
            if !record.has_tests {
                continue;
            }
            let pkg_opt = package_name.map(str::to_owned);
            let fqcn = build_fqcn(package_name, Some(&record.class_name))
                .unwrap_or_else(|| record.class_name.clone());
            out.push(EntryPoint {
                id: fqcn,
                kind: EntryPointKind::JvmTestClass,
                icon_role: GutterIconRole::RunAll,
                label: format!("Run '{}'", record.class_name),
                line: record.line,
                start_line: record.start_line,
                end_line: record.end_line,
                byte_range: record.byte_range.clone(),
                package_name: pkg_opt,
                class_name: Some(record.class_name.clone()),
                method_name: None,
                request_index: None,
                http_method: None,
                http_url: None,
            });
        }
    }
}

#[derive(Debug, Clone)]
struct ClassChainItem {
    node_id: usize,
    name: String,
    line: usize,
    start_line: usize,
    end_line: usize,
    byte_range: Range<usize>,
    is_nested: bool,
    emits_test_class: bool,
}

#[allow(
    clippy::too_many_arguments,
    reason = "Internal helper grouping entry point metadata"
)]
fn push_jvm_main_entry_point(
    out: &mut Vec<EntryPoint>,
    kind: EntryPointKind,
    package_name: Option<&str>,
    class_name: Option<&str>,
    method_name: &str,
    line: usize,
    decl_node: Node<'_>,
    snapshot: &BufferSnapshot,
) {
    let fqcn = build_fqcn(package_name, class_name);
    let id = match (&fqcn, package_name) {
        (Some(fqcn), _) => format!("{fqcn}#{method_name}"),
        (None, Some(pkg)) => format!("{pkg}#{method_name}"),
        (None, None) => method_name.to_owned(),
    };
    let label = match class_name.filter(|s| !s.is_empty()) {
        Some(cls) => format!("Run '{cls}.{method_name}()'"),
        None => format!("Run '{method_name}()'"),
    };

    out.push(EntryPoint {
        id,
        kind,
        icon_role: GutterIconRole::Run,
        label,
        line,
        start_line: decl_node.start_position().row,
        end_line: effective_end_line(decl_node, line),
        byte_range: clamp_byte_range(decl_node.byte_range(), snapshot.len_bytes()),
        package_name: package_name.map(str::to_owned),
        class_name: class_name.map(str::to_owned),
        method_name: Some(method_name.to_owned()),
        request_index: None,
        http_method: None,
        http_url: None,
    });
}

fn push_jvm_test_method_entry_point(
    out: &mut Vec<EntryPoint>,
    package_name: Option<&str>,
    class_name: Option<&str>,
    method_name: &str,
    line: usize,
    decl_node: Node<'_>,
    snapshot: &BufferSnapshot,
) {
    let fqcn = build_fqcn(package_name, class_name);
    let id = match (&fqcn, package_name) {
        (Some(fqcn), _) => format!("{fqcn}#{method_name}"),
        (None, Some(pkg)) => format!("{pkg}#{method_name}"),
        (None, None) => method_name.to_owned(),
    };
    let label = match class_name.filter(|s| !s.is_empty()) {
        Some(cls) => format!("Run '{cls}.{method_name}()'"),
        None => format!("Run '{method_name}()'"),
    };

    out.push(EntryPoint {
        id,
        kind: EntryPointKind::JvmTestMethod,
        icon_role: GutterIconRole::Run,
        label,
        line,
        start_line: decl_node.start_position().row,
        end_line: effective_end_line(decl_node, line),
        byte_range: clamp_byte_range(decl_node.byte_range(), snapshot.len_bytes()),
        package_name: package_name.map(str::to_owned),
        class_name: class_name.map(str::to_owned),
        method_name: Some(method_name.to_owned()),
        request_index: None,
        http_method: None,
        http_url: None,
    });
}

// ---------------------------------------------------------------------------
// Java detection
// ---------------------------------------------------------------------------

fn detect_java_entry_points(tree: &SyntaxTree, snapshot: &BufferSnapshot) -> Vec<EntryPoint> {
    let Some(query) = builtin_entry_points_query(SyntaxLanguage::Java) else {
        return Vec::new();
    };

    let idx_package_name = query.capture_index_for_name("package.name");
    let idx_method_decl = query.capture_index_for_name("method.decl");
    let idx_method_name = query.capture_index_for_name("method.name");
    let idx_method_params = query.capture_index_for_name("method.params");

    let mut package_name: Option<String> = None;
    let mut matched_methods: Vec<(Node<'_>, Node<'_>, Node<'_>)> = Vec::new();

    let Some(root) = tree.root_node() else {
        return Vec::new();
    };
    let mut cursor = QueryCursor::new();
    let provider = RopeTextProvider::from_snapshot(snapshot);
    let mut matches = cursor.matches(query, root, provider);

    while let Some(mat) = matches.next() {
        let mut decl_node = None;
        let mut name_node = None;
        let mut params_node = None;

        for cap in mat.captures {
            if Some(cap.index) == idx_package_name {
                if package_name.is_none() {
                    let pkg = read_node_text(cap.node, snapshot);
                    let trimmed = pkg.trim();
                    if !trimmed.is_empty() {
                        package_name = Some(trimmed.to_owned());
                    }
                }
            } else if Some(cap.index) == idx_method_decl {
                decl_node = Some(cap.node);
            } else if Some(cap.index) == idx_method_name {
                name_node = Some(cap.node);
            } else if Some(cap.index) == idx_method_params {
                params_node = Some(cap.node);
            }
        }

        if let (Some(decl), Some(name), Some(params)) = (decl_node, name_node, params_node) {
            matched_methods.push((decl, name, params));
        }
    }

    let mut out = Vec::new();
    let mut test_classes = TestClassTracker::default();
    let pkg_ref = package_name.as_deref();

    for (method_node, name_node, params_node) in matched_methods {
        if java_has_abstract_modifier(method_node) {
            continue;
        }

        let method_name = read_node_text(name_node, snapshot);
        let line = name_node.start_position().row;

        let is_main =
            method_name == "main" && is_java_main_signature(method_node, params_node, snapshot);
        let is_test = java_node_has_annotation(method_node, snapshot, is_test_annotation_name);

        if !is_main && !is_test {
            continue;
        }

        let chain = collect_java_enclosing_types(method_node, snapshot);
        let class_name = format_class_chain(&chain);

        if is_main {
            push_jvm_main_entry_point(
                &mut out,
                EntryPointKind::JavaMain,
                pkg_ref,
                class_name.as_deref(),
                &method_name,
                line,
                method_node,
                snapshot,
            );
        }

        if is_test {
            push_jvm_test_method_entry_point(
                &mut out,
                pkg_ref,
                class_name.as_deref(),
                &method_name,
                line,
                method_node,
                snapshot,
            );
            let test_chain: Vec<ClassChainItem> =
                chain.into_iter().filter(|c| c.emits_test_class).collect();
            if !test_chain.is_empty() {
                test_classes.ensure_chain(&test_chain, snapshot, true);
            }
        }
    }

    test_classes.emit(pkg_ref, &mut out);
    out
}

fn java_has_abstract_modifier(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "modifiers" {
            let mut mod_cursor = child.walk();
            for mod_child in child.children(&mut mod_cursor) {
                if mod_child.kind() == "abstract" {
                    return true;
                }
            }
        }
    }
    false
}

fn java_node_has_annotation(
    node: Node<'_>,
    snapshot: &BufferSnapshot,
    predicate: fn(&str) -> bool,
) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "marker_annotation" | "annotation" => {
                if let Some(ann_name) = child.child_by_field_name("name") {
                    let text = read_node_text(ann_name, snapshot);
                    if predicate(&text) {
                        return true;
                    }
                }
            }
            "modifiers" => {
                let mut mod_cursor = child.walk();
                for mod_child in child.children(&mut mod_cursor) {
                    if matches!(mod_child.kind(), "marker_annotation" | "annotation")
                        && let Some(ann_name) = mod_child.child_by_field_name("name")
                    {
                        let text = read_node_text(ann_name, snapshot);
                        if predicate(&text) {
                            return true;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    false
}

fn is_java_main_signature(
    method_node: Node<'_>,
    params_node: Node<'_>,
    snapshot: &BufferSnapshot,
) -> bool {
    // Reject trailing array dimensions like `void main()[]`.
    if method_node.child_by_field_name("dimensions").is_some() {
        return false;
    }

    let Some(ret_type) = method_node.child_by_field_name("type") else {
        return false;
    };
    if ret_type.kind() != "void_type" {
        return false;
    }

    let mut param_nodes = Vec::new();
    let mut cursor = params_node.walk();
    for child in params_node.named_children(&mut cursor) {
        if matches!(
            child.kind(),
            "formal_parameter" | "spread_parameter" | "receiver_parameter"
        ) {
            param_nodes.push(child);
        }
    }

    match param_nodes.as_slice() {
        // JEP 445 / Java 21+ parameterless `void main()`.
        [] => true,
        [param] => is_java_string_array_or_varargs(*param, snapshot),
        _ => false,
    }
}

fn is_java_string_array_or_varargs(param: Node<'_>, snapshot: &BufferSnapshot) -> bool {
    match param.kind() {
        "formal_parameter" => {
            let Some(type_node) = param.child_by_field_name("type") else {
                return false;
            };
            if let Some(param_dims) = param.child_by_field_name("dimensions") {
                // `String args[]`
                return is_java_string_type_name(&read_node_text(type_node, snapshot))
                    && count_array_dimensions(&read_node_text(param_dims, snapshot)) == 1;
            }
            if type_node.kind() == "array_type" {
                let Some(elem) = type_node.child_by_field_name("element") else {
                    return false;
                };
                let Some(dims) = type_node.child_by_field_name("dimensions") else {
                    return false;
                };
                return is_java_string_type_name(&read_node_text(elem, snapshot))
                    && count_array_dimensions(&read_node_text(dims, snapshot)) == 1;
            }
            false
        }
        "spread_parameter" => {
            let mut cursor = param.walk();
            for child in param.named_children(&mut cursor) {
                if !matches!(
                    child.kind(),
                    "modifiers" | "annotation" | "marker_annotation" | "variable_declarator"
                ) {
                    return is_java_string_type_name(&read_node_text(child, snapshot));
                }
            }
            false
        }
        _ => false,
    }
}

fn is_java_string_type_name(raw: &str) -> bool {
    let trimmed = raw.trim();
    trimmed == "String" || trimmed == "java.lang.String"
}

fn count_array_dimensions(dims_text: &str) -> usize {
    dims_text.bytes().filter(|&b| b == b'[').count()
}

fn collect_java_enclosing_types(node: Node<'_>, snapshot: &BufferSnapshot) -> Vec<ClassChainItem> {
    let mut chain = Vec::new();
    let mut cur = node.parent();
    while let Some(anc) = cur {
        let emits_test_class = anc.kind() == "class_declaration";
        if matches!(
            anc.kind(),
            "class_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "record_declaration"
        ) && let Some(name_node) = anc.child_by_field_name("name")
        {
            let name = read_node_text(name_node, snapshot);
            let line = name_node.start_position().row;
            chain.push(ClassChainItem {
                node_id: anc.id(),
                name,
                line,
                start_line: anc.start_position().row,
                end_line: effective_end_line(anc, line),
                byte_range: anc.byte_range(),
                is_nested: java_node_has_annotation(anc, snapshot, is_nested_annotation_name),
                emits_test_class,
            });
        }
        cur = anc.parent();
    }
    chain.reverse();
    chain
}

fn format_class_chain(chain: &[ClassChainItem]) -> Option<String> {
    if chain.is_empty() {
        return None;
    }
    let mut out = String::new();
    for item in chain {
        if !out.is_empty() {
            out.push('$');
        }
        out.push_str(&item.name);
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Kotlin detection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KotlinFuncScope {
    TopLevel,
    Class,
    Object,
    CompanionObject,
    NestedLocal,
}

fn detect_kotlin_entry_points(tree: &SyntaxTree, snapshot: &BufferSnapshot) -> Vec<EntryPoint> {
    let Some(query) = builtin_entry_points_query(SyntaxLanguage::Kotlin) else {
        return Vec::new();
    };
    let Some(root) = tree.root_node() else {
        return Vec::new();
    };
    let (package_name, matched_funcs) = collect_kotlin_matched_funcs(query, root, snapshot);

    let mut out = Vec::new();
    let mut test_classes = TestClassTracker::default();
    let pkg_ref = package_name.as_deref();

    for (func_node, name_node, params_node) in matched_funcs {
        let scope = classify_kotlin_function_scope(func_node);
        if scope == KotlinFuncScope::NestedLocal {
            continue;
        }

        let func_name = read_node_text(name_node, snapshot);
        let line = name_node.start_position().row;

        let is_jvm_static =
            kotlin_node_has_annotation(func_node, snapshot, is_jvm_static_annotation_name);
        let is_main_scope = match scope {
            KotlinFuncScope::TopLevel => true,
            KotlinFuncScope::Object | KotlinFuncScope::CompanionObject => is_jvm_static,
            KotlinFuncScope::Class | KotlinFuncScope::NestedLocal => false,
        };
        let is_main = is_main_scope
            && func_name == "main"
            && is_kotlin_main_signature(func_node, name_node, params_node, snapshot);
        let is_test = kotlin_node_has_annotation(func_node, snapshot, is_test_annotation_name);

        if !is_main && !is_test {
            continue;
        }

        let chain = collect_kotlin_enclosing_types(func_node, snapshot);
        let class_name = format_class_chain(&chain);

        if is_main {
            let main_class_name = match scope {
                KotlinFuncScope::Object | KotlinFuncScope::CompanionObject => class_name.clone(),
                KotlinFuncScope::TopLevel
                | KotlinFuncScope::Class
                | KotlinFuncScope::NestedLocal => None,
            };
            push_jvm_main_entry_point(
                &mut out,
                EntryPointKind::KotlinMain,
                pkg_ref,
                main_class_name.as_deref(),
                &func_name,
                line,
                func_node,
                snapshot,
            );
        }

        if is_test {
            push_jvm_test_method_entry_point(
                &mut out,
                pkg_ref,
                class_name.as_deref(),
                &func_name,
                line,
                func_node,
                snapshot,
            );
            let test_chain: Vec<ClassChainItem> =
                chain.into_iter().filter(|c| c.emits_test_class).collect();
            if !test_chain.is_empty() {
                test_classes.ensure_chain(&test_chain, snapshot, true);
            }
        }
    }

    test_classes.emit(pkg_ref, &mut out);
    out
}

fn collect_kotlin_matched_funcs<'a>(
    query: &tree_sitter::Query,
    root: Node<'a>,
    snapshot: &BufferSnapshot,
) -> (Option<String>, Vec<(Node<'a>, Node<'a>, Node<'a>)>) {
    let idx_package_name = query.capture_index_for_name("package.name");
    let idx_func_decl = query.capture_index_for_name("function.decl");
    let idx_func_name = query.capture_index_for_name("function.name");
    let idx_func_params = query.capture_index_for_name("function.params");

    let mut package_name: Option<String> = None;
    let mut matched_funcs: Vec<(Node<'a>, Node<'a>, Node<'a>)> = Vec::new();
    let mut cursor = QueryCursor::new();
    let provider = RopeTextProvider::from_snapshot(snapshot);
    let mut matches = cursor.matches(query, root, provider);

    while let Some(mat) = matches.next() {
        let mut decl_node = None;
        let mut name_node = None;
        let mut params_node = None;

        for cap in mat.captures {
            if Some(cap.index) == idx_package_name {
                if package_name.is_none() {
                    let pkg = read_node_text(cap.node, snapshot);
                    let trimmed = pkg.trim();
                    if !trimmed.is_empty() {
                        package_name = Some(trimmed.to_owned());
                    }
                }
            } else if Some(cap.index) == idx_func_decl {
                decl_node = Some(cap.node);
            } else if Some(cap.index) == idx_func_name {
                name_node = Some(cap.node);
            } else if Some(cap.index) == idx_func_params {
                params_node = Some(cap.node);
            }
        }

        if let (Some(decl), Some(name), Some(params)) = (decl_node, name_node, params_node) {
            matched_funcs.push((decl, name, params));
        }
    }

    (package_name, matched_funcs)
}

fn classify_kotlin_function_scope(func_node: Node<'_>) -> KotlinFuncScope {
    let mut cur = func_node.parent();
    while let Some(anc) = cur {
        match anc.kind() {
            "source_file" => return KotlinFuncScope::TopLevel,
            "class_body" | "ERROR" => {
                cur = anc.parent();
            }
            "class_declaration" => return KotlinFuncScope::Class,
            "object_declaration" => return KotlinFuncScope::Object,
            "companion_object" => return KotlinFuncScope::CompanionObject,
            _ => return KotlinFuncScope::NestedLocal,
        }
    }
    KotlinFuncScope::NestedLocal
}

fn collect_kotlin_enclosing_types(
    func_node: Node<'_>,
    snapshot: &BufferSnapshot,
) -> Vec<ClassChainItem> {
    let mut chain = Vec::new();
    let mut cur = func_node.parent();
    while let Some(anc) = cur {
        match anc.kind() {
            "class_declaration" | "object_declaration" => {
                if let Some(name_node) = anc.child_by_field_name("name") {
                    let name = read_node_text(name_node, snapshot);
                    let line = name_node.start_position().row;
                    let emits_test_class = anc.kind() == "class_declaration";
                    chain.push(ClassChainItem {
                        node_id: anc.id(),
                        name,
                        line,
                        start_line: anc.start_position().row,
                        end_line: effective_end_line(anc, line),
                        byte_range: anc.byte_range(),
                        is_nested: kotlin_node_has_annotation(
                            anc,
                            snapshot,
                            is_nested_annotation_name,
                        ),
                        emits_test_class,
                    });
                }
            }
            // For `companion_object`, `@JvmStatic` emits the static `main` method on the
            // enclosing `class_declaration` itself rather than on `$Companion`.
            "companion_object" | "class_body" | "source_file" | "ERROR" => {}
            _ => break,
        }
        cur = anc.parent();
    }
    chain.reverse();
    chain
}

fn kotlin_node_has_annotation(
    node: Node<'_>,
    snapshot: &BufferSnapshot,
    predicate: fn(&str) -> bool,
) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "modifiers" {
            continue;
        }
        let mut mod_cursor = child.walk();
        for mod_child in child.children(&mut mod_cursor) {
            if mod_child.kind() == "annotation"
                && let Some(ann_name) = extract_kotlin_annotation_name(mod_child, snapshot)
                && predicate(&ann_name)
            {
                return true;
            }
        }
    }
    false
}

fn extract_kotlin_annotation_name(
    annotation_node: Node<'_>,
    snapshot: &BufferSnapshot,
) -> Option<String> {
    let mut cursor = annotation_node.walk();
    for child in annotation_node.named_children(&mut cursor) {
        match child.kind() {
            "user_type" => return Some(extract_kotlin_user_type_name(child, snapshot)),
            "constructor_invocation" => {
                let mut ctor_cursor = child.walk();
                for ctor_child in child.named_children(&mut ctor_cursor) {
                    if ctor_child.kind() == "user_type" {
                        return Some(extract_kotlin_user_type_name(ctor_child, snapshot));
                    }
                }
            }
            _ => {}
        }
    }
    None
}

fn extract_kotlin_user_type_name(user_type: Node<'_>, snapshot: &BufferSnapshot) -> String {
    let mut parts = Vec::new();
    let mut cursor = user_type.walk();
    for child in user_type.named_children(&mut cursor) {
        if child.kind() == "identifier" {
            parts.push(read_node_text(child, snapshot));
        }
    }
    parts.join(".")
}

fn is_kotlin_main_signature(
    func_node: Node<'_>,
    name_node: Node<'_>,
    params_node: Node<'_>,
    snapshot: &BufferSnapshot,
) -> bool {
    let name_byte = name_node.start_byte();
    let params_end = params_node.end_byte();

    let mut cursor = func_node.walk();
    for child in func_node.named_children(&mut cursor) {
        if child.start_byte() < name_byte
            && !matches!(
                child.kind(),
                "modifiers" | "type_parameters" | "type_modifiers"
            )
        {
            // Extension receiver like `fun Foo.main()` is not a JVM entry point.
            return false;
        }
        if child.start_byte() >= params_end
            && !matches!(
                child.kind(),
                "function_body" | "type_constraints" | "type_modifiers"
            )
        {
            // Explicit return type after `(...)` must be `Unit` / `kotlin.Unit`.
            let ret_text = read_node_text(child, snapshot);
            let trimmed = ret_text.trim();
            if trimmed != "Unit" && trimmed != "kotlin.Unit" {
                return false;
            }
        }
    }

    let mut params = Vec::new();
    let mut p_cursor = params_node.walk();
    for child in params_node.named_children(&mut p_cursor) {
        if child.kind() == "parameter" {
            params.push(child);
        }
    }

    match params.as_slice() {
        [] => true,
        [param] => is_kotlin_string_array_param(*param, snapshot),
        _ => false,
    }
}

fn is_kotlin_string_array_param(param: Node<'_>, snapshot: &BufferSnapshot) -> bool {
    let mut cursor = param.walk();
    let Some(type_node) = param
        .named_children(&mut cursor)
        .find(|c| c.kind() != "identifier")
    else {
        return false;
    };
    if type_node.kind() != "user_type" {
        return false;
    }

    let base_name = extract_kotlin_user_type_name(type_node, snapshot);
    if base_name != "Array" && base_name != "kotlin.Array" {
        return false;
    }

    let mut ut_cursor = type_node.walk();
    let Some(type_args) = type_node
        .named_children(&mut ut_cursor)
        .find(|c| c.kind() == "type_arguments")
    else {
        return false;
    };

    let mut ta_cursor = type_args.walk();
    let projections: Vec<Node<'_>> = type_args
        .named_children(&mut ta_cursor)
        .filter(|c| c.kind() == "type_projection")
        .collect();
    if projections.len() != 1 {
        return false;
    }

    let mut proj_cursor = projections[0].walk();
    let Some(inner_type) = projections[0]
        .named_children(&mut proj_cursor)
        .find(|c| c.kind() == "user_type")
    else {
        return false;
    };

    // Reject nested generics inside `String` (such as invalid type arguments).
    let mut inner_cursor = inner_type.walk();
    if inner_type
        .named_children(&mut inner_cursor)
        .any(|c| c.kind() == "type_arguments")
    {
        return false;
    }

    let elem_name = extract_kotlin_user_type_name(inner_type, snapshot);
    elem_name == "String" || elem_name == "kotlin.String"
}

// ---------------------------------------------------------------------------
// HTTP (.http) detection
// ---------------------------------------------------------------------------

fn detect_http_entry_points(tree: &SyntaxTree, snapshot: &BufferSnapshot) -> Vec<EntryPoint> {
    let Some(query) = builtin_entry_points_query(SyntaxLanguage::Http) else {
        return Vec::new();
    };

    let idx_request = query.capture_index_for_name("http.request");
    let idx_method = query.capture_index_for_name("http.method");
    let idx_url = query.capture_index_for_name("http.url");

    let Some(root) = tree.root_node() else {
        return Vec::new();
    };
    let mut cursor = QueryCursor::new();
    let provider = RopeTextProvider::from_snapshot(snapshot);
    let mut matches = cursor.matches(query, root, provider);

    let mut out = Vec::new();
    let mut min_search_row = 0usize;

    while let Some(mat) = matches.next() {
        let mut req_node = None;
        let mut method_node = None;
        let mut url_node = None;

        for cap in mat.captures {
            if Some(cap.index) == idx_request {
                req_node = Some(cap.node);
            } else if Some(cap.index) == idx_method {
                method_node = Some(cap.node);
            } else if Some(cap.index) == idx_url {
                url_node = Some(cap.node);
            }
        }

        let (Some(req_node), Some(method_node), Some(url_node)) = (req_node, method_node, url_node)
        else {
            continue;
        };

        let method = read_node_text(method_node, snapshot)
            .trim()
            .to_ascii_uppercase();
        if !is_supported_http_method(&method) {
            continue;
        }

        let url = read_node_text(url_node, snapshot).trim().to_owned();
        if url.is_empty() {
            continue;
        }

        let line = method_node.start_position().row;
        let start_line = req_node.start_position().row.min(line);
        let end_line = effective_end_line(req_node, line);
        let req_name = find_http_request_name(snapshot, line, min_search_row);

        let idx = out.len();
        let label = match req_name {
            Some(name) => format!("Send '{name}: {method} {url}'"),
            None => format!("Send '{method} {url}'"),
        };

        out.push(EntryPoint {
            id: format!("http#{idx}"),
            kind: EntryPointKind::HttpRequest,
            icon_role: GutterIconRole::Run,
            label,
            line,
            start_line,
            end_line,
            byte_range: clamp_byte_range(req_node.byte_range(), snapshot.len_bytes()),
            package_name: None,
            class_name: None,
            method_name: None,
            request_index: Some(idx),
            http_method: Some(method),
            http_url: Some(url),
        });

        let raw_end = req_node.end_position();
        min_search_row = if raw_end.column == 0 && raw_end.row > start_line {
            raw_end.row
        } else {
            raw_end.row + 1
        };
    }

    out
}

fn is_supported_http_method(method: &str) -> bool {
    matches!(
        method,
        "GET" | "POST" | "PUT" | "DELETE" | "PATCH" | "HEAD" | "OPTIONS" | "TRACE" | "CONNECT"
    )
}

fn find_http_request_name(
    snapshot: &BufferSnapshot,
    request_line: usize,
    min_search_row: usize,
) -> Option<String> {
    if request_line <= min_search_row {
        return None;
    }

    let mut separator_title: Option<String> = None;
    for row in (min_search_row..request_line).rev() {
        let Ok(slice) = snapshot.line_slice(row) else {
            continue;
        };
        let text = slice.to_string();
        let trimmed = text.trim();

        if let Some(rest) = trimmed.strip_prefix("###") {
            let title = rest.trim();
            if !title.is_empty() && separator_title.is_none() {
                separator_title = Some(title.to_owned());
            }
            // Section separator marks the upper boundary of the current request block.
            break;
        }

        let comment_body = trimmed
            .strip_prefix('#')
            .or_else(|| trimmed.strip_prefix("//"))
            .map(str::trim);
        if let Some(body) = comment_body
            && let Some(after_tag) = body.strip_prefix("@name")
        {
            let name = after_tag.trim_start_matches([' ', '\t', '=']).trim();
            if !name.is_empty() {
                return Some(name.to_owned());
            }
        }
    }

    separator_title
}
