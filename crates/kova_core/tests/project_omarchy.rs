//! Headless integration tests for KOVA-020 project model, `.gitignore` scanner, and Omarchy sessions.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "integration test harness"
)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kova_config::KovaPaths;
use kova_core::project::{
    BuildSystemKind, MAX_RECENT_FILES, MAX_RECENT_WORKSPACES, OpenTabState, Project, ProjectError,
    ProjectMetadata, ProjectNodeKind, ProjectScanOptions, SESSION_SCHEMA_VERSION,
    SessionLayoutState, WorkspaceSession, load_recent_workspaces, load_workspace_session,
    load_workspace_session_or_default, record_recent_workspace, save_workspace_session,
    scan_project,
};
use tempfile::TempDir;

fn isolated_paths(root: &TempDir) -> KovaPaths {
    KovaPaths::from_roots(
        root.path().join("config/kova"),
        root.path().join("local/share/kova"),
        root.path().join("cache/kova"),
    )
    .expect("TempDir paths are absolute")
}

fn snapshot_tree_entries(dir: &Path) -> BTreeSet<PathBuf> {
    fn walk(base: &Path, current: &Path, out: &mut BTreeSet<PathBuf>) {
        let entries = std::fs::read_dir(current).expect("read_dir");
        for entry in entries {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            let rel = path.strip_prefix(base).expect("strip prefix").to_path_buf();
            out.insert(rel);
            if entry.file_type().expect("file type").is_dir() {
                walk(base, &path, out);
            }
        }
    }

    let mut out = BTreeSet::new();
    walk(dir, dir, &mut out);
    out
}

#[test]
fn detects_build_systems_and_omarchy_metadata() {
    let omarchy = TempDir::new().expect("omarchy");
    let workspace = TempDir::new().expect("workspace");
    let paths = isolated_paths(&omarchy);

    let gradle = workspace.path().join("gradle-svc");
    std::fs::create_dir_all(&gradle).expect("mkdir");
    std::fs::write(
        gradle.join("settings.gradle.kts"),
        "rootProject.name = \"svc\"",
    )
    .expect("write");
    std::fs::write(gradle.join("kova.toml"), "[project]\nname = \"svc\"\n").expect("kova.toml");

    let gradle_meta = ProjectMetadata::inspect(&gradle, &paths).expect("inspect gradle");
    assert_eq!(gradle_meta.build_system, BuildSystemKind::Gradle);
    assert!(gradle_meta.has_kova_toml);
    assert_eq!(gradle_meta.name, "gradle-svc");
    assert_eq!(gradle_meta.workspace_hash.len(), 32);
    assert!(
        gradle_meta
            .cache_dir
            .starts_with(paths.workspaces_cache_dir())
    );
    assert!(!gradle_meta.cache_dir.exists());

    let maven = workspace.path().join("maven-svc");
    std::fs::create_dir_all(&maven).expect("mkdir");
    std::fs::write(maven.join("pom.xml"), "<project/>").expect("pom");
    let maven_meta = ProjectMetadata::inspect(&maven, &paths).expect("inspect maven");
    assert_eq!(maven_meta.build_system, BuildSystemKind::Maven);
    assert!(!maven_meta.has_kova_toml);

    let mixed = workspace.path().join("mixed-svc");
    std::fs::create_dir_all(&mixed).expect("mkdir");
    std::fs::write(mixed.join("build.gradle"), "").expect("gradle");
    std::fs::write(mixed.join("pom.xml"), "<project/>").expect("pom");
    assert_eq!(
        ProjectMetadata::inspect(&mixed, &paths)
            .expect("inspect mixed")
            .build_system,
        BuildSystemKind::Mixed
    );

    let unknown = workspace.path().join("plain-dir");
    std::fs::create_dir_all(&unknown).expect("mkdir");
    assert_eq!(
        ProjectMetadata::inspect(&unknown, &paths)
            .expect("inspect unknown")
            .build_system,
        BuildSystemKind::Unknown
    );
}

#[test]
fn scanner_respects_gitignore_nested_ignores_and_negations_with_or_without_git_dir() {
    for with_git_dir in [false, true] {
        let project = TempDir::new().expect("project");
        let root = project.path();

        if with_git_dir {
            std::fs::create_dir_all(root.join(".git/info")).expect(".git/info");
            std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").expect("HEAD");
            std::fs::write(root.join(".git/info/exclude"), "*.local\n").expect("exclude");
        }

        std::fs::write(
            root.join(".gitignore"),
            "target/\nbuild/\n.gradle/\n*.class\n!keep.class\n",
        )
        .expect(".gitignore");
        std::fs::write(root.join(".editorconfig"), "root = true\n").expect(".editorconfig");
        std::fs::create_dir_all(root.join(".github/workflows")).expect(".github");
        std::fs::write(root.join(".github/workflows/ci.yml"), "on: [push]\n").expect("ci.yml");

        // Ignored build artifacts.
        std::fs::create_dir_all(root.join("target/classes")).expect("target");
        std::fs::write(root.join("target/classes/App.class"), "cafebabe").expect("App.class");
        std::fs::create_dir_all(root.join("build/libs")).expect("build");
        std::fs::write(root.join("build/libs/app.jar"), "jar").expect("app.jar");
        std::fs::create_dir_all(root.join(".gradle/8.0")).expect(".gradle");
        std::fs::write(root.join(".gradle/8.0/lock"), "lock").expect("lock");
        std::fs::write(root.join("scratch.local"), "local").expect("scratch.local");

        // Tracked sources + nested module .gitignore.
        std::fs::create_dir_all(root.join("module-a/src/main/java")).expect("module-a");
        std::fs::write(root.join("module-a/.gitignore"), "generated/\n*.tmp\n")
            .expect("module-a .gitignore");
        std::fs::create_dir_all(root.join("module-a/generated")).expect("generated");
        std::fs::write(root.join("module-a/generated/Gen.java"), "class Gen {}").expect("Gen.java");
        std::fs::write(root.join("module-a/notes.tmp"), "tmp").expect("notes.tmp");
        std::fs::write(root.join("module-a/src/main/java/App.java"), "class App {}")
            .expect("App.java");
        std::fs::write(root.join("module-a/src/main/java/App.class"), "bytecode")
            .expect("nested App.class");
        std::fs::write(root.join("keep.class"), "kept").expect("keep.class");

        let tree = scan_project(root, &ProjectScanOptions::default()).expect("scan_project");

        assert!(tree.find_by_rel_path(Path::new(".git")).is_none());
        assert!(tree.find_by_rel_path(Path::new("target")).is_none());
        assert!(tree.find_by_rel_path(Path::new("build")).is_none());
        assert!(tree.find_by_rel_path(Path::new(".gradle")).is_none());
        assert!(
            tree.find_by_rel_path(Path::new("module-a/generated"))
                .is_none()
        );
        assert!(
            tree.find_by_rel_path(Path::new("module-a/notes.tmp"))
                .is_none()
        );
        assert!(
            tree.find_by_rel_path(Path::new("module-a/src/main/java/App.class"))
                .is_none()
        );
        if with_git_dir {
            assert!(tree.find_by_rel_path(Path::new("scratch.local")).is_none());
        }

        assert!(tree.find_by_rel_path(Path::new(".gitignore")).is_some());
        assert!(tree.find_by_rel_path(Path::new(".editorconfig")).is_some());
        assert!(
            tree.find_by_rel_path(Path::new(".github/workflows/ci.yml"))
                .is_some()
        );
        assert!(
            tree.find_by_rel_path(Path::new("module-a/src/main/java/App.java"))
                .is_some()
        );
        assert!(tree.find_by_rel_path(Path::new("keep.class")).is_some());

        // Flat `files()` slice must be sorted deterministically by `rel_path`.
        let file_paths: Vec<&Path> = tree.files().iter().map(|f| f.rel_path.as_path()).collect();
        let mut sorted_paths = file_paths.clone();
        sorted_paths.sort();
        assert_eq!(file_paths, sorted_paths);
    }
}

#[cfg(unix)]
#[test]
fn scanner_does_not_loop_on_symlinks_when_follow_symlinks_is_false() {
    let project = TempDir::new().expect("project");
    let root = project.path();
    std::fs::create_dir_all(root.join("src")).expect("src");
    std::fs::write(root.join("src/Main.java"), "class Main {}").expect("Main.java");

    std::os::unix::fs::symlink(root, root.join("src/loop-to-root")).expect("symlink");
    std::os::unix::fs::symlink(root.join("nonexistent-target"), root.join("broken-link"))
        .expect("broken symlink");

    let tree = scan_project(root, &ProjectScanOptions::default()).expect("scan succeeds");
    assert!(tree.find_by_rel_path(Path::new("src/Main.java")).is_some());
    assert!(!tree.truncated);
}

#[test]
fn rescan_subtree_updates_only_target_subtree_and_handles_removal() {
    let project = TempDir::new().expect("project");
    let root = project.path();

    std::fs::create_dir_all(root.join("service-a/src")).expect("service-a");
    std::fs::create_dir_all(root.join("service-b/src")).expect("service-b");
    std::fs::write(root.join("service-a/src/Alpha.java"), "class Alpha {}").expect("Alpha");
    std::fs::write(root.join("service-b/src/Beta.java"), "class Beta {}").expect("Beta");

    let opts = ProjectScanOptions::default();
    let mut tree = scan_project(root, &opts).expect("initial scan");
    assert_eq!(tree.file_count(), 2);

    // Modify service-a only.
    std::fs::write(root.join("service-a/src/Gamma.java"), "class Gamma {}").expect("Gamma");
    std::fs::remove_file(root.join("service-a/src/Alpha.java")).expect("remove Alpha");
    tree.rescan_subtree(root, Path::new("service-a"), &opts)
        .expect("rescan service-a");

    assert!(
        tree.find_by_rel_path(Path::new("service-a/src/Alpha.java"))
            .is_none()
    );
    let gamma_id = tree
        .find_by_rel_path(Path::new("service-a/src/Gamma.java"))
        .expect("Gamma present");
    assert!(matches!(
        tree.get(gamma_id).map(|n| &n.kind),
        Some(ProjectNodeKind::File { .. })
    ));
    assert!(
        tree.find_by_rel_path(Path::new("service-b/src/Beta.java"))
            .is_some()
    );
    assert_eq!(tree.file_count(), 2);

    // Delete service-a completely and rescan subtree.
    std::fs::remove_dir_all(root.join("service-a")).expect("remove service-a");
    tree.rescan_subtree(root, Path::new("service-a"), &opts)
        .expect("rescan deleted subtree");
    assert!(tree.find_by_rel_path(Path::new("service-a")).is_none());
    assert!(
        tree.find_by_rel_path(Path::new("service-b/src/Beta.java"))
            .is_some()
    );
    assert_eq!(tree.file_count(), 1);

    // Reject escaping subtree path.
    assert!(matches!(
        tree.rescan_subtree(root, Path::new("../outside"), &opts),
        Err(ProjectError::InvalidProjectPath { .. })
    ));
}

#[test]
fn omarchy_guarantee_zero_files_created_in_project_root_and_atomic_session_persistence() {
    let omarchy = TempDir::new().expect("omarchy");
    let project_dir = TempDir::new().expect("project");
    let paths = isolated_paths(&omarchy);
    let root = project_dir.path();

    std::fs::create_dir_all(root.join("src/main/java")).expect("src");
    std::fs::write(root.join("build.gradle.kts"), "plugins { java }").expect("gradle");
    std::fs::write(root.join("src/main/java/Main.java"), "class Main {}").expect("Main");
    std::fs::write(root.join("src/main/java/Service.java"), "class Service {}").expect("Service");

    let before_entries = snapshot_tree_entries(root);

    let mut project = Project::open(root, &paths).expect("open project");
    assert_eq!(project.metadata.build_system, BuildSystemKind::Gradle);

    project.session.open_or_focus_tab(OpenTabState {
        rel_path: PathBuf::from("src/main/java/Main.java"),
        cursor_line: 42,
        cursor_column: 8,
        scroll_top_line: 15,
        pinned: true,
    });
    project.session.open_or_focus_tab(OpenTabState {
        rel_path: PathBuf::from("src/main/java/Service.java"),
        cursor_line: 3,
        cursor_column: 1,
        scroll_top_line: 0,
        pinned: false,
    });
    project.session.expanded_dirs.insert(PathBuf::from("src"));
    project
        .session
        .expanded_dirs
        .insert(PathBuf::from("src/main/java"));
    project.session.layout = SessionLayoutState {
        focus_mode: true,
        left_sidebar_visible: false,
        active_left_tool: Some("structure".to_owned()),
        bottom_panel_visible: true,
        active_bottom_tool: Some("terminal".to_owned()),
    };

    let session_file = save_workspace_session(&paths, &project.session).expect("save session");
    let recent = record_recent_workspace(&paths, &project.metadata).expect("record recent");
    assert_eq!(recent.entries.len(), 1);
    assert_eq!(
        recent.entries[0].workspace_hash,
        project.metadata.workspace_hash
    );

    // Critical Omarchy check: project_root is 100% untouched (zero .idea/, zero .kova/).
    let after_entries = snapshot_tree_entries(root);
    assert_eq!(before_entries, after_entries);
    assert!(!root.join(".idea").exists());
    assert!(!root.join(".kova").exists());

    // Cache directory must contain only `session.json` and no leftover `.tmp` files.
    assert!(session_file.starts_with(paths.workspaces_cache_dir()));
    let cache_files: Vec<String> = std::fs::read_dir(&project.metadata.cache_dir)
        .expect("read cache dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(cache_files, vec!["session.json"]);

    let reloaded = load_workspace_session(&paths, root)
        .expect("load session")
        .expect("present");
    assert_eq!(reloaded.open_tabs, project.session.open_tabs);
    assert_eq!(reloaded.active_tab_index, Some(1));
    assert_eq!(reloaded.expanded_dirs, project.session.expanded_dirs);
    assert_eq!(reloaded.layout, project.session.layout);
    assert_eq!(
        reloaded.recent_files,
        vec![
            PathBuf::from("src/main/java/Service.java"),
            PathBuf::from("src/main/java/Main.java")
        ]
    );
}

#[test]
fn session_sanitization_rejects_path_traversal_symlink_escape_and_deleted_tabs() {
    let omarchy = TempDir::new().expect("omarchy");
    let sandbox = TempDir::new().expect("sandbox");
    let paths = isolated_paths(&omarchy);

    let project_root = sandbox.path().join("repo");
    let outside_dir = sandbox.path().join("outside");
    std::fs::create_dir_all(project_root.join("src")).expect("src");
    std::fs::create_dir_all(&outside_dir).expect("outside");
    std::fs::write(project_root.join("src/Keep.java"), "class Keep {}").expect("Keep");
    std::fs::write(project_root.join("src/Active.java"), "class Active {}").expect("Active");
    std::fs::write(outside_dir.join("secret.txt"), "top-secret").expect("secret");

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(
            outside_dir.join("secret.txt"),
            project_root.join("src/escape_file"),
        )
        .expect("escape file symlink");
        std::os::unix::fs::symlink(&outside_dir, project_root.join("escape_dir"))
            .expect("escape dir symlink");
    }

    let meta = ProjectMetadata::inspect(&project_root, &paths).expect("inspect");
    let mut session = WorkspaceSession::new(&meta);
    session.open_tabs = vec![
        OpenTabState::new(PathBuf::from("../../etc/passwd")),
        OpenTabState::new(PathBuf::from("/etc/passwd")),
        OpenTabState::new(PathBuf::from("src/escape_file")),
        OpenTabState::new(PathBuf::from("src/Keep.java")),
        OpenTabState::new(PathBuf::from("src/Active.java")),
    ];
    session.active_tab_index = Some(4);
    session.expanded_dirs.insert(PathBuf::from("src"));
    session.expanded_dirs.insert(PathBuf::from("escape_dir"));
    session.expanded_dirs.insert(PathBuf::from("../outside"));
    session.recent_files = vec![
        PathBuf::from("../../etc/passwd"),
        PathBuf::from("src/escape_file"),
        PathBuf::from("src/Active.java"),
        PathBuf::from("src/Keep.java"),
        PathBuf::from("src/Active.java"),
    ];

    save_workspace_session(&paths, &session).expect("save crafted session");

    let loaded = load_workspace_session(&paths, &project_root)
        .expect("load sanitized")
        .expect("session exists");

    assert_eq!(loaded.open_tabs.len(), 2);
    assert_eq!(loaded.open_tabs[0].rel_path, PathBuf::from("src/Keep.java"));
    assert_eq!(
        loaded.open_tabs[1].rel_path,
        PathBuf::from("src/Active.java")
    );
    // Active tab shifted cleanly from index 4 to index 1 after earlier invalid tabs were stripped.
    assert_eq!(loaded.active_tab_index, Some(1));
    assert_eq!(loaded.expanded_dirs.len(), 1);
    assert!(loaded.expanded_dirs.contains(Path::new("src")));
    assert_eq!(
        loaded.recent_files,
        vec![
            PathBuf::from("src/Active.java"),
            PathBuf::from("src/Keep.java")
        ]
    );

    // Delete the active file on disk and reload -> active_tab_index falls back to Some(0).
    std::fs::remove_file(project_root.join("src/Active.java")).expect("delete Active");
    let reloaded = load_workspace_session(&paths, &project_root)
        .expect("reload")
        .expect("session");
    assert_eq!(reloaded.open_tabs.len(), 1);
    assert_eq!(reloaded.active_tab_index, Some(0));

    // Delete the final file on disk and reload -> active_tab_index becomes None.
    std::fs::remove_file(project_root.join("src/Keep.java")).expect("delete Keep");
    let empty_reloaded = load_workspace_session(&paths, &project_root)
        .expect("reload")
        .expect("session");
    assert_eq!(empty_reloaded.open_tabs.len(), 0);
    assert_eq!(empty_reloaded.active_tab_index, None);
}

#[test]
fn corrupted_or_unsupported_session_surfaces_typed_error_and_recovers_via_fallback() {
    let omarchy = TempDir::new().expect("omarchy");
    let project = TempDir::new().expect("project");
    let paths = isolated_paths(&omarchy);

    let meta = ProjectMetadata::inspect(project.path(), &paths).expect("inspect");
    let cache_dir = paths
        .ensure_workspace_cache_dir(&meta.root)
        .expect("cache dir");
    let session_path = cache_dir.join("session.json");

    std::fs::write(&session_path, "not-json").expect("write bad json");
    assert!(matches!(
        load_workspace_session(&paths, &meta.root),
        Err(ProjectError::CorruptedSession { .. })
    ));
    let (session, warn) = load_workspace_session_or_default(&paths, &meta);
    assert_eq!(session.open_tabs.len(), 0);
    assert!(warn.is_some());

    let future_json = format!(
        r#"{{"schema_version": {}, "project_root": "{}", "workspace_hash": "{}"}}"#,
        SESSION_SCHEMA_VERSION + 99,
        meta.root.display(),
        meta.workspace_hash
    );
    std::fs::write(&session_path, future_json).expect("write future version");
    assert!(matches!(
        load_workspace_session(&paths, &meta.root),
        Err(ProjectError::UnsupportedSessionVersion { version: 100, .. })
    ));
    let (fallback_session, fallback_warn) = load_workspace_session_or_default(&paths, &meta);
    assert_eq!(fallback_session.schema_version, SESSION_SCHEMA_VERSION);
    assert!(fallback_warn.is_some());
}

#[test]
fn recent_workspaces_caps_at_limit_and_recent_files_caps_at_64() {
    let omarchy = TempDir::new().expect("omarchy");
    let root = TempDir::new().expect("root");
    let paths = isolated_paths(&omarchy);

    for idx in 0..40 {
        let dir = root.path().join(format!("proj_{idx:02}"));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let meta = ProjectMetadata::inspect(&dir, &paths).expect("inspect");
        record_recent_workspace(&paths, &meta).expect("record");
    }

    let recent = load_recent_workspaces(&paths).expect("load recent");
    assert_eq!(recent.entries.len(), MAX_RECENT_WORKSPACES);
    assert_eq!(recent.entries[0].name, "proj_39");

    // Verify recent_files cap (64) on WorkspaceSession.
    let proj0 = root.path().join("proj_00");
    let mut session =
        WorkspaceSession::new(&ProjectMetadata::inspect(&proj0, &paths).expect("inspect"));
    for idx in 0..80 {
        let rel = PathBuf::from(format!("File{idx:02}.java"));
        std::fs::write(proj0.join(&rel), "class X {}").expect("write");
        session.record_recent_file(rel);
    }
    assert_eq!(session.recent_files.len(), MAX_RECENT_FILES);
    assert_eq!(session.recent_files[0], PathBuf::from("File79.java"));
}

#[test]
fn scanner_prunes_ignored_target_tree_within_performance_budget() {
    let project = TempDir::new().expect("project");
    let root = project.path();

    std::fs::write(root.join(".gitignore"), "target/\n").expect(".gitignore");

    // 1,000 tracked Java sources + 5,000 ignored class artifacts under target/ to verify
    // early directory pruning keeps scanning fast without walking ignored subtrees.
    for pkg in 0..20 {
        let src_dir = root.join(format!("src/main/java/com/example/p{pkg:02}"));
        std::fs::create_dir_all(&src_dir).expect("src_dir");
        for cls in 0..50 {
            std::fs::write(src_dir.join(format!("Class{cls:02}.java")), "class C {}")
                .expect("java file");
        }
    }

    for pkg in 0..50 {
        let tgt_dir = root.join(format!("target/classes/com/example/p{pkg:02}"));
        std::fs::create_dir_all(&tgt_dir).expect("tgt_dir");
        for cls in 0..100 {
            std::fs::write(tgt_dir.join(format!("Class{cls:02}.class")), "cafebabe")
                .expect("class file");
        }
    }

    let started = Instant::now();
    let tree = scan_project(root, &ProjectScanOptions::default()).expect("scan");
    let elapsed = started.elapsed();

    assert_eq!(tree.file_count(), 1_001); // 1,000 .java files + .gitignore
    assert!(tree.find_by_rel_path(Path::new("target")).is_none());
    assert!(
        elapsed < Duration::from_millis(500),
        "expected pruned scan to finish quickly, took {elapsed:?}"
    );
}
