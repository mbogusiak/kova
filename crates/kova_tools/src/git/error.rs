//! Error type shared by all Git operations.

use std::fmt::Display;
use std::path::PathBuf;

/// Failure of a Git operation.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    /// No Git repository was found at or above the given path.
    #[error("no Git repository found at or above `{}`", .0.display())]
    NotARepository(PathBuf),
    /// The repository is bare and has no working tree.
    #[error("the repository has no working tree")]
    NoWorkingTree,
    /// The path is not inside the repository working tree.
    #[error("`{}` is outside the repository working tree", .0.display())]
    OutsideWorkingTree(PathBuf),
    /// The commit message is empty after trimming whitespace.
    #[error("the commit message is empty")]
    EmptyCommitMessage,
    /// The index matches `HEAD`, so a commit would record no change.
    #[error("nothing to commit: the index matches HEAD")]
    NothingToCommit,
    /// No author/committer identity is configured and none was given.
    #[error("no Git identity configured (set user.name and user.email)")]
    MissingIdentity,
    /// Uncommitted or conflicting working-tree changes block branch checkout.
    #[error("working tree has uncommitted changes: {}", .files.join(", "))]
    DirtyWorkingTree {
        /// Repository-relative paths blocking checkout.
        files: Vec<String>,
    },
    /// Branch name fails Git reference naming rules.
    #[error("invalid branch name `{0}`")]
    InvalidBranchName(String),
    /// A branch with this name already exists.
    #[error("branch `{0}` already exists")]
    BranchAlreadyExists(String),
    /// Requested local or remote branch does not exist.
    #[error("branch `{0}` not found")]
    BranchNotFound(String),
    /// Attempted to delete the currently checked-out branch.
    #[error("cannot delete the currently checked-out branch `{0}`")]
    CannotDeleteCurrentBranch(String),
    /// Attempted a non-forced delete of a branch with unmerged commits.
    #[error("branch `{0}` is not fully merged into HEAD")]
    BranchNotMerged(String),
    /// Cannot create a branch in an unborn repository with no commits yet.
    #[error("cannot create a branch before the initial commit")]
    UnbornHead,
    /// External `git` remote command (`fetch` / `pull`) failed.
    #[error("git {op} failed: {message}")]
    RemoteCommandFailed {
        /// Operation name (`fetch` or `pull`).
        op: &'static str,
        /// Error message reported by `git`.
        message: String,
    },
    /// The operation was cancelled by its caller (superseded by a newer request).
    #[error("the Git operation was cancelled")]
    Cancelled,
    /// File-system error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Error reported by `gix`.
    #[error("git: {0}")]
    Gix(String),
}

impl GitError {
    /// Wraps any `gix` error (whose concrete types vary across gitoxide crates).
    pub(crate) fn gix(err: impl Display) -> Self {
        Self::Gix(err.to_string())
    }
}
