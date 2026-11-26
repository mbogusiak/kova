//! Keystroke, paste and mouse-wheel encoding to the byte sequences an xterm sends.

use alacritty_terminal::term::TermMode;

/// Modifier keys held with a keystroke.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeyModifiers {
    /// Control.
    pub ctrl: bool,
    /// Alt / Option (sent as an `ESC` prefix, "meta sends escape").
    pub alt: bool,
    /// Shift.
    pub shift: bool,
}

impl KeyModifiers {
    /// xterm modifier parameter (`1 + shift + 2·alt + 4·ctrl`).
    const fn xterm_param(self) -> u8 {
        1 + (self.shift as u8) + 2 * (self.alt as u8) + 4 * (self.ctrl as u8)
    }

    const fn any(self) -> bool {
        self.ctrl || self.alt || self.shift
    }
}

/// Encodes one keystroke for the program running in the terminal.
///
/// `key` is a GPUI-style key name (`"enter"`, `"up"`, `"f5"`, `"a"`); `text` is the character
/// the key produces with the current layout, if any. Returns `None` for keys the terminal does
/// not forward (bare modifiers, unknown names).
#[must_use]
pub fn encode_key(
    key: &str,
    text: Option<&str>,
    mods: KeyModifiers,
    term_mode: TermMode,
) -> Option<Vec<u8>> {
    let app_cursor = term_mode.contains(TermMode::APP_CURSOR);
    let bytes: Vec<u8> = match key {
        "enter" => with_alt(mods, b"\r"),
        "tab" if mods.shift => b"\x1b[Z".to_vec(),
        "tab" => with_alt(mods, b"\t"),
        "backspace" if mods.ctrl => with_alt(mods, b"\x08"),
        "backspace" => with_alt(mods, b"\x7f"),
        "escape" => with_alt(mods, b"\x1b"),
        "space" if mods.ctrl => with_alt(mods, b"\x00"),
        "up" => cursor_key(b'A', mods, app_cursor),
        "down" => cursor_key(b'B', mods, app_cursor),
        "right" => cursor_key(b'C', mods, app_cursor),
        "left" => cursor_key(b'D', mods, app_cursor),
        "home" => cursor_key(b'H', mods, app_cursor),
        "end" => cursor_key(b'F', mods, app_cursor),
        "insert" => tilde_key(2, mods),
        "delete" => tilde_key(3, mods),
        "pageup" => tilde_key(5, mods),
        "pagedown" => tilde_key(6, mods),
        "f1" => ss3_key(b'P', mods),
        "f2" => ss3_key(b'Q', mods),
        "f3" => ss3_key(b'R', mods),
        "f4" => ss3_key(b'S', mods),
        "f5" => tilde_key(15, mods),
        "f6" => tilde_key(17, mods),
        "f7" => tilde_key(18, mods),
        "f8" => tilde_key(19, mods),
        "f9" => tilde_key(20, mods),
        "f10" => tilde_key(21, mods),
        "f11" => tilde_key(23, mods),
        "f12" => tilde_key(24, mods),
        _ => return encode_char_key(key, text, mods),
    };
    Some(bytes)
}

fn encode_char_key(key: &str, text: Option<&str>, mods: KeyModifiers) -> Option<Vec<u8>> {
    if mods.ctrl {
        let mut chars = key.chars();
        if let (Some(ch), None) = (chars.next(), chars.next())
            && let Some(control) = control_byte(ch)
        {
            return Some(with_alt(mods, &[control]));
        }
    }
    let text = match text {
        Some(text) if !text.is_empty() => text,
        _ if key == "space" => " ",
        _ => {
            let mut chars = key.chars();
            match (chars.next(), chars.next()) {
                (Some(_), None) => key,
                _ => return None,
            }
        }
    };
    Some(with_alt(mods, text.as_bytes()))
}

/// `Ctrl+<char>` → C0 control byte.
fn control_byte(ch: char) -> Option<u8> {
    let ch = ch.to_ascii_lowercase();
    match ch {
        'a'..='z' => Some(ch as u8 - b'a' + 1),
        '@' | '2' => Some(0x00),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '-' | '7' => Some(0x1f),
        '?' | '8' => Some(0x7f),
        _ => None,
    }
}

fn with_alt(mods: KeyModifiers, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 1);
    if mods.alt {
        out.push(0x1b);
    }
    out.extend_from_slice(bytes);
    out
}

fn cursor_key(code: u8, mods: KeyModifiers, app_cursor: bool) -> Vec<u8> {
    if mods.any() {
        format!("\x1b[1;{}{}", mods.xterm_param(), code as char).into_bytes()
    } else if app_cursor {
        vec![0x1b, b'O', code]
    } else {
        vec![0x1b, b'[', code]
    }
}

fn ss3_key(code: u8, mods: KeyModifiers) -> Vec<u8> {
    if mods.any() {
        format!("\x1b[1;{}{}", mods.xterm_param(), code as char).into_bytes()
    } else {
        vec![0x1b, b'O', code]
    }
}

fn tilde_key(number: u8, mods: KeyModifiers) -> Vec<u8> {
    if mods.any() {
        format!("\x1b[{number};{}~", mods.xterm_param()).into_bytes()
    } else {
        format!("\x1b[{number}~").into_bytes()
    }
}

/// Encodes pasted text, wrapping it in bracketed-paste markers when the program asked for them.
#[must_use]
pub fn encode_paste(text: &str, mode: TermMode) -> Vec<u8> {
    if mode.contains(TermMode::BRACKETED_PASTE) {
        // Strip markers from the payload so it cannot terminate the paste early.
        let clean = text.replace("\x1b[201~", "");
        format!("\x1b[200~{clean}\x1b[201~").into_bytes()
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

/// Encodes a mouse wheel movement of `lines` (positive = up) at visible cell `(col, row)`.
///
/// Returns `None` when the wheel should scroll the local history instead: the program has
/// neither enabled mouse reporting nor runs on the alternate screen.
#[must_use]
pub fn encode_scroll(lines: i32, col: u16, row: u16, mode: TermMode) -> Option<Vec<u8>> {
    if lines == 0 {
        return Some(Vec::new());
    }
    let count = lines.unsigned_abs().min(32);
    let up = lines > 0;
    let mut out = Vec::new();
    if mode.intersects(TermMode::MOUSE_MODE) {
        let button = if up { 64 } else { 65 };
        for _ in 0..count {
            if mode.contains(TermMode::SGR_MOUSE) {
                out.extend_from_slice(
                    format!(
                        "\x1b[<{button};{};{}M",
                        u32::from(col) + 1,
                        u32::from(row) + 1
                    )
                    .as_bytes(),
                );
            } else {
                let cx = u8::try_from(u32::from(col) + 33).unwrap_or(u8::MAX);
                let cy = u8::try_from(u32::from(row) + 33).unwrap_or(u8::MAX);
                out.extend_from_slice(&[0x1b, b'[', b'M', 32 + button, cx, cy]);
            }
        }
        return Some(out);
    }
    if mode.contains(TermMode::ALT_SCREEN) && mode.contains(TermMode::ALTERNATE_SCROLL) {
        let app_cursor = mode.contains(TermMode::APP_CURSOR);
        let key = cursor_key(
            if up { b'A' } else { b'B' },
            KeyModifiers::default(),
            app_cursor,
        );
        for _ in 0..count {
            out.extend_from_slice(&key);
        }
        return Some(out);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: KeyModifiers = KeyModifiers {
        ctrl: false,
        alt: false,
        shift: false,
    };
    const CTRL: KeyModifiers = KeyModifiers {
        ctrl: true,
        alt: false,
        shift: false,
    };
    const ALT: KeyModifiers = KeyModifiers {
        ctrl: false,
        alt: true,
        shift: false,
    };

    fn key(name: &str, text: Option<&str>, mods: KeyModifiers) -> Vec<u8> {
        encode_key(name, text, mods, TermMode::default()).unwrap_or_default()
    }

    #[test]
    fn printable_text_and_control_keys() {
        assert_eq!(key("a", Some("a"), NONE), b"a");
        assert_eq!(key("a", Some("A"), NONE), b"A");
        assert_eq!(key("space", None, NONE), b" ");
        assert_eq!(key("enter", None, NONE), b"\r");
        assert_eq!(key("backspace", None, NONE), b"\x7f");
        assert_eq!(key("tab", None, NONE), b"\t");
        assert_eq!(key("escape", None, NONE), b"\x1b");
        assert_eq!(key("c", Some("c"), CTRL), b"\x03");
        assert_eq!(key("d", None, CTRL), b"\x04");
        assert_eq!(key("b", Some("b"), ALT), b"\x1bb");
        assert_eq!(encode_key("shift", None, NONE, TermMode::default()), None);
    }

    #[test]
    fn cursor_keys_follow_application_mode() {
        assert_eq!(key("up", None, NONE), b"\x1b[A");
        let app = TermMode::APP_CURSOR;
        assert_eq!(
            encode_key("up", None, NONE, app).unwrap_or_default(),
            b"\x1bOA"
        );
        assert_eq!(key("left", None, CTRL), b"\x1b[1;5D");
    }

    #[test]
    fn function_and_editing_keys() {
        assert_eq!(key("f1", None, NONE), b"\x1bOP");
        assert_eq!(key("f5", None, NONE), b"\x1b[15~");
        assert_eq!(key("delete", None, NONE), b"\x1b[3~");
        assert_eq!(key("pageup", None, NONE), b"\x1b[5~");
    }

    #[test]
    fn paste_honours_bracketed_mode() {
        assert_eq!(encode_paste("a\nb", TermMode::default()), b"a\rb");
        assert_eq!(
            encode_paste("ls\n", TermMode::BRACKETED_PASTE),
            b"\x1b[200~ls\n\x1b[201~"
        );
    }

    #[test]
    fn wheel_reports_or_scrolls_history() {
        assert_eq!(encode_scroll(3, 0, 0, TermMode::default()), None);
        let sgr = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        assert_eq!(
            encode_scroll(1, 4, 2, sgr).unwrap_or_default(),
            b"\x1b[<64;5;3M"
        );
        let alt = TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL;
        assert_eq!(
            encode_scroll(-2, 0, 0, alt).unwrap_or_default(),
            b"\x1b[B\x1b[B"
        );
    }
}
