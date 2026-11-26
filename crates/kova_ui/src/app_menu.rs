//! Native application menu bar (`KOVA-049`): GPUI actions, their key bindings and the
//! [`Menu`] tree passed to `cx.set_menus`.
//!
//! The menu only *describes* commands. The workspace shell handles the view commands
//! (tool windows, Focus Mode, Close Tab, Settings); `kova_app` handles the process-level ones
//! (Quit, Open Project / Open File with the native pickers, Recent Projects persistence).
//! Edit commands reuse the editor actions, so the menu and `Cmd+Z` / `Cmd+C` run one code path.

use std::path::PathBuf;

use gpui::{App, KeyBinding, Menu, MenuItem};
use kova_core::builtin_actions::ids;
use kova_core::keymap::presets::jetbrains_keymap_layer_ref;
use kova_core::{Platform, RecentProject};

use crate::editor::actions::{Copy, Cut, Paste, Redo, Save, Undo, WORKSPACE_KEY_CONTEXT};
use crate::git::gpui_keystroke;

/// Name of the application menu (first menu on macOS).
pub const APP_MENU_NAME: &str = "Kova";

/// Label of the disabled placeholder shown when no project was opened yet.
pub const NO_RECENT_PROJECTS_LABEL: &str = "No Recent Projects";

gpui::actions!(
    kova_menu,
    [
        /// Opens the native directory picker and switches the window to the chosen project.
        OpenProject,
        /// Opens the native file picker and shows the chosen file in the editor.
        OpenFile,
        /// Forgets every entry of `File → Recent Projects`.
        ClearRecentProjects,
        /// Quits Kova.
        Quit,
        /// Closes the active editor tab.
        CloseTab,
        /// Opens the Settings overlay.
        OpenSettings,
        /// Opens *Search Everywhere* (double `Shift`).
        SearchEverywhere,
        /// Shows or hides the *Project* tool window.
        ToggleProjectTree,
        /// Shows or hides the *Terminal* tool window.
        ToggleTerminal,
        /// Shows or hides the Git *Commit* tool window.
        ToggleGit,
        /// Enters or leaves Focus Mode.
        ToggleFocusMode,
        /// Placeholder item of an empty `Recent Projects` submenu (never handled → disabled).
        NoRecentProjects,
    ]
);

/// Switches the window to a project from `File → Recent Projects`.
#[derive(Debug, Clone, PartialEq, Eq, gpui::Action)]
#[action(namespace = kova_menu, no_json)]
pub struct OpenRecentProject {
    /// Project root to open.
    pub path: PathBuf,
}

/// Builds the menu bar; `recent` fills `File → Recent Projects` (most recent first).
#[must_use]
pub fn app_menus(recent: &[RecentProject]) -> Vec<Menu> {
    vec![
        Menu {
            name: APP_MENU_NAME.into(),
            items: vec![
                MenuItem::action("Settings…", OpenSettings),
                MenuItem::separator(),
                MenuItem::action("Quit Kova", Quit),
            ],
        },
        Menu {
            name: "File".into(),
            items: vec![
                MenuItem::action("Open Project…", OpenProject),
                MenuItem::action("Open File…", OpenFile),
                MenuItem::submenu(recent_projects_menu(recent)),
                MenuItem::separator(),
                MenuItem::action("Save", Save),
                MenuItem::action("Close Tab", CloseTab),
            ],
        },
        Menu {
            name: "Edit".into(),
            items: vec![
                MenuItem::action("Undo", Undo),
                MenuItem::action("Redo", Redo),
                MenuItem::separator(),
                MenuItem::action("Cut", Cut),
                MenuItem::action("Copy", Copy),
                MenuItem::action("Paste", Paste),
            ],
        },
        Menu {
            name: "View".into(),
            items: vec![
                MenuItem::action("Search Everywhere (⇧⇧)", SearchEverywhere),
                MenuItem::separator(),
                MenuItem::action("Toggle Project Tree", ToggleProjectTree),
                MenuItem::action("Toggle Terminal", ToggleTerminal),
                MenuItem::action("Toggle Git", ToggleGit),
                MenuItem::separator(),
                MenuItem::action("Focus Mode", ToggleFocusMode),
            ],
        },
    ]
}

/// `File → Recent Projects` submenu.
#[must_use]
pub fn recent_projects_menu(recent: &[RecentProject]) -> Menu {
    let mut items: Vec<MenuItem> = recent
        .iter()
        .map(|project| {
            MenuItem::action(
                recent_project_label(project),
                OpenRecentProject {
                    path: project.root.clone(),
                },
            )
        })
        .collect();
    if items.is_empty() {
        // An action without a handler renders disabled: a visible, inert placeholder.
        items.push(MenuItem::action(NO_RECENT_PROJECTS_LABEL, NoRecentProjects));
    } else {
        items.push(MenuItem::separator());
        items.push(MenuItem::action(
            "Clear Recent Projects",
            ClearRecentProjects,
        ));
    }
    Menu {
        name: "Recent Projects".into(),
        items,
    }
}

/// Menu label of a recent project: `name — ~/path/to/parent`.
#[must_use]
pub fn recent_project_label(project: &RecentProject) -> String {
    let parent = project
        .root
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let parent = std::env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(|home| {
            let home = home.display().to_string();
            parent
                .strip_prefix(&home)
                .filter(|rest| rest.is_empty() || rest.starts_with('/'))
                .map(|rest| format!("~{rest}"))
        })
        .unwrap_or(parent);
    if parent.is_empty() {
        project.name.clone()
    } else {
        format!("{} — {parent}", project.name)
    }
}

/// GPUI keystroke of a `JetBrains` preset action for `platform`, or `fallback`.
fn preset_keystroke(platform: Platform, action: &str, fallback: &str) -> String {
    jetbrains_keymap_layer_ref(platform)
        .chord_for_action(action)
        .and_then(gpui_keystroke)
        .unwrap_or_else(|| fallback.to_owned())
}

/// Key bindings of the menu commands for `platform`.
///
/// View commands reuse the `JetBrains` preset chords (so the menu shows the same shortcut the
/// keymap runs) and apply inside the workspace. Open Project / Open File / Quit are global so
/// they work with no window focused. `Cmd+K` stays with the Git Commit panel (`KOVA-043`).
#[must_use]
pub fn menu_key_bindings(platform: Platform) -> Vec<KeyBinding> {
    let workspace = Some(WORKSPACE_KEY_CONTEXT);
    let primary = match platform {
        Platform::MacOs => "cmd",
        Platform::Linux => "ctrl",
    };
    let key = |action: &str, fallback: &str| preset_keystroke(platform, action, fallback);
    let mut bindings = Vec::new();
    if platform == Platform::MacOs {
        // `Cmd+O` / `Cmd+Shift+O` belong to IntelliJ Go to Class / Go to File;
        // `Ctrl+O` / `Ctrl+Q` belong to the shell and Vim on Linux.
        bindings.push(KeyBinding::new("cmd-q", Quit, None));
    }
    bindings.extend([
        KeyBinding::new(
            &key(ids::VIEW_OPEN_SETTINGS, &format!("{primary}-,")),
            OpenSettings,
            workspace,
        ),
        KeyBinding::new(
            &key(ids::VIEW_CLOSE_TAB, &format!("{primary}-w")),
            CloseTab,
            workspace,
        ),
        KeyBinding::new(
            &key(ids::TOOL_WINDOW_PROJECT, &format!("{primary}-1")),
            ToggleProjectTree,
            workspace,
        ),
        KeyBinding::new(
            &key(ids::TOOL_WINDOW_TERMINAL, "alt-f12"),
            ToggleTerminal,
            workspace,
        ),
        KeyBinding::new(
            &key(ids::TOOL_WINDOW_GIT, &format!("{primary}-9")),
            ToggleGit,
            workspace,
        ),
        KeyBinding::new(
            &key(ids::VIEW_TOGGLE_FOCUS_MODE, "ctrl-alt-z"),
            ToggleFocusMode,
            workspace,
        ),
    ]);
    bindings
}

/// Registers [`menu_key_bindings`] for the host platform.
pub fn bind_menu_keys(cx: &mut App) {
    cx.bind_keys(menu_key_bindings(Platform::current()));
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::unwrap_used,
        reason = "test harness: failures must abort the test with context"
    )]

    use std::path::PathBuf;

    use gpui::MenuItem;
    use kova_core::{Platform, RecentProject};

    use super::{
        APP_MENU_NAME, NO_RECENT_PROJECTS_LABEL, OpenRecentProject, app_menus, menu_key_bindings,
        recent_project_label,
    };

    fn labels(items: &[MenuItem]) -> Vec<String> {
        items
            .iter()
            .map(|item| match item {
                MenuItem::Separator => "-".to_owned(),
                MenuItem::Submenu(menu) => format!("{} ▸", menu.name),
                MenuItem::Action { name, .. } => name.to_string(),
                MenuItem::SystemMenu(menu) => menu.name.to_string(),
            })
            .collect()
    }

    fn recent(name: &str) -> RecentProject {
        RecentProject {
            name: name.to_owned(),
            root: PathBuf::from("/work").join(name),
        }
    }

    #[test]
    fn menu_bar_has_kova_file_edit_view() {
        let menus = app_menus(&[]);
        let names: Vec<_> = menus.iter().map(|m| m.name.to_string()).collect();
        assert_eq!(names, [APP_MENU_NAME, "File", "Edit", "View"]);
        assert_eq!(labels(&menus[0].items), ["Settings…", "-", "Quit Kova"]);
        assert_eq!(
            labels(&menus[1].items),
            [
                "Open Project…",
                "Open File…",
                "Recent Projects ▸",
                "-",
                "Save",
                "Close Tab"
            ]
        );
        assert_eq!(
            labels(&menus[2].items),
            ["Undo", "Redo", "-", "Cut", "Copy", "Paste"]
        );
        assert_eq!(
            labels(&menus[3].items),
            [
                "Search Everywhere (⇧⇧)",
                "-",
                "Toggle Project Tree",
                "Toggle Terminal",
                "Toggle Git",
                "-",
                "Focus Mode"
            ]
        );
    }

    #[test]
    fn empty_recent_projects_shows_placeholder() {
        let menus = app_menus(&[]);
        let MenuItem::Submenu(submenu) = &menus[1].items[2] else {
            panic!("Recent Projects is a submenu");
        };
        assert_eq!(labels(&submenu.items), [NO_RECENT_PROJECTS_LABEL]);
    }

    #[test]
    fn recent_projects_open_their_root_in_order() {
        let menus = app_menus(&[recent("orders"), recent("billing")]);
        let MenuItem::Submenu(submenu) = &menus[1].items[2] else {
            panic!("Recent Projects is a submenu");
        };
        assert_eq!(
            labels(&submenu.items),
            [
                "orders — /work",
                "billing — /work",
                "-",
                "Clear Recent Projects"
            ]
        );
        let MenuItem::Action { action, .. } = &submenu.items[1] else {
            panic!("project entry is an action");
        };
        assert!(action.partial_eq(&OpenRecentProject {
            path: PathBuf::from("/work/billing"),
        }));
    }

    #[test]
    fn label_of_root_level_project_has_parent() {
        let project = RecentProject {
            name: "kova-project".to_owned(),
            root: PathBuf::from("/"),
        };
        assert_eq!(recent_project_label(&project), "kova-project");
    }

    #[test]
    fn bindings_follow_the_platform_and_jetbrains_preset() {
        let keys = |platform| {
            menu_key_bindings(platform)
                .iter()
                .map(|b| {
                    (
                        b.action().name().to_owned(),
                        b.keystrokes()
                            .iter()
                            .map(gpui::KeybindingKeystroke::unparse)
                            .collect::<Vec<_>>()
                            .join(" "),
                    )
                })
                .collect::<Vec<_>>()
        };
        let mac = keys(Platform::MacOs);
        let find = |list: &[(String, String)], name: &str| {
            list.iter()
                .find(|(action, _)| action == name)
                .map(|(_, key)| key.clone())
                .unwrap()
        };
        assert!(mac.iter().all(|(action, key)| {
            !matches!(
                action.as_str(),
                "kova_menu::OpenProject" | "kova_menu::OpenFile"
            ) && key != "cmd-o"
                && key != "cmd-shift-o"
        }));
        assert_eq!(find(&mac, "kova_menu::Quit"), "cmd-q");
        assert_eq!(find(&mac, "kova_menu::OpenSettings"), "cmd-,");
        assert_eq!(find(&mac, "kova_menu::CloseTab"), "cmd-w");
        assert_eq!(find(&mac, "kova_menu::ToggleProjectTree"), "cmd-1");
        assert_eq!(find(&mac, "kova_menu::ToggleTerminal"), "alt-f12");
        assert!(mac.iter().all(|(_, key)| key != "cmd-k"));

        assert_eq!(find(&mac, "kova_menu::ToggleGit"), "cmd-9");
        assert_eq!(find(&mac, "kova_menu::ToggleFocusMode"), "ctrl-alt-z");

        let linux = keys(Platform::Linux);
        assert_eq!(find(&linux, "kova_menu::CloseTab"), "ctrl-f4");
        assert!(linux.iter().all(|(action, _)| {
            !matches!(
                action.as_str(),
                "kova_menu::OpenProject" | "kova_menu::OpenFile" | "kova_menu::Quit"
            )
        }));
    }
}
