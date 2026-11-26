//! Repository discovery, current branch and working-tree status.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gix::bstr::{BString, ByteSlice as _};

use super::diff::{DiffLine, GutterHunk, gutter_hunks, unified_lines};
use super::{GitError, GitResult};

/// State of `HEAD`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum HeadState {
    /// `HEAD` points at an existing branch (short name, e.g. `main`).
    Branch(String),
    /// `HEAD` points at a branch with no commit yet (fresh repository).
    Unborn(String),
    /// `HEAD` is detached at the given abbreviated commit id.
    Detached(String),
}

impl HeadState {
    /// Returns the label shown on the status bar (branch name or short SHA).
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::Branch(name) | Self::Unborn(name) | Self::Detached(name) => name,
        }
    }
}

/// Kind of change of one file, either in the index (staged) or the working tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ChangeKind {
    /// New file (staged addition).
    Added,
    /// Content or type changed.
    Modified,
    /// File removed.
    Deleted,
    /// File moved (staged rename detected by `gix`).
    Renamed,
    /// File not tracked by Git yet.
    Untracked,
    /// Merge conflict.
    Conflicted,
}

impl ChangeKind {
    /// Returns the one-letter code used by `git status --short`.
    #[must_use]
    pub const fn code(self) -> char {
        match self {
            Self::Added => 'A',
            Self::Modified => 'M',
            Self::Deleted => 'D',
            Self::Renamed => 'R',
            Self::Untracked => '?',
            Self::Conflicted => 'U',
        }
    }
}

/// Status of one changed path (relative to the working tree, `/`-separated).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FileStatus {
    /// Path relative to the working-tree root.
    pub path: String,
    /// Change between `HEAD` and the index, if any.
    pub staged: Option<ChangeKind>,
    /// Change between the index and the working tree, if any.
    pub unstaged: Option<ChangeKind>,
}

impl FileStatus {
    /// Returns `true` when the file has staged changes and no unstaged ones.
    #[must_use]
    pub const fn is_fully_staged(&self) -> bool {
        self.staged.is_some() && self.unstaged.is_none()
    }

    /// Returns the most relevant kind for display (working tree first, then index).
    #[must_use]
    pub fn display_kind(&self) -> ChangeKind {
        match (self.staged, self.unstaged) {
            (Some(ChangeKind::Added), Some(ChangeKind::Modified)) => ChangeKind::Added,
            (_, Some(kind)) | (Some(kind), None) => kind,
            (None, None) => ChangeKind::Modified,
        }
    }
}

/// A discovered, non-bare Git repository. Cheap to share across threads.
pub struct GitRepository {
    repo: gix::ThreadSafeRepository,
    workdir: PathBuf,
}

impl fmt::Debug for GitRepository {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitRepository")
            .field("workdir", &self.workdir)
            .finish_non_exhaustive()
    }
}

impl GitRepository {
    /// Finds the repository containing `path` (searching parent directories).
    ///
    /// # Errors
    /// [`GitError::NotARepository`] when no repository encloses `path`, or
    /// [`GitError::NoWorkingTree`] for a bare repository.
    pub fn discover(path: impl AsRef<Path>) -> GitResult<Self> {
        let path = path.as_ref();
        let repo = gix::discover(path).map_err(|_| GitError::NotARepository(path.to_owned()))?;
        Self::from_repo(repo)
    }

    /// Creates a new empty repository with a working tree at `path`.
    ///
    /// # Errors
    /// Fails when the directory cannot be initialized.
    pub fn init(path: impl AsRef<Path>) -> GitResult<Self> {
        let repo = gix::init(path.as_ref()).map_err(GitError::gix)?;
        Self::from_repo(repo)
    }

    fn from_repo(repo: gix::Repository) -> GitResult<Self> {
        let workdir = repo.workdir().ok_or(GitError::NoWorkingTree)?.to_owned();
        let workdir = workdir.canonicalize().unwrap_or(workdir);
        Ok(Self {
            repo: repo.into_sync(),
            workdir,
        })
    }

    /// Returns a thread-local handle for one operation.
    pub(crate) fn local(&self) -> gix::Repository {
        self.repo.to_thread_local()
    }

    /// Returns the canonical working-tree root.
    #[must_use]
    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    /// Converts `path` (absolute, or relative to the working tree) to a `/`-separated
    /// repository-relative path.
    ///
    /// # Errors
    /// [`GitError::OutsideWorkingTree`] when the path escapes the working tree.
    pub fn relative_path(&self, path: impl AsRef<Path>) -> GitResult<String> {
        let path = path.as_ref();
        let relative = if path.is_absolute() {
            let absolute = canonicalize_lenient(path);
            absolute
                .strip_prefix(&self.workdir)
                .map(Path::to_path_buf)
                .map_err(|_| GitError::OutsideWorkingTree(path.to_owned()))?
        } else {
            path.to_owned()
        };
        let mut parts = Vec::new();
        for component in relative.components() {
            match component {
                Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
                Component::CurDir => {}
                _ => return Err(GitError::OutsideWorkingTree(path.to_owned())),
            }
        }
        if parts.is_empty() {
            return Err(GitError::OutsideWorkingTree(path.to_owned()));
        }
        Ok(parts.join("/"))
    }

    /// Returns the absolute working-tree path of a repository-relative path.
    #[must_use]
    pub fn absolute_path(&self, relative: &str) -> PathBuf {
        self.workdir.join(relative)
    }

    /// Reads the current `HEAD` state (branch, unborn branch or detached commit).
    ///
    /// # Errors
    /// Fails when `HEAD` cannot be read.
    pub fn head(&self) -> GitResult<HeadState> {
        let repo = self.local();
        let head = repo.head().map_err(GitError::gix)?;
        if let Some(name) = head.referent_name() {
            let short = name.shorten().to_str_lossy().into_owned();
            return Ok(if head.is_unborn() {
                HeadState::Unborn(short)
            } else {
                HeadState::Branch(short)
            });
        }
        let short = head
            .id()
            .map(|id| id.to_hex_with_len(7).to_string())
            .unwrap_or_default();
        Ok(HeadState::Detached(short))
    }

    /// Computes the working-tree status (`HEAD`↔index and index↔worktree), sorted by path.
    /// Untracked files are listed individually; ignored files are skipped.
    ///
    /// # Errors
    /// Fails when the index or object database cannot be read.
    pub fn status(&self) -> GitResult<Vec<FileStatus>> {
        self.status_cancellable(&Arc::new(AtomicBool::new(false)))
    }

    /// Like [`Self::status`], but aborts with [`GitError::Cancelled`] as soon as `cancel`
    /// is set, so a newer scan can supersede a stale one in large repositories.
    ///
    /// # Errors
    /// [`GitError::Cancelled`], or failures reading the index or object database.
    pub fn status_cancellable(&self, cancel: &Arc<AtomicBool>) -> GitResult<Vec<FileStatus>> {
        use gix::status::index_worktree::iter::Summary;

        let repo = self.local();
        let iter = repo
            .status(gix::progress::Discard)
            .map_err(GitError::gix)?
            .should_interrupt_owned(Arc::clone(cancel))
            .untracked_files(gix::status::UntrackedFiles::Files)
            .index_worktree_rewrites(None)
            .index_worktree_submodules(None)
            .tree_index_track_renames(gix::status::tree_index::TrackRenames::Disabled)
            .into_iter(Vec::<BString>::new())
            .map_err(GitError::gix)?;

        let mut by_path: BTreeMap<String, FileStatus> = BTreeMap::new();
        for item in iter {
            if cancel.load(Ordering::Relaxed) {
                return Err(GitError::Cancelled);
            }
            let item = item.map_err(GitError::gix)?;
            let path = item.location().to_str_lossy().into_owned();
            let (staged, kind) = match &item {
                gix::status::Item::TreeIndex(change) => {
                    use gix::diff::index::ChangeRef;
                    let kind = match change {
                        ChangeRef::Addition { .. } => ChangeKind::Added,
                        ChangeRef::Deletion { .. } => ChangeKind::Deleted,
                        ChangeRef::Modification { .. } => ChangeKind::Modified,
                        ChangeRef::Rewrite { .. } => ChangeKind::Renamed,
                    };
                    (true, kind)
                }
                gix::status::Item::IndexWorktree(change) => {
                    let Some(summary) = change.summary() else {
                        continue;
                    };
                    let kind = match summary {
                        Summary::Added => ChangeKind::Untracked,
                        Summary::Removed => ChangeKind::Deleted,
                        Summary::IntentToAdd => ChangeKind::Added,
                        Summary::Conflict => ChangeKind::Conflicted,
                        Summary::Renamed | Summary::Copied => ChangeKind::Renamed,
                        Summary::Modified | Summary::TypeChange => ChangeKind::Modified,
                    };
                    (false, kind)
                }
            };
            let entry = by_path.entry(path.clone()).or_insert_with(|| FileStatus {
                path,
                staged: None,
                unstaged: None,
            });
            if staged {
                entry.staged = Some(kind);
            } else {
                entry.unstaged = Some(kind);
            }
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(GitError::Cancelled);
        }
        Ok(by_path.into_values().collect())
    }

    /// Returns the content of `path` in the `HEAD` commit, or `None` when the file is not
    /// committed (or the branch is unborn).
    ///
    /// # Errors
    /// Fails when the path is invalid or objects cannot be read.
    pub fn head_contents(&self, path: impl AsRef<Path>) -> GitResult<Option<Vec<u8>>> {
        let relative = self.relative_path(path)?;
        let repo = self.local();
        let tree_id = repo.head_tree_id_or_empty().map_err(GitError::gix)?;
        let tree = repo.find_tree(tree_id).map_err(GitError::gix)?;
        let Some(entry) = tree
            .lookup_entry_by_path(&relative)
            .map_err(GitError::gix)?
        else {
            return Ok(None);
        };
        if !entry.mode().is_blob() {
            return Ok(None);
        }
        let object = entry.object().map_err(GitError::gix)?;
        Ok(Some(object.data.clone()))
    }

    /// Computes gutter hunks of the live `buffer_text` of `path` against `HEAD`.
    /// A file absent from `HEAD` is reported as fully added.
    ///
    /// # Errors
    /// Fails when the path is invalid or objects cannot be read.
    pub fn gutter_diff(
        &self,
        path: impl AsRef<Path>,
        buffer_text: &str,
    ) -> GitResult<Vec<GutterHunk>> {
        let base = self.head_contents(path)?.unwrap_or_default();
        Ok(gutter_hunks(&String::from_utf8_lossy(&base), buffer_text))
    }

    /// Builds a unified preview of `path`: `HEAD` content against the working-tree file
    /// (a deleted file shows every line as removed).
    ///
    /// # Errors
    /// Fails when the path is invalid or the file cannot be read.
    pub fn file_diff(&self, path: impl AsRef<Path>) -> GitResult<Vec<DiffLine>> {
        let relative = self.relative_path(path)?;
        let base = self.head_contents(&relative)?.unwrap_or_default();
        let current = match std::fs::read(self.absolute_path(&relative)) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(err) => return Err(err.into()),
        };
        Ok(unified_lines(
            &String::from_utf8_lossy(&base),
            &String::from_utf8_lossy(&current),
        ))
    }
}

/// Canonicalizes `path`, falling back to canonicalizing its parent for files that do not
/// exist (yet or anymore).
fn canonicalize_lenient(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => canonicalize_lenient(parent).join(name),
        _ => path.to_owned(),
    }
}
