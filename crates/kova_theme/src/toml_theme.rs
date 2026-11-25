//! TOML theme loading, serialization, metadata, and built-in preset access.

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    plugin::{PluginVariant, PluginVariantTokens},
    tokens::SemanticTokens,
};

const KOVA_ISLANDS_DARK_TOML: &str = include_str!("../themes/kova-islands-dark.toml");
const KOVA_LIGHT_TOML: &str = include_str!("../themes/kova-light.toml");

static DARK_PRESET: OnceLock<AetherTheme> = OnceLock::new();
static LIGHT_PRESET: OnceLock<AetherTheme> = OnceLock::new();

/// Supported polarity of an Aether theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemePolarity {
    /// Dark backdrop and elevated dark islands.
    Dark,
    /// Light paper-stone backdrop and bright islands.
    Light,
}

/// Descriptive metadata header (`[metadata]`) of an Aether theme file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeMetadata {
    /// Unique kebab-case identifier (matches `<id>.toml`).
    pub id: String,
    /// Human-readable display name.
    pub name: String,
    /// Theme author or maintainer.
    pub author: String,
    /// Light or dark polarity.
    pub polarity: ThemePolarity,
    /// Theme schema version (`1` for current Aether specification).
    pub schema_version: u32,
}

/// Built-in Aether theme presets compiled into `kova_theme`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuiltinTheme {
    /// Default dark theme combining `IntelliJ` Islands hierarchy with Omarchy contrast.
    KovaIslandsDark,
    /// High-luminance light theme with crisp porcelain islands.
    KovaLight,
}

impl BuiltinTheme {
    /// All built-in presets shipped with `kova_theme`.
    pub const ALL: [Self; 2] = [Self::KovaIslandsDark, Self::KovaLight];

    /// Canonical theme ID (`"kova-islands-dark"` or `"kova-light"`).
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::KovaIslandsDark => "kova-islands-dark",
            Self::KovaLight => "kova-light",
        }
    }

    /// Raw TOML source compiled into the binary for this preset.
    #[must_use]
    pub const fn toml_source(self) -> &'static str {
        match self {
            Self::KovaIslandsDark => KOVA_ISLANDS_DARK_TOML,
            Self::KovaLight => KOVA_LIGHT_TOML,
        }
    }
}

/// Complete Aether theme containing metadata and all semantic color tokens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AetherTheme {
    /// Theme metadata (`[metadata]`).
    pub metadata: ThemeMetadata,
    /// All 8 semantic token sections (`[surfaces]`, `[text]`, `[borders]`, etc.).
    #[serde(flatten)]
    pub tokens: SemanticTokens,
}

impl AetherTheme {
    /// Returns a cloned copy of a built-in Aether preset without disk I/O.
    ///
    /// Presets are parsed once from compiled-in TOML sources and cached in memory.
    ///
    /// # Panics
    ///
    /// Panics only if a compiled-in preset `.toml` file is malformed (guarded by
    /// integration tests in `tests/completeness.rs`).
    #[must_use]
    pub fn builtin(preset: BuiltinTheme) -> Self {
        Self::builtin_ref(preset).clone()
    }

    /// Returns a `&'static` reference to a cached built-in Aether preset.
    ///
    /// # Panics
    ///
    /// Panics only if a compiled-in preset `.toml` file is malformed (guarded by
    /// integration tests in `tests/completeness.rs`).
    #[must_use]
    pub fn builtin_ref(preset: BuiltinTheme) -> &'static Self {
        let cell = match preset {
            BuiltinTheme::KovaIslandsDark => &DARK_PRESET,
            BuiltinTheme::KovaLight => &LIGHT_PRESET,
        };
        cell.get_or_init(|| {
            #[allow(
                clippy::expect_used,
                reason = "compiled-in preset TOML is validated by completeness tests"
            )]
            Self::from_toml_str(preset.toml_source())
                .expect("compiled-in Aether preset TOML must be valid and complete")
        })
    }

    /// Parses and validates an Aether theme from a TOML string.
    ///
    /// # Errors
    ///
    /// Returns [`ThemeLoadError::Parse`] if TOML syntax is invalid, any required token
    /// is missing, an unknown field is present, or any hex color is malformed, and
    /// [`ThemeLoadError::UnsupportedSchemaVersion`] if `schema_version != 1`.
    pub fn from_toml_str(toml_str: &str) -> Result<Self, ThemeLoadError> {
        let theme: Self = toml::from_str(toml_str)?;
        if theme.metadata.schema_version != 1 {
            return Err(ThemeLoadError::UnsupportedSchemaVersion {
                found: theme.metadata.schema_version,
                supported: 1,
            });
        }
        Ok(theme)
    }

    /// Serializes this theme to a formatted TOML string.
    ///
    /// # Errors
    ///
    /// Returns [`ThemeLoadError::Serialize`] if `toml` fails to format the theme.
    pub fn to_toml_string(&self) -> Result<String, ThemeLoadError> {
        toml::to_string_pretty(self).map_err(ThemeLoadError::from)
    }

    /// Resolves the semantic WASM plugin style tokens for `variant`.
    #[must_use]
    pub const fn plugin_variant(&self, variant: PluginVariant) -> &PluginVariantTokens {
        self.tokens.plugins.get(variant)
    }
}

/// Errors produced when loading or serializing an Aether theme.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ThemeLoadError {
    /// Failed to parse TOML or validate required Aether theme fields.
    #[error("invalid Aether theme TOML: {0}")]
    Parse(#[from] toml::de::Error),
    /// Failed to serialize an Aether theme to TOML.
    #[error("failed to serialize Aether theme TOML: {0}")]
    Serialize(#[from] toml::ser::Error),
    /// Theme declares an unsupported `metadata.schema_version`.
    #[error("unsupported Aether theme schema_version {found} (expected {supported})")]
    UnsupportedSchemaVersion {
        /// Version found in the theme file.
        found: u32,
        /// Version supported by this build of `kova_theme`.
        supported: u32,
    },
}
