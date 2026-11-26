//! Errors raised by the PTY terminal core.

use std::path::PathBuf;

/// Failure while starting or driving a PTY session.
#[derive(Debug, thiserror::Error)]
pub enum TerminalError {
    /// The operating system could not allocate a pseudo terminal.
    #[error("failed to open a pseudo terminal: {0}")]
    OpenPty(String),
    /// The shell process could not be started inside the PTY.
    #[error("failed to spawn `{program}` in the terminal: {message}")]
    Spawn {
        /// Program that failed to start.
        program: PathBuf,
        /// Underlying error message.
        message: String,
    },
    /// Reading from or writing to the PTY failed.
    #[error("terminal I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// Resizing the PTY (`TIOCSWINSZ`) failed.
    #[error("failed to resize the terminal: {0}")]
    Resize(String),
}
