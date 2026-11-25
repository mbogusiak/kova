//! Platform-aware keystroke, key chord, and modifier double-tap representation.

use std::fmt;

use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::keymap::parser::KeymapParseError;

/// Maximum number of steps permitted in a single [`KeyChord`].
pub const MAX_CHORD_STEPS: usize = 4;

/// Target operating system for platform-specific keybindings (`primary` modifier
/// expansion and `os = "macos" | "linux"` rule filtering).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    /// Apple macOS (`cmd` / `meta` as primary shortcut modifier).
    #[serde(rename = "macos")]
    MacOs,
    /// Linux / X11 / Wayland (`ctrl` as primary shortcut modifier).
    Linux,
}

impl Platform {
    /// Returns the host platform at compile time (defaults to [`Self::Linux`] on
    /// non-macOS Unix/headless targets).
    #[must_use]
    pub const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Linux
        }
    }

    /// Returns the canonical lowercase identifier (`"macos"` or `"linux"`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MacOs => "macos",
            Self::Linux => "linux",
        }
    }

    /// Parses a platform string (`"macos"`, `"mac"`, `"linux"`).
    #[must_use]
    pub fn parse_name(input: &str) -> Option<Self> {
        let trimmed = input.trim();
        if trimmed.eq_ignore_ascii_case("macos") || trimmed.eq_ignore_ascii_case("mac") {
            Some(Self::MacOs)
        } else if trimmed.eq_ignore_ascii_case("linux") {
            Some(Self::Linux)
        } else {
            None
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Modifier key flags for a [`KeyStroke`].
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "four orthogonal keyboard modifier flags match keystroke model"
)]
pub struct Modifiers {
    /// Control modifier (`ctrl` / `control`).
    pub ctrl: bool,
    /// Alt / Option modifier (`alt` / `opt` / `option`).
    pub alt: bool,
    /// Shift modifier (`shift`).
    pub shift: bool,
    /// Meta / Command / Super modifier (`cmd` / `super` / `meta`).
    pub meta: bool,
}

impl Modifiers {
    /// No modifiers pressed.
    pub const NONE: Self = Self {
        ctrl: false,
        alt: false,
        shift: false,
        meta: false,
    };

    /// Only Control pressed.
    pub const CTRL: Self = Self {
        ctrl: true,
        alt: false,
        shift: false,
        meta: false,
    };

    /// Only Alt / Option pressed.
    pub const ALT: Self = Self {
        ctrl: false,
        alt: true,
        shift: false,
        meta: false,
    };

    /// Only Shift pressed.
    pub const SHIFT: Self = Self {
        ctrl: false,
        alt: false,
        shift: true,
        meta: false,
    };

    /// Only Meta / Command pressed.
    pub const META: Self = Self {
        ctrl: false,
        alt: false,
        shift: false,
        meta: true,
    };

    /// Returns the platform's primary shortcut modifier (`meta` on macOS, `ctrl`
    /// on Linux).
    #[must_use]
    pub const fn primary(platform: Platform) -> Self {
        match platform {
            Platform::MacOs => Self::META,
            Platform::Linux => Self::CTRL,
        }
    }

    /// Returns `true` when no modifier flags are set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        !self.ctrl && !self.alt && !self.shift && !self.meta
    }
}

/// Headless key code independent of GPUI/winit key representations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum KeyCode {
    /// Printable character normalized to ASCII lowercase for letters `a..=z`.
    Char(char),
    /// Enter / Return key.
    Enter,
    /// Tab key.
    Tab,
    /// Backspace key.
    Backspace,
    /// Forward Delete key.
    Delete,
    /// Escape key.
    Escape,
    /// Spacebar.
    Space,
    /// Left arrow.
    Left,
    /// Right arrow.
    Right,
    /// Up arrow.
    Up,
    /// Down arrow.
    Down,
    /// Home key.
    Home,
    /// End key.
    End,
    /// Page Up key.
    PageUp,
    /// Page Down key.
    PageDown,
    /// Insert key.
    Insert,
    /// Function key (`F1`..=`F24`).
    F(u8),
}

impl KeyCode {
    /// Normalizes character casing (ASCII letters become lowercase).
    #[must_use]
    pub const fn normalized_char(ch: char) -> Self {
        Self::Char(ch.to_ascii_lowercase())
    }

    fn parse_token(token: &str) -> Option<Self> {
        let lower = token.to_ascii_lowercase();
        match lower.as_str() {
            "enter" | "return" | "ret" => Some(Self::Enter),
            "tab" => Some(Self::Tab),
            "backspace" | "bs" => Some(Self::Backspace),
            "delete" | "del" => Some(Self::Delete),
            "escape" | "esc" => Some(Self::Escape),
            "space" => Some(Self::Space),
            "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            "up" => Some(Self::Up),
            "down" => Some(Self::Down),
            "home" => Some(Self::Home),
            "end" => Some(Self::End),
            "pageup" | "page_up" | "pgup" => Some(Self::PageUp),
            "pagedown" | "page_down" | "pgdn" => Some(Self::PageDown),
            "insert" | "ins" => Some(Self::Insert),
            "plus" => Some(Self::Char('+')),
            "minus" => Some(Self::Char('-')),
            "comma" => Some(Self::Char(',')),
            "period" | "dot" => Some(Self::Char('.')),
            "slash" => Some(Self::Char('/')),
            "backslash" => Some(Self::Char('\\')),
            _ => {
                if let Some(num_str) = lower.strip_prefix('f')
                    && let Ok(num) = num_str.parse::<u8>()
                    && (1..=24).contains(&num)
                {
                    return Some(Self::F(num));
                }
                let mut chars = token.chars();
                let ch = chars.next()?;
                if chars.next().is_none() && !ch.is_whitespace() && !ch.is_control() {
                    return Some(Self::normalized_char(ch));
                }
                None
            }
        }
    }
}

impl fmt::Display for KeyCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Char('+') => f.write_str("plus"),
            Self::Char(ch) => write!(f, "{ch}"),
            Self::Enter => f.write_str("enter"),
            Self::Tab => f.write_str("tab"),
            Self::Backspace => f.write_str("backspace"),
            Self::Delete => f.write_str("delete"),
            Self::Escape => f.write_str("escape"),
            Self::Space => f.write_str("space"),
            Self::Left => f.write_str("left"),
            Self::Right => f.write_str("right"),
            Self::Up => f.write_str("up"),
            Self::Down => f.write_str("down"),
            Self::Home => f.write_str("home"),
            Self::End => f.write_str("end"),
            Self::PageUp => f.write_str("page_up"),
            Self::PageDown => f.write_str("page_down"),
            Self::Insert => f.write_str("insert"),
            Self::F(n) => write!(f, "f{n}"),
        }
    }
}

/// Single keyboard key-down event normalized to modifiers + key code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct KeyStroke {
    /// Active modifier flags.
    pub modifiers: Modifiers,
    /// Base key pressed.
    pub key: KeyCode,
}

impl KeyStroke {
    /// Creates a [`KeyStroke`] from modifiers and a base key.
    #[must_use]
    pub const fn new(modifiers: Modifiers, key: KeyCode) -> Self {
        let key = match key {
            KeyCode::Char(c) => KeyCode::Char(c.to_ascii_lowercase()),
            other => other,
        };
        Self { modifiers, key }
    }

    /// Creates an unmodified [`KeyStroke`].
    #[must_use]
    pub const fn unmodified(key: KeyCode) -> Self {
        Self::new(Modifiers::NONE, key)
    }
}

impl fmt::Display for KeyStroke {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.modifiers.ctrl {
            f.write_str("ctrl+")?;
        }
        if self.modifiers.alt {
            f.write_str("alt+")?;
        }
        if self.modifiers.shift {
            f.write_str("shift+")?;
        }
        if self.modifiers.meta {
            f.write_str("cmd+")?;
        }
        write!(f, "{}", self.key)
    }
}

/// Modifier key that can be double-tapped as a shortcut (for example `Shift+Shift`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ModifierTap {
    /// Double-tap Shift (*Search Everywhere* in `JetBrains`).
    Shift,
    /// Double-tap Control (*Run Anything* in `JetBrains`).
    Ctrl,
    /// Double-tap Alt / Option.
    Alt,
    /// Double-tap Meta / Command.
    Meta,
}

impl ModifierTap {
    fn parse_token(token: &str, platform: Platform) -> Option<Self> {
        match token.to_ascii_lowercase().as_str() {
            "shift" => Some(Self::Shift),
            "ctrl" | "control" => Some(Self::Ctrl),
            "alt" | "opt" | "option" => Some(Self::Alt),
            "cmd" | "super" | "meta" => Some(Self::Meta),
            "primary" => Some(match platform {
                Platform::MacOs => Self::Meta,
                Platform::Linux => Self::Ctrl,
            }),
            _ => None,
        }
    }

    /// Returns the canonical lowercase modifier name (`"shift"`, `"ctrl"`, `"alt"`, `"cmd"`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Shift => "shift",
            Self::Ctrl => "ctrl",
            Self::Alt => "alt",
            Self::Meta => "cmd",
        }
    }
}

impl fmt::Display for ModifierTap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Single step in a [`KeyChord`]: either a key stroke or a modifier double-tap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum KeyStep {
    /// Standard key-down with optional modifiers.
    Stroke(KeyStroke),
    /// Two consecutive clean taps of a modifier key within the double-tap window.
    DoubleTapModifier(ModifierTap),
}

impl fmt::Display for KeyStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stroke(stroke) => write!(f, "{stroke}"),
            Self::DoubleTapModifier(tap) => write!(f, "{tap} {tap}"),
        }
    }
}

/// Sequence of 1 to 4 [`KeyStep`]s representing a full keybinding shortcut.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyChord {
    steps: SmallVec<[KeyStep; 2]>,
}

impl KeyChord {
    /// Constructs a single-stroke [`KeyChord`].
    #[must_use]
    pub fn single(stroke: KeyStroke) -> Self {
        let mut steps = SmallVec::new();
        steps.push(KeyStep::Stroke(stroke));
        Self { steps }
    }

    /// Constructs a single-step modifier double-tap [`KeyChord`].
    #[must_use]
    pub fn double_tap(tap: ModifierTap) -> Self {
        let mut steps = SmallVec::new();
        steps.push(KeyStep::DoubleTapModifier(tap));
        Self { steps }
    }

    /// Constructs a [`KeyChord`] from a non-empty list of steps (up to
    /// [`MAX_CHORD_STEPS`]).
    ///
    /// # Errors
    ///
    /// Returns [`KeymapParseError::InvalidKeyChord`] if `steps` is empty or
    /// exceeds [`MAX_CHORD_STEPS`].
    pub fn from_steps(steps: impl IntoIterator<Item = KeyStep>) -> Result<Self, KeymapParseError> {
        let collected: SmallVec<[KeyStep; 2]> = steps.into_iter().collect();
        if collected.is_empty() || collected.len() > MAX_CHORD_STEPS {
            return Err(KeymapParseError::InvalidKeyChord {
                binding_index: None,
                key: String::new(),
                reason: "chord must contain between 1 and 4 steps".to_owned(),
            });
        }
        Ok(Self { steps: collected })
    }

    /// Parses a chord string such as `"cmd+shift+o"`, `"cmd+k cmd+c"`,
    /// `"primary+d"`, or `"shift shift"` / `"shift+shift"` / `"double+shift"`.
    ///
    /// # Errors
    ///
    /// Returns [`KeymapParseError::InvalidKeyChord`] if any token is empty,
    /// unknown, missing a base key (outside double-tap), or exceeds 4 steps.
    pub fn parse(input: &str, platform: Platform) -> Result<Self, KeymapParseError> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(invalid_chord(input, "key chord cannot be empty"));
        }

        let raw_parts: Vec<&str> = trimmed.split_whitespace().collect();

        // Recognize `"shift shift"`, `"ctrl ctrl"`, etc. as a single DoubleTapModifier step.
        if raw_parts.len() == 2
            && let Some(first_tap) = ModifierTap::parse_token(raw_parts[0], platform)
            && let Some(second_tap) = ModifierTap::parse_token(raw_parts[1], platform)
            && first_tap == second_tap
        {
            return Ok(Self::double_tap(first_tap));
        }

        if raw_parts.len() > MAX_CHORD_STEPS {
            return Err(invalid_chord(
                input,
                "chord cannot exceed 4 whitespace-separated steps",
            ));
        }

        let mut steps = SmallVec::with_capacity(raw_parts.len());
        for part in raw_parts {
            let step = parse_chord_step(part, platform, input)?;
            steps.push(step);
        }

        Ok(Self { steps })
    }

    /// Returns the ordered steps of this chord.
    #[must_use]
    pub fn steps(&self) -> &[KeyStep] {
        &self.steps
    }

    /// Returns the number of steps in this chord.
    #[must_use]
    pub fn len(&self) -> usize {
        self.steps.len()
    }

    /// Returns `true` if the chord has zero steps (unreachable for valid chords).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
}

impl fmt::Display for KeyChord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (idx, step) in self.steps.iter().enumerate() {
            if idx > 0 {
                f.write_str(" ")?;
            }
            write!(f, "{step}")?;
        }
        Ok(())
    }
}

fn parse_chord_step(
    part: &str,
    platform: Platform,
    full_input: &str,
) -> Result<KeyStep, KeymapParseError> {
    let tokens = split_step_tokens(part, full_input)?;

    if tokens.len() == 2 {
        if tokens[0].eq_ignore_ascii_case("double")
            && let Some(tap) = ModifierTap::parse_token(tokens[1], platform)
        {
            return Ok(KeyStep::DoubleTapModifier(tap));
        }
        if let Some(first_tap) = ModifierTap::parse_token(tokens[0], platform)
            && let Some(second_tap) = ModifierTap::parse_token(tokens[1], platform)
            && first_tap == second_tap
        {
            return Ok(KeyStep::DoubleTapModifier(first_tap));
        }
    }

    let mut modifiers = Modifiers::NONE;
    let mut base_key: Option<KeyCode> = None;

    for token in tokens {
        if let Some(tap) = ModifierTap::parse_token(token, platform) {
            if base_key.is_some() {
                return Err(invalid_chord(
                    full_input,
                    format!("modifier `{token}` must precede the base key"),
                ));
            }
            let duplicate = match tap {
                ModifierTap::Ctrl => {
                    let prev = modifiers.ctrl;
                    modifiers.ctrl = true;
                    prev
                }
                ModifierTap::Alt => {
                    let prev = modifiers.alt;
                    modifiers.alt = true;
                    prev
                }
                ModifierTap::Shift => {
                    let prev = modifiers.shift;
                    modifiers.shift = true;
                    prev
                }
                ModifierTap::Meta => {
                    let prev = modifiers.meta;
                    modifiers.meta = true;
                    prev
                }
            };
            if duplicate {
                return Err(invalid_chord(
                    full_input,
                    format!("duplicate modifier `{token}` in key step `{part}`"),
                ));
            }
            continue;
        }

        if base_key.is_some() {
            return Err(invalid_chord(
                full_input,
                format!("multiple base keys in single step `{part}`"),
            ));
        }

        let Some(key) = KeyCode::parse_token(token) else {
            return Err(invalid_chord(
                full_input,
                format!("unrecognized key token `{token}`"),
            ));
        };
        base_key = Some(key);
    }

    let Some(key) = base_key else {
        return Err(invalid_chord(
            full_input,
            format!("key step `{part}` is missing a base key"),
        ));
    };

    Ok(KeyStep::Stroke(KeyStroke::new(modifiers, key)))
}

fn split_step_tokens<'a>(
    part: &'a str,
    full_input: &str,
) -> Result<Vec<&'a str>, KeymapParseError> {
    if part == "+" {
        return Ok(vec!["+"]);
    }

    let (prefix, trailing_plus) = if let Some(rest) = part.strip_suffix("++") {
        (rest, true)
    } else {
        (part, false)
    };

    let mut tokens = Vec::new();
    if !prefix.is_empty() {
        for tok in prefix.split('+') {
            if tok.is_empty() {
                return Err(invalid_chord(
                    full_input,
                    format!("empty token around '+' in `{part}`"),
                ));
            }
            tokens.push(tok);
        }
    }
    if trailing_plus {
        tokens.push("+");
    }

    if tokens.is_empty() {
        return Err(invalid_chord(full_input, "empty key step"));
    }

    Ok(tokens)
}

fn invalid_chord(input: &str, reason: impl Into<String>) -> KeymapParseError {
    KeymapParseError::InvalidKeyChord {
        binding_index: None,
        key: input.to_owned(),
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_primary_modifier_per_platform() {
        let mac = KeyChord::parse("primary+shift+O", Platform::MacOs).expect("valid mac chord");
        assert_eq!(
            mac.steps(),
            &[KeyStep::Stroke(KeyStroke::new(
                Modifiers {
                    ctrl: false,
                    alt: false,
                    shift: true,
                    meta: true,
                },
                KeyCode::Char('o')
            ))]
        );
        assert_eq!(mac.to_string(), "shift+cmd+o");

        let linux = KeyChord::parse("primary+shift+O", Platform::Linux).expect("valid linux chord");
        assert_eq!(
            linux.steps(),
            &[KeyStep::Stroke(KeyStroke::new(
                Modifiers {
                    ctrl: true,
                    alt: false,
                    shift: true,
                    meta: false,
                },
                KeyCode::Char('o')
            ))]
        );
        assert_eq!(linux.to_string(), "ctrl+shift+o");
    }

    #[test]
    fn parses_multi_step_chords_and_double_tap_aliases() {
        let chord = KeyChord::parse("cmd+k cmd+c", Platform::MacOs).expect("valid 2-step chord");
        assert_eq!(chord.len(), 2);
        assert_eq!(chord.to_string(), "cmd+k cmd+c");

        let space_double = KeyChord::parse("shift shift", Platform::MacOs).expect("shift shift");
        let plus_double = KeyChord::parse("shift+shift", Platform::Linux).expect("shift+shift");
        let explicit_double =
            KeyChord::parse("double+shift", Platform::MacOs).expect("double+shift");
        assert_eq!(space_double, KeyChord::double_tap(ModifierTap::Shift));
        assert_eq!(space_double, plus_double);
        assert_eq!(space_double, explicit_double);
        assert_eq!(space_double.to_string(), "shift shift");
    }

    #[test]
    fn round_trips_special_keys_and_plus_punctuation() {
        for raw in [
            "ctrl+alt+shift+cmd+o",
            "cmd++",
            "cmd+plus",
            "cmd+/",
            "cmd+[",
            "cmd+,",
            "alt+enter",
            "shift+f6",
            "f24",
            "escape",
            "ctrl+space",
        ] {
            let parsed = KeyChord::parse(raw, Platform::MacOs).expect("valid chord");
            let formatted = parsed.to_string();
            let reparsed = KeyChord::parse(&formatted, Platform::MacOs).expect("round-trip parse");
            assert_eq!(parsed, reparsed, "failed round-trip for `{raw}`");
        }
    }

    #[test]
    fn rejects_invalid_chords() {
        for bad in [
            "",
            "   ",
            "shift",
            "ctrl+alt",
            "cmd+",
            "+a",
            "cmd++a",
            "cmd+unknownkey",
            "f25",
            "a b c d e",
            "shift+shift+a",
            "a+cmd",
        ] {
            assert!(
                KeyChord::parse(bad, Platform::MacOs).is_err(),
                "expected `{bad}` to fail parsing"
            );
        }
    }
}
