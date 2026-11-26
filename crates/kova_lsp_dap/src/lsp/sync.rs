//! `textDocument/*` synchronisation driven by `kova_buffer`'s [`EditLog`].
//!
//! [`DocumentSyncTracker`] remembers, per open document, the last [`BufferVersion`] the server
//! has seen and maps [`VersionChanges`] 1:1 to `didChange` notifications: UTF-16 deltas when the
//! history window still covers the gap, a single full-text change otherwise.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use kova_buffer::{
    BufferId, BufferSnapshot, BufferVersion, EditLog, Utf16Point, Utf16Range, VersionChanges,
};
use lsp_types::{Position, Range, TextDocumentContentChangeEvent, Uri};
use serde_json::json;
use url::Url;

use crate::error::LspDapError;
use crate::lsp::lifecycle::JdtlsClient;

/// Converts an absolute filesystem path to a percent-encoded `file://` [`Uri`].
///
/// # Errors
/// [`LspDapError::InvalidUri`] for relative paths or unparsable results.
pub fn path_to_lsp_uri(path: &Path) -> Result<Uri, LspDapError> {
    let url = Url::from_file_path(path).map_err(|()| LspDapError::InvalidUri {
        uri: path.display().to_string(),
        reason: "path must be absolute".to_owned(),
    })?;
    Uri::from_str(url.as_str()).map_err(|err| LspDapError::InvalidUri {
        uri: url.to_string(),
        reason: err.to_string(),
    })
}

/// Converts a `file://` [`Uri`] back to a filesystem path (`None` for other schemes).
#[must_use]
pub fn lsp_uri_to_path(uri: &Uri) -> Option<PathBuf> {
    Url::parse(uri.as_str()).ok()?.to_file_path().ok()
}

/// Converts a buffer version to the LSP `integer` document version (saturating at `i32::MAX`).
fn lsp_version(version: BufferVersion) -> i32 {
    i32::try_from(version.as_u64()).unwrap_or(i32::MAX)
}

/// Converts a zero-based `(row, UTF-16 column)` pair to an LSP [`Position`].
pub(crate) fn lsp_position(row: usize, column: usize) -> Result<Position, LspDapError> {
    let convert = |value: usize| {
        u32::try_from(value).map_err(|_| LspDapError::SerializationError {
            reason: format!("position component {value} exceeds u32"),
        })
    };
    Ok(Position::new(convert(row)?, convert(column)?))
}

/// Converts a [`Utf16Range`] to an LSP [`Range`].
pub(crate) fn lsp_range(range: Utf16Range) -> Result<Range, LspDapError> {
    Ok(Range::new(
        lsp_position(range.start.row, range.start.column)?,
        lsp_position(range.end.row, range.end.column)?,
    ))
}

/// Converts an LSP [`Position`] to a [`Utf16Point`].
pub(crate) fn utf16_point(position: Position) -> Utf16Point {
    let widen = |value: u32| usize::try_from(value).unwrap_or(usize::MAX);
    Utf16Point::new(widen(position.line), widen(position.character))
}

/// Converts an LSP [`Range`] to a [`Utf16Range`].
pub(crate) fn utf16_range(range: Range) -> Utf16Range {
    Utf16Range::new(utf16_point(range.start), utf16_point(range.end))
}

/// State of a document opened in the language server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenDocumentState {
    /// Buffer backing the document.
    pub buffer_id: BufferId,
    /// Last buffer version sent to the server.
    pub synced_version: BufferVersion,
    /// LSP language id (`java`, `kotlin`, ...).
    pub language_id: String,
}

/// Which kind of `didChange` [`DocumentSyncTracker::sync_changes`] sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncModeApplied {
    /// The server already had the current version; nothing was sent.
    NoOp,
    /// Incremental UTF-16 range changes were sent.
    Incremental,
    /// The history window no longer covered the gap; the full text was sent.
    FullFallback,
}

/// Tracks documents opened in the server and keeps them in sync with `kova_buffer`.
#[derive(Debug, Clone, Default)]
pub struct DocumentSyncTracker {
    documents: HashMap<Uri, OpenDocumentState>,
}

impl DocumentSyncTracker {
    /// Creates an empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// State of an open document, if any.
    #[must_use]
    pub fn document(&self, path: &Path) -> Option<&OpenDocumentState> {
        let uri = path_to_lsp_uri(path).ok()?;
        self.documents.get(&uri)
    }

    /// Whether `path` is open in the server.
    #[must_use]
    pub fn is_open(&self, path: &Path) -> bool {
        self.document(path).is_some()
    }

    /// Number of open documents.
    #[must_use]
    pub fn len(&self) -> usize {
        self.documents.len()
    }

    /// Whether no document is open.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.documents.is_empty()
    }

    /// Iterates over the open documents and their sync state (arbitrary order).
    pub fn open_documents(&self) -> impl Iterator<Item = (&Uri, &OpenDocumentState)> {
        self.documents.iter()
    }

    /// Sends `textDocument/didOpen` with the full snapshot text.
    ///
    /// # Errors
    /// [`LspDapError::InvalidState`] when the document is already open, [`LspDapError::InvalidUri`]
    /// for relative paths and transport errors.
    pub async fn open_document(
        &mut self,
        client: &JdtlsClient,
        path: &Path,
        language_id: &str,
        snapshot: &BufferSnapshot,
    ) -> Result<(), LspDapError> {
        let uri = path_to_lsp_uri(path)?;
        if self.documents.contains_key(&uri) {
            return Err(LspDapError::InvalidState {
                current: "open".to_owned(),
                expected: "closed".to_owned(),
            });
        }
        client
            .notify(
                "textDocument/didOpen",
                json!({
                    "textDocument": {
                        "uri": uri.as_str(),
                        "languageId": language_id,
                        "version": lsp_version(snapshot.version()),
                        "text": snapshot.text(),
                    }
                }),
            )
            .await?;
        self.documents.insert(
            uri,
            OpenDocumentState {
                buffer_id: snapshot.id(),
                synced_version: snapshot.version(),
                language_id: language_id.to_owned(),
            },
        );
        Ok(())
    }

    /// Sends the changes made since the last synced version as `textDocument/didChange`.
    ///
    /// # Errors
    /// [`LspDapError::InvalidState`] when the document is not open or `snapshot` belongs to a
    /// different buffer, position overflow and transport errors.
    pub async fn sync_changes(
        &mut self,
        client: &JdtlsClient,
        path: &Path,
        edit_log: &EditLog,
        snapshot: &BufferSnapshot,
    ) -> Result<SyncModeApplied, LspDapError> {
        let uri = path_to_lsp_uri(path)?;
        let state = self.open_state(&uri)?;
        if state.buffer_id != snapshot.id() {
            return Err(LspDapError::InvalidState {
                current: format!("buffer {}", snapshot.id().as_u64()),
                expected: format!("buffer {}", state.buffer_id.as_u64()),
            });
        }
        let (mode, version, changes) = match edit_log.changes_since(state.synced_version) {
            VersionChanges::UpToDate => return Ok(SyncModeApplied::NoOp),
            VersionChanges::Deltas {
                current_version,
                deltas,
                ..
            } => {
                let changes = deltas
                    .iter()
                    .flat_map(|delta| delta.edits.iter())
                    .map(|edit| {
                        Ok(TextDocumentContentChangeEvent {
                            range: Some(lsp_range(edit.lsp.range)?),
                            range_length: None,
                            text: edit.lsp.text.clone(),
                        })
                    })
                    .collect::<Result<Vec<_>, LspDapError>>()?;
                (SyncModeApplied::Incremental, current_version, changes)
            }
            VersionChanges::FullResyncRequired { .. } => (
                SyncModeApplied::FullFallback,
                snapshot.version(),
                vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: snapshot.text(),
                }],
            ),
        };
        client
            .notify(
                "textDocument/didChange",
                json!({
                    "textDocument": { "uri": uri.as_str(), "version": lsp_version(version) },
                    "contentChanges": changes,
                }),
            )
            .await?;
        if let Some(state) = self.documents.get_mut(&uri) {
            state.synced_version = version;
        }
        Ok(mode)
    }

    /// Sends `textDocument/didSave` (with the full text when `text` is provided).
    ///
    /// # Errors
    /// [`LspDapError::InvalidState`] when the document is not open and transport errors.
    pub async fn save_document(
        &self,
        client: &JdtlsClient,
        path: &Path,
        text: Option<&str>,
    ) -> Result<(), LspDapError> {
        let uri = path_to_lsp_uri(path)?;
        self.open_state(&uri)?;
        let mut params = json!({ "textDocument": { "uri": uri.as_str() } });
        if let Some(text) = text {
            params["text"] = json!(text);
        }
        client.notify("textDocument/didSave", params).await
    }

    /// Sends `textDocument/didClose` and forgets the document.
    ///
    /// # Errors
    /// [`LspDapError::InvalidState`] when the document is not open and transport errors.
    pub async fn close_document(
        &mut self,
        client: &JdtlsClient,
        path: &Path,
    ) -> Result<(), LspDapError> {
        let uri = path_to_lsp_uri(path)?;
        self.open_state(&uri)?;
        client
            .notify(
                "textDocument/didClose",
                json!({ "textDocument": { "uri": uri.as_str() } }),
            )
            .await?;
        self.documents.remove(&uri);
        Ok(())
    }

    fn open_state(&self, uri: &Uri) -> Result<&OpenDocumentState, LspDapError> {
        self.documents
            .get(uri)
            .ok_or_else(|| LspDapError::InvalidState {
                current: "closed".to_owned(),
                expected: "open".to_owned(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_round_trip_with_spaces_and_unicode() {
        let path = Path::new("/tmp/Mój projekt/src/Zażółć 🦀.java");
        let uri = path_to_lsp_uri(path);
        assert!(uri.is_ok());
        let Ok(uri) = uri else { return };
        assert!(uri.as_str().starts_with("file:///tmp/M%C3%B3j%20projekt/"));
        assert_eq!(lsp_uri_to_path(&uri), Some(path.to_path_buf()));
    }

    #[test]
    fn relative_path_is_rejected() {
        assert!(matches!(
            path_to_lsp_uri(Path::new("src/Main.java")),
            Err(LspDapError::InvalidUri { .. })
        ));
        let Ok(http) = Uri::from_str("https://example.com/a") else {
            return;
        };
        assert_eq!(lsp_uri_to_path(&http), None);
    }

    #[test]
    fn version_conversion_saturates() {
        assert_eq!(lsp_version(BufferVersion::new(7)), 7);
        assert_eq!(lsp_version(BufferVersion::new(u64::MAX)), i32::MAX);
    }
}
