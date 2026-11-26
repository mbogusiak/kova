//! Headless streaming parser for compiler diagnostics (`javac`, `kotlinc`, `groovyc`,
//! Maven, `rustc` plain fallback, and `cargo --message-format=json`).

use std::path::PathBuf;

use serde_json::Value;

use crate::junit::model::OutputStream;

/// Severity level of a parsed compiler diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiagnosticLevel {
    /// Compilation error.
    Error,
    /// Compiler warning.
    Warning,
}

/// Structured compiler diagnostic parsed from build output (1-based line and column).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCompilerDiagnostic {
    /// File path emitted by the compiler (may be relative to `project_root` or absolute).
    pub file: PathBuf,
    /// 1-based line number.
    pub line: usize,
    /// 1-based column number.
    pub col: usize,
    /// Severity level.
    pub severity: DiagnosticLevel,
    /// Diagnostic message.
    pub message: String,
}

/// Result of decoding one `stdout` line from `cargo build --message-format=json`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CargoStdoutLine {
    /// Formatted console lines to display in the *Run* panel.
    pub console_lines: Vec<(OutputStream, String)>,
    /// Extracted compiler diagnostics.
    pub diagnostics: Vec<ParsedCompilerDiagnostic>,
}

/// Parses one line emitted on `stdout` by `cargo build --message-format=json`.
///
/// Compiler messages (`reason == "compiler-message"`) are rendered cleanly via their
/// `message.rendered` field and converted into [`ParsedCompilerDiagnostic`]s. Other JSON
/// artifact/status lines are suppressed because Cargo emits human-readable progress on `stderr`.
/// Non-JSON lines are passed through as plain `Stdout` console lines.
#[must_use]
pub fn parse_cargo_json_line(line: &str) -> CargoStdoutLine {
    let trimmed = line.trim();
    if !trimmed.starts_with('{') {
        return CargoStdoutLine {
            console_lines: vec![(OutputStream::Stdout, line.to_owned())],
            diagnostics: Vec::new(),
        };
    }

    let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
        return CargoStdoutLine {
            console_lines: vec![(OutputStream::Stdout, line.to_owned())],
            diagnostics: Vec::new(),
        };
    };

    let Some(reason) = value.get("reason").and_then(Value::as_str) else {
        return CargoStdoutLine::default();
    };
    if reason != "compiler-message" {
        return CargoStdoutLine::default();
    }

    let Some(msg) = value.get("message") else {
        return CargoStdoutLine::default();
    };

    let level_str = msg.get("level").and_then(Value::as_str).unwrap_or("");
    let severity = match level_str {
        "error" | "error: internal compiler error" => Some(DiagnosticLevel::Error),
        "warning" => Some(DiagnosticLevel::Warning),
        _ => None,
    };

    let stream = if severity == Some(DiagnosticLevel::Error) {
        OutputStream::Stderr
    } else {
        OutputStream::Stdout
    };

    let mut console_lines = Vec::new();
    if let Some(rendered) = msg.get("rendered").and_then(Value::as_str) {
        for text_line in rendered.lines() {
            console_lines.push((stream, text_line.to_owned()));
        }
    } else if let Some(text) = msg.get("message").and_then(Value::as_str) {
        console_lines.push((stream, text.to_owned()));
    }

    let mut diagnostics = Vec::new();
    if let Some(severity) = severity {
        diagnostics = extract_cargo_message_diagnostics(msg, severity);
    }

    CargoStdoutLine {
        console_lines,
        diagnostics,
    }
}

fn extract_cargo_message_diagnostics(
    msg: &Value,
    severity: DiagnosticLevel,
) -> Vec<ParsedCompilerDiagnostic> {
    let mut diagnostics = Vec::new();
    let base_message = msg
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("compiler diagnostic")
        .trim()
        .to_owned();

    let Some(spans) = msg.get("spans").and_then(Value::as_array) else {
        return diagnostics;
    };
    let primary_spans: Vec<&Value> = spans
        .iter()
        .filter(|s| s.get("is_primary").and_then(Value::as_bool) == Some(true))
        .collect();
    let chosen: Vec<&Value> = if primary_spans.is_empty() {
        spans.iter().take(1).collect()
    } else {
        primary_spans
    };

    for span in chosen {
        let Some(file_name) = span.get("file_name").and_then(Value::as_str) else {
            continue;
        };
        if file_name.is_empty() {
            continue;
        }
        let line_num = span
            .get("line_start")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .filter(|&v| v > 0)
            .unwrap_or(1);
        let col_num = span
            .get("column_start")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .filter(|&v| v > 0)
            .unwrap_or(1);
        let label = span
            .get("label")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let message = match label {
            Some(lbl) if !base_message.contains(lbl) => format!("{base_message}: {lbl}"),
            _ => base_message.clone(),
        };
        diagnostics.push(ParsedCompilerDiagnostic {
            file: PathBuf::from(file_name),
            line: line_num,
            col: col_num,
            severity,
            message,
        });
    }
    diagnostics
}

/// Stateful per-line parser for compiler output from `javac`, `kotlinc`, `groovyc`, Maven,
/// and plain `rustc` text fallback.
#[derive(Debug, Default)]
pub struct CompilerOutputParser {
    pending_rustc: Option<(DiagnosticLevel, String)>,
}

impl CompilerOutputParser {
    /// Creates an empty parser.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Parses one output line and returns zero or more diagnostics.
    pub fn parse_line(&mut self, raw_line: &str) -> Vec<ParsedCompilerDiagnostic> {
        let clean = strip_ansi(raw_line);
        let line = clean.trim_end_matches(['\r', '\n']);
        let trimmed = line.trim();

        if let Some((severity, msg)) = self.pending_rustc.take() {
            if let Some(arrow_rest) = trimmed.strip_prefix("--> ")
                && let Some((file, line_num, col_num)) =
                    parse_path_line_col_suffix(arrow_rest.trim())
            {
                return vec![ParsedCompilerDiagnostic {
                    file: PathBuf::from(file),
                    line: line_num,
                    col: col_num,
                    severity,
                    message: msg,
                }];
            }
            if trimmed.starts_with('|') || trimmed.is_empty() {
                self.pending_rustc = Some((severity, msg));
                return Vec::new();
            }
        }

        if let Some(rustc_header) = parse_rustc_header(trimmed) {
            self.pending_rustc = Some(rustc_header);
            return Vec::new();
        }

        if let Some(diag) = parse_maven_line(trimmed) {
            return vec![diag];
        }
        if let Some(diag) = parse_kotlinc_line(trimmed) {
            return vec![diag];
        }
        if let Some(diag) = parse_javac_line(trimmed) {
            return vec![diag];
        }
        if let Some(diag) = parse_groovyc_line(trimmed) {
            return vec![diag];
        }

        Vec::new()
    }
}

fn parse_rustc_header(trimmed: &str) -> Option<(DiagnosticLevel, String)> {
    if let Some(rest) = trimmed.strip_prefix("error") {
        if rest.starts_with('[')
            && let Some((_, msg)) = rest.split_once("]:")
        {
            return Some((DiagnosticLevel::Error, msg.trim().to_owned()));
        }
        if let Some(msg) = rest.strip_prefix(':') {
            let msg = msg.trim();
            if !msg.starts_with("could not compile") && !msg.starts_with("aborting due to") {
                return Some((DiagnosticLevel::Error, msg.to_owned()));
            }
        }
    } else if let Some(rest) = trimmed.strip_prefix("warning:") {
        let msg = rest.trim();
        if !msg.is_empty() && !msg.contains("generated") && !msg.contains("emitted") {
            return Some((DiagnosticLevel::Warning, msg.to_owned()));
        }
    }
    None
}

fn parse_maven_line(trimmed: &str) -> Option<ParsedCompilerDiagnostic> {
    let (severity, rest) = if let Some(r) = trimmed.strip_prefix("[ERROR]") {
        (DiagnosticLevel::Error, r.trim_start())
    } else {
        let r = trimmed.strip_prefix("[WARNING]")?;
        (DiagnosticLevel::Warning, r.trim_start())
    };

    let open = rest.find(":[")?;
    let path_part = rest[..open].trim();
    if path_part.is_empty() {
        return None;
    }
    let after_open = &rest[open + 2..];
    let close = after_open.find(']')?;
    let coords = &after_open[..close];
    let message = after_open[close + 1..].trim();
    if message.is_empty() {
        return None;
    }

    let (line, col) = if let Some((l, c)) = coords.split_once(',') {
        (
            l.trim().parse::<usize>().ok().filter(|&v| v > 0)?,
            c.trim()
                .parse::<usize>()
                .ok()
                .filter(|&v| v > 0)
                .unwrap_or(1),
        )
    } else {
        (coords.trim().parse::<usize>().ok().filter(|&v| v > 0)?, 1)
    };

    Some(ParsedCompilerDiagnostic {
        file: PathBuf::from(strip_file_scheme(path_part)),
        line,
        col,
        severity,
        message: message.to_owned(),
    })
}

fn parse_kotlinc_line(trimmed: &str) -> Option<ParsedCompilerDiagnostic> {
    let (severity, rest) = if let Some(r) = trimmed.strip_prefix("e:") {
        (DiagnosticLevel::Error, r.trim_start())
    } else {
        let r = trimmed.strip_prefix("w:")?;
        (DiagnosticLevel::Warning, r.trim_start())
    };

    // Old format: `<path>: (<line>, <col>): <message>`
    if let Some((loc_part, msg)) = rest.split_once("): ")
        && let Some((path_raw, coords)) = loc_part.rsplit_once(": (")
        && let Some((l_str, c_str)) = coords.split_once(',')
        && let (Ok(line), Ok(col)) = (l_str.trim().parse::<usize>(), c_str.trim().parse::<usize>())
    {
        return Some(ParsedCompilerDiagnostic {
            file: PathBuf::from(strip_file_scheme(path_raw.trim())),
            line: line.max(1),
            col: col.max(1),
            severity,
            message: msg.trim().to_owned(),
        });
    }

    // New format: `[file://]<path>:<line>:<col> <message>`
    let (loc_token, msg) = rest.split_once(' ')?;
    let (path, line, col) = parse_path_line_col_suffix(loc_token)?;
    let message = msg.trim();
    if message.is_empty() {
        return None;
    }
    Some(ParsedCompilerDiagnostic {
        file: PathBuf::from(strip_file_scheme(path)),
        line,
        col,
        severity,
        message: message.to_owned(),
    })
}

fn parse_javac_line(trimmed: &str) -> Option<ParsedCompilerDiagnostic> {
    let (loc_part, severity, message) = if let Some((loc, msg)) = trimmed.split_once(": error:") {
        (loc, DiagnosticLevel::Error, msg.trim())
    } else if let Some((loc, msg)) = trimmed.split_once(": warning:") {
        (loc, DiagnosticLevel::Warning, msg.trim())
    } else {
        return None;
    };
    if message.is_empty() {
        return None;
    }

    let (file, line, col) = parse_path_line_opt_col_suffix(loc_part)?;
    Some(ParsedCompilerDiagnostic {
        file: PathBuf::from(strip_file_scheme(file)),
        line,
        col,
        severity,
        message: message.to_owned(),
    })
}

fn parse_groovyc_line(trimmed: &str) -> Option<ParsedCompilerDiagnostic> {
    // `<path>: <line>: <message> [@ line <L>, column <C>.]`
    let (path_raw, rest) = split_path_before_space_number(trimmed)?;
    let (line_raw, msg_raw) = rest.split_once(": ")?;
    let mut line = line_raw.trim().parse::<usize>().ok().filter(|&v| v > 0)?;
    let mut col = 1usize;
    let mut message = msg_raw.trim();

    if let Some((head, tail)) = message.rsplit_once("@ line ")
        && let Some((l_str, c_rest)) = tail.split_once(", column ")
        && let Ok(parsed_l) = l_str.trim().parse::<usize>()
        && let Ok(parsed_c) = c_rest.trim_end_matches('.').trim().parse::<usize>()
    {
        line = parsed_l.max(1);
        col = parsed_c.max(1);
        message = head.trim();
    }

    if message.is_empty() {
        return None;
    }

    Some(ParsedCompilerDiagnostic {
        file: PathBuf::from(strip_file_scheme(path_raw)),
        line,
        col,
        severity: DiagnosticLevel::Error,
        message: message.to_owned(),
    })
}

fn split_path_before_space_number(s: &str) -> Option<(&str, &str)> {
    // Find `": "` followed by digits and `": "`, preserving Windows `C:\...` colons.
    let bytes = s.as_bytes();
    let mut idx = 0;
    while let Some(pos) = s[idx..].find(": ") {
        let colon = idx + pos;
        let after = &s[colon + 2..];
        let digit_count = after.bytes().take_while(u8::is_ascii_digit).count();
        if digit_count > 0 && after[digit_count..].starts_with(": ") && colon > 0 {
            let path = &s[..colon];
            // Quick sanity: path looks like a source file or path rather than a plain label.
            if path.contains('.') || path.contains('/') || path.contains('\\') {
                return Some((path, after));
            }
        }
        idx = colon + 2;
        if idx >= bytes.len() {
            break;
        }
    }
    None
}

fn parse_path_line_col_suffix(s: &str) -> Option<(&str, usize, usize)> {
    let (prefix, col_str) = s.rsplit_once(':')?;
    let col = col_str.trim().parse::<usize>().ok().filter(|&v| v > 0)?;
    let (path, line_str) = prefix.rsplit_once(':')?;
    let line = line_str.trim().parse::<usize>().ok().filter(|&v| v > 0)?;
    if path.is_empty() {
        return None;
    }
    Some((path, line, col))
}

fn parse_path_line_opt_col_suffix(s: &str) -> Option<(&str, usize, usize)> {
    let (prefix, last) = s.rsplit_once(':')?;
    let last_num = last.trim().parse::<usize>().ok().filter(|&v| v > 0)?;
    if let Some((path, prev)) = prefix.rsplit_once(':')
        && let Ok(line) = prev.trim().parse::<usize>()
        && line > 0
        && !path.is_empty()
    {
        return Some((path, line, last_num));
    }
    if prefix.is_empty() {
        return None;
    }
    Some((prefix, last_num, 1))
}

fn strip_file_scheme(path: &str) -> &str {
    if let Some(rest) = path.strip_prefix("file://") {
        // Handle Windows `file:///C:/...`
        let bytes = rest.as_bytes();
        if bytes.len() >= 3
            && bytes[0] == b'/'
            && bytes[1].is_ascii_alphabetic()
            && bytes[2] == b':'
        {
            return &rest[1..];
        }
        return rest;
    }
    path
}

fn strip_ansi(input: &str) -> String {
    if !input.contains('\x1b') {
        return input.to_owned();
    }
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "unit tests assert compiler diagnostic parsing"
)]
mod tests {
    use super::*;

    #[test]
    fn parses_javac_error_and_warning_with_optional_column_and_spaces() {
        let mut parser = CompilerOutputParser::new();
        let err = parser.parse_line("/Users/dev/My App/src/Main.java:14:22: error: ';' expected");
        assert_eq!(
            err,
            vec![ParsedCompilerDiagnostic {
                file: PathBuf::from("/Users/dev/My App/src/Main.java"),
                line: 14,
                col: 22,
                severity: DiagnosticLevel::Error,
                message: "';' expected".to_owned(),
            }]
        );

        let warn = parser.parse_line("src/App.java:8: warning: [deprecation] old() is deprecated");
        assert_eq!(
            warn,
            vec![ParsedCompilerDiagnostic {
                file: PathBuf::from("src/App.java"),
                line: 8,
                col: 1,
                severity: DiagnosticLevel::Warning,
                message: "[deprecation] old() is deprecated".to_owned(),
            }]
        );
    }

    #[test]
    fn parses_kotlinc_new_and_legacy_formats() {
        let mut parser = CompilerOutputParser::new();
        let new_fmt =
            parser.parse_line("e: file:///work/app/src/Main.kt:12:5 Unresolved reference: foo");
        assert_eq!(
            new_fmt,
            vec![ParsedCompilerDiagnostic {
                file: PathBuf::from("/work/app/src/Main.kt"),
                line: 12,
                col: 5,
                severity: DiagnosticLevel::Error,
                message: "Unresolved reference: foo".to_owned(),
            }]
        );

        let legacy_fmt = parser.parse_line("w: /work/app/src/Util.kt: (7, 9): Variable is unused");
        assert_eq!(
            legacy_fmt,
            vec![ParsedCompilerDiagnostic {
                file: PathBuf::from("/work/app/src/Util.kt"),
                line: 7,
                col: 9,
                severity: DiagnosticLevel::Warning,
                message: "Variable is unused".to_owned(),
            }]
        );
    }

    #[test]
    fn parses_groovyc_and_maven_lines() {
        let mut parser = CompilerOutputParser::new();
        let groovy = parser.parse_line(
            "/work/src/test/groovy/OrderSpec.groovy: 18: unable to resolve class MissingType @ line 18, column 9.",
        );
        assert_eq!(
            groovy,
            vec![ParsedCompilerDiagnostic {
                file: PathBuf::from("/work/src/test/groovy/OrderSpec.groovy"),
                line: 18,
                col: 9,
                severity: DiagnosticLevel::Error,
                message: "unable to resolve class MissingType".to_owned(),
            }]
        );

        let maven =
            parser.parse_line("[ERROR] /work/src/main/java/App.java:[23,14] cannot find symbol");
        assert_eq!(
            maven,
            vec![ParsedCompilerDiagnostic {
                file: PathBuf::from("/work/src/main/java/App.java"),
                line: 23,
                col: 14,
                severity: DiagnosticLevel::Error,
                message: "cannot find symbol".to_owned(),
            }]
        );
    }

    #[test]
    fn parses_cargo_json_and_rustc_plain_fallback() {
        let json_line = r#"{"reason":"compiler-message","package_id":"app 0.1.0","message":{"rendered":"error[E0425]: cannot find value `x`\n --> src/main.rs:4:9\n","message":"cannot find value `x`","level":"error","spans":[{"file_name":"src/main.rs","line_start":4,"column_start":9,"is_primary":true,"label":"not found in this scope"}]}}"#;
        let parsed = parse_cargo_json_line(json_line);
        assert_eq!(
            parsed.diagnostics,
            vec![ParsedCompilerDiagnostic {
                file: PathBuf::from("src/main.rs"),
                line: 4,
                col: 9,
                severity: DiagnosticLevel::Error,
                message: "cannot find value `x`: not found in this scope".to_owned(),
            }]
        );
        assert_eq!(parsed.console_lines.len(), 2);
        assert_eq!(parsed.console_lines[0].0, OutputStream::Stderr);

        // Non-compiler artifact JSON lines are suppressed from console.
        let artifact = r#"{"reason":"compiler-artifact","package_id":"app 0.1.0"}"#;
        let artifact_parsed = parse_cargo_json_line(artifact);
        assert_eq!(artifact_parsed.console_lines, []);
        assert_eq!(artifact_parsed.diagnostics, []);

        // Plain rustc fallback (e.g. during `cargo test` stderr).
        let mut parser = CompilerOutputParser::new();
        assert_eq!(parser.parse_line("error[E0308]: mismatched types"), []);
        let diags = parser.parse_line("  --> src/lib.rs:10:15");
        assert_eq!(
            diags,
            vec![ParsedCompilerDiagnostic {
                file: PathBuf::from("src/lib.rs"),
                line: 10,
                col: 15,
                severity: DiagnosticLevel::Error,
                message: "mismatched types".to_owned(),
            }]
        );
    }
}
