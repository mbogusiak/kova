//! Headless `gix` integration tests (KOVA-043): status, gutter diff, blame and commit on
//! temporary repositories, without any external `git` process.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test harness: failures must abort the test with context"
)]

use std::fs;
use std::path::Path;

use kova_tools::git::{
    ChangeKind, CommitSignature, DiffLineKind, FileStatus, GitError, GitRepository, HeadState,
    LineChangeKind, base_line_for,
};

fn sig(name: &str) -> CommitSignature {
    CommitSignature {
        name: name.to_owned(),
        email: format!("{}@example.com", name.to_lowercase()),
    }
}

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, text).unwrap();
}

fn status_of<'a>(statuses: &'a [FileStatus], path: &str) -> &'a FileStatus {
    statuses
        .iter()
        .find(|s| s.path == path)
        .unwrap_or_else(|| panic!("no status for {path}: {statuses:?}"))
}

fn fresh_repo() -> (tempfile::TempDir, GitRepository) {
    let dir = tempfile::tempdir().unwrap();
    let repo = GitRepository::init(dir.path()).unwrap();
    (dir, repo)
}

#[test]
fn discovers_repository_and_reads_branch() {
    let (dir, repo) = fresh_repo();
    assert!(matches!(repo.head().unwrap(), HeadState::Unborn(_)));

    write(dir.path(), "src/Main.java", "class Main {}\n");
    let nested = GitRepository::discover(dir.path().join("src")).unwrap();
    assert_eq!(nested.workdir(), repo.workdir());

    repo.stage("src/Main.java").unwrap();
    repo.commit("Initial commit", Some(&sig("Ada"))).unwrap();
    let head = repo.head().unwrap();
    assert!(matches!(head, HeadState::Branch(_)), "{head:?}");
    assert_ne!(head.label(), "");

    let outside = tempfile::tempdir().unwrap();
    assert!(matches!(
        GitRepository::discover(outside.path()),
        Err(GitError::NotARepository(_))
    ));
    assert!(matches!(
        repo.relative_path(outside.path().join("x.txt")),
        Err(GitError::OutsideWorkingTree(_))
    ));
    assert_eq!(
        repo.relative_path(dir.path().join("src/Main.java"))
            .unwrap(),
        "src/Main.java"
    );
}

#[test]
fn status_reports_untracked_staged_modified_and_deleted() {
    let (dir, repo) = fresh_repo();
    write(dir.path(), "a.txt", "alpha\n");
    write(dir.path(), "b.txt", "beta\n");

    let status = repo.status().unwrap();
    assert_eq!(
        status_of(&status, "a.txt").unstaged,
        Some(ChangeKind::Untracked)
    );
    assert_eq!(status_of(&status, "a.txt").staged, None);

    repo.stage("a.txt").unwrap();
    repo.stage(dir.path().join("b.txt")).unwrap();
    let status = repo.status().unwrap();
    assert_eq!(status_of(&status, "a.txt").staged, Some(ChangeKind::Added));
    assert!(status_of(&status, "a.txt").is_fully_staged());

    repo.commit("Add files", Some(&sig("Ada"))).unwrap();
    assert_eq!(repo.status().unwrap(), Vec::new());

    write(dir.path(), "a.txt", "alpha\nmore\n");
    fs::remove_file(dir.path().join("b.txt")).unwrap();
    write(dir.path(), "c.txt", "gamma\n");
    let status = repo.status().unwrap();
    assert_eq!(status.len(), 3, "{status:?}");
    assert_eq!(
        status_of(&status, "a.txt").unstaged,
        Some(ChangeKind::Modified)
    );
    assert_eq!(
        status_of(&status, "b.txt").unstaged,
        Some(ChangeKind::Deleted)
    );
    assert_eq!(
        status_of(&status, "c.txt").unstaged,
        Some(ChangeKind::Untracked)
    );
    assert_eq!(status_of(&status, "c.txt").display_kind().code(), '?');

    repo.stage("b.txt").unwrap();
    let status = repo.status().unwrap();
    assert_eq!(
        status_of(&status, "b.txt").staged,
        Some(ChangeKind::Deleted)
    );
    assert_eq!(status_of(&status, "b.txt").unstaged, None);
}

#[test]
fn stage_and_unstage_round_trip() {
    let (dir, repo) = fresh_repo();
    write(dir.path(), "a.txt", "one\n");
    repo.stage("a.txt").unwrap();
    repo.commit("one", Some(&sig("Ada"))).unwrap();

    write(dir.path(), "a.txt", "two\n");
    repo.stage("a.txt").unwrap();
    let staged = repo.status().unwrap();
    assert_eq!(
        status_of(&staged, "a.txt").staged,
        Some(ChangeKind::Modified)
    );
    assert_eq!(status_of(&staged, "a.txt").unstaged, None);

    repo.unstage("a.txt").unwrap();
    let unstaged = repo.status().unwrap();
    assert_eq!(status_of(&unstaged, "a.txt").staged, None);
    assert_eq!(
        status_of(&unstaged, "a.txt").unstaged,
        Some(ChangeKind::Modified)
    );

    // Unstaging a file that is not in HEAD makes it untracked again.
    write(dir.path(), "new.txt", "fresh\n");
    repo.stage("new.txt").unwrap();
    repo.unstage("new.txt").unwrap();
    let status = repo.status().unwrap();
    assert_eq!(status_of(&status, "new.txt").staged, None);
    assert_eq!(
        status_of(&status, "new.txt").unstaged,
        Some(ChangeKind::Untracked)
    );
}

#[test]
fn gutter_diff_tracks_live_buffer_against_head() {
    let (dir, repo) = fresh_repo();
    write(dir.path(), "Main.java", "a\nb\nc\nd\n");
    repo.stage("Main.java").unwrap();
    repo.commit("base", Some(&sig("Ada"))).unwrap();

    // Unsaved buffer content: no file write needed for the gutter.
    let buffer = "a\nB\nc\nnew\n";
    let hunks = repo.gutter_diff("Main.java", buffer).unwrap();
    let kinds: Vec<_> = hunks
        .iter()
        .map(|h| (h.kind, h.start_line, h.line_count))
        .collect();
    assert_eq!(
        kinds,
        vec![
            (LineChangeKind::Modified, 1, 1),
            (LineChangeKind::Modified, 3, 1)
        ]
    );
    assert_eq!(
        repo.gutter_diff("Main.java", "a\nb\nc\nd\n").unwrap(),
        Vec::new()
    );

    let deleted = repo.gutter_diff("Main.java", "a\nd\n").unwrap();
    assert_eq!(deleted.len(), 1);
    assert_eq!(deleted[0].kind, LineChangeKind::Deleted);
    assert_eq!(deleted[0].start_line, 1);

    let untracked = repo.gutter_diff("Other.java", "x\ny\n").unwrap();
    assert_eq!(untracked.len(), 1);
    assert_eq!(untracked[0].kind, LineChangeKind::Added);
    assert_eq!(untracked[0].line_count, 2);

    write(dir.path(), "Main.java", "a\nb\nc\nd\ne\n");
    let preview = repo.file_diff("Main.java").unwrap();
    assert_eq!(preview.last().unwrap().kind, DiffLineKind::Added);
    assert_eq!(preview.last().unwrap().text, "e");
}

#[test]
fn blame_attributes_lines_to_commits() {
    let (dir, repo) = fresh_repo();
    write(dir.path(), "notes.txt", "first\nsecond\n");
    repo.stage("notes.txt").unwrap();
    let base = repo.commit("Add notes", Some(&sig("Ada"))).unwrap();

    write(dir.path(), "notes.txt", "first\nsecond\nthird\n");
    repo.stage("notes.txt").unwrap();
    let second = repo
        .commit("Append third line\n\nBody text.", Some(&sig("Grace")))
        .unwrap();
    assert_eq!(second.summary, "Append third line");

    let blame = repo.blame("notes.txt").unwrap();
    assert_eq!(blame.line_count(), 3);
    let first = blame.line(0).unwrap();
    assert_eq!(first.author, "Ada");
    assert_eq!(first.commit_id, base.id);
    assert_eq!(first.summary, "Add notes");
    let third = blame.line(2).unwrap();
    assert_eq!(third.author, "Grace");
    assert_eq!(third.email, "grace@example.com");
    assert_eq!(third.short_id(), second.short_id());
    assert!(third.inline_label().contains("Append third line"));
    assert!(blame.line(3).is_none());

    let single = repo.blame_line("notes.txt", 1).unwrap().unwrap();
    assert_eq!(single.author, "Ada");

    // Buffer line 1 was inserted, so line 2 maps back to HEAD line 1.
    let hunks = repo
        .gutter_diff("notes.txt", "first\nX\nsecond\nthird\n")
        .unwrap();
    assert_eq!(base_line_for(&hunks, 1), None);
    assert_eq!(base_line_for(&hunks, 2), Some(1));

    assert!(repo.blame("missing.txt").unwrap().is_empty());
}

#[test]
fn commit_validates_message_and_changes() {
    let (dir, repo) = fresh_repo();
    assert!(matches!(
        repo.commit("   ", Some(&sig("Ada"))),
        Err(GitError::EmptyCommitMessage)
    ));
    assert!(matches!(
        repo.commit("nothing", Some(&sig("Ada"))),
        Err(GitError::NothingToCommit)
    ));

    write(dir.path(), "dir/a.txt", "a\n");
    write(dir.path(), "dir/sub/b.txt", "b\n");
    repo.stage("dir/a.txt").unwrap();
    repo.stage("dir/sub/b.txt").unwrap();
    let first = repo.commit("nested tree", Some(&sig("Ada"))).unwrap();
    assert_eq!(first.short_id().len(), 7);
    assert_eq!(
        repo.head_contents("dir/sub/b.txt").unwrap().as_deref(),
        Some(&b"b\n"[..])
    );
    assert!(matches!(
        repo.commit("again", Some(&sig("Ada"))),
        Err(GitError::NothingToCommit)
    ));

    // Only staged changes are committed.
    write(dir.path(), "dir/a.txt", "a2\n");
    write(dir.path(), "dir/sub/b.txt", "b2\n");
    repo.stage("dir/a.txt").unwrap();
    repo.commit("update a", Some(&sig("Ada"))).unwrap();
    assert_eq!(
        repo.head_contents("dir/a.txt").unwrap().as_deref(),
        Some(&b"a2\n"[..])
    );
    assert_eq!(
        repo.head_contents("dir/sub/b.txt").unwrap().as_deref(),
        Some(&b"b\n"[..])
    );
    let status = repo.status().unwrap();
    assert_eq!(status.len(), 1);
    assert_eq!(
        status_of(&status, "dir/sub/b.txt").unstaged,
        Some(ChangeKind::Modified)
    );
}

#[test]
fn commit_uses_configured_identity_and_honours_cancellation() {
    let (dir, repo) = fresh_repo();
    write(dir.path(), "a.txt", "a\n");
    repo.stage("a.txt").unwrap();

    let config = dir.path().join(".git/config");
    let mut text = fs::read_to_string(&config).unwrap();
    text.push_str("[user]\n\tname = Configured Dev\n\temail = dev@example.com\n");
    fs::write(&config, text).unwrap();
    let repo = GitRepository::discover(dir.path()).unwrap();
    let summary = repo.commit("From config", None).unwrap();
    let info = repo.blame_line("a.txt", 0).unwrap().unwrap();
    assert_eq!(info.author, "Configured Dev");
    assert_eq!(info.email, "dev@example.com");
    assert_eq!(info.commit_id, summary.id);

    write(dir.path(), "b.txt", "b\n");
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    assert!(matches!(
        repo.status_cancellable(&cancel),
        Err(GitError::Cancelled)
    ));
    cancel.store(false, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(repo.status_cancellable(&cancel).unwrap().len(), 1);
}

#[test]
fn repository_handle_is_shareable_across_threads() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<GitRepository>();

    let (dir, repo) = fresh_repo();
    write(dir.path(), "a.txt", "a\n");
    let repo = std::sync::Arc::new(repo);
    let worker = {
        let repo = std::sync::Arc::clone(&repo);
        std::thread::spawn(move || repo.status().map(|s| s.len()))
    };
    assert_eq!(worker.join().unwrap().unwrap(), 1);
}
