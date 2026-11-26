//! Headless model behind the Settings view (`Cmd+,`, `KOVA-029`).
//!
//! [`SettingsFormModel`] wraps a lossless [`SettingsDocument`] and owns everything the
//! view needs that does not touch the GPU: the 26 [`SettingDescriptor`]s, the search
//! filter, draft text of numeric / text fields, per-field validation errors, the
//! translation of control input into [`SettingUpdate`]s, and the SHA-256 echo
//! suppression that stops the config watcher from reloading the form with its own writes.
//!
//! The model never touches the filesystem: a successful edit returns the serialized TOML,
//! which the caller hands to an asynchronous writer.

use std::collections::{HashMap, VecDeque};

use kova_config::{ConfigError, KeymapPreset, KovaSettings, SettingUpdate, SettingsDocument};
use sha2::{Digest as _, Sha256};

/// How many recent self-written `settings.toml` hashes are remembered, so a watcher echo of
/// an older write that arrives after a newer edit is still recognised.
const SELF_WRITE_HISTORY: usize = 8;

/// Decimal precision used when stepping or parsing floating point settings (`1.45 + 0.05`
/// must be written as `1.5`, not `1.5000000000000002`).
const FLOAT_ROUNDING: f64 = 10_000.0;

/// Settings categories listed in the left navigation column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SettingsCategory {
    /// `[editor]` font, indentation and gutter options.
    Editor,
    /// `[ui]` theme and Islands layout.
    Appearance,
    /// `editor.keymap` / `editor.vim_mode` (plus a link to `keymap.toml`).
    KeymapAndVim,
    /// `[jvm]` Omakase toolchain options.
    JvmOmakase,
    /// `[terminal]` built-in terminal options.
    Terminal,
}

impl SettingsCategory {
    /// All categories in navigation order.
    pub const ALL: [Self; 5] = [
        Self::Editor,
        Self::Appearance,
        Self::KeymapAndVim,
        Self::JvmOmakase,
        Self::Terminal,
    ];

    /// Navigation label.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Editor => "Editor",
            Self::Appearance => "Appearance & Islands",
            Self::KeymapAndVim => "Keymap & Vim",
            Self::JvmOmakase => "JVM & Omakase",
            Self::Terminal => "Terminal",
        }
    }

    /// TOML table hint shown next to the category title.
    #[must_use]
    pub const fn toml_table(self) -> &'static str {
        match self {
            Self::Editor | Self::KeymapAndVim => "[editor]",
            Self::Appearance => "[ui]",
            Self::JvmOmakase => "[jvm]",
            Self::Terminal => "[terminal]",
        }
    }

    /// The category after (`forward`) or before this one, wrapping around.
    #[must_use]
    pub fn cycle(self, forward: bool) -> Self {
        let len = Self::ALL.len();
        let idx = Self::ALL.iter().position(|c| *c == self).unwrap_or(0);
        let next = if forward {
            (idx + 1) % len
        } else {
            (idx + len - 1) % len
        };
        Self::ALL[next]
    }
}

/// Source of the options of a [`ControlKind::Select`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectSource {
    /// Theme ids from `kova_theme::ThemeRegistry` (built-ins and `~/.config/kova/themes/`).
    Themes,
    /// `KeymapPreset` values (`jetbrains`, `vim`, `custom`).
    Keymap,
}

/// Which control renders a setting and how its input is parsed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ControlKind {
    /// Boolean `ToggleSwitch`.
    Toggle,
    /// Numeric field with an inclusive range and a step for the `−` / `+` buttons.
    Number {
        /// Lowest accepted value.
        min: f64,
        /// Highest accepted value.
        max: f64,
        /// Increment of one step.
        step: f64,
        /// Whether only whole numbers are accepted.
        integer: bool,
    },
    /// Single-line text field; `optional` fields map an empty input to "unset".
    Text {
        /// Whether clearing the field removes the key (`None`).
        optional: bool,
    },
    /// Drop-down choice.
    Select(SelectSource),
}

/// Static description of one `settings.toml` key shown in the Settings view.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SettingDescriptor {
    /// Dotted TOML key (`editor.font_size`).
    pub dotted_key: &'static str,
    /// Category the setting is listed under.
    pub category: SettingsCategory,
    /// Human-readable title.
    pub title: &'static str,
    /// One-line description.
    pub description: &'static str,
    /// Control used to edit the value.
    pub control: ControlKind,
}

impl SettingDescriptor {
    /// Whether the descriptor matches a lower-cased search `query` by title, description or
    /// TOML key. An empty query matches everything.
    #[must_use]
    pub fn matches(&self, query: &str) -> bool {
        if query.is_empty() {
            return true;
        }
        self.dotted_key.to_lowercase().contains(query)
            || self.title.to_lowercase().contains(query)
            || self.description.to_lowercase().contains(query)
    }
}

const fn toggle(
    dotted_key: &'static str,
    category: SettingsCategory,
    title: &'static str,
    description: &'static str,
) -> SettingDescriptor {
    SettingDescriptor {
        dotted_key,
        category,
        title,
        description,
        control: ControlKind::Toggle,
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "const table constructor mirroring the descriptor fields"
)]
const fn number(
    dotted_key: &'static str,
    category: SettingsCategory,
    title: &'static str,
    description: &'static str,
    min: f64,
    max: f64,
    step: f64,
    integer: bool,
) -> SettingDescriptor {
    SettingDescriptor {
        dotted_key,
        category,
        title,
        description,
        control: ControlKind::Number {
            min,
            max,
            step,
            integer,
        },
    }
}

const fn text(
    dotted_key: &'static str,
    category: SettingsCategory,
    title: &'static str,
    description: &'static str,
    optional: bool,
) -> SettingDescriptor {
    SettingDescriptor {
        dotted_key,
        category,
        title,
        description,
        control: ControlKind::Text { optional },
    }
}

const fn select(
    dotted_key: &'static str,
    category: SettingsCategory,
    title: &'static str,
    description: &'static str,
    source: SelectSource,
) -> SettingDescriptor {
    SettingDescriptor {
        dotted_key,
        category,
        title,
        description,
        control: ControlKind::Select(source),
    }
}

use SettingsCategory::{Appearance, Editor, JvmOmakase, KeymapAndVim, Terminal};

/// Every `KovaSettings` field (`KOVA-006`) exposed in the Settings view, in display order.
pub static SETTING_DESCRIPTORS: [SettingDescriptor; 26] = [
    text(
        "editor.font_family",
        Editor,
        "Font family",
        "Monospace font used by the code editor.",
        false,
    ),
    number(
        "editor.font_size",
        Editor,
        "Font size",
        "Editor font size in points.",
        6.0,
        72.0,
        0.5,
        false,
    ),
    number(
        "editor.line_height",
        Editor,
        "Line height",
        "Line height as a multiple of the font size.",
        1.0,
        3.0,
        0.05,
        false,
    ),
    number(
        "editor.tab_size",
        Editor,
        "Tab size",
        "Visual width of a tab, in spaces.",
        1.0,
        16.0,
        1.0,
        true,
    ),
    toggle(
        "editor.insert_spaces",
        Editor,
        "Insert spaces",
        "Tab inserts spaces instead of a tab character.",
    ),
    toggle(
        "editor.show_line_numbers",
        Editor,
        "Line numbers",
        "Show line numbers in the gutter.",
    ),
    toggle(
        "editor.show_indent_guides",
        Editor,
        "Indent guides",
        "Draw vertical indentation guides.",
    ),
    toggle(
        "editor.show_inlay_hints",
        Editor,
        "Inlay hints",
        "Show inline type and argument hints from the language server.",
    ),
    toggle(
        "editor.format_on_save",
        Editor,
        "Format on save",
        "Format the buffer before every save.",
    ),
    select(
        "ui.theme",
        Appearance,
        "Theme",
        "Active Aether theme (built-in or from ~/.config/kova/themes/).",
        SelectSource::Themes,
    ),
    toggle(
        "ui.focus_mode",
        Appearance,
        "Tiling density",
        "Start in the compact Omarchy tiling layout.",
    ),
    toggle(
        "ui.show_status_bar",
        Appearance,
        "Status bar",
        "Show the status bar at the bottom of the window.",
    ),
    number(
        "ui.sidebar_width",
        Appearance,
        "Sidebar width",
        "Width of the side tool window islands, in pixels.",
        120.0,
        800.0,
        10.0,
        false,
    ),
    number(
        "ui.bottom_panel_height",
        Appearance,
        "Bottom panel height",
        "Height of the bottom tool window island, in pixels.",
        100.0,
        800.0,
        10.0,
        false,
    ),
    select(
        "editor.keymap",
        KeymapAndVim,
        "Keymap",
        "Keyboard shortcut preset; Custom reads ~/.config/kova/keymap.toml.",
        SelectSource::Keymap,
    ),
    toggle(
        "editor.vim_mode",
        KeymapAndVim,
        "Vim mode",
        "Modal Vim editing in every editor.",
    ),
    number(
        "jvm.jdtls_max_ram_mb",
        JvmOmakase,
        "jdt.ls max RAM (MiB)",
        "Heap limit passed to the Java language server.",
        256.0,
        65_536.0,
        256.0,
        true,
    ),
    number(
        "jvm.auto_hibernate_minutes",
        JvmOmakase,
        "Auto-hibernate (minutes)",
        "Idle minutes before jdt.ls is suspended (0 = disabled).",
        0.0,
        1440.0,
        5.0,
        true,
    ),
    toggle(
        "jvm.auto_decompile_class",
        JvmOmakase,
        "Decompile .class files",
        "Decompile library classes with Vineflower on Go to Definition.",
    ),
    toggle(
        "jvm.lombok_enabled",
        JvmOmakase,
        "Lombok",
        "Attach the Lombok agent to jdt.ls.",
    ),
    text(
        "jvm.preferred_jdk",
        JvmOmakase,
        "Preferred JDK",
        "JDK identifier or path; leave empty for automatic detection.",
        true,
    ),
    toggle(
        "jvm.gradle_offline",
        JvmOmakase,
        "Gradle offline",
        "Run Gradle with --offline by default.",
    ),
    text(
        "terminal.shell",
        Terminal,
        "Shell",
        "Shell executable; leave empty for the system default.",
        true,
    ),
    number(
        "terminal.font_size",
        Terminal,
        "Font size",
        "Terminal font size in points.",
        6.0,
        72.0,
        0.5,
        false,
    ),
    toggle(
        "terminal.inject_java_home",
        Terminal,
        "Inject JAVA_HOME",
        "Export the project JDK as JAVA_HOME in terminal sessions.",
    ),
    number(
        "terminal.scrollback_lines",
        Terminal,
        "Scrollback lines",
        "Lines of history kept by the terminal.",
        100.0,
        1_000_000.0,
        1000.0,
        true,
    ),
];

/// Looks up the descriptor of `dotted_key`.
#[must_use]
pub fn descriptor(dotted_key: &str) -> Option<&'static SettingDescriptor> {
    SETTING_DESCRIPTORS
        .iter()
        .find(|d| d.dotted_key == dotted_key)
}

/// Validation or parse failure of one field; the document on disk stays unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingFieldError {
    /// Dotted key of the rejected field.
    pub dotted_key: &'static str,
    /// Message shown under the control.
    pub message: String,
}

/// Typed value of one setting, as displayed by its control.
#[derive(Debug, Clone, PartialEq)]
pub enum SettingValue {
    /// Toggle state.
    Bool(bool),
    /// Numeric value.
    Number(f64),
    /// Text value (`None` = unset optional key).
    Text(Option<String>),
    /// Selected option id.
    Choice(String),
}

/// Result of feeding a `settings.toml` source from the watcher into the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReloadOutcome {
    /// The source matches one of our own recent writes; nothing changed.
    SelfWriteEcho,
    /// The form now shows the new source.
    Reloaded,
    /// The source is invalid; the form keeps the last valid settings and shows a banner.
    Invalid(String),
}

/// Headless state of the Settings view.
#[derive(Debug, Clone)]
pub struct SettingsFormModel {
    document: SettingsDocument,
    drafts: HashMap<&'static str, String>,
    errors: HashMap<&'static str, String>,
    filter_query: String,
    self_writes: VecDeque<[u8; 32]>,
    load_error: Option<String>,
    theme_ids: Vec<String>,
}

impl SettingsFormModel {
    /// Creates a model over an already validated `document`.
    #[must_use]
    pub fn new(document: SettingsDocument) -> Self {
        Self {
            document,
            drafts: HashMap::new(),
            errors: HashMap::new(),
            filter_query: String::new(),
            self_writes: VecDeque::with_capacity(SELF_WRITE_HISTORY),
            load_error: None,
            theme_ids: Vec::new(),
        }
    }

    /// Parses `toml_source` into a model.
    ///
    /// # Errors
    ///
    /// Returns the [`ConfigError`] when the source is not a valid `settings.toml`.
    pub fn from_toml(toml_source: &str) -> Result<Self, ConfigError> {
        SettingsDocument::from_toml_str(toml_source).map(Self::new)
    }

    /// Model seeded with the commented Omakase default template.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] only if the built-in template is invalid.
    pub fn omakase_default() -> Result<Self, ConfigError> {
        SettingsDocument::default_template().map(Self::new)
    }

    /// Current validated settings.
    #[must_use]
    pub fn settings(&self) -> &KovaSettings {
        self.document.settings()
    }

    /// Current lossless document.
    #[must_use]
    pub const fn document(&self) -> &SettingsDocument {
        &self.document
    }

    /// Serialized `settings.toml` of the current document.
    #[must_use]
    pub fn to_toml_string(&self) -> String {
        self.document.to_toml_string()
    }

    /// Error of the last rejected external reload (shown as a banner), if any.
    #[must_use]
    pub fn load_error(&self) -> Option<&str> {
        self.load_error.as_deref()
    }

    /// Marks the source as unreadable (e.g. a syntax error reported by the watcher).
    pub fn set_load_error(&mut self, message: Option<String>) {
        self.load_error = message;
    }

    /// Theme ids offered by the `ui.theme` select.
    pub fn set_theme_ids(&mut self, ids: Vec<String>) {
        self.theme_ids = ids;
    }

    /// Current search text.
    #[must_use]
    pub fn filter_query(&self) -> &str {
        &self.filter_query
    }

    /// Replaces the search text.
    pub fn set_filter_query(&mut self, query: impl Into<String>) {
        self.filter_query = query.into();
    }

    /// Validation message shown under `dotted_key`, if its last input was rejected.
    #[must_use]
    pub fn error_for(&self, dotted_key: &str) -> Option<&str> {
        self.errors.get(dotted_key).map(String::as_str)
    }

    /// Descriptors of `category` that match the search text.
    #[must_use]
    pub fn visible_descriptors(
        &self,
        category: SettingsCategory,
    ) -> Vec<&'static SettingDescriptor> {
        let query = self.filter_query.trim().to_lowercase();
        SETTING_DESCRIPTORS
            .iter()
            .filter(|d| d.category == category && d.matches(&query))
            .collect()
    }

    /// Descriptors of every category that match the search text, in display order.
    #[must_use]
    pub fn search_results(&self) -> Vec<&'static SettingDescriptor> {
        let query = self.filter_query.trim().to_lowercase();
        SettingsCategory::ALL
            .iter()
            .flat_map(|category| {
                SETTING_DESCRIPTORS
                    .iter()
                    .filter(move |d| d.category == *category)
            })
            .filter(|d| d.matches(&query))
            .collect()
    }

    /// Current typed value of `dotted_key` (`None` for unknown keys).
    #[must_use]
    pub fn value(&self, dotted_key: &str) -> Option<SettingValue> {
        value_of(self.settings(), dotted_key)
    }

    /// Whether `dotted_key` holds its Omakase default.
    #[must_use]
    pub fn is_default(&self, dotted_key: &str) -> bool {
        self.value(dotted_key) == value_of(&KovaSettings::default(), dotted_key)
    }

    /// Text shown in a text / numeric field: the pending draft, or the current value.
    #[must_use]
    pub fn display_text(&self, dotted_key: &str) -> String {
        if let Some(draft) = self.drafts.get(dotted_key) {
            return draft.clone();
        }
        self.value(dotted_key).map(format_value).unwrap_or_default()
    }

    /// Stores unsubmitted text for a text / numeric field.
    pub fn set_draft(&mut self, dotted_key: &'static str, text: impl Into<String>) {
        self.drafts.insert(dotted_key, text.into());
    }

    /// Drops the draft and error of `dotted_key` (`Esc`).
    pub fn discard_draft(&mut self, dotted_key: &str) {
        self.drafts.remove(dotted_key);
        self.errors.remove(dotted_key);
    }

    /// Whether `dotted_key` has unsubmitted text.
    #[must_use]
    pub fn has_draft(&self, dotted_key: &str) -> bool {
        self.drafts.contains_key(dotted_key)
    }

    /// Options of a select field as `(id, label)` pairs.
    #[must_use]
    pub fn select_options(&self, dotted_key: &str) -> Vec<(String, String)> {
        match descriptor(dotted_key).map(|d| d.control) {
            Some(ControlKind::Select(SelectSource::Keymap)) => vec![
                ("jetbrains".to_owned(), "JetBrains".to_owned()),
                ("vim".to_owned(), "Vim".to_owned()),
                ("custom".to_owned(), "Custom (keymap.toml)".to_owned()),
            ],
            Some(ControlKind::Select(SelectSource::Themes)) => {
                let mut ids = self.theme_ids.clone();
                for builtin in ["kova-islands-dark", "kova-light"] {
                    if !ids.iter().any(|id| id == builtin) {
                        ids.push(builtin.to_owned());
                    }
                }
                let current = self.settings().ui.theme.clone();
                if !ids.contains(&current) {
                    ids.push(current);
                }
                ids.sort();
                ids.into_iter().map(|id| (id.clone(), id)).collect()
            }
            _ => Vec::new(),
        }
    }

    /// Applies a GUI edit to the document, preserving comments and formatting.
    ///
    /// On success returns the serialized TOML to persist and remembers its hash so the
    /// watcher echo of that write is ignored.
    ///
    /// # Errors
    ///
    /// Returns [`SettingFieldError`] (also stored for [`Self::error_for`]) when the update is
    /// rejected; the document is left untouched.
    pub fn apply_gui_update(&mut self, update: SettingUpdate) -> Result<String, SettingFieldError> {
        let key = update_key(&update);
        match self.document.set_setting(update) {
            Ok(()) => {
                self.drafts.remove(key);
                self.errors.remove(key);
                let serialized = self.document.to_toml_string();
                self.record_self_write(&serialized);
                Ok(serialized)
            }
            Err(err) => {
                let message = field_message(&err);
                self.errors.insert(key, message.clone());
                Err(SettingFieldError {
                    dotted_key: key,
                    message,
                })
            }
        }
    }

    /// Parses `input` for the text / numeric field `dotted_key` and applies it.
    ///
    /// # Errors
    ///
    /// Returns [`SettingFieldError`] when the text does not parse or is out of range; the draft
    /// is kept so the user can correct it.
    pub fn commit_text(
        &mut self,
        dotted_key: &str,
        input: &str,
    ) -> Result<String, SettingFieldError> {
        let Some(desc) = descriptor(dotted_key) else {
            return Err(unknown_key(dotted_key));
        };
        let key = desc.dotted_key;
        let value = match desc.control {
            ControlKind::Number {
                min, max, integer, ..
            } => match parse_number(input, min, max, integer) {
                Ok(v) => SettingValue::Number(v),
                Err(message) => return Err(self.reject(key, input, message)),
            },
            ControlKind::Text { optional } => {
                let trimmed = input.trim();
                if trimmed.is_empty() {
                    if optional {
                        SettingValue::Text(None)
                    } else {
                        return Err(self.reject(key, input, "must not be empty".to_owned()));
                    }
                } else {
                    SettingValue::Text(Some(trimmed.to_owned()))
                }
            }
            ControlKind::Toggle | ControlKind::Select(_) => {
                return Err(unknown_key(dotted_key));
            }
        };
        let update = match update_for(key, value) {
            Ok(update) => update,
            Err(err) => return Err(self.reject(key, input, err.message)),
        };
        let result = self.apply_gui_update(update);
        if result.is_err() {
            self.drafts.insert(key, input.to_owned());
        }
        result
    }

    /// Flips a toggle.
    ///
    /// # Errors
    ///
    /// Returns [`SettingFieldError`] when `dotted_key` is not a toggle.
    pub fn toggle(&mut self, dotted_key: &str) -> Result<String, SettingFieldError> {
        let Some(SettingValue::Bool(current)) = self.value(dotted_key) else {
            return Err(unknown_key(dotted_key));
        };
        let key = descriptor(dotted_key).map_or("", |d| d.dotted_key);
        let update = update_for(key, SettingValue::Bool(!current))?;
        self.apply_gui_update(update)
    }

    /// Moves a numeric field one step up (`forward`) or down, clamped to its range.
    ///
    /// # Errors
    ///
    /// Returns [`SettingFieldError`] when `dotted_key` is not numeric.
    pub fn step(&mut self, dotted_key: &str, forward: bool) -> Result<String, SettingFieldError> {
        let Some(desc) = descriptor(dotted_key) else {
            return Err(unknown_key(dotted_key));
        };
        let (ControlKind::Number { min, max, step, .. }, Some(SettingValue::Number(current))) =
            (desc.control, self.value(dotted_key))
        else {
            return Err(unknown_key(dotted_key));
        };
        let delta = if forward { step } else { -step };
        let next = round_float((current + delta).clamp(min, max));
        let update = update_for(desc.dotted_key, SettingValue::Number(next))?;
        self.apply_gui_update(update)
    }

    /// Selects option `option_id` of a select field.
    ///
    /// # Errors
    ///
    /// Returns [`SettingFieldError`] when the option is unknown or rejected.
    pub fn select(
        &mut self,
        dotted_key: &str,
        option_id: &str,
    ) -> Result<String, SettingFieldError> {
        let Some(desc) = descriptor(dotted_key) else {
            return Err(unknown_key(dotted_key));
        };
        let update = update_for(desc.dotted_key, SettingValue::Choice(option_id.to_owned()))?;
        self.apply_gui_update(update)
    }

    /// Selects the option after (`forward`) or before the current one, wrapping around.
    ///
    /// # Errors
    ///
    /// Returns [`SettingFieldError`] when `dotted_key` is not a select field.
    pub fn cycle_select(
        &mut self,
        dotted_key: &str,
        forward: bool,
    ) -> Result<String, SettingFieldError> {
        let options = self.select_options(dotted_key);
        let Some(SettingValue::Choice(current)) = self.value(dotted_key) else {
            return Err(unknown_key(dotted_key));
        };
        if options.is_empty() {
            return Err(unknown_key(dotted_key));
        }
        let len = options.len();
        let idx = options
            .iter()
            .position(|(id, _)| *id == current)
            .unwrap_or(0);
        let next = if forward {
            (idx + 1) % len
        } else {
            (idx + len - 1) % len
        };
        let id = options[next].0.clone();
        self.select(dotted_key, &id)
    }

    /// Restores the Omakase default of `dotted_key`.
    ///
    /// # Errors
    ///
    /// Returns [`SettingFieldError`] for unknown keys.
    pub fn reset_to_default(&mut self, dotted_key: &str) -> Result<String, SettingFieldError> {
        let Some(desc) = descriptor(dotted_key) else {
            return Err(unknown_key(dotted_key));
        };
        let Some(default) = value_of(&KovaSettings::default(), dotted_key) else {
            return Err(unknown_key(dotted_key));
        };
        let update = update_for(desc.dotted_key, default)?;
        self.apply_gui_update(update)
    }

    /// Replaces the form with an externally edited `toml_source` (watcher reload).
    ///
    /// Drafts of every field except `editing` are dropped so the controls show the new values.
    ///
    /// # Errors
    ///
    /// Returns the parse / validation message when the source is invalid; the current
    /// settings stay active and the message is kept for the banner.
    pub fn reload_from_toml(&mut self, toml_source: &str) -> Result<(), String> {
        self.reload_keeping(toml_source, None)
    }

    /// [`Self::reload_from_toml`] that keeps the draft of the field being edited.
    ///
    /// # Errors
    ///
    /// Returns the parse / validation message when the source is invalid.
    pub fn reload_keeping(
        &mut self,
        toml_source: &str,
        editing: Option<&str>,
    ) -> Result<(), String> {
        match SettingsDocument::from_toml_str(toml_source) {
            Ok(document) => {
                self.document = document;
                self.load_error = None;
                self.errors.clear();
                self.drafts.retain(|key, _| Some(*key) == editing);
                Ok(())
            }
            Err(err) => {
                let message = err.to_string();
                self.load_error = Some(message.clone());
                Err(message)
            }
        }
    }

    /// Feeds a watcher source into the form, ignoring echoes of our own writes.
    pub fn apply_watcher_source(
        &mut self,
        toml_source: &str,
        editing: Option<&str>,
    ) -> ReloadOutcome {
        if self.should_ignore_watcher_event(toml_source) {
            // Our own (valid) document is on disk again: any earlier syntax error is gone.
            self.load_error = None;
            return ReloadOutcome::SelfWriteEcho;
        }
        match self.reload_keeping(toml_source, editing) {
            Ok(()) => {
                // The file now holds someone else's edit: older self-write hashes must not
                // mask a later external revert to exactly one of those contents.
                self.self_writes.clear();
                ReloadOutcome::Reloaded
            }
            Err(message) => ReloadOutcome::Invalid(message),
        }
    }

    /// Remembers the hash of contents this process is about to write.
    pub fn record_self_write(&mut self, contents: &str) {
        let hash = content_hash(contents);
        if self.self_writes.contains(&hash) {
            return;
        }
        if self.self_writes.len() == SELF_WRITE_HISTORY {
            self.self_writes.pop_front();
        }
        self.self_writes.push_back(hash);
    }

    /// Whether `contents` is one of our own recent writes (or identical to the current
    /// document), so a watcher event carrying it must not reload the form.
    #[must_use]
    pub fn should_ignore_watcher_event(&self, contents: &str) -> bool {
        let hash = content_hash(contents);
        self.self_writes.contains(&hash) || contents == self.document.to_toml_string()
    }

    fn reject(&mut self, key: &'static str, input: &str, message: String) -> SettingFieldError {
        self.drafts.insert(key, input.to_owned());
        self.errors.insert(key, message.clone());
        SettingFieldError {
            dotted_key: key,
            message,
        }
    }
}

/// SHA-256 of a `settings.toml` source.
#[must_use]
pub fn content_hash(contents: &str) -> [u8; 32] {
    Sha256::digest(contents.as_bytes()).into()
}

fn unknown_key(dotted_key: &str) -> SettingFieldError {
    SettingFieldError {
        dotted_key: descriptor(dotted_key).map_or("", |d| d.dotted_key),
        message: format!("`{dotted_key}` cannot be edited this way"),
    }
}

fn field_message(err: &ConfigError) -> String {
    match err {
        ConfigError::Validation { reason, .. } => reason.clone(),
        other => other.to_string(),
    }
}

/// Converts a whole number to `f64`; exact for every value accepted by the settings schema.
#[allow(
    clippy::cast_precision_loss,
    reason = "settings ranges are far below 2^52, where the conversion is exact"
)]
const fn whole_to_f64(value: i64) -> f64 {
    value as f64
}

fn round_float(value: f64) -> f64 {
    (value * FLOAT_ROUNDING).round() / FLOAT_ROUNDING
}

fn parse_number(input: &str, min: f64, max: f64, integer: bool) -> Result<f64, String> {
    let trimmed = input.trim();
    let range = if integer {
        format!("{min:.0}..={max:.0}")
    } else {
        format!("{min}..={max}")
    };
    let value: f64 = if integer {
        trimmed
            .replace('_', "")
            .parse::<i64>()
            .map(whole_to_f64)
            .map_err(|_| format!("expected a whole number in {range}"))?
    } else {
        trimmed
            .parse::<f64>()
            .map_err(|_| format!("expected a number in {range}"))?
    };
    if !value.is_finite() || value < min || value > max {
        return Err(format!("expected {range}, got {trimmed}"));
    }
    Ok(if integer { value } else { round_float(value) })
}

/// Formats a value for a text / numeric field.
#[must_use]
pub fn format_value(value: SettingValue) -> String {
    match value {
        SettingValue::Bool(v) => v.to_string(),
        SettingValue::Number(v) => {
            if v.fract() == 0.0 && v.abs() < 1e15 {
                format!("{v:.0}")
            } else {
                round_float(v).to_string()
            }
        }
        SettingValue::Text(v) => v.unwrap_or_default(),
        SettingValue::Choice(v) => v,
    }
}

const fn keymap_id(preset: KeymapPreset) -> &'static str {
    match preset {
        KeymapPreset::JetBrains => "jetbrains",
        KeymapPreset::Vim => "vim",
        KeymapPreset::Custom => "custom",
    }
}

/// Reads the typed value of `dotted_key` from `settings`.
#[must_use]
pub fn value_of(settings: &KovaSettings, dotted_key: &str) -> Option<SettingValue> {
    use SettingValue::{Bool, Choice, Number, Text};
    let e = &settings.editor;
    let u = &settings.ui;
    let j = &settings.jvm;
    let t = &settings.terminal;
    Some(match dotted_key {
        "editor.font_family" => Text(Some(e.font_family.clone())),
        "editor.font_size" => Number(e.font_size),
        "editor.line_height" => Number(e.line_height),
        "editor.tab_size" => Number(f64::from(e.tab_size)),
        "editor.insert_spaces" => Bool(e.insert_spaces),
        "editor.show_line_numbers" => Bool(e.show_line_numbers),
        "editor.show_indent_guides" => Bool(e.show_indent_guides),
        "editor.show_inlay_hints" => Bool(e.show_inlay_hints),
        "editor.vim_mode" => Bool(e.vim_mode),
        "editor.keymap" => Choice(keymap_id(e.keymap).to_owned()),
        "editor.format_on_save" => Bool(e.format_on_save),
        "ui.theme" => Choice(u.theme.clone()),
        "ui.focus_mode" => Bool(u.focus_mode),
        "ui.show_status_bar" => Bool(u.show_status_bar),
        "ui.sidebar_width" => Number(u.sidebar_width),
        "ui.bottom_panel_height" => Number(u.bottom_panel_height),
        "jvm.jdtls_max_ram_mb" => Number(f64::from(j.jdtls_max_ram_mb)),
        "jvm.auto_hibernate_minutes" => Number(f64::from(j.auto_hibernate_minutes)),
        "jvm.auto_decompile_class" => Bool(j.auto_decompile_class),
        "jvm.lombok_enabled" => Bool(j.lombok_enabled),
        "jvm.preferred_jdk" => Text(j.preferred_jdk.clone()),
        "jvm.gradle_offline" => Bool(j.gradle_offline),
        "terminal.shell" => Text(t.shell.clone()),
        "terminal.font_size" => Number(t.font_size),
        "terminal.inject_java_home" => Bool(t.inject_java_home),
        "terminal.scrollback_lines" => {
            Number(i64::try_from(t.scrollback_lines).map_or(f64::MAX, whole_to_f64))
        }
        _ => return None,
    })
}

/// Dotted key written by `update`.
#[must_use]
pub const fn update_key(update: &SettingUpdate) -> &'static str {
    match update {
        SettingUpdate::EditorFontFamily(_) => "editor.font_family",
        SettingUpdate::EditorFontSize(_) => "editor.font_size",
        SettingUpdate::EditorLineHeight(_) => "editor.line_height",
        SettingUpdate::EditorTabSize(_) => "editor.tab_size",
        SettingUpdate::EditorInsertSpaces(_) => "editor.insert_spaces",
        SettingUpdate::EditorShowLineNumbers(_) => "editor.show_line_numbers",
        SettingUpdate::EditorShowIndentGuides(_) => "editor.show_indent_guides",
        SettingUpdate::EditorShowInlayHints(_) => "editor.show_inlay_hints",
        SettingUpdate::EditorVimMode(_) => "editor.vim_mode",
        SettingUpdate::EditorKeymap(_) => "editor.keymap",
        SettingUpdate::EditorFormatOnSave(_) => "editor.format_on_save",
        SettingUpdate::UiTheme(_) => "ui.theme",
        SettingUpdate::UiFocusMode(_) => "ui.focus_mode",
        SettingUpdate::UiShowStatusBar(_) => "ui.show_status_bar",
        SettingUpdate::UiSidebarWidth(_) => "ui.sidebar_width",
        SettingUpdate::UiBottomPanelHeight(_) => "ui.bottom_panel_height",
        SettingUpdate::JvmJdtlsMaxRamMb(_) => "jvm.jdtls_max_ram_mb",
        SettingUpdate::JvmAutoHibernateMinutes(_) => "jvm.auto_hibernate_minutes",
        SettingUpdate::JvmAutoDecompileClass(_) => "jvm.auto_decompile_class",
        SettingUpdate::JvmLombokEnabled(_) => "jvm.lombok_enabled",
        SettingUpdate::JvmPreferredJdk(_) => "jvm.preferred_jdk",
        SettingUpdate::JvmGradleOffline(_) => "jvm.gradle_offline",
        SettingUpdate::TerminalShell(_) => "terminal.shell",
        SettingUpdate::TerminalFontSize(_) => "terminal.font_size",
        SettingUpdate::TerminalInjectJavaHome(_) => "terminal.inject_java_home",
        SettingUpdate::TerminalScrollbackLines(_) => "terminal.scrollback_lines",
    }
}

fn int_of<T: TryFrom<i64>>(key: &'static str, value: f64) -> Result<T, SettingFieldError> {
    let mismatch = || SettingFieldError {
        dotted_key: key,
        message: format!("expected a whole number, got {value}"),
    };
    if !value.is_finite() || value.fract() != 0.0 || value.abs() > 1e15 {
        return Err(mismatch());
    }
    #[allow(
        clippy::cast_possible_truncation,
        reason = "checked to be a finite whole number below 1e15"
    )]
    let whole = value as i64;
    T::try_from(whole).map_err(|_| SettingFieldError {
        dotted_key: key,
        message: format!("value {whole} is out of range"),
    })
}

/// Builds the [`SettingUpdate`] that writes `value` to `key`.
///
/// # Errors
///
/// Returns [`SettingFieldError`] when `value` has the wrong type for `key` or `key` is unknown.
pub fn update_for(
    key: &'static str,
    value: SettingValue,
) -> Result<SettingUpdate, SettingFieldError> {
    use SettingValue::{Bool, Choice, Number, Text};
    let mismatch = || SettingFieldError {
        dotted_key: key,
        message: format!("unsupported value for `{key}`"),
    };
    Ok(match (key, value) {
        ("editor.font_family", Text(Some(v))) => SettingUpdate::EditorFontFamily(v),
        ("editor.font_size", Number(v)) => SettingUpdate::EditorFontSize(v),
        ("editor.line_height", Number(v)) => SettingUpdate::EditorLineHeight(v),
        ("editor.tab_size", Number(v)) => SettingUpdate::EditorTabSize(int_of(key, v)?),
        ("editor.insert_spaces", Bool(v)) => SettingUpdate::EditorInsertSpaces(v),
        ("editor.show_line_numbers", Bool(v)) => SettingUpdate::EditorShowLineNumbers(v),
        ("editor.show_indent_guides", Bool(v)) => SettingUpdate::EditorShowIndentGuides(v),
        ("editor.show_inlay_hints", Bool(v)) => SettingUpdate::EditorShowInlayHints(v),
        ("editor.vim_mode", Bool(v)) => SettingUpdate::EditorVimMode(v),
        ("editor.keymap", Choice(v)) => SettingUpdate::EditorKeymap(match v.as_str() {
            "jetbrains" => KeymapPreset::JetBrains,
            "vim" => KeymapPreset::Vim,
            "custom" => KeymapPreset::Custom,
            _ => return Err(mismatch()),
        }),
        ("editor.format_on_save", Bool(v)) => SettingUpdate::EditorFormatOnSave(v),
        ("ui.theme", Choice(v)) => SettingUpdate::UiTheme(v),
        ("ui.focus_mode", Bool(v)) => SettingUpdate::UiFocusMode(v),
        ("ui.show_status_bar", Bool(v)) => SettingUpdate::UiShowStatusBar(v),
        ("ui.sidebar_width", Number(v)) => SettingUpdate::UiSidebarWidth(v),
        ("ui.bottom_panel_height", Number(v)) => SettingUpdate::UiBottomPanelHeight(v),
        ("jvm.jdtls_max_ram_mb", Number(v)) => SettingUpdate::JvmJdtlsMaxRamMb(int_of(key, v)?),
        ("jvm.auto_hibernate_minutes", Number(v)) => {
            SettingUpdate::JvmAutoHibernateMinutes(int_of(key, v)?)
        }
        ("jvm.auto_decompile_class", Bool(v)) => SettingUpdate::JvmAutoDecompileClass(v),
        ("jvm.lombok_enabled", Bool(v)) => SettingUpdate::JvmLombokEnabled(v),
        ("jvm.preferred_jdk", Text(v)) => SettingUpdate::JvmPreferredJdk(v),
        ("jvm.gradle_offline", Bool(v)) => SettingUpdate::JvmGradleOffline(v),
        ("terminal.shell", Text(v)) => SettingUpdate::TerminalShell(v),
        ("terminal.font_size", Number(v)) => SettingUpdate::TerminalFontSize(v),
        ("terminal.inject_java_home", Bool(v)) => SettingUpdate::TerminalInjectJavaHome(v),
        ("terminal.scrollback_lines", Number(v)) => {
            SettingUpdate::TerminalScrollbackLines(int_of(key, v)?)
        }
        _ => return Err(mismatch()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMENTED: &str = r#"# My Kova dotfiles — hand written, keep these comments!

[editor]
# favourite font
font_family = "Iosevka"   # inline: do not lose me
font_size = 14.0
line_height = 1.45
tab_size = 4

vim_mode = false # modal editing

[ui]
theme = "kova-islands-dark" # dark first

[jvm]
jdtls_max_ram_mb = 2048 # RAM budget
"#;

    fn model() -> SettingsFormModel {
        SettingsFormModel::from_toml(COMMENTED).expect("valid settings")
    }

    fn assert_comments_kept(toml: &str) {
        for comment in [
            "# My Kova dotfiles — hand written, keep these comments!",
            "# favourite font",
            "# inline: do not lose me",
            "# modal editing",
            "# dark first",
            "# RAM budget",
        ] {
            assert!(toml.contains(comment), "lost `{comment}` in:\n{toml}");
        }
    }

    #[test]
    fn descriptors_cover_all_26_settings_exactly_once() {
        assert_eq!(SETTING_DESCRIPTORS.len(), 26);
        let defaults = KovaSettings::default();
        let mut keys: Vec<_> = SETTING_DESCRIPTORS.iter().map(|d| d.dotted_key).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), 26);
        for desc in &SETTING_DESCRIPTORS {
            let value = value_of(&defaults, desc.dotted_key)
                .unwrap_or_else(|| panic!("no value for {}", desc.dotted_key));
            // Every default value round-trips through its update constructor.
            update_for(desc.dotted_key, value).expect("default update");
        }
        for category in SettingsCategory::ALL {
            let model = SettingsFormModel::omakase_default().expect("template");
            assert_ne!(model.visible_descriptors(category).len(), 0, "{category:?}");
        }
    }

    #[test]
    fn no_ai_or_telemetry_switches() {
        for desc in &SETTING_DESCRIPTORS {
            let text =
                format!("{} {} {}", desc.dotted_key, desc.title, desc.description).to_lowercase();
            for banned in ["telemetry", " ai ", "copilot", "cloud", "account"] {
                assert!(!text.contains(banned), "{text}");
            }
        }
    }

    #[test]
    fn gui_updates_preserve_every_comment() {
        let mut m = model();
        m.toggle("editor.vim_mode").expect("vim");
        m.select("ui.theme", "kova-light").expect("theme");
        m.commit_text("editor.font_size", "15.5")
            .expect("font size");
        let out = m.commit_text("jvm.jdtls_max_ram_mb", "4096").expect("ram");

        assert_comments_kept(&out);
        assert!(out.contains("vim_mode = true # modal editing"), "{out}");
        assert!(out.contains("theme = \"kova-light\" # dark first"), "{out}");
        assert!(out.contains("font_size = 15.5"), "{out}");
        assert!(
            out.contains("jdtls_max_ram_mb = 4096 # RAM budget"),
            "{out}"
        );
        let s = m.settings();
        assert!(s.editor.vim_mode);
        assert_eq!(s.ui.theme, "kova-light");
        assert!((s.editor.font_size - 15.5).abs() < f64::EPSILON);
        assert_eq!(s.jvm.jdtls_max_ram_mb, 4096);
    }

    #[test]
    fn only_the_edited_line_changes() {
        let mut m = model();
        let out = m.toggle("editor.vim_mode").expect("vim");
        let before: Vec<_> = COMMENTED.lines().collect();
        let after: Vec<_> = out.lines().collect();
        assert_eq!(before.len(), after.len());
        let changed: Vec<_> = before.iter().zip(&after).filter(|(a, b)| a != b).collect();
        assert_eq!(changed.len(), 1, "{out}");
    }

    #[test]
    fn invalid_numbers_are_rejected_without_touching_the_document() {
        let mut m = model();
        let before = m.to_toml_string();
        for (key, input) in [
            ("editor.tab_size", "0"),
            ("editor.tab_size", "300"),
            ("editor.font_size", "-10"),
            ("editor.font_size", "NaN"),
            ("editor.font_size", "abc"),
            ("jvm.jdtls_max_ram_mb", "64"),
            ("jvm.jdtls_max_ram_mb", "16"),
            ("terminal.scrollback_lines", "1.5"),
        ] {
            let err = m.commit_text(key, input).expect_err(input);
            assert_eq!(err.dotted_key, key);
            assert!(m.error_for(key).is_some());
            assert_eq!(m.display_text(key), input, "draft kept for correction");
            assert_eq!(
                m.to_toml_string(),
                before,
                "{key} = {input} reached the TOML"
            );
        }
        m.commit_text("editor.tab_size", "2").expect("valid");
        assert_eq!(m.error_for("editor.tab_size"), None);
        assert_eq!(m.display_text("editor.tab_size"), "2");
    }

    #[test]
    fn optional_text_fields_unset_on_empty_input() {
        let mut m = model();
        let out = m
            .commit_text("jvm.preferred_jdk", " temurin-21 ")
            .expect("jdk");
        assert!(out.contains("preferred_jdk = \"temurin-21\""), "{out}");
        assert_eq!(
            m.settings().jvm.preferred_jdk.as_deref(),
            Some("temurin-21")
        );
        let out = m.commit_text("jvm.preferred_jdk", "   ").expect("unset");
        assert!(!out.contains("preferred_jdk"), "{out}");
        assert_eq!(m.settings().jvm.preferred_jdk, None);
        assert_comments_kept(&out);

        m.commit_text("editor.font_family", "")
            .expect_err("required");
        assert_eq!(m.settings().editor.font_family, "Iosevka");
    }

    #[test]
    fn steps_are_rounded_and_clamped() {
        let mut m = model();
        let out = m.step("editor.line_height", true).expect("step");
        assert!(out.contains("line_height = 1.5\n"), "{out}");
        for _ in 0..200 {
            m.step("editor.tab_size", true).expect("step");
        }
        assert_eq!(m.settings().editor.tab_size, 16);
        m.step("editor.tab_size", false).expect("step");
        assert_eq!(m.settings().editor.tab_size, 15);
    }

    #[test]
    fn select_cycles_and_reset_restores_defaults() {
        let mut m = model();
        m.set_theme_ids(vec!["kova-islands-dark".into(), "nord".into()]);
        let ids: Vec<_> = m
            .select_options("ui.theme")
            .into_iter()
            .map(|o| o.0)
            .collect();
        assert_eq!(ids, ["kova-islands-dark", "kova-light", "nord"]);
        m.cycle_select("ui.theme", true).expect("cycle");
        assert_eq!(m.settings().ui.theme, "kova-light");
        m.cycle_select("editor.keymap", false).expect("keymap");
        assert_eq!(m.settings().editor.keymap, KeymapPreset::Custom);

        assert!(!m.is_default("editor.font_family"));
        let out = m.reset_to_default("editor.font_family").expect("reset");
        assert!(out.contains("font_family = \"JetBrains Mono\"   # inline: do not lose me"));
        assert!(m.is_default("editor.font_family"));
    }

    #[test]
    fn search_filters_by_key_title_and_description() {
        let mut m = model();
        m.set_filter_query("ram");
        let keys: Vec<_> = m.search_results().iter().map(|d| d.dotted_key).collect();
        assert_eq!(keys, ["jvm.jdtls_max_ram_mb"]);

        m.set_filter_query("VIM");
        let keys: Vec<_> = m.search_results().iter().map(|d| d.dotted_key).collect();
        assert!(keys.contains(&"editor.vim_mode"));
        assert_eq!(m.visible_descriptors(Editor).len(), 0);

        m.set_filter_query("editor.font");
        let keys: Vec<_> = m.search_results().iter().map(|d| d.dotted_key).collect();
        assert_eq!(keys, ["editor.font_family", "editor.font_size"]);

        m.set_filter_query("");
        assert_eq!(m.search_results().len(), 26);
    }

    #[test]
    fn own_writes_are_suppressed_but_external_edits_reload() {
        let mut m = model();
        let first = m.toggle("editor.vim_mode").expect("first");
        let second = m.commit_text("editor.font_size", "16").expect("second");

        // The echo of the first write arrives after the second edit: must not revert the form.
        assert_eq!(
            m.apply_watcher_source(&first, None),
            ReloadOutcome::SelfWriteEcho
        );
        assert_eq!(
            m.apply_watcher_source(&second, None),
            ReloadOutcome::SelfWriteEcho
        );
        assert!((m.settings().editor.font_size - 16.0).abs() < f64::EPSILON);

        let external = second.replace("vim_mode = true", "vim_mode = false");
        assert!(!m.should_ignore_watcher_event(&external));
        assert_eq!(
            m.apply_watcher_source(&external, None),
            ReloadOutcome::Reloaded
        );
        assert!(!m.settings().editor.vim_mode);
        assert_comments_kept(&m.to_toml_string());
    }

    #[test]
    fn external_revert_to_an_earlier_self_write_reloads() {
        let mut m = model();
        let ours = m.toggle("editor.vim_mode").expect("gui write");
        let external = ours.replace("vim_mode = true", "vim_mode = false");
        assert_eq!(
            m.apply_watcher_source(&external, None),
            ReloadOutcome::Reloaded
        );
        assert!(!m.settings().editor.vim_mode);
        // The user reverts the file to exactly our earlier write.
        assert_eq!(m.apply_watcher_source(&ours, None), ReloadOutcome::Reloaded);
        assert!(m.settings().editor.vim_mode);
    }

    #[test]
    fn restoring_the_current_document_clears_the_error_banner() {
        let mut m = model();
        let current = m.to_toml_string();
        assert!(matches!(
            m.apply_watcher_source("[editor\n", None),
            ReloadOutcome::Invalid(_)
        ));
        assert!(m.load_error().is_some());
        assert_eq!(
            m.apply_watcher_source(&current, None),
            ReloadOutcome::SelfWriteEcho
        );
        assert_eq!(m.load_error(), None);
    }

    #[test]
    fn invalid_external_source_keeps_last_valid_settings() {
        let mut m = model();
        let outcome = m.apply_watcher_source("[editor\nfont_size = ", None);
        assert!(matches!(outcome, ReloadOutcome::Invalid(_)));
        assert!(m.load_error().is_some());
        assert_eq!(m.settings().editor.font_family, "Iosevka");

        let outcome = m.apply_watcher_source("[editor]\ntab_size = 0\n", None);
        assert!(matches!(outcome, ReloadOutcome::Invalid(_)));
        assert_eq!(m.settings().editor.tab_size, 4);

        m.reload_from_toml(COMMENTED).expect("valid again");
        assert_eq!(m.load_error(), None);
    }

    #[test]
    fn external_reload_keeps_the_field_being_edited() {
        let mut m = model();
        m.set_draft("editor.font_size", "1");
        m.set_draft("editor.tab_size", "9");
        let external = COMMENTED.replace("tab_size = 4", "tab_size = 8");
        assert_eq!(
            m.apply_watcher_source(&external, Some("editor.font_size")),
            ReloadOutcome::Reloaded
        );
        assert_eq!(m.display_text("editor.font_size"), "1");
        assert_eq!(m.display_text("editor.tab_size"), "8");
    }

    #[test]
    fn category_cycle_wraps() {
        assert_eq!(Terminal.cycle(true), Editor);
        assert_eq!(Editor.cycle(false), Terminal);
    }
}
