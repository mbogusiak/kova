//! In-memory registry of built-in and user-loaded Aether themes.

use std::collections::BTreeMap;
use std::sync::Arc;

use thiserror::Error;

use crate::toml_theme::{AetherTheme, BuiltinTheme, ThemeLoadError};

/// Registry managing available themes and the currently active theme selection.
#[derive(Debug, Clone)]
pub struct ThemeRegistry {
    themes: BTreeMap<String, Arc<AetherTheme>>,
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
            let theme = Arc::new(AetherTheme::builtin(preset));
            themes.insert(theme.metadata.id.clone(), theme);
        }
        Self {
            themes,
            active_id: BuiltinTheme::KovaIslandsDark.id().to_owned(),
        }
    }

    /// Registers or replaces a theme in the registry, returning its ID.
    pub fn register(&mut self, theme: AetherTheme) -> String {
        self.register_arc(Arc::new(theme))
    }

    /// Registers or replaces an `Arc`-wrapped theme in the registry, returning its ID.
    pub fn register_arc(&mut self, theme: Arc<AetherTheme>) -> String {
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

    /// Removes a user-defined theme by `id`.
    ///
    /// Built-in presets (`kova-islands-dark`, `kova-light`) are never removed
    /// and always return `false`. If the removed theme was active, the active
    /// theme safely falls back to `kova-islands-dark`.
    pub fn unregister(&mut self, id: &str) -> bool {
        if BuiltinTheme::ALL.iter().any(|preset| preset.id() == id) {
            return false;
        }
        let removed = self.themes.remove(id).is_some();
        if removed && self.active_id == id {
            let fallback = BuiltinTheme::KovaIslandsDark.id();
            fallback.clone_into(&mut self.active_id);
        }
        removed
    }

    /// Removes a user-defined theme by `id`, returning `Some("kova-islands-dark")`
    /// if the removed theme was currently active and fell back to the default preset.
    pub fn unregister_custom(&mut self, id: &str) -> Option<String> {
        let was_active = self.active_id == id;
        if self.unregister(id) && was_active {
            Some(self.active_id.clone())
        } else {
            None
        }
    }

    /// Restores a built-in preset theme by `id` if it matches one of [`BuiltinTheme::ALL`].
    ///
    /// Useful when a user theme file that shadowed a built-in ID is deleted from disk.
    pub fn restore_builtin(&mut self, id: &str) -> bool {
        let Some(preset) = BuiltinTheme::ALL
            .iter()
            .copied()
            .find(|preset| preset.id() == id)
        else {
            return false;
        };
        self.themes
            .insert(id.to_owned(), Arc::new(AetherTheme::builtin(preset)));
        true
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
        self.themes.get(&self.active_id).map_or_else(
            || AetherTheme::builtin_ref(BuiltinTheme::KovaIslandsDark),
            Arc::as_ref,
        )
    }

    /// Returns a shared [`Arc`] handle to the currently active [`AetherTheme`]
    /// in `O(1)` without cloning the underlying token tables.
    #[must_use]
    pub fn active_arc(&self) -> Arc<AetherTheme> {
        self.themes
            .get(&self.active_id)
            .cloned()
            .unwrap_or_else(|| {
                self.themes
                    .get(BuiltinTheme::KovaIslandsDark.id())
                    .cloned()
                    .unwrap_or_else(|| {
                        Arc::new(AetherTheme::builtin(BuiltinTheme::KovaIslandsDark))
                    })
            })
    }

    /// Looks up a registered theme by its `id`.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&AetherTheme> {
        self.themes.get(id).map(Arc::as_ref)
    }

    /// Looks up a shared [`Arc`] handle to a registered theme by its `id`.
    #[must_use]
    pub fn get_arc(&self, id: &str) -> Option<Arc<AetherTheme>> {
        self.themes.get(id).cloned()
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
