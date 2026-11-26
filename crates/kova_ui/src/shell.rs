//! `IntelliJ` New UI *Islands* workspace shell (`WorkspaceShell`) and startup telemetry (`StartupMetrics`).
//!
//! The shell composes the headless dock models from [`crate::dock`] into the Phase 1 layout:
//! integrated 36 px title bar, left/right tool window stripes, the *Project* and *Structure*
//! docks, the multi-tab editor area with a recursive [`SplitTree`], the bottom panel
//! (*Problems* / *Run* / *Terminal*) and the 28 px status bar with the Vim mode pill.
//!
//! No filesystem scanning or parsing ever runs inside `render()`: the project tree is scanned
//! on the background executor after the first frame, files are read off-thread, and the
//! *Structure* outline / syntax problems are extracted by `kova_syntax` on the background
//! executor whenever the active document changes.

use std::collections::HashMap;
use std::fmt;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, AppContext as _, ClickEvent, Context, Div, Entity, FocusHandle, FontWeight, Hsla,
    InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement, Render, ScrollStrategy, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled, Subscription, Task, UniformListScrollHandle, Window,
    div, px, relative, uniform_list,
};
use kova_buffer::{Buffer, BufferSnapshot};
use kova_core::perf::{
    BudgetKind, budgets, check_duration_budget, check_rss_budget, current_process_rss_bytes,
};
use kova_core::project::{ProjectScanOptions, scan_project};
use kova_syntax::{
    Highlighter, LanguageRegistry, SyntaxEngine, SyntaxEngineConfig, detect_language,
};
use kova_theme::SyntaxLanguage;

use crate::dock::bottom_panel::BOTTOM_TAB_BAR_HEIGHT_PX;
use crate::dock::focus::{
    LEFT_STRIPE_WIDTH_PX, RIGHT_STRIPE_WIDTH_PX, STATUSBAR_HEIGHT_PX, TITLEBAR_HEIGHT_PX,
};
use crate::dock::project_tree::{PROJECT_INDENT_PX, PROJECT_ROW_HEIGHT_PX, lang_hint_for};
use crate::dock::structure::{STRUCTURE_ROW_HEIGHT_PX, kind_icon};
use crate::dock::{
    BadgeVariant, BottomPanelModel, BottomTab, Direction, DockLayoutState, DockToolId,
    FocusModeVariant, FocusTilingController, FocusToggle, FocusedRegion, LayoutDensity, PaneId,
    ProblemEntry, ProblemRow, ProblemSeverity, ProjectNode, ProjectNodeKind, ProjectTreeModel,
    SplitAxis, SplitTree, StatusBarState, StructureModel, TabGroupModel, TabId, TabItem,
    TreeActivation, VimModeBadge,
};
use crate::document::{
    DocumentState, SAVE_PATH_REQUIRED_MESSAGE, SaveAction, SaveSink, SaveTicket, StatusKind,
};
use crate::editor::actions::{Save, WORKSPACE_KEY_CONTEXT};
use crate::editor::syntax::{EditorSyntax, RunEntryPointRequested};
use crate::editor::vim::VimKeyResult;
use crate::editor::{EditorEvent, EditorView, SharedBuffer};
use crate::keymap::{
    CHORD_TIMEOUT, DispatchContext, DispatchOutcome, ExCommand, ExCommandLine, ExLineEvent,
    KeyDispatchLayer, KeyRoute, RouteFlags, ShellAction, is_text_composition, key_route,
    parse_ex_command, shortcut_stroke, vim_stroke,
};
use crate::settings::view::{SettingsClick, SettingsKey};
use crate::settings::{
    DebugSink, ReloadOutcome, SettingsKeyOutcome, SettingsSource, SettingsView,
    SettingsWriteRequest, SettingsWriteSink,
};
use crate::terminal::terminal_dock::TerminalDockEvent;
use crate::terminal::{
    TerminalDock, TerminalLaunch, TerminalShortcut, TerminalView, terminal_shortcut,
};
use crate::theme::{KovaTheme, hsla_from_rgba};
use kova_config::{KovaSettings, SettingsDocument};

/// Canonical Islands UI layout geometry (`KOVA-025` spec §FR-1, `new_ui_dock_mockup.svg`).
/// Outer margin, island gap and focus ring width come from [`LayoutDensity`] (`KOVA-026`);
/// the title bar, status bar and stripe sizes are shared with [`crate::dock::focus`].
const ISLAND_RADIUS_PX: f32 = 10.0;
const TAB_BAR_HEIGHT_PX: f32 = 34.0;
const STATUSBAR_RADIUS_PX: f32 = 8.0;
const STRIPE_INDICATOR_WIDTH_PX: f32 = 2.5;
/// Split ratio step of one `ResizeSplit` shortcut.
const RESIZE_SPLIT_STEP: f32 = 0.05;
const DOCK_HEADER_HEIGHT_PX: f32 = 30.0;
const FILTER_BAR_HEIGHT_PX: f32 = 26.0;
/// Debounce applied before re-running `kova_syntax` analysis after an edit. Editors keep
/// painting the previous syntax snapshot (stale fallback) until the new one arrives.
const ANALYSIS_DEBOUNCE: Duration = Duration::from_millis(60);
/// Upper bound on syntax problems collected per document.
const MAX_SYNTAX_PROBLEMS: usize = 200;

#[cfg(target_os = "macos")]
const TITLEBAR_LEFT_PADDING_PX: f32 = 78.0;
#[cfg(not(target_os = "macos"))]
const TITLEBAR_LEFT_PADDING_PX: f32 = 14.0;

/// Project-relative path of the built-in welcome document shown before a project is opened.
const WELCOME_DOCUMENT_PATH: &str = "src/main/java/dev/kova/demo/KovaApplication.java";

/// Diagnostic snapshot of cold-start latency and process memory measured after the first frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupMetrics {
    /// Elapsed time from process entry (`main`) until the first painted frame completed.
    pub cold_start_duration: Option<Duration>,
    /// Whether `cold_start_duration` satisfies `< COLD_START_MAX` (`120 ms`).
    pub cold_start_within_budget: Option<bool>,
    /// Resident Set Size (RSS) of the process sampled after the first frame, in bytes.
    pub rss_bytes: Option<u64>,
    /// Whether `rss_bytes` satisfies `< MAX_PROCESS_RSS_BYTES` (`150 MiB`).
    pub rss_within_budget: Option<bool>,
    /// Active GPU presentation backend name (`"Metal"` on macOS, `"Vulkan"` on Linux).
    pub backend_name: &'static str,
}

impl Default for StartupMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl StartupMetrics {
    /// Creates an unmeasured [`StartupMetrics`] record for the active platform GPU backend.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            cold_start_duration: None,
            cold_start_within_budget: None,
            rss_bytes: None,
            rss_within_budget: None,
            backend_name: Self::active_backend_name(),
        }
    }

    /// Returns the expected GPU backend label for the compiled target OS.
    #[must_use]
    pub const fn active_backend_name() -> &'static str {
        if cfg!(target_os = "macos") {
            "Metal"
        } else if cfg!(target_os = "linux") {
            "Vulkan"
        } else {
            "GPU"
        }
    }

    /// Records first-frame elapsed duration and optional RSS reading against Kova budgets.
    pub fn record_first_frame(&mut self, elapsed: Duration, rss_bytes: Option<u64>) {
        let cold_report =
            check_duration_budget(BudgetKind::ColdStart, elapsed, budgets::COLD_START_MAX);
        self.cold_start_duration = Some(elapsed);
        self.cold_start_within_budget = Some(cold_report.is_within_budget());

        self.rss_bytes = rss_bytes;
        self.rss_within_budget = rss_bytes.map(|bytes| check_rss_budget(bytes).is_within_budget());
    }

    /// Returns `true` when both cold-start latency and RSS memory were measured within budget.
    #[must_use]
    pub const fn is_within_budgets(&self) -> bool {
        matches!(
            (self.cold_start_within_budget, self.rss_within_budget),
            (Some(true), Some(true))
        )
    }

    /// Formats the cold-start duration label for the status bar (e.g., `"Start: 48 ms"`).
    #[must_use]
    pub fn format_start_label(&self) -> String {
        match self.cold_start_duration {
            Some(duration) => format!("Start: {} ms", duration.as_millis()),
            None => "Start: -- ms".to_owned(),
        }
    }

    /// Formats the process RSS footprint in MiB for the status bar (e.g., `"42.1 MiB"`).
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        reason = "process RSS in bytes fits accurately in f64 for MiB status reporting"
    )]
    pub fn format_rss_label(&self) -> String {
        match self.rss_bytes {
            Some(bytes) => {
                let mib = bytes as f64 / (1024.0 * 1024.0);
                format!("{mib:.1} MiB")
            }
            None => "-- MiB".to_owned(),
        }
    }

    /// Formats a compact summary combining start time, RSS footprint, and GPU backend.
    #[must_use]
    pub fn format_summary(&self) -> String {
        format!(
            "{} | {} | {}",
            self.format_start_label(),
            self.format_rss_label(),
            self.backend_name
        )
    }

    /// Formats a structured single-line probe report for automated cold-start benchmarks.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        reason = "sub-second cold-start microseconds convert cleanly to f64 milliseconds"
    )]
    pub fn format_probe_line(&self) -> String {
        let cold_start_ms = self.cold_start_duration.map_or_else(
            || "na".to_owned(),
            |d| format!("{:.2}", d.as_secs_f64() * 1000.0),
        );
        let rss_bytes = self
            .rss_bytes
            .map_or_else(|| "na".to_owned(), |bytes| bytes.to_string());
        format!(
            "cold_start_ms={cold_start_ms} rss_bytes={rss_bytes} backend={} within_budget={}",
            self.backend_name,
            self.is_within_budgets()
        )
    }
}

type FirstFrameCallback = Box<dyn FnOnce(&StartupMetrics)>;

/// Resolved Hsla colours for one frame (copied into row closures without borrowing the theme).
#[derive(Debug, Clone, Copy)]
struct Palette {
    island: Hsla,
    hover: Hsla,
    active: Hsla,
    tab_active: Hsla,
    input: Hsla,
    border_island: Hsla,
    border_subtle: Hsla,
    border_focus: Hsla,
    tab_indicator: Hsla,
    text_primary: Hsla,
    text_secondary: Hsla,
    text_muted: Hsla,
    run_icon: Hsla,
    warning: Hsla,
    error: Hsla,
}

impl Palette {
    fn from_theme(theme: &KovaTheme) -> Self {
        Self {
            island: hsla_from_rgba(theme.surfaces().island),
            hover: hsla_from_rgba(theme.surfaces().hover),
            active: hsla_from_rgba(theme.surfaces().active),
            tab_active: hsla_from_rgba(theme.surfaces().tab_active),
            input: hsla_from_rgba(theme.surfaces().input),
            border_island: hsla_from_rgba(theme.borders().island),
            border_subtle: hsla_from_rgba(theme.borders().subtle),
            border_focus: hsla_from_rgba(theme.borders().focus),
            tab_indicator: hsla_from_rgba(theme.borders().tab_indicator),
            text_primary: hsla_from_rgba(theme.text().primary),
            text_secondary: hsla_from_rgba(theme.text().secondary),
            text_muted: hsla_from_rgba(theme.text().muted),
            run_icon: hsla_from_rgba(theme.gutter().run_icon),
            warning: hsla_from_rgba(theme.editor().diagnostic_warning),
            error: hsla_from_rgba(theme.editor().diagnostic_error),
        }
    }
}

fn rgb_hex(hex: u32) -> Hsla {
    let [_, r, g, b] = hex.to_be_bytes();
    gpui::Rgba {
        r: f32::from(r) / 255.0,
        g: f32::from(g) / 255.0,
        b: f32::from(b) / 255.0,
        a: 1.0,
    }
    .into()
}

/// One editor split pane: its tab group plus one [`EditorView`] per open tab.
struct PaneState {
    tabs: TabGroupModel,
    editors: HashMap<TabId, Entity<EditorView>>,
    subscriptions: HashMap<TabId, Subscription>,
}

impl PaneState {
    fn new(id: PaneId) -> Self {
        Self {
            tabs: TabGroupModel::new(id),
            editors: HashMap::new(),
            subscriptions: HashMap::new(),
        }
    }

    fn active_editor(&self) -> Option<&Entity<EditorView>> {
        self.tabs
            .active_tab_id()
            .and_then(|tab| self.editors.get(&tab))
    }
}

/// Resizable island targeted by an in-progress splitter drag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DragTarget {
    LeftDock,
    RightDock,
    BottomPanel,
}

#[derive(Debug, Clone, Copy)]
struct DragState {
    target: DragTarget,
    start_mouse: f32,
    start_value: f32,
}

/// Output of one background `kova_syntax` analysis pass.
struct AnalysisResult {
    engine: Option<Arc<SyntaxEngine>>,
    structure: StructureModel,
    problems: Vec<ProblemEntry>,
    /// Highlight / fold / entry-point snapshot pushed to every editor showing the document.
    syntax: Option<Arc<EditorSyntax>>,
}

/// Root GPUI view rendering the *Kova Islands* `IntelliJ` New UI workspace.
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent startup, loading and Vim flags of the root view"
)]
pub struct WorkspaceShell {
    process_start: Instant,
    metrics: StartupMetrics,
    first_frame_scheduled: bool,
    exit_after_first_frame: bool,
    on_first_frame: Option<FirstFrameCallback>,
    project_root: Option<PathBuf>,
    project_name: String,
    project_loading: bool,
    dock_layout: DockLayoutState,
    project_tree: ProjectTreeModel,
    project_scroll: UniformListScrollHandle,
    structure: StructureModel,
    structure_scroll: UniformListScrollHandle,
    bottom_panel: BottomPanelModel,
    problems_scroll: UniformListScrollHandle,
    split_tree: SplitTree,
    panes: HashMap<PaneId, PaneState>,
    active_pane: PaneId,
    documents: HashMap<PathBuf, SharedBuffer>,
    /// Persistence state per open document, keyed like [`Self::documents`] (`KOVA-016`).
    doc_states: HashMap<PathBuf, DocumentState>,
    save_sink: Option<SaveSink>,
    /// Document-level message (open failure, missing save path); cleared by the next save.
    doc_notice: Option<(StatusKind, String)>,
    /// Configuration warning (e.g. invalid `settings.toml`); cleared by the next good reload.
    config_notice: Option<(StatusKind, String)>,
    window_title: String,
    status_bar: StatusBarState,
    focus_handle: FocusHandle,
    drag: Option<DragState>,
    syntax_engine: Option<Arc<SyntaxEngine>>,
    analysis_task: Option<Task<()>>,
    project_task: Option<Task<()>>,
    /// Focus Mode snapshot and Islands / Tiling density (`KOVA-026`).
    focus_tiling: FocusTilingController,
    /// `JetBrains` keymap bridge with the chord session (`KOVA-026`).
    keymap: KeyDispatchLayer,
    /// Status bar hint of a pending chord prefix (`ctrl+w …`).
    chord_hint: Option<String>,
    /// Open Vim Ex / search command line.
    ex_line: Option<ExCommandLine>,
    /// Whether modal Vim editing is enabled for every editor.
    vim_enabled: bool,
    /// Last rendered window size in logical pixels (for directional focus navigation).
    viewport_px: (f32, f32),
    /// Document whose tabs `:wq` closes once its save succeeds.
    close_after_save: Option<PathBuf>,
    /// Settings overlay state, created lazily on the first `Cmd+,` (`KOVA-029`).
    settings_view: Option<SettingsView>,
    /// Whether the Settings overlay is visible.
    settings_open: bool,
    /// Latest `settings.toml` snapshot delivered by the host watcher.
    settings_source: Option<SettingsSource>,
    /// Host writer persisting `settings.toml` off the UI thread.
    settings_sink: Option<DebugSink>,
    /// Settings currently applied to the workspace (diffed on every change).
    applied_settings: KovaSettings,
    /// `ui.show_status_bar`.
    show_status_bar: bool,
    /// Last `settings.toml` reload error, kept for a Settings view opened later.
    settings_error: Option<String>,
    /// Integrated PTY terminal, created on the first `Alt+F12` (`KOVA-039`).
    terminal: Option<(Entity<TerminalDock>, Subscription)>,
}

impl fmt::Debug for WorkspaceShell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkspaceShell")
            .field("metrics", &self.metrics)
            .field("first_frame_scheduled", &self.first_frame_scheduled)
            .field("exit_after_first_frame", &self.exit_after_first_frame)
            .field("has_on_first_frame", &self.on_first_frame.is_some())
            .field("project_root", &self.project_root)
            .field("dock_layout", &self.dock_layout)
            .field("split_tree", &self.split_tree)
            .field("active_pane", &self.active_pane)
            .finish_non_exhaustive()
    }
}

impl WorkspaceShell {
    /// Creates a new [`WorkspaceShell`] anchored to `process_start` for cold-start telemetry.
    #[must_use]
    pub fn new(process_start: Instant, cx: &mut Context<'_, Self>) -> Self {
        Self::with_probe(process_start, false, None, cx)
    }

    /// Creates a [`WorkspaceShell`] with optional automated probe exit and completion hook.
    ///
    /// The constructor only builds in-memory state (welcome document and demo project tree);
    /// a real project directory set via [`Self::with_project_root`] is scanned off-thread after
    /// the first frame so the `< 120 ms` cold-start budget is never blocked by disk I/O.
    #[must_use]
    pub fn with_probe(
        process_start: Instant,
        exit_after_first_frame: bool,
        on_first_frame: Option<FirstFrameCallback>,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let root_pane = PaneId(1);
        let welcome = cx.new(EditorView::welcome_demo);
        let welcome_buffer = welcome.read(cx).shared_buffer();
        let welcome_path = PathBuf::from(WELCOME_DOCUMENT_PATH);

        let mut pane = PaneState::new(root_pane);
        let tab = pane.tabs.open_or_focus(
            Some(&welcome_path),
            file_title(&welcome_path),
            language_label(&welcome_path),
        );
        let subscription = Self::subscribe_editor(&welcome, root_pane, tab, cx);
        pane.editors.insert(tab, welcome);
        pane.subscriptions.insert(tab, subscription);

        let mut panes = HashMap::new();
        panes.insert(root_pane, pane);
        let mut doc_states = HashMap::new();
        // The welcome buffer has no file on disk: `Cmd+S` asks for a path (*Save As*).
        doc_states.insert(
            welcome_path.clone(),
            DocumentState::opened(None, welcome_buffer.version().as_u64(), false),
        );
        let mut documents = HashMap::new();
        documents.insert(welcome_path, welcome_buffer);

        let mut project_tree = ProjectTreeModel::from_nodes(demo_project_nodes());
        project_tree.reveal_path(Path::new(WELCOME_DOCUMENT_PATH));

        Self {
            process_start,
            metrics: StartupMetrics::new(),
            first_frame_scheduled: false,
            exit_after_first_frame,
            on_first_frame,
            project_root: None,
            project_name: "kova-orders".to_owned(),
            project_loading: false,
            dock_layout: DockLayoutState::default(),
            project_tree,
            project_scroll: UniformListScrollHandle::new(),
            structure: StructureModel::default(),
            structure_scroll: UniformListScrollHandle::new(),
            bottom_panel: BottomPanelModel::default(),
            problems_scroll: UniformListScrollHandle::new(),
            split_tree: SplitTree::new(root_pane),
            panes,
            active_pane: root_pane,
            documents,
            doc_states,
            save_sink: None,
            doc_notice: None,
            config_notice: None,
            window_title: String::new(),
            status_bar: StatusBarState {
                vim_mode: VimModeBadge::Off,
                ..StatusBarState::default()
            },
            focus_handle: cx.focus_handle(),
            drag: None,
            syntax_engine: None,
            analysis_task: None,
            project_task: None,
            focus_tiling: FocusTilingController::new(),
            keymap: KeyDispatchLayer::new(kova_core::Platform::current()),
            chord_hint: None,
            ex_line: None,
            vim_enabled: false,
            viewport_px: (1440.0, 900.0),
            close_after_save: None,
            settings_view: None,
            settings_open: false,
            settings_source: None,
            settings_sink: None,
            applied_settings: KovaSettings::default(),
            show_status_bar: true,
            settings_error: None,
            terminal: None,
        }
    }

    /// Sets the project directory whose tree is loaded asynchronously after the first frame.
    #[must_use]
    pub fn with_project_root(mut self, root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        if let Some(name) = root.file_name().and_then(|n| n.to_str()) {
            name.clone_into(&mut self.project_name);
        }
        self.project_root = Some(root);
        self
    }

    /// Returns the [`EditorView`] of the active tab in the active split pane, if any.
    #[must_use]
    pub fn active_editor(&self) -> Option<&Entity<EditorView>> {
        self.panes
            .get(&self.active_pane)
            .and_then(PaneState::active_editor)
    }

    /// Returns the current [`StartupMetrics`] captured by this shell.
    #[must_use]
    pub const fn metrics(&self) -> &StartupMetrics {
        &self.metrics
    }

    /// Returns a reference to the [`DockLayoutState`].
    #[must_use]
    pub const fn dock_layout(&self) -> &DockLayoutState {
        &self.dock_layout
    }

    /// Returns a mutable reference to the [`DockLayoutState`].
    pub const fn dock_layout_mut(&mut self) -> &mut DockLayoutState {
        &mut self.dock_layout
    }

    /// Returns the [`TabGroupModel`] of the active split pane.
    #[must_use]
    pub fn active_tabs(&self) -> Option<&TabGroupModel> {
        self.panes.get(&self.active_pane).map(|pane| &pane.tabs)
    }

    /// Returns a reference to the editor [`SplitTree`].
    #[must_use]
    pub const fn split_tree(&self) -> &SplitTree {
        &self.split_tree
    }

    /// Returns the id of the active split pane.
    #[must_use]
    pub const fn active_pane(&self) -> PaneId {
        self.active_pane
    }

    /// Returns a reference to the [`StatusBarState`].
    #[must_use]
    pub const fn status_bar(&self) -> &StatusBarState {
        &self.status_bar
    }

    /// Feeds the modal Vim state from `kova_core` (`None` = Vim disabled) into the status bar.
    pub fn set_vim_status(
        &mut self,
        status: Option<&kova_core::vim::VimStatus>,
        cx: &mut Context<'_, Self>,
    ) {
        self.status_bar.vim_mode = VimModeBadge::from_status(status);
        self.status_bar.vim_sequence = status.map(|s| s.pending.clone()).unwrap_or_default();
        cx.notify();
    }

    fn complete_first_frame(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.metrics.cold_start_duration.is_some() {
            return;
        }

        let elapsed = self.process_start.elapsed();
        // Sample RSS only after the first frame is submitted so memory probing stays off the critical path.
        let rss_bytes = current_process_rss_bytes().ok();
        self.metrics.record_first_frame(elapsed, rss_bytes);

        if let Some(callback) = self.on_first_frame.take() {
            callback(&self.metrics);
        }

        if self.exit_after_first_frame {
            // Defer quitting until after the current frame cycle finishes cleanly.
            window.defer(cx, |_, cx| {
                cx.quit();
            });
            return;
        }

        if let Some(editor) = self.active_editor() {
            window.focus(editor.read(cx).focus_handle());
        }
        self.load_project(cx);
        self.refresh_analysis(Duration::ZERO, cx);
        cx.notify();
    }

    // ---------------------------------------------------------------------------------------
    // Async project loading and kova_syntax analysis
    // ---------------------------------------------------------------------------------------

    fn load_project(&mut self, cx: &mut Context<'_, Self>) {
        let Some(root) = self.project_root.clone() else {
            return;
        };
        self.project_loading = true;
        let scan = cx.background_executor().spawn(async move {
            let tree = scan_project(&root, &ProjectScanOptions::default())
                .map(|tree| ProjectTreeModel::from_project_tree(&tree, 1));
            let branch = std::fs::read_to_string(root.join(".git").join("HEAD"))
                .ok()
                .and_then(|head| parse_git_head(&head));
            (tree, branch)
        });
        self.project_task = Some(cx.spawn(async move |this, cx| {
            let (tree, branch) = scan.await;
            this.update(cx, |this, cx| {
                this.project_loading = false;
                match tree {
                    Ok(tree) => this.project_tree = tree,
                    Err(err) => this
                        .bottom_panel
                        .run_logs
                        .push(format!("Project scan failed: {err}")),
                }
                if let Some(branch) = branch {
                    this.status_bar.git_branch = branch;
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn active_document(&self) -> Option<(PathBuf, BufferSnapshot)> {
        let pane = self.panes.get(&self.active_pane)?;
        let path = pane.tabs.active_tab()?.path.clone()?;
        let buffer = self.documents.get(&path)?;
        Some((path, buffer.snapshot()))
    }

    fn refresh_analysis(&mut self, debounce: Duration, cx: &mut Context<'_, Self>) {
        let Some((path, snapshot)) = self.active_document() else {
            self.structure = StructureModel::default();
            self.bottom_panel.set_problems(Vec::new());
            self.analysis_task = None;
            return;
        };
        let document = self.documents.get(&path).cloned();
        let Some(language) = detect_language(&path) else {
            self.structure = StructureModel::default();
            self.bottom_panel.set_problems(Vec::new());
            self.analysis_task = None;
            if let Some(document) = document.as_ref() {
                self.push_editor_syntax(document, None, cx);
            }
            return;
        };
        let engine = self.syntax_engine.clone();
        let executor = cx.background_executor().clone();
        let file_label = path.to_string_lossy().into_owned();
        self.analysis_task = Some(cx.spawn(async move |this, cx| {
            if !debounce.is_zero() {
                executor.timer(debounce).await;
            }
            let result = executor
                .spawn(async move { analyze_document(engine, language, &snapshot, &file_label) })
                .await;
            this.update(cx, |this, cx| {
                if result.engine.is_some() {
                    this.syntax_engine = result.engine;
                }
                this.structure = result.structure;
                this.bottom_panel.set_problems(result.problems);
                if let (Some(document), Some(syntax)) = (document.as_ref(), result.syntax) {
                    this.push_editor_syntax(document, Some(&syntax), cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Delivers a syntax snapshot to every editor (in any split) showing `document`.
    fn push_editor_syntax(
        &self,
        document: &SharedBuffer,
        syntax: Option<&Arc<EditorSyntax>>,
        cx: &mut Context<'_, Self>,
    ) {
        for state in self.panes.values() {
            for editor in state.editors.values() {
                if editor.read(cx).shared_buffer().same_document(document) {
                    let syntax = syntax.cloned();
                    editor.update(cx, |editor, cx| editor.set_syntax(syntax, cx));
                }
            }
        }
    }

    // ---------------------------------------------------------------------------------------
    // Documents, tabs and splits
    // ---------------------------------------------------------------------------------------

    fn subscribe_editor(
        editor: &Entity<EditorView>,
        pane: PaneId,
        tab: TabId,
        cx: &mut Context<'_, Self>,
    ) -> Subscription {
        cx.subscribe(editor, move |this, editor, event: &EditorEvent, cx| {
            this.on_editor_event(pane, tab, &editor, event, cx);
        })
    }

    fn on_editor_event(
        &mut self,
        pane: PaneId,
        tab: TabId,
        editor: &Entity<EditorView>,
        event: &EditorEvent,
        cx: &mut Context<'_, Self>,
    ) {
        match event {
            EditorEvent::Focused => {
                if let Some(state) = self.panes.get_mut(&pane) {
                    state.tabs.select_tab(tab);
                }
                self.set_active_pane(pane, cx);
                self.dock_layout.focus(FocusedRegion::EditorPane(pane));
                cx.notify();
            }
            EditorEvent::Edited => {
                let buffer = editor.read(cx).shared_buffer();
                let mut dirty_path = None;
                for state in self.panes.values_mut() {
                    for (tab_id, other) in &state.editors {
                        if other.read(cx).shared_buffer().same_document(&buffer) {
                            if other != editor {
                                // Sibling split sharing the buffer: repaint with the new revision.
                                other.update(cx, |_, cx| cx.notify());
                            }
                            dirty_path = dirty_path.or_else(|| {
                                state
                                    .tabs
                                    .tabs()
                                    .iter()
                                    .find(|t| t.id == *tab_id)
                                    .and_then(|t| t.path.clone())
                            });
                        }
                    }
                }
                if let Some(path) = dirty_path {
                    self.sync_dirty_flag(&path);
                }
                self.refresh_analysis(ANALYSIS_DEBOUNCE, cx);
                cx.notify();
            }
            EditorEvent::RunEntryPoint(entry) => {
                let path = self
                    .panes
                    .get(&pane)
                    .and_then(|state| state.tabs.tabs().iter().find(|t| t.id == tab))
                    .and_then(|t| t.path.clone());
                let request = RunEntryPointRequested::from_entry_point(entry, path);
                self.handle_run_request(&request);
                cx.notify();
            }
        }
    }

    /// Phase 1 handling of a gutter `▶` click: opens the *Run* tab and logs the target.
    /// Process execution attaches in Phase 2/3 (`kova_toolchain` / `kova_tools`).
    fn handle_run_request(&mut self, request: &RunEntryPointRequested) {
        self.bottom_panel.run_logs.push(request.log_line());
        self.dock_layout.bottom_open = Some(DockToolId::Run);
    }

    fn set_active_pane(&mut self, pane: PaneId, cx: &mut Context<'_, Self>) {
        if !self.panes.contains_key(&pane) {
            return;
        }
        let changed = self.active_pane != pane;
        self.active_pane = pane;
        for (id, state) in &self.panes {
            let is_active = *id == pane;
            for editor in state.editors.values() {
                editor.update(cx, |editor, cx| editor.set_active_pane(is_active, cx));
            }
        }
        if changed {
            self.refresh_analysis(Duration::ZERO, cx);
        }
    }

    fn focus_active_editor(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.dock_layout
            .focus(FocusedRegion::EditorPane(self.active_pane));
        if let Some(editor) = self.active_editor() {
            window.focus(editor.read(cx).focus_handle());
        }
        cx.notify();
    }

    fn focus_dock(
        &mut self,
        region: FocusedRegion,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.dock_layout.focus(region);
        if region == FocusedRegion::BottomPanel
            && self.dock_layout.bottom_open == Some(DockToolId::Terminal)
        {
            let dock = self.ensure_terminal(window, cx);
            dock.read(cx).focus_active(window, cx);
        } else {
            window.focus(&self.focus_handle);
        }
        cx.notify();
    }

    /// Returns the terminal dock, starting its first session on first use (`KOVA-039`).
    ///
    /// The PTY is never started at construction, so cold start and headless tests stay
    /// free of child processes.
    fn ensure_terminal(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Entity<TerminalDock> {
        if let Some((dock, _)) = &self.terminal {
            return dock.clone();
        }
        let launch =
            TerminalLaunch::from_settings(self.project_root.clone(), &self.applied_settings);
        let dock = cx.new(|cx| TerminalDock::new(launch, cx));
        let subscription =
            cx.subscribe_in(
                &dock,
                window,
                |this, _dock, event, window, cx| match event {
                    TerminalDockEvent::AllClosed => this.close_terminal(window, cx),
                },
            );
        self.terminal = Some((dock.clone(), subscription));
        dock
    }

    /// Hides the terminal after its last tab closed and drops the dock.
    fn close_terminal(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.terminal = None;
        if self.dock_layout.bottom_open == Some(DockToolId::Terminal) {
            self.dock_layout
                .toggle_tool(DockToolId::Terminal, self.active_pane);
        }
        self.focus_active_editor(window, cx);
        cx.notify();
    }

    /// Whether keyboard focus is inside the terminal dock.
    fn terminal_focused(
        &self,
        window: &Window,
        cx: &Context<'_, Self>,
    ) -> Option<Entity<TerminalDock>> {
        self.terminal
            .as_ref()
            .map(|(dock, _)| dock)
            .filter(|dock| dock.read(cx).contains_focus(window, cx))
            .cloned()
    }

    fn run_terminal_shortcut(
        &mut self,
        shortcut: TerminalShortcut,
        dock: &Entity<TerminalDock>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match shortcut {
            TerminalShortcut::Hide => self.toggle_tool(DockToolId::Terminal, window, cx),
            TerminalShortcut::NewTab => dock.update(cx, |dock, cx| dock.new_tab(window, cx)),
            TerminalShortcut::CloseTab => dock.update(cx, |dock, cx| dock.close_active(window, cx)),
            TerminalShortcut::Clear => {
                dock.update(cx, |dock, cx| dock.update_active(cx, TerminalView::clear));
            }
            TerminalShortcut::Copy => {
                dock.update(cx, |dock, cx| dock.update_active(cx, TerminalView::copy));
            }
            TerminalShortcut::Paste => {
                dock.update(cx, |dock, cx| dock.update_active(cx, TerminalView::paste));
            }
        }
        cx.notify();
    }

    /// Opens `rel_path` in the active pane (or focuses its existing tab) and optionally moves
    /// the cursor to the 1-based `(line, col)`.
    ///
    /// Documents already open in another split reuse the same [`SharedBuffer`]; new files are
    /// read on the background executor so the UI thread never blocks on disk I/O.
    pub fn open_path(
        &mut self,
        rel_path: &Path,
        position: Option<(usize, usize)>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(buffer) = self.documents.get(rel_path).cloned() {
            self.show_document(rel_path, buffer, position, window, cx);
            return;
        }
        let rel = rel_path.to_path_buf();
        let Some(root) = self.project_root.clone() else {
            // Demo project: synthesize a small in-memory document for the selected node.
            let text = format!(
                "// {}\n// Demo project — run `kova <project-dir>` to open a real workspace.\n",
                rel.display()
            );
            let buffer = SharedBuffer::new(Buffer::from_text(&text));
            self.doc_states.insert(
                rel.clone(),
                DocumentState::opened(None, buffer.version().as_u64(), false),
            );
            self.documents.insert(rel.clone(), buffer.clone());
            self.show_document(&rel, buffer, position, window, cx);
            return;
        };
        let abs = root.join(&rel);
        let read = cx.background_executor().spawn({
            let abs = abs.clone();
            async move { std::fs::read_to_string(abs) }
        });
        cx.spawn_in(window, async move |this, cx| {
            let text = read.await;
            this.update_in(cx, |this, window, cx| match text {
                Ok(text) => {
                    let buffer = this
                        .documents
                        .entry(rel.clone())
                        .or_insert_with(|| SharedBuffer::new(Buffer::from_text(&text)))
                        .clone();
                    let revision = buffer.version().as_u64();
                    this.doc_states
                        .entry(rel.clone())
                        .or_insert_with(|| DocumentState::opened(Some(abs), revision, false));
                    this.show_document(&rel, buffer, position, window, cx);
                }
                Err(err) => {
                    this.bottom_panel
                        .run_logs
                        .push(format!("Cannot open {}: {err}", rel.display()));
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn show_document(
        &mut self,
        rel_path: &Path,
        buffer: SharedBuffer,
        position: Option<(usize, usize)>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let pane_id = self.active_pane;
        let existing = self
            .panes
            .get(&pane_id)
            .and_then(|pane| pane.tabs.find_by_path(rel_path))
            .map(|tab| tab.id);
        let tab = if let Some(tab) = existing {
            if let Some(pane) = self.panes.get_mut(&pane_id) {
                pane.tabs.select_tab(tab);
            }
            tab
        } else {
            let vim_enabled = self.vim_enabled;
            let (font_family, font_size) = editor_font(&self.applied_settings);
            let editor = cx.new(|cx| {
                let mut editor = EditorView::with_shared_buffer(buffer, cx);
                editor.set_vim_enabled(vim_enabled, cx);
                editor.set_font(&font_family, font_size, cx);
                editor
            });
            let Some(pane) = self.panes.get_mut(&pane_id) else {
                return;
            };
            let tab = pane.tabs.open_or_focus(
                Some(rel_path),
                file_title(rel_path),
                language_label(rel_path),
            );
            pane.editors.insert(tab, editor.clone());
            let subscription = Self::subscribe_editor(&editor, pane_id, tab, cx);
            if let Some(pane) = self.panes.get_mut(&pane_id) {
                pane.subscriptions.insert(tab, subscription);
            }
            tab
        };

        if let Some(editor) = self
            .panes
            .get(&pane_id)
            .and_then(|pane| pane.editors.get(&tab))
            .cloned()
        {
            if let Some((line, col)) = position {
                editor.update(cx, |editor, cx| editor.set_cursor_position(line, col, cx));
            }
            window.focus(editor.read(cx).focus_handle());
        }
        self.project_tree.reveal_path(rel_path);
        self.set_active_pane(pane_id, cx);
        self.dock_layout.focus(FocusedRegion::EditorPane(pane_id));
        self.refresh_analysis(Duration::ZERO, cx);
        cx.notify();
    }

    fn select_tab(
        &mut self,
        pane: PaneId,
        tab: TabId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(state) = self.panes.get_mut(&pane) {
            state.tabs.select_tab(tab);
        }
        self.set_active_pane(pane, cx);
        self.refresh_analysis(Duration::ZERO, cx);
        self.focus_active_editor(window, cx);
    }

    fn close_tab(
        &mut self,
        pane: PaneId,
        tab: TabId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(state) = self.panes.get_mut(&pane) else {
            return;
        };
        state.tabs.close_tab(tab);
        state.editors.remove(&tab);
        state.subscriptions.remove(&tab);
        let now_empty = state.tabs.tab_count() == 0;

        if now_empty && self.split_tree.close(pane) {
            self.panes.remove(&pane);
            let next = self.split_tree.first_pane();
            self.set_active_pane(next, cx);
        } else {
            self.set_active_pane(pane, cx);
        }
        self.drop_unused_documents();
        self.refresh_analysis(Duration::ZERO, cx);
        self.focus_active_editor(window, cx);
    }

    fn drop_unused_documents(&mut self) {
        let panes = &self.panes;
        self.documents.retain(|path, _| {
            panes
                .values()
                .any(|pane| pane.tabs.find_by_path(path).is_some())
        });
        let documents = &self.documents;
        self.doc_states
            .retain(|path, _| documents.contains_key(path));
    }

    // ---------------------------------------------------------------------------------------
    // Document persistence: CLI open, `Cmd+S` / `Ctrl+S` saves, status notices (`KOVA-016`)
    // ---------------------------------------------------------------------------------------

    /// Installs the callback that persists [`crate::SaveRequest`]s off the UI thread.
    pub fn set_save_sink(&mut self, sink: SaveSink) {
        self.save_sink = Some(sink);
    }

    /// Returns the persistence state of the document shown in the active tab.
    #[must_use]
    pub fn active_document_state(&self) -> Option<&DocumentState> {
        let key = self.active_document_key()?;
        self.doc_states.get(&key)
    }

    /// Returns `true` when the document in the active tab has unsaved changes.
    #[must_use]
    pub fn is_active_document_dirty(&self) -> bool {
        self.active_document_key()
            .is_some_and(|key| self.is_document_dirty(&key))
    }

    fn active_document_key(&self) -> Option<PathBuf> {
        self.panes
            .get(&self.active_pane)?
            .tabs
            .active_tab()?
            .path
            .clone()
    }

    fn is_document_dirty(&self, key: &Path) -> bool {
        match (self.doc_states.get(key), self.documents.get(key)) {
            (Some(doc), Some(buffer)) => doc.is_dirty(buffer.version().as_u64()),
            _ => false,
        }
    }

    /// Recomputes the tab dirty marker of `key` in every pane from its saved revision.
    fn sync_dirty_flag(&mut self, key: &Path) {
        let dirty = match (self.doc_states.get(key), self.documents.get(key)) {
            (Some(doc), Some(buffer)) => doc.is_dirty(buffer.version().as_u64()),
            _ => true,
        };
        for state in self.panes.values_mut() {
            if let Some(id) = state.tabs.find_by_path(key).map(|t| t.id) {
                state.tabs.set_dirty(id, dirty);
            }
        }
    }

    /// Shows a document passed on the command line: loaded from disk (`buffer = Some`) or a
    /// new, not yet existing file (`buffer = None`) at `path`, then jumps to the optional
    /// 1-based `line` / `column` with the line centered in the viewport.
    pub fn open_document(
        &mut self,
        path: PathBuf,
        buffer: Option<Buffer>,
        line: Option<usize>,
        column: Option<usize>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let key = self
            .project_root
            .as_deref()
            .and_then(|root| path.strip_prefix(root).ok())
            .map_or_else(|| path.clone(), Path::to_path_buf);
        let shared = if let Some(existing) = self.documents.get(&key) {
            existing.clone()
        } else {
            let is_new_file = buffer.is_none();
            let shared = SharedBuffer::new(buffer.unwrap_or_default());
            self.doc_states.insert(
                key.clone(),
                DocumentState::opened(Some(path), shared.version().as_u64(), is_new_file),
            );
            self.documents.insert(key.clone(), shared.clone());
            shared
        };
        self.show_document(&key, shared, None, window, cx);
        if let Some(line) = line
            && let Some(editor) = self.active_editor().cloned()
        {
            editor.update(cx, |editor, cx| editor.go_to_line_column(line, column, cx));
        }
        cx.notify();
    }

    /// Reports a document that could not be opened (directory, non-UTF-8, I/O error); no tab
    /// is created, so the unreadable file can never be overwritten by an accidental save.
    pub fn report_open_error(&mut self, message: String, cx: &mut Context<'_, Self>) {
        self.bottom_panel.run_logs.push(message.clone());
        self.doc_notice = Some((StatusKind::Error, message));
        cx.notify();
    }

    /// Shows a configuration notice in the status bar (e.g. an invalid `settings.toml`).
    pub fn set_notice(&mut self, kind: StatusKind, message: String, cx: &mut Context<'_, Self>) {
        self.config_notice = Some((kind, message));
        cx.notify();
    }

    /// Clears the configuration notice set by [`Self::set_notice`].
    pub fn clear_notice(&mut self, cx: &mut Context<'_, Self>) {
        if self.config_notice.take().is_some() {
            cx.notify();
        }
    }

    /// Applies the result of a background save started by `Cmd+S` / `Ctrl+S`.
    ///
    /// The ticket identifies the document that issued the save, whichever tab is active now;
    /// results for documents closed in the meantime are ignored.
    pub fn finish_save(
        &mut self,
        ticket: SaveTicket,
        result: Result<(), String>,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(key) = self
            .doc_states
            .iter()
            .find(|(_, doc)| doc.generation() == ticket.generation)
            .map(|(key, _)| key.clone())
        else {
            return;
        };
        let saved = result.is_ok();
        let resave = self
            .doc_states
            .get_mut(&key)
            .is_some_and(|doc| doc.finish_save(ticket, result));
        if resave {
            // Edits happened while the previous save ran and another save was requested.
            self.dispatch_save(&key, cx);
        }
        self.sync_dirty_flag(&key);
        if self.close_after_save.as_deref() == Some(key.as_path()) && !resave {
            self.close_after_save = None;
            if saved {
                self.close_document_tabs(&key, cx);
            }
        }
        cx.notify();
    }

    /// Closes every tab showing `key` (the `:wq` close that waits for the save to succeed).
    fn close_document_tabs(&mut self, key: &Path, cx: &mut Context<'_, Self>) {
        let tabs: Vec<(PaneId, TabId)> = self
            .panes
            .iter()
            .filter_map(|(pane, state)| state.tabs.find_by_path(key).map(|tab| (*pane, tab.id)))
            .collect();
        for (pane, tab) in tabs {
            self.detach_tab(pane, tab);
        }
        if !self.split_tree.contains(self.active_pane) {
            let first = self.split_tree.first_pane();
            self.set_active_pane(first, cx);
        }
        self.dock_layout
            .focus(FocusedRegion::EditorPane(self.active_pane));
        self.drop_unused_documents();
        self.refresh_analysis(Duration::ZERO, cx);
    }

    fn save_action(&mut self, _: &Save, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.save_active(window, cx);
    }

    /// Saves the active document (`Cmd+S`, `:w`), asking for a path when it has none.
    fn save_active(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(key) = self.active_document_key() else {
            return;
        };
        let has_path = self
            .doc_states
            .get(&key)
            .is_some_and(|doc| doc.path.is_some());
        if has_path {
            self.dispatch_save(&key, cx);
        } else {
            Self::prompt_save_path(key, window, cx);
        }
    }

    /// Hands the current snapshot of `key` to the save sink without touching the disk.
    fn dispatch_save(&mut self, key: &Path, cx: &mut Context<'_, Self>) {
        let Some(snapshot) = self.documents.get(key).map(SharedBuffer::snapshot) else {
            return;
        };
        let Some(doc) = self.doc_states.get_mut(key) else {
            return;
        };
        match doc.begin_save(snapshot) {
            SaveAction::Dispatch(request) => {
                if let Some(sink) = &self.save_sink {
                    sink(request);
                } else {
                    doc.finish_save(request.ticket, Err("Saving is not available".to_owned()));
                }
                self.doc_notice = None;
            }
            SaveAction::Queued => {}
            SaveAction::NeedsPath => {
                self.doc_notice =
                    Some((StatusKind::Warning, SAVE_PATH_REQUIRED_MESSAGE.to_owned()));
            }
        }
        cx.notify();
    }

    /// Asks for a target path for a document without a file (welcome / demo buffer) via the
    /// native *Save As* dialog, falling back to a status-bar hint when the platform cannot
    /// show one.
    fn prompt_save_path(key: PathBuf, window: &mut Window, cx: &mut Context<'_, Self>) {
        let directory = std::env::current_dir().unwrap_or_default();
        let suggested = key.file_name().map_or_else(
            || "Untitled.java".to_owned(),
            |n| n.to_string_lossy().into_owned(),
        );
        let receiver = cx.prompt_for_new_path(&directory, Some(&suggested));
        cx.spawn_in(window, async move |this, cx| {
            let chosen = receiver.await;
            drop(this.update(cx, |shell, cx| match chosen {
                Ok(Ok(Some(path))) => {
                    if let Some(doc) = shell.doc_states.get_mut(&key) {
                        doc.set_path(path);
                    }
                    shell.dispatch_save(&key, cx);
                }
                Ok(Ok(None)) => {}
                _ => {
                    shell.doc_notice =
                        Some((StatusKind::Warning, SAVE_PATH_REQUIRED_MESSAGE.to_owned()));
                    cx.notify();
                }
            }));
        })
        .detach();
    }

    /// Window title for the active tab: `Main.java • — Kova` when dirty.
    fn active_window_title(&self) -> String {
        let Some(tab) = self
            .panes
            .get(&self.active_pane)
            .and_then(|pane| pane.tabs.active_tab())
        else {
            return "Kova".to_owned();
        };
        let dirty = tab
            .path
            .as_deref()
            .is_some_and(|key| self.is_document_dirty(key));
        if dirty {
            format!("{} • — Kova", tab.title)
        } else {
            format!("{} — Kova", tab.title)
        }
    }

    /// Status-bar text for the active document: a pending notice, else its save status.
    fn document_status(&self) -> (StatusKind, String) {
        self.doc_notice.clone().unwrap_or_else(|| {
            self.active_document_state()
                .map_or((StatusKind::Info, "Ready".to_owned()), |doc| {
                    doc.status_label()
                })
        })
    }

    /// Splits the active pane along `axis`; the new pane shows the same document through the
    /// shared buffer (independent scroll offset and cursor) and becomes active.
    pub fn split_active_pane(
        &mut self,
        axis: SplitAxis,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let source = self.active_pane;
        let new_id = PaneId(self.split_tree.max_pane_id() + 1);
        if !self.split_tree.split_pane(source, axis, new_id) {
            return;
        }
        self.panes.insert(new_id, PaneState::new(new_id));
        let active_doc = self
            .panes
            .get(&source)
            .and_then(|pane| pane.tabs.active_tab())
            .and_then(|tab| tab.path.clone())
            .and_then(|path| self.documents.get(&path).cloned().map(|buf| (path, buf)));
        self.active_pane = new_id;
        if let Some((path, buffer)) = active_doc {
            self.show_document(&path, buffer, None, window, cx);
        } else {
            self.set_active_pane(new_id, cx);
            cx.notify();
        }
    }

    fn toggle_tool(&mut self, tool: DockToolId, window: &mut Window, cx: &mut Context<'_, Self>) {
        // An explicit tool window request leaves Focus Mode deterministically (FR-1.2).
        self.focus_tiling.exit_for_explicit_tool();
        self.dock_layout.toggle_tool(tool, self.active_pane);
        match self.dock_layout.focused {
            FocusedRegion::EditorPane(_) => self.focus_active_editor(window, cx),
            region => self.focus_dock(region, window, cx),
        }
    }

    fn show_bottom_tab(&mut self, tab: BottomTab, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.dock_layout.bottom_open != Some(tab.tool()) {
            self.dock_layout.toggle_tool(tab.tool(), self.active_pane);
        }
        self.focus_dock(FocusedRegion::BottomPanel, window, cx);
    }

    fn jump_to_symbol(&mut self, index: usize, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.structure.select_index(index);
        let Some((line, col)) = self.structure.selected_item().map(|s| (s.line, s.col)) else {
            return;
        };
        if let Some(editor) = self.active_editor().cloned() {
            editor.update(cx, |editor, cx| editor.set_cursor_position(line, col, cx));
        }
        self.focus_active_editor(window, cx);
    }

    fn jump_to_problem(&mut self, index: usize, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.bottom_panel.select_problem(index);
        let Some(problem) = self.bottom_panel.selected_problem().cloned() else {
            return;
        };
        self.open_path(
            Path::new(&problem.file),
            Some((problem.line, problem.col)),
            window,
            cx,
        );
    }

    fn click_tree_row(&mut self, row: usize, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.project_tree.select_index(row);
        self.dock_layout.focus(FocusedRegion::ProjectDock);
        match self.project_tree.activate_selected() {
            Some(TreeActivation::OpenFile(path)) => self.open_path(&path, None, window, cx),
            Some(TreeActivation::Toggled) | None => {
                self.focus_dock(FocusedRegion::ProjectDock, window, cx);
            }
        }
    }

    // ---------------------------------------------------------------------------------------
    // Keyboard and mouse handling
    // ---------------------------------------------------------------------------------------

    /// Capture-phase key router (`KOVA-026` FR-3): runs before the editor sees the key, in the
    /// order `Ex line → chord continuation → Vim → IME guard → JetBrains keymap`
    /// ([`key_route`]). Keys nobody claims propagate to the editor and the IME unchanged.
    fn handle_key_capture(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let keystroke = &event.keystroke;
        if self.settings_open {
            self.handle_settings_key(keystroke, window, cx);
            cx.stop_propagation();
            return;
        }
        if self.ex_line.is_some() {
            self.handle_ex_key(keystroke, window, cx);
            cx.stop_propagation();
            return;
        }
        // Terminal focused: its own shortcuts first, `Cmd` chords to the IDE keymap, every
        // other key bubbles to the session (`Ctrl+C`, `Ctrl+W`, `Esc`, `j`/`k` …).
        if let Some(dock) = self.terminal_focused(window, cx) {
            if let Some(shortcut) = terminal_shortcut(keystroke, cfg!(target_os = "macos")) {
                self.run_terminal_shortcut(shortcut, &dock, window, cx);
                cx.stop_propagation();
                return;
            }
            if !keystroke.modifiers.platform {
                return;
            }
        }
        let editor = self
            .active_editor()
            .filter(|editor| editor.read(cx).focus_handle().is_focused(window))
            .cloned();
        let (vim_status, vim_owns_keys) = editor.as_ref().map_or((None, false), |editor| {
            let editor = editor.read(cx);
            (editor.vim_status(), editor.vim_owns_keys())
        });
        let accepts_text = editor.is_some() && !vim_owns_keys;
        let flags = RouteFlags {
            command_line_open: false,
            chord_pending: self.keymap.has_pending(),
            vim_owns_keys,
            text_composition: accepts_text
                && is_text_composition(keystroke, self.keymap.platform()),
        };
        let leaf = match (&editor, &vim_status) {
            (Some(_), Some(status)) => status.focus,
            (Some(_), None) => kova_core::FocusContext::EditorText,
            (None, _) => match self.dock_layout.focused {
                FocusedRegion::ProjectDock => kova_core::FocusContext::ProjectTree,
                FocusedRegion::StructureDock => kova_core::FocusContext::StructureView,
                FocusedRegion::BottomPanel => kova_core::FocusContext::ToolWindow,
                FocusedRegion::EditorPane(_) => kova_core::FocusContext::Workspace,
            },
        };
        let ctx = DispatchContext {
            leaf,
            vim_enabled: self.vim_enabled,
        };

        match key_route(flags) {
            KeyRoute::TextInput | KeyRoute::CommandLine => {}
            KeyRoute::VimThenKeymap => {
                if let (Some(editor), Some(stroke)) = (&editor, vim_stroke(keystroke)) {
                    let result =
                        editor.update(cx, |editor, cx| editor.handle_vim_key(stroke, window, cx));
                    match result {
                        VimKeyResult::Handled { .. } => {
                            cx.stop_propagation();
                            cx.notify();
                            return;
                        }
                        VimKeyResult::OpenCommandLine(prefix) => {
                            self.ex_line = Some(ExCommandLine::new(prefix));
                            cx.stop_propagation();
                            cx.notify();
                            return;
                        }
                        VimKeyResult::PassThrough => {}
                    }
                }
                self.dispatch_keymap(keystroke, ctx, window, cx);
            }
            KeyRoute::ChordContinuation | KeyRoute::Keymap => {
                // Vim INSERT: keys that leave the mode go to Vim before the keymap.
                if !flags.chord_pending
                    && let Some(editor) = &editor
                    && vim_status.is_some()
                    && is_vim_insert_exit(keystroke)
                    && let Some(stroke) = vim_stroke(keystroke)
                {
                    editor.update(cx, |editor, cx| editor.handle_vim_key(stroke, window, cx));
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
                self.dispatch_keymap(keystroke, ctx, window, cx);
            }
        }
    }

    /// Resolves `keystroke` against the `JetBrains` keymap and runs the matched shell action.
    fn dispatch_keymap(
        &mut self,
        keystroke: &gpui::Keystroke,
        ctx: DispatchContext,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(stroke) = shortcut_stroke(keystroke) else {
            return;
        };
        match self.keymap.dispatch(stroke, ctx, Instant::now()) {
            DispatchOutcome::Action(action) => {
                self.chord_hint = None;
                cx.stop_propagation();
                self.run_shell_action(action, window, cx);
                cx.notify();
            }
            DispatchOutcome::Pending { hint, epoch } => {
                self.chord_hint = Some(hint);
                Self::schedule_chord_timeout(epoch, window, cx);
                cx.stop_propagation();
                cx.notify();
            }
            DispatchOutcome::Cancelled => {
                self.chord_hint = None;
                cx.stop_propagation();
                cx.notify();
            }
            DispatchOutcome::Delegated(_) | DispatchOutcome::Unhandled => {
                if self.chord_hint.take().is_some() {
                    cx.notify();
                }
            }
        }
    }

    /// Clears the chord prefix after [`CHORD_TIMEOUT`] unless a newer key arrived (epoch).
    fn schedule_chord_timeout(epoch: u64, window: &Window, cx: &mut Context<'_, Self>) {
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(CHORD_TIMEOUT).await;
            this.update(cx, |shell, cx| {
                if shell.keymap.expire(epoch) {
                    shell.chord_hint = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Runs one keymap-resolved shell command.
    fn run_shell_action(
        &mut self,
        action: ShellAction,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match action {
            ShellAction::ToggleTool(tool) => self.toggle_tool(tool, window, cx),
            ShellAction::ToggleFocusMode(variant) => self.toggle_focus_mode(variant, window, cx),
            ShellAction::ToggleTilingMode => {
                self.focus_tiling.toggle_density();
            }
            ShellAction::ToggleVimMode => self.set_vim_enabled(!self.vim_enabled, cx),
            ShellAction::FocusDirection(dir) => self.focus_direction(dir, window, cx),
            ShellAction::MoveTabToSplit(dir) => self.move_tab_to_split(dir, window, cx),
            ShellAction::ResizeSplit(dir) => {
                let axis = if dir.is_horizontal() {
                    SplitAxis::Horizontal
                } else {
                    SplitAxis::Vertical
                };
                let delta = if dir.is_backward() {
                    -RESIZE_SPLIT_STEP
                } else {
                    RESIZE_SPLIT_STEP
                };
                self.split_tree.resize_pane(self.active_pane, axis, delta);
            }
            ShellAction::Split(axis) => self.split_active_pane(axis, window, cx),
            ShellAction::CloseActiveTab => self.close_active_tab(window, cx),
            ShellAction::CycleTab(forward) => self.cycle_tab(forward, window, cx),
            ShellAction::Save => self.save_active(window, cx),
            ShellAction::DuplicateLine => {
                if let Some(editor) = self.active_editor().cloned() {
                    editor.update(cx, |editor, cx| editor.duplicate_lines(window, cx));
                }
            }
            ShellAction::ToggleLineComment => {
                let prefix = line_comment_prefix(self.active_document_key().as_deref());
                if let Some(editor) = self.active_editor().cloned() {
                    editor.update(cx, |editor, cx| {
                        editor.toggle_line_comment(prefix, window, cx);
                    });
                }
            }
            ShellAction::AddNextOccurrence => {
                if let Some(editor) = self.active_editor().cloned() {
                    editor.update(cx, |editor, cx| editor.add_next_occurrence(window, cx));
                }
            }
            ShellAction::OpenSearchEverywhere => self.set_notice(
                StatusKind::Info,
                "Search Everywhere arrives with KOVA-028".to_owned(),
                cx,
            ),
            ShellAction::OpenSettings => self.open_settings(window, cx),
        }
    }

    /// Installs the host writer that persists `settings.toml` edits made in the Settings view.
    pub fn set_settings_sink(&mut self, sink: SettingsWriteSink) {
        self.settings_sink = Some(DebugSink(sink));
    }

    /// Whether the Settings overlay is open.
    #[must_use]
    pub const fn is_settings_open(&self) -> bool {
        self.settings_open
    }

    /// The Settings overlay state, once it has been opened.
    #[must_use]
    pub const fn settings_view(&self) -> Option<&SettingsView> {
        self.settings_view.as_ref()
    }

    /// Settings currently applied to the workspace.
    #[must_use]
    pub const fn applied_settings(&self) -> &KovaSettings {
        &self.applied_settings
    }

    /// Opens the Settings overlay (`Cmd+,` / `Ctrl+Alt+S`), building it on first use.
    pub fn open_settings(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.settings_view.is_none() {
            match SettingsView::from_source(self.settings_source.as_ref()) {
                Ok(mut view) => {
                    if let Some(message) = self.settings_error.clone() {
                        view.set_load_error(message);
                    }
                    self.settings_view = Some(view);
                }
                Err(err) => {
                    self.set_notice(
                        StatusKind::Error,
                        format!("Settings unavailable: {err}"),
                        cx,
                    );
                    return;
                }
            }
        }
        self.settings_open = true;
        self.ex_line = None;
        self.keymap.reset();
        self.chord_hint = None;
        window.focus(&self.focus_handle);
        cx.notify();
    }

    /// Closes the Settings overlay and returns focus to the active editor.
    pub fn close_settings(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if !self.settings_open {
            return;
        }
        let outcome = self
            .settings_view
            .as_mut()
            .map_or(SettingsKeyOutcome::Ignored, SettingsView::commit_editing);
        if outcome == SettingsKeyOutcome::Changed {
            self.persist_settings(cx);
        }
        self.settings_open = false;
        self.focus_active_editor(window, cx);
        cx.notify();
    }

    /// Accepts a `settings.toml` snapshot read by the host after a watcher reload (or at
    /// startup): refreshes the open form (ignoring echoes of our own writes) and applies the
    /// settings to the workspace.
    pub fn update_settings_source(&mut self, source: SettingsSource, cx: &mut Context<'_, Self>) {
        if let Some(view) = self.settings_view.as_mut()
            && let ReloadOutcome::Invalid(message) = view.apply_source(&source)
        {
            self.set_notice(StatusKind::Warning, format!("settings.toml: {message}"), cx);
        }
        let parsed = match &source.contents {
            Some(contents) => {
                SettingsDocument::from_toml_str(contents).map(|d| d.settings().clone())
            }
            None => Ok(KovaSettings::default()),
        };
        self.settings_source = Some(source);
        if let Ok(settings) = parsed {
            self.settings_error = None;
            self.apply_settings(settings, cx);
        }
        cx.notify();
    }

    /// Shows a `settings.toml` reload failure in the Settings banner (Last-Known-Good stays).
    pub fn report_settings_error(&mut self, message: String, cx: &mut Context<'_, Self>) {
        self.settings_error = Some(message.clone());
        if let Some(view) = self.settings_view.as_mut() {
            view.set_load_error(message);
            cx.notify();
        }
    }

    /// Reports the result of a background `settings.toml` write.
    pub fn finish_settings_write(
        &mut self,
        result: Result<(), String>,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(view) = self.settings_view.as_mut() {
            view.set_write_error(result.as_ref().err().cloned());
        }
        if let Err(message) = result {
            self.set_notice(
                StatusKind::Error,
                format!("Could not write settings.toml: {message}"),
                cx,
            );
        }
        cx.notify();
    }

    /// Routes a key to the open Settings overlay.
    fn handle_settings_key(
        &mut self,
        keystroke: &gpui::Keystroke,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let m = &keystroke.modifiers;
        let command = m.platform || m.control || m.alt;
        // `Cmd+,` / `Ctrl+,` / `Ctrl+Alt+S` toggle the overlay closed again.
        let toggle =
            (command && keystroke.key == ",") || (m.control && m.alt && keystroke.key == "s");
        if toggle {
            self.close_settings(window, cx);
            return;
        }
        let Some(view) = self.settings_view.as_mut() else {
            return;
        };
        let outcome = view.handle_key(SettingsKey {
            key: &keystroke.key,
            text: keystroke.key_char.as_deref(),
            shift: m.shift,
            command,
        });
        self.finish_settings_outcome(outcome, window, cx);
    }

    /// Handles a click inside the Settings overlay.
    pub(crate) fn settings_click(
        &mut self,
        click: SettingsClick,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(view) = self.settings_view.as_mut() else {
            return;
        };
        let outcome = view.click(click);
        self.finish_settings_outcome(outcome, window, cx);
    }

    fn finish_settings_outcome(
        &mut self,
        outcome: SettingsKeyOutcome,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match outcome {
            SettingsKeyOutcome::Ignored => return,
            SettingsKeyOutcome::Handled => {}
            SettingsKeyOutcome::Changed => self.persist_settings(cx),
            SettingsKeyOutcome::OpenFile(path) => {
                self.close_settings(window, cx);
                self.open_config_file(path, window, cx);
            }
            SettingsKeyOutcome::Close => self.close_settings(window, cx),
        }
        cx.notify();
    }

    /// Applies the form's settings to the workspace and hands the document to the host writer.
    fn persist_settings(&mut self, cx: &mut Context<'_, Self>) {
        let Some(view) = self.settings_view.as_ref() else {
            return;
        };
        let settings = view.settings().clone();
        let file_broken = view.model().load_error().is_some();
        let request = view.settings_path().map(|path| SettingsWriteRequest {
            path: path.clone(),
            document: view.model().document().clone(),
        });
        match (request, self.settings_sink.as_ref()) {
            // Never overwrite a hand-edited file that does not parse: the user's edits and
            // comments would be lost (NFR-1). The change applies to this session only.
            _ if file_broken => self.set_notice(
                StatusKind::Warning,
                "settings.toml has errors — fix the file to save changes from Settings".to_owned(),
                cx,
            ),
            (Some(request), Some(sink)) => (sink.0)(request),
            _ => self.set_notice(
                StatusKind::Warning,
                "settings.toml is not available yet — the change applies to this session only"
                    .to_owned(),
                cx,
            ),
        }
        self.apply_settings(settings, cx);
    }

    /// Applies the settings that differ from [`Self::applied_settings`] to the live workspace:
    /// Vim mode, editor font, tiling density, dock sizes and the status bar. The theme is
    /// switched by the host watcher, which owns the theme registry.
    pub fn apply_settings(&mut self, settings: KovaSettings, cx: &mut Context<'_, Self>) {
        let previous = std::mem::replace(&mut self.applied_settings, settings);
        let next = &self.applied_settings;
        if next.editor.vim_mode != previous.editor.vim_mode
            && next.editor.vim_mode != self.vim_enabled
        {
            self.set_vim_enabled(next.editor.vim_mode, cx);
        }
        let next = &self.applied_settings;
        if next.ui.focus_mode != previous.ui.focus_mode {
            let density = if next.ui.focus_mode {
                LayoutDensity::Tiling
            } else {
                LayoutDensity::Islands
            };
            self.set_layout_density(density, cx);
        }
        let next = &self.applied_settings;
        if (next.ui.sidebar_width - previous.ui.sidebar_width).abs() > f64::EPSILON {
            let width = f64_to_f32(next.ui.sidebar_width);
            self.dock_layout.set_left_width(width);
            self.dock_layout.set_right_width(width);
        }
        if (next.ui.bottom_panel_height - previous.ui.bottom_panel_height).abs() > f64::EPSILON {
            self.dock_layout
                .set_bottom_height(f64_to_f32(next.ui.bottom_panel_height));
        }
        self.show_status_bar = next.ui.show_status_bar;
        if next.editor.font_family != previous.editor.font_family
            || (next.editor.font_size - previous.editor.font_size).abs() > f64::EPSILON
        {
            let (family, size) = editor_font(next);
            for pane in self.panes.values() {
                for editor in pane.editors.values() {
                    editor.update(cx, |editor, cx| editor.set_font(&family, size, cx));
                }
            }
        }
        let next = &self.applied_settings;
        if (next.terminal != previous.terminal
            || next.editor.font_family != previous.editor.font_family)
            && let Some((dock, _)) = &self.terminal
        {
            let launch = TerminalLaunch::from_settings(self.project_root.clone(), next);
            dock.update(cx, |dock, cx| dock.set_launch(launch, cx));
        }
        cx.notify();
    }

    /// Opens an absolute config file (`settings.toml`, `keymap.toml`) in an editor tab; the
    /// file is read on the background executor.
    pub fn open_config_file(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let read = cx.background_executor().spawn({
            let path = path.clone();
            async move {
                match std::fs::read_to_string(&path) {
                    Ok(text) => Ok(Some(text)),
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(err) => Err(err.to_string()),
                }
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let text = read.await;
            this.update_in(cx, |this, window, cx| match text {
                Ok(text) => {
                    let buffer = text.map(|t| Buffer::from_text(&t));
                    this.open_document(path, buffer, None, None, window, cx);
                }
                Err(err) => {
                    this.report_open_error(format!("Cannot open {}: {err}", path.display()), cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Enables or disables modal Vim editing in every open editor (`ToggleVimMode`).
    /// Buffers, scroll offsets, cursors and undo history are kept.
    pub fn set_vim_enabled(&mut self, enabled: bool, cx: &mut Context<'_, Self>) {
        self.vim_enabled = enabled;
        if !enabled {
            self.ex_line = None;
        }
        for pane in self.panes.values() {
            for editor in pane.editors.values() {
                editor.update(cx, |editor, cx| editor.set_vim_enabled(enabled, cx));
            }
        }
        self.keymap.reset();
        self.chord_hint = None;
        cx.notify();
    }

    /// Whether modal Vim editing is enabled.
    #[must_use]
    pub const fn is_vim_enabled(&self) -> bool {
        self.vim_enabled
    }

    /// Sets the island spacing preset (`UiSettings::focus_mode` = [`LayoutDensity::Tiling`]).
    pub fn set_layout_density(&mut self, density: LayoutDensity, cx: &mut Context<'_, Self>) {
        if self.focus_tiling.density() != density {
            self.focus_tiling.toggle_density();
            cx.notify();
        }
    }

    /// Focus Mode and density state.
    #[must_use]
    pub const fn focus_tiling(&self) -> &FocusTilingController {
        &self.focus_tiling
    }

    /// Toggles Focus Mode (`Cmd+Shift+F12` hides the docks, `Ctrl+Alt+Z` / `:only` also hides
    /// the stripes) and restores the saved docks and focus on the second press.
    pub fn toggle_focus_mode(
        &mut self,
        variant: FocusModeVariant,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let split_tree = &self.split_tree;
        let result = self.focus_tiling.toggle_focus_mode(
            variant,
            &mut self.dock_layout,
            self.active_pane,
            |pane| split_tree.contains(pane),
        );
        match result {
            FocusToggle::Entered(_) | FocusToggle::VariantChanged(_) => {
                self.focus_active_editor(window, cx);
            }
            FocusToggle::Restored(FocusedRegion::EditorPane(pane)) => {
                self.set_active_pane(pane, cx);
                self.focus_active_editor(window, cx);
            }
            FocusToggle::Restored(region) => self.focus_dock(region, window, cx),
        }
    }

    /// On-screen rectangles of every visible island, mirroring [`Self::render`].
    fn region_rects(&self) -> Vec<(FocusedRegion, crate::dock::LayoutRect)> {
        crate::dock::compute_region_rects(
            self.viewport_px.0,
            self.viewport_px.1,
            &self.dock_layout,
            &self.split_tree,
            self.focus_tiling.density(),
            !self.focus_tiling.hides_stripes(),
        )
    }

    /// Moves keyboard focus to the neighbouring island in `dir` (no wrap-around).
    fn focus_direction(&mut self, dir: Direction, window: &mut Window, cx: &mut Context<'_, Self>) {
        let rects = self.region_rects();
        match crate::dock::find_neighbor_pane(&rects, self.dock_layout.focused, dir) {
            Some(FocusedRegion::EditorPane(pane)) => {
                self.set_active_pane(pane, cx);
                self.focus_active_editor(window, cx);
            }
            Some(region) => self.focus_dock(region, window, cx),
            None => {}
        }
    }

    /// Moves the active tab into the neighbouring split in `dir`, creating that split when
    /// there is none (and the pane keeps at least one other tab).
    fn move_tab_to_split(
        &mut self,
        dir: Direction,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let source = self.active_pane;
        let Some((tab, path)) = self
            .panes
            .get(&source)
            .and_then(|pane| pane.tabs.active_tab())
            .and_then(|tab| tab.path.clone().map(|path| (tab.id, path)))
        else {
            return;
        };
        let Some(buffer) = self.documents.get(&path).cloned() else {
            return;
        };
        let position = self
            .active_editor()
            .map(|editor| editor.read(cx).primary_cursor_position());
        let editor_rects: Vec<_> = self
            .region_rects()
            .into_iter()
            .filter(|(region, _)| matches!(region, FocusedRegion::EditorPane(_)))
            .collect();
        let neighbour =
            crate::dock::find_neighbor_pane(&editor_rects, FocusedRegion::EditorPane(source), dir);
        let target = if let Some(FocusedRegion::EditorPane(pane)) = neighbour {
            pane
        } else {
            let tab_count = self.panes.get(&source).map_or(0, |p| p.tabs.tab_count());
            if tab_count < 2 {
                return;
            }
            let new_id = PaneId(self.split_tree.max_pane_id() + 1);
            let axis = if dir.is_horizontal() {
                SplitAxis::Horizontal
            } else {
                SplitAxis::Vertical
            };
            let split = if dir.is_backward() {
                self.split_tree.split_pane_before(source, axis, new_id)
            } else {
                self.split_tree.split_pane(source, axis, new_id)
            };
            if !split {
                return;
            }
            self.panes.insert(new_id, PaneState::new(new_id));
            new_id
        };
        self.active_pane = target;
        self.show_document(&path, buffer, position, window, cx);
        self.detach_tab(source, tab);
        self.set_active_pane(target, cx);
        self.drop_unused_documents();
        cx.notify();
    }

    /// Removes `tab` from `pane` without moving focus; closes the pane when it becomes empty.
    fn detach_tab(&mut self, pane: PaneId, tab: TabId) {
        let Some(state) = self.panes.get_mut(&pane) else {
            return;
        };
        state.tabs.close_tab(tab);
        state.editors.remove(&tab);
        state.subscriptions.remove(&tab);
        if state.tabs.tab_count() == 0 && self.split_tree.close(pane) {
            self.panes.remove(&pane);
        }
    }

    fn close_active_tab(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let pane = self.active_pane;
        if let Some(tab) = self.panes.get(&pane).and_then(|p| p.tabs.active_tab_id()) {
            self.close_tab(pane, tab, window, cx);
        }
    }

    /// Handles one key while the Ex / search command line is open.
    fn handle_ex_key(
        &mut self,
        keystroke: &gpui::Keystroke,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(line) = self.ex_line.as_mut() else {
            return;
        };
        let m = &keystroke.modifiers;
        let event = if m.control || m.platform {
            if m.control && matches!(keystroke.key.as_str(), "c" | "[") {
                ExLineEvent::Cancelled
            } else {
                ExLineEvent::Ignored
            }
        } else {
            match keystroke.key_char.as_deref() {
                Some(text)
                    if !text.is_empty()
                        && !text.chars().any(char::is_control)
                        && keystroke.key != "enter" =>
                {
                    line.insert(text)
                }
                _ => line.handle_named_key(&keystroke.key),
            }
        };
        match event {
            ExLineEvent::Submitted { prefix, text } => {
                self.ex_line = None;
                self.run_ex_line(prefix, &text, window, cx);
            }
            ExLineEvent::Cancelled => {
                self.ex_line = None;
                if let Some(editor) = self.active_editor().cloned() {
                    editor.update(cx, EditorView::force_vim_normal);
                }
            }
            ExLineEvent::Edited | ExLineEvent::Ignored => {}
        }
        cx.notify();
    }

    /// Executes a submitted `:` command (search prompts report that search is not available).
    fn run_ex_line(
        &mut self,
        prefix: char,
        text: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(editor) = self.active_editor().cloned() {
            editor.update(cx, EditorView::force_vim_normal);
        }
        if prefix != ':' {
            self.set_notice(
                StatusKind::Info,
                format!("Search ({prefix}) is not available yet"),
                cx,
            );
            return;
        }
        let command = match parse_ex_command(text) {
            Ok(command) => command,
            Err(err) => {
                self.set_notice(StatusKind::Error, err.to_string(), cx);
                return;
            }
        };
        match command {
            ExCommand::Write => self.save_active(window, cx),
            ExCommand::Quit => {
                if self.is_active_document_dirty() {
                    self.set_notice(
                        StatusKind::Warning,
                        "E37: No write since last change (add ! to override)".to_owned(),
                        cx,
                    );
                } else {
                    self.close_active_tab(window, cx);
                }
            }
            ExCommand::ForceQuit => self.close_active_tab(window, cx),
            ExCommand::WriteQuit => {
                let key = self.active_document_key();
                let has_path = key
                    .as_ref()
                    .and_then(|key| self.doc_states.get(key))
                    .is_some_and(|doc| doc.path.is_some());
                if !self.is_active_document_dirty() {
                    self.close_active_tab(window, cx);
                } else if has_path {
                    // Close only once the asynchronous save succeeds (a failure keeps the tab).
                    self.close_after_save = key;
                    self.save_active(window, cx);
                } else {
                    self.set_notice(
                        StatusKind::Warning,
                        "E32: No file name — save it first (Cmd+S), then :q".to_owned(),
                        cx,
                    );
                    self.save_active(window, cx);
                }
            }
            ExCommand::Split(axis) => self.split_active_pane(axis, window, cx),
            ExCommand::GoToLine(line) => {
                if let Some(editor) = self.active_editor().cloned() {
                    editor.update(cx, |editor, cx| editor.go_to_line_column(line, None, cx));
                }
            }
            ExCommand::NoHighlight => self.clear_notice(cx),
            ExCommand::Only => {
                if !self.focus_tiling.is_focus_mode() {
                    self.toggle_focus_mode(FocusModeVariant::Zen, window, cx);
                }
            }
        }
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let keystroke = &event.keystroke;
        let mods = &keystroke.modifiers;
        let key = keystroke.key.as_str();

        // Keys the keymap did not claim. macOS keeps the `Alt+1..7` tool window aliases, but
        // only while no editor accepts text there (`Option+digit` types characters).
        let text_editor_focused = self.active_editor().is_some_and(|editor| {
            let editor = editor.read(cx);
            editor.focus_handle().is_focused(window) && !editor.vim_owns_keys()
        });
        if mods.alt
            && !mods.platform
            && !mods.control
            && (!cfg!(target_os = "macos") || !text_editor_focused)
        {
            let tool = match key {
                "1" => Some(DockToolId::Project),
                "0" => Some(DockToolId::Commit),
                "4" => Some(DockToolId::Run),
                "6" => Some(DockToolId::Problems),
                "7" => Some(DockToolId::Structure),
                "f12" => Some(DockToolId::Terminal),
                _ => None,
            };
            if let Some(tool) = tool {
                self.toggle_tool(tool, window, cx);
                cx.stop_propagation();
                return;
            }
        }

        // `Cmd+\` / `Cmd+Shift+\`: split right / down (not part of the `JetBrains` preset).
        if mods.platform && !mods.alt && !mods.control && matches!(key, "\\" | "|") {
            let axis = if mods.shift {
                SplitAxis::Vertical
            } else {
                SplitAxis::Horizontal
            };
            self.split_active_pane(axis, window, cx);
            cx.stop_propagation();
            return;
        }

        if !self.focus_handle.is_focused(window) || mods.platform || mods.control || mods.alt {
            return;
        }
        let handled = match self.dock_layout.focused {
            FocusedRegion::ProjectDock => {
                self.handle_tree_key(key, keystroke.key_char.as_deref(), window, cx)
            }
            FocusedRegion::StructureDock => self.handle_structure_key(key, window, cx),
            FocusedRegion::BottomPanel => self.handle_bottom_key(key, window, cx),
            FocusedRegion::EditorPane(_) => false,
        };
        if handled {
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn cycle_tab(&mut self, forward: bool, window: &mut Window, cx: &mut Context<'_, Self>) {
        if let Some(pane) = self.panes.get_mut(&self.active_pane) {
            if forward {
                pane.tabs.next_tab();
            } else {
                pane.tabs.prev_tab();
            }
        }
        self.set_active_pane(self.active_pane, cx);
        self.refresh_analysis(Duration::ZERO, cx);
        self.focus_active_editor(window, cx);
    }

    fn handle_tree_key(
        &mut self,
        key: &str,
        key_char: Option<&str>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let vim_nav = self.status_bar.vim_mode != VimModeBadge::Off
            && self.project_tree.filter_query().is_empty();
        match key {
            "up" => self.project_tree.move_selection_up(),
            "down" => self.project_tree.move_selection_down(),
            "k" if vim_nav => self.project_tree.move_selection_up(),
            "j" if vim_nav => self.project_tree.move_selection_down(),
            "left" => self.project_tree.collapse_selected(),
            "right" => self.project_tree.expand_selected(),
            "enter" => {
                if let Some(TreeActivation::OpenFile(path)) = self.project_tree.activate_selected()
                {
                    self.open_path(&path, None, window, cx);
                }
            }
            "backspace" => {
                self.project_tree.pop_filter_char();
            }
            "escape" => {
                if self.project_tree.filter_query().is_empty() {
                    self.focus_active_editor(window, cx);
                } else {
                    self.project_tree.set_filter("");
                }
            }
            _ => match key_char {
                Some(text) if !text.is_empty() && !text.chars().any(char::is_control) => {
                    for ch in text.chars() {
                        self.project_tree.push_filter_char(ch);
                    }
                }
                _ => return false,
            },
        }
        self.project_scroll
            .scroll_to_item(self.project_tree.selected_index(), ScrollStrategy::Top);
        true
    }

    fn handle_structure_key(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        match key {
            "up" | "k" => self.structure.select_prev(),
            "down" | "j" => self.structure.select_next(),
            "enter" => {
                let index = self.structure.selected_index();
                self.jump_to_symbol(index, window, cx);
            }
            "escape" => self.focus_active_editor(window, cx),
            _ => return false,
        }
        self.structure_scroll
            .scroll_to_item(self.structure.selected_index(), ScrollStrategy::Top);
        true
    }

    fn handle_bottom_key(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        match key {
            "up" | "k" => self.bottom_panel.move_selection(false),
            "down" | "j" => self.bottom_panel.move_selection(true),
            "enter" => {
                let index = self.bottom_panel.selected_problem_index();
                self.jump_to_problem(index, window, cx);
            }
            "escape" => self.focus_active_editor(window, cx),
            _ => return false,
        }
        true
    }

    fn start_drag(
        &mut self,
        target: DragTarget,
        event: &MouseDownEvent,
        cx: &mut Context<'_, Self>,
    ) {
        let (start_mouse, start_value) = match target {
            DragTarget::LeftDock => (f32::from(event.position.x), self.dock_layout.left_width),
            DragTarget::RightDock => (f32::from(event.position.x), self.dock_layout.right_width),
            DragTarget::BottomPanel => {
                (f32::from(event.position.y), self.dock_layout.bottom_height)
            }
        };
        self.drag = Some(DragState {
            target,
            start_mouse,
            start_value,
        });
        cx.stop_propagation();
    }

    fn handle_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(drag) = self.drag else {
            return;
        };
        let x = f32::from(event.position.x);
        let y = f32::from(event.position.y);
        match drag.target {
            DragTarget::LeftDock => self
                .dock_layout
                .set_left_width(drag.start_value + (x - drag.start_mouse)),
            DragTarget::RightDock => self
                .dock_layout
                .set_right_width(drag.start_value - (x - drag.start_mouse)),
            DragTarget::BottomPanel => self
                .dock_layout
                .set_bottom_height(drag.start_value - (y - drag.start_mouse)),
        }
        cx.notify();
    }

    fn handle_mouse_up(
        &mut self,
        _event: &MouseUpEvent,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.drag.take().is_some() {
            cx.notify();
        }
    }

    // ---------------------------------------------------------------------------------------
    // Rendering
    // ---------------------------------------------------------------------------------------

    fn render_titlebar(&self, p: Palette) -> Div {
        let pill = |text: String, bg: Hsla, fg: Hsla| {
            div()
                .h(px(22.0))
                .px(px(10.0))
                .flex()
                .items_center()
                .gap(px(6.0))
                .rounded(px(6.0))
                .border_1()
                .border_color(p.border_island)
                .bg(bg)
                .text_size(px(12.0))
                .text_color(fg)
                .child(text)
        };
        let branch = format!("⎇ {}", self.status_bar.git_branch);

        div()
            .h(px(TITLEBAR_HEIGHT_PX))
            .w_full()
            .pl(px(TITLEBAR_LEFT_PADDING_PX))
            .pr(px(self.focus_tiling.density().outer_margin_px()))
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        pill(self.project_name.clone(), p.island, p.text_primary)
                            .font_weight(FontWeight::SEMIBOLD),
                    )
                    .child(pill(branch, p.island, p.text_secondary)),
            )
            .child(
                div()
                    .w(px(320.0))
                    .h(px(24.0))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(p.border_island)
                    .bg(p.input)
                    .text_size(px(12.0))
                    .child(div().text_color(p.text_muted).child("⌕  Search Everywhere"))
                    .child(div().text_color(p.text_muted).child("⇧⇧")),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        pill(String::new(), p.island, p.text_primary)
                            .child(div().text_color(p.run_icon).child("▶"))
                            .child("OrderApp"),
                    )
                    .child(pill("Islands".to_owned(), p.island, p.text_secondary))
                    .child(pill("⚙  ⌘,".to_owned(), p.island, p.text_muted)),
            )
    }

    fn render_stripe(
        &self,
        width_px: f32,
        top: &[DockToolId],
        bottom: &[DockToolId],
        p: Palette,
        cx: &Context<'_, Self>,
    ) -> Div {
        let render_item = |tool: DockToolId| {
            let is_active = self.dock_layout.is_tool_active(tool);
            let mut item = div()
                .id(SharedString::from(format!("stripe-{}", tool.title())))
                .relative()
                .w(px(width_px - 6.0))
                .h(px(26.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.0))
                .text_size(px(9.0))
                .font_weight(FontWeight::SEMIBOLD)
                .hover(|s| s.bg(p.hover))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.toggle_tool(tool, window, cx);
                }));
            if is_active {
                item = item.bg(p.active).text_color(p.text_primary).child(
                    div()
                        .absolute()
                        .left(px(0.0))
                        .top(px(5.0))
                        .w(px(STRIPE_INDICATOR_WIDTH_PX))
                        .h(px(16.0))
                        .rounded(px(1.0))
                        .bg(p.tab_indicator),
                );
            } else {
                item = item.text_color(p.text_muted);
            }
            item.child(tool.stripe_label())
        };

        div()
            .w(px(width_px))
            .h_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_between()
            .py(px(6.0))
            .bg(p.island)
            .border_1()
            .border_color(p.border_island)
            .rounded(px(STATUSBAR_RADIUS_PX))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .children(top.iter().map(|tool| render_item(*tool))),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .children(bottom.iter().map(|tool| render_item(*tool))),
            )
    }

    fn island(&self, region: FocusedRegion, p: Palette) -> Div {
        let focused = self.dock_layout.focused == region;
        let focus_border = self.focus_tiling.density().focus_border_px();
        let base = div()
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(px(ISLAND_RADIUS_PX))
            .bg(p.island);
        if focused {
            base.border(px(focus_border)).border_color(p.border_focus)
        } else {
            base.border_1().border_color(p.border_island)
        }
    }

    fn dock_header(title: &str, detail: String, p: Palette) -> Div {
        div()
            .h(px(DOCK_HEADER_HEIGHT_PX))
            .px(px(12.0))
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .border_b_1()
            .border_color(p.border_subtle)
            .child(
                div()
                    .text_size(px(12.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.text_primary)
                    .child(title.to_owned()),
            )
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(p.text_muted)
                    .child(detail),
            )
    }

    fn splitter(
        id: &'static str,
        vertical: bool,
        target: DragTarget,
        gap: f32,
        cx: &Context<'_, Self>,
    ) -> Stateful<Div> {
        let base = div().id(id).flex_none();
        let base = if vertical {
            base.w(px(gap)).h_full().cursor_col_resize()
        } else {
            base.h(px(gap)).w_full().cursor_row_resize()
        };
        base.on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                this.start_drag(target, event, cx);
            }),
        )
    }

    fn render_left_dock(&self, p: Palette, cx: &Context<'_, Self>) -> Option<Div> {
        let tool = self.dock_layout.left_open?;
        let island = self
            .island(FocusedRegion::ProjectDock, p)
            .w(px(self.dock_layout.left_width))
            .h_full()
            .flex_none()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    this.focus_dock(FocusedRegion::ProjectDock, window, cx);
                }),
            );

        if tool == DockToolId::Commit {
            return Some(
                island
                    .child(Self::dock_header("Commit", String::new(), p))
                    .child(
                        div()
                            .p(px(12.0))
                            .text_size(px(12.0))
                            .text_color(p.text_muted)
                            .child("Git changes (gix) arrive in Phase 3."),
                    ),
            );
        }

        let detail = if self.project_loading {
            "Loading…".to_owned()
        } else {
            format!("{} items", self.project_tree.visible_len())
        };
        let query = self.project_tree.filter_query();
        let (filter_text, filter_color) = if query.is_empty() {
            ("Type to filter tree...".to_owned(), p.text_muted)
        } else {
            (format!("⌕ {query}"), p.text_primary)
        };

        Some(
            island
                .child(Self::dock_header("Project", detail, p))
                .child(
                    uniform_list(
                        "project-tree",
                        self.project_tree.visible_len(),
                        cx.processor(Self::render_tree_rows),
                    )
                    .track_scroll(self.project_scroll.clone())
                    .flex_1()
                    .w_full()
                    .py(px(4.0)),
                )
                .child(
                    div()
                        .h(px(FILTER_BAR_HEIGHT_PX))
                        .flex_none()
                        .mx(px(6.0))
                        .mb(px(6.0))
                        .px(px(8.0))
                        .flex()
                        .items_center()
                        .rounded(px(6.0))
                        .bg(p.input)
                        .border_1()
                        .border_color(p.border_island)
                        .text_size(px(11.0))
                        .text_color(filter_color)
                        .child(filter_text),
                ),
        )
    }

    #[allow(
        clippy::cast_precision_loss,
        reason = "tree depth is a small u16 that converts exactly to f32"
    )]
    fn render_tree_rows(
        &mut self,
        range: Range<usize>,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Vec<Stateful<Div>> {
        let p = Palette::from_theme(cx.global::<KovaTheme>());
        let selected = self.project_tree.selected_index();
        let dock_focused = self.dock_layout.focused == FocusedRegion::ProjectDock;
        let open_path = self
            .panes
            .get(&self.active_pane)
            .and_then(|pane| pane.tabs.active_tab())
            .and_then(|tab| tab.path.clone());

        self.project_tree
            .visible_range(range)
            .map(|(row, node)| {
                let is_selected = row == selected && dock_focused;
                let is_open = open_path.as_deref() == Some(node.path.as_path());
                let indent = 8.0 + f32::from(node.depth) * PROJECT_INDENT_PX;
                let mut el = div()
                    .id(("tree-row", row))
                    .relative()
                    .h(px(PROJECT_ROW_HEIGHT_PX))
                    .w_full()
                    .pl(px(indent))
                    .pr(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .text_size(px(12.0))
                    .hover(|s| s.bg(p.hover))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.click_tree_row(row, window, cx);
                    }));
                if is_selected || is_open {
                    el = el.bg(p.active);
                }
                for level in 0..node.depth {
                    el = el.child(
                        div()
                            .absolute()
                            .top(px(0.0))
                            .bottom(px(0.0))
                            .left(px(8.0 + f32::from(level) * PROJECT_INDENT_PX + 4.0))
                            .w(px(1.0))
                            .bg(p.border_subtle),
                    );
                }
                el.child(tree_icon(node, p)).child(tree_label(node, p))
            })
            .collect()
    }

    fn render_right_dock(&self, p: Palette, cx: &Context<'_, Self>) -> Option<Div> {
        let tool = self.dock_layout.right_open?;
        Some(
            self.island(FocusedRegion::StructureDock, p)
                .w(px(self.dock_layout.right_width))
                .h_full()
                .flex_none()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, window, cx| {
                        this.focus_dock(FocusedRegion::StructureDock, window, cx);
                    }),
                )
                .child(Self::dock_header(
                    tool.title(),
                    format!("{} symbols", self.structure.items().len()),
                    p,
                ))
                .child(
                    uniform_list(
                        "structure-list",
                        self.structure.items().len(),
                        cx.processor(Self::render_structure_rows),
                    )
                    .track_scroll(self.structure_scroll.clone())
                    .flex_1()
                    .w_full()
                    .py(px(4.0)),
                ),
        )
    }

    fn render_structure_rows(
        &mut self,
        range: Range<usize>,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Vec<Stateful<Div>> {
        let p = Palette::from_theme(cx.global::<KovaTheme>());
        let selected = self.structure.selected_index();
        let focused = self.dock_layout.focused == FocusedRegion::StructureDock;
        let items = self.structure.items();
        let end = range.end.min(items.len());
        let start = range.start.min(end);
        items
            .get(start..end)
            .unwrap_or_default()
            .iter()
            .enumerate()
            .map(|(offset, item)| {
                let row = start + offset;
                let mut el = div()
                    .id(("structure-row", row))
                    .h(px(STRUCTURE_ROW_HEIGHT_PX))
                    .w_full()
                    .pl(px(8.0 + f32::from(item.depth) * PROJECT_INDENT_PX))
                    .pr(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .text_size(px(12.0))
                    .hover(|s| s.bg(p.hover))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.jump_to_symbol(row, window, cx);
                    }));
                if row == selected && focused {
                    el = el.bg(p.active);
                }
                el = el
                    .child(
                        div()
                            .w(px(14.0))
                            .text_color(p.tab_indicator)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(kind_icon(item.kind_label)),
                    )
                    .child(div().text_color(p.text_primary).child(item.name.clone()));
                if item.is_entry_point {
                    el = el.child(div().text_color(p.run_icon).child("▶"));
                }
                el.child(
                    div()
                        .ml_auto()
                        .text_size(px(10.0))
                        .text_color(p.text_muted)
                        .child(format!(":{}", item.line)),
                )
            })
            .collect()
    }

    fn render_bottom_panel(&self, p: Palette, cx: &Context<'_, Self>) -> Option<Div> {
        let active = self
            .dock_layout
            .bottom_open
            .and_then(BottomTab::from_tool)?;
        let errors = self.bottom_panel.error_count();
        let warnings = self.bottom_panel.warning_count();

        let tabs = BottomTab::ALL.iter().map(|tab| {
            let tab = *tab;
            let is_active = tab == active;
            let mut el = div()
                .id(SharedString::from(format!("bottom-tab-{}", tab.title())))
                .h_full()
                .px(px(10.0))
                .flex()
                .items_center()
                .gap(px(6.0))
                .text_size(px(12.0))
                .hover(|s| s.bg(p.hover))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.show_bottom_tab(tab, window, cx);
                }));
            el = if is_active {
                el.text_color(p.text_primary)
                    .font_weight(FontWeight::SEMIBOLD)
                    .border_b_2()
                    .border_color(p.tab_indicator)
            } else {
                el.text_color(p.text_muted)
            };
            el = el.child(tab.title());
            if tab == BottomTab::Problems && errors + warnings > 0 {
                let (variant_color, count) = if errors > 0 {
                    (p.error, errors)
                } else {
                    (p.warning, warnings)
                };
                el = el.child(
                    div()
                        .px(px(6.0))
                        .rounded(px(8.0))
                        .border_1()
                        .border_color(variant_color)
                        .text_size(px(10.0))
                        .text_color(variant_color)
                        .child(count.to_string()),
                );
            }
            el
        });

        let body = self.render_bottom_body(active, p, cx);

        Some(
            self.island(FocusedRegion::BottomPanel, p)
                .h(px(self.dock_layout.bottom_height))
                .w_full()
                .flex_none()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, window, cx| {
                        this.focus_dock(FocusedRegion::BottomPanel, window, cx);
                    }),
                )
                .child(
                    div()
                        .h(px(BOTTOM_TAB_BAR_HEIGHT_PX))
                        .flex_none()
                        .px(px(4.0))
                        .flex()
                        .items_center()
                        .border_b_1()
                        .border_color(p.border_subtle)
                        .children(tabs),
                )
                .child(body),
        )
    }

    fn render_bottom_body(
        &self,
        active: BottomTab,
        p: Palette,
        cx: &Context<'_, Self>,
    ) -> AnyElement {
        match active {
            BottomTab::Problems if self.bottom_panel.problems().is_empty() => {
                placeholder("No problems found in the active file.", p).into_any_element()
            }
            BottomTab::Problems => uniform_list(
                "problems-list",
                self.bottom_panel.grouped_rows().len(),
                cx.processor(Self::render_problem_rows),
            )
            .track_scroll(self.problems_scroll.clone())
            .flex_1()
            .w_full()
            .into_any_element(),
            BottomTab::Run if self.bottom_panel.run_logs.is_empty() => placeholder(
                "No runs yet. ▶ Run entry points from the gutter (KOVA-027); Gradle/JUnit arrive in Phase 3.",
                p,
            )
            .into_any_element(),
            BottomTab::Run => div()
                .flex_1()
                .p(px(10.0))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .text_size(px(12.0))
                .text_color(p.text_secondary)
                .children(self.bottom_panel.run_logs.iter().map(|line| div().child(line.clone())))
                .into_any_element(),
            BottomTab::Terminal => match &self.terminal {
                Some((dock, _)) => div()
                    .flex_1()
                    .min_h(px(0.0))
                    .child(dock.clone())
                    .into_any_element(),
                None => placeholder("Starting terminal…", p).into_any_element(),
            },
        }
    }

    fn render_problem_rows(
        &mut self,
        range: Range<usize>,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Vec<Stateful<Div>> {
        let p = Palette::from_theme(cx.global::<KovaTheme>());
        let selected = self.bottom_panel.selected_problem_index();
        let focused = self.dock_layout.focused == FocusedRegion::BottomPanel;
        let rows = self.bottom_panel.grouped_rows();
        let end = range.end.min(rows.len());
        let start = range.start.min(end);
        rows.get(start..end)
            .unwrap_or_default()
            .iter()
            .enumerate()
            .map(|(offset, row)| {
                let el = div()
                    .id(("problem-row", start + offset))
                    .h(px(22.0))
                    .w_full()
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .text_size(px(12.0));
                match row {
                    ProblemRow::FileHeader { file, count } => el
                        .text_color(p.text_primary)
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(file_title(Path::new(file)))
                        .child(
                            div()
                                .text_color(p.text_muted)
                                .font_weight(FontWeight::NORMAL)
                                .child(format!("{count} problem(s) · {file}")),
                        ),
                    ProblemRow::Entry(index, entry) => {
                        let index = *index;
                        let (glyph, color) = match entry.severity {
                            ProblemSeverity::Error => ("●", p.error),
                            ProblemSeverity::Warning => ("▲", p.warning),
                        };
                        let mut el = el
                            .pl(px(24.0))
                            .hover(|s| s.bg(p.hover))
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.jump_to_problem(index, window, cx);
                            }))
                            .child(div().text_color(color).child(glyph))
                            .child(
                                div()
                                    .text_color(p.text_secondary)
                                    .child(entry.message.clone()),
                            )
                            .child(
                                div()
                                    .text_color(p.text_muted)
                                    .child(format!(":{}:{}", entry.line, entry.col)),
                            );
                        if index == selected && focused {
                            el = el.bg(p.active);
                        }
                        el
                    }
                }
            })
            .collect()
    }

    fn render_split(&self, node: &SplitTree, p: Palette, cx: &Context<'_, Self>) -> AnyElement {
        match node {
            SplitTree::Leaf(pane) => self.render_pane(*pane, p, cx).into_any_element(),
            SplitTree::Split {
                axis,
                ratio,
                left,
                right,
            } => {
                let ratio = SplitTree::clamp_ratio(*ratio);
                let first = div()
                    .flex()
                    .min_w(px(0.0))
                    .min_h(px(0.0))
                    .child(self.render_split(left, p, cx));
                let second = div()
                    .flex()
                    .flex_1()
                    .min_w(px(0.0))
                    .min_h(px(0.0))
                    .child(self.render_split(right, p, cx));
                let container = div()
                    .flex()
                    .flex_1()
                    .size_full()
                    .min_w(px(0.0))
                    .min_h(px(0.0))
                    .gap(px(self.focus_tiling.density().island_gap_px()));
                match axis {
                    SplitAxis::Horizontal => container
                        .flex_row()
                        .child(first.w(relative(ratio)).h_full())
                        .child(second.h_full()),
                    SplitAxis::Vertical => container
                        .flex_col()
                        .child(first.h(relative(ratio)).w_full())
                        .child(second.w_full()),
                }
                .into_any_element()
            }
        }
    }

    fn render_pane(&self, pane_id: PaneId, p: Palette, cx: &Context<'_, Self>) -> Div {
        let is_active = pane_id == self.active_pane;
        let editor_focused = self.dock_layout.focused == FocusedRegion::EditorPane(pane_id);
        let editor_bg = hsla_from_rgba(cx.global::<KovaTheme>().surfaces().editor);
        let base = div()
            .flex_1()
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(px(ISLAND_RADIUS_PX))
            .bg(editor_bg);
        let base = if is_active && editor_focused {
            base.border(px(self.focus_tiling.density().focus_border_px()))
                .border_color(p.border_focus)
        } else {
            base.border_1().border_color(p.border_island)
        };

        let Some(pane) = self.panes.get(&pane_id) else {
            return base;
        };
        let tab_bar = Self::render_tab_bar(pane_id, pane, p, cx);

        let content: AnyElement = match pane.active_editor() {
            Some(editor) => div()
                .flex_1()
                .w_full()
                .min_h(px(0.0))
                .child(editor.clone())
                .into_any_element(),
            None => placeholder("No open files — pick one in the Project dock (Alt+1).", p)
                .into_any_element(),
        };

        base.child(tab_bar).child(content)
    }

    /// Renders one editor tab: language glyph, title and dirty `●` / close `×` badge.
    fn render_tab(
        pane_id: PaneId,
        tab: &TabItem,
        is_tab_active: bool,
        compact: bool,
        p: Palette,
        cx: &Context<'_, Self>,
    ) -> Stateful<Div> {
        let tab_id = tab.id;
        let mut el = div()
            .id(SharedString::from(format!(
                "tab-{}-{}",
                pane_id.0, tab_id.0
            )))
            .h_full()
            .flex_none()
            .px(px(12.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .text_size(px(12.0))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.select_tab(pane_id, tab_id, window, cx);
            }));
        el = if is_tab_active {
            el.bg(p.tab_active)
                .text_color(p.text_primary)
                .font_weight(FontWeight::SEMIBOLD)
                .border_b_2()
                .border_color(p.tab_indicator)
        } else {
            el.text_color(p.text_muted).hover(|s| s.bg(p.hover))
        };
        let badge = div()
            .id(SharedString::from(format!(
                "tab-close-{}-{}",
                pane_id.0, tab_id.0
            )))
            .w(px(12.0))
            .flex()
            .justify_center()
            .rounded(px(3.0))
            .hover(|s| s.bg(p.hover))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                cx.stop_propagation();
                this.close_tab(pane_id, tab_id, window, cx);
            }));
        let badge = if tab.is_dirty {
            badge.text_color(p.tab_indicator).child("●")
        } else {
            badge.text_color(p.text_muted).child("×")
        };
        el.child(
            div().text_color(p.tab_indicator).text_size(px(10.0)).child(
                tab.path
                    .as_deref()
                    .map_or("·", |path| lang_hint_for(&path.to_string_lossy())),
            ),
        )
        .child(tab.display_title(compact))
        .child(badge)
    }

    /// Renders the 34 px tab strip of one split pane with breadcrumbs and split actions.
    fn render_tab_bar(
        pane_id: PaneId,
        pane: &PaneState,
        p: Palette,
        cx: &Context<'_, Self>,
    ) -> Div {
        let compact = pane.tabs.compact_titles();
        let active_tab = pane.tabs.active_tab_id();
        let tabs =
            pane.tabs.tabs().iter().map(|tab| {
                Self::render_tab(pane_id, tab, active_tab == Some(tab.id), compact, p, cx)
            });

        let breadcrumbs = pane
            .tabs
            .active_tab()
            .and_then(|tab| tab.path.as_deref())
            .map(breadcrumb_label)
            .unwrap_or_default();
        let action = |id: &'static str, glyph: &'static str, axis: SplitAxis| {
            div()
                .id(SharedString::from(format!("{id}-{}", pane_id.0)))
                .w(px(22.0))
                .h(px(22.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.0))
                .text_color(p.text_muted)
                .hover(|s| s.bg(p.hover))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.set_active_pane(pane_id, cx);
                    this.split_active_pane(axis, window, cx);
                }))
                .child(glyph)
        };

        div()
            .h(px(TAB_BAR_HEIGHT_PX))
            .w_full()
            .flex_none()
            .flex()
            .items_center()
            .justify_between()
            .bg(hsla_from_rgba(
                cx.global::<KovaTheme>().surfaces().tab_inactive,
            ))
            .border_b_1()
            .border_color(p.border_subtle)
            .child(
                div()
                    .id(SharedString::from(format!("tab-strip-{}", pane_id.0)))
                    .h_full()
                    .flex()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_x_scroll()
                    .children(tabs),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(4.0))
                    .pr(px(8.0))
                    .child(
                        div()
                            .pr(px(6.0))
                            .text_size(px(11.0))
                            .text_color(p.text_muted)
                            .child(breadcrumbs),
                    )
                    .child(action("split-right", "◫", SplitAxis::Horizontal))
                    .child(action("split-down", "⬓", SplitAxis::Vertical)),
            )
    }

    /// Builds the status-bar model for the active pane, tab, cursor and focused island.
    fn current_status(&self, cx: &Context<'_, Self>) -> StatusBarState {
        let active_tab = self
            .panes
            .get(&self.active_pane)
            .and_then(|pane| pane.tabs.active_tab());
        let (file, language) = active_tab.map_or_else(
            || ("—".to_owned(), "Text".to_owned()),
            |tab| (tab.title.clone(), tab.language_label.clone()),
        );
        let mut status = self.status_bar.clone();
        status.focused_region = self.dock_layout.focused;
        status.active_file = file;
        status.language_label = language;
        if let Some(editor) = self.active_editor() {
            let editor = editor.read(cx);
            let (line, col) = editor.primary_cursor_position();
            status.cursor_line = line;
            status.cursor_col = col;
            status.cursor_count = editor.cursor_count();
            if self.vim_enabled {
                let vim = editor.vim_status();
                status.vim_mode = VimModeBadge::from_status(vim.as_ref());
                status.vim_sequence = vim.map(|v| v.pending).unwrap_or_default();
            }
        }
        if self.ex_line.is_some() {
            status.vim_mode = VimModeBadge::CommandLine;
        }
        if let Some(hint) = &self.chord_hint {
            status.vim_sequence.clone_from(hint);
        }
        status
    }

    /// Compact Ex / search prompt (`surfaces.input` with a `borders.focus` ring).
    fn render_ex_line(line: &ExCommandLine, p: Palette, cx: &Context<'_, Self>) -> Div {
        let theme = cx.global::<KovaTheme>();
        let (before, after) = line.split_at_cursor();
        div()
            .h(px(20.0))
            .min_w(px(260.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .rounded(px(4.0))
            .bg(hsla_from_rgba(theme.surfaces().input))
            .border_1()
            .border_color(p.border_focus)
            .text_color(p.text_primary)
            .font_family("JetBrains Mono")
            .child(format!("{}{before}", line.prefix()))
            .child(
                div()
                    .w(px(2.0))
                    .h(px(14.0))
                    .bg(hsla_from_rgba(theme.editor().cursor)),
            )
            .child(after.to_owned())
    }

    /// Layout label: `Focus` / `Zen` while Focus Mode hides docks, otherwise the density.
    fn layout_label(&self) -> &'static str {
        match self.focus_tiling.focus_variant() {
            Some(FocusModeVariant::HideDocks) => "Focus",
            Some(FocusModeVariant::Zen) => "Zen",
            None => self.focus_tiling.density().label(),
        }
    }

    /// Renders the Vim mode pill (`NORMAL` / `INSERT` / `VISUAL` / `COMMAND`, `EDIT` when off).
    fn vim_pill(status: &StatusBarState) -> Div {
        let (bg, border, fg) = status.vim_mode.variant().colors_rgb();
        let vim_label = if status.vim_mode == VimModeBadge::Off {
            "EDIT"
        } else {
            status.vim_mode.label()
        };
        div()
            .h(px(18.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .rounded(px(4.0))
            .bg(rgb_hex(bg))
            .border_1()
            .border_color(rgb_hex(border))
            .text_size(px(10.0))
            .font_weight(FontWeight::BOLD)
            .text_color(rgb_hex(fg))
            .child(vim_label)
    }

    /// Left part of the status bar: Vim pill plus either the Ex line or the file context.
    fn render_status_left(
        &self,
        status: &StatusBarState,
        vim_pill: Div,
        p: Palette,
        cx: &Context<'_, Self>,
    ) -> Div {
        let left = div().flex().items_center().gap(px(12.0)).child(vim_pill);
        if let Some(line) = &self.ex_line {
            return left.child(Self::render_ex_line(line, p, cx));
        }
        let (doc_status_kind, doc_status_text) = self.document_status();
        let kind_color = |kind: StatusKind| match kind {
            StatusKind::Info => p.text_secondary,
            StatusKind::Warning => p.warning,
            StatusKind::Error => p.error,
        };
        let config_notice = self
            .config_notice
            .clone()
            .map(|(kind, text)| div().text_color(kind_color(kind)).child(text));
        left.child(
            div()
                .text_color(p.text_secondary)
                .child(format!("git: {}", status.git_branch)),
        )
        .child(
            div()
                .text_color(p.text_primary)
                .child(status.active_file.clone()),
        )
        .child(
            div()
                .text_color(p.text_muted)
                .child(status.focused_region.label()),
        )
        .child(
            div()
                .text_color(kind_color(doc_status_kind))
                .child(doc_status_text),
        )
        .children(config_notice)
    }

    fn render_statusbar(&self, p: Palette, cx: &Context<'_, Self>) -> Div {
        let status = self.current_status(cx);
        let vim_pill = Self::vim_pill(&status);
        let fps_color = if status.vim_mode.variant() == BadgeVariant::Accent {
            p.tab_indicator
        } else {
            p.run_icon
        };
        let start_color = match self.metrics.cold_start_within_budget {
            Some(true) => p.run_icon,
            Some(false) => p.warning,
            None => p.text_muted,
        };

        div()
            .h(px(STATUSBAR_HEIGHT_PX))
            .w_full()
            .flex_none()
            .px(px(8.0))
            .flex()
            .items_center()
            .justify_between()
            .rounded(px(STATUSBAR_RADIUS_PX))
            .border_1()
            .border_color(p.border_island)
            .bg(hsla_from_rgba(
                cx.global::<KovaTheme>().surfaces().statusbar,
            ))
            .text_size(px(11.0))
            .child(self.render_status_left(&status, vim_pill, p, cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(14.0))
                    .child(
                        div()
                            .text_color(p.text_secondary)
                            .child(status.vim_sequence.clone()),
                    )
                    .child(div().text_color(p.text_muted).child(self.layout_label()))
                    .child(
                        div()
                            .text_color(p.text_primary)
                            .child(status.format_cursor_position()),
                    )
                    .child(
                        div()
                            .text_color(p.text_secondary)
                            .child(status.indent_label.clone()),
                    )
                    .child(
                        div()
                            .text_color(p.text_secondary)
                            .child(status.language_label.clone()),
                    )
                    .child(
                        div()
                            .text_color(start_color)
                            .child(self.metrics.format_start_label()),
                    )
                    .child(div().text_color(fps_color).child(status.fps_label)),
            )
    }

    fn render_workspace(&self, p: Palette, cx: &Context<'_, Self>) -> Div {
        let gap = self.focus_tiling.density().island_gap_px();
        let stripes = !self.focus_tiling.hides_stripes();
        let mut center = div()
            .flex_1()
            .h_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_1()
                    .w_full()
                    .min_h(px(0.0))
                    .flex()
                    .child(self.render_split(&self.split_tree, p, cx)),
            );
        if let Some(bottom) = self.render_bottom_panel(p, cx) {
            center = center
                .child(Self::splitter(
                    "bottom-splitter",
                    false,
                    DragTarget::BottomPanel,
                    gap,
                    cx,
                ))
                .child(bottom);
        }

        let mut row = div().flex_1().w_full().min_h(px(0.0)).flex().flex_row();
        if stripes {
            row = row
                .child(self.render_stripe(
                    LEFT_STRIPE_WIDTH_PX,
                    &[DockToolId::Project, DockToolId::Commit, DockToolId::Run],
                    &[DockToolId::Problems, DockToolId::Terminal],
                    p,
                    cx,
                ))
                .child(div().w(px(gap)).flex_none());
        }
        if let Some(left) = self.render_left_dock(p, cx) {
            row = row.child(left).child(Self::splitter(
                "left-splitter",
                true,
                DragTarget::LeftDock,
                gap,
                cx,
            ));
        }
        row = row.child(center);
        if let Some(right) = self.render_right_dock(p, cx) {
            row = row
                .child(Self::splitter(
                    "right-splitter",
                    true,
                    DragTarget::RightDock,
                    gap,
                    cx,
                ))
                .child(right);
        }
        if stripes {
            row = row
                .child(div().w(px(gap)).flex_none())
                .child(self.render_stripe(
                    RIGHT_STRIPE_WIDTH_PX,
                    &[DockToolId::Structure],
                    &[],
                    p,
                    cx,
                ));
        }
        row
    }
}

impl Render for WorkspaceShell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        if !self.first_frame_scheduled {
            self.first_frame_scheduled = true;
            cx.defer_in(window, Self::complete_first_frame);
        }

        let title = self.active_window_title();
        if title != self.window_title {
            window.set_window_title(&title);
            self.window_title = title;
        }

        if self.terminal.is_none() && self.dock_layout.bottom_open == Some(DockToolId::Terminal) {
            self.ensure_terminal(window, cx);
        }

        let viewport = window.viewport_size();
        self.viewport_px = (f32::from(viewport.width), f32::from(viewport.height));
        let density = self.focus_tiling.density();
        let p = Palette::from_theme(cx.global::<KovaTheme>());

        let settings_overlay = if self.settings_open {
            self.settings_view
                .as_ref()
                .map(|view| view.render(cx.global::<KovaTheme>(), self.viewport_px, cx))
        } else {
            None
        };
        let mut workspace = div()
            .flex_1()
            .w_full()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .px(px(density.outer_margin_px()))
            .pb(px(density.outer_margin_px()))
            .gap(px(density.island_gap_px()))
            .child(self.render_workspace(p, cx));
        if self.show_status_bar {
            workspace = workspace.child(self.render_statusbar(p, cx));
        }

        let root = div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(hsla_from_rgba(cx.global::<KovaTheme>().surfaces().canvas))
            .text_color(p.text_primary)
            .key_context(WORKSPACE_KEY_CONTEXT)
            .on_action(cx.listener(Self::save_action))
            .track_focus(&self.focus_handle)
            .capture_key_down(cx.listener(Self::handle_key_capture))
            .on_key_down(cx.listener(Self::handle_key_down))
            .on_mouse_move(cx.listener(Self::handle_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::handle_mouse_up))
            .child(self.render_titlebar(p))
            .child(workspace);
        match settings_overlay {
            Some(overlay) => root.child(overlay),
            None => root,
        }
    }
}

/// `(font_family, font_size_px)` of the editor from `settings`.
fn editor_font(settings: &KovaSettings) -> (String, f32) {
    (
        settings.editor.font_family.clone(),
        f64_to_f32(settings.editor.font_size),
    )
}

/// Narrows a validated `settings.toml` dimension (always far inside `f32` range) to `f32`.
#[allow(
    clippy::cast_possible_truncation,
    reason = "settings dimensions are validated to small ranges (6..=800)"
)]
const fn f64_to_f32(value: f64) -> f32 {
    value as f32
}

/// Keys that leave Vim `INSERT` mode (`Escape`, `Ctrl+[`, `Ctrl+C`).
fn is_vim_insert_exit(keystroke: &gpui::Keystroke) -> bool {
    let m = &keystroke.modifiers;
    if m.alt || m.platform {
        return false;
    }
    match keystroke.key.as_str() {
        "escape" => !m.control,
        "[" | "c" => m.control && !m.shift,
        _ => false,
    }
}

/// Line comment prefix for `Cmd+/` by file type (`#` for TOML, YAML, properties, shell and
/// `.http` files; `//` otherwise).
fn line_comment_prefix(path: Option<&Path>) -> &'static str {
    let ext = path
        .and_then(Path::extension)
        .and_then(|ext| ext.to_str())
        .unwrap_or_default();
    match ext {
        "toml" | "yaml" | "yml" | "properties" | "sh" | "bash" | "zsh" | "http" | "rest" | "py"
        | "gitignore" => "#",
        _ => "//",
    }
}

fn placeholder(text: &'static str, p: Palette) -> Div {
    div()
        .flex_1()
        .w_full()
        .p(px(12.0))
        .text_size(px(12.0))
        .text_color(p.text_muted)
        .child(text)
}

fn tree_icon(node: &ProjectNode, p: Palette) -> Div {
    let color = match &node.kind {
        ProjectNodeKind::Directory { .. } => p.text_muted,
        ProjectNodeKind::File { lang_hint } => match lang_hint.as_str() {
            "J" => p.tab_indicator,
            "T" => p.run_icon,
            "K" | "⚙" => p.warning,
            "H" => p.error,
            _ => p.text_secondary,
        },
    };
    div()
        .w(px(14.0))
        .flex_none()
        .text_size(px(10.0))
        .font_weight(FontWeight::BOLD)
        .text_color(color)
        .child(node.icon().to_owned())
}

fn tree_label(node: &ProjectNode, p: Palette) -> Div {
    let label = div().text_color(p.text_primary).child(node.name.clone());
    if node.is_kova_config {
        div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(label.font_weight(FontWeight::SEMIBOLD))
            .child(
                div()
                    .px(px(4.0))
                    .rounded(px(3.0))
                    .border_1()
                    .border_color(p.warning)
                    .text_size(px(9.0))
                    .text_color(p.warning)
                    .child("project config"),
            )
    } else {
        label
    }
}

fn file_title(path: &Path) -> String {
    path.file_name()
        .map_or_else(|| path.to_string_lossy(), |name| name.to_string_lossy())
        .into_owned()
}

/// Returns the status bar language label for `path` (`Java`, `Kotlin`, `HTTP`, `TOML`, `JSON`).
fn language_label(path: &Path) -> &'static str {
    match detect_language(path) {
        Some(SyntaxLanguage::Java) => "Java",
        Some(SyntaxLanguage::Kotlin) => "Kotlin",
        Some(SyntaxLanguage::Http) => "HTTP",
        Some(SyntaxLanguage::Toml) => "TOML",
        Some(SyntaxLanguage::Json) => "JSON",
        None => "Text",
    }
}

/// Builds the discrete breadcrumb label (`src › main › java › …`) for the tab bar.
fn breadcrumb_label(path: &Path) -> String {
    let parts: Vec<String> = path
        .parent()
        .into_iter()
        .flat_map(Path::components)
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if parts.len() > 3 {
        let tail = &parts[parts.len() - 2..];
        format!("{} › … › {}", parts[0], tail.join(" › "))
    } else {
        parts.join(" › ")
    }
}

/// Extracts the branch name from the contents of `.git/HEAD`.
fn parse_git_head(head: &str) -> Option<String> {
    let head = head.trim();
    if let Some(reference) = head.strip_prefix("ref: ") {
        return reference.rsplit('/').next().map(str::to_owned);
    }
    head.get(..7).map(|sha| format!("{sha} (detached)"))
}

/// Parses `snapshot` with `kova_syntax` and returns the flattened outline plus syntax problems.
///
/// Runs on the GPUI background executor, never on the render thread.
fn analyze_document(
    engine: Option<Arc<SyntaxEngine>>,
    language: SyntaxLanguage,
    snapshot: &BufferSnapshot,
    file_label: &str,
) -> AnalysisResult {
    let engine = engine.or_else(|| {
        let registry = LanguageRegistry::with_defaults().ok()?;
        let config = SyntaxEngineConfig {
            worker_threads: 1,
            ..SyntaxEngineConfig::default()
        };
        SyntaxEngine::new(registry, config).ok().map(Arc::new)
    });
    let Some(active_engine) = engine.as_ref() else {
        return AnalysisResult {
            engine: None,
            structure: StructureModel::default(),
            problems: Vec::new(),
            syntax: None,
        };
    };
    let Ok(tree) = active_engine.parse_sync(language, snapshot, None) else {
        return AnalysisResult {
            engine,
            structure: StructureModel::default(),
            problems: Vec::new(),
            syntax: None,
        };
    };
    let outline = kova_syntax::extract_structure(&tree, snapshot);
    let structure = StructureModel::from_outline(&outline);

    let problems = kova_syntax::collect_syntax_diagnostics(&tree, MAX_SYNTAX_PROBLEMS)
        .into_iter()
        .map(|diagnostic| ProblemEntry {
            file: file_label.to_owned(),
            line: diagnostic.line + 1,
            col: diagnostic.col + 1,
            severity: ProblemSeverity::Error,
            message: diagnostic.message,
        })
        .collect();
    let highlighter = Highlighter::new(active_engine.registry().clone());
    let syntax = Some(Arc::new(EditorSyntax::build(
        tree,
        snapshot.clone(),
        highlighter,
    )));
    AnalysisResult {
        engine,
        structure,
        problems,
        syntax,
    }
}

/// In-memory demo project (`kova-orders`) shown until a real project directory is scanned.
fn demo_project_nodes() -> Vec<ProjectNode> {
    let entries: [(&str, u16, bool); 18] = [
        ("src", 0, true),
        ("src/main", 1, true),
        ("src/main/java", 2, true),
        ("src/main/java/dev", 3, true),
        ("src/main/java/dev/kova", 4, true),
        ("src/main/java/dev/kova/demo", 5, true),
        (WELCOME_DOCUMENT_PATH, 6, false),
        ("src/main/java/dev/kova/demo/OrderApp.java", 6, false),
        ("src/main/java/dev/kova/demo/OrderService.java", 6, false),
        ("src/test", 1, true),
        ("src/test/java", 2, true),
        ("src/test/java/OrderServiceTest.java", 3, false),
        ("http", 0, true),
        ("http/orders.http", 1, false),
        ("build", 0, true),
        ("build.gradle.kts", 0, false),
        ("settings.gradle.kts", 0, false),
        ("kova.toml", 0, false),
    ];
    entries
        .iter()
        .enumerate()
        .map(|(idx, (path, depth, is_dir))| {
            let path_buf = PathBuf::from(path);
            let name = file_title(&path_buf);
            ProjectNode {
                node_id: idx as u64 + 1,
                kind: if *is_dir {
                    ProjectNodeKind::Directory {
                        expanded: name != "build",
                    }
                } else {
                    ProjectNodeKind::File {
                        lang_hint: lang_hint_for(&name).to_owned(),
                    }
                },
                is_kova_config: name == "kova.toml",
                path: path_buf,
                name,
                depth: *depth,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_metrics_formats_summary_and_evaluates_budgets() {
        let mut metrics = StartupMetrics::new();
        assert_eq!(metrics.format_start_label(), "Start: -- ms");
        assert_eq!(metrics.format_rss_label(), "-- MiB");
        assert!(!metrics.is_within_budgets());

        metrics.record_first_frame(Duration::from_millis(48), Some(42 * 1024 * 1024));
        assert_eq!(metrics.cold_start_within_budget, Some(true));
        assert_eq!(metrics.rss_within_budget, Some(true));
        assert!(metrics.is_within_budgets());
        assert_eq!(metrics.format_start_label(), "Start: 48 ms");
        assert_eq!(metrics.format_rss_label(), "42.0 MiB");
        assert_eq!(
            metrics.format_summary(),
            format!("Start: 48 ms | 42.0 MiB | {}", metrics.backend_name)
        );
        let probe = metrics.format_probe_line();
        assert!(probe.contains("cold_start_ms=48.00"));
        assert!(probe.contains("rss_bytes=44040192"));
        assert!(probe.contains("within_budget=true"));
    }

    #[test]
    fn startup_metrics_flags_budget_violations() {
        let mut metrics = StartupMetrics::new();
        metrics.record_first_frame(Duration::from_millis(120), Some(150 * 1024 * 1024));
        assert_eq!(metrics.cold_start_within_budget, Some(false));
        assert_eq!(metrics.rss_within_budget, Some(false));
        assert!(!metrics.is_within_budgets());

        let mut missing_rss = StartupMetrics::new();
        missing_rss.record_first_frame(Duration::from_millis(50), None);
        assert_eq!(missing_rss.cold_start_within_budget, Some(true));
        assert_eq!(missing_rss.rss_within_budget, None);
        assert!(!missing_rss.is_within_budgets());
    }

    #[test]
    fn helpers_format_branch_breadcrumbs_and_languages() {
        assert_eq!(
            parse_git_head(
                "ref: refs/heads/feat/KOVA-025
"
            )
            .as_deref(),
            Some("KOVA-025")
        );
        assert_eq!(
            parse_git_head("0123456789abcdef").as_deref(),
            Some("0123456 (detached)")
        );
        assert_eq!(
            breadcrumb_label(Path::new("src/main/java/dev/App.java")),
            "src › … › java › dev"
        );
        assert_eq!(breadcrumb_label(Path::new("http/orders.http")), "http");
        assert_eq!(language_label(Path::new("kova.toml")), "TOML");
        assert_eq!(language_label(Path::new("README.md")), "Text");
        assert_eq!(file_title(Path::new("a/b/C.java")), "C.java");
    }

    #[test]
    fn demo_project_has_no_idea_dir_and_highlights_kova_toml() {
        let tree = ProjectTreeModel::from_nodes(demo_project_nodes());
        let rows = tree.visible_rows();
        assert!(
            rows.iter()
                .any(|n| n.path == Path::new(WELCOME_DOCUMENT_PATH))
        );
        assert!(rows.iter().any(|n| n.is_kova_config));
        assert!(rows.iter().all(|n| n.name != ".idea"));
    }

    #[test]
    fn analysis_extracts_structure_and_syntax_problems_off_thread_ready() {
        let snapshot = Buffer::from_text(
            "public class Broken {
    void run( {
}
",
        )
        .snapshot();
        let result = analyze_document(None, SyntaxLanguage::Java, &snapshot, "Broken.java");
        assert!(result.engine.is_some());
        assert!(result.structure.items().iter().any(|s| s.name == "Broken"));
        assert_ne!(result.problems.len(), 0);
        assert!(
            result
                .problems
                .iter()
                .all(|p| p.file == "Broken.java" && p.line >= 1)
        );

        let ok = Buffer::from_text(
            "class Ok { void run() {} }
",
        )
        .snapshot();
        let reused = analyze_document(result.engine, SyntaxLanguage::Java, &ok, "Ok.java");
        assert_eq!(reused.problems.len(), 0);
        assert!(reused.structure.items().iter().any(|s| s.name == "run"));
    }
}
