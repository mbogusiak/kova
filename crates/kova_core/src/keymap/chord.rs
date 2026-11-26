//! Deterministic chord & modifier double-tap state machine (`KeymapSession`).

use std::time::{Duration, Instant};

use smallvec::SmallVec;

use crate::context::{ContextState, FocusStack};
use crate::keymap::stroke::{KeyCode, KeyStep, KeyStroke, ModifierTap};
use crate::keymap::{KeyDispatch, KeymapResolver};

/// Default timeout between consecutive steps of a multi-key chord (`1000 ms`).
pub const DEFAULT_CHORD_TIMEOUT: Duration = Duration::from_millis(1_000);

/// Default time window for detecting two clean taps of a modifier (`400 ms`).
pub const DEFAULT_DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(400);

/// Deterministic state machine detecting clean modifier double-taps such as
/// `Shift↓ -> Shift↑ -> Shift↓ -> Shift↑` while rejecting normal modified keys
/// like `Shift+A`.
#[derive(Debug, Clone)]
pub struct ModifierTapDetector {
    window: Duration,
    state: TapState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TapState {
    Idle,
    FirstPressed {
        tap: ModifierTap,
        pressed_at: Instant,
    },
    FirstReleased {
        tap: ModifierTap,
        first_pressed_at: Instant,
        released_at: Instant,
    },
    SecondPressed {
        tap: ModifierTap,
        first_pressed_at: Instant,
    },
}

impl ModifierTapDetector {
    /// Creates a new [`ModifierTapDetector`] with the given `window`.
    #[must_use]
    pub const fn new(window: Duration) -> Self {
        Self {
            window,
            state: TapState::Idle,
        }
    }

    /// Cancels any in-progress tap sequence (called on any non-modifier key press).
    pub const fn cancel(&mut self) {
        self.state = TapState::Idle;
    }

    /// Feeds a modifier edge (`pressed = true` on key-down, `false` on key-up)
    /// and returns `Some(tap)` upon the second clean release within `window`.
    pub fn on_edge(
        &mut self,
        tap: ModifierTap,
        pressed: bool,
        now: Instant,
    ) -> Option<ModifierTap> {
        if pressed {
            match self.state {
                TapState::Idle => {
                    self.state = TapState::FirstPressed {
                        tap,
                        pressed_at: now,
                    };
                }
                TapState::FirstPressed { tap: prev, .. }
                | TapState::SecondPressed { tap: prev, .. } => {
                    if prev != tap {
                        self.state = TapState::Idle;
                    }
                }
                TapState::FirstReleased {
                    tap: prev,
                    first_pressed_at,
                    released_at,
                } => {
                    if prev == tap && now.saturating_duration_since(released_at) <= self.window {
                        self.state = TapState::SecondPressed {
                            tap,
                            first_pressed_at,
                        };
                    } else {
                        self.state = TapState::FirstPressed {
                            tap,
                            pressed_at: now,
                        };
                    }
                }
            }
            return None;
        }

        // Release edge (`pressed == false`)
        match self.state {
            TapState::FirstPressed {
                tap: prev,
                pressed_at,
            } if prev == tap => {
                if now.saturating_duration_since(pressed_at) <= self.window {
                    self.state = TapState::FirstReleased {
                        tap,
                        first_pressed_at: pressed_at,
                        released_at: now,
                    };
                } else {
                    self.state = TapState::Idle;
                }
                None
            }
            TapState::SecondPressed {
                tap: prev,
                first_pressed_at,
            } if prev == tap => {
                self.state = TapState::Idle;
                if now.saturating_duration_since(first_pressed_at) <= self.window {
                    Some(tap)
                } else {
                    None
                }
            }
            _ => {
                self.state = TapState::Idle;
                None
            }
        }
    }
}

impl Default for ModifierTapDetector {
    fn default() -> Self {
        Self::new(DEFAULT_DOUBLE_TAP_WINDOW)
    }
}

/// Stateful input dispatch session tracking pending multi-key chord prefixes
/// and modifier double-taps using caller-supplied `Instant` timestamps.
#[derive(Debug, Clone)]
pub struct KeymapSession {
    pending_steps: SmallVec<[KeyStep; 3]>,
    last_step_at: Option<Instant>,
    tap_detector: ModifierTapDetector,
    chord_timeout: Duration,
}

impl KeymapSession {
    /// Creates a new [`KeymapSession`] with [`DEFAULT_CHORD_TIMEOUT`] and
    /// [`DEFAULT_DOUBLE_TAP_WINDOW`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending_steps: SmallVec::new(),
            last_step_at: None,
            tap_detector: ModifierTapDetector::new(DEFAULT_DOUBLE_TAP_WINDOW),
            chord_timeout: DEFAULT_CHORD_TIMEOUT,
        }
    }

    /// Overrides the timeout for multi-step chords.
    #[must_use]
    pub const fn with_chord_timeout(mut self, timeout: Duration) -> Self {
        self.chord_timeout = timeout;
        self
    }

    /// Overrides the maximum window for modifier double-tap detection.
    #[must_use]
    pub const fn with_double_tap_window(mut self, window: Duration) -> Self {
        self.tap_detector.window = window;
        self
    }

    /// Clears all pending chord steps and resets the double-tap detector.
    pub fn reset(&mut self) {
        self.pending_steps.clear();
        self.last_step_at = None;
        self.tap_detector.cancel();
    }

    /// Cancels any in-progress modifier double-tap sequence without clearing
    /// pending key chord steps.
    pub const fn cancel_modifier_tap(&mut self) {
        self.tap_detector.cancel();
    }

    /// Returns the currently buffered chord prefix steps.
    #[must_use]
    pub fn pending_steps(&self) -> &[KeyStep] {
        &self.pending_steps
    }

    /// Processes a normal key-down event and dispatches against `resolver`.
    ///
    /// - Any pending chord prefix older than `chord_timeout` is discarded.
    /// - Pressing unmodified `Escape` while a chord prefix is pending cancels
    ///   the chord and returns [`KeyDispatch::NotMatched`].
    /// - If a second key does not continue a pending chord, the pending prefix
    ///   is cleared and the new key is retried as a fresh single step.
    pub fn on_key_down(
        &mut self,
        resolver: &KeymapResolver,
        stroke: KeyStroke,
        stack: &FocusStack,
        state: &ContextState,
        now: Instant,
    ) -> KeyDispatch {
        self.tap_detector.cancel();
        self.expire_pending_if_needed(now);

        let had_pending = !self.pending_steps.is_empty();
        if had_pending && stroke == KeyStroke::unmodified(KeyCode::Escape) {
            self.pending_steps.clear();
            self.last_step_at = None;
            return KeyDispatch::NotMatched;
        }

        let step = KeyStep::Stroke(stroke);
        let mut candidate: SmallVec<[KeyStep; 3]> = self.pending_steps.clone();
        candidate.push(step);

        match resolver.resolve_steps(&candidate, stack, state) {
            matched @ KeyDispatch::Matched { .. } => {
                self.pending_steps.clear();
                self.last_step_at = None;
                matched
            }
            pending @ KeyDispatch::Pending { .. } => {
                self.pending_steps = candidate;
                self.last_step_at = Some(now);
                pending
            }
            KeyDispatch::NotMatched => {
                self.pending_steps.clear();
                self.last_step_at = None;
                if !had_pending {
                    return KeyDispatch::NotMatched;
                }
                match resolver.resolve_step(step, stack, state) {
                    matched @ KeyDispatch::Matched { .. } => matched,
                    pending @ KeyDispatch::Pending { .. } => {
                        self.pending_steps.push(step);
                        self.last_step_at = Some(now);
                        pending
                    }
                    KeyDispatch::NotMatched => KeyDispatch::NotMatched,
                }
            }
        }
    }

    /// Processes a modifier press/release edge and dispatches a double-tap
    /// chord (`Shift+Shift`, `Ctrl+Ctrl`, etc.) when detected.
    pub fn on_modifier_edge(
        &mut self,
        resolver: &KeymapResolver,
        tap: ModifierTap,
        pressed: bool,
        stack: &FocusStack,
        state: &ContextState,
        now: Instant,
    ) -> KeyDispatch {
        let Some(detected) = self.tap_detector.on_edge(tap, pressed, now) else {
            return KeyDispatch::NotMatched;
        };

        self.expire_pending_if_needed(now);
        let step = KeyStep::DoubleTapModifier(detected);
        let mut candidate: SmallVec<[KeyStep; 3]> = self.pending_steps.clone();
        candidate.push(step);

        match resolver.resolve_steps(&candidate, stack, state) {
            matched @ KeyDispatch::Matched { .. } => {
                self.pending_steps.clear();
                self.last_step_at = None;
                matched
            }
            pending @ KeyDispatch::Pending { .. } => {
                self.pending_steps = candidate;
                self.last_step_at = Some(now);
                pending
            }
            KeyDispatch::NotMatched => {
                let had_pending = !self.pending_steps.is_empty();
                self.pending_steps.clear();
                self.last_step_at = None;
                if !had_pending {
                    return KeyDispatch::NotMatched;
                }
                match resolver.resolve_step(step, stack, state) {
                    matched @ KeyDispatch::Matched { .. } => matched,
                    pending @ KeyDispatch::Pending { .. } => {
                        self.pending_steps.push(step);
                        self.last_step_at = Some(now);
                        pending
                    }
                    KeyDispatch::NotMatched => KeyDispatch::NotMatched,
                }
            }
        }
    }

    fn expire_pending_if_needed(&mut self, now: Instant) {
        if let Some(last) = self.last_step_at
            && now.saturating_duration_since(last) > self.chord_timeout
        {
            self.pending_steps.clear();
            self.last_step_at = None;
        }
    }
}

impl Default for KeymapSession {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::ActionId;
    use crate::context::FocusContext;
    use crate::keymap::parser::parse_keymap_toml;
    use crate::keymap::stroke::{Modifiers, Platform};
    use crate::keymap::{BindingRule, BindingTarget, KeyChord, KeymapLayer, KeymapLayerKind};

    fn test_resolver() -> KeymapResolver {
        let toml = r#"
[[bindings]]
context = "Global"
key = "shift shift"
action = "navigation.search_everywhere"

[[bindings]]
context = "EditorText"
key = "cmd+k cmd+c"
action = "editor.toggle_line_comment"

[[bindings]]
context = "EditorText"
key = "cmd+d"
action = "editor.duplicate_line"
"#;
        let user_layer = parse_keymap_toml(toml, Platform::MacOs).expect("valid toml");
        let mut resolver = KeymapResolver::new(Platform::MacOs);
        resolver.add_layer(user_layer);
        resolver
    }

    #[test]
    fn double_shift_triggers_only_on_second_clean_release_within_window() {
        let resolver = test_resolver();
        let stack = FocusStack::from_leaf(FocusContext::Global);
        let state = ContextState::new();
        let mut session = KeymapSession::new();

        let t0 = Instant::now();
        assert_eq!(
            session.on_modifier_edge(&resolver, ModifierTap::Shift, true, &stack, &state, t0),
            KeyDispatch::NotMatched
        );
        assert_eq!(
            session.on_modifier_edge(
                &resolver,
                ModifierTap::Shift,
                false,
                &stack,
                &state,
                t0 + Duration::from_millis(50)
            ),
            KeyDispatch::NotMatched
        );
        assert_eq!(
            session.on_modifier_edge(
                &resolver,
                ModifierTap::Shift,
                true,
                &stack,
                &state,
                t0 + Duration::from_millis(120)
            ),
            KeyDispatch::NotMatched
        );
        assert_eq!(
            session.on_modifier_edge(
                &resolver,
                ModifierTap::Shift,
                false,
                &stack,
                &state,
                t0 + Duration::from_millis(180)
            ),
            KeyDispatch::Matched {
                action: ActionId::from_static("navigation.search_everywhere"),
                chord: KeyChord::double_tap(ModifierTap::Shift),
            }
        );
    }

    #[test]
    fn shift_a_shift_b_does_not_trigger_double_shift() {
        let resolver = test_resolver();
        let stack = FocusStack::from_leaf(FocusContext::EditorText);
        let state = ContextState::new();
        let mut session = KeymapSession::new();
        let t0 = Instant::now();

        let shift_a = KeyStroke::new(Modifiers::SHIFT, KeyCode::Char('a'));
        let shift_b = KeyStroke::new(Modifiers::SHIFT, KeyCode::Char('b'));

        assert_eq!(
            session.on_modifier_edge(&resolver, ModifierTap::Shift, true, &stack, &state, t0),
            KeyDispatch::NotMatched
        );
        assert_eq!(
            session.on_key_down(
                &resolver,
                shift_a,
                &stack,
                &state,
                t0 + Duration::from_millis(20)
            ),
            KeyDispatch::NotMatched
        );
        assert_eq!(
            session.on_modifier_edge(
                &resolver,
                ModifierTap::Shift,
                false,
                &stack,
                &state,
                t0 + Duration::from_millis(40)
            ),
            KeyDispatch::NotMatched
        );
        assert_eq!(
            session.on_modifier_edge(
                &resolver,
                ModifierTap::Shift,
                true,
                &stack,
                &state,
                t0 + Duration::from_millis(80)
            ),
            KeyDispatch::NotMatched
        );
        assert_eq!(
            session.on_key_down(
                &resolver,
                shift_b,
                &stack,
                &state,
                t0 + Duration::from_millis(100)
            ),
            KeyDispatch::NotMatched
        );
        assert_eq!(
            session.on_modifier_edge(
                &resolver,
                ModifierTap::Shift,
                false,
                &stack,
                &state,
                t0 + Duration::from_millis(120)
            ),
            KeyDispatch::NotMatched
        );
    }

    #[test]
    fn double_shift_expires_past_400ms_window() {
        let resolver = test_resolver();
        let stack = FocusStack::from_leaf(FocusContext::Global);
        let state = ContextState::new();
        let mut session = KeymapSession::new();
        let t0 = Instant::now();

        session.on_modifier_edge(&resolver, ModifierTap::Shift, true, &stack, &state, t0);
        session.on_modifier_edge(
            &resolver,
            ModifierTap::Shift,
            false,
            &stack,
            &state,
            t0 + Duration::from_millis(40),
        );
        session.on_modifier_edge(
            &resolver,
            ModifierTap::Shift,
            true,
            &stack,
            &state,
            t0 + Duration::from_millis(460),
        );
        assert_eq!(
            session.on_modifier_edge(
                &resolver,
                ModifierTap::Shift,
                false,
                &stack,
                &state,
                t0 + Duration::from_millis(500)
            ),
            KeyDispatch::NotMatched
        );
    }

    #[test]
    fn multi_step_chord_pending_match_escape_and_fallback_retry() {
        let resolver = test_resolver();
        let stack = FocusStack::from_leaf(FocusContext::EditorText);
        let state = ContextState::new();
        let mut session = KeymapSession::new();
        let t0 = Instant::now();

        let cmd_k = KeyStroke::new(Modifiers::META, KeyCode::Char('k'));
        let cmd_c = KeyStroke::new(Modifiers::META, KeyCode::Char('c'));
        let cmd_d = KeyStroke::new(Modifiers::META, KeyCode::Char('d'));
        let esc = KeyStroke::unmodified(KeyCode::Escape);

        // Step 1 -> Pending, Step 2 -> Matched
        assert!(matches!(
            session.on_key_down(&resolver, cmd_k, &stack, &state, t0),
            KeyDispatch::Pending {
                candidate_count: 1,
                ..
            }
        ));
        assert_eq!(session.pending_steps().len(), 1);
        assert_eq!(
            session.on_key_down(
                &resolver,
                cmd_c,
                &stack,
                &state,
                t0 + Duration::from_millis(150)
            ),
            KeyDispatch::Matched {
                action: ActionId::from_static("editor.toggle_line_comment"),
                chord: KeyChord::parse("cmd+k cmd+c", Platform::MacOs).expect("chord"),
            }
        );
        assert_eq!(session.pending_steps(), &[]);

        // Escape cancels pending chord without retrying Escape itself
        assert!(matches!(
            session.on_key_down(&resolver, cmd_k, &stack, &state, t0),
            KeyDispatch::Pending { .. }
        ));
        assert_eq!(
            session.on_key_down(
                &resolver,
                esc,
                &stack,
                &state,
                t0 + Duration::from_millis(50)
            ),
            KeyDispatch::NotMatched
        );
        assert_eq!(session.pending_steps(), &[]);

        // Non-matching follow-up retries as a fresh single step (`cmd+d`)
        assert!(matches!(
            session.on_key_down(&resolver, cmd_k, &stack, &state, t0),
            KeyDispatch::Pending { .. }
        ));
        assert_eq!(
            session.on_key_down(
                &resolver,
                cmd_d,
                &stack,
                &state,
                t0 + Duration::from_millis(50)
            ),
            KeyDispatch::Matched {
                action: ActionId::from_static("editor.duplicate_line"),
                chord: KeyChord::single(cmd_d),
            }
        );

        // Chord timeout (> 1000 ms) drops stale prefix before evaluating next key
        assert!(matches!(
            session.on_key_down(&resolver, cmd_k, &stack, &state, t0),
            KeyDispatch::Pending { .. }
        ));
        assert_eq!(
            session.on_key_down(
                &resolver,
                cmd_c,
                &stack,
                &state,
                t0 + Duration::from_millis(1_100)
            ),
            KeyDispatch::NotMatched
        );
        assert_eq!(session.pending_steps(), &[]);
    }

    #[test]
    fn mixed_modifier_press_cancels_double_tap_candidate() {
        let mut resolver = KeymapResolver::new(Platform::MacOs);
        resolver.add_layer(KeymapLayer::new(
            KeymapLayerKind::DefaultPreset,
            crate::keymap::KeymapPresetMode::JetBrains,
            vec![BindingRule {
                id: 0,
                layer: KeymapLayerKind::DefaultPreset,
                platform: None,
                root_context: Some(FocusContext::Global),
                when: None,
                chord: KeyChord::double_tap(ModifierTap::Shift),
                target: BindingTarget::Action(ActionId::from_static(
                    "navigation.search_everywhere",
                )),
            }],
        ));

        let stack = FocusStack::default();
        let state = ContextState::new();
        let mut session = KeymapSession::new();
        let t0 = Instant::now();

        session.on_modifier_edge(&resolver, ModifierTap::Shift, true, &stack, &state, t0);
        session.on_modifier_edge(
            &resolver,
            ModifierTap::Ctrl,
            true,
            &stack,
            &state,
            t0 + Duration::from_millis(20),
        );
        session.on_modifier_edge(
            &resolver,
            ModifierTap::Shift,
            false,
            &stack,
            &state,
            t0 + Duration::from_millis(40),
        );
        session.on_modifier_edge(
            &resolver,
            ModifierTap::Shift,
            true,
            &stack,
            &state,
            t0 + Duration::from_millis(80),
        );
        assert_eq!(
            session.on_modifier_edge(
                &resolver,
                ModifierTap::Shift,
                false,
                &stack,
                &state,
                t0 + Duration::from_millis(120)
            ),
            KeyDispatch::NotMatched
        );
    }
}
