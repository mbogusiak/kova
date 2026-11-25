//! Lossless `toml_edit::DocumentMut` wrapper for `settings.toml` (`KOVA-006`).
//!
//! Updates individual keys in place while preserving user comments, inline
//! comments, blank lines, and key ordering.

use std::io::{ErrorKind, Write as _};
use std::path::Path;

use tempfile::NamedTempFile;
use toml_edit::{DocumentMut, Item, Table, Value};

use crate::model::{ConfigError, KovaSettings, SettingUpdate};

const DEFAULT_SETTINGS_TEMPLATE: &str = r#"# Kova IDE configuration (Omarchy dotfiles)
# Changes made in the Settings UI (Cmd+,) preserve comments and formatting.

[editor]
font_family = "JetBrains Mono"
font_size = 13.0
line_height = 1.45
tab_size = 4
insert_spaces = true
vim_mode = false
keymap = "jet_brains"

[ui]
theme = "kova-islands-dark"
focus_mode = false

[jvm]
jdtls_max_ram_mb = 2048
auto_hibernate_minutes = 15
auto_decompile_class = true
lombok_enabled = true

[terminal]
font_size = 12.5
inject_java_home = true
"#;

/// Bidirectional `settings.toml` document holding both the lossless TOML CST
/// and the validated typed [`KovaSettings`] model.
#[derive(Debug, Clone)]
pub struct SettingsDocument {
    doc: DocumentMut,
    settings: KovaSettings,
}

impl SettingsDocument {
    /// Parses a `settings.toml` string into a lossless [`SettingsDocument`].
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::ParseToml`] on syntax errors,
    /// [`ConfigError::Deserialize`] on type mismatches, or
    /// [`ConfigError::Validation`] if any setting violates allowed bounds.
    pub fn from_toml_str(input: &str) -> Result<Self, ConfigError> {
        let doc = input
            .parse::<DocumentMut>()
            .map_err(|err| ConfigError::ParseToml {
                message: err.to_string(),
            })?;
        let settings = deserialize_and_validate(&doc)?;
        Ok(Self { doc, settings })
    }

    /// Creates a new [`SettingsDocument`] seeded with the commented Omakase
    /// default template.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] only if the built-in template fails validation.
    pub fn default_template() -> Result<Self, ConfigError> {
        Self::from_toml_str(DEFAULT_SETTINGS_TEMPLATE)
    }

    /// Loads `settings.toml` from `path`, or returns [`Self::default_template`]
    /// if the file does not exist yet (without writing anything to disk).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Io`] for unreadable files, or parsing/validation
    /// errors if an existing file is malformed.
    pub fn load_or_default(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(contents) => Self::from_toml_str(&contents),
            Err(err) if err.kind() == ErrorKind::NotFound => Self::default_template(),
            Err(err) => Err(ConfigError::Io {
                path: path.to_string_lossy().into_owned(),
                message: err.to_string(),
            }),
        }
    }

    /// Returns the currently validated typed settings.
    #[must_use]
    pub fn settings(&self) -> &KovaSettings {
        &self.settings
    }

    /// Serializes the CST document back to TOML while preserving all untouched
    /// comments, blank lines, and formatting.
    #[must_use]
    pub fn to_toml_string(&self) -> String {
        self.doc.to_string()
    }

    /// Applies a strongly-typed [`SettingUpdate`] (for example from the GUI)
    /// while preserving existing comments and formatting, rolling back on error.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] or [`ConfigError::InvalidPath`] if
    /// the update produces an invalid configuration.
    pub fn set_setting(&mut self, update: SettingUpdate) -> Result<(), ConfigError> {
        let (path, maybe_val) = update.into_patch()?;
        if let Some(val) = maybe_val {
            self.set_dotted_value(path, val)
        } else {
            let _ = self.unset_dotted_key(path)?;
            Ok(())
        }
    }

    /// Updates a single key by dotted path (such as `"editor.tab_size"`),
    /// creating missing intermediate tables and preserving existing value decor
    /// (spacing and trailing inline comments).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidPath`] if `dotted_path` is empty or
    /// crosses a non-table value, or [`ConfigError::Validation`] /
    /// [`ConfigError::Deserialize`] if the resulting document is invalid.
    pub fn set_dotted_value(&mut self, dotted_path: &str, value: Value) -> Result<(), ConfigError> {
        let segments = parse_dotted_path(dotted_path)?;
        let prev_doc = self.doc.clone();
        let prev_settings = self.settings.clone();

        let apply_result = apply_value_to_doc(&mut self.doc, &segments, value, dotted_path);
        if let Err(err) = apply_result {
            self.doc = prev_doc;
            self.settings = prev_settings;
            return Err(err);
        }

        match deserialize_and_validate(&self.doc) {
            Ok(next_settings) => {
                self.settings = next_settings;
                Ok(())
            }
            Err(err) => {
                self.doc = prev_doc;
                self.settings = prev_settings;
                Err(err)
            }
        }
    }

    /// Removes a key by dotted path while leaving any parent table header and
    /// section comments intact. Returns `true` if the key existed.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidPath`] if `dotted_path` is invalid or
    /// traverses a non-table item.
    pub fn unset_dotted_key(&mut self, dotted_path: &str) -> Result<bool, ConfigError> {
        let segments = parse_dotted_path(dotted_path)?;
        let prev_doc = self.doc.clone();
        let prev_settings = self.settings.clone();

        let removed = match remove_key_from_doc(&mut self.doc, &segments, dotted_path) {
            Ok(removed) => removed,
            Err(err) => {
                self.doc = prev_doc;
                self.settings = prev_settings;
                return Err(err);
            }
        };

        if removed {
            match deserialize_and_validate(&self.doc) {
                Ok(next_settings) => {
                    self.settings = next_settings;
                }
                Err(err) => {
                    self.doc = prev_doc;
                    self.settings = prev_settings;
                    return Err(err);
                }
            }
        }

        Ok(removed)
    }

    /// Atomically writes the current TOML document to `path` via a temporary
    /// file in the same directory followed by `sync_all` and `rename`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Io`] if creating the parent directory, writing,
    /// syncing, or persisting the temporary file fails.
    pub fn save_to_file(&self, path: &Path) -> Result<(), ConfigError> {
        save_atomic(path, &self.to_toml_string())
    }

    /// Lists dotted keys present in the TOML document that are not part of the
    /// core `KovaSettings` schema.
    #[must_use]
    pub fn unknown_keys(&self) -> Vec<String> {
        let mut unknown = Vec::new();
        collect_unknown_keys(self.doc.as_table(), "", &mut unknown);
        unknown
    }
}

pub(crate) fn deserialize_and_validate(doc: &DocumentMut) -> Result<KovaSettings, ConfigError> {
    let settings: KovaSettings =
        toml_edit::de::from_document(doc.clone()).map_err(|err| ConfigError::Deserialize {
            message: err.to_string(),
        })?;
    settings.validate()?;
    Ok(settings)
}

pub(crate) fn parse_dotted_path(dotted_path: &str) -> Result<Vec<&str>, ConfigError> {
    if dotted_path.is_empty() {
        return Err(ConfigError::InvalidPath {
            path: dotted_path.to_owned(),
            reason: "path must not be empty".to_owned(),
        });
    }
    let segments: Vec<&str> = dotted_path.split('.').collect();
    if segments.iter().any(|seg| seg.is_empty()) {
        return Err(ConfigError::InvalidPath {
            path: dotted_path.to_owned(),
            reason: "path contains an empty segment".to_owned(),
        });
    }
    Ok(segments)
}

pub(crate) fn apply_value_to_doc(
    doc: &mut DocumentMut,
    segments: &[&str],
    value: Value,
    full_path: &str,
) -> Result<(), ConfigError> {
    let Some((&leaf, parents)) = segments.split_last() else {
        return Err(ConfigError::InvalidPath {
            path: full_path.to_owned(),
            reason: "path must not be empty".to_owned(),
        });
    };

    let mut current: &mut Table = doc.as_table_mut();
    for &seg in parents {
        if !current.contains_key(seg) {
            current.insert(seg, Item::Table(Table::new()));
        }
        let item = current
            .get_mut(seg)
            .ok_or_else(|| ConfigError::InvalidPath {
                path: full_path.to_owned(),
                reason: format!("missing intermediate table `{seg}`"),
            })?;
        current = item
            .as_table_mut()
            .ok_or_else(|| ConfigError::InvalidPath {
                path: full_path.to_owned(),
                reason: format!("segment `{seg}` is not a table"),
            })?;
    }

    match current.get_mut(leaf) {
        Some(Item::Value(existing_val)) => {
            let mut updated = value;
            *updated.decor_mut() = existing_val.decor().clone();
            *existing_val = updated;
        }
        Some(_) => {
            return Err(ConfigError::InvalidPath {
                path: full_path.to_owned(),
                reason: format!("leaf `{leaf}` is a table, not a scalar value"),
            });
        }
        None => {
            current.insert(leaf, Item::Value(value));
        }
    }

    Ok(())
}

fn remove_key_from_doc(
    doc: &mut DocumentMut,
    segments: &[&str],
    full_path: &str,
) -> Result<bool, ConfigError> {
    let Some((&leaf, parents)) = segments.split_last() else {
        return Err(ConfigError::InvalidPath {
            path: full_path.to_owned(),
            reason: "path must not be empty".to_owned(),
        });
    };

    let mut current: &mut Table = doc.as_table_mut();
    for &seg in parents {
        let Some(item) = current.get_mut(seg) else {
            return Ok(false);
        };
        current = item
            .as_table_mut()
            .ok_or_else(|| ConfigError::InvalidPath {
                path: full_path.to_owned(),
                reason: format!("segment `{seg}` is not a table"),
            })?;
    }

    Ok(current.remove(leaf).is_some())
}

pub(crate) fn save_atomic(path: &Path, contents: &str) -> Result<(), ConfigError> {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };

    if !parent.exists() {
        std::fs::create_dir_all(parent).map_err(|err| ConfigError::Io {
            path: parent.to_string_lossy().into_owned(),
            message: err.to_string(),
        })?;
    }

    let mut tmp = NamedTempFile::new_in(parent).map_err(|err| ConfigError::Io {
        path: path.to_string_lossy().into_owned(),
        message: err.to_string(),
    })?;
    tmp.write_all(contents.as_bytes())
        .map_err(|err| ConfigError::Io {
            path: path.to_string_lossy().into_owned(),
            message: err.to_string(),
        })?;
    tmp.as_file().sync_all().map_err(|err| ConfigError::Io {
        path: path.to_string_lossy().into_owned(),
        message: err.to_string(),
    })?;
    tmp.persist(path).map_err(|err| ConfigError::Io {
        path: path.to_string_lossy().into_owned(),
        message: err.error.to_string(),
    })?;

    Ok(())
}

fn collect_unknown_keys(table: &Table, prefix: &str, out: &mut Vec<String>) {
    for (key, item) in table {
        let full = if prefix.is_empty() {
            key.to_owned()
        } else {
            format!("{prefix}.{key}")
        };
        if let Some(sub) = item.as_table() {
            if matches!(key, "editor" | "ui" | "jvm" | "terminal") && prefix.is_empty() {
                collect_unknown_keys(sub, &full, out);
            } else {
                out.push(full);
            }
        } else if !is_known_setting_key(&full) {
            out.push(full);
        }
    }
}

fn is_known_setting_key(dotted: &str) -> bool {
    matches!(
        dotted,
        "editor.font_family"
            | "editor.font_size"
            | "editor.line_height"
            | "editor.tab_size"
            | "editor.insert_spaces"
            | "editor.show_line_numbers"
            | "editor.show_indent_guides"
            | "editor.show_inlay_hints"
            | "editor.vim_mode"
            | "editor.keymap"
            | "editor.format_on_save"
            | "ui.theme"
            | "ui.focus_mode"
            | "ui.show_status_bar"
            | "ui.sidebar_width"
            | "ui.bottom_panel_height"
            | "jvm.jdtls_max_ram_mb"
            | "jvm.auto_hibernate_minutes"
            | "jvm.auto_decompile_class"
            | "jvm.lombok_enabled"
            | "jvm.preferred_jdk"
            | "jvm.gradle_offline"
            | "terminal.shell"
            | "terminal.font_size"
            | "terminal.inject_java_home"
            | "terminal.scrollback_lines"
    )
}
