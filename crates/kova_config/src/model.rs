//! Typed configuration schema, Omakase defaults, validation, and patch types
//! for `~/.config/kova/settings.toml` (`KOVA-006`).

use serde::{Deserialize, Serialize};

/// Errors returned while parsing, validating, updating, or persisting
/// Kova configuration files.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// Failed to parse raw TOML syntax into a CST document.
    #[error("failed to parse TOML document: {message}")]
    ParseToml {
        /// Underlying parser error description.
        message: String,
    },
    /// Failed to deserialize TOML into a typed configuration model.
    #[error("failed to deserialize configuration: {message}")]
    Deserialize {
        /// Underlying deserialization error description.
        message: String,
    },
    /// Semantic or range validation failed for a configuration field.
    #[error("invalid configuration value for `{field}`: {reason}")]
    Validation {
        /// Dotted configuration field path.
        field: &'static str,
        /// Human-readable validation failure reason.
        reason: String,
    },
    /// Dotted key path is empty or traverses a non-table value.
    #[error("invalid configuration key path `{path}`: {reason}")]
    InvalidPath {
        /// Target dotted key path.
        path: String,
        /// Explanation of why the path cannot be updated.
        reason: String,
    },
    /// Filesystem I/O failure while reading or atomically writing config files.
    #[error("configuration I/O error at `{path}`: {message}")]
    Io {
        /// Target file path.
        path: String,
        /// Underlying I/O error description.
        message: String,
    },
}

/// Default keymap preset selected in editor preferences.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum KeymapPreset {
    /// `IntelliJ` IDEA / `JetBrains` default keybindings (Kova Omakase default).
    #[default]
    JetBrains,
    /// Modal Vim keybindings as primary keymap.
    Vim,
    /// User-customized keymap overrides (`~/.config/kova/keymap.toml`).
    Custom,
}

/// Complete user preferences loaded from `~/.config/kova/settings.toml`.
///
/// Follows the Zero-Bloat Core doctrine: no telemetry or AI flags live here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct KovaSettings {
    /// Text editor behavior, font, and keymap settings (`[editor]`).
    pub editor: EditorSettings,
    /// Islands UI shell and theme settings (`[ui]`).
    pub ui: UiSettings,
    /// JVM toolchain, JDT.LS RAM limit, and auto-hibernation (`[jvm]`).
    pub jvm: JvmSettings,
    /// Built-in PTY terminal settings (`[terminal]`).
    pub terminal: TerminalSettings,
}

impl KovaSettings {
    /// Validates all numeric bounds and invariants across every settings section.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] if any field is out of its allowed
    /// range or empty when a non-empty value is required.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.editor.validate()?;
        self.ui.validate()?;
        self.jvm.validate()?;
        self.terminal.validate()?;
        Ok(())
    }
}

/// Editor preferences under `[editor]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "TOML configuration schema models independent boolean editor flags"
)]
pub struct EditorSettings {
    /// Editor font family name.
    pub font_family: String,
    /// Editor font size in logical points (`6.0..=72.0`).
    pub font_size: f64,
    /// Line height multiplier (`1.0..=3.0`).
    pub line_height: f64,
    /// Visual tab width in spaces (`1..=16`).
    pub tab_size: u8,
    /// Whether Tab inserts spaces instead of literal `\t`.
    pub insert_spaces: bool,
    /// Whether gutter line numbers are visible.
    pub show_line_numbers: bool,
    /// Whether vertical indent guides are rendered.
    pub show_indent_guides: bool,
    /// Whether LSP inlay hints are rendered inline.
    pub show_inlay_hints: bool,
    /// Whether modal Vim emulation is active.
    pub vim_mode: bool,
    /// Active keymap preset.
    pub keymap: KeymapPreset,
    /// Whether buffers are formatted automatically before saving.
    pub format_on_save: bool,
}

impl Default for EditorSettings {
    fn default() -> Self {
        Self {
            font_family: "JetBrains Mono".to_owned(),
            font_size: 13.0,
            line_height: 1.45,
            tab_size: 4,
            insert_spaces: true,
            show_line_numbers: true,
            show_indent_guides: true,
            show_inlay_hints: true,
            vim_mode: false,
            keymap: KeymapPreset::JetBrains,
            format_on_save: false,
        }
    }
}

impl EditorSettings {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.font_family.trim().is_empty() {
            return Err(ConfigError::Validation {
                field: "editor.font_family",
                reason: "must not be empty".to_owned(),
            });
        }
        validate_finite_range("editor.font_size", self.font_size, 6.0, 72.0)?;
        validate_finite_range("editor.line_height", self.line_height, 1.0, 3.0)?;
        if !(1..=16).contains(&self.tab_size) {
            return Err(ConfigError::Validation {
                field: "editor.tab_size",
                reason: format!("expected 1..=16, got {}", self.tab_size),
            });
        }
        Ok(())
    }
}

/// UI shell and theme preferences under `[ui]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSettings {
    /// Active Aether theme identifier (`kova-islands-dark`, `kova-light`, or custom).
    pub theme: String,
    /// Whether Omarchy Focus / Tiling mode is enabled by default.
    pub focus_mode: bool,
    /// Whether the bottom status bar is visible.
    pub show_status_bar: bool,
    /// Default sidebar island width in logical pixels (`120.0..=800.0`).
    pub sidebar_width: f64,
    /// Default bottom tool-window height in logical pixels (`100.0..=800.0`).
    pub bottom_panel_height: f64,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            theme: "kova-islands-dark".to_owned(),
            focus_mode: false,
            show_status_bar: true,
            sidebar_width: 260.0,
            bottom_panel_height: 240.0,
        }
    }
}

impl UiSettings {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.theme.trim().is_empty() {
            return Err(ConfigError::Validation {
                field: "ui.theme",
                reason: "must not be empty".to_owned(),
            });
        }
        validate_finite_range("ui.sidebar_width", self.sidebar_width, 120.0, 800.0)?;
        validate_finite_range(
            "ui.bottom_panel_height",
            self.bottom_panel_height,
            100.0,
            800.0,
        )?;
        Ok(())
    }
}

/// JVM language server and toolchain preferences under `[jvm]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct JvmSettings {
    /// Maximum heap limit in MiB passed to `jdt.ls` (`256..=65536`).
    pub jdtls_max_ram_mb: u32,
    /// Idle timeout in minutes before hibernating `jdt.ls` (`0` disables, `0..=1440`).
    pub auto_hibernate_minutes: u32,
    /// Automatically decompile `.class` library symbols with Vineflower on Go to Definition.
    pub auto_decompile_class: bool,
    /// Automatically attach `lombok.jar` agent to `jdt.ls`.
    pub lombok_enabled: bool,
    /// Optional preferred JDK identifier or path.
    pub preferred_jdk: Option<String>,
    /// Whether Gradle invocations run in `--offline` mode by default.
    pub gradle_offline: bool,
}

impl Default for JvmSettings {
    fn default() -> Self {
        Self {
            jdtls_max_ram_mb: 2048,
            auto_hibernate_minutes: 15,
            auto_decompile_class: true,
            lombok_enabled: true,
            preferred_jdk: None,
            gradle_offline: false,
        }
    }
}

impl JvmSettings {
    fn validate(&self) -> Result<(), ConfigError> {
        if !(256..=65536).contains(&self.jdtls_max_ram_mb) {
            return Err(ConfigError::Validation {
                field: "jvm.jdtls_max_ram_mb",
                reason: format!("expected 256..=65536 MiB, got {}", self.jdtls_max_ram_mb),
            });
        }
        if self.auto_hibernate_minutes > 1440 {
            return Err(ConfigError::Validation {
                field: "jvm.auto_hibernate_minutes",
                reason: format!(
                    "expected 0..=1440 minutes, got {}",
                    self.auto_hibernate_minutes
                ),
            });
        }
        if let Some(jdk) = &self.preferred_jdk
            && jdk.trim().is_empty()
        {
            return Err(ConfigError::Validation {
                field: "jvm.preferred_jdk",
                reason: "must not be whitespace-only when specified".to_owned(),
            });
        }
        Ok(())
    }
}

/// Built-in terminal preferences under `[terminal]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalSettings {
    /// Optional explicit shell executable path.
    pub shell: Option<String>,
    /// Terminal font size in logical points (`6.0..=72.0`).
    pub font_size: f64,
    /// Whether project `JAVA_HOME` is injected automatically into PTY sessions.
    pub inject_java_home: bool,
    /// Scrollback history capacity in lines (`100..=1_000_000`).
    pub scrollback_lines: usize,
}

impl Default for TerminalSettings {
    fn default() -> Self {
        Self {
            shell: None,
            font_size: 12.5,
            inject_java_home: true,
            scrollback_lines: 10_000,
        }
    }
}

impl TerminalSettings {
    fn validate(&self) -> Result<(), ConfigError> {
        if let Some(shell) = &self.shell
            && shell.trim().is_empty()
        {
            return Err(ConfigError::Validation {
                field: "terminal.shell",
                reason: "must not be whitespace-only when specified".to_owned(),
            });
        }
        validate_finite_range("terminal.font_size", self.font_size, 6.0, 72.0)?;
        if !(100..=1_000_000).contains(&self.scrollback_lines) {
            return Err(ConfigError::Validation {
                field: "terminal.scrollback_lines",
                reason: format!("expected 100..=1_000_000, got {}", self.scrollback_lines),
            });
        }
        Ok(())
    }
}

/// Strongly typed single-setting update emitted by the Settings UI (`Cmd+,`).
#[derive(Debug, Clone, PartialEq)]
pub enum SettingUpdate {
    /// Update `editor.font_family`.
    EditorFontFamily(String),
    /// Update `editor.font_size`.
    EditorFontSize(f64),
    /// Update `editor.line_height`.
    EditorLineHeight(f64),
    /// Update `editor.tab_size`.
    EditorTabSize(u8),
    /// Update `editor.insert_spaces`.
    EditorInsertSpaces(bool),
    /// Update `editor.show_line_numbers`.
    EditorShowLineNumbers(bool),
    /// Update `editor.show_indent_guides`.
    EditorShowIndentGuides(bool),
    /// Update `editor.show_inlay_hints`.
    EditorShowInlayHints(bool),
    /// Update `editor.vim_mode`.
    EditorVimMode(bool),
    /// Update `editor.keymap`.
    EditorKeymap(KeymapPreset),
    /// Update `editor.format_on_save`.
    EditorFormatOnSave(bool),
    /// Update `ui.theme`.
    UiTheme(String),
    /// Update `ui.focus_mode`.
    UiFocusMode(bool),
    /// Update `ui.show_status_bar`.
    UiShowStatusBar(bool),
    /// Update `ui.sidebar_width`.
    UiSidebarWidth(f64),
    /// Update `ui.bottom_panel_height`.
    UiBottomPanelHeight(f64),
    /// Update `jvm.jdtls_max_ram_mb`.
    JvmJdtlsMaxRamMb(u32),
    /// Update `jvm.auto_hibernate_minutes`.
    JvmAutoHibernateMinutes(u32),
    /// Update `jvm.auto_decompile_class`.
    JvmAutoDecompileClass(bool),
    /// Update `jvm.lombok_enabled`.
    JvmLombokEnabled(bool),
    /// Update or clear `jvm.preferred_jdk`.
    JvmPreferredJdk(Option<String>),
    /// Update `jvm.gradle_offline`.
    JvmGradleOffline(bool),
    /// Update or clear `terminal.shell`.
    TerminalShell(Option<String>),
    /// Update `terminal.font_size`.
    TerminalFontSize(f64),
    /// Update `terminal.inject_java_home`.
    TerminalInjectJavaHome(bool),
    /// Update `terminal.scrollback_lines`.
    TerminalScrollbackLines(usize),
}

impl SettingUpdate {
    pub(crate) fn into_patch(
        self,
    ) -> Result<(&'static str, Option<toml_edit::Value>), ConfigError> {
        let patch = match self {
            Self::EditorFontFamily(v) => ("editor.font_family", Some(toml_edit::Value::from(v))),
            Self::EditorFontSize(v) => ("editor.font_size", Some(toml_edit::Value::from(v))),
            Self::EditorLineHeight(v) => ("editor.line_height", Some(toml_edit::Value::from(v))),
            Self::EditorTabSize(v) => (
                "editor.tab_size",
                Some(toml_edit::Value::from(i64::from(v))),
            ),
            Self::EditorInsertSpaces(v) => {
                ("editor.insert_spaces", Some(toml_edit::Value::from(v)))
            }
            Self::EditorShowLineNumbers(v) => {
                ("editor.show_line_numbers", Some(toml_edit::Value::from(v)))
            }
            Self::EditorShowIndentGuides(v) => {
                ("editor.show_indent_guides", Some(toml_edit::Value::from(v)))
            }
            Self::EditorShowInlayHints(v) => {
                ("editor.show_inlay_hints", Some(toml_edit::Value::from(v)))
            }
            Self::EditorVimMode(v) => ("editor.vim_mode", Some(toml_edit::Value::from(v))),
            Self::EditorKeymap(preset) => {
                let s = match preset {
                    KeymapPreset::JetBrains => "jet_brains",
                    KeymapPreset::Vim => "vim",
                    KeymapPreset::Custom => "custom",
                };
                ("editor.keymap", Some(toml_edit::Value::from(s)))
            }
            Self::EditorFormatOnSave(v) => {
                ("editor.format_on_save", Some(toml_edit::Value::from(v)))
            }
            Self::UiTheme(v) => ("ui.theme", Some(toml_edit::Value::from(v))),
            Self::UiFocusMode(v) => ("ui.focus_mode", Some(toml_edit::Value::from(v))),
            Self::UiShowStatusBar(v) => ("ui.show_status_bar", Some(toml_edit::Value::from(v))),
            Self::UiSidebarWidth(v) => ("ui.sidebar_width", Some(toml_edit::Value::from(v))),
            Self::UiBottomPanelHeight(v) => {
                ("ui.bottom_panel_height", Some(toml_edit::Value::from(v)))
            }
            Self::JvmJdtlsMaxRamMb(v) => (
                "jvm.jdtls_max_ram_mb",
                Some(toml_edit::Value::from(i64::from(v))),
            ),
            Self::JvmAutoHibernateMinutes(v) => (
                "jvm.auto_hibernate_minutes",
                Some(toml_edit::Value::from(i64::from(v))),
            ),
            Self::JvmAutoDecompileClass(v) => {
                ("jvm.auto_decompile_class", Some(toml_edit::Value::from(v)))
            }
            Self::JvmLombokEnabled(v) => ("jvm.lombok_enabled", Some(toml_edit::Value::from(v))),
            Self::JvmPreferredJdk(v) => ("jvm.preferred_jdk", v.map(toml_edit::Value::from)),
            Self::JvmGradleOffline(v) => ("jvm.gradle_offline", Some(toml_edit::Value::from(v))),
            Self::TerminalShell(v) => ("terminal.shell", v.map(toml_edit::Value::from)),
            Self::TerminalFontSize(v) => ("terminal.font_size", Some(toml_edit::Value::from(v))),
            Self::TerminalInjectJavaHome(v) => {
                ("terminal.inject_java_home", Some(toml_edit::Value::from(v)))
            }
            Self::TerminalScrollbackLines(v) => {
                let n = i64::try_from(v).map_err(|_| ConfigError::Validation {
                    field: "terminal.scrollback_lines",
                    reason: format!("expected 100..=1_000_000, got {v}"),
                })?;
                ("terminal.scrollback_lines", Some(toml_edit::Value::from(n)))
            }
        };
        Ok(patch)
    }
}

pub(crate) fn validate_finite_range(
    field: &'static str,
    value: f64,
    min: f64,
    max: f64,
) -> Result<(), ConfigError> {
    if !value.is_finite() || value < min || value > max {
        return Err(ConfigError::Validation {
            field,
            reason: format!("expected finite value in {min}..={max}, got {value}"),
        });
    }
    Ok(())
}
