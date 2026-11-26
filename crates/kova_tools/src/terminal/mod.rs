//! Headless PTY terminal core (`KOVA-039`).
//!
//! * [`env_builder`] — the Omakase JVM environment (`JAVA_HOME`, `PATH`, `TERM`) of a session.
//! * [`pty_session`] — a `portable-pty` child process with a background reader thread that feeds
//!   the emulator and raises a coalesced *dirty* flag for the UI.
//! * [`term_state`] — the `alacritty_terminal` emulator plus a theme-neutral grid snapshot.
//! * [`shell_integration`] — `zsh`/`bash` wrappers that re-apply `JAVA_HOME` after the
//!   user's startup files (SDKMAN, jenv, `path_helper`).
//! * [`input`] — keystroke to byte-sequence encoding (VT100 / xterm).
//! * [`colors`] — resolution of snapshot colors against the `kova_theme` terminal palette.
//!
//! Nothing here depends on GPUI: every module compiles and is tested without a window.

pub mod colors;
pub mod env_builder;
pub mod error;
pub mod input;
pub mod pty_session;
pub mod shell_integration;
pub mod term_state;

/// Terminal mode flags (application cursor, alternate screen, mouse reporting …).
pub use alacritty_terminal::term::TermMode;
pub use colors::{resolve_background, resolve_foreground, xterm_256_color};
pub use env_builder::{TerminalEnv, resolve_project_java_home};
pub use error::TerminalError;
pub use input::{KeyModifiers, encode_key, encode_paste, encode_scroll};
pub use pty_session::{PtySession, PtySpawnConfig, default_shell};
pub use shell_integration::prepare_login_shell;
pub use term_state::{
    CellColor, CellStyle, SnapshotCell, TermState, TerminalSize, TerminalSnapshot,
};
