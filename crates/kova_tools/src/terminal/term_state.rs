//! `alacritty_terminal` emulator wrapper and the theme-neutral grid snapshot for the UI.

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::{Color, Handler as _, NamedColor, Processor};

/// Grid size of a terminal in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TerminalSize {
    /// Number of columns (at least 1).
    pub cols: u16,
    /// Number of visible rows (at least 1).
    pub rows: u16,
}

impl TerminalSize {
    /// Creates a size, clamping both dimensions to at least 1 cell.
    #[must_use]
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols: cols.max(1),
            rows: rows.max(1),
        }
    }

    /// Fits a grid into `width` × `height` logical pixels with the given cell metrics.
    #[must_use]
    pub fn fit(width: f32, height: f32, cell_width: f32, cell_height: f32) -> Self {
        Self::new(cells_in(width, cell_width), cells_in(height, cell_height))
    }
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "value is clamped to 1..=u16::MAX before the cast"
)]
fn cells_in(extent: f32, cell: f32) -> u16 {
    if !extent.is_finite() || !cell.is_finite() || cell <= 0.0 {
        return 1;
    }
    (extent / cell).floor().clamp(1.0, f32::from(u16::MAX)) as u16
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self::new(80, 24)
    }
}

impl Dimensions for TerminalSize {
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }

    fn screen_lines(&self) -> usize {
        usize::from(self.rows)
    }

    fn columns(&self) -> usize {
        usize::from(self.cols)
    }
}

/// Theme-neutral color of a cell; resolved against the theme palette by the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CellColor {
    /// The terminal default foreground.
    DefaultForeground,
    /// The terminal default background.
    DefaultBackground,
    /// One of the 16 ANSI colors (`0..=15`).
    Ansi(u8),
    /// An xterm 256-color index (`16..=255`).
    Indexed(u8),
    /// A 24-bit `TrueColor` value.
    Rgb(u8, u8, u8),
}

impl CellColor {
    fn from_vte(color: Color) -> Self {
        match color {
            Color::Spec(rgb) => Self::Rgb(rgb.r, rgb.g, rgb.b),
            Color::Indexed(index) if index < 16 => Self::Ansi(index),
            Color::Indexed(index) => Self::Indexed(index),
            Color::Named(named) => Self::from_named(named),
        }
    }

    fn from_named(named: NamedColor) -> Self {
        match named {
            NamedColor::Foreground
            | NamedColor::BrightForeground
            | NamedColor::DimForeground
            | NamedColor::Cursor => Self::DefaultForeground,
            NamedColor::Background => Self::DefaultBackground,
            NamedColor::DimBlack => Self::Ansi(0),
            NamedColor::DimRed => Self::Ansi(1),
            NamedColor::DimGreen => Self::Ansi(2),
            NamedColor::DimYellow => Self::Ansi(3),
            NamedColor::DimBlue => Self::Ansi(4),
            NamedColor::DimMagenta => Self::Ansi(5),
            NamedColor::DimCyan => Self::Ansi(6),
            NamedColor::DimWhite => Self::Ansi(7),
            other => {
                let index = other as usize;
                Self::Ansi(u8::try_from(index.min(15)).unwrap_or(15))
            }
        }
    }

    /// Returns the bright variant used for bold text in ANSI colors `0..=7`.
    #[must_use]
    pub const fn brightened(self) -> Self {
        match self {
            Self::Ansi(index) if index < 8 => Self::Ansi(index + 8),
            other => other,
        }
    }
}

/// Text attributes of a cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "plain SGR attribute flags mirrored for the renderer"
)]
pub struct CellStyle {
    /// Bold (`SGR 1`).
    pub bold: bool,
    /// Italic (`SGR 3`).
    pub italic: bool,
    /// Any underline style (`SGR 4`).
    pub underline: bool,
    /// Strikethrough (`SGR 9`).
    pub strikeout: bool,
    /// Dim / faint (`SGR 2`).
    pub dim: bool,
}

/// One rendered cell: character, resolved colors and attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotCell {
    /// Character to draw (a space for empty cells and wide-character spacers).
    pub ch: char,
    /// Foreground color (after `SGR 7` inverse, cursor and selection are applied).
    pub fg: CellColor,
    /// Background color (after `SGR 7` inverse, cursor and selection are applied).
    pub bg: CellColor,
    /// Text attributes.
    pub style: CellStyle,
    /// Whether this cell is under the cursor.
    pub cursor: bool,
    /// Whether this cell is selected.
    pub selected: bool,
    /// Whether this cell is the right half of a wide character (renderers skip it).
    pub spacer: bool,
}

/// Visible grid contents for one frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalSnapshot {
    /// Grid size.
    pub size: TerminalSize,
    /// Visible rows, top to bottom, each exactly `size.cols` cells wide.
    pub lines: Vec<Vec<SnapshotCell>>,
    /// Cursor position as `(column, row)` in the visible grid, when shown.
    pub cursor: Option<(u16, u16)>,
    /// How many lines the view is scrolled back into history.
    pub display_offset: usize,
}

impl TerminalSnapshot {
    /// Returns row `row` as plain text with trailing blanks trimmed.
    #[must_use]
    pub fn line_text(&self, row: usize) -> String {
        self.lines.get(row).map_or_else(String::new, |cells| {
            cells
                .iter()
                .map(|cell| cell.ch)
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
    }

    /// Returns all visible rows as text, joined by `\n`.
    #[must_use]
    pub fn text(&self) -> String {
        (0..self.lines.len())
            .map(|row| self.line_text(row))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Events from the emulator that the session must act on.
#[derive(Debug, Default)]
struct PendingEvents {
    /// Bytes the terminal answers to the program (device status reports, …).
    pty_responses: Vec<u8>,
    /// Latest window title change (`OSC 0/2`).
    title: Option<TitleUpdate>,
    /// Clipboard store requests (`OSC 52`).
    clipboard: Option<String>,
}

/// A window title change requested by the program.
#[derive(Debug)]
enum TitleUpdate {
    Set(String),
    Reset,
}

/// `alacritty_terminal` listener that buffers events for [`TermState`].
#[derive(Clone, Default)]
struct EventProxy {
    pending: Arc<Mutex<PendingEvents>>,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        match event {
            Event::PtyWrite(text) => pending.pty_responses.extend_from_slice(text.as_bytes()),
            Event::Title(title) => pending.title = Some(TitleUpdate::Set(title)),
            Event::ResetTitle => pending.title = Some(TitleUpdate::Reset),
            Event::ClipboardStore(_, text) => pending.clipboard = Some(text),
            _ => {}
        }
    }
}

/// Terminal emulator state: the `alacritty_terminal` grid fed through a `vte` parser.
pub struct TermState {
    term: Term<EventProxy>,
    parser: Processor,
    events: EventProxy,
    size: TerminalSize,
    title: Option<String>,
}

impl fmt::Debug for TermState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TermState")
            .field("size", &self.size)
            .field("title", &self.title)
            .finish_non_exhaustive()
    }
}

impl TermState {
    /// Creates an empty terminal of `size` keeping `scrollback` lines of history.
    #[must_use]
    pub fn new(size: TerminalSize, scrollback: usize) -> Self {
        let events = EventProxy::default();
        let config = Config {
            scrolling_history: scrollback,
            ..Config::default()
        };
        Self {
            term: Term::new(config, &size, events.clone()),
            parser: Processor::new(),
            events,
            size,
            title: None,
        }
    }

    /// Feeds raw program output through the ANSI parser.
    pub fn advance(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
        let mut pending = self
            .events
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match pending.title.take() {
            Some(TitleUpdate::Set(title)) => self.title = Some(title),
            Some(TitleUpdate::Reset) => self.title = None,
            None => {}
        }
    }

    /// Drains the bytes the emulator wants written back to the program.
    #[must_use]
    pub fn take_pty_responses(&mut self) -> Vec<u8> {
        let mut pending = self
            .events
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        std::mem::take(&mut pending.pty_responses)
    }

    /// Drains the latest `OSC 52` clipboard store request.
    #[must_use]
    pub fn take_clipboard_request(&mut self) -> Option<String> {
        let mut pending = self
            .events
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        pending.clipboard.take()
    }

    /// Current window title set by the program (`OSC 0/2`).
    #[must_use]
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Current grid size.
    #[must_use]
    pub const fn size(&self) -> TerminalSize {
        self.size
    }

    /// Active terminal modes (application cursor keys, alternate screen, mouse reporting …).
    #[must_use]
    pub fn mode(&self) -> TermMode {
        *self.term.mode()
    }

    /// Resizes the grid, re-wrapping the primary screen.
    pub fn resize(&mut self, size: TerminalSize) {
        if size != self.size {
            self.size = size;
            self.term.resize(size);
        }
    }

    /// Scrolls the view by `delta` lines (positive = back into history).
    pub fn scroll(&mut self, delta: i32) {
        self.term.scroll_display(Scroll::Delta(delta));
    }

    /// Scrolls the view back to the live bottom of the output.
    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    /// Clears the visible screen and the scrollback history (`Cmd+K`).
    ///
    /// The line holding the cursor is kept so an interactive prompt stays visible.
    pub fn clear_buffer(&mut self) {
        self.term.selection = None;
        self.term.grid_mut().clear_history();
        let cursor = self.term.grid().cursor.point;
        let row = usize::try_from(cursor.line.0).unwrap_or(0);
        if row > 0 {
            // Move the prompt line to the top; scrolled-off lines go to history, cleared below.
            self.term.scroll_up(row);
            self.term.goto(0, cursor.column.0);
        }
        self.term.grid_mut().clear_history();
        self.term.scroll_display(Scroll::Bottom);
    }

    /// Starts a mouse selection at visible cell `(col, row)`.
    pub fn start_selection(&mut self, col: u16, row: u16, right_half: bool) {
        let point = self.viewport_point(col, row);
        let side = if right_half { Side::Right } else { Side::Left };
        self.term.selection = Some(Selection::new(SelectionType::Simple, point, side));
    }

    /// Extends the current selection to visible cell `(col, row)`.
    pub fn update_selection(&mut self, col: u16, row: u16, right_half: bool) {
        let point = self.viewport_point(col, row);
        let side = if right_half { Side::Right } else { Side::Left };
        if let Some(selection) = self.term.selection.as_mut() {
            selection.update(point, side);
        }
    }

    /// Drops the current selection.
    pub fn clear_selection(&mut self) {
        self.term.selection = None;
    }

    /// Returns the selected text, if a non-empty selection exists.
    #[must_use]
    pub fn selection_text(&self) -> Option<String> {
        self.term
            .selection_to_string()
            .filter(|text| !text.is_empty())
    }

    /// Whether any part of the grid changed since the last [`Self::reset_damage`].
    pub fn is_damaged(&mut self) -> bool {
        match self.term.damage() {
            TermDamage::Full => true,
            TermDamage::Partial(mut lines) => lines.next().is_some(),
        }
    }

    /// Clears the damage tracking state after a frame was drawn.
    pub fn reset_damage(&mut self) {
        self.term.reset_damage();
    }

    fn viewport_point(&self, col: u16, row: u16) -> Point {
        let offset = i32::try_from(self.term.grid().display_offset()).unwrap_or(i32::MAX);
        let last_col = self.term.columns().saturating_sub(1);
        let last_row = i32::try_from(self.term.screen_lines().saturating_sub(1)).unwrap_or(0);
        Point::new(
            Line(i32::from(row).min(last_row) - offset),
            Column(usize::from(col).min(last_col)),
        )
    }

    /// Captures the visible grid for rendering.
    #[must_use]
    pub fn snapshot(&self) -> TerminalSnapshot {
        let content = self.term.renderable_content();
        let cols = usize::from(self.size.cols);
        let rows = usize::from(self.size.rows);
        let blank = SnapshotCell {
            ch: ' ',
            fg: CellColor::DefaultForeground,
            bg: CellColor::DefaultBackground,
            style: CellStyle::default(),
            cursor: false,
            selected: false,
            spacer: false,
        };
        let mut lines = vec![vec![blank; cols]; rows];
        let offset = i32::try_from(content.display_offset).unwrap_or(i32::MAX);
        let show_cursor = content.mode.contains(TermMode::SHOW_CURSOR);
        let cursor_point = content.cursor.point;
        let selection = content.selection;
        let mut cursor = None;

        for indexed in content.display_iter {
            let Ok(row) = usize::try_from(indexed.point.line.0 + offset) else {
                continue;
            };
            let col = indexed.point.column.0;
            let Some(slot) = lines.get_mut(row).and_then(|line| line.get_mut(col)) else {
                continue;
            };
            let cell = indexed.cell;
            let flags = cell.flags;
            let mut fg = CellColor::from_vte(cell.fg);
            let mut bg = CellColor::from_vte(cell.bg);
            if flags.contains(Flags::BOLD) {
                fg = fg.brightened();
            }
            if flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            let spacer =
                flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER);
            let ch = if spacer || flags.contains(Flags::HIDDEN) || cell.c == '\t' {
                ' '
            } else {
                cell.c
            };
            let is_cursor = show_cursor && indexed.point == cursor_point;
            if is_cursor {
                cursor = u16::try_from(col).ok().zip(u16::try_from(row).ok());
            }
            *slot = SnapshotCell {
                ch,
                fg,
                bg,
                style: CellStyle {
                    bold: flags.contains(Flags::BOLD),
                    italic: flags.contains(Flags::ITALIC),
                    underline: flags.intersects(Flags::ALL_UNDERLINES),
                    strikeout: flags.contains(Flags::STRIKEOUT),
                    dim: flags.contains(Flags::DIM),
                },
                cursor: is_cursor,
                selected: selection.is_some_and(|range| range.contains(indexed.point)),
                spacer: flags.contains(Flags::WIDE_CHAR_SPACER),
            };
        }

        TerminalSnapshot {
            size: self.size,
            lines,
            cursor,
            display_offset: content.display_offset,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(cols: u16, rows: u16) -> TermState {
        TermState::new(TerminalSize::new(cols, rows), 100)
    }

    #[test]
    fn plain_text_lands_in_the_grid() {
        let mut t = term(20, 3);
        t.advance(b"hello\r\nworld");
        let snap = t.snapshot();
        assert_eq!(snap.line_text(0), "hello");
        assert_eq!(snap.line_text(1), "world");
        assert_eq!(snap.cursor, Some((5, 1)));
        assert!(snap.lines[1][5].cursor);
    }

    #[test]
    fn sgr_colors_map_to_ansi_indexed_and_truecolor() {
        let mut t = term(20, 2);
        t.advance(b"\x1b[31mR\x1b[0m\x1b[38;5;200mI\x1b[38;2;1;2;3mT\x1b[1;32mB");
        let row = &t.snapshot().lines[0];
        assert_eq!(row[0].fg, CellColor::Ansi(1));
        assert_eq!(row[1].fg, CellColor::Indexed(200));
        assert_eq!(row[2].fg, CellColor::Rgb(1, 2, 3));
        assert_eq!(row[3].fg, CellColor::Ansi(10), "bold green renders bright");
        assert!(row[3].style.bold);
    }

    #[test]
    fn inverse_swaps_default_colors() {
        let mut t = term(10, 1);
        t.advance(b"\x1b[7mX");
        let cell = t.snapshot().lines[0][0];
        assert_eq!(cell.fg, CellColor::DefaultBackground);
        assert_eq!(cell.bg, CellColor::DefaultForeground);
    }

    #[test]
    fn resize_rewraps_long_lines() {
        let mut t = term(10, 4);
        t.advance(b"0123456789abc");
        assert_eq!(t.snapshot().line_text(0), "0123456789");
        t.resize(TerminalSize::new(20, 4));
        assert_eq!(t.size(), TerminalSize::new(20, 4));
        assert_eq!(t.snapshot().line_text(0), "0123456789abc");
    }

    #[test]
    fn alternate_screen_is_tracked_in_mode() {
        let mut t = term(10, 2);
        t.advance(b"\x1b[?1049h");
        assert!(t.mode().contains(TermMode::ALT_SCREEN));
        t.advance(b"\x1b[?1049l");
        assert!(!t.mode().contains(TermMode::ALT_SCREEN));
    }

    #[test]
    fn device_status_report_is_answered() {
        let mut t = term(10, 2);
        t.advance(b"ab\x1b[6n");
        assert_eq!(t.take_pty_responses(), b"\x1b[1;3R");
        assert_eq!(t.take_pty_responses(), Vec::<u8>::new());
    }

    #[test]
    fn osc_title_is_captured() {
        let mut t = term(10, 2);
        t.advance(b"\x1b]0;gradle build\x07");
        assert_eq!(t.title(), Some("gradle build"));
    }

    #[test]
    fn scrollback_and_clear_buffer() {
        let mut t = term(10, 2);
        t.advance(b"1\r\n2\r\n3\r\n$ ");
        t.scroll(10);
        assert!(t.snapshot().display_offset > 0);
        t.scroll_to_bottom();
        t.clear_buffer();
        let snap = t.snapshot();
        assert_eq!(snap.line_text(0), "$");
        assert_eq!(snap.line_text(1), "");
        assert_eq!(snap.cursor, Some((2, 0)));
        t.scroll(10);
        assert_eq!(t.snapshot().display_offset, 0, "history is gone");
    }

    #[test]
    fn selection_yields_text() {
        let mut t = term(20, 2);
        t.advance(b"java -version");
        t.start_selection(0, 0, false);
        t.update_selection(3, 0, true);
        assert_eq!(t.selection_text().as_deref(), Some("java"));
        assert!(t.snapshot().lines[0][2].selected);
        t.clear_selection();
        assert_eq!(t.selection_text(), None);
    }

    #[test]
    fn damage_tracking_reports_changes() {
        let mut t = term(10, 2);
        t.advance(b"x");
        assert!(t.is_damaged());
        t.reset_damage();
    }

    #[test]
    fn fit_computes_cells_from_pixels() {
        assert_eq!(
            TerminalSize::fit(805.0, 170.0, 8.0, 17.0),
            TerminalSize::new(100, 10)
        );
        assert_eq!(
            TerminalSize::fit(0.0, f32::NAN, 8.0, 17.0),
            TerminalSize::new(1, 1)
        );
    }
}
