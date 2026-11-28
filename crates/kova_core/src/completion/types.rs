//! Type-name completion for the `IntelliJ`-style as-you-type popup (`KOVA-074`).
//!
//! - [`TypeCandidate`] is a class / interface / enum / record found in the JDK, the project, or a
//!   dependency `.jar`, identified by its FQCN.
//! - [`build_type_proposals`] does prefix / `CamelHumps` filtering, FQCN de-duplication, capping,
//!   and attaches automatic `import` edits.
//! - [`import_edit_for_type`] computes the `import pkg.Type;` edit (skipping `java.lang`, the same
//!   package, existing explicit / wildcard imports and simple-name conflicts).
//! - [`compose_completion_edits`] turns "replacement + additional edits" into buffer edits and the
//!   resulting caret, shared by the editor and headless tests.
//!
//! Pure functions over strings and [`BufferSnapshot`]; no GPUI or `tokio`.

use std::collections::HashSet;
use std::hash::BuildHasher;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};

use kova_buffer::{BufferSnapshot, TextEdit};

use super::{
    CompletionContext, CompletionProposal, CompletionProposalKind, CompletionSource,
    CompletionTextEdit, score_completion_match,
};
use crate::navigation::SourceLanguageKind;
use crate::navigation::resolver::{ImportEntry, parse_package_and_imports};

/// Maximum number of type proposals returned for one request; more matches mark the result as
/// incomplete so the UI re-queries on the next keystroke.
pub const TYPE_COMPLETION_LIMIT: usize = 150;

/// Minimum match score accepted for type names (case-insensitive prefix or `CamelHumps`).
const TYPE_MATCH_MIN_SCORE: u32 = 100_000;

/// A completable type discovered in the JDK, the project, or a dependency archive.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TypeCandidate {
    /// Simple name (`Objects`, `ObjIntConsumer`).
    pub simple_name: String,
    /// Canonical fully-qualified name (`java.util.Objects`).
    pub fqcn: String,
    /// Type kind (`Class`, `Interface`, `Enum`, `Record`, `Annotation`).
    pub kind: CompletionProposalKind,
    /// Generic parameter names shown after the name (`"<T>"`).
    pub type_params: Option<String>,
    /// Where the type was found.
    pub source: CompletionSource,
    /// Whether the type is `@Deprecated`.
    pub is_deprecated: bool,
}

impl TypeCandidate {
    /// Creates a non-deprecated candidate from its FQCN (`simple_name` is the last segment).
    #[must_use]
    pub fn new(fqcn: &str, kind: CompletionProposalKind, source: CompletionSource) -> Self {
        let simple_name = fqcn.rsplit('.').next().unwrap_or(fqcn).to_owned();
        Self {
            simple_name,
            fqcn: fqcn.to_owned(),
            kind,
            type_params: None,
            source,
            is_deprecated: false,
        }
    }

    /// Package of the type (`""` for the default package).
    #[must_use]
    pub fn package(&self) -> &str {
        self.fqcn
            .strip_suffix(self.simple_name.as_str())
            .map_or("", |p| p.trim_end_matches('.'))
    }
}

/// Returns `true` when `simple_name` matches the typed `prefix` by case-insensitive prefix or
/// `CamelHumps` (`OIC` → `ObjIntConsumer`); the first character must always match.
#[must_use]
pub fn type_name_matches(simple_name: &str, prefix: &str) -> bool {
    let (Some(first_name), Some(first_prefix)) =
        (simple_name.chars().next(), prefix.chars().next())
    else {
        return false;
    };
    if !first_name.eq_ignore_ascii_case(&first_prefix) {
        return false;
    }
    score_completion_match(simple_name, prefix).is_some_and(|(s, _)| s >= TYPE_MATCH_MIN_SCORE)
}

/// Filters `candidates` against `ctx.prefix`, skips FQCNs already in `seen_fqcns`, caps the result at
/// `limit` (best matches first) and attaches auto-import edits.
///
/// Returns `(proposals, truncated)`.
#[must_use]
pub fn build_type_proposals<S: BuildHasher>(
    ctx: &CompletionContext,
    source_text: &str,
    candidates: impl IntoIterator<Item = TypeCandidate>,
    seen_fqcns: &mut HashSet<String, S>,
    limit: usize,
) -> (Vec<CompletionProposal>, bool) {
    if ctx.prefix.is_empty() || ctx.separator.is_some() {
        return (Vec::new(), false);
    }
    let mut matched: Vec<(u32, TypeCandidate)> = Vec::new();
    for cand in candidates {
        if !type_name_matches(&cand.simple_name, &ctx.prefix) || seen_fqcns.contains(&cand.fqcn) {
            continue;
        }
        let Some((score, _)) = score_completion_match(&cand.simple_name, &ctx.prefix) else {
            continue;
        };
        seen_fqcns.insert(cand.fqcn.clone());
        matched.push((score, cand));
    }
    matched.sort_by(|(sa, a), (sb, b)| {
        sb.cmp(sa)
            .then_with(|| a.simple_name.len().cmp(&b.simple_name.len()))
            .then_with(|| a.fqcn.cmp(&b.fqcn))
    });
    let truncated = matched.len() > limit;
    matched.truncate(limit);

    let proposals = matched
        .into_iter()
        .map(|(_, cand)| type_candidate_proposal(ctx, source_text, cand))
        .collect();
    (proposals, truncated)
}

fn type_candidate_proposal(
    ctx: &CompletionContext,
    source_text: &str,
    cand: TypeCandidate,
) -> CompletionProposal {
    let package = cand.package().to_owned();
    let conflict = has_conflicting_import(&ctx.imports, &cand.simple_name, &cand.fqcn);
    let needs_import = type_requires_import(
        ctx.language,
        ctx.package_name.as_deref(),
        &ctx.imports,
        &cand.fqcn,
    );
    let additional_text_edits = if needs_import {
        vec![import_edit_for_text(source_text, ctx.language, &cand.fqcn)]
    } else {
        Vec::new()
    };
    // A different type with the same simple name is already imported: insert the FQCN instead.
    let insert_text = if conflict && !package.is_empty() {
        cand.fqcn.clone()
    } else {
        cand.simple_name.clone()
    };
    let sort_priority = if needs_import || conflict { 4 } else { 2 };
    CompletionProposal {
        label: cand.simple_name,
        kind: cand.kind,
        params_signature: None,
        return_type: None,
        detail_right: (!package.is_empty()).then_some(package),
        insert_text,
        is_callable: false,
        has_params: false,
        is_method_reference: false,
        additional_text_edits,
        source: cand.source,
        is_deprecated: cand.is_deprecated,
        sort_priority,
        is_static: false,
        type_params: cand.type_params,
        fqcn: Some(cand.fqcn),
    }
}

/// Kotlin packages imported by default (no explicit `import` needed).
const KOTLIN_DEFAULT_PACKAGES: &[&str] = &[
    "kotlin",
    "kotlin.annotation",
    "kotlin.collections",
    "kotlin.comparisons",
    "kotlin.io",
    "kotlin.ranges",
    "kotlin.sequences",
    "kotlin.text",
    "java.lang",
];

/// Returns `true` when referencing `fqcn` by its simple name requires a new `import` line.
#[must_use]
pub fn type_requires_import(
    lang: SourceLanguageKind,
    package_name: Option<&str>,
    imports: &[ImportEntry],
    fqcn: &str,
) -> bool {
    if !matches!(lang, SourceLanguageKind::Java | SourceLanguageKind::Kotlin) {
        return false;
    }
    let Some((pkg, simple)) = fqcn.rsplit_once('.') else {
        return false;
    };
    if pkg == "java.lang" || package_name == Some(pkg) {
        return false;
    }
    if lang == SourceLanguageKind::Kotlin && KOTLIN_DEFAULT_PACKAGES.contains(&pkg) {
        return false;
    }
    let already = imports.iter().any(|imp| {
        !imp.is_static
            && (imp.path == fqcn
                || (imp.is_wildcard
                    && imp.path.trim_end_matches(".*").trim_end_matches('*') == pkg))
    });
    !already && !has_conflicting_import(imports, simple, fqcn)
}

fn has_conflicting_import(imports: &[ImportEntry], simple: &str, fqcn: &str) -> bool {
    imports
        .iter()
        .any(|imp| !imp.is_wildcard && imp.local_name() == Some(simple) && imp.path != fqcn)
}

/// Computes the auto-import edit for `fqcn` in `source_text`, or `None` when no import is needed.
#[must_use]
pub fn import_edit_for_type(
    source_text: &str,
    lang: SourceLanguageKind,
    fqcn: &str,
) -> Option<CompletionTextEdit> {
    let (package_name, imports) = parse_package_and_imports(source_text, lang);
    if !type_requires_import(lang, package_name.as_deref(), &imports, fqcn) {
        return None;
    }
    Some(import_edit_for_text(source_text, lang, fqcn))
}

/// Builds the `import` insertion edit without checking whether it is needed.
fn import_edit_for_text(
    source_text: &str,
    lang: SourceLanguageKind,
    fqcn: &str,
) -> CompletionTextEdit {
    let semi = if lang == SourceLanguageKind::Java {
        ";"
    } else {
        ""
    };
    let stmt = format!("import {fqcn}{semi}");

    let mut last_import: Option<usize> = None;
    let mut package_line: Option<usize> = None;
    let mut lines: Vec<&str> = Vec::new();
    let mut in_block_comment = false;
    for (idx, raw) in source_text.split('\n').enumerate() {
        lines.push(raw);
        let line = raw.trim();
        if in_block_comment {
            if line.contains("*/") {
                in_block_comment = false;
            }
            continue;
        }
        if line.starts_with("/*") {
            in_block_comment = !line.contains("*/");
            continue;
        }
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        if line.starts_with("package ") {
            package_line = Some(idx);
        } else if line.starts_with("import ") {
            last_import = Some(idx);
        } else {
            // First declaration after the header: imports can no longer follow.
            break;
        }
    }

    let edit_at = |line: usize, col: usize, text: String| CompletionTextEdit {
        start_utf16_line: line,
        start_utf16_col: col,
        end_utf16_line: line,
        end_utf16_col: col,
        new_text: text,
    };

    let total_lines = source_text.split('\n').count();
    let after_line = |line: usize, text: &str, prefix_blank: bool| {
        let blank = if prefix_blank { "\n" } else { "" };
        if line + 1 < total_lines {
            edit_at(line + 1, 0, format!("{blank}{text}\n"))
        } else {
            // Header line is the last line without a trailing newline: append to its end.
            let col = lines
                .get(line)
                .map_or(0, |l| l.trim_end_matches('\r').encode_utf16().count());
            edit_at(line, col, format!("\n{blank}{text}"))
        }
    };

    match (last_import, package_line) {
        (Some(line), _) => after_line(line, &stmt, false),
        (None, Some(line)) => after_line(line, &stmt, true),
        (None, None) => edit_at(0, 0, format!("{stmt}\n\n")),
    }
}

/// Derives candidates for project types from workspace files and open buffers.
///
/// The package comes from the open buffer's `package` declaration, else from the path segment
/// below a `java` / `kotlin` source root (`src/main/java/com/acme/Foo.java` → `com.acme`); the
/// kind comes from the declaration when the buffer text is available.
#[must_use]
pub fn project_type_candidates(
    lang: SourceLanguageKind,
    open_buffers: &[(PathBuf, String)],
    workspace_files: &[PathBuf],
) -> Vec<TypeCandidate> {
    if !matches!(lang, SourceLanguageKind::Java | SourceLanguageKind::Kotlin) {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let open_paths: HashSet<&Path> = open_buffers.iter().map(|(p, _)| p.as_path()).collect();

    for (path, text) in open_buffers {
        let Some(file_lang) = jvm_lang_of(path) else {
            continue;
        };
        let (pkg, _) = parse_package_and_imports(text, file_lang);
        let pkg = pkg.or_else(|| package_from_path(path)).unwrap_or_default();
        for (name, kind) in declared_top_level_types(text) {
            let fqcn = qualify(&pkg, &name);
            if seen.insert(fqcn.clone()) {
                out.push(TypeCandidate::new(&fqcn, kind, CompletionSource::Workspace));
            }
        }
    }
    for path in workspace_files {
        if open_paths.contains(path.as_path()) || jvm_lang_of(path).is_none() {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !stem.chars().next().is_some_and(char::is_uppercase) {
            continue;
        }
        let pkg = package_from_path(path).unwrap_or_default();
        let fqcn = qualify(&pkg, stem);
        if seen.insert(fqcn.clone()) {
            out.push(TypeCandidate::new(
                &fqcn,
                CompletionProposalKind::Class,
                CompletionSource::Workspace,
            ));
        }
    }
    out
}

fn qualify(pkg: &str, name: &str) -> String {
    if pkg.is_empty() {
        name.to_owned()
    } else {
        format!("{pkg}.{name}")
    }
}

fn jvm_lang_of(path: &Path) -> Option<SourceLanguageKind> {
    match SourceLanguageKind::from_path(path) {
        lang @ (SourceLanguageKind::Java | SourceLanguageKind::Kotlin) => Some(lang),
        _ => None,
    }
}

/// `src/main/java/com/acme/Foo.java` → `Some("com.acme")`.
fn package_from_path(path: &Path) -> Option<String> {
    let parts: Vec<&str> = path
        .parent()?
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();
    let root_idx = parts
        .iter()
        .rposition(|p| matches!(*p, "java" | "kotlin" | "groovy"))?;
    Some(parts[root_idx + 1..].join("."))
}

/// Top-level (column-0 or modifier-prefixed) type declarations in `text`.
fn declared_top_level_types(text: &str) -> Vec<(String, CompletionProposalKind)> {
    let mut out = Vec::new();
    for raw in text.lines() {
        if raw.starts_with(char::is_whitespace) {
            continue;
        }
        let mut words = raw
            .split(|c: char| c.is_whitespace() || c == '{' || c == '<' || c == '(' || c == ':')
            .filter(|w| !w.is_empty())
            .peekable();
        while let Some(word) = words.next() {
            let kind = match word {
                "class" | "object" => CompletionProposalKind::Class,
                "interface" => CompletionProposalKind::Interface,
                "@interface" => CompletionProposalKind::Annotation,
                "enum" => CompletionProposalKind::Enum,
                "record" => CompletionProposalKind::Record,
                _ => continue,
            };
            // Kotlin `enum class` / `annotation class` / `data class`.
            let (kind, name) = match words.next() {
                Some("class") if kind == CompletionProposalKind::Enum => {
                    (kind, words.next().unwrap_or_default())
                }
                Some(n) => (kind, n),
                None => break,
            };
            if name.chars().next().is_some_and(char::is_alphabetic) {
                out.push((name.to_owned(), kind));
            }
            break;
        }
    }
    out
}

/// Buffer edits and resulting caret for one accepted completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposedCompletionEdit {
    /// Disjoint edits sorted by start offset (pre-edit byte ranges).
    pub edits: Vec<TextEdit>,
    /// Caret byte offset after all edits are applied.
    pub caret_after: usize,
}

/// Composes the primary replacement (`replace_range` → `insert_text`) with non-overlapping
/// `additional` edits (e.g. auto-import) and computes where the caret lands
/// (`cursor_offset_in_text` inside the inserted text, shifted by edits placed before it).
#[must_use]
pub fn compose_completion_edits(
    snap: &BufferSnapshot,
    replace_range: Range<usize>,
    insert_text: &str,
    cursor_offset_in_text: usize,
    additional: &[CompletionTextEdit],
) -> ComposedCompletionEdit {
    let len = snap.len_bytes();
    let primary = snap.clamp_byte_to_char_boundary(replace_range.start.min(len))
        ..snap.clamp_byte_to_char_boundary(replace_range.end.min(len));

    let mut extra: Vec<TextEdit> = Vec::new();
    for edit in additional {
        let start = utf16_line_col_to_byte(snap, edit.start_utf16_line, edit.start_utf16_col);
        let end = utf16_line_col_to_byte(snap, edit.end_utf16_line, edit.end_utf16_col);
        let range = start.min(end)..start.max(end);
        let overlaps = range.start < primary.end && range.end > primary.start;
        let touches_empty_primary =
            primary.is_empty() && range.start <= primary.start && range.end >= primary.start;
        if overlaps || touches_empty_primary {
            continue;
        }
        extra.push(TextEdit::new(range, edit.new_text.as_str()));
    }
    extra.sort_by_key(|e| (e.range.start, e.range.end));
    let mut disjoint: Vec<TextEdit> = Vec::with_capacity(extra.len());
    for edit in extra {
        if disjoint
            .last()
            .is_some_and(|prev| edit.range.start < prev.range.end)
        {
            continue;
        }
        disjoint.push(edit);
    }

    let mut delta_before: isize = 0;
    for edit in &disjoint {
        if edit.range.end <= primary.start {
            let new_len = isize::try_from(edit.replacement.len()).unwrap_or(0);
            let old_len = isize::try_from(edit.range.len()).unwrap_or(0);
            delta_before += new_len - old_len;
        }
    }

    let mut edits = disjoint;
    edits.push(TextEdit::new(primary.clone(), insert_text));
    edits.sort_by_key(|e| (e.range.start, e.range.end));
    let caret_after = primary
        .start
        .saturating_add_signed(delta_before)
        .saturating_add(cursor_offset_in_text.min(insert_text.len()));
    ComposedCompletionEdit { edits, caret_after }
}

/// Converts a 0-based `(line, utf16 column)` into a byte offset in `snap` (clamped to the line).
#[must_use]
pub fn utf16_line_col_to_byte(snap: &BufferSnapshot, line: usize, col_utf16: usize) -> usize {
    let total_lines = snap.len_lines().max(1);
    let clamped_line = line.min(total_lines.saturating_sub(1));
    let line_start_byte = snap.rope().line_to_byte(clamped_line);
    let line_len_bytes = snap.line_len_bytes(clamped_line).unwrap_or(0);
    let line_start_utf16 = snap.byte_to_utf16(line_start_byte).unwrap_or(0);
    let line_end_utf16 = snap
        .byte_to_utf16(line_start_byte + line_len_bytes)
        .unwrap_or(line_start_utf16);
    let target_utf16 = (line_start_utf16 + col_utf16).min(line_end_utf16);
    snap.utf16_to_byte(target_utf16).unwrap_or(line_start_byte)
}

#[cfg(test)]
mod tests {
    use kova_buffer::Buffer;

    use super::*;
    use crate::completion::resolver::resolve_local_completion;
    use crate::completion::{
        CompletionTriggerKind, extract_completion_context, should_auto_trigger_completion,
        should_auto_trigger_identifier_completion,
    };

    fn java() -> PathBuf {
        PathBuf::from("src/main/java/demo/App.java")
    }

    fn apply(text: &str, edit: &CompletionTextEdit) -> String {
        let buf = Buffer::from_text(text);
        let snap = buf.snapshot();
        let start = utf16_line_col_to_byte(&snap, edit.start_utf16_line, edit.start_utf16_col);
        let end = utf16_line_col_to_byte(&snap, edit.end_utf16_line, edit.end_utf16_col);
        let mut buf = buf;
        assert!(
            buf.apply_edits(&[TextEdit::new(start..end, edit.new_text.as_str())])
                .is_ok()
        );
        buf.snapshot().text()
    }

    #[test]
    fn type_name_matching_uses_prefix_and_camel_humps_only() {
        assert!(type_name_matches("Objects", "Obj"));
        assert!(type_name_matches("Object", "obj"));
        assert!(type_name_matches("ObjIntConsumer", "OIC"));
        assert!(type_name_matches("ObjIntConsumer", "ObjInt"));
        assert!(!type_name_matches("Objects", "bj"));
        assert!(!type_name_matches("Objects", "Ojs"));
        assert!(!type_name_matches("Objects", ""));
    }

    #[test]
    fn import_edit_is_placed_after_imports_package_or_at_top() {
        let with_imports = "package demo;\n\nimport java.util.List;\n\nclass App {}\n";
        let edit =
            import_edit_for_type(with_imports, SourceLanguageKind::Java, "java.util.Objects")
                .unwrap();
        assert_eq!(
            apply(with_imports, &edit),
            "package demo;\n\nimport java.util.List;\nimport java.util.Objects;\n\nclass App {}\n"
        );

        let only_package = "package demo;\n\nclass App {}\n";
        let edit =
            import_edit_for_type(only_package, SourceLanguageKind::Java, "java.util.Objects")
                .unwrap();
        assert_eq!(
            apply(only_package, &edit),
            "package demo;\n\nimport java.util.Objects;\n\nclass App {}\n"
        );

        let bare = "class App {}\n";
        let edit =
            import_edit_for_type(bare, SourceLanguageKind::Java, "java.util.Objects").unwrap();
        assert_eq!(
            apply(bare, &edit),
            "import java.util.Objects;\n\nclass App {}\n"
        );

        let kotlin = "package demo\n\nimport kotlin.math.max\n\nfun main() {}\n";
        let edit =
            import_edit_for_type(kotlin, SourceLanguageKind::Kotlin, "java.util.Objects").unwrap();
        assert_eq!(
            apply(kotlin, &edit),
            "package demo\n\nimport kotlin.math.max\nimport java.util.Objects\n\nfun main() {}\n"
        );

        let no_trailing_newline = "package demo;";
        let edit = import_edit_for_type(
            no_trailing_newline,
            SourceLanguageKind::Java,
            "java.util.Objects",
        )
        .unwrap();
        assert_eq!(
            apply(no_trailing_newline, &edit),
            "package demo;\n\nimport java.util.Objects;"
        );
    }

    #[test]
    fn import_is_skipped_for_java_lang_same_package_existing_and_conflicting_imports() {
        let src = "package demo;\n\nimport java.util.*;\nimport com.acme.Objects;\nimport java.time.Instant;\n";
        let lang = SourceLanguageKind::Java;
        assert!(import_edit_for_type(src, lang, "java.lang.Object").is_none());
        assert!(import_edit_for_type(src, lang, "demo.OrderService").is_none());
        assert!(import_edit_for_type(src, lang, "java.time.Instant").is_none());
        assert!(import_edit_for_type(src, lang, "java.util.Optional").is_none());
        // `Objects` already refers to `com.acme.Objects`.
        assert!(import_edit_for_type(src, lang, "java.util.function.Objects").is_none());
        assert!(import_edit_for_type(src, lang, "java.io.File").is_some());
        assert!(import_edit_for_type(src, SourceLanguageKind::Rust, "java.io.File").is_none());
    }

    #[test]
    fn compose_edits_shifts_caret_past_import_inserted_above() {
        let text = "package demo;\n\nclass App {\n    Obj\n}\n";
        let snap = Buffer::from_text(text).snapshot();
        let start = text.find("Obj\n").unwrap();
        let import =
            import_edit_for_type(text, SourceLanguageKind::Java, "java.util.Objects").unwrap();
        let composed = compose_completion_edits(
            &snap,
            start..start + 3,
            "Objects",
            "Objects".len(),
            &[import],
        );
        let mut buf = Buffer::from_text(text);
        assert!(buf.apply_edits(&composed.edits).is_ok());
        let after = buf.snapshot().text();
        assert_eq!(
            after,
            "package demo;\n\nimport java.util.Objects;\n\nclass App {\n    Objects\n}\n"
        );
        assert_eq!(
            composed.caret_after,
            after.find("Objects\n}").unwrap() + "Objects".len()
        );
    }

    #[test]
    fn build_type_proposals_dedupes_by_fqcn_caps_and_inserts_fqcn_on_conflict() {
        let src = "import com.acme.Objects;\nclass App { void f() { Obj } }\n";
        let snap = Buffer::from_text(src).snapshot();
        let pos = src.find("Obj }").unwrap() + 3;
        let ctx =
            extract_completion_context(&snap, &java(), pos, CompletionTriggerKind::AutoIdentifier)
                .unwrap();
        let cands = vec![
            TypeCandidate::new(
                "java.util.Objects",
                CompletionProposalKind::Class,
                CompletionSource::JdkBuiltin,
            ),
            TypeCandidate::new(
                "java.util.Objects",
                CompletionProposalKind::Class,
                CompletionSource::JdkBytecode,
            ),
            TypeCandidate::new(
                "java.lang.Object",
                CompletionProposalKind::Class,
                CompletionSource::JdkBuiltin,
            ),
            TypeCandidate::new(
                "java.io.ObjectInput",
                CompletionProposalKind::Interface,
                CompletionSource::JdkBuiltin,
            ),
            TypeCandidate::new(
                "java.util.List",
                CompletionProposalKind::Interface,
                CompletionSource::JdkBuiltin,
            ),
        ];
        let mut seen = HashSet::new();
        let (props, truncated) = build_type_proposals(&ctx, src, cands.clone(), &mut seen, 10);
        assert!(!truncated);
        let labels: Vec<&str> = props.iter().map(|p| p.label.as_str()).collect();
        assert_eq!(labels, vec!["Object", "Objects", "ObjectInput"]);
        let objects = &props[1];
        assert_eq!(objects.insert_text, "java.util.Objects");
        assert_eq!(objects.additional_text_edits, vec![]);
        assert_eq!(props[0].additional_text_edits, vec![]);
        assert_eq!(props[2].additional_text_edits.len(), 1);
        assert_eq!(props[2].kind, CompletionProposalKind::Interface);
        assert_eq!(props[2].inline_detail().as_deref(), Some("java.io"));

        let mut seen2 = HashSet::new();
        let (capped, truncated) = build_type_proposals(&ctx, src, cands, &mut seen2, 2);
        assert!(truncated);
        assert_eq!(capped.len(), 2);
    }

    #[test]
    fn project_types_take_package_from_declaration_or_source_root_path() {
        let buffers = vec![(
            PathBuf::from("src/main/java/com/acme/api/OrderPort.java"),
            "package com.acme.api;\n\npublic interface OrderPort {}\n".to_owned(),
        )];
        let files = vec![
            PathBuf::from("src/main/java/com/acme/domain/Order.java"),
            PathBuf::from("src/main/kotlin/com/acme/util/Strings.kt"),
            PathBuf::from("src/main/resources/application.yml"),
            PathBuf::from("src/main/java/com/acme/api/OrderPort.java"),
        ];
        let cands = project_type_candidates(SourceLanguageKind::Java, &buffers, &files);
        let by_fqcn: Vec<(&str, CompletionProposalKind)> =
            cands.iter().map(|c| (c.fqcn.as_str(), c.kind)).collect();
        assert_eq!(
            by_fqcn,
            vec![
                ("com.acme.api.OrderPort", CompletionProposalKind::Interface),
                ("com.acme.domain.Order", CompletionProposalKind::Class),
                ("com.acme.util.Strings", CompletionProposalKind::Class),
            ]
        );
        assert_eq!(cands[1].package(), "com.acme.domain");
    }

    #[test]
    fn identifier_auto_trigger_needs_two_chars_outside_comments_strings_and_imports() {
        let src = "\
package demo;
import java.util.Li
// Obj
class App {
    String s = \"Obj\";
    void f() {
        O
        Obj
        3x
        Objects.re
        Ob1
        Obj x
    }
}
";
        let snap = Buffer::from_text(src).snapshot();
        let lang = SourceLanguageKind::Java;
        let end_of = |needle: &str| src.find(needle).unwrap() + needle.len();
        assert!(should_auto_trigger_identifier_completion(
            &snap,
            lang,
            end_of("        Obj\n") - 1
        ));
        assert!(should_auto_trigger_identifier_completion(
            &snap,
            lang,
            end_of("Objects.re")
        ));
        assert!(should_auto_trigger_identifier_completion(
            &snap,
            lang,
            end_of("Ob1")
        ));
        assert!(!should_auto_trigger_identifier_completion(
            &snap,
            lang,
            end_of("        O\n") - 1
        ));
        assert!(!should_auto_trigger_identifier_completion(
            &snap,
            lang,
            end_of("3x")
        ));
        assert!(!should_auto_trigger_identifier_completion(
            &snap,
            lang,
            end_of("// Obj")
        ));
        assert!(!should_auto_trigger_identifier_completion(
            &snap,
            lang,
            end_of("\"Obj")
        ));
        assert!(!should_auto_trigger_identifier_completion(
            &snap,
            lang,
            end_of("java.util.Li")
        ));
        // Caret in the middle of a word.
        assert!(!should_auto_trigger_identifier_completion(
            &snap,
            lang,
            end_of("        Ob")
        ));
        assert!(!should_auto_trigger_identifier_completion(
            &snap,
            SourceLanguageKind::Unknown,
            end_of("Ob1")
        ));
    }

    #[test]
    fn end_to_end_obj_tab_imports_objects_then_dot_lists_static_methods() {
        let mut text =
            "package demo;\n\npublic class App {\n    void run() {\n        Obj\n    }\n}\n"
                .to_owned();
        let mut caret = text.find("Obj\n").unwrap() + 3;
        let snap = Buffer::from_text(&text).snapshot();
        assert!(should_auto_trigger_identifier_completion(
            &snap,
            SourceLanguageKind::Java,
            caret
        ));

        // 1. `Obj` → class proposals with packages; the typed token itself is not offered.
        let ctx = extract_completion_context(
            &snap,
            &java(),
            caret,
            CompletionTriggerKind::AutoIdentifier,
        )
        .unwrap();
        let res = resolve_local_completion(&ctx, &text, &[], None, &[]);
        assert!(!res.proposals.iter().any(|p| p.label == "Obj"));
        let objects = res
            .proposals
            .iter()
            .find(|p| p.label == "Objects")
            .expect("Objects proposal");
        assert_eq!(objects.detail_right.as_deref(), Some("java.util"));
        assert_eq!(objects.fqcn.as_deref(), Some("java.util.Objects"));
        assert_eq!(objects.kind.icon_letter(), 'C');
        let consumer = res
            .proposals
            .iter()
            .find(|p| p.label == "ObjIntConsumer")
            .expect("ObjIntConsumer proposal");
        assert_eq!(consumer.kind.icon_letter(), 'I');
        assert_eq!(
            consumer.inline_detail().as_deref(),
            Some("<T> java.util.function")
        );
        let object = res.proposals.iter().find(|p| p.label == "Object").unwrap();
        assert_eq!(
            object.additional_text_edits,
            vec![],
            "java.lang needs no import"
        );

        // 2. Tab → `Objects` + `import java.util.Objects;`, caret right after `Objects`.
        let composed = compose_completion_edits(
            &snap,
            ctx.replace_range.clone(),
            &objects.insert_text,
            objects.insert_text.len(),
            &objects.additional_text_edits,
        );
        let mut buf = Buffer::from_text(&text);
        assert!(buf.apply_edits(&composed.edits).is_ok());
        text = buf.snapshot().text();
        caret = composed.caret_after;
        assert!(text.contains("\nimport java.util.Objects;\n"));
        assert_eq!(&text[caret - "Objects".len()..caret], "Objects");
        assert_eq!(&text[caret..=caret], "\n");

        // 3. `.` → static members of `Objects`.
        text.insert(caret, '.');
        caret += 1;
        let snap2 = Buffer::from_text(&text).snapshot();
        assert!(should_auto_trigger_completion(
            &snap2,
            SourceLanguageKind::Java,
            caret,
            '.'
        ));
        let ctx2 = extract_completion_context(
            &snap2,
            &java(),
            caret,
            CompletionTriggerKind::AutoChar('.'),
        )
        .unwrap();
        let members = resolve_local_completion(&ctx2, &text, &[], None, &[]);
        for name in ["requireNonNull", "checkIndex", "isNull"] {
            let m = members
                .proposals
                .iter()
                .find(|p| p.label == name)
                .unwrap_or_else(|| panic!("missing {name}"));
            assert!(m.is_static, "{name} must carry the static badge");
            assert!(m.is_callable && m.has_params);
        }
        let check = members
            .proposals
            .iter()
            .find(|p| p.label == "checkIndex")
            .unwrap();
        assert_eq!(
            check.inline_detail().as_deref(),
            Some("(int index, int length)")
        );
        assert_eq!(check.right_label(), Some("int"));
    }
}
