//! Isolated integration tests for Omarchy path resolution (`KOVA-005`).
#![allow(clippy::expect_used, reason = "integration test harness")]

use std::path::PathBuf;

use kova_config::{KovaPaths, PathsError};
use tempfile::TempDir;

fn isolated_paths(root: &TempDir) -> KovaPaths {
    KovaPaths::from_roots(
        root.path().join("config/kova"),
        root.path().join("local/share/kova"),
        root.path().join("cache/kova"),
    )
    .expect("TempDir paths are absolute")
}

#[test]
fn ensure_omarchy_dirs_and_workspace_cache_never_touch_project_dir() {
    let omarchy_root = TempDir::new().expect("create omarchy temp dir");
    let project_root = TempDir::new().expect("create project temp dir");
    let paths = isolated_paths(&omarchy_root);

    paths
        .ensure_omarchy_dirs()
        .expect("creating Omarchy system dirs should succeed");

    let workspace_cache = paths
        .ensure_workspace_cache_dir(project_root.path())
        .expect("creating workspace cache dir should succeed");
    assert!(workspace_cache.is_dir());
    assert!(workspace_cache.starts_with(paths.workspaces_cache_dir()));

    // Pure helper must not create kova.toml or anything else in project_root.
    let project_cfg = KovaPaths::project_config_file(project_root.path());
    assert_eq!(project_cfg, project_root.path().join("kova.toml"));
    assert!(!project_cfg.exists());

    // Verify all Omarchy subdirectories exist inside the isolated system root.
    for dir in [
        paths.config_dir().to_path_buf(),
        paths.themes_dir(),
        paths.data_dir().to_path_buf(),
        paths.omakase_dir(),
        paths.jdks_dir(),
        paths.jdtls_dir(),
        paths.java_debug_dir(),
        paths.vineflower_dir(),
        paths.plugins_dir(),
        paths.cache_dir().to_path_buf(),
        paths.workspaces_cache_dir(),
    ] {
        assert!(dir.is_dir(), "expected Omarchy dir to exist: {dir:?}");
    }

    // Critical Omarchy guarantee: ZERO files or hidden folders created inside project_root.
    let entries: Vec<_> = std::fs::read_dir(project_root.path())
        .expect("read project dir")
        .collect();
    assert!(
        entries.is_empty(),
        "project directory must remain completely clean, found: {entries:?}"
    );
}

#[test]
fn workspace_hash_is_stable_across_aliases_and_symlinks() {
    let temp = TempDir::new().expect("create temp dir");
    let paths = isolated_paths(&temp);

    let project_a = temp.path().join("projects/alpha");
    let project_b = temp.path().join("projects/beta");
    let helper_dir = temp.path().join("projects/other");
    std::fs::create_dir_all(&project_a).expect("create project_a");
    std::fs::create_dir_all(&project_b).expect("create project_b");
    std::fs::create_dir_all(&helper_dir).expect("create helper_dir");

    let symlink_a = temp.path().join("alpha-link");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&project_a, &symlink_a).expect("create unix symlink");

    let hash_direct = paths.workspace_hash(&project_a).expect("hash direct");
    let hash_dotdot = paths
        .workspace_hash(&helper_dir.join("../alpha"))
        .expect("hash via ..");
    let hash_trailing = paths
        .workspace_hash(&PathBuf::from(format!("{}/", project_a.display())))
        .expect("hash trailing slash");

    assert_eq!(hash_direct.len(), 32);
    assert!(
        hash_direct
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    );
    assert_eq!(hash_direct, hash_dotdot);
    assert_eq!(hash_direct, hash_trailing);

    #[cfg(unix)]
    {
        let hash_symlink = paths.workspace_hash(&symlink_a).expect("hash symlink");
        assert_eq!(hash_direct, hash_symlink);
    }

    let hash_b = paths.workspace_hash(&project_b).expect("hash project_b");
    assert_ne!(hash_direct, hash_b);
}

#[test]
fn non_existent_absolute_paths_hash_deterministically_while_relative_fail() {
    let temp = TempDir::new().expect("create temp dir");
    let paths = isolated_paths(&temp);

    let h1 = paths
        .workspace_hash(&PathBuf::from("/nonexistent/kova/sub/../project"))
        .expect("non-existent absolute path normalizes lexically");
    let h2 = paths
        .workspace_hash(&PathBuf::from("/nonexistent/kova/project/"))
        .expect("non-existent absolute path normalizes lexically");
    assert_eq!(h1, h2);

    let rel_err = paths
        .workspace_hash(&PathBuf::from("relative/nonexistent/project"))
        .expect_err("non-existent relative path must fail");
    assert!(matches!(rel_err, PathsError::CanonicalizeFailed { .. }));
}
