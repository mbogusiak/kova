//! Semantic style variants for sandboxed WASM plugins (`primary`, `muted`, `warning`, `error`, `accent`).

use serde::{Deserialize, Serialize};

use crate::color::RgbaColor;

/// Semantic style variant exposed to WASM plugins over WIT.
///
/// Plugins never pass raw pixels or hex colors across the sandbox boundary; they
/// pick one of these five semantic variants and `kova_ui` resolves the active
/// theme colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginVariant {
    /// Primary call-to-action or emphasized badge.
    Primary,
    /// Neutral, secondary, or low-emphasis panel/control.
    Muted,
    /// Caution banner or non-fatal warning state.
    Warning,
    /// Destructive action or error badge/banner.
    Error,
    /// Highlighted info pill or active accent indicator.
    Accent,
}

impl PluginVariant {
    /// All five semantic plugin variants in canonical order.
    pub const ALL: [Self; 5] = [
        Self::Primary,
        Self::Muted,
        Self::Warning,
        Self::Error,
        Self::Accent,
    ];
}

/// Complete set of color tokens resolved for a single [`PluginVariant`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginVariantTokens {
    /// Filled button, badge, or highlighted container background.
    pub background: RgbaColor,
    /// Text and icon foreground on `background`.
    pub foreground: RgbaColor,
    /// `1px` border around the plugin widget or badge.
    pub border: RgbaColor,
    /// Interactive hover background color.
    pub hover_bg: RgbaColor,
    /// Interactive pressed/active background color.
    pub active_bg: RgbaColor,
    /// Soft banner or low-weight row background.
    pub subtle_bg: RgbaColor,
    /// Text and icon foreground on `subtle_bg`.
    pub subtle_fg: RgbaColor,
}

/// Map of all five semantic plugin variants (`[plugins]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginVariants {
    /// `primary` variant tokens (`[plugins.primary]`).
    pub primary: PluginVariantTokens,
    /// `muted` variant tokens (`[plugins.muted]`).
    pub muted: PluginVariantTokens,
    /// `warning` variant tokens (`[plugins.warning]`).
    pub warning: PluginVariantTokens,
    /// `error` variant tokens (`[plugins.error]`).
    pub error: PluginVariantTokens,
    /// `accent` variant tokens (`[plugins.accent]`).
    pub accent: PluginVariantTokens,
}

impl PluginVariants {
    /// Resolves the [`PluginVariantTokens`] for the requested semantic `variant`.
    #[must_use]
    pub const fn get(&self, variant: PluginVariant) -> &PluginVariantTokens {
        match variant {
            PluginVariant::Primary => &self.primary,
            PluginVariant::Muted => &self.muted,
            PluginVariant::Warning => &self.warning,
            PluginVariant::Error => &self.error,
            PluginVariant::Accent => &self.accent,
        }
    }
}
