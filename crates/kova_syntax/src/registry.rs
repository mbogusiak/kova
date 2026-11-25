//! Language registry and path-based language detection for Tree-sitter grammars.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use kova_theme::SyntaxLanguage;

use crate::error::SyntaxError;

/// Immutable entry describing a registered Tree-sitter grammar.
#[derive(Debug, Clone)]
pub struct LanguageEntry {
    language: SyntaxLanguage,
    ts_language: tree_sitter::Language,
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

    /// Validates ABI compatibility and registers `ts_language` under `language`.
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
        let mut probe = tree_sitter::Parser::new();
        probe
            .set_language(&ts_language)
            .map_err(|err| SyntaxError::LanguageLoadFailed {
                language,
                reason: err.to_string(),
            })?;

        self.entries.insert(
            language,
            Arc::new(LanguageEntry {
                language,
                ts_language,
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
    reason = "unit tests assert valid grammar registration"
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
    }
}
