//! Native menu bar wiring (`KOVA-049`, `KOVA-054`): `cx.set_menus`, the process-level menu
//! commands (Quit, Open Project, Open File, Recent Projects) and `Recent Projects` persistence.
//!
//! The menu tree and key bindings come from [`kova_ui::app_menu`]; the recent-projects list
//! is the headless [`RecentProjectsStore`] from `kova_core`, read and written only on the
//! `tokio` blocking pool so neither cold start nor the UI thread touch the disk.

use std::path::PathBuf;

use gpui::{App, PathPromptOptions, WindowHandle, WindowId};
use kova_config::KovaPaths;
use kova_core::{MAX_RECENT_PROJECTS_IN_MENU, RecentProject, RecentProjectsStore};
use kova_ui::app_menu::{
    ClearRecentProjects, OpenFile, OpenProject, OpenRecentProject, Quit, app_menus, bind_menu_keys,
};
use kova_ui::{StatusKind, WorkspaceShell};
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use crate::cli::FileTarget;
use crate::{AppEvent, AppHostState, ProcessEvent};

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

/// Runs `update` on the blocking pool and reports [`ProcessEvent::RecentProjects`].
pub fn spawn_recent_update(
    runtime: &Handle,
    process_events: &mpsc::UnboundedSender<ProcessEvent>,
    update: RecentProjectsUpdate,
) {
    let process_events = process_events.clone();
    runtime.spawn(async move {
        let result = tokio::task::spawn_blocking(move || {
            let paths = KovaPaths::resolve().map_err(|err| err.to_string())?;
            apply_recent_update(paths, &update)
        })
        .await
        .unwrap_or_else(|err| Err(err.to_string()));
        drop(process_events.send(ProcessEvent::RecentProjects(result)));
    });
}

/// Installs the menu bar (with an empty `Recent Projects` list until the background load
/// finishes), its key bindings and the process-level command handlers targeting the active
/// [`WorkspaceShell`] window dynamically (`KOVA-054`).
pub fn install(cx: &mut App) {
    bind_menu_keys(cx);
    cx.set_menus(app_menus(&[]));

    cx.on_action(|_: &Quit, cx| cx.quit());

    cx.on_action(|_: &OpenProject, cx| {
        prompt_and_open_project(cx);
    });

    cx.on_action(|action: &OpenRecentProject, cx| {
        open_project(action.path.clone(), None, cx);
    });

    cx.on_action(|_: &ClearRecentProjects, cx| {
        let (runtime, process_tx) = {
            let host = cx.global::<AppHostState>();
            (host.runtime.clone(), host.process_tx.clone())
        };
        spawn_recent_update(&runtime, &process_tx, RecentProjectsUpdate::Clear);
    });

    cx.on_action(|_: &OpenFile, cx| {
        prompt_and_open_file(cx);
    });
}

/// Rebuilds the menu bar from the latest `Recent Projects` entries.
pub fn refresh_menus(recent: &[RecentProject], cx: &mut App) {
    cx.set_menus(app_menus(recent));
}

/// Routes a project open request for `root` across open windows (`KOVA-054`).
pub fn open_project(root: PathBuf, preferred_window: Option<WindowId>, cx: &mut App) {
    crate::prune_closed_windows(cx);
    let preferred = preferred_window.and_then(|id| {
        cx.global::<AppHostState>()
            .registry
            .slot_for(id)
            .map(|slot| slot.window)
    });
    let target_window = preferred.or_else(|| crate::resolve_target_window(cx).map(|(w, _)| w));
    let (runtime, process_tx) = {
        let host = cx.global::<AppHostState>();
        (host.runtime.clone(), host.process_tx.clone())
    };

    if !root.is_dir() {
        spawn_recent_update(
            &runtime,
            &process_tx,
            RecentProjectsUpdate::Remove(root.clone()),
        );
        if let Some(window) = target_window {
            drop(window.update(cx, |shell, window, cx| {
                shell.request_open_project(root, window, cx);
            }));
        }
        return;
    }

    if let Some(existing) = crate::find_window_by_project_root(&root, cx)
        && target_window.is_none_or(|target| target.window_id() != existing.window_id())
    {
        drop(existing.update(cx, |_, window, _| window.activate_window()));
        spawn_recent_update(&runtime, &process_tx, RecentProjectsUpdate::Record(root));
        return;
    }

    if let Some(window) = target_window {
        drop(window.update(cx, |shell, window, cx| {
            shell.request_open_project(root, window, cx);
        }));
    } else if crate::open_workspace_window(Some(root.clone()), cx).is_ok() {
        spawn_recent_update(&runtime, &process_tx, RecentProjectsUpdate::Record(root));
    }
}

/// Shows the native directory picker (`NSOpenPanel` on macOS) and opens the choice.
fn prompt_and_open_project(cx: &mut App) {
    let preferred_window = crate::resolve_target_window(cx).map(|(w, _)| w);
    let preferred_id = preferred_window.map(|w| w.window_id());
    let picked = cx.prompt_for_paths(PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: Some("Open Project".into()),
    });
    cx.spawn(async move |cx| {
        let outcome = picker_outcome(picked.await);
        cx.update(|cx| {
            let notice_window =
                preferred_window.or_else(|| crate::resolve_target_window(cx).map(|(w, _)| w));
            let Some(root) = first_picked_in_app(notice_window, outcome, cx) else {
                return;
            };
            open_project(root, preferred_id, cx);
        })
        .ok();
    })
    .detach();
}

/// Shows the native file picker and loads the choice into the active window off-thread.
fn prompt_and_open_file(cx: &mut App) {
    let target = crate::resolve_target_window(cx);
    let runtime = cx.global::<AppHostState>().runtime.clone();
    let Some((window, events)) = target else {
        return;
    };
    let picked = cx.prompt_for_paths(PathPromptOptions {
        files: true,
        directories: false,
        multiple: false,
        prompt: Some("Open".into()),
    });
    cx.spawn(async move |cx| {
        let outcome = picker_outcome(picked.await);
        let path = cx
            .update(|cx| first_picked_in_app(Some(window), outcome, cx))
            .ok()
            .flatten();
        let Some(path) = path else {
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

/// First path of a picker result; a picker error is shown in `window`'s status bar.
fn first_picked_in_app(
    window: Option<WindowHandle<WorkspaceShell>>,
    picked: PickerResult,
    cx: &mut App,
) -> Option<PathBuf> {
    match picked {
        Some(Ok(Some(paths))) => paths.into_iter().next(),
        Some(Err(err)) => {
            if let Some(window) = window {
                drop(window.update(cx, |shell, _, cx| {
                    shell.set_notice(
                        StatusKind::Error,
                        format!("Cannot show the file picker: {err}"),
                        cx,
                    );
                }));
            }
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
