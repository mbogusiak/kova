//! Per-editor Git state: `HEAD` base text, live gutter hunks and blame for the inline
//! annotation. Pure data — the background scheduling lives in [`crate::editor::EditorView`].

use std::sync::Arc;

use kova_tools::git::{FileBlame, GitRepository, GutterHunk, LineChangeKind, base_line_for};

/// Label shown by inline blame on lines that differ from `HEAD`.
pub const UNCOMMITTED_LABEL: &str = "Uncommitted changes";

/// Git context of the file shown in an editor.
#[derive(Debug, Clone)]
pub struct EditorGitState {
    repo: Option<Arc<GitRepository>>,
    rel_path: Option<String>,
    /// `HEAD` text (`""` when the file is not committed); `None` until loaded.
    base: Option<Arc<str>>,
    hunks: Arc<[GutterHunk]>,
    blame: Option<Arc<FileBlame>>,
    /// Buffer version the current `hunks` were computed for.
    diffed_version: Option<u64>,
    /// Buffer version of the diff running in the background, if any.
    diff_in_flight: Option<u64>,
    /// Bumped whenever the file or its `HEAD` base changes; stale results are dropped.
    base_epoch: u64,
    inline_blame: bool,
}

impl Default for EditorGitState {
    fn default() -> Self {
        Self {
            repo: None,
            rel_path: None,
            base: None,
            hunks: Arc::from(Vec::new()),
            blame: None,
            diffed_version: None,
            diff_in_flight: None,
            base_epoch: 0,
            inline_blame: true,
        }
    }
}

impl EditorGitState {
    /// Attaches the editor to `rel_path` in `repo`, clearing every derived result.
    /// Returns the new base epoch.
    pub fn attach(&mut self, repo: Arc<GitRepository>, rel_path: String) -> u64 {
        self.repo = Some(repo);
        self.rel_path = Some(rel_path);
        self.base = None;
        self.blame = None;
        self.hunks = Arc::from(Vec::new());
        self.diffed_version = None;
        self.bump_epoch()
    }

    /// Invalidates in-flight work after `HEAD` moved (e.g. a commit). Returns the new epoch.
    pub fn bump_epoch(&mut self) -> u64 {
        self.base_epoch = self.base_epoch.wrapping_add(1);
        self.diff_in_flight = None;
        self.base_epoch
    }

    /// Returns the repository and path when the editor shows a file inside a repository.
    #[must_use]
    pub fn file(&self) -> Option<(Arc<GitRepository>, String)> {
        Some((Arc::clone(self.repo.as_ref()?), self.rel_path.clone()?))
    }

    /// Returns the current base epoch.
    #[must_use]
    pub const fn epoch(&self) -> u64 {
        self.base_epoch
    }

    /// Stores the `HEAD` text loaded for `epoch` (ignored when stale).
    pub fn set_base(&mut self, epoch: u64, base: Option<String>) {
        if epoch == self.base_epoch {
            self.base = base.map(Arc::from);
            self.diffed_version = None;
        }
    }

    /// Stores the blame loaded for `epoch` (ignored when stale).
    pub fn set_blame(&mut self, epoch: u64, blame: Option<FileBlame>) {
        if epoch == self.base_epoch {
            self.blame = blame.map(Arc::new);
        }
    }

    /// Starts a diff for buffer `version` when one is due; returns the base text to diff
    /// against and the epoch to report back with.
    pub fn begin_diff(&mut self, version: u64) -> Option<(Arc<str>, u64)> {
        let base = self.base.as_ref()?;
        if self.diffed_version == Some(version) || self.diff_in_flight.is_some() {
            return None;
        }
        self.diff_in_flight = Some(version);
        Some((Arc::clone(base), self.base_epoch))
    }

    /// Stores hunks computed for buffer `version` against the base of `epoch`.
    ///
    /// Results for an older buffer version are still shown (so markers never flicker) and
    /// the next render schedules a fresh diff; results for a stale base are dropped.
    pub fn finish_diff(&mut self, version: u64, epoch: u64, hunks: Vec<GutterHunk>) {
        if epoch == self.base_epoch {
            self.diff_in_flight = None;
            self.hunks = Arc::from(hunks);
            self.diffed_version = Some(version);
        }
    }

    /// Returns the current gutter hunks (sorted by line).
    #[must_use]
    pub fn hunks(&self) -> &[GutterHunk] {
        &self.hunks
    }

    /// Returns the gutter marker painted on buffer row `row`.
    #[must_use]
    pub fn marker_at(&self, row: usize) -> Option<LineChangeKind> {
        let idx = self
            .hunks
            .partition_point(|h| h.start_line + h.line_count.max(1) <= row);
        self.hunks
            .get(idx)
            .filter(|h| h.covers(row))
            .map(|h| h.kind)
    }

    /// Enables or disables the inline blame annotation.
    pub const fn set_inline_blame(&mut self, enabled: bool) {
        self.inline_blame = enabled;
    }

    /// Returns `true` when the inline blame annotation is enabled.
    #[must_use]
    pub const fn inline_blame_enabled(&self) -> bool {
        self.inline_blame
    }

    /// Returns the inline blame text for buffer row `row`, if any.
    #[must_use]
    pub fn blame_label(&self, row: usize) -> Option<String> {
        if !self.inline_blame || self.diffed_version.is_none() {
            return None;
        }
        if matches!(
            self.marker_at(row),
            Some(LineChangeKind::Added | LineChangeKind::Modified)
        ) {
            return Some(UNCOMMITTED_LABEL.to_owned());
        }
        let base_row = base_line_for(&self.hunks, row)?;
        self.blame
            .as_ref()?
            .line(base_row)
            .map(kova_tools::git::BlameInfo::inline_label)
    }
}

#[cfg(test)]
mod tests {
    use kova_tools::git::gutter_hunks;

    use super::*;

    fn state_with(base: &str, buffer: &str) -> EditorGitState {
        let mut state = EditorGitState::default();
        let epoch = state.epoch();
        state.set_base(epoch, Some(base.to_owned()));
        let (base_text, epoch) = state.begin_diff(1).expect("diff due");
        state.finish_diff(1, epoch, gutter_hunks(&base_text, buffer));
        state
    }

    #[test]
    fn markers_follow_hunks() {
        let state = state_with("a\nb\nc\nd\n", "a\nB\nc\nx\ny\nd\n");
        assert_eq!(state.marker_at(0), None);
        assert_eq!(state.marker_at(1), Some(LineChangeKind::Modified));
        assert_eq!(state.marker_at(2), None);
        assert_eq!(state.marker_at(3), Some(LineChangeKind::Added));
        assert_eq!(state.marker_at(4), Some(LineChangeKind::Added));
        assert_eq!(state.marker_at(5), None);

        let deleted = state_with("a\nb\nc\n", "a\nc\n");
        assert_eq!(deleted.marker_at(1), Some(LineChangeKind::Deleted));
        assert_eq!(deleted.marker_at(0), None);
    }

    #[test]
    fn diff_scheduling_is_single_flight_and_versioned() {
        let mut state = EditorGitState::default();
        assert!(state.begin_diff(1).is_none(), "no base loaded yet");
        let epoch = state.epoch();
        state.set_base(epoch, Some("a\n".to_owned()));
        let (_, epoch) = state.begin_diff(1).expect("first diff");
        assert!(state.begin_diff(2).is_none(), "one diff in flight");
        state.finish_diff(1, epoch, Vec::new());
        assert!(state.begin_diff(1).is_none(), "version 1 already diffed");
        assert!(state.begin_diff(2).is_some(), "new version is due");
    }

    #[test]
    fn stale_base_results_are_dropped() {
        let mut state = state_with("a\n", "b\n");
        let old_epoch = state.epoch();
        state.bump_epoch();
        state.finish_diff(9, old_epoch, Vec::new());
        assert_eq!(state.hunks().len(), 1, "stale diff ignored");
        state.set_base(old_epoch, None);
        assert_eq!(state.marker_at(0), Some(LineChangeKind::Modified));
    }

    #[test]
    fn bump_epoch_unblocks_in_flight_diff_and_ignores_stale_finish() {
        let mut state = EditorGitState::default();
        let e1 = state.epoch();
        state.set_base(e1, Some("a\n".to_owned()));
        assert!(state.begin_diff(1).is_some());

        // Base changes (e.g. commit) while diff for e1 is still running.
        let e2 = state.bump_epoch();
        state.set_base(e2, Some("b\n".to_owned()));
        let (_, started_epoch) = state.begin_diff(1).expect("new epoch must not be blocked");
        assert_eq!(started_epoch, e2);

        // Late completion from e1 must not clear e2's in-flight guard.
        state.finish_diff(1, e1, Vec::new());
        assert!(state.begin_diff(2).is_none(), "e2 diff still in flight");

        state.finish_diff(1, e2, gutter_hunks("b\n", "c\n"));
        assert_eq!(state.marker_at(0), Some(LineChangeKind::Modified));
    }

    #[test]
    fn inline_blame_marks_uncommitted_lines() {
        let mut state = state_with("a\nb\n", "a\nB\n");
        assert_eq!(state.blame_label(1).as_deref(), Some(UNCOMMITTED_LABEL));
        assert_eq!(state.blame_label(0), None, "no blame loaded");
        state.set_inline_blame(false);
        assert_eq!(state.blame_label(1), None);
    }
}
