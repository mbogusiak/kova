//! Tree-sitter source scanner for `.java`, `.kt`, and `.kts` files.
//!
//! Extracts package declarations, top-level and nested types, and their members
//! with 0-based `line:column` positions for immediate Go-to-Class / Go-to-Symbol
//! navigation without waiting for an external language server.

use std::cell::RefCell;
use std::path::Path;

use kova_syntax::{LanguageRegistry, detect_language};
use tree_sitter::{Node, Parser};

use crate::error::JvmIndexError;
use crate::model::{
    JvmMemberEntry, JvmModifiers, JvmSourceOrigin, JvmSymbolKind, JvmTypeEntry, JvmVisibility,
    ParsedClassUnit,
};

/// Default maximum source file size in bytes (4 MiB) before skipping.
pub const DEFAULT_MAX_SOURCE_BYTES: usize = 4 * 1024 * 1024;

/// Configuration options for scanning `.java`, `.kt`, and `.kts` source files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceScanOptions {
    /// Whether to extract methods, constructors, and fields.
    pub include_members: bool,
    /// Whether to include `private` members in extracted units.
    pub include_private_members: bool,
    /// Maximum source file size in bytes; larger files are skipped.
    pub max_source_bytes: usize,
}

impl Default for SourceScanOptions {
    fn default() -> Self {
        Self {
            include_members: true,
            include_private_members: true,
            max_source_bytes: DEFAULT_MAX_SOURCE_BYTES,
        }
    }
}

thread_local! {
    static JAVA_PARSER: RefCell<Option<Parser>> = const { RefCell::new(None) };
    static KOTLIN_PARSER: RefCell<Option<Parser>> = const { RefCell::new(None) };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JvmSourceLang {
    Java,
    Kotlin,
}

fn parse_with_thread_local(
    language: JvmSourceLang,
    source: &str,
) -> Result<tree_sitter::Tree, JvmIndexError> {
    let registry = LanguageRegistry::builtin().map_err(|err| JvmIndexError::Io {
        path: String::from("<tree-sitter-registry>"),
        message: err.to_string(),
    })?;
    let probe = match language {
        JvmSourceLang::Java => Path::new("Probe.java"),
        JvmSourceLang::Kotlin => Path::new("Probe.kt"),
    };
    let syntax_lang = detect_language(probe).ok_or_else(|| JvmIndexError::Io {
        path: String::from("<tree-sitter-registry>"),
        message: format!("language {language:?} is not detectable"),
    })?;
    let entry = registry.get(syntax_lang).ok_or_else(|| JvmIndexError::Io {
        path: String::from("<tree-sitter-registry>"),
        message: format!("language {language:?} is not registered"),
    })?;
    let ts_lang = entry
        .ts_language()
        .cloned()
        .ok_or_else(|| JvmIndexError::Io {
            path: String::from("<tree-sitter-registry>"),
            message: format!("language {language:?} has no Tree-sitter grammar"),
        })?;

    let cell = match language {
        JvmSourceLang::Java => &JAVA_PARSER,
        JvmSourceLang::Kotlin => &KOTLIN_PARSER,
    };

    cell.with(|slot| {
        let mut guard = slot.borrow_mut();
        let parser = if let Some(p) = guard.as_mut() {
            p.reset();
            p
        } else {
            let mut p = Parser::new();
            p.set_language(&ts_lang).map_err(|err| JvmIndexError::Io {
                path: String::from("<tree-sitter-parser>"),
                message: err.to_string(),
            })?;
            *guard = Some(p);
            guard.as_mut().ok_or_else(|| JvmIndexError::Io {
                path: String::from("<tree-sitter-parser>"),
                message: String::from("parser initialization failed"),
            })?
        };

        parser.parse(source, None).ok_or_else(|| JvmIndexError::Io {
            path: String::from("<tree-sitter-parser>"),
            message: String::from("tree-sitter returned no syntax tree"),
        })
    })
}

/// Parses a Java, Kotlin, or Kotlin Script source file based on its extension.
///
/// # Errors
///
/// Returns [`JvmIndexError`] if Tree-sitter fails to initialize or parse.
pub fn parse_source_file(
    rel_path: &Path,
    source: &str,
    options: &SourceScanOptions,
) -> Result<Vec<ParsedClassUnit>, JvmIndexError> {
    let ext = rel_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();
    if ext.eq_ignore_ascii_case("java") {
        parse_java_source(rel_path, source, options)
    } else if ext.eq_ignore_ascii_case("kt") || ext.eq_ignore_ascii_case("kts") {
        parse_kotlin_source(rel_path, source, options)
    } else {
        Ok(Vec::new())
    }
}

/// Parses a Java source string and extracts all type and member declarations.
///
/// # Errors
///
/// Returns [`JvmIndexError`] if Tree-sitter fails to initialize or parse.
pub fn parse_java_source(
    rel_path: &Path,
    source: &str,
    options: &SourceScanOptions,
) -> Result<Vec<ParsedClassUnit>, JvmIndexError> {
    if source.len() > options.max_source_bytes {
        return Ok(Vec::new());
    }

    let tree = parse_with_thread_local(JvmSourceLang::Java, source)?;
    let root = tree.root_node();
    let package_name = extract_java_package(root, source);

    let mut units = Vec::new();
    let mut enclosing_stack: Vec<String> = Vec::new();
    collect_java_types(
        root,
        rel_path,
        source,
        &package_name,
        &mut enclosing_stack,
        options,
        &mut units,
    );
    Ok(units)
}

/// Parses a Kotlin (`.kt`/`.kts`) source string and extracts all type and top-level declarations.
///
/// # Errors
///
/// Returns [`JvmIndexError`] if Tree-sitter fails to initialize or parse.
pub fn parse_kotlin_source(
    rel_path: &Path,
    source: &str,
    options: &SourceScanOptions,
) -> Result<Vec<ParsedClassUnit>, JvmIndexError> {
    if source.len() > options.max_source_bytes {
        return Ok(Vec::new());
    }

    let tree = parse_with_thread_local(JvmSourceLang::Kotlin, source)?;
    let root = tree.root_node();
    let package_name = extract_kotlin_package(root, source);

    let mut units = Vec::new();
    let mut enclosing_stack: Vec<String> = Vec::new();
    let mut top_level_members: Vec<JvmMemberEntry> = Vec::new();

    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if is_kotlin_type_node(child.kind()) {
            collect_kotlin_type(
                child,
                rel_path,
                source,
                &package_name,
                &mut enclosing_stack,
                options,
                &mut units,
            );
        } else if options.include_members {
            if let Some(member) = extract_kotlin_member(child, source, options) {
                top_level_members.push(member);
            }
        }
    }

    if !top_level_members.is_empty() {
        let file_kt_name = kotlin_file_facade_name(rel_path);
        let fqcn = if package_name.is_empty() {
            file_kt_name.clone()
        } else {
            format!("{package_name}.{file_kt_name}")
        };
        units.push(ParsedClassUnit {
            ty: JvmTypeEntry {
                fqcn: fqcn.clone(),
                binary_name: fqcn,
                simple_name: file_kt_name,
                package_name,
                kind: JvmSymbolKind::Class,
                modifiers: JvmModifiers {
                    visibility: JvmVisibility::Public,
                    is_final: true,
                    ..JvmModifiers::default()
                },
                origin: JvmSourceOrigin::ProjectSource {
                    rel_path: rel_path.to_path_buf(),
                    line: 0,
                    column: 0,
                },
                super_fqcn: Some(String::from("java.lang.Object")),
                interfaces: Vec::new(),
            },
            members: top_level_members,
        });
    }

    Ok(units)
}

fn node_text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    source.get(node.byte_range()).unwrap_or("")
}

fn node_position(node: Node<'_>) -> (u32, u32) {
    let pos = node.start_position();
    (
        u32::try_from(pos.row).unwrap_or(u32::MAX),
        u32::try_from(pos.column).unwrap_or(u32::MAX),
    )
}

fn build_fqcn_and_binary(package_name: &str, stack: &[String]) -> (String, String) {
    let joined_dot = stack.join(".");
    let joined_dollar = stack.join("$");
    if package_name.is_empty() {
        (joined_dot, joined_dollar)
    } else {
        (
            format!("{package_name}.{joined_dot}"),
            format!("{package_name}.{joined_dollar}"),
        )
    }
}

fn extract_java_package(root: Node<'_>, source: &str) -> String {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "package_declaration" {
            let mut inner = child.walk();
            for sub in child.children(&mut inner) {
                if matches!(sub.kind(), "scoped_identifier" | "identifier") {
                    return node_text(sub, source).trim().to_owned();
                }
            }
        }
    }
    String::new()
}

fn java_type_kind(kind: &str) -> Option<JvmSymbolKind> {
    match kind {
        "class_declaration" => Some(JvmSymbolKind::Class),
        "interface_declaration" => Some(JvmSymbolKind::Interface),
        "enum_declaration" => Some(JvmSymbolKind::Enum),
        "record_declaration" => Some(JvmSymbolKind::Record),
        "annotation_type_declaration" => Some(JvmSymbolKind::Annotation),
        _ => None,
    }
}

fn collect_java_types(
    node: Node<'_>,
    rel_path: &Path,
    source: &str,
    package_name: &str,
    enclosing_stack: &mut Vec<String>,
    options: &SourceScanOptions,
    units: &mut Vec<ParsedClassUnit>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(sym_kind) = java_type_kind(child.kind()) {
            let Some(name_node) = child.child_by_field_name("name") else {
                continue;
            };
            let simple_name = node_text(name_node, source).trim().to_owned();
            if simple_name.is_empty() {
                continue;
            }
            let (line, column) = node_position(name_node);
            let modifiers = extract_java_modifiers(child, source, JvmVisibility::PackagePrivate);
            let (superclass, interfaces) = extract_java_supertypes(child, source);

            enclosing_stack.push(simple_name.clone());
            let (fqcn, binary_name) = build_fqcn_and_binary(package_name, enclosing_stack);

            let mut members = Vec::new();
            if let Some(body) = child.child_by_field_name("body") {
                if options.include_members {
                    collect_java_members(body, source, options, &mut members);
                }
                collect_java_types(
                    body,
                    rel_path,
                    source,
                    package_name,
                    enclosing_stack,
                    options,
                    units,
                );
            }

            if sym_kind == JvmSymbolKind::Record && options.include_members {
                extract_java_record_components(child, source, &mut members);
            }

            enclosing_stack.pop();

            units.push(ParsedClassUnit {
                ty: JvmTypeEntry {
                    fqcn,
                    binary_name,
                    simple_name,
                    package_name: package_name.to_owned(),
                    kind: sym_kind,
                    modifiers,
                    origin: JvmSourceOrigin::ProjectSource {
                        rel_path: rel_path.to_path_buf(),
                        line,
                        column,
                    },
                    super_fqcn: superclass,
                    interfaces,
                },
                members,
            });
        }
    }
}

fn extract_java_modifiers(
    node: Node<'_>,
    source: &str,
    default_vis: JvmVisibility,
) -> JvmModifiers {
    let mut mods = JvmModifiers {
        visibility: default_vis,
        ..JvmModifiers::default()
    };
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "modifiers" {
            let text = node_text(child, source);
            for token in text.split_whitespace() {
                match token {
                    "public" => mods.visibility = JvmVisibility::Public,
                    "protected" => mods.visibility = JvmVisibility::Protected,
                    "private" => mods.visibility = JvmVisibility::Private,
                    "static" => mods.is_static = true,
                    "final" => mods.is_final = true,
                    "abstract" => mods.is_abstract = true,
                    "sealed" => mods.is_sealed = true,
                    _ => {}
                }
            }
        }
    }
    mods
}

fn extract_java_supertypes(node: Node<'_>, source: &str) -> (Option<String>, Vec<String>) {
    let mut superclass = None;
    let mut interfaces = Vec::new();

    if let Some(sc) = node.child_by_field_name("superclass") {
        let raw = node_text(sc, source).trim();
        let cleaned = raw.strip_prefix("extends").unwrap_or(raw).trim();
        if !cleaned.is_empty() {
            superclass = Some(cleaned.to_owned());
        }
    }
    if let Some(ifaces) = node.child_by_field_name("interfaces") {
        let raw = node_text(ifaces, source).trim();
        let cleaned = raw
            .strip_prefix("implements")
            .or_else(|| raw.strip_prefix("extends"))
            .unwrap_or(raw);
        for part in cleaned.split(',') {
            let trimmed = part.trim();
            if !trimmed.is_empty() {
                interfaces.push(trimmed.to_owned());
            }
        }
    }
    (superclass, interfaces)
}

#[allow(
    clippy::too_many_lines,
    reason = "single-pass AST member dispatch over Java declaration kinds"
)]
fn collect_java_members(
    body: Node<'_>,
    source: &str,
    options: &SourceScanOptions,
    members: &mut Vec<JvmMemberEntry>,
) {
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        match child.kind() {
            "method_declaration" => {
                let modifiers =
                    extract_java_modifiers(child, source, JvmVisibility::PackagePrivate);
                if !options.include_private_members
                    && modifiers.visibility == JvmVisibility::Private
                {
                    continue;
                }
                let Some(name_node) = child.child_by_field_name("name") else {
                    continue;
                };
                let name = node_text(name_node, source).trim().to_owned();
                let params = child
                    .child_by_field_name("parameters")
                    .map_or("()", |n| node_text(n, source).trim());
                let ret = child
                    .child_by_field_name("type")
                    .map_or("void", |n| node_text(n, source).trim());
                let (line, _) = node_position(name_node);
                members.push(JvmMemberEntry {
                    owner_fqcn: String::new(),
                    descriptor: String::new(),
                    name,
                    kind: JvmSymbolKind::Method,
                    display_signature: format!("{params} -> {ret}"),
                    modifiers,
                    line: Some(line),
                });
            }
            "constructor_declaration" => {
                let modifiers =
                    extract_java_modifiers(child, source, JvmVisibility::PackagePrivate);
                if !options.include_private_members
                    && modifiers.visibility == JvmVisibility::Private
                {
                    continue;
                }
                let Some(name_node) = child.child_by_field_name("name") else {
                    continue;
                };
                let name = node_text(name_node, source).trim().to_owned();
                let params = child
                    .child_by_field_name("parameters")
                    .map_or("()", |n| node_text(n, source).trim());
                let (line, _) = node_position(name_node);
                members.push(JvmMemberEntry {
                    owner_fqcn: String::new(),
                    descriptor: String::new(),
                    name,
                    kind: JvmSymbolKind::Constructor,
                    display_signature: params.to_owned(),
                    modifiers,
                    line: Some(line),
                });
            }
            "field_declaration" => {
                let modifiers =
                    extract_java_modifiers(child, source, JvmVisibility::PackagePrivate);
                if !options.include_private_members
                    && modifiers.visibility == JvmVisibility::Private
                {
                    continue;
                }
                let field_type = child
                    .child_by_field_name("type")
                    .map_or("", |n| node_text(n, source).trim());
                let mut inner = child.walk();
                for sub in child.children(&mut inner) {
                    if sub.kind() == "variable_declarator" {
                        if let Some(name_node) = sub.child_by_field_name("name") {
                            let (line, _) = node_position(name_node);
                            members.push(JvmMemberEntry {
                                owner_fqcn: String::new(),
                                descriptor: String::new(),
                                name: node_text(name_node, source).trim().to_owned(),
                                kind: JvmSymbolKind::Field,
                                display_signature: field_type.to_owned(),
                                modifiers,
                                line: Some(line),
                            });
                        }
                    }
                }
            }
            "enum_body_declarations" => {
                collect_java_members(child, source, options, members);
            }
            "enum_constant" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let (line, _) = node_position(name_node);
                    members.push(JvmMemberEntry {
                        owner_fqcn: String::new(),
                        descriptor: String::new(),
                        name: node_text(name_node, source).trim().to_owned(),
                        kind: JvmSymbolKind::Field,
                        display_signature: String::from("enum constant"),
                        modifiers: JvmModifiers {
                            visibility: JvmVisibility::Public,
                            is_static: true,
                            is_final: true,
                            ..JvmModifiers::default()
                        },
                        line: Some(line),
                    });
                }
            }
            _ => {}
        }
    }
}

fn extract_java_record_components(
    record_node: Node<'_>,
    source: &str,
    members: &mut Vec<JvmMemberEntry>,
) {
    let Some(params) = record_node.child_by_field_name("parameters") else {
        return;
    };
    let mut cursor = params.walk();
    for child in params.children(&mut cursor) {
        if child.kind() == "formal_parameter" {
            if let Some(name_node) = child.child_by_field_name("name") {
                let ty = child
                    .child_by_field_name("type")
                    .map_or("", |n| node_text(n, source).trim());
                let (line, _) = node_position(name_node);
                members.push(JvmMemberEntry {
                    owner_fqcn: String::new(),
                    descriptor: String::new(),
                    name: node_text(name_node, source).trim().to_owned(),
                    kind: JvmSymbolKind::Field,
                    display_signature: ty.to_owned(),
                    modifiers: JvmModifiers {
                        visibility: JvmVisibility::Private,
                        is_final: true,
                        ..JvmModifiers::default()
                    },
                    line: Some(line),
                });
            }
        }
    }
}

fn extract_kotlin_package(root: Node<'_>, source: &str) -> String {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "package_header" {
            let text = node_text(child, source).trim();
            return text
                .strip_prefix("package")
                .unwrap_or(text)
                .trim()
                .trim_end_matches(';')
                .trim()
                .to_owned();
        }
    }
    String::new()
}

fn kotlin_file_facade_name(rel_path: &Path) -> String {
    let stem = rel_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("File");
    let mut chars = stem.chars();
    match chars.next() {
        Some(first) => format!("{}{}Kt", first.to_uppercase(), chars.as_str()),
        None => String::from("FileKt"),
    }
}

fn is_kotlin_type_node(kind: &str) -> bool {
    matches!(
        kind,
        "class_declaration" | "object_declaration" | "companion_object"
    )
}

fn collect_kotlin_type(
    node: Node<'_>,
    rel_path: &Path,
    source: &str,
    package_name: &str,
    enclosing_stack: &mut Vec<String>,
    options: &SourceScanOptions,
    units: &mut Vec<ParsedClassUnit>,
) {
    let (simple_name, name_node) = if node.kind() == "companion_object" {
        let explicit = find_kotlin_type_name(node, source);
        match explicit {
            Some((name, n)) => (name, n),
            None => (String::from("Companion"), node),
        }
    } else {
        match find_kotlin_type_name(node, source) {
            Some(pair) => pair,
            None => return,
        }
    };

    if simple_name.is_empty() {
        return;
    }

    let modifiers = extract_kotlin_modifiers(node, source);
    let kind = classify_kotlin_type_kind(node, source);
    let (line, column) = node_position(name_node);

    enclosing_stack.push(simple_name.clone());
    let (fqcn, binary_name) = build_fqcn_and_binary(package_name, enclosing_stack);

    let mut members = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if matches!(child.kind(), "class_body" | "enum_class_body") {
            let mut body_cursor = child.walk();
            for body_item in child.children(&mut body_cursor) {
                if is_kotlin_type_node(body_item.kind()) {
                    collect_kotlin_type(
                        body_item,
                        rel_path,
                        source,
                        package_name,
                        enclosing_stack,
                        options,
                        units,
                    );
                } else if options.include_members {
                    if let Some(member) = extract_kotlin_member(body_item, source, options) {
                        members.push(member);
                    }
                }
            }
        }
    }

    enclosing_stack.pop();

    units.push(ParsedClassUnit {
        ty: JvmTypeEntry {
            fqcn,
            binary_name,
            simple_name,
            package_name: package_name.to_owned(),
            kind,
            modifiers,
            origin: JvmSourceOrigin::ProjectSource {
                rel_path: rel_path.to_path_buf(),
                line,
                column,
            },
            super_fqcn: None,
            interfaces: Vec::new(),
        },
        members,
    });
}

fn find_kotlin_type_name<'a>(node: Node<'a>, source: &str) -> Option<(String, Node<'a>)> {
    if let Some(n) = node.child_by_field_name("name") {
        return Some((node_text(n, source).trim().to_owned(), n));
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if matches!(child.kind(), "type_identifier" | "simple_identifier") {
            return Some((node_text(child, source).trim().to_owned(), child));
        }
    }
    None
}

fn classify_kotlin_type_kind(node: Node<'_>, source: &str) -> JvmSymbolKind {
    if matches!(node.kind(), "object_declaration" | "companion_object") {
        return JvmSymbolKind::Object;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let text = node_text(child, source);
        if child.kind() == "interface" || text == "interface" {
            return JvmSymbolKind::Interface;
        }
        if child.kind() == "modifiers" {
            for token in text.split_whitespace() {
                if token == "enum" {
                    return JvmSymbolKind::Enum;
                }
                if token == "annotation" {
                    return JvmSymbolKind::Annotation;
                }
            }
        }
    }
    JvmSymbolKind::Class
}

fn extract_kotlin_modifiers(node: Node<'_>, source: &str) -> JvmModifiers {
    let mut mods = JvmModifiers {
        visibility: JvmVisibility::Public,
        ..JvmModifiers::default()
    };
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "modifiers" {
            for token in node_text(child, source).split_whitespace() {
                match token {
                    "private" => mods.visibility = JvmVisibility::Private,
                    "protected" => mods.visibility = JvmVisibility::Protected,
                    "internal" => mods.visibility = JvmVisibility::Internal,
                    "public" => mods.visibility = JvmVisibility::Public,
                    "abstract" => mods.is_abstract = true,
                    "sealed" => mods.is_sealed = true,
                    _ => {}
                }
            }
        }
    }
    mods
}

fn extract_kotlin_member(
    node: Node<'_>,
    source: &str,
    options: &SourceScanOptions,
) -> Option<JvmMemberEntry> {
    let modifiers = extract_kotlin_modifiers(node, source);
    if !options.include_private_members && modifiers.visibility == JvmVisibility::Private {
        return None;
    }

    match node.kind() {
        "function_declaration" => {
            let (name, name_node) = find_kotlin_type_name(node, source)?;
            let (line, _) = node_position(name_node);
            Some(JvmMemberEntry {
                owner_fqcn: String::new(),
                descriptor: String::new(),
                name,
                kind: JvmSymbolKind::Method,
                display_signature: String::from("()"),
                modifiers,
                line: Some(line),
            })
        }
        "property_declaration" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "variable_declaration" {
                    if let Some((name, name_node)) = find_kotlin_type_name(child, source) {
                        let (line, _) = node_position(name_node);
                        return Some(JvmMemberEntry {
                            owner_fqcn: String::new(),
                            descriptor: String::new(),
                            name,
                            kind: JvmSymbolKind::Field,
                            display_signature: String::new(),
                            modifiers,
                            line: Some(line),
                        });
                    }
                }
            }
            None
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn parses_java_nested_record_enum_and_ignores_syntax_errors() {
        let src = r"
package com.example;

public class Outer {
    private int count;
    public Outer(int count) { this.count = count; }
    public static class Inner {
        public String greet(String name) { return name; }
    }
}

public record UserDto(String id, int age) {}

public enum Status { ACTIVE, INACTIVE }

public class Broken {
    public void ok() {}
    public void broken( {
}
";
        let units = parse_java_source(
            Path::new("src/Outer.java"),
            src,
            &SourceScanOptions::default(),
        )
        .expect("java parse should succeed");

        let fqcns: Vec<&str> = units.iter().map(|u| u.ty.fqcn.as_str()).collect();
        assert!(fqcns.contains(&"com.example.Outer"));
        assert!(fqcns.contains(&"com.example.Outer.Inner"));
        assert!(fqcns.contains(&"com.example.UserDto"));
        assert!(fqcns.contains(&"com.example.Status"));
        assert!(fqcns.contains(&"com.example.Broken"));

        let inner = units
            .iter()
            .find(|u| u.ty.fqcn == "com.example.Outer.Inner")
            .expect("Inner must exist");
        assert_eq!(inner.ty.binary_name, "com.example.Outer$Inner");
        assert!(matches!(
            inner.ty.origin,
            JvmSourceOrigin::ProjectSource { .. }
        ));
    }

    #[test]
    fn parses_kotlin_classes_objects_and_top_level_functions() {
        let src = r"
package dev.kova

data class Person(val name: String) {
    fun hello() {}
    companion object {
        val VERSION = 1
    }
}

object Singleton {
    internal fun ping() {}
}

fun topLevelHelper() {}
";
        let units = parse_kotlin_source(
            Path::new("src/Person.kt"),
            src,
            &SourceScanOptions::default(),
        )
        .expect("kotlin parse should succeed");

        let fqcns: Vec<&str> = units.iter().map(|u| u.ty.fqcn.as_str()).collect();
        assert!(fqcns.contains(&"dev.kova.Person"));
        assert!(fqcns.contains(&"dev.kova.Person.Companion"));
        assert!(fqcns.contains(&"dev.kova.Singleton"));
        assert!(fqcns.contains(&"dev.kova.PersonKt"));
    }
}
