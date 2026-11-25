//! Canonical Action identifiers, descriptors, and deterministic Action registry.

use std::borrow::{Borrow, Cow};
use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::context::{ContextExpr, ContextState, FocusStack};

/// Errors returned when validating or registering IDE actions.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ActionError {
    /// Supplied [`ActionId`] violated canonical `namespace.action_name` rules.
    #[error("invalid action id `{id}`: {reason}")]
    InvalidActionId {
        /// Offending action identifier string.
        id: String,
        /// Human-readable reason for rejection.
        reason: &'static str,
    },
    /// Attempted to register an action whose [`ActionId`] is already present.
    #[error("duplicate action id `{0}`")]
    DuplicateAction(ActionId),
    /// A plugin attempted to override a built-in IDE action.
    #[error("cannot override built-in action `{0}` from a plugin")]
    BuiltinOverrideForbidden(ActionId),
}

/// Canonical `namespace.action_name` identifier for an IDE action.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ActionId(Cow<'static, str>);

impl ActionId {
    /// Validates and constructs an [`ActionId`] from an owned or borrowed string.
    ///
    /// # Errors
    ///
    /// Returns [`ActionError::InvalidActionId`] unless `id` consists of at least
    /// two dot-separated segments where each segment starts with ASCII `[a-z]`
    /// and contains only `[a-z0-9_]`.
    pub fn new(id: impl Into<String>) -> Result<Self, ActionError> {
        let s = id.into();
        validate_action_id(&s).map_err(|reason| ActionError::InvalidActionId {
            id: s.clone(),
            reason,
        })?;
        Ok(Self(Cow::Owned(s)))
    }

    /// Constructs a built-in [`ActionId`] from a `'static` string literal
    /// without heap allocation.
    ///
    /// # Panics
    ///
    /// Panics at compile time (when evaluated in `const`) or runtime if `id`
    /// violates [`ActionId::new`] validation rules.
    #[must_use]
    pub const fn from_static(id: &'static str) -> Self {
        assert!(
            validate_action_id_bytes(id.as_bytes()).is_ok(),
            "ActionId::from_static requires valid `namespace.action_name` ASCII identifier"
        );
        Self(Cow::Borrowed(id))
    }

    /// Returns the string slice representation of this action identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the leading namespace segment before the first `.`.
    #[must_use]
    pub fn namespace(&self) -> &str {
        self.0.split('.').next().unwrap_or_else(|| self.0.as_ref())
    }
}

const fn validate_action_id_bytes(bytes: &[u8]) -> Result<(), &'static str> {
    if bytes.is_empty() {
        return Err("action id cannot be empty");
    }

    let mut segments: usize = 0;
    let mut seg_len: usize = 0;
    let mut idx: usize = 0;

    while idx < bytes.len() {
        let b = bytes[idx];
        if b == b'.' {
            if seg_len == 0 {
                return Err("empty segment or leading/consecutive dot");
            }
            segments += 1;
            seg_len = 0;
        } else if seg_len == 0 {
            if !b.is_ascii_lowercase() {
                return Err("each segment must start with lowercase ASCII [a-z]");
            }
            seg_len = 1;
        } else {
            if !(b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
                return Err("segments may only contain [a-z0-9_]");
            }
            seg_len += 1;
        }
        idx += 1;
    }

    if seg_len == 0 {
        return Err("trailing dot is not allowed");
    }
    segments += 1;

    if segments < 2 {
        return Err("action id must contain at least two dot-separated segments");
    }

    Ok(())
}

fn validate_action_id(id: &str) -> Result<(), &'static str> {
    validate_action_id_bytes(id.as_bytes())
}

impl AsRef<str> for ActionId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for ActionId {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for ActionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ActionId {
    type Err = ActionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl Serialize for ActionId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ActionId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::new(raw).map_err(serde::de::Error::custom)
    }
}

/// Logical grouping for actions displayed in *Find Action* and *Search Everywhere*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionCategory {
    /// Text editing, formatting, completion, and selections.
    Editor,
    /// Go to Class/File/Symbol/Declaration, back/forward, errors.
    Navigation,
    /// In-file and workspace search/replace.
    Search,
    /// Structural code refactorings.
    Refactor,
    /// Running and debugging JVM/test configurations.
    RunDebug,
    /// Opening, focusing, and hiding tool windows.
    ToolWindow,
    /// Version control operations.
    Git,
    /// Window layout, focus mode, settings, and tabs.
    View,
    /// Project and build lifecycle actions.
    Project,
    /// Vim modal editing commands and toggles.
    Vim,
    /// Actions contributed by sandboxed WASM plugins.
    Plugin,
}

/// Origin of a registered action.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ActionSource {
    /// Built-in IDE action registered by `kova_core`.
    Builtin,
    /// Action contributed by an external plugin.
    Plugin {
        /// Identifier of the plugin owning this action.
        plugin_id: String,
    },
}

/// Metadata describing an executable IDE action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionDescriptor {
    /// Canonical action identifier.
    pub id: ActionId,
    /// Human-readable title shown in *Find Action* and *Search Everywhere*.
    pub title: Cow<'static, str>,
    /// UI category grouping.
    pub category: ActionCategory,
    /// Optional extended description for tooltips or documentation.
    pub description: Option<Cow<'static, str>>,
    /// Default context predicate controlling palette availability.
    pub default_when: Option<ContextExpr>,
    /// Origin of this action (`Builtin` or `Plugin`).
    pub source: ActionSource,
}

impl ActionDescriptor {
    /// Creates a built-in [`ActionDescriptor`] without description or `default_when`.
    #[must_use]
    pub fn builtin(
        id: ActionId,
        title: impl Into<Cow<'static, str>>,
        category: ActionCategory,
    ) -> Self {
        Self {
            id,
            title: title.into(),
            category,
            description: None,
            default_when: None,
            source: ActionSource::Builtin,
        }
    }

    /// Creates a plugin-contributed [`ActionDescriptor`].
    #[must_use]
    pub fn plugin(
        id: ActionId,
        title: impl Into<Cow<'static, str>>,
        category: ActionCategory,
        plugin_id: impl Into<String>,
    ) -> Self {
        Self {
            id,
            title: title.into(),
            category,
            description: None,
            default_when: None,
            source: ActionSource::Plugin {
                plugin_id: plugin_id.into(),
            },
        }
    }

    /// Attaches an extended description to this descriptor.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<Cow<'static, str>>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Attaches a default context availability predicate to this descriptor.
    #[must_use]
    pub fn with_when(mut self, expr: ContextExpr) -> Self {
        self.default_when = Some(expr);
        self
    }
}

/// Deterministic registry storing all built-in and plugin actions.
#[derive(Debug, Clone, Default)]
pub struct ActionRegistry {
    actions: BTreeMap<ActionId, ActionDescriptor>,
}

impl ActionRegistry {
    /// Creates an empty [`ActionRegistry`].
    #[must_use]
    pub const fn new() -> Self {
        Self {
            actions: BTreeMap::new(),
        }
    }

    /// Registers a new action, rejecting duplicate [`ActionId`]s.
    ///
    /// # Errors
    ///
    /// Returns [`ActionError::DuplicateAction`] if an action with the same
    /// [`ActionId`] is already registered.
    pub fn register(&mut self, descriptor: ActionDescriptor) -> Result<(), ActionError> {
        if self.actions.contains_key(&descriptor.id) {
            return Err(ActionError::DuplicateAction(descriptor.id));
        }
        self.actions.insert(descriptor.id.clone(), descriptor);
        Ok(())
    }

    /// Registers a plugin action or replaces an existing plugin action, while
    /// forbidding overrides of [`ActionSource::Builtin`] actions.
    ///
    /// # Errors
    ///
    /// Returns [`ActionError::BuiltinOverrideForbidden`] if the target
    /// [`ActionId`] belongs to a built-in action (or if `descriptor` itself
    /// claims [`ActionSource::Builtin`] when overriding).
    pub fn register_or_replace_plugin(
        &mut self,
        descriptor: ActionDescriptor,
    ) -> Result<(), ActionError> {
        if let Some(existing) = self.actions.get(&descriptor.id)
            && existing.source == ActionSource::Builtin
        {
            return Err(ActionError::BuiltinOverrideForbidden(descriptor.id));
        }
        if descriptor.source == ActionSource::Builtin {
            return Err(ActionError::BuiltinOverrideForbidden(descriptor.id));
        }
        self.actions.insert(descriptor.id.clone(), descriptor);
        Ok(())
    }

    /// Returns the descriptor for `id`, if registered.
    #[must_use]
    pub fn get(&self, id: &ActionId) -> Option<&ActionDescriptor> {
        self.actions.get(id)
    }

    /// Returns the descriptor matching the raw action id string `id`, if any.
    #[must_use]
    pub fn get_by_str(&self, id: &str) -> Option<&ActionDescriptor> {
        self.actions.get(id)
    }

    /// Returns `true` if an action with raw identifier `id` is registered.
    #[must_use]
    pub fn contains(&self, id: &str) -> bool {
        self.actions.contains_key(id)
    }

    /// Iterates over all registered actions in deterministic [`ActionId`] order.
    pub fn iter(&self) -> impl Iterator<Item = &ActionDescriptor> {
        self.actions.values()
    }

    /// Returns all registered actions whose `default_when` predicate is either
    /// `None` or evaluates to `true` for `(stack, state)`.
    #[must_use]
    pub fn available_in<'a>(
        &'a self,
        stack: &FocusStack,
        state: &ContextState,
    ) -> Vec<&'a ActionDescriptor> {
        self.actions
            .values()
            .filter(|desc| {
                desc.default_when
                    .as_ref()
                    .is_none_or(|expr| expr.matches(stack, state))
            })
            .collect()
    }

    /// Returns the total number of registered actions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.actions.len()
    }

    /// Returns `true` when no actions are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::FocusContext;

    #[test]
    fn action_id_validation_accepts_valid_and_rejects_invalid() {
        const STATIC_ID: ActionId = ActionId::from_static("navigation.search_everywhere");
        assert_eq!(STATIC_ID.as_str(), "navigation.search_everywhere");

        let valid = ActionId::new("editor.duplicate_line").expect("valid action id");
        assert_eq!(valid.as_str(), "editor.duplicate_line");
        assert_eq!(valid.namespace(), "editor");

        let multi = ActionId::new("plugin.acme.run_v2").expect("multi-segment valid");
        assert_eq!(multi.namespace(), "plugin");

        for invalid in [
            "",
            "editor",
            ".editor.copy",
            "editor.copy.",
            "editor..copy",
            "Editor.copy",
            "editor.Copy",
            "editor.1copy",
            "1editor.copy",
            "editor.copy line",
            "editor.copy-line",
            "editor.copy\n",
        ] {
            assert!(
                ActionId::new(invalid).is_err(),
                "expected `{invalid}` to be rejected"
            );
        }
    }

    #[test]
    fn action_registry_rejects_duplicates_and_protects_builtins() {
        let mut reg = ActionRegistry::new();
        assert!(reg.is_empty());

        let dup_id = ActionId::from_static("editor.duplicate_line");
        let desc = ActionDescriptor::builtin(
            dup_id.clone(),
            "Duplicate Line or Selection",
            ActionCategory::Editor,
        );
        reg.register(desc.clone()).expect("first register succeeds");
        assert_eq!(reg.len(), 1);
        assert!(reg.contains("editor.duplicate_line"));
        assert_eq!(
            reg.get(&dup_id).map(|d| d.title.as_ref()),
            Some("Duplicate Line or Selection")
        );

        assert_eq!(
            reg.register(desc),
            Err(ActionError::DuplicateAction(dup_id.clone()))
        );

        let override_attempt = ActionDescriptor::plugin(
            dup_id.clone(),
            "Hijacked Duplicate",
            ActionCategory::Plugin,
            "evil-plugin",
        );
        assert_eq!(
            reg.register_or_replace_plugin(override_attempt),
            Err(ActionError::BuiltinOverrideForbidden(dup_id))
        );

        let plugin_id = ActionId::from_static("plugin.hello.say_hi");
        let plugin_v1 = ActionDescriptor::plugin(
            plugin_id.clone(),
            "Say Hi v1",
            ActionCategory::Plugin,
            "hello-plugin",
        );
        reg.register_or_replace_plugin(plugin_v1)
            .expect("plugin registration succeeds");

        let plugin_v2 = ActionDescriptor::plugin(
            plugin_id.clone(),
            "Say Hi v2",
            ActionCategory::Plugin,
            "hello-plugin",
        );
        reg.register_or_replace_plugin(plugin_v2)
            .expect("plugin reload replaces existing plugin action");
        assert_eq!(
            reg.get(&plugin_id).map(|d| d.title.as_ref()),
            Some("Say Hi v2")
        );
    }

    #[test]
    fn action_registry_filters_available_in_context() {
        let mut reg = ActionRegistry::new();
        reg.register(ActionDescriptor::builtin(
            ActionId::from_static("navigation.search_everywhere"),
            "Search Everywhere",
            ActionCategory::Navigation,
        ))
        .expect("register global action");
        reg.register(
            ActionDescriptor::builtin(
                ActionId::from_static("editor.duplicate_line"),
                "Duplicate Line",
                ActionCategory::Editor,
            )
            .with_when(ContextExpr::parse("EditorText && !editor_read_only").expect("expr")),
        )
        .expect("register editor action");

        let editor_stack = FocusStack::from_leaf(FocusContext::EditorText);
        let editable = ContextState::new();
        let avail_editable: Vec<&str> = reg
            .available_in(&editor_stack, &editable)
            .into_iter()
            .map(|d| d.id.as_str())
            .collect();
        assert_eq!(
            avail_editable,
            vec!["editor.duplicate_line", "navigation.search_everywhere"]
        );

        let read_only = ContextState::new().with_flag("editor_read_only", true);
        let avail_ro: Vec<&str> = reg
            .available_in(&editor_stack, &read_only)
            .into_iter()
            .map(|d| d.id.as_str())
            .collect();
        assert_eq!(avail_ro, vec!["navigation.search_everywhere"]);

        let tree_stack = FocusStack::from_leaf(FocusContext::ProjectTree);
        let avail_tree: Vec<&str> = reg
            .available_in(&tree_stack, &editable)
            .into_iter()
            .map(|d| d.id.as_str())
            .collect();
        assert_eq!(avail_tree, vec!["navigation.search_everywhere"]);
    }
}
