//! One terminal session rendered with GPUI.
//!
//! The session is started on the background executor (JDK discovery touches the disk), so the
//! UI thread never blocks on `fork` or on PTY reads. A frame-paced poll (~8 ms, 120 FPS) checks
//! the session's coalesced *dirty* flag and repaints only when new output arrived.

use std::path::PathBuf;
use std::time::Duration;

use gpui::{
    App, Bounds, ClipboardItem, Context, EventEmitter, FocusHandle, Font, FontFallbacks,
    FontFeatures, FontStyle, FontWeight, Hsla, InteractiveElement as _, IntoElement, KeyDownEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _, Pixels, Point,
    Render, ScrollDelta, ScrollWheelEvent, SharedString, StrikethroughStyle, Styled as _,
    StyledText, Task, TextRun, UnderlineStyle, Window, canvas, div, px,
};
use kova_config::KovaSettings;
use kova_theme::TerminalPalette;
use kova_tools::terminal::{
    KeyModifiers, PtySession, PtySpawnConfig, SnapshotCell, TermMode, TermState, TerminalEnv,
    TerminalError, TerminalSize, TerminalSnapshot, encode_key, encode_paste, encode_scroll,
    resolve_background, resolve_foreground, resolve_project_java_home,
};

use crate::theme::{KovaTheme, hsla_from_rgba};

/// Repaint cadence for new PTY output (one frame at 120 FPS).
pub const FRAME_INTERVAL: Duration = Duration::from_millis(8);

/// Inner padding around the character grid in logical pixels.
const GRID_PADDING_PX: f32 = 6.0;

/// Line height as a multiple of the font size.
const LINE_HEIGHT_FACTOR: f32 = 1.3;

/// Everything needed to start a terminal session.
#[derive(Debug, Clone)]
pub struct TerminalLaunch {
    /// Working directory: the open project root.
    pub cwd: Option<PathBuf>,
    /// `terminal.shell` override; `None` uses `$SHELL`.
    pub shell: Option<PathBuf>,
    /// `terminal.scrollback_lines`.
    pub scrollback: usize,
    /// `terminal.inject_java_home`.
    pub inject_java_home: bool,
    /// User settings merged with `kova.toml` to select the project JDK.
    pub settings: KovaSettings,
    /// Monospace font family (the editor font).
    pub font_family: String,
    /// `terminal.font_size` in logical pixels.
    pub font_size: f32,
}

impl TerminalLaunch {
    /// Builds launch options for `project_root` from `settings`.
    #[must_use]
    pub fn from_settings(project_root: Option<PathBuf>, settings: &KovaSettings) -> Self {
        let terminal = &settings.terminal;
        Self {
            cwd: project_root,
            shell: terminal
                .shell
                .as_deref()
                .filter(|shell| !shell.trim().is_empty())
                .map(PathBuf::from),
            scrollback: terminal.scrollback_lines,
            inject_java_home: terminal.inject_java_home,
            settings: settings.clone(),
            font_family: settings.editor.font_family.clone(),
            font_size: narrow(terminal.font_size),
        }
    }

    /// Resolves the JDK and builds the PTY spawn configuration (touches the disk).
    fn spawn_config(&self) -> (PtySpawnConfig, Option<PathBuf>) {
        let java_home = if self.inject_java_home {
            self.cwd
                .as_deref()
                .and_then(|root| resolve_project_java_home(root, &self.settings))
        } else {
            None
        };
        let env = TerminalEnv::from_process(java_home.as_deref());
        let mut config = PtySpawnConfig::shell(self.cwd.as_deref(), env);
        if let Some(shell) = &self.shell {
            config.program.clone_from(shell);
        }
        config.scrollback = self.scrollback;
        (config, java_home)
    }
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "font sizes are validated to 6..=72"
)]
const fn narrow(value: f64) -> f32 {
    value as f32
}

/// Events emitted by a [`TerminalView`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalViewEvent {
    /// The program exited (e.g. `exit` typed in the shell).
    Exited,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Status {
    Starting,
    Running,
    Failed(String),
    Exited,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct CellMetrics {
    width: f32,
    line_height: f32,
}

/// A terminal session view: grid rendering, input, selection, clipboard and resize.
pub struct TerminalView {
    session: Option<PtySession>,
    status: Status,
    focus_handle: FocusHandle,
    font_family: String,
    font_size: f32,
    metrics: Option<CellMetrics>,
    grid_size: TerminalSize,
    grid_bounds: Option<Bounds<Pixels>>,
    selecting: bool,
    scroll_remainder: f32,
    java_home: Option<PathBuf>,
    _spawn: Task<()>,
    _poll: Task<()>,
}

impl std::fmt::Debug for TerminalView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalView")
            .field("status", &self.status)
            .field("grid_size", &self.grid_size)
            .field("java_home", &self.java_home)
            .finish_non_exhaustive()
    }
}

impl EventEmitter<TerminalViewEvent> for TerminalView {}

impl TerminalView {
    /// Starts a session for `launch` off the UI thread and begins frame-paced polling.
    pub fn new(launch: &TerminalLaunch, cx: &mut Context<'_, Self>) -> Self {
        let executor = cx.background_executor().clone();
        let spawn_launch = launch.clone();
        let spawn = cx.spawn(async move |this, cx| {
            let result = executor
                .spawn(async move {
                    let (config, java_home) = spawn_launch.spawn_config();
                    PtySession::spawn(&config).map(|session| (session, java_home))
                })
                .await;
            this.update(cx, |view, cx| view.attach(result, cx)).ok();
        });
        let poll_executor = cx.background_executor().clone();
        let poll = cx.spawn(async move |this, cx| {
            loop {
                poll_executor.timer(FRAME_INTERVAL).await;
                if this.update(cx, Self::poll).is_err() {
                    break;
                }
            }
        });
        Self {
            session: None,
            status: Status::Starting,
            focus_handle: cx.focus_handle(),
            font_family: launch.font_family.clone(),
            font_size: launch.font_size,
            metrics: None,
            grid_size: TerminalSize::default(),
            grid_bounds: None,
            selecting: false,
            scroll_remainder: 0.0,
            java_home: None,
            _spawn: spawn,
            _poll: poll,
        }
    }

    fn attach(
        &mut self,
        result: Result<(PtySession, Option<PathBuf>), TerminalError>,
        cx: &mut Context<'_, Self>,
    ) {
        match result {
            Ok((session, java_home)) => {
                session.resize(self.grid_size).ok();
                self.session = Some(session);
                self.java_home = java_home;
                self.status = Status::Running;
            }
            Err(err) => self.status = Status::Failed(err.to_string()),
        }
        cx.notify();
    }

    fn poll(&mut self, cx: &mut Context<'_, Self>) {
        let Some(session) = &self.session else {
            return;
        };
        if !session.take_dirty() {
            return;
        }
        if let Some(text) = session.with_state(TermState::take_clipboard_request) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
        if session.is_closed() && self.status == Status::Running {
            self.status = Status::Exited;
            cx.emit(TerminalViewEvent::Exited);
        }
        cx.notify();
    }

    /// Focus handle of the grid (keyboard input goes to the shell while it is focused).
    #[must_use]
    pub const fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    /// `JAVA_HOME` injected into this session, once started.
    #[must_use]
    pub const fn java_home(&self) -> Option<&PathBuf> {
        self.java_home.as_ref()
    }

    /// Title set by the running program (`OSC 0/2`), if any.
    #[must_use]
    pub fn program_title(&self) -> Option<String> {
        self.session
            .as_ref()
            .and_then(|session| session.with_state(|state| state.title().map(str::to_owned)))
    }

    /// Whether the program has exited.
    #[must_use]
    pub fn has_exited(&self) -> bool {
        self.status == Status::Exited
    }

    /// Current grid size in cells.
    #[must_use]
    pub const fn grid_size(&self) -> TerminalSize {
        self.grid_size
    }

    /// Changes the font; the grid is re-fitted on the next frame.
    pub fn set_font(&mut self, family: &str, size: f32, cx: &mut Context<'_, Self>) {
        family.clone_into(&mut self.font_family);
        self.font_size = size;
        self.metrics = None;
        cx.notify();
    }

    /// Clears the screen and scrollback (`Cmd+K`).
    pub fn clear(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(session) = &self.session {
            session.clear_buffer();
        }
        cx.notify();
    }

    /// Copies the selection to the clipboard; returns whether anything was copied.
    pub fn copy(&mut self, cx: &mut Context<'_, Self>) -> bool {
        let text = self
            .session
            .as_ref()
            .and_then(|session| session.with_state(|state| state.selection_text()));
        match text {
            Some(text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                true
            }
            None => false,
        }
    }

    /// Pastes the clipboard into the program (bracketed when the program asked for it).
    pub fn paste(&mut self, cx: &mut Context<'_, Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        self.write_input(|mode| encode_paste(&text, mode), cx);
    }

    fn write_input(&self, encode: impl FnOnce(TermMode) -> Vec<u8>, cx: &mut Context<'_, Self>) {
        let Some(session) = &self.session else {
            return;
        };
        let bytes = session.with_state(|state| {
            state.clear_selection();
            state.scroll_to_bottom();
            encode(state.mode())
        });
        session.write_bytes(&bytes).ok();
        session.mark_dirty();
        cx.notify();
    }

    fn font(&self) -> Font {
        Font {
            family: self.font_family.clone().into(),
            features: FontFeatures::default(),
            fallbacks: Some(FontFallbacks::from_fonts(vec![
                "SF Mono".to_owned(),
                "Menlo".to_owned(),
                "Cascadia Mono".to_owned(),
                "DejaVu Sans Mono".to_owned(),
                "monospace".to_owned(),
            ])),
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
        }
    }

    fn ensure_metrics(&mut self, window: &Window) -> CellMetrics {
        if let Some(metrics) = self.metrics {
            return metrics;
        }
        let run = TextRun {
            len: 1,
            font: self.font(),
            color: Hsla::default(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let width = f32::from(
            window
                .text_system()
                .shape_line(SharedString::from("0"), px(self.font_size), &[run], None)
                .width,
        )
        .max(1.0);
        let metrics = CellMetrics {
            width,
            line_height: (self.font_size * LINE_HEIGHT_FACTOR).round().max(1.0),
        };
        self.metrics = Some(metrics);
        metrics
    }

    /// Re-fits the grid to the laid-out bounds (called from prepaint).
    fn fit_to_bounds(&mut self, bounds: Bounds<Pixels>, cx: &mut Context<'_, Self>) {
        self.grid_bounds = Some(bounds);
        let Some(metrics) = self.metrics else {
            return;
        };
        let size = TerminalSize::fit(
            f32::from(bounds.size.width),
            f32::from(bounds.size.height),
            metrics.width,
            metrics.line_height,
        );
        if size != self.grid_size {
            self.grid_size = size;
            if let Some(session) = &self.session {
                session.resize(size).ok();
            }
            cx.notify();
        }
    }

    /// Maps a window position to `(col, row, right_half)` in the visible grid.
    fn cell_at(&self, position: Point<Pixels>) -> Option<(u16, u16, bool)> {
        let bounds = self.grid_bounds?;
        let metrics = self.metrics?;
        let x = (f32::from(position.x - bounds.origin.x) / metrics.width).max(0.0);
        let y = (f32::from(position.y - bounds.origin.y) / metrics.line_height).max(0.0);
        let col = clamp_cell(x, self.grid_size.cols);
        let row = clamp_cell(y, self.grid_size.rows);
        Some((col, row, x.fract() > 0.5))
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let keystroke = &event.keystroke;
        let m = &keystroke.modifiers;
        // `Cmd` combinations belong to the IDE (handled by the workspace keymap).
        if m.platform {
            return;
        }
        let Some(session) = &self.session else {
            return;
        };
        let key = keystroke.key.as_str();
        if m.shift && !m.control && !m.alt && matches!(key, "pageup" | "pagedown") {
            let page = i32::from(self.grid_size.rows.saturating_sub(1)).max(1);
            session.with_state(|state| state.scroll(if key == "pageup" { page } else { -page }));
            session.mark_dirty();
            cx.stop_propagation();
            return;
        }
        let mods = KeyModifiers {
            ctrl: m.control,
            alt: m.alt,
            shift: m.shift,
        };
        // Option/Alt acts as Meta: send `ESC` + the unmodified key, not the composed glyph.
        let text = if m.alt {
            None
        } else {
            keystroke.key_char.clone()
        };
        let term_mode = session.with_state(|state| state.mode());
        if let Some(bytes) = encode_key(key, text.as_deref(), mods, term_mode) {
            self.write_input(|_| bytes, cx);
            cx.stop_propagation();
        }
    }

    fn handle_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        window.focus(&self.focus_handle);
        if let (Some(session), Some((col, row, right))) =
            (&self.session, self.cell_at(event.position))
        {
            session.with_state(|state| state.start_selection(col, row, right));
            session.mark_dirty();
            self.selecting = true;
        }
        cx.notify();
    }

    fn handle_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.selecting || event.pressed_button != Some(MouseButton::Left) {
            return;
        }
        if let (Some(session), Some((col, row, right))) =
            (&self.session, self.cell_at(event.position))
        {
            session.with_state(|state| state.update_selection(col, row, right));
            session.mark_dirty();
            cx.notify();
        }
    }

    fn handle_mouse_up(
        &mut self,
        _event: &MouseUpEvent,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.selecting = false;
        if let Some(session) = &self.session
            && session.with_state(|state| state.selection_text()).is_none()
        {
            session.with_state(TermState::clear_selection);
            session.mark_dirty();
        }
        cx.notify();
    }

    fn handle_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(metrics) = self.metrics else {
            return;
        };
        let lines = match event.delta {
            ScrollDelta::Lines(delta) => delta.y,
            ScrollDelta::Pixels(delta) => f32::from(delta.y) / metrics.line_height,
        } + self.scroll_remainder;
        let whole = lines.trunc();
        self.scroll_remainder = lines - whole;
        let whole = clamp_lines(whole);
        if whole == 0 {
            return;
        }
        let Some(session) = &self.session else {
            return;
        };
        let (col, row, _) = self.cell_at(event.position).unwrap_or((0, 0, false));
        let mode = session.with_state(|state| state.mode());
        match encode_scroll(whole, col, row, mode) {
            Some(bytes) => {
                session.write_bytes(&bytes).ok();
            }
            None => session.with_state(|state| state.scroll(whole)),
        }
        session.mark_dirty();
        cx.notify();
        cx.stop_propagation();
    }

    fn render_line(
        cells: &[SnapshotCell],
        palette: &TerminalPalette,
        base_font: &Font,
        focused: bool,
    ) -> StyledText {
        let mut text = String::with_capacity(cells.len());
        let mut runs: Vec<TextRun> = Vec::new();
        let default_bg = hsla_from_rgba(palette.background);
        for cell in cells.iter().filter(|cell| !cell.spacer) {
            let mut fg = hsla_from_rgba(resolve_foreground(palette, cell.fg));
            let mut bg = hsla_from_rgba(resolve_background(palette, cell.bg));
            let mut underline = cell.style.underline;
            if cell.selected {
                fg = hsla_from_rgba(palette.selection_fg);
                bg = hsla_from_rgba(palette.selection_bg);
            }
            if cell.cursor {
                if focused {
                    fg = hsla_from_rgba(palette.cursor_text);
                    bg = hsla_from_rgba(palette.cursor);
                } else {
                    underline = true;
                }
            }
            if cell.style.dim {
                fg.a *= 0.66;
            }
            let mut font = base_font.clone();
            if cell.style.bold {
                font.weight = FontWeight::BOLD;
            }
            if cell.style.italic {
                font.style = FontStyle::Italic;
            }
            let run = TextRun {
                len: cell.ch.len_utf8(),
                font,
                color: fg,
                background_color: (bg != default_bg).then_some(bg),
                underline: underline.then(|| UnderlineStyle {
                    thickness: px(1.0),
                    color: Some(if cell.cursor && !focused {
                        hsla_from_rgba(palette.cursor)
                    } else {
                        fg
                    }),
                    wavy: false,
                }),
                strikethrough: cell.style.strikeout.then(|| StrikethroughStyle {
                    thickness: px(1.0),
                    color: Some(fg),
                }),
            };
            text.push(cell.ch);
            match runs.last_mut() {
                Some(last) if same_style(last, &run) => last.len += run.len,
                _ => runs.push(run),
            }
        }
        StyledText::new(text).with_runs(runs)
    }

    fn render_status(&self, snapshot: Option<&TerminalSnapshot>) -> Option<String> {
        match &self.status {
            Status::Starting => Some("Starting terminal…".to_owned()),
            Status::Failed(message) => Some(format!("Terminal failed to start: {message}")),
            Status::Exited if snapshot.is_none() => Some("[Process completed]".to_owned()),
            Status::Running | Status::Exited => None,
        }
    }
}

fn same_style(a: &TextRun, b: &TextRun) -> bool {
    a.font == b.font
        && a.color == b.color
        && a.background_color == b.background_color
        && a.underline == b.underline
        && a.strikethrough == b.strikethrough
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "value is clamped to the grid before the cast"
)]
fn clamp_cell(value: f32, limit: u16) -> u16 {
    value.floor().clamp(0.0, f32::from(limit.saturating_sub(1))) as u16
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "value is clamped to ±1000 lines before the cast"
)]
fn clamp_lines(value: f32) -> i32 {
    value.clamp(-1000.0, 1000.0) as i32
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let metrics = self.ensure_metrics(window);
        let palette = cx.global::<KovaTheme>().tokens().terminal.clone();
        let focused = self.focus_handle.is_focused(window);
        let snapshot = self.session.as_ref().map(PtySession::snapshot);
        let font = self.font();
        let line_height = px(metrics.line_height);

        let status = self.render_status(snapshot.as_ref());
        let lines = snapshot.iter().flat_map(|snapshot| {
            snapshot.lines.iter().map(|cells| {
                div()
                    .h(line_height)
                    .flex_none()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .child(Self::render_line(cells, &palette, &font, focused))
            })
        });
        let lines: Vec<_> = lines.collect();
        let entity = cx.entity();

        div()
            .id("terminal-view")
            .key_context("Terminal")
            .track_focus(&self.focus_handle)
            .size_full()
            .min_h(px(0.0))
            .p(px(GRID_PADDING_PX))
            .bg(hsla_from_rgba(palette.background))
            .text_color(hsla_from_rgba(palette.foreground))
            .font(font.clone())
            .text_size(px(self.font_size))
            .line_height(line_height)
            .on_key_down(cx.listener(Self::handle_key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::handle_mouse_down))
            .on_mouse_move(cx.listener(Self::handle_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::handle_mouse_up))
            .on_scroll_wheel(cx.listener(Self::handle_scroll_wheel))
            .child(
                div()
                    .relative()
                    .size_full()
                    .overflow_hidden()
                    .child(
                        canvas(
                            move |bounds, _window, cx: &mut App| {
                                entity.update(cx, |view, cx| view.fit_to_bounds(bounds, cx));
                            },
                            |_, (), _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .children(status.map(|text| div().child(text)))
                    .children(lines),
            )
    }
}
