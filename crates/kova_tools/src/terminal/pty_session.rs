//! A shell process inside a pseudo terminal, read off the UI thread.
//!
//! [`PtySession::spawn`] opens a PTY with `portable-pty`, starts the program in it and runs a
//! reader thread that feeds every chunk of output into the shared [`TermState`]. The reader
//! never notifies the UI directly: it raises a *dirty* flag that the UI polls once per frame
//! (~8 ms), so a flood of output (`cat` of a large file) costs at most one repaint per frame.
//!
//! Dropping the session sends `SIGHUP` to the child (escalating to `SIGKILL` after a short
//! grace period) and reaps it on a background thread, so no orphaned shell survives a closed
//! tab or window.

use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use super::env_builder::TerminalEnv;
use super::error::TerminalError;
use super::shell_integration::prepare_login_shell;
use super::term_state::{TermState, TerminalSize, TerminalSnapshot};

/// Default scrollback for sessions that do not configure one.
pub const DEFAULT_SCROLLBACK_LINES: usize = 10_000;

/// Returns the user's login shell: `$SHELL`, else `/bin/zsh` on macOS and `/bin/bash` elsewhere.
#[must_use]
pub fn default_shell() -> PathBuf {
    std::env::var_os("SHELL")
        .filter(|shell| !shell.is_empty())
        .map_or_else(
            || {
                PathBuf::from(if cfg!(target_os = "macos") {
                    "/bin/zsh"
                } else {
                    "/bin/bash"
                })
            },
            PathBuf::from,
        )
}

/// What to run in a new session and where.
#[derive(Debug, Clone)]
pub struct PtySpawnConfig {
    /// Program to start (a shell by default).
    pub program: PathBuf,
    /// Program arguments.
    pub args: Vec<String>,
    /// Working directory (the project root).
    pub cwd: Option<PathBuf>,
    /// Environment overrides on top of the inherited environment.
    pub env: TerminalEnv,
    /// Initial grid size.
    pub size: TerminalSize,
    /// Scrollback history capacity in lines.
    pub scrollback: usize,
}

impl PtySpawnConfig {
    /// Runs `program` (default: [`default_shell`]) as an interactive login shell in `cwd`,
    /// with `java_home` exported and re-applied after the user's startup files.
    #[must_use]
    pub fn login_shell(
        program: Option<PathBuf>,
        cwd: Option<&Path>,
        java_home: Option<&Path>,
    ) -> Self {
        let program = program.unwrap_or_else(default_shell);
        let mut env = TerminalEnv::from_process(java_home);
        let args = prepare_login_shell(&program, java_home, &mut env, |key| std::env::var_os(key));
        Self {
            program,
            args,
            cwd: cwd.map(Path::to_path_buf),
            env,
            size: TerminalSize::default(),
            scrollback: DEFAULT_SCROLLBACK_LINES,
        }
    }

    /// Runs an explicit program with arguments.
    #[must_use]
    pub fn command(program: impl Into<PathBuf>, args: &[&str]) -> Self {
        Self {
            program: program.into(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            cwd: None,
            env: TerminalEnv::default(),
            size: TerminalSize::default(),
            scrollback: DEFAULT_SCROLLBACK_LINES,
        }
    }
}

type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// A running program attached to a pseudo terminal and its emulator state.
pub struct PtySession {
    state: Arc<Mutex<TermState>>,
    writer: SharedWriter,
    master: Box<dyn MasterPty + Send>,
    child: Option<Box<dyn Child + Send + Sync>>,
    dirty: Arc<AtomicBool>,
    output_closed: Arc<AtomicBool>,
    pid: Option<u32>,
}

impl fmt::Debug for PtySession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PtySession")
            .field("pid", &self.pid)
            .field("output_closed", &self.output_closed.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

fn lock<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

const fn pty_size(size: TerminalSize) -> PtySize {
    PtySize {
        rows: size.rows,
        cols: size.cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

impl PtySession {
    /// Opens a PTY, starts `config.program` in it and starts the reader thread.
    ///
    /// # Errors
    /// Returns [`TerminalError`] when the PTY cannot be opened or the program cannot start.
    pub fn spawn(config: &PtySpawnConfig) -> Result<Self, TerminalError> {
        let pair = native_pty_system()
            .openpty(pty_size(config.size))
            .map_err(|err| TerminalError::OpenPty(err.to_string()))?;

        let mut cmd = CommandBuilder::new(&config.program);
        cmd.args(&config.args);
        if let Some(cwd) = &config.cwd {
            cmd.cwd(cwd);
        }
        for (key, value) in config.env.iter() {
            cmd.env(key, value);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|err| TerminalError::Spawn {
                program: config.program.clone(),
                message: err.to_string(),
            })?;
        // Our copy of the slave must close, or the reader never sees EOF when the child exits.
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|err| TerminalError::OpenPty(err.to_string()))?;
        let writer: SharedWriter = Arc::new(Mutex::new(
            pair.master
                .take_writer()
                .map_err(|err| TerminalError::OpenPty(err.to_string()))?,
        ));

        let state = Arc::new(Mutex::new(TermState::new(config.size, config.scrollback)));
        let dirty = Arc::new(AtomicBool::new(true));
        let output_closed = Arc::new(AtomicBool::new(false));
        let pid = child.process_id();

        let reader_ctx = ReaderContext {
            state: Arc::clone(&state),
            writer: Arc::clone(&writer),
            dirty: Arc::clone(&dirty),
            output_closed: Arc::clone(&output_closed),
        };
        thread::Builder::new()
            .name("kova-pty-reader".to_owned())
            .spawn(move || reader_ctx.run(reader))?;

        Ok(Self {
            state,
            writer,
            master: pair.master,
            child: Some(child),
            dirty,
            output_closed,
            pid,
        })
    }

    /// Process id of the child, when the platform reports one.
    #[must_use]
    pub const fn process_id(&self) -> Option<u32> {
        self.pid
    }

    /// Writes raw bytes (encoded keystrokes, paste) to the program.
    ///
    /// # Errors
    /// Returns [`TerminalError::Io`] when the PTY is closed.
    pub fn write_bytes(&self, bytes: &[u8]) -> Result<(), TerminalError> {
        if bytes.is_empty() {
            return Ok(());
        }
        let mut writer = lock(&self.writer);
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(())
    }

    /// Resizes the PTY (`TIOCSWINSZ`, delivering `SIGWINCH`) and the emulator grid.
    ///
    /// # Errors
    /// Returns [`TerminalError::Resize`] when the kernel rejects the new size.
    pub fn resize(&self, size: TerminalSize) -> Result<(), TerminalError> {
        let mut state = lock(&self.state);
        if state.size() == size {
            return Ok(());
        }
        self.master
            .resize(pty_size(size))
            .map_err(|err| TerminalError::Resize(err.to_string()))?;
        state.resize(size);
        drop(state);
        self.dirty.store(true, Ordering::Release);
        Ok(())
    }

    /// Current grid size.
    #[must_use]
    pub fn size(&self) -> TerminalSize {
        lock(&self.state).size()
    }

    /// Returns and clears the *dirty* flag raised by new output (the coalesced wake-up).
    #[must_use]
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::AcqRel)
    }

    /// Marks the view dirty after a local change (scroll, selection).
    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Release);
    }

    /// Whether the program closed its output (it exited).
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.output_closed.load(Ordering::Acquire)
    }

    /// Whether the child process has exited.
    pub fn has_exited(&mut self) -> bool {
        self.child
            .as_mut()
            .is_none_or(|child| matches!(child.try_wait(), Ok(Some(_))))
    }

    /// Runs `f` with exclusive access to the emulator state.
    pub fn with_state<R>(&self, f: impl FnOnce(&mut TermState) -> R) -> R {
        f(&mut lock(&self.state))
    }

    /// Captures the visible grid.
    #[must_use]
    pub fn snapshot(&self) -> TerminalSnapshot {
        lock(&self.state).snapshot()
    }

    /// Clears the screen and scrollback, keeping the prompt line (`Cmd+K`).
    pub fn clear_buffer(&self) {
        lock(&self.state).clear_buffer();
        self.mark_dirty();
    }

    /// Sends `SIGHUP` (then `SIGKILL` after a grace period) and reaps the child off-thread.
    pub fn terminate(&mut self) {
        if let Some(mut child) = self.child.take() {
            let reap = move || {
                // `kill` sends SIGHUP and escalates to SIGKILL after ~200 ms (portable-pty).
                child.kill().ok();
                child.wait().ok();
            };
            if thread::Builder::new()
                .name("kova-pty-reaper".to_owned())
                .spawn(reap)
                .is_err()
            {
                // Thread creation failed (resource exhaustion): nothing more we can do here.
            }
        }
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        self.terminate();
    }
}

struct ReaderContext {
    state: Arc<Mutex<TermState>>,
    writer: SharedWriter,
    dirty: Arc<AtomicBool>,
    output_closed: Arc<AtomicBool>,
}

impl ReaderContext {
    fn run(self, mut reader: Box<dyn Read + Send>) {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let responses = {
                        let mut state = lock(&self.state);
                        state.advance(&buf[..n]);
                        state.take_pty_responses()
                    };
                    if !responses.is_empty() {
                        let mut writer = lock(&self.writer);
                        writer
                            .write_all(&responses)
                            .and_then(|()| writer.flush())
                            .ok();
                    }
                    self.dirty.store(true, Ordering::Release);
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        self.output_closed.store(true, Ordering::Release);
        self.dirty.store(true, Ordering::Release);
    }
}
