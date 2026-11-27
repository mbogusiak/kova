//! Floating right-click context menu for the editor and *Project* tree (`KOVA-063`).
//!
//! Menu construction, label truncation, viewport clamping, and keyboard navigation live in the
//! headless [`ContextMenuModel`] layer so they are covered by unit tests without a GPUI window.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    Bounds, ClickEvent, ClipboardItem, Context, Div, InteractiveElement as _, Keystroke,
    MouseButton, MouseDownEvent, ParentElement as _, Pixels, Point, Stateful,
    StatefulInteractiveElement as _, Styled as _, Window, canvas, div, px,
};
use kova_buffer::Buffer;
use kova_syntax::{
    EntryPoint, EntryPointKind, LanguageRegistry, SyntaxEngine, SyntaxEngineConfig,
    detect_entry_points, detect_language,
};

use super::{NewEntryKind, Palette, WorkspaceShell, line_comment_prefix};
use crate::dock::project_tree::{
    fallback_fqcn_for_test_file, is_test_file_path, test_package_for_dir,
};
use crate::dock::{DockToolId, FocusedRegion, render_run_play_icon, render_tool_icon};
use crate::document::StatusKind;
use crate::editor::syntax::EnclosingEntryPoints;
use crate::theme::{KovaTheme, hsla_from_rgba};

/// Fixed width of the floating context menu in logical pixels.
pub(super) const CONTEXT_MENU_WIDTH_PX: f32 = 272.0;
/// Height of a single clickable menu row in logical pixels.
pub(super) const CONTEXT_MENU_ROW_HEIGHT_PX: f32 = 26.0;
/// Height of a horizontal separator slot in logical pixels.
pub(super) const CONTEXT_MENU_SEPARATOR_HEIGHT_PX: f32 = 9.0;
/// Inner container padding of the popup card in logical pixels.
pub(super) const CONTEXT_MENU_PADDING_PX: f32 = 4.0;
/// Minimum margin kept from viewport edges when clamping popup origin.
pub(super) const CONTEXT_MENU_EDGE_MARGIN_PX: f32 = 8.0;
/// Maximum characters for a method/class/node name in a menu label before truncation.
pub(super) const CONTEXT_MENU_MAX_NAME_CHARS: usize = 44;

/// Leading icon shown beside a context-menu action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum ContextMenuIcon {
    /// Green play triangle (`Run` / `Send`).
    Run,
    /// Debugger bug glyph (`Debug`).
    Debug,
    /// Empty leading gutter slot (keeps action labels aligned).
    None,
}

/// Concrete action triggered by clicking or pressing `Enter` on a context-menu row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ContextMenuAction {
    /// Run a detected entry point (`JvmTestMethod`, `JvmTestClass`, `main`, `.http`).
    RunEntryPoint(EntryPoint),
    /// Debug a detected JVM entry point (`JvmTestMethod`, `JvmTestClass`, `main`).
    DebugEntryPoint(EntryPoint),
    /// Jump to definition at the right-clicked UTF-8 byte offset.
    EditorGoToDefinition(usize),
    /// Cut active selection/line to clipboard.
    EditorCut,
    /// Copy active selection/line to clipboard.
    EditorCopy,
    /// Paste plain text from clipboard at active cursor.
    EditorPaste,
    /// Toggle line comment for active selection/line.
    EditorToggleLineComment,
    /// Reformat active buffer.
    EditorReformatCode,
    /// Run all tests in the selected *Project* tree file or directory.
    ProjectRunTests {
        /// Project-relative path of the clicked node (empty for project root).
        rel_path: PathBuf,
        /// Whether the clicked node is a directory.
        is_dir: bool,
        /// Display name shown in the *Test Runner* tab.
        label: String,
    },
    /// Open the inline `New File` modal anchored to the clicked node.
    ProjectNewFile,
    /// Open the inline `New Folder` modal anchored to the clicked node.
    ProjectNewFolder,
    /// Copy the full (or demo-relative) path of the clicked node to the clipboard.
    ProjectCopyPath(PathBuf),
    /// Reveal the clicked node in the OS file manager (`Finder` / `Explorer` / file manager).
    ProjectRevealInFinder(PathBuf),
}

/// Single entry inside a [`ContextMenuModel`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(
    clippy::large_enum_variant,
    reason = "menus hold at most ~15 short-lived items; boxing adds noise without benefit"
)]
pub(super) enum ContextMenuItem {
    /// Clickable row with optional icon and shortcut hint.
    Action {
        /// Stable element ID suffix.
        id: &'static str,
        /// Leading icon kind.
        icon: ContextMenuIcon,
        /// Menu item label.
        label: String,
        /// Right-aligned keyboard shortcut hint.
        shortcut: Option<&'static str>,
        /// Whether the item can be activated.
        enabled: bool,
        /// Action executed when triggered.
        action: ContextMenuAction,
    },
    /// Horizontal divider between logical groups.
    Separator,
}

impl ContextMenuItem {
    const fn is_enabled_action(&self) -> bool {
        matches!(self, Self::Action { enabled: true, .. })
    }
}

/// Input parameters for building an editor right-click context menu.
#[derive(Debug, Clone, Copy)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent enabled-state flags captured once when the menu opens"
)]
pub(super) struct EditorContextMenuInput<'a> {
    /// Narrowest entry points enclosing the clicked line.
    pub(super) enclosing: EnclosingEntryPoints<'a>,
    /// UTF-8 byte offset under the right-click pointer.
    pub(super) clicked_byte: usize,
    /// Whether `clicked_byte` sits on a navigable identifier.
    pub(super) can_go_to_definition: bool,
    /// Whether the editor buffer is read-only.
    pub(super) read_only: bool,
    /// Whether the clipboard currently contains non-empty text.
    pub(super) can_paste: bool,
    /// Whether the current platform uses macOS shortcut labels (`⌘`, `⌥`).
    pub(super) macos: bool,
}

/// Truncates `text` to at most `max_chars` Unicode scalar values, appending `…` when trimmed.
#[must_use]
pub(super) fn truncate_menu_label(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars || max_chars <= 1 {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Clamps a popup origin `(anchor.x, anchor.y)` so a popup of `(width, height)` stays inside
/// `viewport_px` with [`CONTEXT_MENU_EDGE_MARGIN_PX`] breathing room.
#[must_use]
pub(super) fn compute_context_menu_origin(
    anchor: (f32, f32),
    width: f32,
    height: f32,
    viewport_px: (f32, f32),
) -> (f32, f32) {
    let margin = CONTEXT_MENU_EDGE_MARGIN_PX;
    let max_x = (viewport_px.0 - width - margin).max(margin);
    let max_y = (viewport_px.1 - height - margin).max(margin);
    (anchor.0.clamp(margin, max_x), anchor.1.clamp(margin, max_y))
}

/// Builds the deterministic item list for an editor right-click context menu.
#[must_use]
pub(super) fn build_editor_context_menu(input: EditorContextMenuInput<'_>) -> Vec<ContextMenuItem> {
    let mut items = Vec::new();
    append_editor_run_debug_items(&mut items, input.enclosing);

    let (sc_def, sc_cut, sc_copy, sc_paste, sc_comment, sc_reformat) = if input.macos {
        ("⌘B", "⌘X", "⌘C", "⌘V", "⌘/", "⌥⌘L")
    } else {
        (
            "Ctrl+B",
            "Ctrl+X",
            "Ctrl+C",
            "Ctrl+V",
            "Ctrl+/",
            "Ctrl+Alt+L",
        )
    };

    items.push(ContextMenuItem::Action {
        id: "go-to-definition",
        icon: ContextMenuIcon::None,
        label: "Go to Definition".to_owned(),
        shortcut: Some(sc_def),
        enabled: input.can_go_to_definition,
        action: ContextMenuAction::EditorGoToDefinition(input.clicked_byte),
    });
    items.push(ContextMenuItem::Separator);
    items.push(ContextMenuItem::Action {
        id: "cut",
        icon: ContextMenuIcon::None,
        label: "Cut".to_owned(),
        shortcut: Some(sc_cut),
        enabled: !input.read_only,
        action: ContextMenuAction::EditorCut,
    });
    items.push(ContextMenuItem::Action {
        id: "copy",
        icon: ContextMenuIcon::None,
        label: "Copy".to_owned(),
        shortcut: Some(sc_copy),
        enabled: true,
        action: ContextMenuAction::EditorCopy,
    });
    items.push(ContextMenuItem::Action {
        id: "paste",
        icon: ContextMenuIcon::None,
        label: "Paste".to_owned(),
        shortcut: Some(sc_paste),
        enabled: !input.read_only && input.can_paste,
        action: ContextMenuAction::EditorPaste,
    });
    items.push(ContextMenuItem::Separator);
    items.push(ContextMenuItem::Action {
        id: "toggle-line-comment",
        icon: ContextMenuIcon::None,
        label: "Toggle Line Comment".to_owned(),
        shortcut: Some(sc_comment),
        enabled: !input.read_only,
        action: ContextMenuAction::EditorToggleLineComment,
    });
    items.push(ContextMenuItem::Action {
        id: "reformat-code",
        icon: ContextMenuIcon::None,
        label: "Reformat Code".to_owned(),
        shortcut: Some(sc_reformat),
        enabled: !input.read_only,
        action: ContextMenuAction::EditorReformatCode,
    });

    items
}

#[allow(
    clippy::too_many_lines,
    reason = "flat list of Run/Debug sections mirrors the spec's menu layout"
)]
fn append_editor_run_debug_items(
    items: &mut Vec<ContextMenuItem>,
    enclosing: EnclosingEntryPoints<'_>,
) {
    match (enclosing.method, enclosing.class) {
        (Some(method), class) => match method.kind {
            EntryPointKind::JvmTestMethod => {
                let raw_name = method.method_name.as_deref().unwrap_or(&method.label);
                let name = truncate_menu_label(raw_name, CONTEXT_MENU_MAX_NAME_CHARS);
                items.push(ContextMenuItem::Action {
                    id: "run-method",
                    icon: ContextMenuIcon::Run,
                    label: format!("Run '{name}'"),
                    shortcut: None,
                    enabled: true,
                    action: ContextMenuAction::RunEntryPoint(method.clone()),
                });
                items.push(ContextMenuItem::Action {
                    id: "debug-method",
                    icon: ContextMenuIcon::Debug,
                    label: format!("Debug '{name}'"),
                    shortcut: None,
                    enabled: true,
                    action: ContextMenuAction::DebugEntryPoint(method.clone()),
                });
                if let Some(cls) = class {
                    let cls_raw = cls.class_name.as_deref().unwrap_or(&cls.label);
                    let cls_name = truncate_menu_label(cls_raw, CONTEXT_MENU_MAX_NAME_CHARS);
                    items.push(ContextMenuItem::Action {
                        id: "run-class",
                        icon: ContextMenuIcon::Run,
                        label: format!("Run '{cls_name}'"),
                        shortcut: None,
                        enabled: true,
                        action: ContextMenuAction::RunEntryPoint(cls.clone()),
                    });
                    items.push(ContextMenuItem::Action {
                        id: "debug-class",
                        icon: ContextMenuIcon::Debug,
                        label: format!("Debug '{cls_name}'"),
                        shortcut: None,
                        enabled: true,
                        action: ContextMenuAction::DebugEntryPoint(cls.clone()),
                    });
                }
                items.push(ContextMenuItem::Separator);
            }
            EntryPointKind::JavaMain | EntryPointKind::KotlinMain => {
                let main_name = match method.class_name.as_deref().filter(|s| !s.is_empty()) {
                    Some(cls) => format!("{cls}.main()"),
                    None => "main()".to_owned(),
                };
                let main_name = truncate_menu_label(&main_name, CONTEXT_MENU_MAX_NAME_CHARS);
                items.push(ContextMenuItem::Action {
                    id: "run-main",
                    icon: ContextMenuIcon::Run,
                    label: format!("Run '{main_name}'"),
                    shortcut: None,
                    enabled: true,
                    action: ContextMenuAction::RunEntryPoint(method.clone()),
                });
                items.push(ContextMenuItem::Action {
                    id: "debug-main",
                    icon: ContextMenuIcon::Debug,
                    label: format!("Debug '{main_name}'"),
                    shortcut: None,
                    enabled: true,
                    action: ContextMenuAction::DebugEntryPoint(method.clone()),
                });
                items.push(ContextMenuItem::Separator);
            }
            EntryPointKind::HttpRequest => {
                let summary = match (&method.http_method, &method.http_url) {
                    (Some(m), Some(u)) => format!("{m} {u}"),
                    _ => method.label.clone(),
                };
                let summary = truncate_menu_label(&summary, CONTEXT_MENU_MAX_NAME_CHARS);
                items.push(ContextMenuItem::Action {
                    id: "send-http",
                    icon: ContextMenuIcon::Run,
                    label: format!("Send '{summary}'"),
                    shortcut: None,
                    enabled: true,
                    action: ContextMenuAction::RunEntryPoint(method.clone()),
                });
                items.push(ContextMenuItem::Separator);
            }
            EntryPointKind::JvmTestClass => {}
        },
        (None, Some(cls)) => {
            let cls_raw = cls.class_name.as_deref().unwrap_or(&cls.label);
            let cls_name = truncate_menu_label(cls_raw, CONTEXT_MENU_MAX_NAME_CHARS);
            items.push(ContextMenuItem::Action {
                id: "run-class",
                icon: ContextMenuIcon::Run,
                label: format!("Run '{cls_name}'"),
                shortcut: None,
                enabled: true,
                action: ContextMenuAction::RunEntryPoint(cls.clone()),
            });
            items.push(ContextMenuItem::Action {
                id: "debug-class",
                icon: ContextMenuIcon::Debug,
                label: format!("Debug '{cls_name}'"),
                shortcut: None,
                enabled: true,
                action: ContextMenuAction::DebugEntryPoint(cls.clone()),
            });
            items.push(ContextMenuItem::Separator);
        }
        (None, None) => {}
    }
}

/// Builds the deterministic item list for a *Project* tree right-click context menu.
#[must_use]
#[allow(
    clippy::fn_params_excessive_bools,
    reason = "node kind and platform flags are independent inputs of a pure builder"
)]
pub(super) fn build_project_tree_context_menu(
    rel_path: PathBuf,
    node_name: &str,
    is_dir: bool,
    is_test: bool,
    macos: bool,
    windows: bool,
) -> Vec<ContextMenuItem> {
    let mut items = Vec::new();
    if is_test {
        let short_name = truncate_menu_label(node_name, CONTEXT_MENU_MAX_NAME_CHARS);
        items.push(ContextMenuItem::Action {
            id: "project-run-tests",
            icon: ContextMenuIcon::Run,
            label: format!("Run Tests in '{short_name}'"),
            shortcut: None,
            enabled: true,
            action: ContextMenuAction::ProjectRunTests {
                rel_path: rel_path.clone(),
                is_dir,
                label: node_name.to_owned(),
            },
        });
        items.push(ContextMenuItem::Separator);
    }

    items.push(ContextMenuItem::Action {
        id: "project-new-file",
        icon: ContextMenuIcon::None,
        label: "New File...".to_owned(),
        shortcut: None,
        enabled: true,
        action: ContextMenuAction::ProjectNewFile,
    });
    items.push(ContextMenuItem::Action {
        id: "project-new-folder",
        icon: ContextMenuIcon::None,
        label: "New Folder...".to_owned(),
        shortcut: None,
        enabled: true,
        action: ContextMenuAction::ProjectNewFolder,
    });
    items.push(ContextMenuItem::Separator);
    items.push(ContextMenuItem::Action {
        id: "project-copy-path",
        icon: ContextMenuIcon::None,
        label: "Copy Path".to_owned(),
        shortcut: None,
        enabled: true,
        action: ContextMenuAction::ProjectCopyPath(rel_path.clone()),
    });

    let reveal_label = if macos {
        "Reveal in Finder"
    } else if windows {
        "Reveal in Explorer"
    } else {
        "Show in File Manager"
    };
    items.push(ContextMenuItem::Action {
        id: "project-reveal",
        icon: ContextMenuIcon::None,
        label: reveal_label.to_owned(),
        shortcut: None,
        enabled: true,
        action: ContextMenuAction::ProjectRevealInFinder(rel_path),
    });

    items
}

/// Outcome of feeding a keystroke into [`ContextMenuModel::handle_key`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ContextMenuKeyOutcome {
    /// Menu selection changed (caller should re-render).
    Consumed,
    /// User pressed `Enter` on an enabled item.
    Activate,
    /// User pressed `Escape` to dismiss the menu.
    Close,
    /// Key is ignored (not consumed by the context menu).
    Ignored,
}

/// Headless state model for the floating context menu.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ContextMenuModel {
    items: Vec<ContextMenuItem>,
    selected_index: Option<usize>,
    anchor_px: (f32, f32),
}

impl ContextMenuModel {
    /// Creates a new [`ContextMenuModel`] anchored at `anchor_px`.
    #[must_use]
    pub(super) const fn new(anchor_px: (f32, f32), items: Vec<ContextMenuItem>) -> Self {
        Self {
            items,
            selected_index: None,
            anchor_px,
        }
    }

    /// Returns the items rendered in the menu.
    #[must_use]
    pub(super) fn items(&self) -> &[ContextMenuItem] {
        &self.items
    }

    /// Returns the currently highlighted row index (if navigated by keyboard).
    #[must_use]
    pub(super) const fn selected_index(&self) -> Option<usize> {
        self.selected_index
    }

    /// Returns the raw window-space anchor `(x, y)` in logical pixels.
    #[must_use]
    pub(super) const fn anchor_px(&self) -> (f32, f32) {
        self.anchor_px
    }

    /// Estimates the rendered height of the menu in logical pixels for viewport clamping.
    #[must_use]
    pub(super) fn estimated_height(&self) -> f32 {
        let rows_h: f32 = self
            .items
            .iter()
            .map(|item| match item {
                ContextMenuItem::Action { .. } => CONTEXT_MENU_ROW_HEIGHT_PX,
                ContextMenuItem::Separator => CONTEXT_MENU_SEPARATOR_HEIGHT_PX,
            })
            .sum();
        rows_h + CONTEXT_MENU_PADDING_PX * 2.0 + 2.0
    }

    /// Moves keyboard highlight by `delta` (`+1` or `-1`), skipping separators and disabled items.
    pub(super) fn move_selection(&mut self, delta: isize) {
        let enabled_indices: Vec<usize> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(idx, item)| item.is_enabled_action().then_some(idx))
            .collect();
        if enabled_indices.is_empty() {
            self.selected_index = None;
            return;
        }
        let next_pos = match self
            .selected_index
            .and_then(|cur| enabled_indices.iter().position(|&idx| idx == cur))
        {
            Some(pos) => {
                let len = enabled_indices.len();
                let step = delta.unsigned_abs() % len;
                if delta >= 0 {
                    (pos + step) % len
                } else {
                    (pos + len - step) % len
                }
            }
            None => {
                if delta >= 0 {
                    0
                } else {
                    enabled_indices.len() - 1
                }
            }
        };
        self.selected_index = Some(enabled_indices[next_pos]);
    }

    /// Returns the action of the highlighted row when it is enabled.
    #[must_use]
    pub(super) fn selected_action(&self) -> Option<&ContextMenuAction> {
        let idx = self.selected_index?;
        match self.items.get(idx)? {
            ContextMenuItem::Action {
                enabled: true,
                action,
                ..
            } => Some(action),
            _ => None,
        }
    }

    /// Handles a keystroke while the context menu is open.
    pub(super) fn handle_key(&mut self, keystroke: &Keystroke) -> ContextMenuKeyOutcome {
        let mods = &keystroke.modifiers;
        if mods.platform || mods.control || mods.alt {
            return ContextMenuKeyOutcome::Ignored;
        }
        match keystroke.key.as_str() {
            "escape" => ContextMenuKeyOutcome::Close,
            "down" => {
                self.move_selection(1);
                ContextMenuKeyOutcome::Consumed
            }
            "up" => {
                self.move_selection(-1);
                ContextMenuKeyOutcome::Consumed
            }
            "enter" => {
                if self.selected_action().is_some() {
                    ContextMenuKeyOutcome::Activate
                } else {
                    ContextMenuKeyOutcome::Close
                }
            }
            _ => ContextMenuKeyOutcome::Ignored,
        }
    }
}

/// Active context menu wrapper storing the headless model and painted bounds.
#[derive(Debug, Clone)]
pub(super) struct ContextMenuState {
    model: ContextMenuModel,
    bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
}

impl ContextMenuState {
    fn new(model: ContextMenuModel) -> Self {
        Self {
            model,
            bounds: Rc::new(Cell::new(None)),
        }
    }
}

impl WorkspaceShell {
    /// Opens the editor right-click context menu at `position` for `(row, byte)`.
    pub(super) fn open_editor_context_menu(
        &mut self,
        position: Point<Pixels>,
        row: usize,
        byte: usize,
        cx: &mut Context<'_, Self>,
    ) {
        self.close_completion_popup(cx);
        self.close_project_options(cx);

        let Some(editor) = self.active_editor().cloned() else {
            return;
        };
        let can_paste = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .is_some_and(|text| !text.is_empty());

        let items = {
            let ed = editor.read(cx);
            let enclosing = ed
                .syntax()
                .map_or_else(Default::default, |syn| syn.enclosing_entry_points_at(row));
            build_editor_context_menu(EditorContextMenuInput {
                enclosing,
                clicked_byte: byte,
                can_go_to_definition: ed.can_go_to_definition_at(byte),
                read_only: ed.is_read_only(),
                can_paste,
                macos: cfg!(target_os = "macos"),
            })
        };
        self.context_menu = Some(ContextMenuState::new(ContextMenuModel::new(
            (f32::from(position.x), f32::from(position.y)),
            items,
        )));
        cx.notify();
    }

    /// Opens the *Project* tree right-click context menu for visible `row` at `position`.
    pub(super) fn open_project_context_menu(
        &mut self,
        row: usize,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.close_completion_popup(cx);
        self.close_project_options(cx);
        self.project_tree.select_index(row);
        self.dock_layout.focus(FocusedRegion::ProjectDock);
        window.focus(&self.focus_handle);

        let Some(node) = self.project_tree.visible_row(row).cloned() else {
            return;
        };
        let is_dir = node.is_dir();
        let is_test = if is_dir {
            self.project_tree.is_test_directory(&node.path)
        } else {
            is_test_file_path(&node.path) || self.open_syntax_has_tests(&node.path, cx)
        };
        let items = build_project_tree_context_menu(
            node.path,
            &node.name,
            is_dir,
            is_test,
            cfg!(target_os = "macos"),
            cfg!(target_os = "windows"),
        );
        self.context_menu = Some(ContextMenuState::new(ContextMenuModel::new(
            (f32::from(position.x), f32::from(position.y)),
            items,
        )));
        cx.notify();
    }

    /// Closes the floating context menu if open.
    pub(super) fn close_context_menu(&mut self, cx: &mut Context<'_, Self>) {
        if self.context_menu.take().is_some() {
            cx.notify();
        }
    }

    /// Capture-phase key handler for the open context menu (`Esc`, `Up`, `Down`, `Enter`).
    pub(super) fn handle_context_menu_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let Some(state) = self.context_menu.as_mut() else {
            return false;
        };
        match state.model.handle_key(keystroke) {
            ContextMenuKeyOutcome::Close => {
                self.close_context_menu(cx);
                cx.stop_propagation();
                true
            }
            ContextMenuKeyOutcome::Consumed => {
                cx.stop_propagation();
                cx.notify();
                true
            }
            ContextMenuKeyOutcome::Activate => {
                let action = state.model.selected_action().cloned();
                self.close_context_menu(cx);
                if let Some(action) = action {
                    self.execute_context_menu_action(action, window, cx);
                }
                cx.stop_propagation();
                true
            }
            ContextMenuKeyOutcome::Ignored => false,
        }
    }

    /// Dismisses the floating context menu when a mouse down lands outside its bounds.
    pub(super) fn handle_context_menu_outside_click(
        &mut self,
        event: &MouseDownEvent,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(state) = self.context_menu.as_ref() else {
            return;
        };
        let inside = state
            .bounds
            .get()
            .is_some_and(|b| b.contains(&event.position));
        if !inside {
            self.close_context_menu(cx);
        }
    }

    /// Executes `action` triggered from the floating context menu.
    pub(super) fn execute_context_menu_action(
        &mut self,
        action: ContextMenuAction,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match action {
            ContextMenuAction::RunEntryPoint(entry) => {
                let path = self.active_document_key();
                let request =
                    crate::editor::syntax::RunEntryPointRequested::from_entry_point(&entry, path);
                if request.kind == EntryPointKind::HttpRequest {
                    if let Some(editor) = self.active_editor() {
                        let text = editor.read(cx).shared_buffer().snapshot().text();
                        self.run_http_request(&request, &text, cx);
                    }
                } else {
                    self.handle_run_request(&request, cx);
                }
            }
            ContextMenuAction::DebugEntryPoint(entry) => {
                let path = self.active_document_key();
                self.start_debug_for_entry_point(&entry, path, cx);
            }
            ContextMenuAction::EditorGoToDefinition(byte) => {
                self.go_to_definition_at(Some(byte), window, cx);
            }
            ContextMenuAction::EditorCut => {
                if let Some(editor) = self.active_editor().cloned() {
                    editor.update(cx, |ed, cx| ed.cut_selection(window, cx));
                }
            }
            ContextMenuAction::EditorCopy => {
                if let Some(editor) = self.active_editor().cloned() {
                    editor.update(cx, crate::editor::EditorView::copy_selection);
                }
            }
            ContextMenuAction::EditorPaste => {
                if let Some(editor) = self.active_editor().cloned() {
                    editor.update(cx, |ed, cx| ed.paste_from_clipboard(window, cx));
                }
            }
            ContextMenuAction::EditorToggleLineComment => {
                let prefix = line_comment_prefix(self.active_document_key().as_deref());
                if let Some(editor) = self.active_editor().cloned() {
                    editor.update(cx, |ed, cx| ed.toggle_line_comment(prefix, window, cx));
                }
            }
            ContextMenuAction::EditorReformatCode => {
                let indent_width = usize::from(self.applied_settings.editor.tab_size).max(1);
                if let Some(editor) = self.active_editor().cloned() {
                    editor.update(cx, |ed, cx| ed.reformat_code(indent_width, window, cx));
                }
            }
            ContextMenuAction::ProjectRunTests {
                rel_path,
                is_dir,
                label,
            } => {
                self.run_tests_for_project_node(rel_path, is_dir, label, cx);
            }
            ContextMenuAction::ProjectNewFile => {
                self.open_new_item_modal(NewEntryKind::File, window, cx);
            }
            ContextMenuAction::ProjectNewFolder => {
                self.open_new_item_modal(NewEntryKind::Directory, window, cx);
            }
            ContextMenuAction::ProjectCopyPath(rel_path) => {
                let text = match self.project.root() {
                    Some(root) if rel_path.as_os_str().is_empty() => root.display().to_string(),
                    Some(root) => root.join(&rel_path).display().to_string(),
                    None => rel_path.display().to_string(),
                };
                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                self.set_notice(StatusKind::Info, format!("Copied path: {text}"), cx);
            }
            ContextMenuAction::ProjectRevealInFinder(rel_path) => {
                self.reveal_project_node_in_file_manager(&rel_path, cx);
            }
        }
        cx.notify();
    }

    fn open_syntax_has_tests(&self, rel_path: &Path, cx: &Context<'_, Self>) -> bool {
        self.open_syntax_test_targets(rel_path, cx)
            .is_some_and(|targets| !targets.is_empty())
    }

    fn open_syntax_test_targets(
        &self,
        rel_path: &Path,
        cx: &Context<'_, Self>,
    ) -> Option<Vec<String>> {
        for pane in self.panes.values() {
            for tab in pane.tabs.tabs() {
                if tab.path.as_deref() == Some(rel_path)
                    && let Some(editor) = pane.editors.get(&tab.id)
                    && let Some(syntax) = editor.read(cx).syntax()
                {
                    let classes: Vec<String> = syntax
                        .entry_points()
                        .iter()
                        .filter(|ep| ep.kind == EntryPointKind::JvmTestClass)
                        .map(EntryPoint::qualified_target)
                        .collect();
                    if !classes.is_empty() {
                        return Some(classes);
                    }
                    let methods: Vec<String> = syntax
                        .entry_points()
                        .iter()
                        .filter(|ep| ep.kind == EntryPointKind::JvmTestMethod)
                        .map(EntryPoint::qualified_target)
                        .collect();
                    return Some(methods);
                }
            }
        }
        None
    }

    fn run_tests_for_project_node(
        &mut self,
        rel_path: PathBuf,
        is_dir: bool,
        label: String,
        cx: &mut Context<'_, Self>,
    ) {
        if is_dir {
            let targets = test_package_for_dir(&rel_path)
                .map(|pkg| vec![format!("{pkg}.*")])
                .unwrap_or_default();
            let file = self.project.root().map(|root| {
                if rel_path.as_os_str().is_empty() {
                    root.to_path_buf()
                } else {
                    root.join(&rel_path)
                }
            });
            let req = self
                .test_runner
                .new_request(label, targets, self.project.root_owned(), file);
            self.start_test_run(req, cx);
            return;
        }

        if let Some(targets) = self.open_syntax_test_targets(&rel_path, cx) {
            if targets.is_empty() {
                self.set_notice(
                    StatusKind::Info,
                    format!("No runnable tests found in '{}'", rel_path.display()),
                    cx,
                );
                return;
            }
            let file = self.project.root().map(|root| root.join(&rel_path));
            let req = self
                .test_runner
                .new_request(label, targets, self.project.root_owned(), file);
            self.start_test_run(req, cx);
            return;
        }

        let Some(root) = self.project.root_owned() else {
            if let Some(fqcn) = fallback_fqcn_for_test_file(&rel_path) {
                let req = self.test_runner.new_request(label, vec![fqcn], None, None);
                self.start_test_run(req, cx);
            } else {
                self.set_notice(
                    StatusKind::Info,
                    format!("No runnable tests found in '{}'", rel_path.display()),
                    cx,
                );
            }
            return;
        };

        let abs_path = root.join(&rel_path);
        let engine = self.syntax_engine.clone();
        let rel_clone = rel_path.clone();
        let abs_clone = abs_path.clone();
        let bg = cx.background_executor().spawn(async move {
            resolve_file_test_targets_on_disk(&abs_clone, &rel_clone, engine)
        });
        cx.spawn(async move |this, cx| {
            let result = bg.await;
            drop(this.update(cx, |this, cx| match result {
                Ok(targets) if targets.is_empty() => {
                    this.set_notice(
                        StatusKind::Info,
                        format!("No runnable tests found in '{}'", rel_path.display()),
                        cx,
                    );
                }
                Ok(targets) => {
                    let req = this.test_runner.new_request(
                        label,
                        targets,
                        this.project.root_owned(),
                        Some(abs_path),
                    );
                    this.start_test_run(req, cx);
                    cx.notify();
                }
                Err(err) => {
                    this.set_notice(StatusKind::Error, err, cx);
                }
            }));
        })
        .detach();
    }

    fn reveal_project_node_in_file_manager(&mut self, rel_path: &Path, cx: &mut Context<'_, Self>) {
        let Some(root) = self.project.root() else {
            self.set_notice(
                StatusKind::Info,
                "Reveal in Finder is only available for projects opened from disk".to_owned(),
                cx,
            );
            return;
        };
        let is_root = rel_path.as_os_str().is_empty();
        let abs_path = if is_root {
            root.to_path_buf()
        } else {
            root.join(rel_path)
        };
        let bg = cx
            .background_executor()
            .spawn(async move { reveal_in_os_file_manager(&abs_path, is_root) });
        cx.spawn(async move |this, cx| {
            if let Err(err) = bg.await {
                drop(this.update(cx, |this, cx| {
                    this.set_notice(StatusKind::Error, err, cx);
                }));
            }
        })
        .detach();
    }

    /// Renders the floating context menu overlay when open.
    #[allow(
        clippy::too_many_lines,
        reason = "single GPUI element tree for the popup card and its rows"
    )]
    pub(super) fn render_context_menu_overlay(
        &self,
        p: Palette,
        cx: &Context<'_, Self>,
    ) -> Option<Stateful<Div>> {
        let state = self.context_menu.as_ref()?;
        let theme = cx.global::<KovaTheme>();
        let elevated = hsla_from_rgba(theme.surfaces().elevated);
        let border_strong = hsla_from_rgba(theme.borders().strong);
        let width = CONTEXT_MENU_WIDTH_PX;
        let height = state.model.estimated_height();
        let (x, y) =
            compute_context_menu_origin(state.model.anchor_px(), width, height, self.viewport_px);
        let selected = state.model.selected_index();
        let bounds_slot = Rc::clone(&state.bounds);

        let mut menu = div()
            .id("workspace-context-menu")
            .absolute()
            .left(px(x))
            .top(px(y))
            .w(px(width))
            .p(px(CONTEXT_MENU_PADDING_PX))
            .flex()
            .flex_col()
            .rounded(px(8.0))
            .bg(elevated)
            .border_1()
            .border_color(border_strong)
            .shadow_lg()
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .child(
                canvas(
                    move |bounds, _, _| {
                        bounds_slot.set(Some(bounds));
                    },
                    |_, (), _, _| {},
                )
                .absolute()
                .size_full(),
            );

        for (idx, item) in state.model.items().iter().enumerate() {
            match item {
                ContextMenuItem::Separator => {
                    menu = menu.child(
                        div()
                            .h(px(CONTEXT_MENU_SEPARATOR_HEIGHT_PX))
                            .w_full()
                            .flex()
                            .items_center()
                            .px(px(6.0))
                            .child(div().h(px(1.0)).w_full().bg(p.border_subtle)),
                    );
                }
                ContextMenuItem::Action {
                    id,
                    icon,
                    label,
                    shortcut,
                    enabled,
                    action,
                } => {
                    let is_selected = selected == Some(idx);
                    let fg = if *enabled {
                        p.text_primary
                    } else {
                        p.text_muted
                    };
                    let icon_color = if *enabled { p.run_icon } else { p.text_muted };
                    let icon_slot = match icon {
                        ContextMenuIcon::Run => div()
                            .w(px(16.0))
                            .h(px(16.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(render_run_play_icon(icon_color)),
                        ContextMenuIcon::Debug => div()
                            .w(px(16.0))
                            .h(px(16.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(render_tool_icon(DockToolId::Debug, icon_color)),
                        ContextMenuIcon::None => div().w(px(16.0)).h(px(16.0)).flex_none(),
                    };

                    let mut row = div()
                        .id(("context-menu-item", idx))
                        .h(px(CONTEXT_MENU_ROW_HEIGHT_PX))
                        .w_full()
                        .px(px(8.0))
                        .rounded(px(5.0))
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(px(8.0))
                        .text_size(px(12.0))
                        .text_color(fg);

                    if *enabled {
                        let action = action.clone();
                        let _ = id;
                        row = row.cursor_pointer().hover(move |s| s.bg(p.hover)).on_click(
                            cx.listener(move |this, _: &ClickEvent, window, cx| {
                                cx.stop_propagation();
                                this.close_context_menu(cx);
                                this.execute_context_menu_action(action.clone(), window, cx);
                            }),
                        );
                    } else {
                        row = row.opacity(0.5);
                    }
                    if is_selected && *enabled {
                        row = row.bg(p.active);
                    }

                    let left = div()
                        .flex_1()
                        .min_w(px(0.0))
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .overflow_hidden()
                        .child(icon_slot)
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .child(label.clone()),
                        );
                    row = row.child(left);
                    if let Some(sc) = shortcut {
                        row = row.child(
                            div()
                                .flex_none()
                                .text_size(px(11.0))
                                .text_color(p.text_muted)
                                .whitespace_nowrap()
                                .child(*sc),
                        );
                    }
                    menu = menu.child(row);
                }
            }
        }

        Some(menu)
    }
}

fn resolve_file_test_targets_on_disk(
    abs_path: &Path,
    rel_path: &Path,
    existing_engine: Option<Arc<SyntaxEngine>>,
) -> Result<Vec<String>, String> {
    let source = std::fs::read_to_string(abs_path)
        .map_err(|err| format!("Cannot read '{}': {err}", rel_path.display()))?;
    let Some(lang) = detect_language(rel_path) else {
        return Ok(fallback_fqcn_for_test_file(rel_path).into_iter().collect());
    };
    let engine = if let Some(engine) = existing_engine {
        engine
    } else {
        let registry =
            LanguageRegistry::builtin().map_err(|err| format!("Syntax registry error: {err}"))?;
        Arc::new(
            SyntaxEngine::new(registry.clone(), SyntaxEngineConfig::default())
                .map_err(|err| format!("Syntax engine error: {err}"))?,
        )
    };
    let snapshot = Buffer::from_text(&source).snapshot();
    let tree = engine
        .parse_sync(lang, &snapshot, None)
        .map_err(|err| format!("Cannot parse '{}': {err}", rel_path.display()))?;
    let eps = detect_entry_points(&tree, &snapshot);
    let classes: Vec<String> = eps
        .iter()
        .filter(|ep| ep.kind == EntryPointKind::JvmTestClass)
        .map(EntryPoint::qualified_target)
        .collect();
    if !classes.is_empty() {
        return Ok(classes);
    }
    let methods: Vec<String> = eps
        .iter()
        .filter(|ep| ep.kind == EntryPointKind::JvmTestMethod)
        .map(EntryPoint::qualified_target)
        .collect();
    Ok(methods)
}

fn reveal_in_os_file_manager(abs_path: &Path, is_root: bool) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let status = if is_root {
        std::process::Command::new("open").arg(abs_path).status()
    } else {
        std::process::Command::new("open")
            .arg("-R")
            .arg(abs_path)
            .status()
    };

    #[cfg(target_os = "windows")]
    let status = if is_root {
        std::process::Command::new("explorer")
            .arg(abs_path)
            .status()
    } else {
        std::process::Command::new("explorer")
            .arg(format!("/select,{}", abs_path.display()))
            .status()
    };

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let status = {
        let _ = is_root;
        let dir = if abs_path.is_dir() {
            abs_path
        } else {
            abs_path.parent().unwrap_or(abs_path)
        };
        std::process::Command::new("xdg-open").arg(dir).status()
    };

    match status {
        Ok(exit) if exit.success() => Ok(()),
        Ok(exit) => Err(format!("File manager exited with status {exit}")),
        Err(err) => Err(format!("Cannot open file manager: {err}")),
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::unwrap_used,
        reason = "unit tests assert deterministic context menu construction, clamping, and key navigation"
    )]

    use kova_syntax::GutterIconRole;

    use super::*;

    fn sample_method(name: &str, class: &str) -> EntryPoint {
        EntryPoint {
            id: format!("com.kova.{class}#{name}"),
            kind: EntryPointKind::JvmTestMethod,
            icon_role: GutterIconRole::Run,
            label: format!("Run '{class}.{name}()'"),
            line: 5,
            start_line: 4,
            end_line: 8,
            byte_range: 10..40,
            package_name: Some("com.kova".to_owned()),
            class_name: Some(class.to_owned()),
            method_name: Some(name.to_owned()),
            request_index: None,
            http_method: None,
            http_url: None,
        }
    }

    fn sample_class(class: &str) -> EntryPoint {
        EntryPoint {
            id: format!("com.kova.{class}"),
            kind: EntryPointKind::JvmTestClass,
            icon_role: GutterIconRole::RunAll,
            label: format!("Run '{class}'"),
            line: 2,
            start_line: 2,
            end_line: 20,
            byte_range: 0..100,
            package_name: Some("com.kova".to_owned()),
            class_name: Some(class.to_owned()),
            method_name: None,
            request_index: None,
            http_method: None,
            http_url: None,
        }
    }

    #[test]
    fn editor_menu_for_test_method_class_and_read_only_states() {
        let method = sample_method(
            "should calculate total with very long Spock feature description exceeding forty four chars",
            "OrderSpec",
        );
        let class = sample_class("OrderSpec");

        // Inside test method + enclosing class.
        let items = build_editor_context_menu(EditorContextMenuInput {
            enclosing: EnclosingEntryPoints {
                method: Some(&method),
                class: Some(&class),
            },
            clicked_byte: 25,
            can_go_to_definition: true,
            read_only: false,
            can_paste: true,
            macos: true,
        });
        let labels: Vec<&str> = items
            .iter()
            .filter_map(|item| match item {
                ContextMenuItem::Action { label, .. } => Some(label.as_str()),
                ContextMenuItem::Separator => None,
            })
            .collect();
        assert!(labels[0].starts_with("Run 'should calculate total"));
        assert!(labels[0].contains('…'));
        assert!(labels[1].starts_with("Debug 'should calculate total"));
        assert_eq!(labels[2], "Run 'OrderSpec'");
        assert_eq!(labels[3], "Debug 'OrderSpec'");
        assert_eq!(labels[4], "Go to Definition");
        assert_eq!(labels[5], "Cut");
        assert_eq!(labels[6], "Copy");
        assert_eq!(labels[7], "Paste");
        assert_eq!(labels[8], "Toggle Line Comment");
        assert_eq!(labels[9], "Reformat Code");

        // Underlying EntryPoint retains full untruncated target.
        if let ContextMenuItem::Action {
            action: ContextMenuAction::RunEntryPoint(ep),
            ..
        } = &items[0]
        {
            assert_eq!(
                ep.qualified_target(),
                "com.kova.OrderSpec#should calculate total with very long Spock feature description exceeding forty four chars"
            );
        } else {
            panic!("expected RunEntryPoint");
        }

        // Inside test class outside any method.
        let class_only = build_editor_context_menu(EditorContextMenuInput {
            enclosing: EnclosingEntryPoints {
                method: None,
                class: Some(&class),
            },
            clicked_byte: 5,
            can_go_to_definition: false,
            read_only: true,
            can_paste: false,
            macos: false,
        });
        assert!(matches!(
            &class_only[0],
            ContextMenuItem::Action { label, .. } if label == "Run 'OrderSpec'"
        ));
        assert!(matches!(
            &class_only[1],
            ContextMenuItem::Action { label, .. } if label == "Debug 'OrderSpec'"
        ));
        // Read-only disables Cut, Paste, Toggle Line Comment, Reformat Code; Copy stays enabled.
        for item in &class_only {
            if let ContextMenuItem::Action { id, enabled, .. } = item {
                match *id {
                    "go-to-definition"
                    | "cut"
                    | "paste"
                    | "toggle-line-comment"
                    | "reformat-code" => assert!(!enabled, "{id} should be disabled"),
                    _ => assert!(*enabled, "{id} should be enabled"),
                }
            }
        }
    }

    #[test]
    fn project_tree_menu_distinguishes_test_and_non_test_nodes() {
        let test_items = build_project_tree_context_menu(
            PathBuf::from("src/test/groovy/com/kova/OrderSpec.groovy"),
            "OrderSpec.groovy",
            false,
            true,
            true,
            false,
        );
        assert!(matches!(
            &test_items[0],
            ContextMenuItem::Action { label, .. } if label == "Run Tests in 'OrderSpec.groovy'"
        ));
        assert!(test_items.iter().any(|item| matches!(
            item,
            ContextMenuItem::Action { label, .. } if label == "Reveal in Finder"
        )));

        let plain_items = build_project_tree_context_menu(
            PathBuf::from("README.md"),
            "README.md",
            false,
            false,
            false,
            false,
        );
        assert!(matches!(
            &plain_items[0],
            ContextMenuItem::Action { label, .. } if label == "New File..."
        ));
        assert!(plain_items.iter().any(|item| matches!(
            item,
            ContextMenuItem::Action { label, .. } if label == "Show in File Manager"
        )));
    }

    #[test]
    fn viewport_clamping_and_keyboard_navigation_work() {
        let origin = compute_context_menu_origin((1400.0, 880.0), 272.0, 260.0, (1440.0, 900.0));
        assert!((origin.0 - (1440.0 - 272.0 - 8.0)).abs() < 0.01);
        assert!((origin.1 - (900.0 - 260.0 - 8.0)).abs() < 0.01);

        let items = build_editor_context_menu(EditorContextMenuInput {
            enclosing: EnclosingEntryPoints::default(),
            clicked_byte: 0,
            can_go_to_definition: false,
            read_only: false,
            can_paste: false,
            macos: true,
        });
        let mut model = ContextMenuModel::new((100.0, 100.0), items);
        // First enabled item is `Cut` (index 2, skipping disabled `Go to Definition` and separator).
        assert_eq!(
            model.handle_key(&Keystroke::parse("down").unwrap()),
            ContextMenuKeyOutcome::Consumed
        );
        assert_eq!(model.selected_action(), Some(&ContextMenuAction::EditorCut));
        assert_eq!(
            model.handle_key(&Keystroke::parse("down").unwrap()),
            ContextMenuKeyOutcome::Consumed
        );
        assert_eq!(
            model.selected_action(),
            Some(&ContextMenuAction::EditorCopy)
        );
        assert_eq!(
            model.handle_key(&Keystroke::parse("enter").unwrap()),
            ContextMenuKeyOutcome::Activate
        );
        assert_eq!(
            model.handle_key(&Keystroke::parse("escape").unwrap()),
            ContextMenuKeyOutcome::Close
        );
    }
}
