//! Language registry and path-based language detection for Tree-sitter grammars.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use kova_theme::{SyntaxLanguage, SyntaxTokenKind, resolve_capture};

use crate::error::SyntaxError;

const JAVA_HIGHLIGHTS_SCM: &str = include_str!("../queries/java/highlights.scm");
const KOTLIN_HIGHLIGHTS_SCM: &str = include_str!("../queries/kotlin/highlights.scm");
const HTTP_HIGHLIGHTS_SCM: &str = include_str!("../queries/http/highlights.scm");
const TOML_HIGHLIGHTS_SCM: &str = include_str!("../queries/toml/highlights.scm");
const JSON_HIGHLIGHTS_SCM: &str = include_str!("../queries/json/highlights.scm");

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

/// Immutable entry describing a registered Tree-sitter grammar and its optional highlight query.
#[derive(Debug, Clone)]
pub struct LanguageEntry {
    language: SyntaxLanguage,
    ts_language: tree_sitter::Language,
    highlight_query: Option<Arc<HighlightQuery>>,
}

impl LanguageEntry {
    /// Returns the canonical [`SyntaxLanguage`] identifier.
    #[must_use]
    pub const fn language(&self) -> SyntaxLanguage {
        self.language
    }

    /// Returns the underlying Tree-sitter [`tree_sitter::Language`].
    #[must_use]
    pub const fn ts_language(&self) -> &tree_sitter::Language {
        &self.ts_language
    }

    /// Returns the precompiled [`HighlightQuery`] for this language, if registered.
    #[must_use]
    pub fn highlight_query(&self) -> Option<&HighlightQuery> {
        self.highlight_query.as_deref()
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

    /// Creates a [`LanguageRegistry`] pre-populated with all 5 built-in grammars
    /// (`Java`, `Kotlin`, `Http`, `Toml`, `Json`) and their compiled `highlights.scm` queries.
    ///
    /// Prefer [`Self::builtin`] on hot paths to reuse the process-wide cached registry.
    ///
    /// # Errors
    ///
    /// Returns [`SyntaxError::LanguageLoadFailed`] or [`SyntaxError::InvalidQuery`] if any
    /// built-in grammar or highlight query fails validation.
    pub fn with_defaults() -> Result<Self, SyntaxError> {
        let mut registry = Self::new();
        registry.register_with_highlights(
            SyntaxLanguage::Java,
            tree_sitter_java::LANGUAGE.into(),
            JAVA_HIGHLIGHTS_SCM,
        )?;
        registry.register_with_highlights(
            SyntaxLanguage::Kotlin,
            tree_sitter_kotlin_ng::LANGUAGE.into(),
            KOTLIN_HIGHLIGHTS_SCM,
        )?;
        registry.register_with_highlights(
            SyntaxLanguage::Http,
            tree_sitter_http::language(),
            HTTP_HIGHLIGHTS_SCM,
        )?;
        registry.register_with_highlights(
            SyntaxLanguage::Toml,
            tree_sitter_toml_ng::LANGUAGE.into(),
            TOML_HIGHLIGHTS_SCM,
        )?;
        registry.register_with_highlights(
            SyntaxLanguage::Json,
            tree_sitter_json::LANGUAGE.into(),
            JSON_HIGHLIGHTS_SCM,
        )?;
        Ok(registry)
    }

    /// Returns a process-wide cached [`LanguageRegistry`] containing all 5 built-in grammars
    /// and compiled highlight queries, compiling them at most once per process.
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

    /// Validates ABI compatibility and registers `ts_language` under `language` without a
    /// highlight query.
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
        self.register_entry(language, ts_language, None)
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
        self.entries.insert(
            language,
            Arc::new(LanguageEntry {
                language,
                ts_language,
                highlight_query: Some(Arc::new(query)),
            }),
        );
        Ok(())
    }

    fn register_entry(
        &mut self,
        language: SyntaxLanguage,
        ts_language: tree_sitter::Language,
        highlight_query: Option<Arc<HighlightQuery>>,
    ) -> Result<(), SyntaxError> {
        validate_language_abi(language, &ts_language)?;
        self.entries.insert(
            language,
            Arc::new(LanguageEntry {
                language,
                ts_language,
                highlight_query,
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
/// Recognizes `.java`, `.kt`, `.kts`, `.http`, `.toml` (including `Cargo.toml`, `settings.toml`,
/// `kova.toml`), and `.json`.
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
    fn builtin_registry_returns_all_five_languages() {
        let registry = LanguageRegistry::builtin().unwrap();
        assert_eq!(registry.len(), 5);
        for lang in [
            SyntaxLanguage::Java,
            SyntaxLanguage::Kotlin,
            SyntaxLanguage::Http,
            SyntaxLanguage::Toml,
            SyntaxLanguage::Json,
        ] {
            let entry = registry.get(lang).unwrap();
            assert!(entry.highlight_query().is_some());
        }
    }
}
