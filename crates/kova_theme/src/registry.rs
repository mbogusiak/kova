//! In-memory registry of built-in and user-loaded Aether themes.

use std::collections::BTreeMap;

use thiserror::Error;

use crate::toml_theme::{AetherTheme, BuiltinTheme, ThemeLoadError};

/// Registry managing available themes and the currently active theme selection.
#[derive(Debug, Clone)]
pub struct ThemeRegistry {
    themes: BTreeMap<String, AetherTheme>,
    active_id: String,
}

impl Default for ThemeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ThemeRegistry {
    /// Creates a registry pre-populated with all built-in presets and
    /// `kova-islands-dark` set as the active theme.
    #[must_use]
    pub fn new() -> Self {
        let mut themes = BTreeMap::new();
        for preset in BuiltinTheme::ALL {
            let theme = AetherTheme::builtin(preset);
            themes.insert(theme.metadata.id.clone(), theme);
        }
        Self {
            themes,
            active_id: BuiltinTheme::KovaIslandsDark.id().to_owned(),
        }
    }

    /// Registers or replaces a theme in the registry, returning its ID.
    pub fn register(&mut self, theme: AetherTheme) -> String {
        let id = theme.metadata.id.clone();
        self.themes.insert(id.clone(), theme);
        id
    }

    /// Parses a TOML theme string and registers it in the registry.
    ///
    /// # Errors
    ///
    /// Returns [`ThemeLoadError`] if the TOML fails validation.
    pub fn register_toml(&mut self, toml_str: &str) -> Result<String, ThemeLoadError> {
        let theme = AetherTheme::from_toml_str(toml_str)?;
        Ok(self.register(theme))
    }

    /// Switches the active theme to `id`.
    ///
    /// # Errors
    ///
    /// Returns [`ThemeRegistryError::UnknownTheme`] if no theme with `id` is registered.
    pub fn set_active(&mut self, id: &str) -> Result<&AetherTheme, ThemeRegistryError> {
        if !self.themes.contains_key(id) {
            return Err(ThemeRegistryError::UnknownTheme { id: id.to_owned() });
        }
        id.clone_into(&mut self.active_id);
        Ok(self.active())
    }

    /// Returns the ID of the currently active theme.
    #[must_use]
    pub fn active_id(&self) -> &str {
        &self.active_id
    }

    /// Returns the currently active [`AetherTheme`].
    ///
    /// Falls back to [`BuiltinTheme::KovaIslandsDark`] if the active ID is somehow
    /// absent, ensuring a panicking lookup can never occur.
    #[must_use]
    pub fn active(&self) -> &AetherTheme {
        self.themes
            .get(&self.active_id)
            .unwrap_or_else(|| AetherTheme::builtin_ref(BuiltinTheme::KovaIslandsDark))
    }

    /// Looks up a registered theme by its `id`.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&AetherTheme> {
        self.themes.get(id)
    }

    /// Returns an iterator over all registered theme IDs in sorted order.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.themes.keys().map(String::as_str)
    }

    /// Returns the number of registered themes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.themes.len()
    }

    /// Returns `true` if no themes are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.themes.is_empty()
    }
}

/// Errors returned by [`ThemeRegistry`] operations.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ThemeRegistryError {
    /// Requested theme ID is not present in the registry.
    #[error("unknown theme `{id}`")]
    UnknownTheme {
        /// Unrecognized theme identifier.
        id: String,
    },
}
