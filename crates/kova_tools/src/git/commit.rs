//! Index writes (`stage` / `unstage`) and `commit`, all through `gix`.

use std::path::Path;

use gix::bstr::{BStr, ByteSlice as _};
use gix::index::entry::{Flags, Mode, Stat};
use gix::object::tree::EntryKind;

use super::repo::GitRepository;
use super::{GitError, GitResult};

/// Explicit author/committer identity for [`GitRepository::commit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitSignature {
    /// Display name.
    pub name: String,
    /// E-mail address.
    pub email: String,
}

/// Result of a successful commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitSummary {
    /// Full hexadecimal id of the new commit.
    pub id: String,
    /// First line of the commit message.
    pub summary: String,
}

impl CommitSummary {
    /// Returns the 7-character abbreviated commit id.
    #[must_use]
    pub fn short_id(&self) -> &str {
        self.id.get(..7).unwrap_or(&self.id)
    }
}

impl GitRepository {
    /// Stages the working-tree state of `path` (`git add <path>`): a present file is
    /// hashed into the object database, a missing file is staged as deleted.
    ///
    /// Content is stored as-is (no clean filters such as `core.autocrlf`).
    ///
    /// # Errors
    /// Fails when the path is invalid or the index cannot be written.
    pub fn stage(&self, path: impl AsRef<Path>) -> GitResult<()> {
        let relative = self.relative_path(path)?;
        let absolute = self.absolute_path(&relative);
        let repo = self.local();
        let mut index = owned_index(&repo)?;
        remove_path(&mut index, &relative);

        match std::fs::symlink_metadata(&absolute) {
            Ok(meta) if meta.is_dir() => {
                return Err(GitError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("`{relative}` is a directory; stage its files instead"),
                )));
            }
            Ok(meta) => {
                let (data, mode) = if meta.file_type().is_symlink() {
                    let target = std::fs::read_link(&absolute)?;
                    (
                        target.to_string_lossy().into_owned().into_bytes(),
                        Mode::SYMLINK,
                    )
                } else {
                    let mode = if is_executable(&meta) {
                        Mode::FILE_EXECUTABLE
                    } else {
                        Mode::FILE
                    };
                    (std::fs::read(&absolute)?, mode)
                };
                let id = repo.write_blob(&data).map_err(GitError::gix)?.detach();
                let fs_meta = gix::index::fs::Metadata::from_path_no_follow(&absolute)?;
                let stat = Stat::from_fs(&fs_meta).unwrap_or_default();
                index.dangerously_push_entry(stat, id, Flags::empty(), mode, as_bstr(&relative));
                index.sort_entries();
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
        write_index(&mut index)
    }

    /// Unstages `path` (`git restore --staged <path>`): the index entry is reset to the
    /// `HEAD` version, or removed when the file is not in `HEAD`.
    ///
    /// # Errors
    /// Fails when the path is invalid or the index cannot be written.
    pub fn unstage(&self, path: impl AsRef<Path>) -> GitResult<()> {
        let relative = self.relative_path(path)?;
        let repo = self.local();
        let mut index = owned_index(&repo)?;
        remove_path(&mut index, &relative);

        let tree_id = repo.head_tree_id_or_empty().map_err(GitError::gix)?;
        let tree = repo.find_tree(tree_id).map_err(GitError::gix)?;
        if let Some(entry) = tree
            .lookup_entry_by_path(&relative)
            .map_err(GitError::gix)?
        {
            let mode = match entry.mode().kind() {
                EntryKind::BlobExecutable => Mode::FILE_EXECUTABLE,
                EntryKind::Link => Mode::SYMLINK,
                EntryKind::Commit => Mode::COMMIT,
                EntryKind::Blob | EntryKind::Tree => Mode::FILE,
            };
            // A zeroed stat forces the next status run to re-hash the working-tree file.
            index.dangerously_push_entry(
                Stat::default(),
                entry.object_id(),
                Flags::empty(),
                mode,
                as_bstr(&relative),
            );
            index.sort_entries();
        }
        write_index(&mut index)
    }

    /// Records the index as a new commit on `HEAD` (`git commit -m <message>`).
    ///
    /// Uses `signature` for author and committer, or the configured `user.name` /
    /// `user.email` when `None`.
    ///
    /// # Errors
    /// [`GitError::EmptyCommitMessage`], [`GitError::NothingToCommit`] when the index
    /// matches `HEAD`, [`GitError::MissingIdentity`], or object/ref write failures.
    pub fn commit(
        &self,
        message: &str,
        signature: Option<&CommitSignature>,
    ) -> GitResult<CommitSummary> {
        let message = message.trim();
        if message.is_empty() {
            return Err(GitError::EmptyCommitMessage);
        }
        let repo = self.local();
        let index = repo.index_or_empty().map_err(GitError::gix)?;

        let mut editor = repo
            .edit_tree(gix::ObjectId::empty_tree(repo.object_hash()))
            .map_err(GitError::gix)?;
        for entry in index.entries() {
            if entry.flags.contains(Flags::REMOVE) {
                continue;
            }
            let Some(mode) = entry.mode.to_tree_entry_mode() else {
                continue;
            };
            editor
                .upsert(entry.path(&index), mode.kind(), entry.id)
                .map_err(GitError::gix)?;
        }
        let tree_id = editor.write().map_err(GitError::gix)?.detach();

        let head_tree = repo
            .head_tree_id_or_empty()
            .map_err(GitError::gix)?
            .detach();
        if tree_id == head_tree {
            return Err(GitError::NothingToCommit);
        }
        let parents: Vec<gix::ObjectId> = repo
            .head()
            .map_err(GitError::gix)?
            .id()
            .map(gix::Id::detach)
            .into_iter()
            .collect();

        let (author, committer) = match signature {
            Some(sig) => {
                let actor = gix::actor::Signature {
                    name: sig.name.as_str().into(),
                    email: sig.email.as_str().into(),
                    time: gix::date::Time::now_local_or_utc(),
                };
                (actor.clone(), actor)
            }
            None => (configured(repo.author())?, configured(repo.committer())?),
        };
        let (mut author_buf, mut committer_buf) = (
            gix::date::parse::TimeBuf::default(),
            gix::date::parse::TimeBuf::default(),
        );
        let full_message = format!("{message}\n");
        let id = repo
            .commit_as(
                committer.to_ref(&mut committer_buf),
                author.to_ref(&mut author_buf),
                "HEAD",
                &full_message,
                tree_id,
                parents,
            )
            .map_err(GitError::gix)?;
        Ok(CommitSummary {
            id: id.to_string(),
            summary: message.lines().next().unwrap_or_default().to_owned(),
        })
    }
}

fn configured(
    identity: Option<gix::Result<gix::actor::SignatureRef<'_>>>,
) -> GitResult<gix::actor::Signature> {
    identity
        .ok_or(GitError::MissingIdentity)?
        .map_err(GitError::gix)?
        .to_owned()
        .map_err(GitError::gix)
}

fn as_bstr(relative: &str) -> &BStr {
    relative.as_bytes().as_bstr()
}

fn owned_index(repo: &gix::Repository) -> GitResult<gix::index::File> {
    let shared = repo.index_or_empty().map_err(GitError::gix)?;
    Ok(gix::index::File::clone(&shared))
}

fn remove_path(index: &mut gix::index::File, relative: &str) {
    let target = as_bstr(relative);
    index.remove_entries(|_, path, _| path == target);
}

fn write_index(index: &mut gix::index::File) -> GitResult<()> {
    // The cached tree extension no longer matches the edited entries.
    drop(index.remove_tree());
    drop(index.remove_resolve_undo());
    index
        .write(
            gix::index::write::Options::default(),
            gix::fs::SharedPermissions::default(),
        )
        .map_err(GitError::gix)
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
const fn is_executable(_meta: &std::fs::Metadata) -> bool {
    false
}
