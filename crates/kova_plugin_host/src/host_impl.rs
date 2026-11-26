//! Host-side implementation of WIT imports (`kova:plugin/host`) and controlled editor context access (FR-2, FR-3).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use kova_buffer::Buffer;
use wasmtime::component::ResourceTable;
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

use crate::bindings::{
    AstSymbol, CommandDescriptor, EditorSnapshot, Host, LogLevel, ToolWindowDescriptor,
};
use crate::manifest::PluginPermissions;
use crate::sandbox::PluginResourceLimiter;

/// Maximum number of log entries retained per plugin store.
pub const MAX_PLUGIN_LOG_ENTRIES: usize = 256;

/// Structured log message emitted by a plugin via `kova:plugin/host.log`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginLogEntry {
    /// Severity level reported by the plugin.
    pub level: LogLevel,
    /// Log message payload.
    pub message: String,
}

/// Bridge providing controlled, permission-gated access to the IDE's active
/// editor buffer (`kova_buffer::BufferSnapshot` / `ropey::Rope`), workspace path,
/// and AST/JVM metadata without coupling `kova_plugin_host` to GPUI.
pub trait EditorContextBridge: Send + Sync {
    /// Returns a read-only snapshot of the current editor state.
    fn snapshot(&self) -> EditorSnapshot;

    /// Applies a byte-range edit to the active buffer, returning the new buffer version.
    ///
    /// # Errors
    ///
    /// Returns an error string if no buffer is active or the edit range is invalid.
    fn apply_edit(&self, start_byte: u64, end_byte: u64, replacement: &str) -> Result<u64, String>;
}

/// Thread-safe headless implementation of [`EditorContextBridge`] backed by
/// [`kova_buffer::Buffer`] (`ropey::Rope` $O(1)$ snapshots).
#[derive(Debug)]
pub struct InMemoryEditorBridge {
    inner: RwLock<EditorBridgeState>,
}

#[derive(Debug)]
struct EditorBridgeState {
    workspace_root: Option<PathBuf>,
    active_file: Option<PathBuf>,
    language: Option<String>,
    buffer: Option<Buffer>,
    cursor_line: u32,
    cursor_column: u32,
    symbols: Vec<AstSymbol>,
    jvm_classpath_count: u32,
}

impl Default for InMemoryEditorBridge {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryEditorBridge {
    /// Creates an empty editor context bridge with no open workspace or buffer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(EditorBridgeState {
                workspace_root: None,
                active_file: None,
                language: None,
                buffer: None,
                cursor_line: 0,
                cursor_column: 0,
                symbols: Vec::new(),
                jvm_classpath_count: 0,
            }),
        }
    }

    /// Sets the active workspace root directory.
    pub fn set_workspace_root(&self, root: Option<PathBuf>) {
        if let Ok(mut state) = self.inner.write() {
            state.workspace_root = root;
        }
    }

    /// Installs an open buffer backed by [`kova_buffer::Buffer`] (`ropey::Rope`)
    /// along with its file path and detected language identifier.
    pub fn set_active_buffer(&self, file: Option<PathBuf>, language: Option<String>, text: &str) {
        if let Ok(mut state) = self.inner.write() {
            state.active_file = file;
            state.language = language;
            state.buffer = Some(Buffer::from_text(text));
            state.cursor_line = 0;
            state.cursor_column = 0;
        }
    }

    /// Clears the active buffer.
    pub fn clear_active_buffer(&self) {
        if let Ok(mut state) = self.inner.write() {
            state.active_file = None;
            state.language = None;
            state.buffer = None;
            state.symbols.clear();
        }
    }

    /// Updates cursor coordinates (zero-based line and column).
    pub fn set_cursor(&self, line: u32, column: u32) {
        if let Ok(mut state) = self.inner.write() {
            state.cursor_line = line;
            state.cursor_column = column;
        }
    }

    /// Updates the AST/JVM symbol outline and classpath entry count.
    pub fn set_jvm_metadata(&self, symbols: Vec<AstSymbol>, jvm_classpath_count: u32) {
        if let Ok(mut state) = self.inner.write() {
            state.symbols = symbols;
            state.jvm_classpath_count = jvm_classpath_count;
        }
    }

    /// Reads the current active buffer text if one is open.
    #[must_use]
    pub fn active_text(&self) -> Option<String> {
        let state = self.inner.read().ok()?;
        state.buffer.as_ref().map(|buf| buf.snapshot().text())
    }
}

impl EditorContextBridge for InMemoryEditorBridge {
    fn snapshot(&self) -> EditorSnapshot {
        let Ok(state) = self.inner.read() else {
            return EditorSnapshot {
                workspace_root: None,
                active_file: None,
                language: None,
                buffer_version: 0,
                content: None,
                line_count: 0,
                cursor_line: 0,
                cursor_column: 0,
                symbols: Vec::new(),
                jvm_classpath_count: 0,
            };
        };

        let (buffer_version, content, line_count) = match &state.buffer {
            Some(buf) => {
                // O(1) Rope snapshot clone via Arc inside `BufferSnapshot`.
                let snap = buf.snapshot();
                let lines = u32::try_from(snap.len_lines()).unwrap_or(u32::MAX);
                (snap.version().as_u64(), Some(snap.text()), lines)
            }
            None => (0, None, 0),
        };

        EditorSnapshot {
            workspace_root: state
                .workspace_root
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            active_file: state
                .active_file
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            language: state.language.clone(),
            buffer_version,
            content,
            line_count,
            cursor_line: state.cursor_line,
            cursor_column: state.cursor_column,
            symbols: state.symbols.clone(),
            jvm_classpath_count: state.jvm_classpath_count,
        }
    }

    fn apply_edit(&self, start_byte: u64, end_byte: u64, replacement: &str) -> Result<u64, String> {
        let mut state = self
            .inner
            .write()
            .map_err(|_| "editor bridge lock poisoned".to_owned())?;
        let buf = state
            .buffer
            .as_mut()
            .ok_or_else(|| "no active editor buffer".to_owned())?;
        let start = usize::try_from(start_byte)
            .map_err(|_| "start_byte overflows platform usize".to_owned())?;
        let end = usize::try_from(end_byte)
            .map_err(|_| "end_byte overflows platform usize".to_owned())?;
        let version = buf
            .apply_edit(start..end, replacement)
            .map_err(|err| err.to_string())?;
        Ok(version.as_u64())
    }
}

/// Per-plugin Wasmtime `Store` state holding WASI context, resource limiter,
/// permissions, and host-side registrations.
pub struct PluginStoreState {
    plugin_id: String,
    permissions: PluginPermissions,
    wasi: WasiCtx,
    table: ResourceTable,
    limiter: PluginResourceLimiter,
    editor_bridge: Arc<dyn EditorContextBridge>,
    registered_commands: Vec<CommandDescriptor>,
    registered_tool_windows: Vec<ToolWindowDescriptor>,
    logs: VecDeque<PluginLogEntry>,
    max_commands: usize,
    max_tool_windows: usize,
}

impl std::fmt::Debug for PluginStoreState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginStoreState")
            .field("plugin_id", &self.plugin_id)
            .field("permissions", &self.permissions)
            .field("registered_commands", &self.registered_commands.len())
            .field(
                "registered_tool_windows",
                &self.registered_tool_windows.len(),
            )
            .field("logs", &self.logs.len())
            .finish_non_exhaustive()
    }
}

impl PluginStoreState {
    /// Creates a new isolated store state for a plugin instance.
    #[must_use]
    pub fn new(
        plugin_id: String,
        permissions: PluginPermissions,
        wasi: WasiCtx,
        limiter: PluginResourceLimiter,
        editor_bridge: Arc<dyn EditorContextBridge>,
        max_commands: usize,
        max_tool_windows: usize,
    ) -> Self {
        Self {
            plugin_id,
            permissions,
            wasi,
            table: ResourceTable::new(),
            limiter,
            editor_bridge,
            registered_commands: Vec::new(),
            registered_tool_windows: Vec::new(),
            logs: VecDeque::new(),
            max_commands,
            max_tool_windows,
        }
    }

    /// Mutable access to the plugin's [`PluginResourceLimiter`] for `Store::limiter`.
    pub(crate) fn limiter_mut(&mut self) -> &mut dyn wasmtime::ResourceLimiter {
        &mut self.limiter
    }

    /// Peak linear memory recorded for this store in bytes.
    #[must_use]
    pub const fn peak_memory_bytes(&self) -> usize {
        self.limiter.peak_memory_bytes()
    }

    /// Commands registered by this plugin instance.
    #[must_use]
    pub fn registered_commands(&self) -> &[CommandDescriptor] {
        &self.registered_commands
    }

    /// Tool windows registered by this plugin instance.
    #[must_use]
    pub fn registered_tool_windows(&self) -> &[ToolWindowDescriptor] {
        &self.registered_tool_windows
    }

    /// Bounded log history emitted by this plugin instance.
    #[must_use]
    pub fn logs(&self) -> Vec<PluginLogEntry> {
        self.logs.iter().cloned().collect()
    }
}

impl WasiView for PluginStoreState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl crate::bindings::raw::kova::plugin::types::Host for PluginStoreState {}

impl Host for PluginStoreState {
    fn log(&mut self, level: LogLevel, message: String) {
        match level {
            LogLevel::Debug => tracing::debug!(plugin_id = %self.plugin_id, "{message}"),
            LogLevel::Info => tracing::info!(plugin_id = %self.plugin_id, "{message}"),
            LogLevel::Warn => tracing::warn!(plugin_id = %self.plugin_id, "{message}"),
            LogLevel::Error => tracing::error!(plugin_id = %self.plugin_id, "{message}"),
        }

        if self.logs.len() >= MAX_PLUGIN_LOG_ENTRIES {
            drop(self.logs.pop_front());
        }
        self.logs.push_back(PluginLogEntry { level, message });
    }

    fn get_editor_snapshot(&mut self) -> Result<EditorSnapshot, String> {
        if !self.permissions.editor_read {
            return Err(format!(
                "permission denied: plugin `{}` lacks `editor_read` permission",
                self.plugin_id
            ));
        }
        Ok(self.editor_bridge.snapshot())
    }

    fn apply_editor_edit(
        &mut self,
        start_byte: u64,
        end_byte: u64,
        replacement: String,
    ) -> Result<u64, String> {
        if !self.permissions.editor_write {
            return Err(format!(
                "permission denied: plugin `{}` lacks `editor_write` permission",
                self.plugin_id
            ));
        }
        self.editor_bridge
            .apply_edit(start_byte, end_byte, &replacement)
    }

    fn register_command(&mut self, cmd: CommandDescriptor) -> Result<(), String> {
        if cmd.id.trim().is_empty() || cmd.title.trim().is_empty() {
            return Err("command `id` and `title` must be non-empty".to_owned());
        }
        if self.registered_commands.len() >= self.max_commands {
            return Err(format!(
                "command registration limit ({}) reached",
                self.max_commands
            ));
        }
        if self
            .registered_commands
            .iter()
            .any(|existing| existing.id == cmd.id)
        {
            return Err(format!("duplicate command id `{}`", cmd.id));
        }
        self.registered_commands.push(cmd);
        Ok(())
    }

    fn register_tool_window(&mut self, panel: ToolWindowDescriptor) -> Result<(), String> {
        if panel.id.trim().is_empty() || panel.title.trim().is_empty() {
            return Err("tool window `id` and `title` must be non-empty".to_owned());
        }
        if self.registered_tool_windows.len() >= self.max_tool_windows {
            return Err(format!(
                "tool window registration limit ({}) reached",
                self.max_tool_windows
            ));
        }
        if self
            .registered_tool_windows
            .iter()
            .any(|existing| existing.id == panel.id)
        {
            return Err(format!("duplicate tool window id `{}`", panel.id));
        }
        self.registered_tool_windows.push(panel);
        Ok(())
    }
}
