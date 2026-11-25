//! Layered keymap resolution, TOML configuration loading, and chord dispatch.

pub mod chord;
pub mod parser;
pub mod stroke;

use std::collections::HashMap;

use smallvec::SmallVec;

use crate::action::{ActionId, ActionRegistry};
use crate::context::{ContextExpr, ContextState, FocusContext, FocusStack};

pub use chord::{
    DEFAULT_CHORD_TIMEOUT, DEFAULT_DOUBLE_TAP_WINDOW, KeymapSession, ModifierTapDetector,
};
pub use parser::{
    KEYMAP_SCHEMA_VERSION, KeymapParseError, KeymapPresetMode, load_user_keymap_or_default,
    parse_keymap_toml, parse_keymap_toml_with_kind,
};
pub use stroke::{
    KeyChord, KeyCode, KeyStep, KeyStroke, MAX_CHORD_STEPS, ModifierTap, Modifiers, Platform,
};

/// Precedence layer of a [`KeymapLayer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum KeymapLayerKind {
    /// Bindings contributed by plugins (lowest priority).
    Plugin = 0,
    /// Built-in default preset bindings (for example `JetBrains` macOS/Linux).
    DefaultPreset = 1,
    /// User overrides from `~/.config/kova/keymap.toml` (highest priority).
    UserOverride = 2,
}

/// Resolution target of a [`BindingRule`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum BindingTarget {
    /// Invoke an action identified by [`ActionId`].
    Action(ActionId),
    /// Explicitly block inherited bindings for this chord (`action = "none"`).
    Unbind,
}

/// Single keybinding rule within a [`KeymapLayer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingRule {
    /// Declaration index within the originating layer.
    pub id: usize,
    /// Layer kind governing base precedence.
    pub layer: KeymapLayerKind,
    /// Optional target OS restriction (`None` applies on all platforms).
    pub platform: Option<Platform>,
    /// Optional root [`FocusContext`] required in the active [`FocusStack`].
    pub root_context: Option<FocusContext>,
    /// Optional additional predicate expression (`when` or compound `context`).
    pub when: Option<ContextExpr>,
    /// Key chord or modifier double-tap that activates this rule.
    pub chord: KeyChord,
    /// Target action or explicit unbind.
    pub target: BindingTarget,
}

/// Collection of [`BindingRule`]s belonging to a single precedence layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeymapLayer {
    /// Layer kind (`DefaultPreset`, `UserOverride`, or `Plugin`).
    pub kind: KeymapLayerKind,
    /// Base preset mode configured in `keymap.toml`.
    pub preset: KeymapPresetMode,
    /// Ordered rules declared in this layer.
    pub rules: Vec<BindingRule>,
}

impl KeymapLayer {
    /// Creates a [`KeymapLayer`] from explicit parts.
    #[must_use]
    pub const fn new(
        kind: KeymapLayerKind,
        preset: KeymapPresetMode,
        rules: Vec<BindingRule>,
    ) -> Self {
        Self {
            kind,
            preset,
            rules,
        }
    }

    /// Creates an empty user override layer defaulting to
    /// [`KeymapPresetMode::JetBrains`].
    #[must_use]
    pub const fn empty_user_default() -> Self {
        Self {
            kind: KeymapLayerKind::UserOverride,
            preset: KeymapPresetMode::JetBrains,
            rules: Vec::new(),
        }
    }
}

/// Result of dispatching a key step or chord prefix through [`KeymapResolver`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyDispatch {
    /// A binding matched unambiguously and resolved to an [`ActionId`].
    Matched {
        /// Winning action identifier.
        action: ActionId,
        /// Full chord that triggered the match.
        chord: KeyChord,
    },
    /// The input matches the prefix of one or more multi-step chords.
    Pending {
        /// Steps entered so far.
        prefix: Vec<KeyStep>,
        /// Number of active candidate chords extending `prefix`.
        candidate_count: usize,
    },
    /// No active binding matched (or the winning rule was [`BindingTarget::Unbind`]).
    NotMatched,
}

/// Diagnostic category emitted by [`KeymapResolver::validate_bindings`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeymapDiagnosticKind {
    /// A binding references an [`ActionId`] not present in [`ActionRegistry`].
    UnknownAction {
        /// Rule declaration index.
        rule_id: usize,
        /// Layer containing the rule.
        layer: KeymapLayerKind,
        /// Unregistered action identifier.
        action_id: ActionId,
        /// Trigger chord.
        chord: KeyChord,
    },
    /// Two rules in the same layer, compatible platform, and identical context/when
    /// bind different targets to the same chord.
    AmbiguousConflict {
        /// Layer containing both rules.
        layer: KeymapLayerKind,
        /// Trigger chord shared by both rules.
        chord: KeyChord,
        /// First conflicting rule index.
        first_rule_id: usize,
        /// Second conflicting rule index.
        second_rule_id: usize,
    },
}

/// Validation diagnostic produced by [`KeymapResolver::validate_bindings`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeymapDiagnostic {
    /// Structured kind and metadata for the issue.
    pub kind: KeymapDiagnosticKind,
    /// Human-readable summary message.
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Precedence {
    layer: KeymapLayerKind,
    context_depth: u8,
    when_specificity: u16,
    rule_id: usize,
    insertion_seq: usize,
}

/// Deterministic, zero-I/O resolver composing keymap layers with first-step
/// indexing for sub-millisecond key dispatch.
#[derive(Debug, Clone)]
pub struct KeymapResolver {
    platform: Platform,
    rules: Vec<BindingRule>,
    first_step_index: HashMap<KeyStep, SmallVec<[usize; 4]>>,
}

impl KeymapResolver {
    /// Creates an empty [`KeymapResolver`] targeting `platform`.
    #[must_use]
    pub fn new(platform: Platform) -> Self {
        Self {
            platform,
            rules: Vec::new(),
            first_step_index: HashMap::new(),
        }
    }

    /// Builds a [`KeymapResolver`] from an iterator of [`KeymapLayer`]s.
    #[must_use]
    pub fn from_layers(platform: Platform, layers: impl IntoIterator<Item = KeymapLayer>) -> Self {
        let mut resolver = Self::new(platform);
        for layer in layers {
            resolver.add_layer(layer);
        }
        resolver
    }

    /// Appends all rules from `layer` and updates the first-step lookup index.
    pub fn add_layer(&mut self, layer: KeymapLayer) {
        for rule in layer.rules {
            let idx = self.rules.len();
            let active_for_platform =
                rule.platform.is_none() || rule.platform == Some(self.platform);
            if active_for_platform && let Some(&first_step) = rule.chord.steps().first() {
                self.first_step_index
                    .entry(first_step)
                    .or_default()
                    .push(idx);
            }
            self.rules.push(rule);
        }
    }

    /// Returns the target [`Platform`] of this resolver.
    #[must_use]
    pub const fn platform(&self) -> Platform {
        self.platform
    }

    /// Returns all registered rules across all added layers.
    #[must_use]
    pub fn rules(&self) -> &[BindingRule] {
        &self.rules
    }

    /// Resolves a single [`KeyStep`] on the hot path.
    #[must_use]
    pub fn resolve_step(
        &self,
        step: KeyStep,
        stack: &FocusStack,
        state: &ContextState,
    ) -> KeyDispatch {
        self.resolve_steps(std::slice::from_ref(&step), stack, state)
    }

    /// Resolves a single [`KeyStroke`] on the hot path.
    #[must_use]
    pub fn resolve_key_event(
        &self,
        stroke: KeyStroke,
        stack: &FocusStack,
        state: &ContextState,
    ) -> KeyDispatch {
        self.resolve_step(KeyStep::Stroke(stroke), stack, state)
    }

    /// Resolves a sequence of 1 or more [`KeyStep`]s against active rules,
    /// applying `(layer, context_depth, when_specificity, rule_id)` precedence.
    #[must_use]
    pub fn resolve_steps(
        &self,
        steps: &[KeyStep],
        stack: &FocusStack,
        state: &ContextState,
    ) -> KeyDispatch {
        let Some(&first_step) = steps.first() else {
            return KeyDispatch::NotMatched;
        };
        let Some(candidate_indices) = self.first_step_index.get(&first_step) else {
            return KeyDispatch::NotMatched;
        };

        let mut best_exact: Option<(Precedence, usize)> = None;
        let mut best_longer_prec: Option<Precedence> = None;
        let mut longer_chords: SmallVec<[&KeyChord; 4]> = SmallVec::new();

        for &idx in candidate_indices {
            let rule = &self.rules[idx];
            let rule_steps = rule.chord.steps();
            if rule_steps.len() < steps.len() || !rule_steps.starts_with(steps) {
                continue;
            }

            let Some(prec) = evaluate_rule(rule, idx, stack, state) else {
                continue;
            };

            if rule_steps.len() == steps.len() {
                if best_exact
                    .as_ref()
                    .is_none_or(|(cur_prec, _)| prec > *cur_prec)
                {
                    best_exact = Some((prec, idx));
                }
            } else {
                // Track active longer candidates that are not unbound by a higher rule.
                if best_longer_prec.is_none_or(|cur| prec > cur) {
                    best_longer_prec = Some(prec);
                }
                if !longer_chords.contains(&&rule.chord) {
                    longer_chords.push(&rule.chord);
                }
            }
        }

        // Filter out longer chords whose exact binding is overridden by Unbind in active scope.
        let active_longer_count = longer_chords
            .iter()
            .filter(|chord| !self.is_chord_unbound(chord, stack, state))
            .count();

        match best_exact {
            Some((exact_prec, exact_idx)) => {
                let rule = &self.rules[exact_idx];
                let longer_outranks =
                    active_longer_count > 0 && best_longer_prec.is_some_and(|lp| lp > exact_prec);
                if longer_outranks {
                    return KeyDispatch::Pending {
                        prefix: steps.to_vec(),
                        candidate_count: active_longer_count,
                    };
                }
                match &rule.target {
                    BindingTarget::Action(action) => KeyDispatch::Matched {
                        action: action.clone(),
                        chord: rule.chord.clone(),
                    },
                    BindingTarget::Unbind => {
                        if active_longer_count > 0 {
                            KeyDispatch::Pending {
                                prefix: steps.to_vec(),
                                candidate_count: active_longer_count,
                            }
                        } else {
                            KeyDispatch::NotMatched
                        }
                    }
                }
            }
            None => {
                if active_longer_count > 0 {
                    KeyDispatch::Pending {
                        prefix: steps.to_vec(),
                        candidate_count: active_longer_count,
                    }
                } else {
                    KeyDispatch::NotMatched
                }
            }
        }
    }

    /// Validates all registered bindings against `registry`, reporting unknown
    /// [`ActionId`]s and intra-layer conflicts.
    #[must_use]
    pub fn validate_bindings(&self, registry: &ActionRegistry) -> Vec<KeymapDiagnostic> {
        let mut diagnostics = Vec::new();

        for (i, rule) in self.rules.iter().enumerate() {
            if let BindingTarget::Action(action_id) = &rule.target
                && !registry.contains(action_id.as_str())
            {
                diagnostics.push(KeymapDiagnostic {
                    kind: KeymapDiagnosticKind::UnknownAction {
                        rule_id: rule.id,
                        layer: rule.layer,
                        action_id: action_id.clone(),
                        chord: rule.chord.clone(),
                    },
                    message: format!(
                        "rule #{} in {:?} references unknown action `{}` for `{}`",
                        rule.id, rule.layer, action_id, rule.chord
                    ),
                });
            }

            for other in &self.rules[i + 1..] {
                if rule.layer == other.layer
                    && platforms_overlap(rule.platform, other.platform)
                    && rule.root_context == other.root_context
                    && rule.when == other.when
                    && rule.chord == other.chord
                    && rule.target != other.target
                {
                    diagnostics.push(KeymapDiagnostic {
                        kind: KeymapDiagnosticKind::AmbiguousConflict {
                            layer: rule.layer,
                            chord: rule.chord.clone(),
                            first_rule_id: rule.id,
                            second_rule_id: other.id,
                        },
                        message: format!(
                            "conflicting bindings #{} and #{} in {:?} for `{}` in context `{:?}`",
                            rule.id, other.id, rule.layer, rule.chord, rule.root_context
                        ),
                    });
                }
            }
        }

        diagnostics
    }

    fn is_chord_unbound(&self, chord: &KeyChord, stack: &FocusStack, state: &ContextState) -> bool {
        let Some(&first_step) = chord.steps().first() else {
            return false;
        };
        let Some(indices) = self.first_step_index.get(&first_step) else {
            return false;
        };

        let mut best: Option<(Precedence, &BindingTarget)> = None;
        for &idx in indices {
            let rule = &self.rules[idx];
            if &rule.chord != chord {
                continue;
            }
            if let Some(prec) = evaluate_rule(rule, idx, stack, state)
                && best.as_ref().is_none_or(|(cur, _)| prec > *cur)
            {
                best = Some((prec, &rule.target));
            }
        }
        matches!(best, Some((_, BindingTarget::Unbind)))
    }
}

fn evaluate_rule(
    rule: &BindingRule,
    insertion_seq: usize,
    stack: &FocusStack,
    state: &ContextState,
) -> Option<Precedence> {
    let root_depth = match rule.root_context {
        Some(ctx) => stack.depth_of(ctx)?,
        None => 0,
    };

    let (when_depth, when_specificity) = match &rule.when {
        Some(expr) => {
            if !expr.matches(stack, state) {
                return None;
            }
            (
                expr.max_matched_context_depth(stack).unwrap_or(0),
                expr.specificity(),
            )
        }
        None => (0, 0),
    };

    Some(Precedence {
        layer: rule.layer,
        context_depth: root_depth.max(when_depth),
        when_specificity,
        rule_id: rule.id,
        insertion_seq,
    })
}

const fn platforms_overlap(a: Option<Platform>, b: Option<Platform>) -> bool {
    match (a, b) {
        (Some(pa), Some(pb)) => matches!(
            (pa, pb),
            (Platform::MacOs, Platform::MacOs) | (Platform::Linux, Platform::Linux)
        ),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{ActionCategory, ActionDescriptor};

    fn rule(
        id: usize,
        layer: KeymapLayerKind,
        ctx: FocusContext,
        when: Option<&str>,
        chord: &str,
        action: Option<&str>,
    ) -> BindingRule {
        BindingRule {
            id,
            layer,
            platform: None,
            root_context: Some(ctx),
            when: when.map(|w| ContextExpr::parse(w).expect("valid when")),
            chord: KeyChord::parse(chord, Platform::MacOs).expect("valid chord"),
            target: match action {
                Some(a) => BindingTarget::Action(ActionId::new(a).expect("valid action")),
                None => BindingTarget::Unbind,
            },
        }
    }

    #[test]
    fn precedence_orders_layer_then_context_depth_then_when_then_rule_order() {
        let mut resolver = KeymapResolver::new(Platform::MacOs);
        resolver.add_layer(KeymapLayer::new(
            KeymapLayerKind::DefaultPreset,
            KeymapPresetMode::JetBrains,
            vec![
                rule(
                    0,
                    KeymapLayerKind::DefaultPreset,
                    FocusContext::Global,
                    None,
                    "cmd+d",
                    Some("global.default"),
                ),
                rule(
                    1,
                    KeymapLayerKind::DefaultPreset,
                    FocusContext::EditorText,
                    None,
                    "cmd+d",
                    Some("editor.duplicate_line"),
                ),
                rule(
                    2,
                    KeymapLayerKind::DefaultPreset,
                    FocusContext::EditorText,
                    Some("!vim_enabled"),
                    "cmd+d",
                    Some("editor.duplicate_non_vim"),
                ),
                rule(
                    3,
                    KeymapLayerKind::DefaultPreset,
                    FocusContext::EditorVimNormal,
                    None,
                    "cmd+d",
                    Some("vim.half_page_down"),
                ),
            ],
        ));

        let chord = KeyChord::parse("cmd+d", Platform::MacOs).expect("chord");
        let stroke = match chord.steps()[0] {
            KeyStep::Stroke(s) => s,
            KeyStep::DoubleTapModifier(_) => unreachable!(),
        };

        // At EditorText with !vim_enabled, `when` specificity breaks tie over plain EditorText.
        let editor_stack = FocusStack::from_leaf(FocusContext::EditorText);
        let non_vim = ContextState::new();
        assert_eq!(
            resolver.resolve_key_event(stroke, &editor_stack, &non_vim),
            KeyDispatch::Matched {
                action: ActionId::from_static("editor.duplicate_non_vim"),
                chord: chord.clone(),
            }
        );

        // At EditorVimNormal (depth 4 > EditorText depth 3), Vim binding wins.
        let vim_stack = FocusStack::from_leaf(FocusContext::EditorVimNormal);
        let vim_state = ContextState::new().with_flag("vim_enabled", true);
        assert_eq!(
            resolver.resolve_key_event(stroke, &vim_stack, &vim_state),
            KeyDispatch::Matched {
                action: ActionId::from_static("vim.half_page_down"),
                chord: chord.clone(),
            }
        );

        // UserOverride layer beats DefaultPreset even at shallower context.
        resolver.add_layer(KeymapLayer::new(
            KeymapLayerKind::UserOverride,
            KeymapPresetMode::JetBrains,
            vec![rule(
                0,
                KeymapLayerKind::UserOverride,
                FocusContext::Workspace,
                None,
                "cmd+d",
                Some("user.custom_d"),
            )],
        ));
        assert_eq!(
            resolver.resolve_key_event(stroke, &vim_stack, &vim_state),
            KeyDispatch::Matched {
                action: ActionId::from_static("user.custom_d"),
                chord,
            }
        );
    }

    #[test]
    fn user_unbind_blocks_inherited_preset_binding() {
        let mut resolver = KeymapResolver::new(Platform::MacOs);
        resolver.add_layer(KeymapLayer::new(
            KeymapLayerKind::DefaultPreset,
            KeymapPresetMode::JetBrains,
            vec![rule(
                0,
                KeymapLayerKind::DefaultPreset,
                FocusContext::EditorText,
                None,
                "cmd+d",
                Some("editor.duplicate_line"),
            )],
        ));
        resolver.add_layer(KeymapLayer::new(
            KeymapLayerKind::UserOverride,
            KeymapPresetMode::JetBrains,
            vec![rule(
                0,
                KeymapLayerKind::UserOverride,
                FocusContext::EditorText,
                None,
                "cmd+d",
                None,
            )],
        ));

        let stroke = KeyStroke::new(Modifiers::META, KeyCode::Char('d'));
        let stack = FocusStack::from_leaf(FocusContext::EditorText);
        assert_eq!(
            resolver.resolve_key_event(stroke, &stack, &ContextState::new()),
            KeyDispatch::NotMatched
        );
    }

    #[test]
    fn validate_bindings_reports_unknown_actions_and_intra_layer_conflicts() {
        let mut registry = ActionRegistry::new();
        registry
            .register(ActionDescriptor::builtin(
                ActionId::from_static("editor.duplicate_line"),
                "Duplicate Line",
                ActionCategory::Editor,
            ))
            .expect("register");

        let mut resolver = KeymapResolver::new(Platform::MacOs);
        resolver.add_layer(KeymapLayer::new(
            KeymapLayerKind::DefaultPreset,
            KeymapPresetMode::JetBrains,
            vec![
                rule(
                    0,
                    KeymapLayerKind::DefaultPreset,
                    FocusContext::EditorText,
                    None,
                    "cmd+d",
                    Some("editor.duplicate_line"),
                ),
                rule(
                    1,
                    KeymapLayerKind::DefaultPreset,
                    FocusContext::EditorText,
                    None,
                    "cmd+d",
                    Some("editor.missing_action"),
                ),
            ],
        ));

        let diags = resolver.validate_bindings(&registry);
        assert_eq!(diags.len(), 2);
        assert!(diags.iter().any(|d| matches!(
            d.kind,
            KeymapDiagnosticKind::UnknownAction { rule_id: 1, .. }
        )));
        assert!(diags.iter().any(|d| matches!(
            d.kind,
            KeymapDiagnosticKind::AmbiguousConflict {
                first_rule_id: 0,
                second_rule_id: 1,
                ..
            }
        )));
    }
}
