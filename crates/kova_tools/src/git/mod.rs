//! Git integration built on [`gix`] (gitoxide), with `fetch`/`pull` delegated to `git`
//! in background tasks so system credential helpers and SSH agents work seamlessly.
//!
//! - [`repo`]: repository discovery, current branch ([`HeadState`]) and working-tree status.
//! - [`branches`]: branch listing, checkout, creation, deletion, and remote `fetch`/`pull`.
//! - [`diff`]: line diff and gutter hunks of a live buffer against `HEAD`.
//! - [`blame`]: last commit (author, date, short SHA, summary) per line of a file.
//! - [`commit`]: `stage`, `unstage` and `commit`.
//!
//! Every operation is synchronous and thread-safe ([`GitRepository`] is `Send + Sync`), so
//! the UI can run it on a background pool and never block the render thread.

pub mod blame;
pub mod branches;
pub mod commit;
pub mod diff;
mod error;
pub mod repo;

pub use blame::{BlameInfo, FileBlame};
pub use branches::{BranchInfo, BranchKind, BranchList, CheckoutResult, FetchSummary, PullSummary};
pub use commit::{CommitSignature, CommitSummary};
pub use diff::{
    DiffLine, DiffLineKind, GutterHunk, LineChangeKind, base_line_for, gutter_hunks, unified_lines,
};
pub use error::GitError;
pub use repo::{ChangeKind, FileStatus, GitRepository, HeadState};

/// Result alias for Git operations.
pub type GitResult<T> = Result<T, GitError>;
