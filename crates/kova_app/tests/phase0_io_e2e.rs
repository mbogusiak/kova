//! Headless Phase 0 end-to-end checks (`KOVA-016`): large-file load, atomic save with a
//! concurrent edit, live theme reload through `kova_config::ConfigWatcher`, and the
//! `< 150 MiB` RSS budget.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "integration tests assert on setup failures directly"
)]

use std::path::Path;
use std::time::Duration;

use kova_app::io::{LoadedFile, atomic_write_snapshot, load_file_for_editor};
use kova_app::theme_watch::{self, ThemeWatch, ThemeWatchEvent};
use kova_config::KovaPaths;
use kova_core::perf::{budgets, current_process_rss_bytes};
use kova_ui::document::{DocumentState, SaveAction};
use tempfile::TempDir;
use tokio::sync::mpsc::UnboundedReceiver;

const TEN_MIB: usize = 10 * 1024 * 1024;

fn java_source_of_at_least(bytes: usize) -> String {
    let line = "    public static int compute(int value) { return value * 31 + 7; } // ąę\n";
    let mut out = String::with_capacity(bytes + 256);
    out.push_str("package dev.kova.big;\n\npublic final class Big {\n");
    while out.len() < bytes {
        out.push_str(line);
    }
    out.push_str("}\n");
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_file_load_edit_atomic_save_and_rss_budget() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("Big.java");
    let source = java_source_of_at_least(TEN_MIB);
    std::fs::write(&path, &source).expect("write source");

    let LoadedFile::Existing { mut buffer, .. } =
        load_file_for_editor(&path).await.expect("load big file")
    else {
        panic!("expected an existing file");
    };
    assert_eq!(buffer.snapshot().len_bytes(), source.len());
    drop(source);

    let mut doc = DocumentState::untitled();
    doc.reset_for(Some(path.clone()), buffer.version().as_u64(), false);

    buffer
        .apply_edit(0..0, "// edited by Kova\n")
        .expect("first edit");
    assert!(doc.is_dirty(buffer.version().as_u64()));

    let SaveAction::Dispatch(request) = doc.begin_save(buffer.snapshot()) else {
        panic!("expected a save dispatch");
    };
    let saved_text_len = request.snapshot.len_bytes();
    let (save_path, save_snapshot) = (request.path.clone(), request.snapshot.clone());
    let save = tokio::spawn(async move { atomic_write_snapshot(&save_path, save_snapshot).await });

    // Keep typing while tokio writes revision R.
    buffer
        .apply_edit(0..0, "// typed during save\n")
        .expect("concurrent edit");

    let result = save.await.expect("join").map_err(|e| e.to_string());
    assert!(!doc.finish_save(request.ticket, result));
    assert_eq!(doc.saved_revision, request.ticket.revision);
    assert!(
        doc.is_dirty(buffer.version().as_u64()),
        "edits made during the save must keep the document dirty"
    );

    let on_disk = std::fs::read(&path).expect("read back");
    assert_eq!(on_disk.len(), saved_text_len);
    assert!(on_disk.starts_with(b"// edited by Kova\npackage dev.kova.big;"));
    assert!(
        request.snapshot.rope().bytes().eq(on_disk.iter().copied()),
        "file content must equal the saved snapshot byte for byte"
    );
    assert_no_temp_files(dir.path());

    let rss = current_process_rss_bytes().expect("rss probe");
    assert!(
        rss < budgets::MAX_PROCESS_RSS_BYTES,
        "RSS {rss} B exceeds the {} B Phase 0 budget",
        budgets::MAX_PROCESS_RSS_BYTES
    );
}

fn assert_no_temp_files(dir: &Path) {
    let leftovers: Vec<String> = std::fs::read_dir(dir)
        .expect("read_dir")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".kova-tmp-"))
        .collect();
    assert_eq!(leftovers, Vec::<String>::new());
}

async fn next_theme_or_warning(rx: &mut UnboundedReceiver<ThemeWatchEvent>) -> ThemeWatchEvent {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match rx.recv().await.expect("theme channel open") {
                ThemeWatchEvent::ConfigReloaded => {}
                other => return other,
            }
        }
    })
    .await
    .expect("timed out waiting for a theme update")
}

async fn expect_theme(rx: &mut UnboundedReceiver<ThemeWatchEvent>, id: &str) {
    match next_theme_or_warning(rx).await {
        ThemeWatchEvent::ThemeChanged(theme) => assert_eq!(theme.metadata.id, id),
        other => panic!("expected theme `{id}`, got {other:?}"),
    }
}

async fn write_settings(watch: &ThemeWatch, paths: &KovaPaths, contents: &str) {
    tokio::fs::write(paths.settings_file(), contents)
        .await
        .expect("write settings.toml");
    watch.reload_now().await.expect("reload");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn theme_live_reload_switches_and_survives_invalid_toml() {
    let root = TempDir::new().expect("tempdir");
    let config = root.path().join("config");
    std::fs::create_dir_all(&config).expect("config dir");
    let paths = KovaPaths::from_roots(
        config.clone(),
        root.path().join("data"),
        root.path().join("cache"),
    )
    .expect("absolute roots");
    std::fs::write(paths.settings_file(), "[ui]\ntheme = \"kova-light\"\n").expect("settings");

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let watch = theme_watch::start(paths.clone(), tx).expect("start watcher");

    // Initial state already resolves the configured theme.
    expect_theme(&mut rx, "kova-light").await;

    write_settings(&watch, &paths, "[ui]\ntheme = \"kova-islands-dark\"\n").await;
    expect_theme(&mut rx, "kova-islands-dark").await;

    // Broken TOML: warning, previous theme stays active (no ThemeChanged).
    write_settings(&watch, &paths, "[ui\ntheme = = \"kova-light\"\n").await;
    match next_theme_or_warning(&mut rx).await {
        ThemeWatchEvent::Warning(message) => assert!(message.contains("settings.toml")),
        other => panic!("expected a warning, got {other:?}"),
    }

    write_settings(&watch, &paths, "[ui]\ntheme = \"kova-light\"\n").await;
    expect_theme(&mut rx, "kova-light").await;

    // `--config-dir` maps onto the same directory.
    let resolved = theme_watch::resolve_paths(Some(&config)).expect("resolve");
    assert_eq!(resolved.config_dir(), config.as_path());
}
