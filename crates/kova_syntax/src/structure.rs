//! Hierarchical code structure outline extraction (`StructureOutline`, `StructureNode`, `extract_structure`).

use std::ops::Range;

use kova_buffer::{BufferSnapshot, BufferVersion, Point};
use kova_theme::SyntaxLanguage;
use tree_sitter::Node;

use crate::input::{from_ts_point, read_node_text, read_node_text_compact};
use crate::tree::SyntaxTree;

/// Maximum nesting depth traversed during structure extraction to prevent stack overflow
/// on pathological inputs (e.g. 500-level nested JSON).
pub const MAX_STRUCTURE_DEPTH: usize = 128;

/// Semantic kind of a symbol in the file structure outline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StructureSymbolKind {
    /// Package declaration.
    Package,
    /// Class declaration.
    Class,
    /// Interface declaration.
    Interface,
    /// Enum declaration.
    Enum,
    /// Record declaration.
    Record,
    /// Annotation type declaration.
    Annotation,
    /// Kotlin `object` or `companion object`.
    Object,
    /// Constructor declaration.
    Constructor,
    /// Member method declaration.
    Method,
    /// Top-level or local function declaration.
    Function,
    /// Class/record field declaration.
    Field,
    /// Property, TOML key, or JSON key.
    Property,
    /// Enum constant / entry.
    EnumConstant,
    /// TOML table section, HTTP request, or Gradle DSL block.
    Section,
}

/// Visibility modifier of a symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SymbolVisibility {
    /// Publicly accessible (`public`).
    Public,
    /// Protected access (`protected`).
    Protected,
    /// Java package-private (no explicit access modifier).
    PackagePrivate,
    /// Private access (`private`).
    Private,
    /// Kotlin `internal` access.
    Internal,
    /// Visibility not applicable or unspecified (e.g. JSON/TOML/HTTP).
    #[default]
    Unspecified,
}

/// A single node in the hierarchical file structure tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructureNode {
    /// Display name of the symbol.
    pub name: String,
    /// Optional signature or type detail (e.g. `(Long, String): Optional<Order>`).
    pub detail: Option<String>,
    /// Semantic kind of the symbol.
    pub kind: StructureSymbolKind,
    /// Visibility of the symbol.
    pub visibility: SymbolVisibility,
    /// Whether the symbol is `static`.
    pub is_static: bool,
    /// Whether the symbol is `abstract`.
    pub is_abstract: bool,
    /// Full byte range of the symbol's AST node.
    pub full_range: Range<usize>,
    /// Byte range of the symbol's identifier / name for cursor selection.
    pub selection_range: Range<usize>,
    /// Start point of `full_range`.
    pub start_point: Point,
    /// End point of `full_range`.
    pub end_point: Point,
    /// Start point of `selection_range`.
    pub selection_point: Point,
    /// Nested child symbols.
    pub children: Vec<StructureNode>,
}

/// Hierarchical structure outline for a parsed buffer snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructureOutline {
    /// Language of the parsed buffer.
    pub language: SyntaxLanguage,
    /// Buffer version this outline was extracted from.
    pub version: BufferVersion,
    /// Top-level symbol nodes.
    pub roots: Vec<StructureNode>,
}

impl StructureOutline {
    /// Returns `true` if the outline contains no symbols.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// Returns the total number of symbol nodes in the tree.
    #[must_use]
    pub fn symbol_count(&self) -> usize {
        let mut count = 0usize;
        let mut stack: Vec<&StructureNode> = self.roots.iter().collect();
        while let Some(node) = stack.pop() {
            count += 1;
            stack.extend(node.children.iter());
        }
        count
    }

    /// Returns the path from a root symbol down to the deepest symbol containing `byte`.
    ///
    /// Used by breadcrumbs and cursor-to-structure synchronization.
    #[must_use]
    pub fn path_at_byte(&self, byte: usize) -> Vec<&StructureNode> {
        let mut path = Vec::new();
        let mut current_level: &[StructureNode] = &self.roots;
        while let Some(found) = current_level
            .iter()
            .find(|node| node.full_range.start <= byte && byte < node.full_range.end)
        {
            path.push(found);
            current_level = &found.children;
        }
        path
    }
}

/// Extracts a [`StructureOutline`] from `tree` and `snapshot`.
#[must_use]
pub fn extract_structure(tree: &SyntaxTree, snapshot: &BufferSnapshot) -> StructureOutline {
    let language = tree.language();
    let version = snapshot.version();
    if let Some(doc) = tree.scanned() {
        return StructureOutline {
            language,
            version,
            roots: doc.structure().to_vec(),
        };
    }

    let Some(root) = tree.root_node() else {
        return StructureOutline {
            language,
            version,
            roots: Vec::new(),
        };
    };

    let roots = match language {
        SyntaxLanguage::Java => extract_java_roots(root, snapshot),
        SyntaxLanguage::Kotlin => extract_kotlin_roots(root, snapshot),
        SyntaxLanguage::Http => extract_http_roots(root, snapshot),
        SyntaxLanguage::Toml => extract_toml_roots(root, snapshot),
        SyntaxLanguage::Json => extract_json_roots(root, snapshot),
        SyntaxLanguage::Yaml
        | SyntaxLanguage::Xml
        | SyntaxLanguage::Groovy
        | SyntaxLanguage::Sql => Vec::new(),
    };

    StructureOutline {
        language,
        version,
        roots,
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "Internal helper constructing StructureNode fields directly"
)]
fn make_node(
    name: String,
    detail: Option<String>,
    kind: StructureSymbolKind,
    visibility: SymbolVisibility,
    is_static: bool,
    is_abstract: bool,
    full_node: Node<'_>,
    selection_node: Node<'_>,
    children: Vec<StructureNode>,
) -> StructureNode {
    StructureNode {
        name,
        detail,
        kind,
        visibility,
        is_static,
        is_abstract,
        full_range: full_node.byte_range(),
        selection_range: selection_node.byte_range(),
        start_point: from_ts_point(full_node.start_position()),
        end_point: from_ts_point(full_node.end_position()),
        selection_point: from_ts_point(selection_node.start_position()),
        children,
    }
}

fn find_child_by_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find(|child| child.kind() == kind)
}

// ---------------------------------------------------------------------------
// Java
// ---------------------------------------------------------------------------

fn extract_java_roots(root: Node<'_>, snapshot: &BufferSnapshot) -> Vec<StructureNode> {
    let mut roots = Vec::new();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if let Some(node) = extract_java_type(child, snapshot, 0) {
            roots.push(node);
        }
    }
    roots
}

fn parse_java_modifiers(
    node: Node<'_>,
    snapshot: &BufferSnapshot,
    in_interface: bool,
) -> (SymbolVisibility, bool, bool) {
    let Some(mods) = find_child_by_kind(node, "modifiers") else {
        let vis = if in_interface {
            SymbolVisibility::Public
        } else {
            SymbolVisibility::PackagePrivate
        };
        return (vis, false, false);
    };
    let text = read_node_text(mods, snapshot);
    let mut vis = if in_interface {
        SymbolVisibility::Public
    } else {
        SymbolVisibility::PackagePrivate
    };
    let mut is_static = false;
    let mut is_abstract = false;
    for token in text.split_whitespace() {
        match token {
            "public" => vis = SymbolVisibility::Public,
            "protected" => vis = SymbolVisibility::Protected,
            "private" => vis = SymbolVisibility::Private,
            "static" => is_static = true,
            "abstract" => is_abstract = true,
            _ => {}
        }
    }
    (vis, is_static, is_abstract)
}

fn extract_java_type(
    node: Node<'_>,
    snapshot: &BufferSnapshot,
    depth: usize,
) -> Option<StructureNode> {
    if depth >= MAX_STRUCTURE_DEPTH {
        return None;
    }
    let (kind, body_kind) = match node.kind() {
        "class_declaration" => (StructureSymbolKind::Class, "class_body"),
        "interface_declaration" => (StructureSymbolKind::Interface, "interface_body"),
        "enum_declaration" => (StructureSymbolKind::Enum, "enum_body"),
        "record_declaration" => (StructureSymbolKind::Record, "class_body"),
        "annotation_type_declaration" => (StructureSymbolKind::Annotation, "annotation_type_body"),
        _ => return None,
    };

    let name_node = node.child_by_field_name("name")?;
    let name = read_node_text_compact(name_node, snapshot, 128);
    if name.is_empty() {
        return None;
    }
    let (visibility, is_static, is_abstract) = parse_java_modifiers(node, snapshot, false);
    let in_interface = kind == StructureSymbolKind::Interface;

    let mut children = Vec::new();
    if let Some(body) = node
        .child_by_field_name("body")
        .or_else(|| find_child_by_kind(node, body_kind))
    {
        extract_java_body_members(body, snapshot, in_interface, depth + 1, &mut children);
    }

    Some(make_node(
        name,
        None,
        kind,
        visibility,
        is_static,
        is_abstract,
        node,
        name_node,
        children,
    ))
}

#[allow(
    clippy::too_many_lines,
    reason = "Cohesive match over Java AST member kinds"
)]
fn extract_java_body_members(
    body: Node<'_>,
    snapshot: &BufferSnapshot,
    in_interface: bool,
    depth: usize,
    out: &mut Vec<StructureNode>,
) {
    if depth >= MAX_STRUCTURE_DEPTH {
        return;
    }
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        match child.kind() {
            "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "annotation_type_declaration" => {
                if let Some(nested) = extract_java_type(child, snapshot, depth) {
                    out.push(nested);
                }
            }
            "method_declaration" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = read_node_text_compact(name_node, snapshot, 128);
                    if !name.is_empty() {
                        let params = child.child_by_field_name("parameters").map_or_else(
                            || "()".to_owned(),
                            |p| read_node_text_compact(p, snapshot, 128),
                        );
                        let ret = child
                            .child_by_field_name("type")
                            .map(|t| read_node_text_compact(t, snapshot, 64));
                        let detail = Some(match ret {
                            Some(r) if !r.is_empty() => format!("{params}: {r}"),
                            _ => params,
                        });
                        let (visibility, is_static, is_abstract) =
                            parse_java_modifiers(child, snapshot, in_interface);
                        out.push(make_node(
                            name,
                            detail,
                            StructureSymbolKind::Method,
                            visibility,
                            is_static,
                            is_abstract,
                            child,
                            name_node,
                            Vec::new(),
                        ));
                    }
                }
            }
            "constructor_declaration" | "compact_constructor_declaration" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = read_node_text_compact(name_node, snapshot, 128);
                    if !name.is_empty() {
                        let detail = child
                            .child_by_field_name("parameters")
                            .map(|p| read_node_text_compact(p, snapshot, 128));
                        let (visibility, is_static, is_abstract) =
                            parse_java_modifiers(child, snapshot, false);
                        out.push(make_node(
                            name,
                            detail,
                            StructureSymbolKind::Constructor,
                            visibility,
                            is_static,
                            is_abstract,
                            child,
                            name_node,
                            Vec::new(),
                        ));
                    }
                }
            }
            "field_declaration" => {
                let field_type = child
                    .child_by_field_name("type")
                    .map(|t| read_node_text_compact(t, snapshot, 64));
                let (visibility, is_static, is_abstract) =
                    parse_java_modifiers(child, snapshot, in_interface);
                let mut fcursor = child.walk();
                for fchild in child.children(&mut fcursor) {
                    if fchild.kind() == "variable_declarator"
                        && let Some(name_node) = fchild.child_by_field_name("name")
                    {
                        let name = read_node_text_compact(name_node, snapshot, 128);
                        if !name.is_empty() {
                            out.push(make_node(
                                name,
                                field_type.clone(),
                                StructureSymbolKind::Field,
                                visibility,
                                is_static,
                                is_abstract,
                                fchild,
                                name_node,
                                Vec::new(),
                            ));
                        }
                    }
                }
            }
            "enum_constant" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = read_node_text_compact(name_node, snapshot, 128);
                    if !name.is_empty() {
                        out.push(make_node(
                            name,
                            None,
                            StructureSymbolKind::EnumConstant,
                            SymbolVisibility::Public,
                            true,
                            false,
                            child,
                            name_node,
                            Vec::new(),
                        ));
                    }
                }
            }
            "enum_body_declarations" => {
                extract_java_body_members(child, snapshot, false, depth, out);
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Kotlin
// ---------------------------------------------------------------------------

fn extract_kotlin_roots(root: Node<'_>, snapshot: &BufferSnapshot) -> Vec<StructureNode> {
    let mut roots = Vec::new();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if let Some(node) = extract_kotlin_member(child, snapshot, false, 0) {
            roots.push(node);
        }
    }
    roots
}

fn parse_kotlin_visibility(node: Node<'_>, snapshot: &BufferSnapshot) -> (SymbolVisibility, bool) {
    let Some(mods) = find_child_by_kind(node, "modifiers") else {
        return (SymbolVisibility::Public, false);
    };
    let text = read_node_text(mods, snapshot);
    let mut vis = SymbolVisibility::Public;
    let mut is_abstract = false;
    for token in text.split_whitespace() {
        match token {
            "private" => vis = SymbolVisibility::Private,
            "protected" => vis = SymbolVisibility::Protected,
            "internal" => vis = SymbolVisibility::Internal,
            "public" => vis = SymbolVisibility::Public,
            "abstract" => is_abstract = true,
            _ => {}
        }
    }
    (vis, is_abstract)
}

fn find_kotlin_identifier(node: Node<'_>) -> Option<Node<'_>> {
    node.child_by_field_name("name").or_else(|| {
        let mut cursor = node.walk();
        node.children(&mut cursor).find(|c| {
            matches!(
                c.kind(),
                "simple_identifier" | "type_identifier" | "identifier"
            )
        })
    })
}

#[allow(
    clippy::too_many_lines,
    reason = "Cohesive match over Java AST member kinds"
)]
fn extract_kotlin_member(
    node: Node<'_>,
    snapshot: &BufferSnapshot,
    inside_type: bool,
    depth: usize,
) -> Option<StructureNode> {
    if depth >= MAX_STRUCTURE_DEPTH {
        return None;
    }
    match node.kind() {
        "class_declaration" => {
            let name_node = find_kotlin_identifier(node)?;
            let name = read_node_text_compact(name_node, snapshot, 128);
            if name.is_empty() {
                return None;
            }
            let raw_header = read_node_text(node, snapshot);
            let kind = if find_child_by_kind(node, "interface").is_some()
                || raw_header.trim_start().starts_with("interface ")
            {
                StructureSymbolKind::Interface
            } else if let Some(mods) = find_child_by_kind(node, "modifiers") {
                let mtext = read_node_text(mods, snapshot);
                if mtext.contains("enum") {
                    StructureSymbolKind::Enum
                } else if mtext.contains("annotation") {
                    StructureSymbolKind::Annotation
                } else {
                    StructureSymbolKind::Class
                }
            } else {
                StructureSymbolKind::Class
            };
            let (visibility, is_abstract) = parse_kotlin_visibility(node, snapshot);
            let mut children = Vec::new();
            if let Some(body) = find_child_by_kind(node, "class_body")
                .or_else(|| find_child_by_kind(node, "enum_class_body"))
            {
                extract_kotlin_body_members(body, snapshot, depth + 1, &mut children);
            }
            Some(make_node(
                name,
                None,
                kind,
                visibility,
                false,
                is_abstract,
                node,
                name_node,
                children,
            ))
        }
        "object_declaration" => {
            let name_node = find_kotlin_identifier(node)?;
            let name = read_node_text_compact(name_node, snapshot, 128);
            if name.is_empty() {
                return None;
            }
            let (visibility, is_abstract) = parse_kotlin_visibility(node, snapshot);
            let mut children = Vec::new();
            if let Some(body) = find_child_by_kind(node, "class_body") {
                extract_kotlin_body_members(body, snapshot, depth + 1, &mut children);
            }
            Some(make_node(
                name,
                None,
                StructureSymbolKind::Object,
                visibility,
                false,
                is_abstract,
                node,
                name_node,
                children,
            ))
        }
        "companion_object" => {
            let name_node = find_kotlin_identifier(node).unwrap_or(node);
            let name = if name_node == node {
                "companion object".to_owned()
            } else {
                let n = read_node_text_compact(name_node, snapshot, 128);
                if n.is_empty() {
                    "companion object".to_owned()
                } else {
                    n
                }
            };
            let (visibility, _) = parse_kotlin_visibility(node, snapshot);
            let mut children = Vec::new();
            if let Some(body) = find_child_by_kind(node, "class_body") {
                extract_kotlin_body_members(body, snapshot, depth + 1, &mut children);
            }
            Some(make_node(
                name,
                None,
                StructureSymbolKind::Object,
                visibility,
                true,
                false,
                node,
                name_node,
                children,
            ))
        }
        "function_declaration" => {
            let name_node = find_kotlin_identifier(node)?;
            let name = read_node_text_compact(name_node, snapshot, 128);
            if name.is_empty() {
                return None;
            }
            let params = find_child_by_kind(node, "function_value_parameters").map_or_else(
                || "()".to_owned(),
                |p| read_node_text_compact(p, snapshot, 128),
            );
            let (visibility, is_abstract) = parse_kotlin_visibility(node, snapshot);
            let kind = if inside_type {
                StructureSymbolKind::Method
            } else {
                StructureSymbolKind::Function
            };
            Some(make_node(
                name,
                Some(params),
                kind,
                visibility,
                false,
                is_abstract,
                node,
                name_node,
                Vec::new(),
            ))
        }
        "secondary_constructor" => {
            let (visibility, _) = parse_kotlin_visibility(node, snapshot);
            let params = find_child_by_kind(node, "function_value_parameters")
                .map(|p| read_node_text_compact(p, snapshot, 128));
            Some(make_node(
                "constructor".to_owned(),
                params,
                StructureSymbolKind::Constructor,
                visibility,
                false,
                false,
                node,
                node,
                Vec::new(),
            ))
        }
        "property_declaration" => {
            let var_decl = find_child_by_kind(node, "variable_declaration")?;
            let name_node = find_kotlin_identifier(var_decl)?;
            let name = read_node_text_compact(name_node, snapshot, 128);
            if name.is_empty() {
                return None;
            }
            let (visibility, is_abstract) = parse_kotlin_visibility(node, snapshot);
            Some(make_node(
                name,
                None,
                StructureSymbolKind::Property,
                visibility,
                false,
                is_abstract,
                node,
                name_node,
                Vec::new(),
            ))
        }
        "enum_entry" => {
            let name_node = find_kotlin_identifier(node)?;
            let name = read_node_text_compact(name_node, snapshot, 128);
            if name.is_empty() {
                return None;
            }
            Some(make_node(
                name,
                None,
                StructureSymbolKind::EnumConstant,
                SymbolVisibility::Public,
                true,
                false,
                node,
                name_node,
                Vec::new(),
            ))
        }
        "call_expression" if !inside_type => {
            // Support Gradle Kotlin DSL top-level blocks such as `plugins { ... }`, `dependencies { ... }`
            let callee = node.child(0)?;
            let name = read_node_text_compact(callee, snapshot, 64);
            if name.is_empty() {
                return None;
            }
            Some(make_node(
                name,
                None,
                StructureSymbolKind::Section,
                SymbolVisibility::Unspecified,
                false,
                false,
                node,
                callee,
                Vec::new(),
            ))
        }
        _ => None,
    }
}

fn extract_kotlin_body_members(
    body: Node<'_>,
    snapshot: &BufferSnapshot,
    depth: usize,
    out: &mut Vec<StructureNode>,
) {
    if depth >= MAX_STRUCTURE_DEPTH {
        return;
    }
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        if let Some(member) = extract_kotlin_member(child, snapshot, true, depth) {
            out.push(member);
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP (.http)
// ---------------------------------------------------------------------------

fn extract_http_roots(root: Node<'_>, snapshot: &BufferSnapshot) -> Vec<StructureNode> {
    let mut roots = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "request" {
            let first_line = read_node_text(node, snapshot)
                .lines()
                .next()
                .map_or("", str::trim)
                .to_owned();
            if !first_line.is_empty() {
                roots.push(make_node(
                    first_line,
                    None,
                    StructureSymbolKind::Section,
                    SymbolVisibility::Unspecified,
                    false,
                    false,
                    node,
                    node,
                    Vec::new(),
                ));
            }
        } else {
            let mut cursor = node.walk();
            let mut children: Vec<Node<'_>> = node.children(&mut cursor).collect();
            children.reverse();
            stack.extend(children);
        }
    }
    roots
}

// ---------------------------------------------------------------------------
// TOML
// ---------------------------------------------------------------------------

fn extract_toml_roots(root: Node<'_>, snapshot: &BufferSnapshot) -> Vec<StructureNode> {
    let mut roots = Vec::new();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        match child.kind() {
            "pair" => {
                if let Some(prop) = extract_toml_pair(child, snapshot) {
                    roots.push(prop);
                }
            }
            "table" | "table_array_element" => {
                let mut tcursor = child.walk();
                let mut name_node = None;
                let mut children = Vec::new();
                for tchild in child.children(&mut tcursor) {
                    match tchild.kind() {
                        "bare_key" | "quoted_key" | "dotted_key" if name_node.is_none() => {
                            name_node = Some(tchild);
                        }
                        "pair" => {
                            if let Some(prop) = extract_toml_pair(tchild, snapshot) {
                                children.push(prop);
                            }
                        }
                        _ => {}
                    }
                }
                if let Some(nnode) = name_node {
                    let name = read_node_text_compact(nnode, snapshot, 128);
                    if !name.is_empty() {
                        roots.push(make_node(
                            name,
                            None,
                            StructureSymbolKind::Section,
                            SymbolVisibility::Unspecified,
                            false,
                            false,
                            child,
                            nnode,
                            children,
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    roots
}

fn extract_toml_pair(node: Node<'_>, snapshot: &BufferSnapshot) -> Option<StructureNode> {
    let mut cursor = node.walk();
    let key_node = node
        .children(&mut cursor)
        .find(|c| matches!(c.kind(), "bare_key" | "quoted_key" | "dotted_key"))?;
    let name = read_node_text_compact(key_node, snapshot, 128);
    if name.is_empty() {
        return None;
    }
    Some(make_node(
        name,
        None,
        StructureSymbolKind::Property,
        SymbolVisibility::Unspecified,
        false,
        false,
        node,
        key_node,
        Vec::new(),
    ))
}

// ---------------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------------

fn extract_json_roots(root: Node<'_>, snapshot: &BufferSnapshot) -> Vec<StructureNode> {
    let mut roots = Vec::new();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        extract_json_value(child, snapshot, 0, &mut roots);
    }
    roots
}

fn extract_json_value(
    node: Node<'_>,
    snapshot: &BufferSnapshot,
    depth: usize,
    out: &mut Vec<StructureNode>,
) {
    if depth >= MAX_STRUCTURE_DEPTH {
        return;
    }
    match node.kind() {
        "object" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "pair"
                    && let Some(prop) = extract_json_pair(child, snapshot, depth)
                {
                    out.push(prop);
                }
            }
        }
        "array" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_json_value(child, snapshot, depth + 1, out);
            }
        }
        _ => {}
    }
}

fn extract_json_pair(
    node: Node<'_>,
    snapshot: &BufferSnapshot,
    depth: usize,
) -> Option<StructureNode> {
    if depth >= MAX_STRUCTURE_DEPTH {
        return None;
    }
    let key_node = node.child_by_field_name("key")?;
    let raw_key = read_node_text_compact(key_node, snapshot, 128);
    let name = raw_key.trim_matches('"').to_owned();
    if name.is_empty() {
        return None;
    }
    let mut children = Vec::new();
    if let Some(val) = node.child_by_field_name("value") {
        extract_json_value(val, snapshot, depth + 1, &mut children);
    }
    Some(make_node(
        name,
        None,
        StructureSymbolKind::Property,
        SymbolVisibility::Unspecified,
        false,
        false,
        node,
        key_node,
        children,
    ))
}
