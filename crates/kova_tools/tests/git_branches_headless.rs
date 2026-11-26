//! Headless `gix` branch integration tests (`KOVA-060`) on temporary repositories.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test harness: failures must abort the test with context"
)]

use std::fs;
use std::path::Path;
use std::process::Command;

use kova_tools::git::{BranchKind, CommitSignature, GitError, GitRepository, HeadState};

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

fn run_git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "tester@example.com")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "tester@example.com")
        .output()
        .expect("git binary must exist for headless branch tests");
    assert!(
        out.status.success(),
        "git {args:?} failed in {}: stdout={}, stderr={}",
        dir.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn fresh_repo_with_commit() -> (tempfile::TempDir, GitRepository) {
    let dir = tempfile::tempdir().unwrap();
    let repo = GitRepository::init(dir.path()).unwrap();
    write(dir.path(), "README.md", "hello\n");
    repo.stage("README.md").unwrap();
    repo.commit("initial", Some(&sig("Ada"))).unwrap();
    (dir, repo)
}

#[test]
fn create_checkout_list_and_delete_local_branches() {
    let (dir, repo) = fresh_repo_with_commit();
    let initial_branch = repo.head().unwrap().label().to_owned();

    // Unborn error check on an empty repo.
    let unborn_dir = tempfile::tempdir().unwrap();
    let unborn_repo = GitRepository::init(unborn_dir.path()).unwrap();
    assert!(matches!(
        unborn_repo.create_branch_and_checkout("feat/x"),
        Err(GitError::UnbornHead)
    ));

    // Invalid branch names rejected.
    for bad in ["", "has space", "a..b", "foo.lock", "-leading-dash", "HEAD"] {
        assert!(
            matches!(
                repo.create_branch_and_checkout(bad),
                Err(GitError::InvalidBranchName(_))
            ),
            "expected `{bad}` to be rejected"
        );
    }

    // Create feature branch and commit differing files.
    let head = repo.create_branch_and_checkout("feat/ui").unwrap();
    assert_eq!(head, HeadState::Branch("feat/ui".to_owned()));
    assert!(matches!(
        repo.create_branch_and_checkout("feat/ui"),
        Err(GitError::BranchAlreadyExists(_))
    ));

    write(dir.path(), "README.md", "hello from feat/ui\n");
    write(dir.path(), "nested/only_feat.txt", "feature file\n");
    repo.stage("README.md").unwrap();
    repo.stage("nested/only_feat.txt").unwrap();
    repo.commit("feature commit", Some(&sig("Ada"))).unwrap();

    // Untracked non-colliding file survives checkout.
    write(dir.path(), "scratch.tmp", "keep me\n");

    let listed = repo.branches().unwrap();
    assert_eq!(listed.head, HeadState::Branch("feat/ui".to_owned()));
    assert_eq!(listed.local.len(), 2);
    assert_eq!(listed.local[0].name, "feat/ui");
    assert!(listed.local[0].is_current);
    assert_eq!(listed.local[0].target_oid.len(), 7);
    assert_eq!(listed.local[1].name, initial_branch);
    assert!(!listed.local[1].is_current);

    // Cannot delete current branch.
    assert!(matches!(
        repo.delete_branch("feat/ui", false),
        Err(GitError::CannotDeleteCurrentBranch(_))
    ));

    // Switch back to initial branch; feature file & pruned dir disappear, scratch remains.
    let checkout = repo.checkout_local(&initial_branch).unwrap();
    assert_eq!(checkout.previous, HeadState::Branch("feat/ui".to_owned()));
    assert_eq!(checkout.current, HeadState::Branch(initial_branch.clone()));
    assert!(checkout.changed_files);
    assert_eq!(
        fs::read_to_string(dir.path().join("README.md")).unwrap(),
        "hello\n"
    );
    assert!(!dir.path().join("nested/only_feat.txt").exists());
    assert!(!dir.path().join("nested").exists());
    assert_eq!(
        fs::read_to_string(dir.path().join("scratch.tmp")).unwrap(),
        "keep me\n"
    );
    assert_eq!(repo.status().unwrap().len(), 1); // Only scratch.tmp untracked.

    // No-op checkout of current branch succeeds cleanly.
    let noop = repo.checkout_local(&initial_branch).unwrap();
    assert!(!noop.changed_files);

    // Missing branch returns BranchNotFound.
    assert!(matches!(
        repo.checkout_local("does-not-exist"),
        Err(GitError::BranchNotFound(_))
    ));
    assert!(matches!(
        repo.delete_branch("does-not-exist", false),
        Err(GitError::BranchNotFound(_))
    ));

    // Unmerged delete fails with BranchNotMerged unless forced.
    assert!(matches!(
        repo.delete_branch("feat/ui", false),
        Err(GitError::BranchNotMerged(_))
    ));
    repo.delete_branch("feat/ui", true).unwrap();
    assert_eq!(repo.branches().unwrap().local.len(), 1);

    // Merged branch deletes cleanly with force = false.
    repo.create_branch_and_checkout("merged-topic").unwrap();
    repo.checkout_local(&initial_branch).unwrap();
    repo.delete_branch("merged-topic", false).unwrap();
    assert_eq!(repo.branches().unwrap().local.len(), 1);
}

#[test]
fn checkout_blocks_on_dirty_working_tree_and_colliding_untracked_files() {
    let (dir, repo) = fresh_repo_with_commit();
    let base_branch = repo.head().unwrap().label().to_owned();

    repo.create_branch_and_checkout("other").unwrap();
    write(dir.path(), "collide.txt", "tracked on other\n");
    repo.stage("collide.txt").unwrap();
    repo.commit("add collide", Some(&sig("Ada"))).unwrap();

    repo.checkout_local(&base_branch).unwrap();

    // 1. Unstaged modification of tracked file blocks checkout.
    write(dir.path(), "README.md", "dirty edit\n");
    assert!(matches!(
        repo.checkout_local("other"),
        Err(GitError::DirtyWorkingTree { ref files }) if files == &vec!["README.md".to_owned()]
    ));
    write(dir.path(), "README.md", "hello\n");

    // 2. Staged addition blocks checkout.
    write(dir.path(), "staged_new.txt", "staged\n");
    repo.stage("staged_new.txt").unwrap();
    assert!(matches!(
        repo.checkout_local("other"),
        Err(GitError::DirtyWorkingTree { ref files }) if files == &vec!["staged_new.txt".to_owned()]
    ));
    repo.unstage("staged_new.txt").unwrap();
    fs::remove_file(dir.path().join("staged_new.txt")).unwrap();

    // 3. Untracked file colliding with path in target branch blocks checkout.
    write(dir.path(), "collide.txt", "local untracked\n");
    assert!(matches!(
        repo.checkout_local("other"),
        Err(GitError::DirtyWorkingTree { ref files }) if files == &vec!["collide.txt".to_owned()]
    ));
}

#[test]
fn remote_branches_checkout_fetch_and_pull_against_bare_origin() {
    let tmp = tempfile::tempdir().unwrap();
    let bare_dir = tmp.path().join("origin.git");
    fs::create_dir_all(&bare_dir).unwrap();
    run_git(&bare_dir, &["init", "--bare"]);

    let seed_dir = tmp.path().join("seed");
    fs::create_dir_all(&seed_dir).unwrap();
    run_git(&seed_dir, &["init", "-b", "main"]);
    write(&seed_dir, "app.txt", "v1\n");
    run_git(&seed_dir, &["add", "."]);
    run_git(&seed_dir, &["commit", "-m", "v1"]);
    run_git(
        &seed_dir,
        &["remote", "add", "origin", bare_dir.to_str().unwrap()],
    );
    run_git(&seed_dir, &["push", "-u", "origin", "main"]);
    run_git(&seed_dir, &["checkout", "-b", "release/1.0"]);
    write(&seed_dir, "release.txt", "1.0\n");
    run_git(&seed_dir, &["add", "."]);
    run_git(&seed_dir, &["commit", "-m", "release"]);
    run_git(&seed_dir, &["push", "-u", "origin", "release/1.0"]);
    run_git(&bare_dir, &["symbolic-ref", "HEAD", "refs/heads/main"]);

    let local_dir = tmp.path().join("local");
    run_git(
        tmp.path(),
        &[
            "clone",
            bare_dir.to_str().unwrap(),
            local_dir.to_str().unwrap(),
        ],
    );

    let repo = GitRepository::discover(&local_dir).unwrap();
    let branches = repo.branches().unwrap();
    assert_eq!(branches.head, HeadState::Branch("main".to_owned()));
    assert_eq!(branches.local.len(), 1);
    assert_eq!(branches.local[0].upstream.as_deref(), Some("origin/main"));
    assert_eq!(branches.local[0].ahead_behind, Some((0, 0)));

    // origin/HEAD must be skipped; origin/main and origin/release/1.0 present alphabetically.
    let remote_names: Vec<&str> = branches.remote.iter().map(|b| b.name.as_str()).collect();
    assert_eq!(remote_names, vec!["origin/main", "origin/release/1.0"]);
    assert_eq!(
        branches.remote[1].kind,
        BranchKind::Remote {
            remote: "origin".to_owned(),
            short_name: "release/1.0".to_owned(),
        }
    );

    // Checkout remote branch creates local tracking branch.
    let co = repo.checkout_remote("origin/release/1.0").unwrap();
    assert_eq!(co.current, HeadState::Branch("release/1.0".to_owned()));
    assert_eq!(
        fs::read_to_string(local_dir.join("release.txt")).unwrap(),
        "1.0\n"
    );

    // Local commit puts branch ahead by 1, and checkout_remote detects divergence if local differs.
    repo.checkout_local("main").unwrap();
    write(&local_dir, "local.txt", "ahead\n");
    repo.stage("local.txt").unwrap();
    repo.commit("local ahead", Some(&sig("Ada"))).unwrap();
    let listed = repo.branches().unwrap();
    let main_info = listed.local.iter().find(|b| b.name == "main").unwrap();
    assert_eq!(main_info.ahead_behind, Some((1, 0)));
    assert!(matches!(
        repo.checkout_remote("origin/main"),
        Err(GitError::BranchAlreadyExists(_))
    ));

    // Push a new commit from seed onto release/1.0, then fetch & pull from local.
    write(&seed_dir, "release.txt", "1.0.1\n");
    run_git(&seed_dir, &["add", "."]);
    run_git(&seed_dir, &["commit", "-m", "patch"]);
    run_git(&seed_dir, &["push", "origin", "release/1.0"]);

    let fetch1 = repo.fetch().unwrap();
    assert!(fetch1.changed);
    let fetch2 = repo.fetch().unwrap();
    assert!(!fetch2.changed);

    repo.checkout_local("release/1.0").unwrap();
    let listed_before_pull = repo.branches().unwrap();
    let rel_info = listed_before_pull
        .local
        .iter()
        .find(|b| b.name == "release/1.0")
        .unwrap();
    assert_eq!(rel_info.upstream.as_deref(), Some("origin/release/1.0"));
    assert_eq!(rel_info.ahead_behind, Some((0, 1)));

    let pull1 = repo.pull().unwrap();
    assert!(pull1.changed);
    assert_eq!(
        fs::read_to_string(local_dir.join("release.txt")).unwrap(),
        "1.0.1\n"
    );
    let pull2 = repo.pull().unwrap();
    assert!(!pull2.changed);

    // Pull on a branch with no upstream fails cleanly with RemoteCommandFailed.
    repo.create_branch_and_checkout("no-upstream").unwrap();
    assert!(matches!(
        repo.pull(),
        Err(GitError::RemoteCommandFailed { op: "pull", .. })
    ));
}
