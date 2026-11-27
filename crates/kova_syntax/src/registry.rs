//! Language registry and path-based language detection for Tree-sitter grammars.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use kova_theme::{SyntaxLanguage, SyntaxTokenKind, resolve_capture};

use crate::error::SyntaxError;
use crate::folding::FoldKind;

const JAVA_HIGHLIGHTS_SCM: &str = include_str!("../queries/java/highlights.scm");
const KOTLIN_HIGHLIGHTS_SCM: &str = include_str!("../queries/kotlin/highlights.scm");
const HTTP_HIGHLIGHTS_SCM: &str = include_str!("../queries/http/highlights.scm");
const TOML_HIGHLIGHTS_SCM: &str = include_str!("../queries/toml/highlights.scm");
const JSON_HIGHLIGHTS_SCM: &str = include_str!("../queries/json/highlights.scm");

const JAVA_FOLDS_SCM: &str = include_str!("../queries/java/folds.scm");
const KOTLIN_FOLDS_SCM: &str = include_str!("../queries/kotlin/folds.scm");
const HTTP_FOLDS_SCM: &str = include_str!("../queries/http/folds.scm");
const TOML_FOLDS_SCM: &str = include_str!("../queries/toml/folds.scm");
const JSON_FOLDS_SCM: &str = include_str!("../queries/json/folds.scm");

const JAVA_ENTRY_POINTS_SCM: &str = include_str!("../queries/java/entry_points.scm");
const KOTLIN_ENTRY_POINTS_SCM: &str = include_str!("../queries/kotlin/entry_points.scm");
const HTTP_ENTRY_POINTS_SCM: &str = include_str!("../queries/http/entry_points.scm");

static BUILTIN_REGISTRY: OnceLock<Result<LanguageRegistry, SyntaxError>> = OnceLock::new();

/// Precompiled Tree-sitter highlight query with $O(1)$ capture-to-token lookup.
#[derive(Debug)]
pub struct HighlightQuery {
    query: tree_sitter::Query,
    capture_kinds: Vec<Option<SyntaxTokenKind>>,
}

impl HighlightQuery {
    /// Compiles `source` for `ts_language` and resolves every capture name once via
    /// [`kova_theme::resolve_capture`].
    ///
    /// # Errors
    ///
    /// Returns [`SyntaxError::InvalidQuery`] if Tree-sitter rejects `source`.
    pub fn compile(
        language: SyntaxLanguage,
        ts_language: &tree_sitter::Language,
        source: &str,
    ) -> Result<Self, SyntaxError> {
        let query = tree_sitter::Query::new(ts_language, source).map_err(|err| {
            SyntaxError::InvalidQuery {
                language,
                kind: "highlights",
                message: err.to_string(),
            }
        })?;

        let capture_kinds = query
            .capture_names()
            .iter()
            .map(|name| resolve_capture(language, name))
            .collect();

        Ok(Self {
            query,
            capture_kinds,
        })
    }

    /// Returns the underlying compiled [`tree_sitter::Query`].
    #[must_use]
    pub const fn query(&self) -> &tree_sitter::Query {
        &self.query
    }

    /// Returns the precompiled [`SyntaxTokenKind`] for `capture_index` in $O(1)$ time,
    /// or `None` for internal `_`-prefixed or unmapped captures.
    #[must_use]
    pub fn capture_kind(&self, capture_index: u32) -> Option<SyntaxTokenKind> {
        self.capture_kinds
            .get(capture_index as usize)
            .copied()
            .flatten()
    }

    /// Returns the precomputed capture-index-to-[`SyntaxTokenKind`] lookup table.
    #[must_use]
    pub fn capture_kinds(&self) -> &[Option<SyntaxTokenKind>] {
        &self.capture_kinds
    }

    /// Returns any non-internal capture names (`!name.starts_with('_')`) that failed to map
    /// to a [`SyntaxTokenKind`].
    #[must_use]
    pub fn unmapped_captures(&self) -> Vec<&str> {
        self.query
            .capture_names()
            .iter()
            .zip(&self.capture_kinds)
            .filter_map(|(&name, kind)| {
                if !name.starts_with('_') && kind.is_none() {
                    Some(name)
                } else {
                    None
                }
            })
            .collect()
    }
}

/// Precompiled Tree-sitter fold query with $O(1)$ capture-to-[`FoldKind`] lookup.
#[derive(Debug)]
pub struct FoldQuery {
    query: tree_sitter::Query,
    capture_kinds: Vec<Option<FoldKind>>,
}

impl FoldQuery {
    /// Compiles `source` for `ts_language` and resolves every `@fold.*` capture name once.
    ///
    /// # Errors
    ///
    /// Returns [`SyntaxError::InvalidQuery`] if Tree-sitter rejects `source`.
    pub fn compile(
        language: SyntaxLanguage,
        ts_language: &tree_sitter::Language,
        source: &str,
    ) -> Result<Self, SyntaxError> {
        let query = tree_sitter::Query::new(ts_language, source).map_err(|err| {
            SyntaxError::InvalidQuery {
                language,
                kind: "folds",
                message: err.to_string(),
            }
        })?;

        let capture_kinds = query
            .capture_names()
            .iter()
            .map(|name| resolve_fold_capture(name))
            .collect();

        Ok(Self {
            query,
            capture_kinds,
        })
    }

    /// Returns the underlying compiled [`tree_sitter::Query`].
    #[must_use]
    pub const fn query(&self) -> &tree_sitter::Query {
        &self.query
    }

    /// Returns the precompiled [`FoldKind`] for `capture_index` in $O(1)$ time,
    /// or `None` for internal `_`-prefixed or unrecognized captures.
    #[must_use]
    pub fn capture_kind(&self, capture_index: u32) -> Option<FoldKind> {
        self.capture_kinds
            .get(capture_index as usize)
            .copied()
            .flatten()
    }
}

fn resolve_fold_capture(name: &str) -> Option<FoldKind> {
    match name {
        "fold.type" => Some(FoldKind::Type),
        "fold.function" => Some(FoldKind::Function),
        "fold.block" => Some(FoldKind::Block),
        "fold.comment" => Some(FoldKind::Comment),
        "fold.request" => Some(FoldKind::Request),
        "fold.imports" => Some(FoldKind::Imports),
        "fold.region" => Some(FoldKind::Region),
        _ => None,
    }
}

/// Immutable entry describing a registered language backend (Tree-sitter grammar or built-in scanner).
#[derive(Debug, Clone)]
pub struct LanguageEntry {
    language: SyntaxLanguage,
    ts_language: Option<tree_sitter::Language>,
    highlight_query: Option<Arc<HighlightQuery>>,
    fold_query: Option<Arc<FoldQuery>>,
    entry_points_query: Option<Arc<tree_sitter::Query>>,
}

impl LanguageEntry {
    /// Returns the canonical [`SyntaxLanguage`] identifier.
    #[must_use]
    pub const fn language(&self) -> SyntaxLanguage {
        self.language
    }

    /// Returns the underlying Tree-sitter [`tree_sitter::Language`], or `None` for scanner-backed languages.
    #[must_use]
    pub const fn ts_language(&self) -> Option<&tree_sitter::Language> {
        self.ts_language.as_ref()
    }

    /// Returns `true` when this entry uses a Tree-sitter grammar backend rather than the built-in scanner.
    #[must_use]
    pub const fn is_tree_sitter(&self) -> bool {
        self.ts_language.is_some()
    }

    /// Returns the precompiled [`HighlightQuery`] for this language, if registered.
    #[must_use]
    pub fn highlight_query(&self) -> Option<&HighlightQuery> {
        self.highlight_query.as_deref()
    }

    /// Returns the precompiled [`FoldQuery`] for this language, if registered.
    #[must_use]
    pub fn fold_query(&self) -> Option<&FoldQuery> {
        self.fold_query.as_deref()
    }

    /// Returns the precompiled entry-point [`tree_sitter::Query`] for this language, if registered.
    #[must_use]
    pub fn entry_points_query(&self) -> Option<&tree_sitter::Query> {
        self.entry_points_query.as_deref()
    }
}

/// Thread-safe, cheaply cloneable registry mapping [`SyntaxLanguage`] to Tree-sitter grammars.
#[derive(Debug, Clone, Default)]
pub struct LanguageRegistry {
    entries: HashMap<SyntaxLanguage, Arc<LanguageEntry>>,
}

impl LanguageRegistry {
    /// Creates an empty [`LanguageRegistry`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a [`LanguageRegistry`] pre-populated with all 9 built-in languages
    /// (`Java`, `Kotlin`, `Http`, `Toml`, `Json`, `Yaml`, `Xml`, `Groovy`, `Sql`), including compiled
    /// `highlights.scm` and `folds.scm` queries for Tree-sitter grammars, entry-point queries
    /// (`entry_points.scm`) for `Java`, `Kotlin`, and `Http`, and built-in scanners for
    /// `Yaml`, `Xml`, `Groovy`, and `Sql`.
    ///
    /// Prefer [`Self::builtin`] on hot paths to reuse the process-wide cached registry.
    ///
    /// # Errors
    ///
    /// Returns [`SyntaxError::LanguageLoadFailed`] or [`SyntaxError::InvalidQuery`] if any
    /// built-in grammar or query fails validation.
    pub fn with_defaults() -> Result<Self, SyntaxError> {
        let mut registry = Self::new();
        registry.register_with_all_queries(
            SyntaxLanguage::Java,
            tree_sitter_java::LANGUAGE.into(),
            JAVA_HIGHLIGHTS_SCM,
            JAVA_FOLDS_SCM,
            Some(JAVA_ENTRY_POINTS_SCM),
        )?;
        registry.register_with_all_queries(
            SyntaxLanguage::Kotlin,
            tree_sitter_kotlin_ng::LANGUAGE.into(),
            KOTLIN_HIGHLIGHTS_SCM,
            KOTLIN_FOLDS_SCM,
            Some(KOTLIN_ENTRY_POINTS_SCM),
        )?;
        registry.register_with_all_queries(
            SyntaxLanguage::Http,
            tree_sitter_http::language(),
            HTTP_HIGHLIGHTS_SCM,
            HTTP_FOLDS_SCM,
            Some(HTTP_ENTRY_POINTS_SCM),
        )?;
        registry.register_with_all_queries(
            SyntaxLanguage::Toml,
            tree_sitter_toml_ng::LANGUAGE.into(),
            TOML_HIGHLIGHTS_SCM,
            TOML_FOLDS_SCM,
            None,
        )?;
        registry.register_with_all_queries(
            SyntaxLanguage::Json,
            tree_sitter_json::LANGUAGE.into(),
            JSON_HIGHLIGHTS_SCM,
            JSON_FOLDS_SCM,
            None,
        )?;
        registry.register_scanner(SyntaxLanguage::Yaml);
        registry.register_scanner(SyntaxLanguage::Xml);
        registry.register_scanner(SyntaxLanguage::Groovy);
        registry.register_scanner(SyntaxLanguage::Sql);
        Ok(registry)
    }

    /// Returns a process-wide cached [`LanguageRegistry`] containing all 9 built-in languages
    /// and compiled highlight, fold, and entry-point queries, compiling them at most once per
    /// process.
    ///
    /// # Errors
    ///
    /// Returns [`SyntaxError`] if initializing the built-in registry failed.
    pub fn builtin() -> Result<&'static Self, SyntaxError> {
        BUILTIN_REGISTRY
            .get_or_init(Self::with_defaults)
            .as_ref()
            .map_err(Clone::clone)
    }

    /// Registers a scanner-backed language (`Yaml`, `Xml`, `Groovy`, or `Sql`) in this registry.
    pub fn register_scanner(&mut self, language: SyntaxLanguage) {
        self.entries.insert(
            language,
            Arc::new(LanguageEntry {
                language,
                ts_language: None,
                highlight_query: None,
                fold_query: None,
                entry_points_query: None,
            }),
        );
    }

    /// Validates ABI compatibility and registers `ts_language` under `language` without
    /// queries.
    ///
    /// Replacing an existing registration overwrites the previous entry.
    ///
    /// # Errors
    ///
    /// Returns [`SyntaxError::LanguageLoadFailed`] if Tree-sitter rejects the grammar ABI.
    pub fn register(
        &mut self,
        language: SyntaxLanguage,
        ts_language: tree_sitter::Language,
    ) -> Result<(), SyntaxError> {
        self.register_entry(language, ts_language, None, None, None)
    }

    /// Validates ABI compatibility, compiles `highlights_scm`, and registers both under
    /// `language`.
    ///
    /// Replacing an existing registration overwrites the previous entry.
    ///
    /// # Errors
    ///
    /// Returns [`SyntaxError::LanguageLoadFailed`] if Tree-sitter rejects the grammar ABI, or
    /// [`SyntaxError::InvalidQuery`] if `highlights_scm` fails to compile.
    pub fn register_with_highlights(
        &mut self,
        language: SyntaxLanguage,
        ts_language: tree_sitter::Language,
        highlights_scm: &str,
    ) -> Result<(), SyntaxError> {
        validate_language_abi(language, &ts_language)?;
        let query = HighlightQuery::compile(language, &ts_language, highlights_scm)?;
        self.register_entry(language, ts_language, Some(Arc::new(query)), None, None)
    }

    /// Validates ABI compatibility, compiles `highlights_scm` and `folds_scm`, and registers
    /// them under `language`.
    ///
    /// Replacing an existing registration overwrites the previous entry.
    ///
    /// # Errors
    ///
    /// Returns [`SyntaxError::LanguageLoadFailed`] if Tree-sitter rejects the grammar ABI, or
    /// [`SyntaxError::InvalidQuery`] if either query fails to compile.
    pub fn register_with_queries(
        &mut self,
        language: SyntaxLanguage,
        ts_language: tree_sitter::Language,
        highlights_scm: &str,
        folds_scm: &str,
    ) -> Result<(), SyntaxError> {
        self.register_with_all_queries(language, ts_language, highlights_scm, folds_scm, None)
    }

    /// Validates ABI compatibility, compiles `highlights_scm`, `folds_scm`, and an optional
    /// `entry_points_scm`, and registers them under `language`.
    ///
    /// Replacing an existing registration overwrites the previous entry.
    ///
    /// # Errors
    ///
    /// Returns [`SyntaxError::LanguageLoadFailed`] if Tree-sitter rejects the grammar ABI, or
    /// [`SyntaxError::InvalidQuery`] if any query fails to compile.
    pub fn register_with_all_queries(
        &mut self,
        language: SyntaxLanguage,
        ts_language: tree_sitter::Language,
        highlights_scm: &str,
        folds_scm: &str,
        entry_points_scm: Option<&str>,
    ) -> Result<(), SyntaxError> {
        validate_language_abi(language, &ts_language)?;
        let highlight_query = HighlightQuery::compile(language, &ts_language, highlights_scm)?;
        let fold_query = FoldQuery::compile(language, &ts_language, folds_scm)?;
        let entry_points_query = match entry_points_scm {
            Some(src) => Some(Arc::new(
                tree_sitter::Query::new(&ts_language, src).map_err(|err| {
                    SyntaxError::InvalidQuery {
                        language,
                        kind: "entry_points",
                        message: err.to_string(),
                    }
                })?,
            )),
            None => None,
        };
        self.register_entry(
            language,
            ts_language,
            Some(Arc::new(highlight_query)),
            Some(Arc::new(fold_query)),
            entry_points_query,
        )
    }

    fn register_entry(
        &mut self,
        language: SyntaxLanguage,
        ts_language: tree_sitter::Language,
        highlight_query: Option<Arc<HighlightQuery>>,
        fold_query: Option<Arc<FoldQuery>>,
        entry_points_query: Option<Arc<tree_sitter::Query>>,
    ) -> Result<(), SyntaxError> {
        validate_language_abi(language, &ts_language)?;
        self.entries.insert(
            language,
            Arc::new(LanguageEntry {
                language,
                ts_language: Some(ts_language),
                highlight_query,
                fold_query,
                entry_points_query,
            }),
        );
        Ok(())
    }

    /// Returns the registered [`LanguageEntry`] for `language`, if present.
    #[must_use]
    pub fn get(&self, language: SyntaxLanguage) -> Option<&LanguageEntry> {
        self.entries.get(&language).map(Arc::as_ref)
    }

    /// Returns a cloned [`Arc<LanguageEntry>`] for sharing across worker threads.
    #[must_use]
    pub(crate) fn get_entry(&self, language: SyntaxLanguage) -> Option<Arc<LanguageEntry>> {
        self.entries.get(&language).map(Arc::clone)
    }

    /// Returns `true` if `language` has been registered.
    #[must_use]
    pub fn contains(&self, language: SyntaxLanguage) -> bool {
        self.entries.contains_key(&language)
    }

    /// Returns the number of registered languages.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` if no languages are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn validate_language_abi(
    language: SyntaxLanguage,
    ts_language: &tree_sitter::Language,
) -> Result<(), SyntaxError> {
    let mut probe = tree_sitter::Parser::new();
    probe
        .set_language(ts_language)
        .map_err(|err| SyntaxError::LanguageLoadFailed {
            language,
            reason: err.to_string(),
        })
}

/// Detects the canonical [`SyntaxLanguage`] for a file path using ASCII case-insensitive extension matching.
///
/// Recognizes `.java`, `.kt`, `.kts` (including `.gradle.kts`), `.http`, `.toml` (including
/// `Cargo.toml`, `settings.toml`, `kova.toml`), `.json`, `.yaml`/`.yml`, `.xml`/`.pom`/`.xsd`,
/// `.groovy`/`.gradle`, and `.sql`/`.ddl`/`.psql`/`.hql`.
#[must_use]
pub fn detect_language(path: &Path) -> Option<SyntaxLanguage> {
    let ext = path.extension()?.to_str()?;
    if ext.eq_ignore_ascii_case("java") {
        Some(SyntaxLanguage::Java)
    } else if ext.eq_ignore_ascii_case("kt") || ext.eq_ignore_ascii_case("kts") {
        Some(SyntaxLanguage::Kotlin)
    } else if ext.eq_ignore_ascii_case("http") {
        Some(SyntaxLanguage::Http)
    } else if ext.eq_ignore_ascii_case("toml") {
        Some(SyntaxLanguage::Toml)
    } else if ext.eq_ignore_ascii_case("json") {
        Some(SyntaxLanguage::Json)
    } else if ext.eq_ignore_ascii_case("yaml") || ext.eq_ignore_ascii_case("yml") {
        Some(SyntaxLanguage::Yaml)
    } else if ext.eq_ignore_ascii_case("xml")
        || ext.eq_ignore_ascii_case("pom")
        || ext.eq_ignore_ascii_case("xsd")
    {
        Some(SyntaxLanguage::Xml)
    } else if ext.eq_ignore_ascii_case("groovy") || ext.eq_ignore_ascii_case("gradle") {
        Some(SyntaxLanguage::Groovy)
    } else if ext.eq_ignore_ascii_case("sql")
        || ext.eq_ignore_ascii_case("ddl")
        || ext.eq_ignore_ascii_case("psql")
        || ext.eq_ignore_ascii_case("hql")
    {
        Some(SyntaxLanguage::Sql)
    } else {
        None
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "unit tests assert valid grammar and query registration"
)]
mod tests {
    use super::*;

    #[test]
    fn detect_language_handles_supported_extensions_and_case() {
        assert_eq!(
            detect_language(Path::new("src/Main.java")),
            Some(SyntaxLanguage::Java)
        );
        assert_eq!(
            detect_language(Path::new("App.JAVA")),
            Some(SyntaxLanguage::Java)
        );
        assert_eq!(
            detect_language(Path::new("Service.kt")),
            Some(SyntaxLanguage::Kotlin)
        );
        assert_eq!(
            detect_language(Path::new("build.gradle.kts")),
            Some(SyntaxLanguage::Kotlin)
        );
        assert_eq!(
            detect_language(Path::new("requests.HTTP")),
            Some(SyntaxLanguage::Http)
        );
        assert_eq!(
            detect_language(Path::new("Cargo.toml")),
            Some(SyntaxLanguage::Toml)
        );
        assert_eq!(
            detect_language(Path::new("kova.toml")),
            Some(SyntaxLanguage::Toml)
        );
        assert_eq!(
            detect_language(Path::new("app.config.json")),
            Some(SyntaxLanguage::Json)
        );
        assert_eq!(
            detect_language(Path::new("application.yaml")),
            Some(SyntaxLanguage::Yaml)
        );
        assert_eq!(
            detect_language(Path::new("docker-compose.YML")),
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
            detect_language(Path::new("schema.xsd")),
            Some(SyntaxLanguage::Xml)
        );
        assert_eq!(
            detect_language(Path::new("Script.groovy")),
            Some(SyntaxLanguage::Groovy)
        );
        assert_eq!(
            detect_language(Path::new("build.GRADLE")),
            Some(SyntaxLanguage::Groovy)
        );
        assert_eq!(
            detect_language(Path::new("schema.sql")),
            Some(SyntaxLanguage::Sql)
        );
        assert_eq!(
            detect_language(Path::new("migrations.DDL")),
            Some(SyntaxLanguage::Sql)
        );
        assert_eq!(
            detect_language(Path::new("queries.psql")),
            Some(SyntaxLanguage::Sql)
        );
        assert_eq!(
            detect_language(Path::new("analytics.HQL")),
            Some(SyntaxLanguage::Sql)
        );
        assert_eq!(detect_language(Path::new("README")), None);
        assert_eq!(detect_language(Path::new("script.py")), None);
        assert_eq!(detect_language(Path::new("api.rest")), None);
    }

    #[test]
    fn registry_registers_and_queries_json_grammar() {
        let mut registry = LanguageRegistry::new();
        assert!(registry.is_empty());
        assert!(!registry.contains(SyntaxLanguage::Json));

        registry
            .register(SyntaxLanguage::Json, tree_sitter_json::LANGUAGE.into())
            .unwrap();

        assert_eq!(registry.len(), 1);
        assert!(registry.contains(SyntaxLanguage::Json));
        let entry = registry.get(SyntaxLanguage::Json).unwrap();
        assert_eq!(entry.language(), SyntaxLanguage::Json);
        assert!(entry.highlight_query().is_none());
    }

    #[test]
    fn invalid_query_returns_syntax_error_without_panicking() {
        let mut registry = LanguageRegistry::new();
        let err = registry
            .register_with_highlights(
                SyntaxLanguage::Json,
                tree_sitter_json::LANGUAGE.into(),
                "(non_existent_node_type) @string",
            )
            .unwrap_err();
        assert!(matches!(
            err,
            SyntaxError::InvalidQuery {
                language: SyntaxLanguage::Json,
                kind: "highlights",
                ..
            }
        ));
    }

    #[test]
    fn builtin_registry_returns_all_nine_languages() {
        let registry = LanguageRegistry::builtin().unwrap();
        assert_eq!(registry.len(), 9);
        for lang in [
            SyntaxLanguage::Java,
            SyntaxLanguage::Kotlin,
            SyntaxLanguage::Http,
            SyntaxLanguage::Toml,
            SyntaxLanguage::Json,
        ] {
            let entry = registry.get(lang).unwrap();
            assert!(entry.is_tree_sitter());
            assert!(entry.ts_language().is_some());
            assert!(entry.highlight_query().is_some());
            let expects_entry_points = matches!(
                lang,
                SyntaxLanguage::Java | SyntaxLanguage::Kotlin | SyntaxLanguage::Http
            );
            assert_eq!(entry.entry_points_query().is_some(), expects_entry_points);
        }
        for lang in [
            SyntaxLanguage::Yaml,
            SyntaxLanguage::Xml,
            SyntaxLanguage::Groovy,
            SyntaxLanguage::Sql,
        ] {
            let entry = registry.get(lang).unwrap();
            assert!(!entry.is_tree_sitter());
            assert!(entry.ts_language().is_none());
            assert!(entry.highlight_query().is_none());
            assert!(entry.entry_points_query().is_none());
        }
    }
}
