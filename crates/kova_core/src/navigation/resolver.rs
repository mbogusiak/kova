//! Fast Level-1 workspace symbol resolver for Java, Kotlin, and Rust (`< 15 ms`).
//!
//! Operates purely on [`BufferSnapshot`] and plain UTF-8 source text without GPUI, `tokio`,
//! or any external language server.

use std::collections::HashSet;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kova_buffer::BufferSnapshot;

/// Hard time budget for Level-1 disk scanning across workspace files.
pub(crate) const LEVEL1_SCAN_BUDGET: Duration = Duration::from_millis(12);

/// Build/VCS directories skipped during workspace candidate scanning.
const IGNORED_DIRS: &[&str] = &[
    "target",
    "build",
    ".git",
    ".gradle",
    "out",
    "node_modules",
    ".idea",
    ".kova",
];

pub(crate) const JAVA_KEYWORDS: &[&str] = &[
    "abstract",
    "assert",
    "boolean",
    "break",
    "byte",
    "case",
    "catch",
    "char",
    "class",
    "const",
    "continue",
    "default",
    "do",
    "double",
    "else",
    "enum",
    "extends",
    "false",
    "final",
    "finally",
    "float",
    "for",
    "goto",
    "if",
    "implements",
    "import",
    "instanceof",
    "int",
    "interface",
    "long",
    "native",
    "new",
    "null",
    "package",
    "private",
    "protected",
    "public",
    "record",
    "return",
    "short",
    "static",
    "strictfp",
    "super",
    "switch",
    "synchronized",
    "this",
    "throw",
    "throws",
    "transient",
    "true",
    "try",
    "var",
    "void",
    "volatile",
    "while",
    "yield",
    "sealed",
    "non-sealed",
    "permits",
];

pub(crate) const KOTLIN_KEYWORDS: &[&str] = &[
    "abstract",
    "actual",
    "annotation",
    "as",
    "break",
    "by",
    "catch",
    "class",
    "companion",
    "const",
    "constructor",
    "continue",
    "crossinline",
    "data",
    "do",
    "else",
    "enum",
    "expect",
    "external",
    "false",
    "final",
    "finally",
    "for",
    "fun",
    "if",
    "import",
    "in",
    "infix",
    "init",
    "inline",
    "inner",
    "interface",
    "internal",
    "is",
    "lateinit",
    "noinline",
    "null",
    "object",
    "open",
    "operator",
    "out",
    "override",
    "package",
    "private",
    "protected",
    "public",
    "reified",
    "return",
    "sealed",
    "super",
    "suspend",
    "tailrec",
    "this",
    "throw",
    "true",
    "try",
    "typealias",
    "typeof",
    "val",
    "value",
    "var",
    "vararg",
    "when",
    "while",
];

pub(crate) const RUST_KEYWORDS: &[&str] = &[
    "Self", "abstract", "as", "async", "await", "become", "box", "break", "const", "continue",
    "crate", "do", "dyn", "else", "enum", "extern", "false", "final", "fn", "for", "if", "impl",
    "in", "let", "loop", "macro", "match", "mod", "move", "mut", "override", "priv", "pub", "ref",
    "return", "self", "static", "struct", "super", "trait", "true", "try", "type", "typeof",
    "unsafe", "unsized", "use", "virtual", "where", "while", "yield",
];

/// Source language recognized by the Level-1 local resolver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SourceLanguageKind {
    /// Java (`.java`).
    Java,
    /// Kotlin (`.kt`, `.kts`).
    Kotlin,
    /// Rust (`.rs`).
    Rust,
    /// Any other or unknown file type.
    #[default]
    Unknown,
}

impl SourceLanguageKind {
    /// Detects the language from a file extension.
    #[must_use]
    pub fn from_path(path: &Path) -> Self {
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            return Self::Unknown;
        };
        if ext.eq_ignore_ascii_case("java") {
            Self::Java
        } else if ext.eq_ignore_ascii_case("kt") || ext.eq_ignore_ascii_case("kts") {
            Self::Kotlin
        } else if ext.eq_ignore_ascii_case("rs") {
            Self::Rust
        } else {
            Self::Unknown
        }
    }

    fn is_keyword(self, word: &str) -> bool {
        match self {
            Self::Java => JAVA_KEYWORDS.contains(&word),
            Self::Kotlin => KOTLIN_KEYWORDS.contains(&word),
            Self::Rust => RUST_KEYWORDS.contains(&word),
            Self::Unknown => false,
        }
    }

    pub(crate) fn matches_path(self, path: &Path) -> bool {
        let other = Self::from_path(path);
        match self {
            Self::Java => other == Self::Java,
            // Kotlin projects commonly interop with Java classes.
            Self::Kotlin => matches!(other, Self::Kotlin | Self::Java),
            Self::Rust => other == Self::Rust,
            Self::Unknown => false,
        }
    }
}

/// One parsed `import` / `use` directive from the active file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportEntry {
    /// Full imported path (`dev.kova.demo.OrderService`, `crate::orders::Order`, etc.).
    pub path: String,
    /// Terminal symbol name (`OrderService`), or `None` for wildcard imports (`*`).
    pub symbol: Option<String>,
    /// Optional local alias (`import Foo as Bar` or `use foo::Bar as Baz`).
    pub alias: Option<String>,
    /// Whether this entry ends with `.*` or `::*`.
    pub is_wildcard: bool,
    /// Whether this is a Java `import static`.
    pub is_static: bool,
}

impl ImportEntry {
    /// Effective local name introduced in scope, if non-wildcard.
    #[must_use]
    pub fn local_name(&self) -> Option<&str> {
        self.alias.as_deref().or(self.symbol.as_deref())
    }
}

/// Extracted symbol query at the caret or mouse position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolQuery {
    /// Language of the source file.
    pub language: SourceLanguageKind,
    /// Source file path (matching the shell's path key, e.g. project-relative).
    pub source_path: PathBuf,
    /// Identifier under cursor.
    pub ident: String,
    /// Byte range of `ident` inside `snapshot`.
    pub ident_range: Range<usize>,
    /// Optional qualifier immediately preceding `.` or `::` (e.g. `service` in `service.create`).
    pub qualifier: Option<String>,
    /// Separator before `ident` (`"."` or `"::"`), if `qualifier` is present.
    pub separator: Option<String>,
    /// `true` when the cursor is on an `import`, `use`, or `mod` statement line.
    pub is_import_or_use: bool,
    /// Qualified path on the `import`/`use` line, when `is_import_or_use` is `true`.
    pub import_path: Option<String>,
    /// Declared `package` in Java/Kotlin, if present.
    pub package_name: Option<String>,
    /// Import / `use` entries in the source file.
    pub imports: Vec<ImportEntry>,
}

/// Resolved target location produced by Level-1 local resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLocation {
    /// Target file path (relative for workspace files, or matching `source_path`).
    pub path: PathBuf,
    /// 1-based line number of the declaration identifier.
    pub line: usize,
    /// 1-based character column of the declaration identifier.
    pub column: usize,
    /// Resolved symbol name.
    pub symbol_name: String,
}

/// Ordered external JVM FQN candidates and optional member lookup produced for Level-1.5
/// bytecode/decompilation navigation (`KOVA-061`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExternalTargetCandidates {
    /// Ordered candidate binary/canonical FQCNs (e.g. `java.util.Objects`, `java.util.Map$Entry`).
    pub fqcns: Vec<String>,
    /// Optional member name on the resolved class (e.g. `requireNonNull` in `Objects.requireNonNull`).
    pub member_name: Option<String>,
    /// Optional intermediate field chain on the receiver (e.g. `["out"]` for `System.out.println`).
    pub field_chain: Vec<String>,
}

pub(crate) const fn is_ident_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

pub(crate) const fn is_ident_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_'
}

/// Checks in $O(\text{line length})$ whether `byte` lies on a navigable identifier (not inside
/// whitespace, punctuation, a comment, a string/char literal, a numeric literal, or a language
/// keyword) and returns its document byte range.
#[must_use]
pub fn navigable_identifier_at_byte(
    snapshot: &BufferSnapshot,
    byte: usize,
    lang: SourceLanguageKind,
) -> Option<Range<usize>> {
    if lang == SourceLanguageKind::Unknown {
        return None;
    }
    let total_bytes = snapshot.len_bytes();
    if byte >= total_bytes {
        return None;
    }
    let byte = snapshot.clamp_byte_to_char_boundary(byte);
    let pt = snapshot.byte_to_point(byte).ok()?;
    let line_start = snapshot.rope().line_to_byte(pt.row);
    let line_slice = snapshot.line_slice(pt.row).ok()?;
    let line: String = line_slice.chunks().collect();
    let rel = byte.saturating_sub(line_start);
    if rel >= line.len() {
        return None;
    }

    let range = identifier_range_in_line(&line, rel, lang)?;
    Some((line_start + range.start)..(line_start + range.end))
}

fn identifier_range_in_line(
    line: &str,
    rel: usize,
    lang: SourceLanguageKind,
) -> Option<Range<usize>> {
    if rel >= line.len() {
        return None;
    }
    let current = line[rel..].chars().next()?;
    if !is_ident_char(current) {
        return None;
    }
    if is_inside_comment_or_string(line, rel, lang) {
        return None;
    }

    let mut start = rel;
    while start > 0 {
        let prev = line[..start].chars().next_back()?;
        if !is_ident_char(prev) {
            break;
        }
        start -= prev.len_utf8();
    }

    let mut end = rel;
    while end < line.len() {
        let next = line[end..].chars().next()?;
        if !is_ident_char(next) {
            break;
        }
        end += next.len_utf8();
    }

    let word = &line[start..end];
    let first = word.chars().next()?;
    if !is_ident_start(first) || lang.is_keyword(word) {
        return None;
    }
    Some(start..end)
}

/// Fast single-line lexical check rejecting line comments (`//`), Javadoc continuation lines
/// (` * ...`), inline block comments (`/* ... */`), and string/char literals.
pub(crate) fn is_inside_comment_or_string(
    line: &str,
    target_rel: usize,
    lang: SourceLanguageKind,
) -> bool {
    let trimmed = line.trim_start();
    if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*') {
        return true;
    }

    let bytes = line.as_bytes();
    let mut i = 0;
    let mut in_double = false;
    let mut in_single = false;
    let mut in_block_comment = false;

    while i <= target_rel && i < bytes.len() {
        let b = bytes[i];
        if in_block_comment {
            if b == b'*' && bytes.get(i + 1) == Some(&b'/') {
                if target_rel <= i + 1 {
                    return true;
                }
                in_block_comment = false;
                i += 2;
                continue;
            }
            if i == target_rel {
                return true;
            }
            i += 1;
            continue;
        }
        if in_double {
            if b == b'\\' {
                if i == target_rel || i + 1 == target_rel {
                    return true;
                }
                i += 2;
                continue;
            }
            if i == target_rel {
                return true;
            }
            if b == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        if in_single {
            if b == b'\\' {
                if i == target_rel || i + 1 == target_rel {
                    return true;
                }
                i += 2;
                continue;
            }
            if i == target_rel {
                return true;
            }
            if b == b'\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }

        if b == b'/' && bytes.get(i + 1) == Some(&b'/') {
            return target_rel >= i;
        }
        if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            in_block_comment = true;
            i += 2;
            continue;
        }
        if b == b'"' {
            in_double = true;
            i += 1;
            continue;
        }
        if b == b'\'' {
            // In Rust `'a` / `'static` are lifetimes, whereas `'x'` / `'\n'` are char literals.
            let is_char_lit = if lang == SourceLanguageKind::Rust {
                has_closing_char_quote(bytes, i)
            } else {
                true
            };
            if is_char_lit {
                in_single = true;
            }
            i += 1;
            continue;
        }
        i += 1;
    }

    in_double || in_single || in_block_comment
}

fn has_closing_char_quote(bytes: &[u8], open_idx: usize) -> bool {
    let mut j = open_idx + 1;
    if j >= bytes.len() {
        return false;
    }
    if bytes[j] == b'\\' {
        j += 2;
    } else {
        j += 1;
    }
    bytes.get(j) == Some(&b'\'')
}

/// Extracts the [`SymbolQuery`] at `byte` (falling back to the character immediately before
/// `byte` when the caret sits right after an identifier, e.g. `Order|`).
#[must_use]
pub fn extract_symbol_query(
    snapshot: &BufferSnapshot,
    source_path: &Path,
    byte: usize,
) -> Option<SymbolQuery> {
    let language = SourceLanguageKind::from_path(source_path);
    if language == SourceLanguageKind::Unknown {
        return None;
    }
    let clamped = snapshot.clamp_byte_to_char_boundary(byte.min(snapshot.len_bytes()));
    let ident_range = navigable_identifier_at_byte(snapshot, clamped, language).or_else(|| {
        if clamped == 0 {
            return None;
        }
        let pt = snapshot.byte_to_point(clamped).ok()?;
        let line_start = snapshot.rope().line_to_byte(pt.row);
        if clamped <= line_start {
            return None;
        }
        let prev = snapshot.clamp_byte_to_char_boundary(clamped - 1);
        navigable_identifier_at_byte(snapshot, prev, language)
    })?;

    let pt = snapshot.byte_to_point(ident_range.start).ok()?;
    let line_start = snapshot.rope().line_to_byte(pt.row);
    let line: String = snapshot.line_slice(pt.row).ok()?.chunks().collect();
    let local_start = ident_range.start.saturating_sub(line_start);
    let local_end = ident_range.end.saturating_sub(line_start);
    let ident = line.get(local_start..local_end)?.to_owned();

    let (qualifier, separator) = extract_qualifier(&line, local_start);
    let trimmed = line.trim_start();
    let is_import_or_use = match language {
        SourceLanguageKind::Java | SourceLanguageKind::Kotlin => trimmed.starts_with("import "),
        SourceLanguageKind::Rust => {
            trimmed.starts_with("use ")
                || trimmed.starts_with("pub use ")
                || trimmed.starts_with("mod ")
                || trimmed.starts_with("pub mod ")
        }
        SourceLanguageKind::Unknown => false,
    };
    let import_path = if is_import_or_use {
        extract_import_path_up_to(&line, local_end, language)
    } else {
        None
    };

    let full_text = snapshot.text();
    let (package_name, imports) = parse_package_and_imports(&full_text, language);

    Some(SymbolQuery {
        language,
        source_path: source_path.to_path_buf(),
        ident,
        ident_range,
        qualifier,
        separator,
        is_import_or_use,
        import_path,
        package_name,
        imports,
    })
}

fn extract_qualifier(line: &str, local_start: usize) -> (Option<String>, Option<String>) {
    let prefix = line[..local_start].trim_end();
    let (sep, before_sep) = if let Some(rest) = prefix.strip_suffix("::") {
        ("::", rest.trim_end())
    } else if let Some(rest) = prefix.strip_suffix('.') {
        (".", rest.trim_end())
    } else {
        return (None, None);
    };

    // Walk backwards over qualifier characters (`foo`, `crate::orders`, `self`, `Foo<T>` -> strip generic if simple).
    let bytes = before_sep.as_bytes();
    let mut start = bytes.len();
    while start > 0 {
        let b = bytes[start - 1];
        if b.is_ascii_alphanumeric() || b == b'_' || b == b':' || b == b'.' {
            start -= 1;
        } else {
            break;
        }
    }
    let qual = before_sep[start..].trim_matches(|c| c == ':' || c == '.');
    if qual.is_empty() {
        (None, None)
    } else {
        (Some(qual.to_owned()), Some(sep.to_owned()))
    }
}

fn extract_import_path_up_to(
    line: &str,
    local_end: usize,
    lang: SourceLanguageKind,
) -> Option<String> {
    let prefix = line.get(..local_end)?.trim_start();
    let body = match lang {
        SourceLanguageKind::Java | SourceLanguageKind::Kotlin => prefix
            .strip_prefix("import ")?
            .trim_start()
            .strip_prefix("static ")
            .unwrap_or_else(|| {
                prefix
                    .strip_prefix("import ")
                    .unwrap_or(prefix)
                    .trim_start()
            }),
        SourceLanguageKind::Rust => {
            let s = prefix.strip_prefix("pub ").unwrap_or(prefix).trim_start();
            s.strip_prefix("use ")
                .or_else(|| s.strip_prefix("mod "))?
                .trim_start()
        }
        SourceLanguageKind::Unknown => return None,
    };
    let cleaned = body.trim().trim_end_matches(';').replace(['{', '}'], "");
    let cleaned = cleaned.trim();
    (!cleaned.is_empty()).then(|| cleaned.to_owned())
}

pub(crate) fn parse_package_and_imports(
    text: &str,
    lang: SourceLanguageKind,
) -> (Option<String>, Vec<ImportEntry>) {
    let mut package_name = None;
    let mut imports = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("//") {
            continue;
        }
        match lang {
            SourceLanguageKind::Java | SourceLanguageKind::Kotlin => {
                if package_name.is_none()
                    && let Some(pkg) = trimmed.strip_prefix("package ")
                {
                    let pkg = pkg.trim().trim_end_matches(';').trim();
                    if !pkg.is_empty() {
                        package_name = Some(pkg.to_owned());
                    }
                    continue;
                }
                if let Some(rest) = trimmed.strip_prefix("import ") {
                    parse_jvm_import(rest, &mut imports);
                    continue;
                }
                if trimmed.starts_with("public ")
                    || trimmed.starts_with("class ")
                    || trimmed.starts_with("interface ")
                    || trimmed.starts_with("enum ")
                    || trimmed.starts_with("record ")
                    || trimmed.starts_with("data ")
                    || trimmed.starts_with("sealed ")
                    || trimmed.starts_with("object ")
                    || trimmed.starts_with("fun ")
                {
                    break;
                }
            }
            SourceLanguageKind::Rust => {
                let s = trimmed.strip_prefix("pub ").unwrap_or(trimmed).trim_start();
                if let Some(rest) = s.strip_prefix("use ") {
                    parse_rust_use(rest, &mut imports);
                } else if let Some(rest) = s.strip_prefix("mod ") {
                    let name = rest.trim().trim_end_matches([';', '{', ' ']).trim();
                    if !name.is_empty() && name.chars().all(is_ident_char) {
                        imports.push(ImportEntry {
                            path: name.to_owned(),
                            symbol: Some(name.to_owned()),
                            alias: None,
                            is_wildcard: false,
                            is_static: false,
                        });
                    }
                }
            }
            SourceLanguageKind::Unknown => {}
        }
    }
    (package_name, imports)
}

fn parse_jvm_import(rest: &str, out: &mut Vec<ImportEntry>) {
    let rest = rest.trim().trim_end_matches(';').trim();
    let (is_static, rest) = if let Some(r) = rest.strip_prefix("static ") {
        (true, r.trim())
    } else {
        (false, rest)
    };
    let (path_part, alias) = if let Some((left, right)) = rest.split_once(" as ") {
        (left.trim(), Some(right.trim().to_owned()))
    } else {
        (rest, None)
    };
    let is_wildcard = path_part.ends_with(".*");
    let symbol = if is_wildcard {
        None
    } else {
        path_part
            .rsplit('.')
            .next()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    out.push(ImportEntry {
        path: path_part.to_owned(),
        symbol,
        alias,
        is_wildcard,
        is_static,
    });
}

fn parse_rust_use(rest: &str, out: &mut Vec<ImportEntry>) {
    let rest = rest.trim().trim_end_matches(';').trim();
    if let Some((prefix, group)) = rest.split_once("::{") {
        let prefix = prefix.trim();
        let inner = group.trim_end_matches('}');
        for item in inner.split(',') {
            let item = item.trim();
            if item.is_empty() {
                continue;
            }
            let (raw_sym, alias) = if let Some((l, r)) = item.split_once(" as ") {
                (l.trim(), Some(r.trim().to_owned()))
            } else {
                (item, None)
            };
            let is_wildcard = raw_sym == "*";
            let full_path = if raw_sym == "self" {
                prefix.to_owned()
            } else {
                format!("{prefix}::{raw_sym}")
            };
            let symbol = if is_wildcard {
                None
            } else if raw_sym == "self" {
                prefix.rsplit("::").next().map(str::to_owned)
            } else {
                raw_sym.rsplit("::").next().map(str::to_owned)
            };
            out.push(ImportEntry {
                path: full_path,
                symbol,
                alias,
                is_wildcard,
                is_static: false,
            });
        }
        return;
    }
    let (path_part, alias) = if let Some((l, r)) = rest.split_once(" as ") {
        (l.trim(), Some(r.trim().to_owned()))
    } else {
        (rest, None)
    };
    let is_wildcard = path_part.ends_with("::*");
    let symbol = if is_wildcard {
        None
    } else {
        path_part
            .rsplit("::")
            .next()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    out.push(ImportEntry {
        path: path_part.to_owned(),
        symbol,
        alias,
        is_wildcard,
        is_static: false,
    });
}

/// Resolves `query` using the Level-1 fast local strategy:
/// 1. Same-file lexical variables, parameters, fields, methods, and types (`< 1 ms`, 0 disk I/O),
/// 2. Open in-memory buffers (`open_buffers`),
/// 3. Prioritized workspace files on disk (`import`/`use`, same package/module, matching filename,
///    then fast byte-filtered scan within a `12 ms` budget).
#[must_use]
pub fn resolve_local_definition(
    query: &SymbolQuery,
    source_text: &str,
    open_buffers: &[(PathBuf, String)],
    project_root: Option<&Path>,
    workspace_files: &[PathBuf],
) -> Option<ResolvedLocation> {
    let started = Instant::now();

    // Step 1a: If cursor is NOT on an `import`/`use` line, try same-file resolution first.
    if !query.is_import_or_use {
        if let Some(local) = resolve_in_same_file(query, source_text) {
            return Some(local);
        }
    }

    // Determine target type + optional member if this is a qualified access (`service.create()` or `OrderService::new()`).
    let (target_type, member_name) = classify_target(query, source_text);

    // Build prioritized list of candidate paths from open buffers + workspace files.
    let candidates = collect_prioritized_candidates(
        query,
        target_type.as_deref(),
        open_buffers,
        workspace_files,
    );

    let open_map: Vec<(&Path, &str)> = open_buffers
        .iter()
        .map(|(p, text)| (p.as_path(), text.as_str()))
        .collect();

    for candidate_path in candidates {
        if started.elapsed() > LEVEL1_SCAN_BUDGET {
            break;
        }
        let owned_text;
        let text: &str = if let Some((_, buf)) = open_map
            .iter()
            .find(|(p, _)| *p == candidate_path.as_path())
        {
            buf
        } else {
            let disk_path = match project_root {
                Some(root) if candidate_path.is_relative() => root.join(&candidate_path),
                _ => candidate_path.clone(),
            };
            let Ok(content) = std::fs::read_to_string(&disk_path) else {
                continue;
            };
            owned_text = content;
            &owned_text
        };

        if let Some(hit) = search_file_for_target(
            &candidate_path,
            text,
            query,
            target_type.as_deref(),
            member_name.as_deref(),
        ) {
            return Some(hit);
        }
    }

    None
}

/// Resolves ordered external JVM FQCN candidates (plus optional member/field chain) for a
/// [`SymbolQuery`] in Java or Kotlin source code (`KOVA-061`).
///
/// This is pure headless text logic used by Level-1.5 bytecode/decompilation navigation when
/// Level-1 workspace resolution finds no local match.
#[must_use]
pub fn resolve_external_fqcn_candidates(
    query: &SymbolQuery,
    source_text: &str,
) -> ExternalTargetCandidates {
    if !matches!(
        query.language,
        SourceLanguageKind::Java | SourceLanguageKind::Kotlin
    ) || query.ident.is_empty()
    {
        return ExternalTargetCandidates::default();
    }

    if query.is_import_or_use {
        return resolve_import_external_candidates(query);
    }

    if let Some(qual) = query.qualifier.as_deref() {
        return resolve_qualified_external_candidates(query, source_text, qual);
    }

    let mut fqcns = Vec::new();
    let mut member_name = None;
    expand_simple_type_to_fqcns(
        &query.ident,
        query,
        &mut fqcns,
        &mut member_name,
        Some(&query.ident),
    );
    ExternalTargetCandidates {
        fqcns,
        member_name,
        field_chain: Vec::new(),
    }
}

fn resolve_import_external_candidates(query: &SymbolQuery) -> ExternalTargetCandidates {
    let Some(imp_path) = query.import_path.as_deref() else {
        return ExternalTargetCandidates::default();
    };

    let mut fqcns = Vec::new();
    let mut member_name = None;

    let matching_static = query.imports.iter().find(|imp| {
        imp.is_static
            && (imp.path == imp_path
                || imp
                    .path
                    .strip_prefix(imp_path)
                    .is_some_and(|rest| rest.starts_with('.')))
    });

    if let Some(static_imp) = matching_static
        && static_imp.path == imp_path
        && let Some((owner, member)) = imp_path.rsplit_once('.')
    {
        if member
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_uppercase())
        {
            push_fqcn_variants(&mut fqcns, imp_path);
            push_fqcn_variants(&mut fqcns, owner);
        } else {
            push_fqcn_variants(&mut fqcns, owner);
            push_fqcn_variants(&mut fqcns, imp_path);
        }
        member_name = Some(member.to_owned());
        return ExternalTargetCandidates {
            fqcns,
            member_name,
            field_chain: Vec::new(),
        };
    }

    push_fqcn_variants(&mut fqcns, imp_path);
    if let Some((parent, leaf)) = imp_path.rsplit_once('.')
        && parent
            .rsplit('.')
            .next()
            .is_some_and(|seg| seg.chars().next().is_some_and(|c| c.is_ascii_uppercase()))
    {
        push_fqcn_variants(&mut fqcns, parent);
        member_name = Some(leaf.to_owned());
    }

    ExternalTargetCandidates {
        fqcns,
        member_name,
        field_chain: Vec::new(),
    }
}

fn resolve_qualified_external_candidates(
    query: &SymbolQuery,
    source_text: &str,
    qual: &str,
) -> ExternalTargetCandidates {
    let parts: Vec<&str> = qual.split('.').filter(|s| !s.is_empty()).collect();
    if parts.is_empty()
        || matches!(
            parts.last().copied(),
            Some("this" | "super" | "self" | "Self" | "crate")
        )
    {
        return ExternalTargetCandidates::default();
    }

    let ident_starts_upper = query
        .ident
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_uppercase());

    if parts.len() == 1 {
        return resolve_single_segment_qualifier(query, source_text, parts[0], ident_starts_upper);
    }

    let first_starts_upper = parts[0]
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_uppercase());

    if !first_starts_upper {
        return resolve_lowercase_multi_qualifier(
            query,
            source_text,
            qual,
            &parts,
            ident_starts_upper,
        );
    }

    resolve_uppercase_multi_qualifier(query, &parts, ident_starts_upper)
}

fn resolve_single_segment_qualifier(
    query: &SymbolQuery,
    source_text: &str,
    single: &str,
    ident_starts_upper: bool,
) -> ExternalTargetCandidates {
    let mut fqcns = Vec::new();
    if let Some(inferred_ty) =
        infer_receiver_type(source_text, query.ident_range.start, single, query.language)
    {
        let mut member_name = Some(query.ident.clone());
        expand_simple_type_to_fqcns(&inferred_ty, query, &mut fqcns, &mut member_name, None);
        return ExternalTargetCandidates {
            fqcns,
            member_name,
            field_chain: Vec::new(),
        };
    }

    if single
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_uppercase())
    {
        let mut owners = Vec::new();
        let mut unused_member = None;
        expand_simple_type_to_fqcns(single, query, &mut owners, &mut unused_member, None);
        if ident_starts_upper {
            for owner in &owners {
                push_unique(&mut fqcns, format!("{owner}${}", query.ident));
                push_unique(&mut fqcns, format!("{owner}.{}", query.ident));
            }
        }
        for owner in owners {
            push_unique(&mut fqcns, owner);
        }
        return ExternalTargetCandidates {
            fqcns,
            member_name: Some(query.ident.clone()),
            field_chain: Vec::new(),
        };
    }

    ExternalTargetCandidates::default()
}

fn resolve_lowercase_multi_qualifier(
    query: &SymbolQuery,
    source_text: &str,
    qual: &str,
    parts: &[&str],
    ident_starts_upper: bool,
) -> ExternalTargetCandidates {
    let mut fqcns = Vec::new();
    if let Some(inferred_ty) = infer_receiver_type(
        source_text,
        query.ident_range.start,
        parts[0],
        query.language,
    ) {
        let mut unused_member = None;
        expand_simple_type_to_fqcns(&inferred_ty, query, &mut fqcns, &mut unused_member, None);
        return ExternalTargetCandidates {
            fqcns,
            member_name: Some(query.ident.clone()),
            field_chain: parts[1..].iter().map(|s| (*s).to_owned()).collect(),
        };
    }

    let first_upper_idx = parts
        .iter()
        .position(|seg| seg.chars().next().is_some_and(|c| c.is_ascii_uppercase()));
    if first_upper_idx.is_none() {
        push_fqcn_variants(&mut fqcns, &format!("{qual}.{}", query.ident));
        return ExternalTargetCandidates {
            fqcns,
            member_name: None,
            field_chain: Vec::new(),
        };
    }

    let last_upper_idx = parts
        .iter()
        .rposition(|seg| seg.chars().next().is_some_and(|c| c.is_ascii_uppercase()))
        .unwrap_or(parts.len() - 1);

    if last_upper_idx == parts.len() - 1 {
        if ident_starts_upper {
            push_fqcn_variants(&mut fqcns, &format!("{qual}.{}", query.ident));
        }
        push_fqcn_variants(&mut fqcns, qual);
        return ExternalTargetCandidates {
            fqcns,
            member_name: Some(query.ident.clone()),
            field_chain: Vec::new(),
        };
    }

    let owner_qual = parts[..=last_upper_idx].join(".");
    push_fqcn_variants(&mut fqcns, &owner_qual);
    ExternalTargetCandidates {
        fqcns,
        member_name: Some(query.ident.clone()),
        field_chain: parts[last_upper_idx + 1..]
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
    }
}

fn resolve_uppercase_multi_qualifier(
    query: &SymbolQuery,
    parts: &[&str],
    ident_starts_upper: bool,
) -> ExternalTargetCandidates {
    let mut fqcns = Vec::new();
    let mut owners = Vec::new();
    let mut unused_member = None;
    expand_simple_type_to_fqcns(parts[0], query, &mut owners, &mut unused_member, None);

    let split_at = parts
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, seg)| !seg.chars().next().is_some_and(|c| c.is_ascii_uppercase()))
        .map_or(parts.len(), |(idx, _)| idx);

    if split_at == parts.len() {
        let nested_dollar = parts[1..].join("$");
        let nested_dot = parts[1..].join(".");
        for owner in &owners {
            if ident_starts_upper {
                push_unique(
                    &mut fqcns,
                    format!("{owner}${nested_dollar}${}", query.ident),
                );
            }
            push_unique(&mut fqcns, format!("{owner}${nested_dollar}"));
            push_unique(&mut fqcns, format!("{owner}.{nested_dot}"));
        }
        for owner in owners {
            push_unique(&mut fqcns, owner);
        }
        return ExternalTargetCandidates {
            fqcns,
            member_name: Some(query.ident.clone()),
            field_chain: Vec::new(),
        };
    }

    for owner in &owners {
        if split_at > 1 {
            let nested_dollar = parts[1..split_at].join("$");
            let nested_dot = parts[1..split_at].join(".");
            push_unique(&mut fqcns, format!("{owner}${nested_dollar}"));
            push_unique(&mut fqcns, format!("{owner}.{nested_dot}"));
        } else {
            push_unique(&mut fqcns, owner.clone());
        }
    }

    ExternalTargetCandidates {
        fqcns,
        member_name: Some(query.ident.clone()),
        field_chain: parts[split_at..].iter().map(|s| (*s).to_owned()).collect(),
    }
}

fn expand_simple_type_to_fqcns(
    type_name: &str,
    query: &SymbolQuery,
    out: &mut Vec<String>,
    member_name: &mut Option<String>,
    ident_for_static: Option<&str>,
) {
    if let Some((head, rest)) = type_name.split_once('.') {
        let mut matched_import = false;
        for imp in &query.imports {
            if !imp.is_wildcard && imp.local_name() == Some(head) {
                push_fqcn_variants(out, &format!("{}.{rest}", imp.path));
                matched_import = true;
            }
        }
        if !matched_import {
            if head.chars().next().is_some_and(|c| c.is_ascii_lowercase()) {
                push_fqcn_variants(out, type_name);
            } else {
                if let Some(pkg) = query.package_name.as_deref() {
                    push_fqcn_variants(out, &format!("{pkg}.{type_name}"));
                }
                for imp in &query.imports {
                    if imp.is_wildcard && !imp.is_static {
                        let pkg = imp.path.trim_end_matches(".*");
                        push_fqcn_variants(out, &format!("{pkg}.{type_name}"));
                    }
                }
                push_fqcn_variants(out, &format!("java.lang.{type_name}"));
                push_fqcn_variants(out, type_name);
            }
        }
        return;
    }

    for imp in &query.imports {
        if !imp.is_wildcard && imp.local_name() == Some(type_name) {
            if imp.is_static {
                if let Some((owner, member)) = imp.path.rsplit_once('.') {
                    if member_name.is_none() {
                        *member_name =
                            Some(ident_for_static.map_or_else(|| member.to_owned(), str::to_owned));
                    }
                    if member
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_uppercase())
                    {
                        push_fqcn_variants(out, &imp.path);
                        push_fqcn_variants(out, owner);
                    } else {
                        push_fqcn_variants(out, owner);
                        push_fqcn_variants(out, &imp.path);
                    }
                } else {
                    push_fqcn_variants(out, &imp.path);
                }
            } else {
                push_fqcn_variants(out, &imp.path);
            }
        }
    }

    if let Some(pkg) = query.package_name.as_deref() {
        push_unique(out, format!("{pkg}.{type_name}"));
    }

    for imp in &query.imports {
        if imp.is_wildcard {
            let pkg = imp.path.trim_end_matches(".*");
            if imp.is_static {
                if member_name.is_none()
                    && let Some(sym) = ident_for_static
                {
                    *member_name = Some(sym.to_owned());
                }
                push_unique(out, format!("{pkg}${type_name}"));
                push_unique(out, pkg.to_owned());
            } else {
                push_unique(out, format!("{pkg}.{type_name}"));
            }
        }
    }

    push_unique(out, format!("java.lang.{type_name}"));
}

fn push_fqcn_variants(out: &mut Vec<String>, raw: &str) {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return;
    }
    let parts: Vec<&str> = trimmed.split('.').filter(|s| !s.is_empty()).collect();
    if parts.len() >= 2
        && let Some(first_upper) = parts
            .iter()
            .take(parts.len() - 1)
            .position(|seg| seg.chars().next().is_some_and(|c| c.is_ascii_uppercase()))
    {
        let pkg = parts[..first_upper].join(".");
        let nested = parts[first_upper..].join("$");
        let dollar_fqcn = if pkg.is_empty() {
            nested
        } else {
            format!("{pkg}.{nested}")
        };
        push_unique(out, dollar_fqcn);
    }
    push_unique(out, trimmed.to_owned());
    for split_idx in (1..parts.len()).rev() {
        if parts[split_idx - 1]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_uppercase())
        {
            let cand = format!(
                "{}${}",
                parts[..split_idx].join("."),
                parts[split_idx..].join("$")
            );
            push_unique(out, cand);
        }
    }
}

fn push_unique(out: &mut Vec<String>, value: String) {
    if !value.is_empty() && !out.contains(&value) {
        out.push(value);
    }
}

fn classify_target(query: &SymbolQuery, source_text: &str) -> (Option<String>, Option<String>) {
    if query.is_import_or_use {
        // Clicking `OrderService` in `import dev.kova.demo.OrderService;`
        return (Some(query.ident.clone()), None);
    }
    let Some(qual) = query.qualifier.as_deref() else {
        return (Some(query.ident.clone()), None);
    };
    let last_seg = qual.rsplit(['.', ':']).next().unwrap_or(qual);
    if matches!(last_seg, "this" | "self" | "Self" | "super" | "crate") {
        return (None, Some(query.ident.clone()));
    }
    // Check if `last_seg` is a local variable or field whose type we can infer from `source_text`.
    if let Some(inferred) = infer_receiver_type(
        source_text,
        query.ident_range.start,
        last_seg,
        query.language,
    ) {
        return (Some(inferred), Some(query.ident.clone()));
    }
    // Otherwise `qual` itself may be a class/module/enum name (e.g. `OrderService::new`, `OrderStatus.PAID`, `orders::Order`).
    if query.language == SourceLanguageKind::Rust && query.separator.as_deref() == Some("::") {
        // In Rust `orders::Order`, `ident` might be a top-level item in module `orders`,
        // OR `OrderService::new` where `OrderService` is a struct/enum and `new` is an associated fn.
        return (Some(last_seg.to_owned()), Some(query.ident.clone()));
    }
    (Some(last_seg.to_owned()), Some(query.ident.clone()))
}

fn resolve_in_same_file(query: &SymbolQuery, source_text: &str) -> Option<ResolvedLocation> {
    let lang = query.language;
    let cursor_byte = query.ident_range.start;

    if let Some(qual) = query.qualifier.as_deref() {
        let last_seg = qual.rsplit(['.', ':']).next().unwrap_or(qual);
        if matches!(last_seg, "this" | "self" | "Self") {
            return find_member_in_text(&query.source_path, source_text, None, &query.ident, lang)
                .filter(|hit| !is_same_position(source_text, query.ident_range.start, hit));
        }
        // Check if `last_seg` is a type defined in the same file (e.g. `Order::new`, `Order.create`).
        if let Some(hit) = find_member_in_text(
            &query.source_path,
            source_text,
            Some(last_seg),
            &query.ident,
            lang,
        ) {
            return Some(hit);
        }
        // Check if `last_seg` is a local variable/field whose type is defined in this same file.
        if let Some(ty) = infer_receiver_type(source_text, cursor_byte, last_seg, lang) {
            if let Some(hit) = find_member_in_text(
                &query.source_path,
                source_text,
                Some(&ty),
                &query.ident,
                lang,
            ) {
                return Some(hit);
            }
        }
        return None;
    }

    // 1. Local variables & parameters in enclosing scope before `cursor_byte`.
    if let Some(hit) = find_local_var_or_param(
        &query.source_path,
        source_text,
        cursor_byte,
        &query.ident,
        lang,
    ) {
        if !is_same_position(source_text, query.ident_range.start, &hit) {
            return Some(hit);
        }
    }

    // 2. Top-level or class-level declarations in the same file (types, methods/fns, fields/consts).
    let calls_function = looks_like_call(source_text, query.ident_range.end);
    if calls_function {
        if let Some(hit) =
            find_callable_in_text(&query.source_path, source_text, &query.ident, lang)
                .filter(|h| !is_same_position(source_text, query.ident_range.start, h))
        {
            return Some(hit);
        }
    }

    if let Some(hit) = find_type_decl_in_text(&query.source_path, source_text, &query.ident, lang)
        .filter(|h| !is_same_position(source_text, query.ident_range.start, h))
    {
        return Some(hit);
    }

    if let Some(hit) = find_callable_in_text(&query.source_path, source_text, &query.ident, lang)
        .filter(|h| !is_same_position(source_text, query.ident_range.start, h))
    {
        return Some(hit);
    }

    if let Some(hit) =
        find_field_or_const_in_text(&query.source_path, source_text, &query.ident, lang)
            .filter(|h| !is_same_position(source_text, query.ident_range.start, h))
    {
        return Some(hit);
    }

    None
}

fn looks_like_call(source_text: &str, end_byte: usize) -> bool {
    source_text
        .get(end_byte..)
        .and_then(|s| s.trim_start().chars().next())
        == Some('(')
}

fn is_same_position(source_text: &str, byte: usize, hit: &ResolvedLocation) -> bool {
    let (line, col) = byte_to_line_col(source_text, byte);
    line == hit.line && col == hit.column
}

fn byte_to_line_col(text: &str, byte: usize) -> (usize, usize) {
    let clamped = byte.min(text.len());
    let prefix = &text[..clamped];
    let line = prefix.bytes().filter(|&b| b == b'\n').count() + 1;
    let line_start = prefix.rfind('\n').map_or(0, |idx| idx + 1);
    let col = prefix[line_start..].chars().count() + 1;
    (line, col)
}

fn line_col_of_in_line(line_idx_0: usize, line: &str, local_byte: usize) -> (usize, usize) {
    let clamped = local_byte.min(line.len());
    let col = line[..clamped].chars().count() + 1;
    (line_idx_0 + 1, col)
}

/// Scans enclosing block lines before `cursor_byte` for local variables and function parameters.
fn find_local_var_or_param(
    path: &Path,
    source_text: &str,
    cursor_byte: usize,
    ident: &str,
    lang: SourceLanguageKind,
) -> Option<ResolvedLocation> {
    let prefix = source_text.get(..cursor_byte)?;
    let mut active_lines: Vec<(usize, &str)> = Vec::new();
    let mut depth: isize = 0;

    // Walk lines backward from cursor to collect only lines in active enclosing scopes.
    let lines: Vec<&str> = prefix.lines().collect();
    for (idx, line) in lines.iter().enumerate().rev() {
        let trimmed = line.trim();
        // Count braces outside simple strings/comments to know whether a block is closed before cursor.
        let (opens, closes) = count_braces(trimmed);
        // When walking backwards: `}` increases closed depth, `{` decreases it.
        if depth == 0 {
            active_lines.push((idx, line));
        }
        depth += closes - opens;
        if depth < 0 {
            // Entered an enclosing outer scope (e.g. function signature); reset depth to 0 so we capture outer scope & params.
            active_lines.push((idx, line));
            depth = 0;
        }
    }

    // Prefer nearest (innermost) declaration first.
    for (line_idx, line) in active_lines {
        if let Some(col_byte) = match_local_var_decl_in_line(line, ident, lang) {
            let (line_no, col) = line_col_of_in_line(line_idx, line, col_byte);
            return Some(ResolvedLocation {
                path: path.to_path_buf(),
                line: line_no,
                column: col,
                symbol_name: ident.to_owned(),
            });
        }
        if let Some(col_byte) = match_param_decl_in_line(line, ident, lang) {
            let (line_no, col) = line_col_of_in_line(line_idx, line, col_byte);
            return Some(ResolvedLocation {
                path: path.to_path_buf(),
                line: line_no,
                column: col,
                symbol_name: ident.to_owned(),
            });
        }
    }
    None
}

pub(crate) fn count_braces(line: &str) -> (isize, isize) {
    let trimmed = line.trim_start();
    if trimmed.starts_with("//") || trimmed.starts_with('*') {
        return (0, 0);
    }
    let mut opens = 0isize;
    let mut closes = 0isize;
    let mut in_str = false;
    let mut escape = false;
    for b in line.bytes() {
        if in_str {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => opens += 1,
            b'}' => closes += 1,
            _ => {}
        }
    }
    (opens, closes)
}

fn match_local_var_decl_in_line(
    line: &str,
    ident: &str,
    lang: SourceLanguageKind,
) -> Option<usize> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("//") || trimmed.starts_with('*') || trimmed.starts_with("/*") {
        return None;
    }
    match lang {
        SourceLanguageKind::Rust => {
            let after_let = trimmed.strip_prefix("let ")?;
            let after_mut = after_let
                .trim_start()
                .strip_prefix("mut ")
                .unwrap_or(after_let)
                .trim_start();
            let pos = find_exact_ident_at_start(after_mut, ident)?;
            let offset = line.len() - after_mut.len() + pos;
            Some(offset)
        }
        SourceLanguageKind::Kotlin => {
            let after_kw = trimmed
                .strip_prefix("val ")
                .or_else(|| trimmed.strip_prefix("var "))?;
            let after_kw = after_kw.trim_start();
            let pos = find_exact_ident_at_start(after_kw, ident)?;
            Some(line.len() - after_kw.len() + pos)
        }
        SourceLanguageKind::Java => {
            // Match `var x =`, `final Type x =`, `Type x =`, `Type<A, B> x =`, `for (Type x : ...)`
            if let Some(after_for) = trimmed.strip_prefix("for") {
                if let Some(paren) = after_for.trim_start().strip_prefix('(') {
                    if let Some(pos) = match_java_type_and_name(paren, ident) {
                        return Some(line.len() - paren.len() + pos);
                    }
                }
            }
            if let Some(after_catch) = trimmed.strip_prefix("catch") {
                if let Some(paren) = after_catch.trim_start().strip_prefix('(') {
                    if let Some(pos) = match_java_type_and_name(paren, ident) {
                        return Some(line.len() - paren.len() + pos);
                    }
                }
            }
            let stmt = trimmed
                .strip_prefix("final ")
                .unwrap_or(trimmed)
                .trim_start();
            // Must contain `=` or `;` and not be a method header or return/throw/if.
            if stmt.starts_with("return ") || stmt.starts_with("throw ") || stmt.starts_with("if ")
            {
                return None;
            }
            if let Some(pos) = match_java_type_and_name(stmt, ident) {
                let abs_pos = line.len() - stmt.len() + pos;
                let after = line[abs_pos + ident.len()..].trim_start();
                if after.starts_with('=') || after.starts_with(';') || after.starts_with(':') {
                    return Some(abs_pos);
                }
            }
            None
        }
        SourceLanguageKind::Unknown => None,
    }
}

fn match_param_decl_in_line(line: &str, ident: &str, lang: SourceLanguageKind) -> Option<usize> {
    let open = line.find('(')?;
    let close = line[open + 1..]
        .find(')')
        .map_or(line.len(), |i| open + 1 + i);
    let inside = &line[open + 1..close];
    if !inside.contains(ident) {
        return None;
    }
    let base_offset = open + 1;
    let mut part_start = 0;
    for part in split_top_level_commas(inside) {
        let part_offset = base_offset + part_start;
        part_start += part.len() + 1;
        let trimmed = part.trim_start();
        let trim_shift = part.len() - trimmed.len();
        match lang {
            SourceLanguageKind::Rust | SourceLanguageKind::Kotlin => {
                let s = trimmed
                    .strip_prefix("mut ")
                    .or_else(|| trimmed.strip_prefix("val "))
                    .or_else(|| trimmed.strip_prefix("var "))
                    .unwrap_or(trimmed);
                let kw_shift = trimmed.len() - s.len();
                let s_trim = s.trim_start();
                let extra = s.len() - s_trim.len();
                if let Some(pos) = find_exact_ident_at_start(s_trim, ident) {
                    let after = s_trim[pos + ident.len()..].trim_start();
                    if after.starts_with(':') {
                        return Some(part_offset + trim_shift + kw_shift + extra + pos);
                    }
                }
            }
            SourceLanguageKind::Java => {
                let s = trimmed
                    .strip_prefix("final ")
                    .unwrap_or(trimmed)
                    .trim_start();
                let kw_shift = trimmed.len() - s.len();
                if let Some(pos) = match_java_type_and_name(s, ident) {
                    let after = s[pos + ident.len()..].trim_start();
                    if after.is_empty() || after.starts_with(')') {
                        return Some(part_offset + trim_shift + kw_shift + pos);
                    }
                }
            }
            SourceLanguageKind::Unknown => {}
        }
    }
    None
}

pub(crate) fn split_top_level_commas(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut angle = 0usize;
    let mut paren = 0usize;
    let mut start = 0;
    for (i, b) in s.bytes().enumerate() {
        match b {
            b'<' => angle += 1,
            b'>' => angle = angle.saturating_sub(1),
            b'(' => paren += 1,
            b')' => paren = paren.saturating_sub(1),
            b',' if angle == 0 && paren == 0 => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts
}

pub(crate) fn find_exact_ident_at_start(s: &str, ident: &str) -> Option<usize> {
    if !s.starts_with(ident) {
        return None;
    }
    let next = s[ident.len()..].chars().next();
    if next.is_some_and(is_ident_char) {
        return None;
    }
    Some(0)
}

/// Matches `<Type> <ident>` at the start of `s`, returning the byte offset of `<ident>` within `s`.
pub(crate) fn match_java_type_and_name(s: &str, ident: &str) -> Option<usize> {
    // Skip `<Type>` (identifier + optional generic `<...>` + optional `[]`/`...`).
    let bytes = s.as_bytes();
    let mut i = 0;
    // Skip annotations like `@NonNull `
    while i < bytes.len() && bytes[i] == b'@' {
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
    }
    let ty_start = i;
    while i < bytes.len()
        && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'.')
    {
        i += 1;
    }
    if i == ty_start {
        return None;
    }
    // Optional generics `<...>`
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b'<' {
        let mut depth = 1usize;
        i += 1;
        while i < bytes.len() && depth > 0 {
            if bytes[i] == b'<' {
                depth += 1;
            } else if bytes[i] == b'>' {
                depth -= 1;
            }
            i += 1;
        }
    }
    // Optional `[]` or `...`
    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if s[i..].starts_with("[]") {
            i += 2;
        } else if s[i..].starts_with("...") {
            i += 3;
        } else {
            break;
        }
    }
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let rest = &s[i..];
    find_exact_ident_at_start(rest, ident).map(|pos| i + pos)
}

/// Attempts lightweight type inference for a local variable or field `var_name` in `source_text`
/// before `cursor_byte` (e.g. `OrderService service = ...` -> `"OrderService"`,
/// `let service = OrderService::new()` -> `"OrderService"`).
pub(crate) fn infer_receiver_type(
    source_text: &str,
    cursor_byte: usize,
    var_name: &str,
    lang: SourceLanguageKind,
) -> Option<String> {
    let search_slice = source_text.get(..cursor_byte).unwrap_or(source_text);
    for line in search_slice.lines().rev() {
        let trimmed_line = line.trim();
        if trimmed_line.starts_with("//") || !trimmed_line.contains(var_name) {
            continue;
        }
        for segment in trimmed_line.split_inclusive([';', '{', '}']).rev() {
            let trimmed = segment.trim().trim_end_matches(['{', '}']).trim();
            if trimmed.is_empty() || !trimmed.contains(var_name) {
                continue;
            }
            let found = match lang {
                SourceLanguageKind::Java => infer_java_receiver_type(trimmed, var_name),
                SourceLanguageKind::Kotlin => infer_kotlin_receiver_type(trimmed, var_name),
                SourceLanguageKind::Rust => infer_rust_receiver_type(trimmed, var_name),
                SourceLanguageKind::Unknown => None,
            };
            if found.is_some() {
                return found;
            }
        }
    }
    None
}

fn infer_java_receiver_type(trimmed: &str, var_name: &str) -> Option<String> {
    let cleaned = trimmed
        .trim_start_matches("private ")
        .trim_start_matches("public ")
        .trim_start_matches("protected ")
        .trim_start_matches("final ")
        .trim_start_matches("static ")
        .trim();
    let ty = extract_java_declared_type(cleaned, var_name)?;
    if ty != "var" {
        return Some(ty);
    }
    let (_, rhs) = cleaned.split_once('=')?;
    let rhs_trim = rhs.trim();
    if rhs_trim.starts_with('"') {
        return Some("String".to_owned());
    }
    if let Some(new_ty) = rhs_trim.strip_prefix("new ") {
        let ty_name: String = new_ty.chars().take_while(|c| is_ident_char(*c)).collect();
        return (!ty_name.is_empty()).then_some(ty_name);
    }
    // Common Java static factory calls such as `List.of(...)`, `Optional.of(...)`, `Files.newBufferedReader(...)`.
    let factory_head: String = rhs_trim.chars().take_while(|c| is_ident_char(*c)).collect();
    if factory_head
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_uppercase())
        && rhs_trim[factory_head.len()..].starts_with('.')
    {
        return match factory_head.as_str() {
            "List" | "Set" | "Map" | "Optional" | "Stream" | "Path" | "Paths" | "LocalDate"
            | "LocalDateTime" | "Instant" | "Duration" | "StringBuilder" | "String" => {
                Some(factory_head)
            }
            _ => None,
        };
    }
    None
}

fn infer_kotlin_receiver_type(trimmed: &str, var_name: &str) -> Option<String> {
    let cleaned = trimmed
        .trim_start_matches("private ")
        .trim_start_matches("val ")
        .trim_start_matches("var ")
        .trim();
    let after_name = cleaned
        .strip_prefix(var_name)
        .filter(|rest| rest.is_empty() || !rest.chars().next().is_some_and(is_ident_char))?
        .trim_start();
    if let Some(after_colon) = after_name.strip_prefix(':') {
        let ty: String = after_colon
            .trim_start()
            .chars()
            .take_while(|c| is_ident_char(*c))
            .collect();
        return (!ty.is_empty()).then_some(ty);
    }
    let after_eq = after_name.strip_prefix('=')?;
    let ty: String = after_eq
        .trim_start()
        .chars()
        .take_while(|c| is_ident_char(*c))
        .collect();
    ty.chars()
        .next()
        .is_some_and(|c| c.is_ascii_uppercase())
        .then_some(ty)
}

fn infer_rust_receiver_type(trimmed: &str, var_name: &str) -> Option<String> {
    let cleaned = trimmed
        .strip_prefix("let ")
        .map_or(trimmed, |r| {
            r.trim_start().strip_prefix("mut ").unwrap_or(r)
        })
        .trim();
    let after_name = cleaned
        .strip_prefix(var_name)
        .filter(|rest| rest.is_empty() || !rest.chars().next().is_some_and(is_ident_char))?
        .trim_start();
    if let Some(after_colon) = after_name.strip_prefix(':') {
        let ty = after_colon
            .trim_start()
            .trim_start_matches('&')
            .trim_start_matches("mut ")
            .trim_start();
        let ty_name: String = ty.chars().take_while(|c| is_ident_char(*c)).collect();
        return (!ty_name.is_empty()).then_some(ty_name);
    }
    let after_eq = after_name.strip_prefix('=')?;
    let ty_name: String = after_eq
        .trim_start()
        .chars()
        .take_while(|c| is_ident_char(*c))
        .collect();
    ty_name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_uppercase())
        .then_some(ty_name)
}

fn extract_java_declared_type(stmt: &str, var_name: &str) -> Option<String> {
    // Check inside `(Type var_name)` parameter list or `Type var_name` statement.
    let candidate = if let Some(open) = stmt.find('(') {
        if let Some(close) = stmt[open + 1..].find(')') {
            let inside = &stmt[open + 1..open + 1 + close];
            for part in split_top_level_commas(inside) {
                let p = part
                    .trim()
                    .strip_prefix("final ")
                    .unwrap_or(part.trim())
                    .trim();
                if let Some(pos) = match_java_type_and_name(p, var_name) {
                    if p[pos + var_name.len()..].trim().is_empty() {
                        let ty: String = p.chars().take_while(|c| is_ident_char(*c)).collect();
                        if !ty.is_empty() {
                            return Some(ty);
                        }
                    }
                }
            }
        }
        stmt
    } else {
        stmt
    };
    if let Some(pos) = match_java_type_and_name(candidate, var_name) {
        let after = candidate[pos + var_name.len()..].trim_start();
        if after.starts_with('=') || after.starts_with(';') {
            let ty: String = candidate
                .chars()
                .take_while(|c| is_ident_char(*c))
                .collect();
            if !ty.is_empty() {
                return Some(ty);
            }
        }
    }
    None
}

fn collect_prioritized_candidates(
    query: &SymbolQuery,
    target_type: Option<&str>,
    open_buffers: &[(PathBuf, String)],
    workspace_files: &[PathBuf],
) -> Vec<PathBuf> {
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut tier1_import = Vec::new();
    let mut tier2_package_or_name = Vec::new();
    let mut tier3_open_buffers = Vec::new();
    let mut tier4_workspace = Vec::new();

    let all_paths: Vec<PathBuf> = open_buffers
        .iter()
        .map(|(p, _)| p.clone())
        .chain(workspace_files.iter().cloned())
        .collect();

    let expected_suffixes = expected_import_suffixes(query, target_type);
    let package_rel_dir = query.package_name.as_deref().map(|p| p.replace('.', "/"));
    let source_dir = query.source_path.parent().map(Path::to_path_buf);
    let target_snake = target_type.map(to_snake_case);

    for path in all_paths {
        if is_ignored_path(&path) || !query.language.matches_path(&path) {
            continue;
        }
        if !seen.insert(path.clone()) {
            continue;
        }
        let path_str = path.to_string_lossy().replace('\\', "/");
        if expected_suffixes.iter().any(|sfx| path_str.ends_with(sfx)) {
            tier1_import.push(path);
            continue;
        }
        let is_open = open_buffers.iter().any(|(p, _)| p == &path);
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let name_matches = target_type.is_some_and(|t| stem == t)
            || target_snake.as_deref().is_some_and(|s| stem == s)
            || stem == query.ident;
        let same_pkg = package_rel_dir
            .as_deref()
            .is_some_and(|dir| path_str.contains(dir))
            || source_dir
                .as_deref()
                .is_some_and(|dir| !dir.as_os_str().is_empty() && path.parent() == Some(dir));

        if name_matches || same_pkg {
            tier2_package_or_name.push(path);
        } else if is_open {
            tier3_open_buffers.push(path);
        } else {
            tier4_workspace.push(path);
        }
    }

    tier1_import
        .into_iter()
        .chain(tier2_package_or_name)
        .chain(tier3_open_buffers)
        .chain(tier4_workspace)
        .collect()
}

fn expected_import_suffixes(query: &SymbolQuery, target_type: Option<&str>) -> Vec<String> {
    let mut suffixes = Vec::new();
    match query.language {
        SourceLanguageKind::Java | SourceLanguageKind::Kotlin => {
            if query.is_import_or_use
                && let Some(imp) = query.import_path.as_deref()
            {
                let rel = imp.replace('.', "/");
                suffixes.push(format!("{rel}.java"));
                suffixes.push(format!("{rel}.kt"));
                suffixes.push(format!("{rel}.kts"));
            }
            if let Some(ty) = target_type {
                for imp in &query.imports {
                    if imp.local_name() == Some(ty) {
                        let rel = imp.path.replace('.', "/");
                        suffixes.push(format!("{rel}.java"));
                        suffixes.push(format!("{rel}.kt"));
                    } else if imp.is_wildcard {
                        let pkg = imp.path.trim_end_matches(".*").replace('.', "/");
                        suffixes.push(format!("{pkg}/{ty}.java"));
                        suffixes.push(format!("{pkg}/{ty}.kt"));
                    }
                }
                if let Some(pkg) = query.package_name.as_deref() {
                    let rel_pkg = pkg.replace('.', "/");
                    suffixes.push(format!("{rel_pkg}/{ty}.java"));
                    suffixes.push(format!("{rel_pkg}/{ty}.kt"));
                }
            }
        }
        SourceLanguageKind::Rust => {
            let add_rust_mod = |mod_path: &str, out: &mut Vec<String>| {
                let rel = mod_path
                    .trim_start_matches("crate::")
                    .trim_start_matches("self::")
                    .trim_start_matches("super::")
                    .replace("::", "/");
                if !rel.is_empty() {
                    out.push(format!("{rel}.rs"));
                    out.push(format!("{rel}/mod.rs"));
                }
            };
            if query.is_import_or_use
                && let Some(imp) = query.import_path.as_deref()
            {
                add_rust_mod(imp, &mut suffixes);
                if let Some((parent, _)) = imp.rsplit_once("::") {
                    add_rust_mod(parent, &mut suffixes);
                }
            }
            if let Some(ty) = target_type {
                for imp in &query.imports {
                    if imp.local_name() == Some(ty) {
                        add_rust_mod(&imp.path, &mut suffixes);
                        if let Some((parent, _)) = imp.path.rsplit_once("::") {
                            add_rust_mod(parent, &mut suffixes);
                        }
                    }
                }
            }
        }
        SourceLanguageKind::Unknown => {}
    }
    suffixes
}

pub(crate) fn to_snake_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for (i, ch) in name.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

pub(crate) fn is_ignored_path(path: &Path) -> bool {
    path.components().any(|comp| {
        comp.as_os_str()
            .to_str()
            .is_some_and(|s| IGNORED_DIRS.contains(&s))
    })
}

fn search_file_for_target(
    path: &Path,
    text: &str,
    query: &SymbolQuery,
    target_type: Option<&str>,
    member_name: Option<&str>,
) -> Option<ResolvedLocation> {
    let file_lang = SourceLanguageKind::from_path(path);
    let lang = if file_lang == SourceLanguageKind::Unknown {
        query.language
    } else {
        file_lang
    };

    // Fast byte pre-filter before scanning lines.
    if !text.contains(&query.ident) {
        let ty = target_type?;
        if !text.contains(ty) {
            return None;
        }
    }

    if let (Some(ty), Some(member)) = (target_type, member_name) {
        if let Some(hit) = find_member_in_text(path, text, Some(ty), member, lang) {
            return Some(hit);
        }
        // In Rust `mod_name::item`, `ty` may be the module/file stem and `member` a top-level item.
        if lang == SourceLanguageKind::Rust {
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if stem == ty || stem == "mod" {
                if let Some(hit) = find_any_top_level_decl(path, text, member, lang) {
                    return Some(hit);
                }
            }
        }
        return None;
    }

    if let Some(ty) = target_type {
        return find_any_top_level_decl(path, text, ty, lang);
    }

    None
}

fn find_any_top_level_decl(
    path: &Path,
    text: &str,
    name: &str,
    lang: SourceLanguageKind,
) -> Option<ResolvedLocation> {
    find_type_decl_in_text(path, text, name, lang)
        .or_else(|| find_callable_in_text(path, text, name, lang))
        .or_else(|| find_field_or_const_in_text(path, text, name, lang))
}

fn find_type_decl_in_text(
    path: &Path,
    text: &str,
    name: &str,
    lang: SourceLanguageKind,
) -> Option<ResolvedLocation> {
    if !text.contains(name) {
        return None;
    }
    let keywords: &[&str] = match lang {
        SourceLanguageKind::Java => &["class ", "interface ", "enum ", "record ", "@interface "],
        SourceLanguageKind::Kotlin => &[
            "class ",
            "interface ",
            "object ",
            "typealias ",
            "enum class ",
            "data class ",
            "sealed class ",
            "value class ",
        ],
        SourceLanguageKind::Rust => &[
            "struct ",
            "enum ",
            "trait ",
            "type ",
            "mod ",
            "macro_rules! ",
        ],
        SourceLanguageKind::Unknown => &[],
    };

    for (row_idx, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*') {
            continue;
        }
        for kw in keywords {
            if let Some(pos) = find_keyword_decl_in_line(line, kw, name) {
                let (line_no, col) = line_col_of_in_line(row_idx, line, pos);
                return Some(ResolvedLocation {
                    path: path.to_path_buf(),
                    line: line_no,
                    column: col,
                    symbol_name: name.to_owned(),
                });
            }
        }
    }
    None
}

pub(crate) fn find_keyword_decl_in_line(line: &str, keyword: &str, name: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(found) = line[from..].find(keyword) {
        let start = from + found;
        let before_ok = line[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !is_ident_char(c));
        let after_kw = start + keyword.len();
        let rest = line[after_kw..].trim_start();
        let ws = line[after_kw..].len() - rest.len();
        if before_ok && find_exact_ident_at_start(rest, name).is_some() {
            return Some(after_kw + ws);
        }
        from = after_kw;
    }
    None
}

fn find_callable_in_text(
    path: &Path,
    text: &str,
    name: &str,
    lang: SourceLanguageKind,
) -> Option<ResolvedLocation> {
    if !text.contains(name) {
        return None;
    }
    for (row_idx, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*') {
            continue;
        }
        let col_byte = match lang {
            SourceLanguageKind::Rust => find_keyword_decl_in_line(line, "fn ", name),
            SourceLanguageKind::Kotlin => find_keyword_decl_in_line(line, "fun ", name),
            SourceLanguageKind::Java => find_java_method_or_ctor_in_line(line, name),
            SourceLanguageKind::Unknown => None,
        };
        if let Some(pos) = col_byte {
            let (line_no, col) = line_col_of_in_line(row_idx, line, pos);
            return Some(ResolvedLocation {
                path: path.to_path_buf(),
                line: line_no,
                column: col,
                symbol_name: name.to_owned(),
            });
        }
    }
    None
}

fn find_java_method_or_ctor_in_line(line: &str, name: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("if ")
        || trimmed.starts_with("if(")
        || trimmed.starts_with("for ")
        || trimmed.starts_with("for(")
        || trimmed.starts_with("while ")
        || trimmed.starts_with("while(")
        || trimmed.starts_with("switch ")
        || trimmed.starts_with("return ")
        || trimmed.starts_with("new ")
        || trimmed.contains(" = ")
    {
        return None;
    }
    let mut from = 0;
    while let Some(found) = line[from..].find(name) {
        let start = from + found;
        let end = start + name.len();
        let before_ok = line[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !is_ident_char(c) && c != '.');
        let after = line[end..].trim_start();
        if before_ok && after.starts_with('(') {
            // Confirm it's a method/constructor signature (has `{` or `throws` or `;` on interface/abstract)
            // and has either a modifier/return type before `name` or ends with `{` / `)` / `throws`.
            let prefix = line[..start].trim();
            let rest_after_name = after.trim_end();
            let has_body_or_throws =
                rest_after_name.contains('{') || rest_after_name.contains("throws ");
            let is_constructor_header = prefix.is_empty()
                && name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                && !rest_after_name.ends_with(';');
            let is_decl_prefix = is_constructor_header
                || prefix.ends_with("public")
                || prefix.ends_with("private")
                || prefix.ends_with("protected")
                || prefix.ends_with("static")
                || prefix.ends_with("final")
                || prefix.ends_with("default")
                || prefix.ends_with("synchronized")
                || prefix.ends_with("abstract")
                || (!prefix.is_empty()
                    && !prefix.ends_with("new")
                    && !prefix.ends_with("return")
                    && !prefix.ends_with("else")
                    && (has_body_or_throws || rest_after_name.ends_with(';'))
                    && prefix
                        .chars()
                        .next_back()
                        .is_some_and(|c| is_ident_char(c) || c == '>' || c == ']'));
            if is_decl_prefix {
                return Some(start);
            }
        }
        from = end;
    }
    None
}

fn find_field_or_const_in_text(
    path: &Path,
    text: &str,
    name: &str,
    lang: SourceLanguageKind,
) -> Option<ResolvedLocation> {
    if !text.contains(name) {
        return None;
    }
    for (row_idx, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*') {
            continue;
        }
        let col_byte = match lang {
            SourceLanguageKind::Rust => find_keyword_decl_in_line(line, "const ", name)
                .or_else(|| find_keyword_decl_in_line(line, "static ", name))
                .or_else(|| {
                    let s = trimmed.strip_prefix("pub ").unwrap_or(trimmed).trim_start();
                    find_exact_ident_at_start(s, name)
                        .filter(|_| s[name.len()..].trim_start().starts_with(':'))
                        .map(|pos| line.len() - s.len() + pos)
                }),
            SourceLanguageKind::Kotlin => find_keyword_decl_in_line(line, "val ", name)
                .or_else(|| find_keyword_decl_in_line(line, "var ", name)),
            SourceLanguageKind::Java => {
                let s = trimmed
                    .strip_prefix("public ")
                    .or_else(|| trimmed.strip_prefix("private "))
                    .or_else(|| trimmed.strip_prefix("protected "))
                    .unwrap_or(trimmed)
                    .trim_start();
                let s = s.strip_prefix("static ").unwrap_or(s).trim_start();
                let s = s.strip_prefix("final ").unwrap_or(s).trim_start();
                match_java_type_and_name(s, name)
                    .filter(|&pos| {
                        let after = s[pos + name.len()..].trim_start();
                        after.starts_with('=') || after.starts_with(';')
                    })
                    .map(|pos| line.len() - s.len() + pos)
            }
            SourceLanguageKind::Unknown => None,
        };
        if let Some(pos) = col_byte {
            let (line_no, col) = line_col_of_in_line(row_idx, line, pos);
            return Some(ResolvedLocation {
                path: path.to_path_buf(),
                line: line_no,
                column: col,
                symbol_name: name.to_owned(),
            });
        }
    }
    None
}

/// Finds a member (`method`, `fn`, `val`, field, enum variant) inside `type_name` (or anywhere
/// in the file when `type_name` is `None`).
fn find_member_in_text(
    path: &Path,
    text: &str,
    type_name: Option<&str>,
    member: &str,
    lang: SourceLanguageKind,
) -> Option<ResolvedLocation> {
    if !text.contains(member) {
        return None;
    }
    if let Some(ty) = type_name {
        if !text.contains(ty) {
            return None;
        }
        // Locate the `class/struct/impl/object/interface/trait Ty` block and scan its body.
        let mut inside_target_block = false;
        let mut block_depth: isize = 0;
        for (row_idx, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            if !inside_target_block {
                let is_type_header = match lang {
                    SourceLanguageKind::Java | SourceLanguageKind::Kotlin => {
                        ["class ", "interface ", "enum ", "record ", "object "]
                            .iter()
                            .any(|kw| find_keyword_decl_in_line(line, kw, ty).is_some())
                    }
                    SourceLanguageKind::Rust => {
                        ["struct ", "enum ", "trait ", "impl "]
                            .iter()
                            .any(|kw| find_keyword_decl_in_line(line, kw, ty).is_some())
                            || trimmed.starts_with("impl") && trimmed.contains(ty)
                    }
                    SourceLanguageKind::Unknown => false,
                };
                if is_type_header {
                    inside_target_block = true;
                    let (opens, closes) = count_braces(line);
                    block_depth += opens - closes;
                }
                continue;
            }
            let (opens, closes) = count_braces(line);
            if let Some(hit) = find_callable_in_text_line(path, row_idx, line, member, lang)
                .or_else(|| find_field_or_const_in_line(path, row_idx, line, member, lang))
            {
                return Some(hit);
            }
            block_depth += opens - closes;
            if block_depth <= 0 {
                inside_target_block = false;
                block_depth = 0;
            }
        }
        return None;
    }

    find_callable_in_text(path, text, member, lang)
        .or_else(|| find_field_or_const_in_text(path, text, member, lang))
}

fn find_callable_in_text_line(
    path: &Path,
    row_idx: usize,
    line: &str,
    name: &str,
    lang: SourceLanguageKind,
) -> Option<ResolvedLocation> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*') {
        return None;
    }
    let pos = match lang {
        SourceLanguageKind::Rust => find_keyword_decl_in_line(line, "fn ", name),
        SourceLanguageKind::Kotlin => find_keyword_decl_in_line(line, "fun ", name),
        SourceLanguageKind::Java => find_java_method_or_ctor_in_line(line, name),
        SourceLanguageKind::Unknown => None,
    }?;
    let (line_no, col) = line_col_of_in_line(row_idx, line, pos);
    Some(ResolvedLocation {
        path: path.to_path_buf(),
        line: line_no,
        column: col,
        symbol_name: name.to_owned(),
    })
}

fn find_field_or_const_in_line(
    path: &Path,
    row_idx: usize,
    line: &str,
    name: &str,
    lang: SourceLanguageKind,
) -> Option<ResolvedLocation> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*') {
        return None;
    }
    let pos = match lang {
        SourceLanguageKind::Rust => find_keyword_decl_in_line(line, "const ", name)
            .or_else(|| find_keyword_decl_in_line(line, "static ", name))
            .or_else(|| {
                let s = trimmed.strip_prefix("pub ").unwrap_or(trimmed).trim_start();
                find_exact_ident_at_start(s, name)
                    .filter(|_| s[name.len()..].trim_start().starts_with(':'))
                    .map(|p| line.len() - s.len() + p)
            }),
        SourceLanguageKind::Kotlin => find_keyword_decl_in_line(line, "val ", name)
            .or_else(|| find_keyword_decl_in_line(line, "var ", name)),
        SourceLanguageKind::Java => {
            let s = trimmed
                .strip_prefix("public ")
                .or_else(|| trimmed.strip_prefix("private "))
                .or_else(|| trimmed.strip_prefix("protected "))
                .unwrap_or(trimmed)
                .trim_start();
            let s = s.strip_prefix("static ").unwrap_or(s).trim_start();
            let s = s.strip_prefix("final ").unwrap_or(s).trim_start();
            match_java_type_and_name(s, name)
                .filter(|&p| {
                    let after = s[p + name.len()..].trim_start();
                    after.starts_with('=') || after.starts_with(';')
                })
                .map(|p| line.len() - s.len() + p)
        }
        SourceLanguageKind::Unknown => None,
    }?;
    let (line_no, col) = line_col_of_in_line(row_idx, line, pos);
    Some(ResolvedLocation {
        path: path.to_path_buf(),
        line: line_no,
        column: col,
        symbol_name: name.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use kova_buffer::Buffer;

    use super::*;

    #[test]
    fn navigable_identifier_filters_keywords_comments_strings_and_numbers() {
        let text = "\
// OrderService in comment
public class Demo {
    String label = \"OrderService\";
    int count = 42;
    OrderService service = new OrderService();
}
";
        let snap = Buffer::from_text(text).snapshot();
        // Comment
        let c_byte = text.find("OrderService in comment").unwrap();
        assert_eq!(
            navigable_identifier_at_byte(&snap, c_byte, SourceLanguageKind::Java),
            None
        );
        // Keyword `public`
        let kw_byte = text.find("public class").unwrap();
        assert_eq!(
            navigable_identifier_at_byte(&snap, kw_byte, SourceLanguageKind::Java),
            None
        );
        // String literal
        let str_byte = text.find("\"OrderService\"").unwrap() + 2;
        assert_eq!(
            navigable_identifier_at_byte(&snap, str_byte, SourceLanguageKind::Java),
            None
        );
        // Number literal
        let num_byte = text.find("42").unwrap();
        assert_eq!(
            navigable_identifier_at_byte(&snap, num_byte, SourceLanguageKind::Java),
            None
        );
        // Valid identifier `OrderService`
        let id_byte = text.find("OrderService service").unwrap() + 3;
        let range = navigable_identifier_at_byte(&snap, id_byte, SourceLanguageKind::Java).unwrap();
        assert_eq!(&text[range], "OrderService");

        // Unknown language returns None
        assert_eq!(
            navigable_identifier_at_byte(&snap, id_byte, SourceLanguageKind::Unknown),
            None
        );
    }

    #[test]
    fn rust_lifetimes_do_not_hide_following_identifiers() {
        let text = "fn borrow<'a>(item: &'a Order) -> &'a Order { item }\n";
        let snap = Buffer::from_text(text).snapshot();
        let order_byte = text.find("Order)").unwrap() + 1;
        let range =
            navigable_identifier_at_byte(&snap, order_byte, SourceLanguageKind::Rust).unwrap();
        assert_eq!(&text[range], "Order");
    }

    #[test]
    fn java_same_file_and_cross_file_import_resolution() {
        let app_path = PathBuf::from("src/main/java/dev/kova/demo/OrderApp.java");
        let app_src = "\
package dev.kova.demo;

import dev.kova.orders.OrderService;

public class OrderApp {
    private final OrderService service;

    public OrderApp(OrderService service) {
        this.service = service;
    }

    public void run(String id) {
        var current = id;
        service.create(current);
    }
}
";
        let svc_path = PathBuf::from("src/main/java/dev/kova/orders/OrderService.java");
        let svc_src = "\
package dev.kova.orders;

public class OrderService {
    public void create(String id) {}
}
";

        let snap = Buffer::from_text(app_src).snapshot();
        let open_buffers = vec![
            (app_path.clone(), app_src.to_owned()),
            (svc_path.clone(), svc_src.to_owned()),
        ];
        let workspace_files = vec![app_path.clone(), svc_path.clone()];

        // 1. Same-file parameter `id`
        let id_use = app_src.find("id;").unwrap();
        let q = extract_symbol_query(&snap, &app_path, id_use).unwrap();
        let hit = resolve_local_definition(&q, app_src, &open_buffers, None, &workspace_files)
            .expect("resolve param");
        assert_eq!(hit.path, app_path);
        assert_eq!(hit.line, 12);

        // 2. Same-file local var `current`
        let cur_use = app_src.find("current);").unwrap();
        let q = extract_symbol_query(&snap, &app_path, cur_use).unwrap();
        let hit = resolve_local_definition(&q, app_src, &open_buffers, None, &workspace_files)
            .expect("resolve local var");
        assert_eq!(hit.path, app_path);
        assert_eq!(hit.line, 13);

        // 3. Cross-file class via import `OrderService`
        let type_use = app_src.find("OrderService service;").unwrap();
        let q = extract_symbol_query(&snap, &app_path, type_use).unwrap();
        let hit = resolve_local_definition(&q, app_src, &open_buffers, None, &workspace_files)
            .expect("resolve imported class");
        assert_eq!(hit.path, svc_path);
        assert_eq!(hit.line, 3);
        assert_eq!(hit.symbol_name, "OrderService");

        // 4. Cross-file method via field type inference `service.create(...)`
        let call_use = app_src.find("create(current)").unwrap();
        let q = extract_symbol_query(&snap, &app_path, call_use).unwrap();
        let hit = resolve_local_definition(&q, app_src, &open_buffers, None, &workspace_files)
            .expect("resolve method on inferred field");
        assert_eq!(hit.path, svc_path);
        assert_eq!(hit.line, 4);
        assert_eq!(hit.symbol_name, "create");

        // 5. Clicking on the import line itself jumps to the target class
        let imp_use = app_src.find("OrderService;").unwrap();
        let q = extract_symbol_query(&snap, &app_path, imp_use).unwrap();
        assert!(q.is_import_or_use);
        let hit = resolve_local_definition(&q, app_src, &open_buffers, None, &workspace_files)
            .expect("resolve from import line");
        assert_eq!(hit.path, svc_path);
        assert_eq!(hit.line, 3);
    }

    #[test]
    fn kotlin_and_rust_cross_file_resolution() {
        // Kotlin
        let kt_main = PathBuf::from("src/Main.kt");
        let kt_main_src = "\
package dev.kova

import dev.kova.model.Order

fun main() {
    val item = Order()
}
";
        let kt_order = PathBuf::from("src/dev/kova/model/Order.kt");
        let kt_order_src = "\
package dev.kova.model

data class Order(val id: Int = 1)
";
        let kt_snap = Buffer::from_text(kt_main_src).snapshot();
        let kt_byte = kt_main_src.find("Order()").unwrap();
        let kt_q = extract_symbol_query(&kt_snap, &kt_main, kt_byte).unwrap();
        let kt_hit = resolve_local_definition(
            &kt_q,
            kt_main_src,
            &[
                (kt_main.clone(), kt_main_src.to_owned()),
                (kt_order.clone(), kt_order_src.to_owned()),
            ],
            None,
            &[kt_main, kt_order.clone()],
        )
        .expect("kotlin cross-file");
        assert_eq!(kt_hit.path, kt_order);
        assert_eq!(kt_hit.line, 3);

        // Rust
        let rs_main = PathBuf::from("src/main.rs");
        let rs_main_src = "\
use crate::orders::OrderService;

fn main() {
    let svc = OrderService::new();
    svc.process();
}
";
        let rs_orders = PathBuf::from("src/orders.rs");
        let rs_orders_src = "\
pub struct OrderService;

impl OrderService {
    pub fn new() -> Self { Self }
    pub fn process(&self) {}
}
";
        let rs_snap = Buffer::from_text(rs_main_src).snapshot();
        let rs_call = rs_main_src.find("process()").unwrap();
        let rs_q = extract_symbol_query(&rs_snap, &rs_main, rs_call).unwrap();
        let rs_hit = resolve_local_definition(
            &rs_q,
            rs_main_src,
            &[
                (rs_main.clone(), rs_main_src.to_owned()),
                (rs_orders.clone(), rs_orders_src.to_owned()),
            ],
            None,
            &[rs_main, rs_orders.clone()],
        )
        .expect("rust cross-file method");
        assert_eq!(rs_hit.path, rs_orders);
        assert_eq!(rs_hit.line, 5);
        assert_eq!(rs_hit.symbol_name, "process");
    }

    #[test]
    fn level1_resolution_completes_under_15ms_across_many_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let mut files = Vec::with_capacity(200);
        for i in 0..199 {
            let rel = PathBuf::from(format!("src/main/java/dev/kova/gen/Helper{i}.java"));
            let abs = root.join(&rel);
            std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
            let mut body = String::new();
            let _ = writeln!(body, "package dev.kova.gen;\npublic class Helper{i} {{}}");
            std::fs::write(&abs, body).unwrap();
            files.push(rel);
        }
        let target_rel = PathBuf::from("src/main/java/dev/kova/orders/OrderTarget.java");
        let target_abs = root.join(&target_rel);
        std::fs::create_dir_all(target_abs.parent().unwrap()).unwrap();
        std::fs::write(
            &target_abs,
            "package dev.kova.orders;\npublic class OrderTarget {}\n",
        )
        .unwrap();
        files.push(target_rel.clone());

        let app_rel = PathBuf::from("src/main/java/dev/kova/App.java");
        let app_src = "\
package dev.kova;
import dev.kova.orders.OrderTarget;
public class App { OrderTarget t; }
";
        let snap = Buffer::from_text(app_src).snapshot();
        let pos = app_src.find("OrderTarget t").unwrap();
        let query = extract_symbol_query(&snap, &app_rel, pos).unwrap();

        let started = Instant::now();
        let hit = resolve_local_definition(&query, app_src, &[], Some(root), &files)
            .expect("must resolve target");
        let elapsed = started.elapsed();

        assert_eq!(hit.path, target_rel);
        assert_eq!(hit.line, 2);
        assert!(
            elapsed < Duration::from_millis(15),
            "Level-1 resolution took {elapsed:?} (>= 15 ms budget)"
        );
    }

    #[test]
    fn external_fqcn_candidates_cover_java_lang_imports_nested_and_receivers() {
        let src = "\
package dev.kova.app;

import java.util.Objects;
import java.util.Map;
import java.util.*;
import static java.util.Objects.requireNonNull;

public class Main {
    public void run(List<String> list) {
        String s = \"ok\";
        Objects.requireNonNull(s);
        requireNonNull(s);
        Map.Entry<String, String> entry = null;
        list.size();
        SamePkgHelper h = null;
        java.util.Objects.requireNonNull(s);
        System.out.println(s);
    }
}
";
        let path = PathBuf::from("src/Main.java");
        let snap = Buffer::from_text(src).snapshot();
        let resolve_at = |needle: &str| {
            let byte = src.find(needle).unwrap();
            let q = extract_symbol_query(&snap, &path, byte).unwrap();
            resolve_external_fqcn_candidates(&q, src)
        };

        // 1. Implicit java.lang.String
        let c_str = resolve_at("String s");
        assert!(c_str.fqcns.contains(&"java.lang.String".to_owned()));
        assert_eq!(c_str.member_name, None);

        // 2. Explicit import `Objects` on import line & in body
        let c_imp = resolve_at("Objects;");
        assert_eq!(c_imp.fqcns, vec!["java.util.Objects".to_owned()]);
        let c_obj = resolve_at("Objects.requireNonNull(s)");
        assert_eq!(c_obj.fqcns[0], "java.util.Objects");
        assert_eq!(c_obj.member_name, None);

        // 3. Qualified static method `Objects.requireNonNull`
        let c_req = resolve_at("requireNonNull(s);");
        assert_eq!(c_req.fqcns[0], "java.util.Objects");
        assert_eq!(c_req.member_name.as_deref(), Some("requireNonNull"));

        // 4. Static import usage & import line
        let c_static_imp = resolve_at("requireNonNull;\n");
        assert_eq!(c_static_imp.fqcns[0], "java.util.Objects");
        assert_eq!(c_static_imp.member_name.as_deref(), Some("requireNonNull"));
        let c_static_call = resolve_at("requireNonNull(s);\n        Map");
        assert_eq!(c_static_call.fqcns[0], "java.util.Objects");
        assert_eq!(c_static_call.member_name.as_deref(), Some("requireNonNull"));

        // 5. Nested class `Map.Entry` -> `java.util.Map$Entry`
        let c_entry = resolve_at("Entry<String");
        assert_eq!(c_entry.fqcns[0], "java.util.Map$Entry");
        assert!(c_entry.fqcns.contains(&"java.util.Map".to_owned()));

        // 6. Receiver inference `list.size()`
        let c_size = resolve_at("size()");
        assert!(c_size.fqcns.contains(&"java.util.List".to_owned()));
        assert_eq!(c_size.member_name.as_deref(), Some("size"));

        // 7. Same package + wildcard candidate ordering
        let c_pkg = resolve_at("SamePkgHelper");
        assert_eq!(c_pkg.fqcns[0], "dev.kova.app.SamePkgHelper");
        assert!(c_pkg.fqcns.contains(&"java.util.SamePkgHelper".to_owned()));
        assert!(c_pkg.fqcns.contains(&"java.lang.SamePkgHelper".to_owned()));

        // 8. Fully qualified in code `java.util.Objects.requireNonNull`
        let c_fqn_type = resolve_at("Objects.requireNonNull(s);\n        System");
        assert_eq!(c_fqn_type.fqcns, vec!["java.util.Objects".to_owned()]);
        let c_fqn_method = resolve_at("requireNonNull(s);\n        System");
        assert_eq!(c_fqn_method.fqcns, vec!["java.util.Objects".to_owned()]);
        assert_eq!(c_fqn_method.member_name.as_deref(), Some("requireNonNull"));

        // 9. Chained field access `System.out.println`
        let c_println = resolve_at("println(s)");
        assert!(c_println.fqcns.contains(&"java.lang.System".to_owned()));
        assert_eq!(c_println.field_chain, vec!["out".to_owned()]);
        assert_eq!(c_println.member_name.as_deref(), Some("println"));
    }

    #[test]
    fn external_fqcn_candidates_support_kotlin_alias_and_import_nested() {
        let kt_src = "\
package dev.kova.kt

import java.util.List as JList
import java.util.Map.Entry

fun demo(items: JList<String>, entry: Entry<String, String>) {}
";
        let kt_path = PathBuf::from("src/Demo.kt");
        let snap = Buffer::from_text(kt_src).snapshot();

        let alias_byte = kt_src.find("JList<").unwrap();
        let q_alias = extract_symbol_query(&snap, &kt_path, alias_byte).unwrap();
        let c_alias = resolve_external_fqcn_candidates(&q_alias, kt_src);
        assert_eq!(c_alias.fqcns[0], "java.util.List");

        let imp_entry_byte = kt_src.find("Entry\n").unwrap();
        let q_imp = extract_symbol_query(&snap, &kt_path, imp_entry_byte).unwrap();
        let c_imp = resolve_external_fqcn_candidates(&q_imp, kt_src);
        assert_eq!(c_imp.fqcns[0], "java.util.Map$Entry");
        assert!(c_imp.fqcns.contains(&"java.util.Map.Entry".to_owned()));
    }
}
