//! Line-ending representation, detection, normalization, and `RopeSlice` line helpers.

use std::borrow::Cow;

use ropey::{Rope, RopeSlice};

/// Maximum number of bytes inspected when auto-detecting line endings.
const DETECT_SAMPLE_BYTES: usize = 64 * 1024;

/// Line ending style associated with a text buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum LineEnding {
    /// Unix line feed (`\n`).
    #[default]
    Lf,
    /// Windows carriage return + line feed (`\r\n`).
    CrLf,
}

impl LineEnding {
    /// Returns the string representation of this line ending (`"\n"` or `"\r\n"`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::CrLf => "\r\n",
        }
    }

    /// Returns the byte length of this line ending (`1` for LF, `2` for CRLF).
    #[must_use]
    pub const fn len_bytes(self) -> usize {
        match self {
            Self::Lf => 1,
            Self::CrLf => 2,
        }
    }

    /// Detects the dominant line ending in `text` (sampling up to 64 KiB).
    ///
    /// Returns [`LineEnding::CrLf`] when `\r\n` occurrences strictly outnumber
    /// standalone `\n` occurrences; otherwise defaults to [`LineEnding::Lf`].
    #[must_use]
    pub fn detect(text: &str) -> Self {
        let mut crlf_count: usize = 0;
        let mut lf_count: usize = 0;
        let mut prev_cr = false;

        for &byte in text.as_bytes().iter().take(DETECT_SAMPLE_BYTES) {
            if byte == b'\n' {
                if prev_cr {
                    crlf_count += 1;
                } else {
                    lf_count += 1;
                }
                prev_cr = false;
            } else {
                prev_cr = byte == b'\r';
            }
        }

        if crlf_count > lf_count {
            Self::CrLf
        } else {
            Self::Lf
        }
    }

    /// Detects the dominant line ending in a [`Rope`] without allocating a contiguous string.
    #[must_use]
    pub fn detect_rope(rope: &Rope) -> Self {
        let mut crlf_count: usize = 0;
        let mut lf_count: usize = 0;
        let mut prev_cr = false;
        let mut scanned: usize = 0;

        for chunk in rope.chunks() {
            for &byte in chunk.as_bytes() {
                if scanned >= DETECT_SAMPLE_BYTES {
                    return if crlf_count > lf_count {
                        Self::CrLf
                    } else {
                        Self::Lf
                    };
                }
                scanned += 1;
                if byte == b'\n' {
                    if prev_cr {
                        crlf_count += 1;
                    } else {
                        lf_count += 1;
                    }
                    prev_cr = false;
                } else {
                    prev_cr = byte == b'\r';
                }
            }
        }

        if crlf_count > lf_count {
            Self::CrLf
        } else {
            Self::Lf
        }
    }

    /// Normalizes `\r\n` and `\n` line endings in `input` to `target`.
    ///
    /// Borrows `input` without allocation when it already uses `target` exclusively.
    #[must_use]
    pub fn normalize_text(input: &str, target: Self) -> Cow<'_, str> {
        let bytes = input.as_bytes();
        let needs_normalization = match target {
            Self::Lf => bytes.windows(2).any(|w| w == b"\r\n"),
            Self::CrLf => {
                let mut prev_cr = false;
                let mut found_bare_lf = false;
                for &b in bytes {
                    if b == b'\n' && !prev_cr {
                        found_bare_lf = true;
                        break;
                    }
                    prev_cr = b == b'\r';
                }
                found_bare_lf
            }
        };

        if !needs_normalization {
            return Cow::Borrowed(input);
        }

        let mut out = String::with_capacity(input.len());
        let mut iter = input.char_indices().peekable();
        while let Some((idx, ch)) = iter.next() {
            if ch == '\r' {
                if let Some(&(_, '\n')) = iter.peek() {
                    let _ = iter.next();
                    out.push_str(target.as_str());
                } else {
                    out.push('\r');
                }
            } else if ch == '\n' {
                out.push_str(target.as_str());
            } else {
                let end = idx + ch.len_utf8();
                out.push_str(&input[idx..end]);
            }
        }
        Cow::Owned(out)
    }
}

/// Returns the length in bytes of the trailing line separator (`2` for `\r\n`, `1` for `\n`, `0` otherwise).
#[must_use]
pub(crate) fn line_ending_len_bytes(line: RopeSlice<'_>) -> usize {
    let len = line.len_bytes();
    if len == 0 {
        return 0;
    }
    if line.get_byte(len - 1) == Some(b'\n') {
        if len >= 2 && line.get_byte(len - 2) == Some(b'\r') {
            return 2;
        }
        return 1;
    }
    0
}

/// Returns the byte length of `line` excluding any trailing `\n` or `\r\n`.
#[must_use]
pub(crate) fn line_content_len_bytes(line: RopeSlice<'_>) -> usize {
    line.len_bytes() - line_ending_len_bytes(line)
}

/// Returns the UTF-16 code-unit length of `line` excluding any trailing `\n` or `\r\n`.
#[must_use]
pub(crate) fn line_content_len_utf16(line: RopeSlice<'_>) -> usize {
    line.len_utf16_cu() - line_ending_len_bytes(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_lf_and_crlf() {
        assert_eq!(LineEnding::detect(""), LineEnding::Lf);
        assert_eq!(LineEnding::detect("single line"), LineEnding::Lf);
        assert_eq!(LineEnding::detect("a\nb\nc\n"), LineEnding::Lf);
        assert_eq!(LineEnding::detect("a\r\nb\r\n"), LineEnding::CrLf);
        assert_eq!(LineEnding::detect("a\r\nb\nc\r\n"), LineEnding::CrLf);
        assert_eq!(LineEnding::detect("a\nb\nc\r\n"), LineEnding::Lf);
    }

    #[test]
    fn detects_crlf_across_rope_chunks() {
        let mut s = "x".repeat(4095);
        s.push_str("\r\ny\r\n");
        let rope = Rope::from_str(&s);
        assert_eq!(LineEnding::detect_rope(&rope), LineEnding::CrLf);
    }

    #[test]
    fn normalizes_line_endings_without_unnecessary_allocation() {
        let already_lf = LineEnding::normalize_text("a\nb\n", LineEnding::Lf);
        assert!(matches!(already_lf, Cow::Borrowed(_)));
        assert_eq!(already_lf, "a\nb\n");

        let crlf_to_lf = LineEnding::normalize_text("a\r\nb\r\nc", LineEnding::Lf);
        assert_eq!(crlf_to_lf, "a\nb\nc");

        let already_crlf = LineEnding::normalize_text("a\r\nb\r\n", LineEnding::CrLf);
        assert!(matches!(already_crlf, Cow::Borrowed(_)));

        let mixed_to_crlf = LineEnding::normalize_text("a\nb\r\nc\n", LineEnding::CrLf);
        assert_eq!(mixed_to_crlf, "a\r\nb\r\nc\r\n");
    }
}
