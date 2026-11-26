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
