//! Compiled-in default `JetBrains` keymap presets for macOS and Linux.

use std::sync::OnceLock;

use crate::keymap::parser::parse_keymap_toml_with_kind;
use crate::keymap::stroke::Platform;
use crate::keymap::{KeymapLayer, KeymapLayerKind};

/// Raw TOML source of the built-in `JetBrains` macOS keymap preset.
pub const JETBRAINS_MACOS_TOML: &str = include_str!("jetbrains_macos.toml");

/// Raw TOML source of the built-in `JetBrains` Linux keymap preset.
pub const JETBRAINS_LINUX_TOML: &str = include_str!("jetbrains_linux.toml");

static MACOS_PRESET: OnceLock<KeymapLayer> = OnceLock::new();
static LINUX_PRESET: OnceLock<KeymapLayer> = OnceLock::new();

/// Returns a `&'static` reference to the cached default `JetBrains`
/// [`KeymapLayer`] for `platform`.
///
/// Presets are parsed once from the compiled-in TOML definitions and cached in
/// memory without any disk I/O.
///
/// # Panics
///
/// Panics only if a compiled-in preset TOML file is malformed (guarded by
/// unit tests and `tests/jetbrains_keymap_audit.rs`).
#[must_use]
pub fn jetbrains_keymap_layer_ref(platform: Platform) -> &'static KeymapLayer {
    let (cell, source) = match platform {
        Platform::MacOs => (&MACOS_PRESET, JETBRAINS_MACOS_TOML),
        Platform::Linux => (&LINUX_PRESET, JETBRAINS_LINUX_TOML),
    };
    cell.get_or_init(|| {
        #[allow(
            clippy::expect_used,
            reason = "compiled-in JetBrains preset TOML is validated by completeness tests"
        )]
        parse_keymap_toml_with_kind(source, platform, KeymapLayerKind::DefaultPreset)
            .expect("compiled-in JetBrains preset TOML must be valid")
    })
}

/// Returns a cloned copy of the built-in default `JetBrains` [`KeymapLayer`]
/// for `platform`.
#[must_use]
pub fn jetbrains_keymap_layer(platform: Platform) -> KeymapLayer {
    jetbrains_keymap_layer_ref(platform).clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_builtin_jetbrains_presets_parse_cleanly() {
        for platform in [Platform::MacOs, Platform::Linux] {
            let layer = jetbrains_keymap_layer(platform);
            assert_eq!(layer.kind, KeymapLayerKind::DefaultPreset);
            assert_ne!(layer.rules.len(), 0);
        }
    }
}
