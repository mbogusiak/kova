//! Open-document state machine (`DocumentState`) and save requests (`KOVA-016`).
//!
//! Pure Rust with no GPUI or `tokio` types: `kova_ui` never touches the disk. Save
//! requests are handed to a [`SaveSink`] supplied by `kova_app`, which performs the
//! atomic write on its `tokio` runtime and reports back via
//! [`DocumentState::finish_save`].

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use kova_buffer::BufferSnapshot;

/// Status-bar message when saving an *Untitled* buffer without a path.
pub const SAVE_PATH_REQUIRED_MESSAGE: &str = "Save path required (open via `kova <path>`)";

/// Request to atomically persist `snapshot` at `path`, issued on `Cmd+S` / `Ctrl+S`.
#[derive(Debug, Clone)]
pub struct SaveRequest {
    /// Target file path.
    pub path: PathBuf,
    /// $O(1)$ immutable snapshot of the buffer to write.
    pub snapshot: BufferSnapshot,
    /// Ticket identifying this save when the result comes back.
    pub ticket: SaveTicket,
}

/// Source of process-wide unique document generations, so a [`SaveTicket`] identifies the
/// document that issued it even when several documents are open in tabs and splits.
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

fn next_generation() -> u64 {
    NEXT_GENERATION.fetch_add(1, Ordering::Relaxed)
}

/// Identifies one in-flight save (document generation + buffer revision).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SaveTicket {
    /// Document generation; unique per process and renewed whenever a different buffer is
    /// loaded into the document.
    pub generation: u64,
    /// Buffer revision (`BufferSnapshot::version`) being written.
    pub revision: u64,
}

/// Callback that performs a [`SaveRequest`] off the UI thread.
pub type SaveSink = Arc<dyn Fn(SaveRequest) + Send + Sync>;

/// Severity of the transient status-bar message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    /// Neutral information (`Saving…`, `Saved`, `New file`).
    Info,
    /// Recoverable problem (e.g. invalid `settings.toml`).
    Warning,
    /// I/O failure.
    Error,
}

/// Lifecycle of the document's persistence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveStatus {
    /// Nothing to report.
    Idle,
    /// A save is running in the background.
    Saving,
    /// The last save succeeded.
    Saved,
    /// The last save failed with the given message.
    Failed(String),
}

/// What the shell should do after `Save` was triggered.
#[derive(Debug, Clone)]
pub enum SaveAction {
    /// Dispatch the request to the [`SaveSink`].
    Dispatch(SaveRequest),
    /// A save is already running; another one is queued after it completes.
    Queued,
    /// The document has no path yet (*Untitled*).
    NeedsPath,
}

/// Persistence state of one open document (one per file, shared by every tab and split).
#[derive(Clone, PartialEq, Eq)]
pub struct DocumentState {
    /// File path, or `None` for an *Untitled* buffer.
    pub path: Option<PathBuf>,
    /// Buffer revision at load time or after the last successful save.
    pub saved_revision: u64,
    /// Revision currently being written in the background.
    pub save_in_flight: Option<u64>,
    /// Message of the last I/O error, if any.
    pub last_io_error: Option<String>,
    /// `true` when the file did not exist on disk when it was opened.
    pub is_new_file: bool,
    generation: u64,
    save_queued: bool,
    status: SaveStatus,
}

impl fmt::Debug for DocumentState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DocumentState")
            .field("path", &self.path)
            .field("saved_revision", &self.saved_revision)
            .field("save_in_flight", &self.save_in_flight)
            .field("last_io_error", &self.last_io_error)
            .field("is_new_file", &self.is_new_file)
            .field("generation", &self.generation)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl Default for DocumentState {
    fn default() -> Self {
        Self::untitled()
    }
}

impl DocumentState {
    /// Creates an *Untitled* document at revision `0`.
    #[must_use]
    pub const fn untitled() -> Self {
        Self {
            path: None,
            saved_revision: 0,
            save_in_flight: None,
            last_io_error: None,
            is_new_file: false,
            generation: 0,
            save_queued: false,
            status: SaveStatus::Idle,
        }
    }

    /// Creates the state of a document loaded at `revision` and backed by `path`
    /// (`None` for in-memory documents such as the welcome buffer).
    #[must_use]
    pub fn opened(path: Option<PathBuf>, revision: u64, is_new_file: bool) -> Self {
        let mut doc = Self::untitled();
        doc.reset_for(path, revision, is_new_file);
        doc
    }

    /// Resets the state for a newly loaded buffer at `revision` backed by `path`.
    pub fn reset_for(&mut self, path: Option<PathBuf>, revision: u64, is_new_file: bool) {
        self.path = path;
        self.saved_revision = revision;
        self.save_in_flight = None;
        self.last_io_error = None;
        self.is_new_file = is_new_file;
        self.generation = next_generation();
        self.save_queued = false;
        self.status = SaveStatus::Idle;
    }

    /// Returns the current document generation.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns `true` when `current_revision` differs from the last saved revision.
    #[must_use]
    pub const fn is_dirty(&self, current_revision: u64) -> bool {
        current_revision != self.saved_revision
    }

    /// Returns the persistence status.
    #[must_use]
    pub const fn status(&self) -> &SaveStatus {
        &self.status
    }

    /// Returns the file name for tabs and window titles (`Untitled` without a path).
    #[must_use]
    pub fn display_name(&self) -> String {
        self.path.as_deref().and_then(Path::file_name).map_or_else(
            || "Untitled".to_owned(),
            |n| n.to_string_lossy().into_owned(),
        )
    }

    /// Formats the window title, e.g. `Main.java • — Kova` when dirty.
    #[must_use]
    pub fn window_title(&self, current_revision: u64) -> String {
        if self.is_dirty(current_revision) {
            format!("{} • — Kova", self.display_name())
        } else {
            format!("{} — Kova", self.display_name())
        }
    }

    /// Returns the status-bar text and severity describing the document.
    #[must_use]
    pub fn status_label(&self) -> (StatusKind, String) {
        match &self.status {
            SaveStatus::Saving => (StatusKind::Info, "Saving…".to_owned()),
            SaveStatus::Saved => (StatusKind::Info, "Saved".to_owned()),
            SaveStatus::Failed(message) => (StatusKind::Error, message.clone()),
            SaveStatus::Idle if self.is_new_file => (
                StatusKind::Info,
                format!("New file · {}", self.display_name()),
            ),
            SaveStatus::Idle => (StatusKind::Info, "Ready".to_owned()),
        }
    }

    /// Assigns a path to an *Untitled* document (e.g. after a *Save As* prompt).
    pub fn set_path(&mut self, path: PathBuf) {
        self.path = Some(path);
        self.is_new_file = true;
    }

    /// Records a non-save failure (e.g. the CLI file could not be read).
    pub fn set_error(&mut self, message: String) {
        self.last_io_error = Some(message.clone());
        self.status = SaveStatus::Failed(message);
    }

    /// Starts a save of `snapshot`, or queues one when a save is already in flight.
    pub fn begin_save(&mut self, snapshot: BufferSnapshot) -> SaveAction {
        let Some(path) = self.path.clone() else {
            return SaveAction::NeedsPath;
        };
        if self.save_in_flight.is_some() {
            self.save_queued = true;
            return SaveAction::Queued;
        }
        let revision = snapshot.version().as_u64();
        self.save_in_flight = Some(revision);
        self.status = SaveStatus::Saving;
        SaveAction::Dispatch(SaveRequest {
            path,
            snapshot,
            ticket: SaveTicket {
                generation: self.generation,
                revision,
            },
        })
    }

    /// Applies the result of a background save.
    ///
    /// Returns `true` when a queued save should be dispatched now (call
    /// [`Self::begin_save`] again with a fresh snapshot). Results from a previous
    /// generation (a different buffer was loaded meanwhile) are ignored.
    pub fn finish_save(&mut self, ticket: SaveTicket, result: Result<(), String>) -> bool {
        if ticket.generation != self.generation || self.save_in_flight != Some(ticket.revision) {
            return false;
        }
        self.save_in_flight = None;
        match result {
            Ok(()) => {
                // Edits made while saving keep the document dirty: `saved_revision = R`
                // while the live buffer is already at `R + k`.
                self.saved_revision = ticket.revision;
                self.last_io_error = None;
                self.is_new_file = false;
                self.status = SaveStatus::Saved;
            }
            Err(message) => {
                self.last_io_error = Some(message.clone());
                self.status = SaveStatus::Failed(message);
                self.save_queued = false;
                return false;
            }
        }
        std::mem::take(&mut self.save_queued)
    }
}

#[cfg(test)]
mod tests {
    use kova_buffer::Buffer;

    use super::*;

    fn dispatch(doc: &mut DocumentState, buffer: &Buffer) -> SaveRequest {
        match doc.begin_save(buffer.snapshot()) {
            SaveAction::Dispatch(req) => req,
            other => panic!("expected dispatch, got {other:?}"),
        }
    }

    #[test]
    fn documents_get_distinct_generations() {
        let a = DocumentState::opened(Some("/p/A.java".into()), 0, false);
        let b = DocumentState::opened(Some("/p/B.java".into()), 0, false);
        assert_ne!(a.generation(), b.generation());
        assert_ne!(a.generation(), DocumentState::untitled().generation());
    }

    #[test]
    fn untitled_requires_a_path() {
        let mut doc = DocumentState::untitled();
        assert_eq!(doc.display_name(), "Untitled");
        assert!(matches!(
            doc.begin_save(Buffer::new().snapshot()),
            SaveAction::NeedsPath
        ));
        assert_eq!(doc.status_label(), (StatusKind::Info, "Ready".to_owned()));
    }

    #[test]
    fn save_clears_dirty_and_reports_saved() {
        let mut buffer = Buffer::from_text("class A {}\n");
        let mut doc = DocumentState::untitled();
        doc.reset_for(Some("/p/A.java".into()), buffer.version().as_u64(), false);
        assert!(!doc.is_dirty(buffer.version().as_u64()));

        buffer.apply_edit(0..0, "// x\n").expect("edit");
        let now = buffer.version().as_u64();
        assert!(doc.is_dirty(now));
        assert_eq!(doc.window_title(now), "A.java • — Kova");

        let req = dispatch(&mut doc, &buffer);
        assert_eq!(req.path, PathBuf::from("/p/A.java"));
        assert_eq!(doc.status_label().1, "Saving…");
        assert!(!doc.finish_save(req.ticket, Ok(())));
        assert!(!doc.is_dirty(now));
        assert_eq!(doc.status_label().1, "Saved");
        assert_eq!(doc.window_title(now), "A.java — Kova");
    }

    #[test]
    fn edits_during_save_keep_document_dirty() {
        let mut buffer = Buffer::from_text("a");
        let mut doc = DocumentState::untitled();
        doc.reset_for(Some("/p/a.txt".into()), 0, false);
        buffer.apply_edit(1..1, "b").expect("edit");
        let req = dispatch(&mut doc, &buffer);

        // User keeps typing while tokio writes revision R.
        buffer.apply_edit(2..2, "c").expect("edit");
        assert!(matches!(
            doc.begin_save(buffer.snapshot()),
            SaveAction::Queued
        ));
        let should_resave = doc.finish_save(req.ticket, Ok(()));
        assert_eq!(doc.saved_revision, req.ticket.revision);
        assert!(doc.is_dirty(buffer.version().as_u64()));
        assert!(should_resave, "queued save must be re-dispatched");

        let req2 = dispatch(&mut doc, &buffer);
        assert!(!doc.finish_save(req2.ticket, Ok(())));
        assert!(!doc.is_dirty(buffer.version().as_u64()));
    }

    #[test]
    fn failed_save_keeps_dirty_and_stale_results_are_ignored() {
        let mut buffer = Buffer::from_text("x");
        let mut doc = DocumentState::untitled();
        doc.reset_for(Some("/ro/x.txt".into()), 0, true);
        assert_eq!(doc.status_label().1, "New file · x.txt");
        buffer.apply_edit(0..0, "y").expect("edit");

        let req = dispatch(&mut doc, &buffer);
        doc.finish_save(req.ticket, Err("permission denied".to_owned()));
        assert!(doc.is_dirty(buffer.version().as_u64()));
        assert_eq!(
            doc.status_label(),
            (StatusKind::Error, "permission denied".to_owned())
        );
        assert_eq!(doc.last_io_error.as_deref(), Some("permission denied"));

        let req = dispatch(&mut doc, &buffer);
        doc.reset_for(Some("/p/other.txt".into()), 0, false);
        assert!(!doc.finish_save(req.ticket, Ok(())));
        assert_eq!(doc.saved_revision, 0);
        assert_eq!(*doc.status(), SaveStatus::Idle);
    }
}
