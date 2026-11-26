//! Branch listing, dirty-tree validation, checkout, creation, deletion, and remote
//! `fetch`/`pull` operations on [`GitRepository`].

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Component, Path};
use std::process::{Command, Stdio};

use gix::bstr::{BStr, BString, ByteSlice as _};
use gix::index::entry::{Flags, Mode, Stat};
use gix::refs::FullName;
use gix::refs::Target;
use gix::refs::transaction::{PreviousValue, RefEdit};

use super::repo::{ChangeKind, GitRepository, HeadState};
use super::{GitError, GitResult};

/// Kind of a Git branch entry.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum BranchKind {
    /// Local branch under `refs/heads/`.
    Local,
    /// Remote-tracking branch under `refs/remotes/`.
    Remote {
        /// Configured remote name (e.g. `origin`).
        remote: String,
        /// Short branch name on that remote (e.g. `main` or `feat/ui`).
        short_name: String,
    },
}

/// Metadata for a single local or remote branch.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BranchInfo {
    /// Short display name (e.g. `main`, `feat/ui`, `origin/main`).
    pub name: String,
    /// Full reference name (`refs/heads/...` or `refs/remotes/...`).
    pub full_ref: String,
    /// Whether this branch is local or remote-tracking.
    pub kind: BranchKind,
    /// `true` when `HEAD` currently points to this local branch.
    pub is_current: bool,
    /// Abbreviated 7-character target commit hash.
    pub target_oid: String,
    /// Configured upstream tracking branch name (e.g. `origin/main`), if any.
    pub upstream: Option<String>,
    /// `(ahead, behind)` commit count relative to `upstream`, when resolvable.
    pub ahead_behind: Option<(usize, usize)>,
}

/// Snapshot of `HEAD`, local branches, and remote-tracking branches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchList {
    /// Current `HEAD` state.
    pub head: HeadState,
    /// Local branches sorted with the current branch first, then alphabetically.
    pub local: Vec<BranchInfo>,
    /// Remote-tracking branches sorted alphabetically (excluding `*/HEAD`).
    pub remote: Vec<BranchInfo>,
}

/// Result of switching branches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutResult {
    /// `HEAD` state prior to checkout.
    pub previous: HeadState,
    /// `HEAD` state after checkout.
    pub current: HeadState,
    /// `true` when working-tree files or `HEAD` target changed.
    pub changed_files: bool,
}

/// Summary of a background `git fetch --all --prune` operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchSummary {
    /// `HEAD` state after fetching.
    pub head: HeadState,
    /// `true` when remote-tracking references changed.
    pub changed: bool,
    /// Concise output message from `git fetch`.
    pub message: String,
}

/// Summary of a background `git pull --ff-only` operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullSummary {
    /// `HEAD` state after pulling.
    pub head: HeadState,
    /// `true` when `HEAD` moved and working-tree files changed.
    pub changed: bool,
    /// Concise output message from `git pull`.
    pub message: String,
}

impl GitRepository {
    /// Lists `HEAD`, all local branches (`refs/heads/*`), and all remote-tracking branches
    /// (`refs/remotes/*`, excluding `*/HEAD`).
    ///
    /// # Errors
    /// Fails when references or object metadata cannot be read.
    pub fn branches(&self) -> GitResult<BranchList> {
        let mut repo = self.local();
        drop(repo.reload());
        let head = read_head(&repo)?;
        let current_branch = match &head {
            HeadState::Branch(name) => Some(name.as_str()),
            HeadState::Unborn(_) | HeadState::Detached(_) => None,
        };
        let remote_names: Vec<String> = repo
            .remote_names()
            .into_iter()
            .map(|r| r.to_str_lossy().into_owned())
            .collect();

        let refs = repo.references().map_err(GitError::gix)?;

        let mut local = Vec::new();
        for item in refs.local_branches().map_err(GitError::gix)? {
            let mut reference = item.map_err(GitError::gix)?;
            let full_ref = reference.name().as_bstr().to_str_lossy().into_owned();
            let name = reference.name().shorten().to_str_lossy().into_owned();
            let is_current = current_branch == Some(name.as_str());
            let (upstream, ahead_behind_ids) = {
                let tracking = reference
                    .remote_tracking_ref_name(gix::remote::Direction::Fetch)
                    .and_then(Result::ok)
                    .map(|f| f.shorten().to_str_lossy().into_owned());
                (
                    tracking,
                    reference.remote_tracking_ref_name(gix::remote::Direction::Fetch),
                )
            };
            let local_id = reference.peel_to_id().ok().map(gix::Id::detach);
            let target_oid = local_id
                .map(|id| id.to_hex_with_len(7).to_string())
                .unwrap_or_default();

            let ahead_behind = ahead_behind_ids
                .and_then(Result::ok)
                .and_then(|tracking_full| {
                    compute_ahead_behind(&repo, tracking_full.as_ref(), local_id)
                });

            local.push(BranchInfo {
                name,
                full_ref,
                kind: BranchKind::Local,
                is_current,
                target_oid,
                upstream,
                ahead_behind,
            });
        }

        let mut remote = Vec::new();
        for item in refs.remote_branches().map_err(GitError::gix)? {
            let mut reference = item.map_err(GitError::gix)?;
            if matches!(reference.target(), gix::refs::TargetRef::Symbolic(_)) {
                continue;
            }
            let name = reference.name().shorten().to_str_lossy().into_owned();
            if name.ends_with("/HEAD") {
                continue;
            }
            let Some((remote_name, short_name)) = split_remote_branch(&name, &remote_names) else {
                continue;
            };
            let full_ref = reference.name().as_bstr().to_str_lossy().into_owned();
            let target_oid = reference
                .peel_to_id()
                .ok()
                .map(|id| id.to_hex_with_len(7).to_string())
                .unwrap_or_default();
            remote.push(BranchInfo {
                name,
                full_ref,
                kind: BranchKind::Remote {
                    remote: remote_name,
                    short_name,
                },
                is_current: false,
                target_oid,
                upstream: None,
                ahead_behind: None,
            });
        }

        local.sort_by(|a, b| {
            b.is_current
                .cmp(&a.is_current)
                .then_with(|| a.name.cmp(&b.name))
        });
        remote.sort_by(|a, b| a.name.cmp(&b.name));

        Ok(BranchList {
            head,
            local,
            remote,
        })
    }

    /// Verifies that the working tree and index are safe to switch to `target_tree`.
    ///
    /// Staged changes, tracked working-tree modifications/deletions/conflicts, and untracked
    /// files that collide with paths in `target_tree` cause [`GitError::DirtyWorkingTree`].
    ///
    /// # Errors
    /// [`GitError::DirtyWorkingTree`] when uncommitted changes would block checkout, or status
    /// inspection errors.
    pub fn validate_clean_for_checkout(&self, target_tree: Option<gix::ObjectId>) -> GitResult<()> {
        let statuses = self.status()?;
        let repo = self.local();
        let tree = match target_tree {
            Some(id) => Some(repo.find_tree(id).map_err(GitError::gix)?),
            None => None,
        };

        let mut blocking = Vec::new();
        for status in statuses {
            let has_staged = status.staged.is_some();
            let has_tracked_unstaged = status
                .unstaged
                .is_some_and(|kind| kind != ChangeKind::Untracked);
            if has_staged || has_tracked_unstaged {
                blocking.push(status.path);
                continue;
            }
            if status.unstaged == Some(ChangeKind::Untracked)
                && let Some(tree_ref) = &tree
                && untracked_collides_with_tree(tree_ref, &status.path)?
            {
                blocking.push(status.path);
            }
        }

        if !blocking.is_empty() {
            return Err(GitError::DirtyWorkingTree { files: blocking });
        }
        Ok(())
    }

    /// Switches `HEAD` to local branch `name`, updating the working tree and index.
    ///
    /// Returns a safe no-op result when `name` is already the active branch.
    ///
    /// # Errors
    /// [`GitError::BranchNotFound`], [`GitError::DirtyWorkingTree`], or checkout failures.
    pub fn checkout_local(&self, name: &str) -> GitResult<CheckoutResult> {
        let repo = self.local();
        let full_ref_str = format!("refs/heads/{name}");
        let mut target_ref = repo
            .try_find_reference(&full_ref_str)
            .map_err(GitError::gix)?
            .ok_or_else(|| GitError::BranchNotFound(name.to_owned()))?;

        let previous = read_head(&repo)?;
        if matches!(&previous, HeadState::Branch(current) if current == name) {
            return Ok(CheckoutResult {
                previous: previous.clone(),
                current: previous,
                changed_files: false,
            });
        }

        let target_commit_id = target_ref.peel_to_id().map_err(GitError::gix)?.detach();
        let target_commit = repo.find_commit(target_commit_id).map_err(GitError::gix)?;
        let target_tree_id = target_commit.tree_id().map_err(GitError::gix)?.detach();

        self.validate_clean_for_checkout(Some(target_tree_id))?;

        let old_tree_id = repo.head_tree_id_or_empty().ok().map(gix::Id::detach);
        let changed_files = self.checkout_tree_and_index(&repo, target_tree_id, old_tree_id)?;

        let branch_full_name = FullName::try_from(full_ref_str).map_err(GitError::gix)?;
        let log_msg = format!("checkout: moving from {} to {name}", previous.label());
        update_head_symbolic(&repo, branch_full_name, &log_msg)?;

        let current = read_head(&repo)?;
        Ok(CheckoutResult {
            previous,
            current,
            changed_files,
        })
    }

    /// Creates a local tracking branch for `remote_branch` (e.g. `origin/main`) and checks it out.
    ///
    /// # Errors
    /// [`GitError::BranchNotFound`], [`GitError::BranchAlreadyExists`],
    /// [`GitError::DirtyWorkingTree`], or checkout failures.
    pub fn checkout_remote(&self, remote_branch: &str) -> GitResult<CheckoutResult> {
        let mut repo = self.local();
        drop(repo.reload());
        let remote_full_ref = format!("refs/remotes/{remote_branch}");
        let mut remote_ref = repo
            .try_find_reference(&remote_full_ref)
            .map_err(GitError::gix)?
            .ok_or_else(|| GitError::BranchNotFound(remote_branch.to_owned()))?;

        let remote_names: Vec<String> = repo
            .remote_names()
            .into_iter()
            .map(|r| r.to_str_lossy().into_owned())
            .collect();
        let Some((remote_name, short_name)) = split_remote_branch(remote_branch, &remote_names)
        else {
            return Err(GitError::InvalidBranchName(remote_branch.to_owned()));
        };

        let target_commit_id = remote_ref.peel_to_id().map_err(GitError::gix)?.detach();
        let target_commit = repo.find_commit(target_commit_id).map_err(GitError::gix)?;
        let target_tree_id = target_commit.tree_id().map_err(GitError::gix)?.detach();

        let local_full_ref = format!("refs/heads/{short_name}");
        if let Some(mut existing) = repo
            .try_find_reference(&local_full_ref)
            .map_err(GitError::gix)?
        {
            let existing_id = existing.peel_to_id().map_err(GitError::gix)?.detach();
            if existing_id != target_commit_id {
                return Err(GitError::BranchAlreadyExists(short_name));
            }
            return self.checkout_local(&short_name);
        }

        self.validate_clean_for_checkout(Some(target_tree_id))?;

        let local_name = FullName::try_from(local_full_ref.clone()).map_err(GitError::gix)?;
        let edit = RefEdit::update(
            local_name,
            Target::Object(target_commit_id),
            PreviousValue::MustNotExist,
            format!("branch: Created from {remote_branch}"),
        );
        edit_references_with_fallback(&repo, [edit])?;

        write_branch_tracking_config(&repo, &short_name, &remote_name, &local_full_ref)?;

        self.checkout_local(&short_name)
    }

    /// Creates a new local branch `name` at the current `HEAD` commit and switches `HEAD` to it.
    ///
    /// # Errors
    /// [`GitError::InvalidBranchName`], [`GitError::UnbornHead`], or
    /// [`GitError::BranchAlreadyExists`].
    pub fn create_branch_and_checkout(&self, name: &str) -> GitResult<HeadState> {
        validate_branch_short_name(name)?;

        let repo = self.local();
        let head = repo.head().map_err(GitError::gix)?;
        if head.is_unborn() {
            return Err(GitError::UnbornHead);
        }
        let head_id = head.id().ok_or(GitError::UnbornHead)?.detach();
        let previous_label = read_head(&repo)?.label().to_owned();

        let full_ref_str = format!("refs/heads/{name}");
        if repo
            .try_find_reference(&full_ref_str)
            .map_err(GitError::gix)?
            .is_some()
        {
            return Err(GitError::BranchAlreadyExists(name.to_owned()));
        }

        let branch_full_name = FullName::try_from(full_ref_str).map_err(GitError::gix)?;
        let head_full_name = FullName::try_from("HEAD").map_err(GitError::gix)?;
        let edits = [
            RefEdit::update(
                branch_full_name.clone(),
                Target::Object(head_id),
                PreviousValue::MustNotExist,
                format!("branch: Created from {previous_label}"),
            ),
            RefEdit::update(
                head_full_name,
                Target::Symbolic(branch_full_name),
                PreviousValue::Any,
                format!("checkout: moving from {previous_label} to {name}"),
            ),
        ];
        edit_references_with_fallback(&repo, edits)?;

        read_head(&repo)
    }

    /// Deletes local branch `name`.
    ///
    /// When `force` is `false`, returns [`GitError::BranchNotMerged`] if the branch tip is not
    /// an ancestor of `HEAD`.
    ///
    /// # Errors
    /// [`GitError::CannotDeleteCurrentBranch`], [`GitError::BranchNotFound`], or
    /// [`GitError::BranchNotMerged`].
    pub fn delete_branch(&self, name: &str, force: bool) -> GitResult<()> {
        let mut repo = self.local();
        let head = read_head(&repo)?;
        if matches!(&head, HeadState::Branch(current) if current == name) {
            return Err(GitError::CannotDeleteCurrentBranch(name.to_owned()));
        }

        let full_ref_str = format!("refs/heads/{name}");
        let mut branch_ref = repo
            .try_find_reference(&full_ref_str)
            .map_err(GitError::gix)?
            .ok_or_else(|| GitError::BranchNotFound(name.to_owned()))?;

        if !force {
            let branch_id = branch_ref.peel_to_id().map_err(GitError::gix)?.detach();
            let head_id = repo
                .head()
                .map_err(GitError::gix)?
                .id()
                .map(gix::Id::detach);
            let Some(head_id) = head_id else {
                return Err(GitError::BranchNotMerged(name.to_owned()));
            };
            if branch_id != head_id {
                let has_unmerged = repo
                    .rev_walk([branch_id])
                    .with_hidden([head_id])
                    .all()
                    .map_err(GitError::gix)?
                    .next()
                    .is_some();
                if has_unmerged {
                    return Err(GitError::BranchNotMerged(name.to_owned()));
                }
            }
        }
        drop(branch_ref);

        let full_name = FullName::try_from(full_ref_str).map_err(GitError::gix)?;
        repo.delete_local_branches([full_name])
            .map_err(GitError::gix)?;
        Ok(())
    }

    /// Runs `git fetch --all --prune` in the repository working tree (intended for background
    /// execution).
    ///
    /// # Errors
    /// [`GitError::RemoteCommandFailed`] when `git fetch` fails or cannot spawn.
    pub fn fetch(&self) -> GitResult<FetchSummary> {
        let before = self.snapshot_remote_oids()?;
        let output = self.run_git_command("fetch", &["fetch", "--all", "--prune"])?;
        let after = self.snapshot_remote_oids()?;
        let changed = before != after;
        let head = self.head()?;
        let message = if output.is_empty() {
            if changed {
                "Fetched remote updates".to_owned()
            } else {
                "Already up to date".to_owned()
            }
        } else {
            output
        };
        Ok(FetchSummary {
            head,
            changed,
            message,
        })
    }

    /// Runs `git pull --ff-only` in the repository working tree (intended for background
    /// execution).
    ///
    /// # Errors
    /// [`GitError::RemoteCommandFailed`] when `git pull` fails (including non-fast-forward
    /// divergence or missing tracking upstream).
    pub fn pull(&self) -> GitResult<PullSummary> {
        let before_id = self
            .local()
            .head()
            .ok()
            .and_then(|h| h.id().map(gix::Id::detach));
        let output = self.run_git_command("pull", &["pull", "--ff-only"])?;
        let after_id = self
            .local()
            .head()
            .ok()
            .and_then(|h| h.id().map(gix::Id::detach));
        let changed = before_id != after_id;
        let head = self.head()?;
        let message = if output.is_empty() {
            if changed {
                "Fast-forwarded current branch".to_owned()
            } else {
                "Already up to date".to_owned()
            }
        } else {
            output
        };
        Ok(PullSummary {
            head,
            changed,
            message,
        })
    }

    fn snapshot_remote_oids(&self) -> GitResult<BTreeMap<String, Option<gix::ObjectId>>> {
        let repo = self.local();
        let refs = repo.references().map_err(GitError::gix)?;
        let mut map = BTreeMap::new();
        for item in refs
            .remote_branches()
            .map_err(GitError::gix)?
            .peeled()
            .map_err(GitError::gix)?
        {
            let reference = item.map_err(GitError::gix)?;
            let name = reference.name().as_bstr().to_str_lossy().into_owned();
            let id = reference.try_id().map(gix::Id::detach);
            map.insert(name, id);
        }
        Ok(map)
    }

    fn run_git_command(&self, op: &'static str, args: &[&str]) -> GitResult<String> {
        let output = Command::new("git")
            .args(args)
            .current_dir(self.workdir())
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .output()
            .map_err(|err| GitError::RemoteCommandFailed {
                op,
                message: err.to_string(),
            })?;

        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let combined = match (stdout.is_empty(), stderr.is_empty()) {
            (false, false) => format!("{stdout}\n{stderr}"),
            (false, true) => stdout,
            (true, false) => stderr,
            (true, true) => String::new(),
        };

        if !output.status.success() {
            let message = if combined.is_empty() {
                format!("exited with status {}", output.status)
            } else {
                combined
            };
            return Err(GitError::RemoteCommandFailed { op, message });
        }

        Ok(combined)
    }

    fn checkout_tree_and_index(
        &self,
        repo: &gix::Repository,
        target_tree_id: gix::ObjectId,
        old_tree_id: Option<gix::ObjectId>,
    ) -> GitResult<bool> {
        let current_index = repo.index_or_empty().map_err(GitError::gix)?;
        let mut old_entries: BTreeMap<BString, (gix::ObjectId, Mode, Stat)> = BTreeMap::new();
        for entry in current_index.entries() {
            if entry.flags.contains(Flags::REMOVE) {
                continue;
            }
            old_entries.insert(
                entry.path(&current_index).to_owned(),
                (entry.id, entry.mode, entry.stat),
            );
        }

        let mut new_index = repo
            .index_from_tree(&target_tree_id)
            .map_err(GitError::gix)?;
        let new_paths: HashSet<BString> = new_index
            .entries()
            .iter()
            .map(|entry| entry.path(&new_index).to_owned())
            .collect();

        let mut changed = old_tree_id != Some(target_tree_id);

        // Remove files that were tracked in the old index but do not exist in the target tree.
        for old_path in old_entries.keys() {
            if new_paths.contains(old_path) {
                continue;
            }
            let rel_str = old_path.to_str_lossy();
            let abs_path = self.workdir().join(rel_str.as_ref());
            if fs::symlink_metadata(&abs_path).is_ok() {
                remove_file_and_prune_dirs(&abs_path, self.workdir())?;
                changed = true;
            }
        }

        // Write updated/new files and refresh stat metadata for all entries in `new_index`.
        let entry_count = new_index.entries().len();
        for idx in 0..entry_count {
            let (entry_bstr, id, mode) = {
                let entry = &new_index.entries()[idx];
                (entry.path(&new_index).to_owned(), entry.id, entry.mode)
            };
            let rel_str = entry_bstr.to_str_lossy();
            let abs_path = self.workdir().join(rel_str.as_ref());

            if let Some((old_id, old_mode, old_stat)) = old_entries.get(&entry_bstr)
                && *old_id == id
                && *old_mode == mode
                && fs::symlink_metadata(&abs_path).is_ok()
            {
                new_index.entries_mut()[idx].stat = *old_stat;
                continue;
            }

            // If an untracked empty directory exists where a file should be written, remove it;
            // if a file exists where a parent dir is needed, clean up.
            write_worktree_entry(repo, &abs_path, id, mode)?;
            let fs_meta = gix::index::fs::Metadata::from_path_no_follow(&abs_path)?;
            new_index.entries_mut()[idx].stat = Stat::from_fs(&fs_meta).unwrap_or_default();
            changed = true;
        }

        new_index.sort_entries();
        drop(new_index.remove_tree());
        drop(new_index.remove_resolve_undo());
        new_index
            .write(
                gix::index::write::Options::default(),
                gix::fs::SharedPermissions::default(),
            )
            .map_err(GitError::gix)?;

        Ok(changed)
    }
}

fn read_head(repo: &gix::Repository) -> GitResult<HeadState> {
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

fn compute_ahead_behind(
    repo: &gix::Repository,
    tracking_full: &gix::refs::FullNameRef,
    local_id: Option<gix::ObjectId>,
) -> Option<(usize, usize)> {
    let mut upstream_ref = repo.try_find_reference(tracking_full).ok()??;
    let local_id = local_id?;
    let upstream_id = upstream_ref.peel_to_id().ok()?.detach();
    if local_id == upstream_id {
        return Some((0, 0));
    }
    let ahead = repo
        .rev_walk([local_id])
        .with_hidden([upstream_id])
        .all()
        .ok()
        .map(|iter| iter.filter_map(Result::ok).count())?;
    let behind = repo
        .rev_walk([upstream_id])
        .with_hidden([local_id])
        .all()
        .ok()
        .map(|iter| iter.filter_map(Result::ok).count())?;
    Some((ahead, behind))
}

fn split_remote_branch(name: &str, remote_names: &[String]) -> Option<(String, String)> {
    let mut best_remote: Option<&str> = None;
    for remote in remote_names {
        let prefix = format!("{remote}/");
        if name.starts_with(&prefix)
            && best_remote.is_none_or(|current| remote.len() > current.len())
        {
            best_remote = Some(remote.as_str());
        }
    }
    if let Some(remote) = best_remote {
        let short = name.strip_prefix(remote)?.strip_prefix('/')?;
        if !short.is_empty() {
            return Some((remote.to_owned(), short.to_owned()));
        }
    }
    let (remote, short) = name.split_once('/')?;
    if remote.is_empty() || short.is_empty() {
        return None;
    }
    Some((remote.to_owned(), short.to_owned()))
}

fn untracked_collides_with_tree(tree: &gix::Tree<'_>, path: &str) -> GitResult<bool> {
    if tree
        .lookup_entry_by_path(path)
        .map_err(GitError::gix)?
        .is_some()
    {
        return Ok(true);
    }
    let mut prefix = String::new();
    for component in Path::new(path).components() {
        let Component::Normal(part) = component else {
            continue;
        };
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(&part.to_string_lossy());
        if prefix == path {
            break;
        }
        if let Some(entry) = tree.lookup_entry_by_path(&prefix).map_err(GitError::gix)?
            && !entry.mode().is_tree()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn validate_branch_short_name(name: &str) -> GitResult<()> {
    if name.is_empty()
        || name.starts_with('-')
        || name == "HEAD"
        || name.chars().any(char::is_whitespace)
    {
        return Err(GitError::InvalidBranchName(name.to_owned()));
    }
    let full = format!("refs/heads/{name}");
    let bstr: &BStr = full.as_bytes().as_bstr();
    gix::validate::reference::branch_name(bstr)
        .map_err(|_| GitError::InvalidBranchName(name.to_owned()))?;
    Ok(())
}

fn edit_references_with_fallback(
    repo: &gix::Repository,
    edits: impl IntoIterator<Item = RefEdit>,
) -> GitResult<()> {
    if let Some(committer) = repo.committer().transpose().map_err(GitError::gix)? {
        repo.edit_references_as(edits, Some(committer))
            .map_err(GitError::gix)?;
    } else {
        let fallback = gix::actor::Signature {
            name: "Kova".into(),
            email: "kova@local".into(),
            time: gix::date::Time::now_local_or_utc(),
        };
        let mut time_buf = gix::date::parse::TimeBuf::default();
        repo.edit_references_as(edits, Some(fallback.to_ref(&mut time_buf)))
            .map_err(GitError::gix)?;
    }
    Ok(())
}

fn update_head_symbolic(
    repo: &gix::Repository,
    branch_full_name: FullName,
    log_msg: &str,
) -> GitResult<()> {
    let head_full_name = FullName::try_from("HEAD").map_err(GitError::gix)?;
    let edit = RefEdit::update(
        head_full_name,
        Target::Symbolic(branch_full_name),
        PreviousValue::Any,
        log_msg,
    );
    edit_references_with_fallback(repo, [edit])
}

fn write_branch_tracking_config(
    repo: &gix::Repository,
    short_name: &str,
    remote_name: &str,
    local_full_ref: &str,
) -> GitResult<()> {
    let config_path = repo.common_dir().join("config");
    let mut config = repo.config_file_mut(&config_path).map_err(GitError::gix)?;
    let mut section = config
        .section_mut_or_create_new("branch", Some(short_name.as_bytes().as_bstr()))
        .map_err(GitError::gix)?;
    section
        .set("remote", remote_name.as_bytes().as_bstr())
        .map_err(GitError::gix)?;
    section
        .set("merge", local_full_ref.as_bytes().as_bstr())
        .map_err(GitError::gix)?;
    config.commit().map_err(GitError::gix)?;
    Ok(())
}

fn write_worktree_entry(
    repo: &gix::Repository,
    abs_path: &Path,
    id: gix::ObjectId,
    mode: Mode,
) -> GitResult<()> {
    if let Some(parent) = abs_path.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Ok(meta) = fs::symlink_metadata(abs_path) {
        if meta.file_type().is_dir() {
            fs::remove_dir_all(abs_path)?;
        } else if meta.file_type().is_symlink() {
            fs::remove_file(abs_path)?;
        }
    }
    let object = repo.find_object(id).map_err(GitError::gix)?;
    if mode == Mode::SYMLINK {
        let target = String::from_utf8_lossy(&object.data).into_owned();
        create_symlink(&target, abs_path, &object.data)?;
        return Ok(());
    }
    fs::write(abs_path, &object.data)?;
    set_executable_permission(abs_path, mode == Mode::FILE_EXECUTABLE)?;
    Ok(())
}

#[cfg(unix)]
fn create_symlink(target: &str, link_path: &Path, _raw: &[u8]) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link_path)
}

#[cfg(not(unix))]
fn create_symlink(_target: &str, link_path: &Path, raw: &[u8]) -> std::io::Result<()> {
    fs::write(link_path, raw)
}

#[cfg(unix)]
fn set_executable_permission(path: &Path, executable: bool) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let perms = fs::metadata(path)?.permissions();
    let mode = perms.mode();
    let new_mode = if executable {
        mode | 0o111
    } else {
        mode & !0o111
    };
    if new_mode != mode {
        fs::set_permissions(path, fs::Permissions::from_mode(new_mode))?;
    }
    Ok(())
}

#[cfg(not(unix))]
const fn set_executable_permission(_path: &Path, _executable: bool) -> std::io::Result<()> {
    Ok(())
}

fn remove_file_and_prune_dirs(path: &Path, workdir: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path)?,
        Ok(_) => fs::remove_file(path)?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    }
    let mut current = path.parent();
    while let Some(dir) = current {
        if dir == workdir || !dir.starts_with(workdir) {
            break;
        }
        if fs::remove_dir(dir).is_err() {
            break;
        }
        current = dir.parent();
    }
    Ok(())
}
