//! Strict TOML parser and loader for `~/.config/kova/keymap.toml`.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::action::{ActionError, ActionId};
use crate::context::{ContextExpr, ContextExprError, FocusContext};
use crate::keymap::stroke::{KeyChord, Platform};
use crate::keymap::{BindingRule, BindingTarget, KeymapLayer, KeymapLayerKind};

/// Supported `schema_version` of `keymap.toml`.
pub const KEYMAP_SCHEMA_VERSION: u32 = 1;

/// Base preset mode selected in `keymap.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeymapPresetMode {
    /// Inherit the built-in `JetBrains` macOS/Linux default bindings.
    #[default]
    #[serde(rename = "jetbrains")]
    JetBrains,
    /// Start from a completely empty keymap using only user bindings.
    None,
}

/// Errors produced when reading, deserializing, or validating `keymap.toml` or key chords.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeymapParseError {
    /// Failed to read `keymap.toml` from disk.
    #[error("failed to read keymap file `{path}`: {message}")]
    Io {
        /// Path that failed to open or read.
        path: PathBuf,
        /// Underlying OS I/O error message.
        message: String,
    },
    /// Invalid TOML syntax or unexpected field (`deny_unknown_fields`).
    #[error("invalid keymap TOML: {message}")]
    TomlSyntax {
        /// Parser message produced by `toml`.
        message: String,
    },
    /// Unsupported `schema_version` declared in `keymap.toml`.
    #[error("unsupported keymap schema_version {version} (expected {KEYMAP_SCHEMA_VERSION})")]
    UnsupportedSchemaVersion {
        /// Unsupported version number supplied by the file.
        version: u32,
    },
    /// Invalid `os` selector in a `[[bindings]]` entry.
    #[error(
        "[[bindings]][{binding_index}]: invalid os `{value}` (expected \"macos\" or \"linux\")"
    )]
    InvalidPlatform {
        /// Zero-based index of the offending `[[bindings]]` entry.
        binding_index: usize,
        /// Supplied `os` string.
        value: String,
    },
    /// Invalid `context` expression or context name in a `[[bindings]]` entry.
    #[error("[[bindings]][{binding_index}]: invalid context `{input}`: {source}")]
    InvalidContext {
        /// Zero-based index of the offending `[[bindings]]` entry.
        binding_index: usize,
        /// Raw `context` value.
        input: String,
        /// Detailed expression error.
        #[source]
        source: ContextExprError,
    },
    /// Invalid `when` predicate expression in a `[[bindings]]` entry.
    #[error("[[bindings]][{binding_index}]: invalid when `{input}`: {source}")]
    InvalidWhen {
        /// Zero-based index of the offending `[[bindings]]` entry.
        binding_index: usize,
        /// Raw `when` value.
        input: String,
        /// Detailed expression error.
        #[source]
        source: ContextExprError,
    },
    /// Binding specified an empty array of keys (`key = []`).
    #[error("[[bindings]][{binding_index}]: key list cannot be empty")]
    EmptyKeyList {
        /// Zero-based index of the offending `[[bindings]]` entry.
        binding_index: usize,
    },
    /// Invalid key chord specification.
    #[error("{}", format_invalid_chord(*binding_index, key, reason))]
    InvalidKeyChord {
        /// Zero-based index of the offending `[[bindings]]` entry, if parsed from TOML.
        binding_index: Option<usize>,
        /// Raw key chord string.
        key: String,
        /// Reason the chord was rejected.
        reason: String,
    },
    /// Invalid `action` identifier in a `[[bindings]]` entry.
    #[error("[[bindings]][{binding_index}]: invalid action `{action}`: {source}")]
    InvalidActionId {
        /// Zero-based index of the offending `[[bindings]]` entry.
        binding_index: usize,
        /// Raw `action` string.
        action: String,
        /// Detailed validation error.
        #[source]
        source: ActionError,
    },
}

fn format_invalid_chord(binding_index: Option<usize>, key: &str, reason: &str) -> String {
    match binding_index {
        Some(idx) => format!("[[bindings]][{idx}]: invalid key `{key}`: {reason}"),
        None => format!("invalid key `{key}`: {reason}"),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawKeymapFile {
    #[serde(default = "default_schema_version")]
    schema_version: u32,
    #[serde(default)]
    preset: KeymapPresetMode,
    #[serde(default)]
    bindings: Vec<RawBindingEntry>,
}

const fn default_schema_version() -> u32 {
    KEYMAP_SCHEMA_VERSION
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBindingEntry {
    context: Option<String>,
    when: Option<String>,
    os: Option<String>,
    key: RawKeySpec,
    action: String,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawKeySpec {
    Single(String),
    Multiple(Vec<String>),
}

/// Parses a `keymap.toml` document into a [`KeymapLayer`] tagged as
/// [`KeymapLayerKind::UserOverride`].
///
/// # Errors
///
/// Returns [`KeymapParseError`] on syntax errors, unknown fields, unsupported
/// schema versions, invalid contexts/`when` expressions, invalid chords, or
/// malformed action IDs.
pub fn parse_keymap_toml(
    toml_str: &str,
    current_platform: Platform,
) -> Result<KeymapLayer, KeymapParseError> {
    parse_keymap_toml_with_kind(toml_str, current_platform, KeymapLayerKind::UserOverride)
}

/// Parses a `keymap.toml` document into a [`KeymapLayer`] with an explicit
/// [`KeymapLayerKind`] (used for both user overrides and built-in TOML presets).
///
/// # Errors
///
/// Returns [`KeymapParseError`] if parsing or validation fails.
pub fn parse_keymap_toml_with_kind(
    toml_str: &str,
    current_platform: Platform,
    kind: KeymapLayerKind,
) -> Result<KeymapLayer, KeymapParseError> {
    let raw: RawKeymapFile =
        toml::from_str(toml_str).map_err(|err| KeymapParseError::TomlSyntax {
            message: err.to_string(),
        })?;

    if raw.schema_version != KEYMAP_SCHEMA_VERSION {
        return Err(KeymapParseError::UnsupportedSchemaVersion {
            version: raw.schema_version,
        });
    }

    let mut rules = Vec::with_capacity(raw.bindings.len());
    for (binding_index, entry) in raw.bindings.into_iter().enumerate() {
        parse_binding_entry(binding_index, entry, current_platform, kind, &mut rules)?;
    }

    Ok(KeymapLayer {
        kind,
        preset: raw.preset,
        rules,
    })
}

fn parse_binding_entry(
    binding_index: usize,
    entry: RawBindingEntry,
    current_platform: Platform,
    kind: KeymapLayerKind,
    out: &mut Vec<BindingRule>,
) -> Result<(), KeymapParseError> {
    let entry_platform = match entry.os {
        Some(ref os_str) => {
            let parsed =
                Platform::parse_name(os_str).ok_or_else(|| KeymapParseError::InvalidPlatform {
                    binding_index,
                    value: os_str.clone(),
                })?;
            Some(parsed)
        }
        None => None,
    };

    let (root_context, context_expr) = match entry.context {
        None => (Some(FocusContext::Global), None),
        Some(ref ctx_str) => {
            if let Some(ctx) = FocusContext::parse_name(ctx_str) {
                (Some(ctx), None)
            } else {
                let parsed_expr = ContextExpr::parse(ctx_str).map_err(|source| {
                    KeymapParseError::InvalidContext {
                        binding_index,
                        input: ctx_str.clone(),
                        source,
                    }
                })?;
                (None, Some(parsed_expr))
            }
        }
    };

    let when_expr = match entry.when {
        None => None,
        Some(ref when_str) => {
            let parsed =
                ContextExpr::parse(when_str).map_err(|source| KeymapParseError::InvalidWhen {
                    binding_index,
                    input: when_str.clone(),
                    source,
                })?;
            Some(parsed)
        }
    };

    let combined_when = match (context_expr, when_expr) {
        (Some(c), Some(w)) => Some(ContextExpr::and(c, w)),
        (Some(c), None) => Some(c),
        (None, Some(w)) => Some(w),
        (None, None) => None,
    };

    let target = if entry.action.trim() == "none" {
        BindingTarget::Unbind
    } else {
        let id = ActionId::new(entry.action.clone()).map_err(|source| {
            KeymapParseError::InvalidActionId {
                binding_index,
                action: entry.action.clone(),
                source,
            }
        })?;
        BindingTarget::Action(id)
    };

    let keys = match entry.key {
        RawKeySpec::Single(k) => vec![k],
        RawKeySpec::Multiple(ks) => {
            if ks.is_empty() {
                return Err(KeymapParseError::EmptyKeyList { binding_index });
            }
            ks
        }
    };

    let parse_platform = entry_platform.unwrap_or(current_platform);
    for key_str in keys {
        let chord = KeyChord::parse(&key_str, parse_platform).map_err(|err| match err {
            KeymapParseError::InvalidKeyChord { key, reason, .. } => {
                KeymapParseError::InvalidKeyChord {
                    binding_index: Some(binding_index),
                    key: if key.is_empty() { key_str.clone() } else { key },
                    reason,
                }
            }
            other => other,
        })?;

        let id = out.len();
        out.push(BindingRule {
            id,
            layer: kind,
            platform: entry_platform,
            root_context,
            when: combined_when.clone(),
            chord,
            target: target.clone(),
        });
    }

    Ok(())
}

/// Loads `~/.config/kova/keymap.toml` from `path`, returning an empty
/// [`KeymapLayer`] with [`KeymapPresetMode::JetBrains`] when the file does not
/// exist (`ErrorKind::NotFound`). Never creates files or directories on disk.
///
/// # Errors
///
/// Returns [`KeymapParseError`] on non-`NotFound` I/O failures or invalid TOML
/// content.
pub fn load_user_keymap_or_default(
    path: &Path,
    current_platform: Platform,
) -> Result<KeymapLayer, KeymapParseError> {
    match std::fs::read_to_string(path) {
        Ok(contents) => parse_keymap_toml(&contents, current_platform),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(KeymapLayer::empty_user_default()),
        Err(err) => Err(KeymapParseError::Io {
            path: path.to_path_buf(),
            message: err.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use kova_config::KovaPaths;
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn parses_valid_user_keymap_with_multi_keys_os_when_and_unbind() {
        let toml = r#"
schema_version = 1
preset = "jetbrains"

[[bindings]]
context = "EditorText"
when = "!vim_enabled"
os = "macos"
key = ["cmd+d", "primary+shift+d"]
action = "editor.duplicate_line"

[[bindings]]
key = "shift shift"
action = "none"

[[bindings]]
context = "EditorText && editor_has_selection"
os = "linux"
key = "ctrl+/"
action = "editor.toggle_line_comment"
"#;

        let layer = parse_keymap_toml(toml, Platform::MacOs).expect("valid keymap.toml");
        assert_eq!(layer.kind, KeymapLayerKind::UserOverride);
        assert_eq!(layer.preset, KeymapPresetMode::JetBrains);
        assert_eq!(layer.rules.len(), 4);

        assert_eq!(layer.rules[0].root_context, Some(FocusContext::EditorText));
        assert_eq!(layer.rules[0].platform, Some(Platform::MacOs));
        assert_eq!(
            layer.rules[0].target,
            BindingTarget::Action(ActionId::from_static("editor.duplicate_line"))
        );

        assert_eq!(layer.rules[2].root_context, Some(FocusContext::Global));
        assert_eq!(layer.rules[2].target, BindingTarget::Unbind);

        assert_eq!(layer.rules[3].root_context, None);
        assert!(layer.rules[3].when.is_some());
        assert_eq!(layer.rules[3].platform, Some(Platform::Linux));
    }

    #[test]
    fn parses_preset_none_mode() {
        let layer = parse_keymap_toml("preset = \"none\"\n", Platform::Linux).expect("valid");
        assert_eq!(layer.preset, KeymapPresetMode::None);
        assert_eq!(layer.rules.len(), 0);
    }

    #[test]
    fn rejects_unknown_fields_bad_version_and_reports_binding_index() {
        let unknown_field = r#"
[[bindings]]
kontext = "EditorText"
key = "cmd+d"
action = "editor.duplicate_line"
"#;
        assert!(matches!(
            parse_keymap_toml(unknown_field, Platform::MacOs),
            Err(KeymapParseError::TomlSyntax { .. })
        ));

        let bad_version = "schema_version = 99\n";
        assert_eq!(
            parse_keymap_toml(bad_version, Platform::MacOs),
            Err(KeymapParseError::UnsupportedSchemaVersion { version: 99 })
        );

        let bad_chord = r#"
[[bindings]]
key = "cmd+d"
action = "editor.duplicate_line"

[[bindings]]
key = "ctrl+"
action = "editor.save"
"#;
        let err = parse_keymap_toml(bad_chord, Platform::MacOs).expect_err("must fail");
        assert!(matches!(
            err,
            KeymapParseError::InvalidKeyChord {
                binding_index: Some(1),
                ..
            }
        ));
        assert!(err.to_string().contains("[[bindings]][1]"));

        let bad_when = r#"
[[bindings]]
when = "EditorText &&"
key = "cmd+d"
action = "editor.duplicate_line"
"#;
        assert!(matches!(
            parse_keymap_toml(bad_when, Platform::MacOs),
            Err(KeymapParseError::InvalidWhen {
                binding_index: 0,
                ..
            })
        ));
    }

    #[test]
    fn load_user_keymap_or_default_does_not_create_missing_file() {
        let tmp = TempDir::new().expect("temp dir");
        let paths = KovaPaths::from_roots(
            tmp.path().join("cfg"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
        )
        .expect("valid roots");
        let keymap_path: PathBuf = paths.keymap_file();
        assert!(!keymap_path.exists());

        let layer = load_user_keymap_or_default(&keymap_path, Platform::MacOs)
            .expect("missing file returns empty default layer");
        assert_eq!(layer.preset, KeymapPresetMode::JetBrains);
        assert_eq!(layer.rules.len(), 0);
        assert!(
            !keymap_path.exists(),
            "loader must never create keymap.toml"
        );
    }

    #[test]
    fn load_user_keymap_reads_existing_file_from_kova_paths() {
        let tmp = TempDir::new().expect("temp dir");
        let cfg_dir = tmp.path().join("cfg");
        std::fs::create_dir_all(&cfg_dir).expect("create cfg dir");
        let paths =
            KovaPaths::from_roots(cfg_dir, tmp.path().join("data"), tmp.path().join("cache"))
                .expect("valid roots");

        std::fs::write(
            paths.keymap_file(),
            r#"
[[bindings]]
context = "Workspace"
key = "primary+s"
action = "editor.save"
"#,
        )
        .expect("write keymap.toml");

        let layer =
            load_user_keymap_or_default(&paths.keymap_file(), Platform::Linux).expect("load");
        assert_eq!(layer.rules.len(), 1);
        assert_eq!(
            layer.rules[0].chord,
            KeyChord::parse("ctrl+s", Platform::Linux).expect("chord")
        );
    }
}
