//! Real-time diagnostics store fed by `textDocument/publishDiagnostics` (KOVA-037).
//!
//! [`DiagnosticsStore`] keeps the latest diagnostics per document URI, answers cursor queries
//! for *Quick Fixes* (`Alt+Enter`), summarises error / warning counts for the status bar and
//! exports a serialisable [`DiagnosticSnapshot`] that KOVA-038 persists while `jdt.ls`
//! hibernates.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use kova_buffer::{Utf16Point, Utf16Range};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::lsp::sync::utf16_range;

/// Severity of a diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum DiagnosticSeverity {
    /// Compilation error.
    Error,
    /// Warning.
    Warning,
    /// Informational message.
    Information,
    /// Hint (usually rendered subtly).
    Hint,
}

impl DiagnosticSeverity {
    /// Maps the LSP severity; a missing severity is treated as an error.
    #[must_use]
    pub fn from_lsp(severity: Option<lsp_types::DiagnosticSeverity>) -> Self {
        match severity {
            Some(s) if s == lsp_types::DiagnosticSeverity::WARNING => Self::Warning,
            Some(s) if s == lsp_types::DiagnosticSeverity::INFORMATION => Self::Information,
            Some(s) if s == lsp_types::DiagnosticSeverity::HINT => Self::Hint,
            _ => Self::Error,
        }
    }
}

/// Extra rendering tag of a diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DiagnosticTag {
    /// Unused or unnecessary code (rendered faded).
    Unnecessary,
    /// Deprecated API (rendered struck through).
    Deprecated,
}

/// Serde proxy for [`Utf16Range`] (`kova_buffer` carries no serde dependency).
#[derive(Serialize, Deserialize)]
struct RangeProxy {
    start_row: usize,
    start_column: usize,
    end_row: usize,
    end_column: usize,
}

mod range_serde {
    use kova_buffer::{Utf16Point, Utf16Range};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::RangeProxy;

    pub(super) fn serialize<S: Serializer>(
        range: &Utf16Range,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        RangeProxy {
            start_row: range.start.row,
            start_column: range.start.column,
            end_row: range.end.row,
            end_column: range.end.column,
        }
        .serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Utf16Range, D::Error> {
        let proxy = RangeProxy::deserialize(deserializer)?;
        Ok(Utf16Range::new(
            Utf16Point::new(proxy.start_row, proxy.start_column),
            Utf16Point::new(proxy.end_row, proxy.end_column),
        ))
    }
}

/// One diagnostic of a document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticEntry {
    /// Affected range (UTF-16 columns, as sent by the server).
    #[serde(with = "range_serde")]
    pub range: Utf16Range,
    /// Severity.
    pub severity: DiagnosticSeverity,
    /// Server-specific code (`jdt.ls` sends numeric problem ids as strings or numbers).
    pub code: Option<String>,
    /// Producer, e.g. `Java`.
    pub source: Option<String>,
    /// Human readable message.
    pub message: String,
    /// Rendering tags.
    pub tags: Vec<DiagnosticTag>,
    /// The original LSP diagnostic, forwarded verbatim in `textDocument/codeAction`.
    pub raw_lsp_diagnostic: Value,
}

impl DiagnosticEntry {
    /// Builds an entry from an LSP diagnostic.
    #[must_use]
    pub fn from_lsp(diagnostic: &lsp_types::Diagnostic) -> Self {
        let code = diagnostic.code.as_ref().map(|code| match code {
            lsp_types::NumberOrString::Number(n) => n.to_string(),
            lsp_types::NumberOrString::String(s) => s.clone(),
        });
        let tags = diagnostic
            .tags
            .iter()
            .flatten()
            .filter_map(|tag| {
                if *tag == lsp_types::DiagnosticTag::UNNECESSARY {
                    Some(DiagnosticTag::Unnecessary)
                } else if *tag == lsp_types::DiagnosticTag::DEPRECATED {
                    Some(DiagnosticTag::Deprecated)
                } else {
                    None
                }
            })
            .collect();
        Self {
            range: utf16_range(diagnostic.range),
            severity: DiagnosticSeverity::from_lsp(diagnostic.severity),
            code,
            source: diagnostic.source.clone(),
            message: diagnostic.message.clone(),
            tags,
            raw_lsp_diagnostic: serde_json::to_value(diagnostic).unwrap_or(Value::Null),
        }
    }

    /// Whether `point` lies in the range, including both ends (a zero-width diagnostic or a
    /// cursor right after the offending token still matches).
    #[must_use]
    pub fn touches_point(&self, point: Utf16Point) -> bool {
        self.range.start <= point && point <= self.range.end
    }

    /// Whether the diagnostic intersects `range` (inclusive ends).
    #[must_use]
    pub fn intersects(&self, range: Utf16Range) -> bool {
        self.range.start <= range.end && range.start <= self.range.end
    }
}

/// Aggregated counts for the status bar (`jdt.ls ready · N errors`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DiagnosticsSummary {
    /// Number of [`DiagnosticSeverity::Error`] entries across all documents.
    pub total_errors: usize,
    /// Number of [`DiagnosticSeverity::Warning`] entries across all documents.
    pub total_warnings: usize,
    /// Number of documents with at least one diagnostic.
    pub files_with_diagnostics: usize,
}

/// Diagnostics of one document inside a [`DiagnosticSnapshot`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticFileSnapshot {
    /// Document URI.
    pub uri: String,
    /// Document version the diagnostics refer to, if the server sent one.
    pub version: Option<i32>,
    /// Diagnostics of the document.
    pub diagnostics: Vec<DiagnosticEntry>,
}

/// Serialisable copy of a [`DiagnosticsStore`] (persisted across `jdt.ls` hibernation).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticSnapshot {
    /// Project root the diagnostics belong to.
    pub project_root: PathBuf,
    /// Documents sorted by URI.
    pub files: Vec<DiagnosticFileSnapshot>,
}

#[derive(Debug, Default)]
struct DocumentDiagnostics {
    version: Option<i32>,
    entries: Vec<DiagnosticEntry>,
}

/// Thread-safe per-document diagnostics store.
#[derive(Debug, Default)]
pub struct DiagnosticsStore {
    documents: RwLock<HashMap<String, DocumentDiagnostics>>,
}

impl DiagnosticsStore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn read(&self) -> RwLockReadGuard<'_, HashMap<String, DocumentDiagnostics>> {
        self.documents
            .read()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, HashMap<String, DocumentDiagnostics>> {
        self.documents
            .write()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Replaces the diagnostics of `params.uri` atomically.
    ///
    /// Returns `false` (and keeps the current list) when both the stored and the incoming
    /// version are known and the incoming one is older.
    pub fn apply_publish_diagnostics(&self, params: lsp_types::PublishDiagnosticsParams) -> bool {
        let lsp_types::PublishDiagnosticsParams {
            uri,
            diagnostics,
            version,
        } = params;
        let uri = uri.as_str().to_owned();
        let entries: Vec<DiagnosticEntry> =
            diagnostics.iter().map(DiagnosticEntry::from_lsp).collect();
        let mut documents = self.write();
        let stored = documents.get(&uri).and_then(|doc| doc.version);
        if matches!((stored, version), (Some(current), Some(incoming)) if incoming < current) {
            return false;
        }
        documents.insert(uri, DocumentDiagnostics { version, entries });
        true
    }

    /// Diagnostics of `uri` (empty when unknown).
    #[must_use]
    pub fn diagnostics_for_uri(&self, uri: &str) -> Vec<DiagnosticEntry> {
        self.read()
            .get(uri)
            .map(|doc| doc.entries.clone())
            .unwrap_or_default()
    }

    /// Diagnostics of `uri` covering `pos` (input of *Quick Fix* `Alt+Enter`).
    #[must_use]
    pub fn diagnostics_at(&self, uri: &str, pos: Utf16Point) -> Vec<DiagnosticEntry> {
        self.read()
            .get(uri)
            .map(|doc| {
                doc.entries
                    .iter()
                    .filter(|entry| entry.touches_point(pos))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Diagnostics of `uri` intersecting `range`.
    #[must_use]
    pub fn diagnostics_in_range(&self, uri: &str, range: Utf16Range) -> Vec<DiagnosticEntry> {
        self.read()
            .get(uri)
            .map(|doc| {
                doc.entries
                    .iter()
                    .filter(|entry| entry.intersects(range))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Error / warning counts across all documents.
    #[must_use]
    pub fn summary(&self) -> DiagnosticsSummary {
        let documents = self.read();
        let mut summary = DiagnosticsSummary::default();
        for doc in documents.values().filter(|doc| !doc.entries.is_empty()) {
            summary.files_with_diagnostics += 1;
            for entry in &doc.entries {
                match entry.severity {
                    DiagnosticSeverity::Error => summary.total_errors += 1,
                    DiagnosticSeverity::Warning => summary.total_warnings += 1,
                    DiagnosticSeverity::Information | DiagnosticSeverity::Hint => {}
                }
            }
        }
        summary
    }

    /// Exports every document (sorted by URI) for persistence.
    #[must_use]
    pub fn export_snapshot(&self, project_root: &Path) -> DiagnosticSnapshot {
        let documents = self.read();
        let sorted: BTreeMap<&String, &DocumentDiagnostics> = documents.iter().collect();
        DiagnosticSnapshot {
            project_root: project_root.to_path_buf(),
            files: sorted
                .into_iter()
                .map(|(uri, doc)| DiagnosticFileSnapshot {
                    uri: uri.clone(),
                    version: doc.version,
                    diagnostics: doc.entries.clone(),
                })
                .collect(),
        }
    }

    /// Replaces the whole store with `snapshot`.
    ///
    /// Versions are not restored: a freshly started server numbers documents from scratch and
    /// its first publication must never be rejected as stale.
    pub fn restore_snapshot(&self, snapshot: DiagnosticSnapshot) {
        let mut documents = self.write();
        documents.clear();
        for file in snapshot.files {
            documents.insert(
                file.uri,
                DocumentDiagnostics {
                    version: None,
                    entries: file.diagnostics,
                },
            );
        }
    }

    /// Forgets the diagnostics (and version) of `uri`.
    pub fn clear_uri(&self, uri: &str) {
        self.write().remove(uri);
    }

    /// Forgets every document.
    pub fn clear_all(&self) {
        self.write().clear();
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use lsp_types::{Diagnostic, Position, PublishDiagnosticsParams, Range, Uri};
    use serde_json::json;

    use super::*;

    const URI: &str = "file:///p/src/A.java";

    fn diag(
        line: u32,
        start: u32,
        end: u32,
        severity: lsp_types::DiagnosticSeverity,
    ) -> Diagnostic {
        Diagnostic {
            range: Range::new(Position::new(line, start), Position::new(line, end)),
            severity: Some(severity),
            code: Some(lsp_types::NumberOrString::Number(16_777_218)),
            source: Some("Java".to_owned()),
            message: format!("problem at {line}:{start}"),
            tags: Some(vec![lsp_types::DiagnosticTag::DEPRECATED]),
            ..Diagnostic::default()
        }
    }

    fn publish(version: Option<i32>, diagnostics: Vec<Diagnostic>) -> PublishDiagnosticsParams {
        PublishDiagnosticsParams {
            uri: Uri::from_str(URI).unwrap_or_else(|_| unreachable!()),
            diagnostics,
            version,
        }
    }

    #[test]
    fn maps_lsp_fields() {
        let entry =
            DiagnosticEntry::from_lsp(&diag(3, 4, 9, lsp_types::DiagnosticSeverity::WARNING));
        assert_eq!(entry.severity, DiagnosticSeverity::Warning);
        assert_eq!(entry.code.as_deref(), Some("16777218"));
        assert_eq!(entry.tags, vec![DiagnosticTag::Deprecated]);
        assert_eq!(
            entry.range,
            Utf16Range::new(Utf16Point::new(3, 4), Utf16Point::new(3, 9))
        );
        assert_eq!(entry.raw_lsp_diagnostic["source"], json!("Java"));
    }

    #[test]
    fn ignores_stale_versions_and_matches_zero_width_ranges() {
        let store = DiagnosticsStore::new();
        assert!(store.apply_publish_diagnostics(publish(
            Some(5),
            vec![diag(1, 2, 2, lsp_types::DiagnosticSeverity::ERROR)],
        )));
        assert!(!store.apply_publish_diagnostics(publish(Some(4), vec![])));
        assert_eq!(store.diagnostics_for_uri(URI).len(), 1);
        assert_eq!(store.diagnostics_at(URI, Utf16Point::new(1, 2)).len(), 1);
        assert_eq!(store.diagnostics_at(URI, Utf16Point::new(1, 3)), Vec::new());
        // Unversioned publications always apply.
        assert!(store.apply_publish_diagnostics(publish(None, vec![])));
        assert_eq!(store.summary(), DiagnosticsSummary::default());
        store.clear_uri(URI);
        assert!(store.apply_publish_diagnostics(publish(Some(1), vec![])));
    }

    #[test]
    fn snapshot_round_trips_through_json() {
        let store = DiagnosticsStore::new();
        store.apply_publish_diagnostics(publish(
            Some(2),
            vec![
                diag(0, 0, 3, lsp_types::DiagnosticSeverity::ERROR),
                diag(2, 1, 4, lsp_types::DiagnosticSeverity::WARNING),
            ],
        ));
        let snapshot = store.export_snapshot(Path::new("/p"));
        let json = serde_json::to_string(&snapshot).unwrap_or_default();
        let back: DiagnosticSnapshot =
            serde_json::from_str(&json).unwrap_or_else(|_| unreachable!());
        assert_eq!(back, snapshot);

        let restored = DiagnosticsStore::new();
        restored.restore_snapshot(back);
        assert_eq!(restored.summary(), store.summary());
        assert_eq!(
            restored.diagnostics_for_uri(URI),
            store.diagnostics_for_uri(URI)
        );
        // The restored store accepts a fresh server's first (low) version.
        assert!(restored.apply_publish_diagnostics(publish(Some(1), vec![])));
        restored.clear_all();
        assert_eq!(restored.summary().files_with_diagnostics, 0);
    }
}
