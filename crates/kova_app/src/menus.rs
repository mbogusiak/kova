//! Native menu bar wiring (`KOVA-049`): `cx.set_menus`, the process-level menu commands
//! (Quit, Open Project, Open File, Recent Projects) and `Recent Projects` persistence.
//!
//! The menu tree and key bindings come from [`kova_ui::app_menu`]; the recent-projects list
//! is the headless [`RecentProjectsStore`] from `kova_core`, read and written only on the
//! `tokio` blocking pool so neither cold start nor the UI thread touch the disk.

use std::path::PathBuf;

use gpui::{App, PathPromptOptions, WindowHandle};
use kova_config::KovaPaths;
use kova_core::{MAX_RECENT_PROJECTS_IN_MENU, RecentProject, RecentProjectsStore};
use kova_ui::app_menu::{
    ClearRecentProjects, OpenFile, OpenProject, OpenRecentProject, Quit, app_menus, bind_menu_keys,
};
use kova_ui::{StatusKind, SwitchProjectError, WorkspaceShell};
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use crate::AppEvent;
use crate::cli::FileTarget;

/// A change to the persisted `Recent Projects` list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecentProjectsUpdate {
    /// Only read the list (startup).
    Load,
    /// `root` was opened: move it to the front.
    Record(PathBuf),
    /// `root` no longer exists: forget it.
    Remove(PathBuf),
    /// `Clear Recent Projects`.
    Clear,
}

/// Applies `update` to the store under `paths` and returns the menu entries afterwards.
///
/// # Errors
///
/// Returns a human-readable message when the list cannot be written.
pub fn apply_recent_update(
    paths: KovaPaths,
    update: &RecentProjectsUpdate,
) -> Result<Vec<RecentProject>, String> {
    let mut store = RecentProjectsStore::load(paths);
    match update {
        RecentProjectsUpdate::Load => {}
        RecentProjectsUpdate::Record(root) => {
            store.record(root).map_err(|err| err.to_string())?;
        }
        RecentProjectsUpdate::Remove(root) => {
            store.remove(root).map_err(|err| err.to_string())?;
        }
        RecentProjectsUpdate::Clear => store.clear().map_err(|err| err.to_string())?,
    }
    Ok(store.menu_entries(MAX_RECENT_PROJECTS_IN_MENU))
}

/// Runs `update` on the blocking pool and reports [`AppEvent::RecentProjects`].
pub fn spawn_recent_update(
    runtime: &Handle,
    events: &mpsc::UnboundedSender<AppEvent>,
    update: RecentProjectsUpdate,
) {
    let events = events.clone();
    runtime.spawn(async move {
        let result = tokio::task::spawn_blocking(move || {
            let paths = KovaPaths::resolve().map_err(|err| err.to_string())?;
            apply_recent_update(paths, &update)
        })
        .await
        .unwrap_or_else(|err| Err(err.to_string()));
        drop(events.send(AppEvent::RecentProjects(result)));
    });
}

/// Installs the menu bar (with an empty `Recent Projects` list until the background load
/// finishes), its key bindings and the process-level command handlers for `window`.
pub fn install(
    window: WindowHandle<WorkspaceShell>,
    runtime: &Handle,
    events: &mpsc::UnboundedSender<AppEvent>,
    cx: &mut App,
) {
    bind_menu_keys(cx);
    cx.set_menus(app_menus(&[]));

    cx.on_action(|_: &Quit, cx| cx.quit());

    let (rt, tx) = (runtime.clone(), events.clone());
    cx.on_action(move |_: &OpenProject, cx| {
        prompt_and_open_project(window, rt.clone(), tx.clone(), cx);
    });

    let (rt, tx) = (runtime.clone(), events.clone());
    cx.on_action(move |action: &OpenRecentProject, cx| {
        open_project(window, action.path.clone(), &rt, &tx, cx);
    });

    let (rt, tx) = (runtime.clone(), events.clone());
    cx.on_action(move |_: &ClearRecentProjects, _| {
        spawn_recent_update(&rt, &tx, RecentProjectsUpdate::Clear);
    });

    let (rt, tx) = (runtime.clone(), events.clone());
    cx.on_action(move |_: &OpenFile, cx| {
        prompt_and_open_file(window, rt.clone(), tx.clone(), cx);
    });
}

/// Rebuilds the menu bar from the latest `Recent Projects` entries.
pub fn refresh_menus(recent: &[RecentProject], cx: &mut App) {
    cx.set_menus(app_menus(recent));
}

/// Switches `window` to `root` and records it; a vanished recent project is forgotten.
fn open_project(
    window: WindowHandle<WorkspaceShell>,
    root: PathBuf,
    runtime: &Handle,
    events: &mpsc::UnboundedSender<AppEvent>,
    cx: &mut App,
) {
    let outcome = window.update(cx, |shell, window, cx| {
        shell.switch_project(root.clone(), window, cx)
    });
    match outcome {
        Ok(Ok(())) => spawn_recent_update(runtime, events, RecentProjectsUpdate::Record(root)),
        Ok(Err(SwitchProjectError::NotADirectory(root))) => {
            spawn_recent_update(runtime, events, RecentProjectsUpdate::Remove(root));
        }
        Ok(Err(SwitchProjectError::UnsavedChanges(_))) | Err(_) => {}
    }
}

/// Shows the native directory picker (`NSOpenPanel` on macOS) and opens the choice.
fn prompt_and_open_project(
    window: WindowHandle<WorkspaceShell>,
    runtime: Handle,
    events: mpsc::UnboundedSender<AppEvent>,
    cx: &mut App,
) {
    let picked = cx.prompt_for_paths(PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: Some("Open Project".into()),
    });
    cx.spawn(async move |cx| {
        let Some(root) = first_picked(window, picker_outcome(picked.await), cx) else {
            return;
        };
        cx.update(|cx| open_project(window, root, &runtime, &events, cx))
            .ok();
    })
    .detach();
}

/// Shows the native file picker and loads the choice into the editor off-thread.
fn prompt_and_open_file(
    window: WindowHandle<WorkspaceShell>,
    runtime: Handle,
    events: mpsc::UnboundedSender<AppEvent>,
    cx: &mut App,
) {
    let picked = cx.prompt_for_paths(PathPromptOptions {
        files: true,
        directories: false,
        multiple: false,
        prompt: Some("Open".into()),
    });
    cx.spawn(async move |cx| {
        let Some(path) = first_picked(window, picker_outcome(picked.await), cx) else {
            return;
        };
        runtime.spawn(async move {
            let result = crate::io::load_file_for_editor(&path).await;
            let target = FileTarget {
                path,
                line: None,
                column: None,
            };
            drop(events.send(AppEvent::FileLoaded { target, result }));
        });
    })
    .detach();
}

/// Outcome of a native picker: `None` when the picker was dropped, `Err` when it failed.
type PickerResult = Option<Result<Option<Vec<PathBuf>>, String>>;

/// Flattens the `prompt_for_paths` channel result.
fn picker_outcome<E: std::fmt::Display, C>(
    received: Result<Result<Option<Vec<PathBuf>>, E>, C>,
) -> PickerResult {
    received
        .ok()
        .map(|result| result.map_err(|err| err.to_string()))
}

/// First path of a picker result; a picker error is shown in the status bar.
fn first_picked(
    window: WindowHandle<WorkspaceShell>,
    picked: PickerResult,
    cx: &mut gpui::AsyncApp,
) -> Option<PathBuf> {
    match picked {
        Some(Ok(Some(paths))) => paths.into_iter().next(),
        Some(Err(err)) => {
            window
                .update(cx, |shell, _, cx| {
                    shell.set_notice(
                        StatusKind::Error,
                        format!("Cannot show the file picker: {err}"),
                        cx,
                    );
                })
                .ok();
            None
        }
        Some(Ok(None)) | None => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::unwrap_used,
        reason = "test harness: failures must abort the test with context"
    )]

    use kova_config::KovaPaths;
    use tempfile::TempDir;

    use super::{RecentProjectsUpdate, apply_recent_update};

    fn paths(temp: &TempDir) -> KovaPaths {
        KovaPaths::from_roots(
            temp.path().join("config"),
            temp.path().join("data"),
            temp.path().join("cache"),
        )
        .expect("absolute")
    }

    #[test]
    fn updates_drive_the_menu_entries() {
        let temp = TempDir::new().expect("tempdir");
        let orders = temp.path().join("orders");
        std::fs::create_dir(&orders).expect("dir");
        let orders = orders.canonicalize().expect("canonical");

        assert_eq!(
            apply_recent_update(paths(&temp), &RecentProjectsUpdate::Load)
                .expect("load")
                .len(),
            0
        );
        let entries =
            apply_recent_update(paths(&temp), &RecentProjectsUpdate::Record(orders.clone()))
                .expect("record");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].root, orders);

        let entries = apply_recent_update(paths(&temp), &RecentProjectsUpdate::Load).expect("load");
        assert_eq!(entries[0].name, "orders");

        let entries = apply_recent_update(paths(&temp), &RecentProjectsUpdate::Remove(orders))
            .expect("remove");
        assert_eq!(entries.len(), 0);

        let other = temp.path().join("billing");
        std::fs::create_dir(&other).expect("dir");
        apply_recent_update(paths(&temp), &RecentProjectsUpdate::Record(other)).expect("record");
        assert_eq!(
            apply_recent_update(paths(&temp), &RecentProjectsUpdate::Clear)
                .expect("clear")
                .len(),
            0
        );
    }

    #[test]
    fn recording_a_missing_directory_fails() {
        let temp = TempDir::new().expect("tempdir");
        let missing = temp.path().join("missing");
        assert!(apply_recent_update(paths(&temp), &RecentProjectsUpdate::Record(missing)).is_err());
    }
}
