//! Minimal, dependency-free command-line parser for the `kova` launcher (`KOVA-016`).
//!
//! Supported forms:
//!
//! ```text
//! kova [OPTIONS] [FILE[:LINE[:COLUMN]] | PROJECT_DIR]
//!   --config-dir <DIR>        override the configuration directory (`KOVA_CONFIG_HOME`)
//!   -h, --help                print help and exit
//!   -V, --version             print version and exit
//!   --probe-startup           exit after the first frame (startup probe)
//!   --assert-startup-budget   fail when Phase 0 startup budgets are exceeded
//! ```
//!
//! The parser performs no allocation-heavy work and at most one `stat` per positional
//! argument (to disambiguate `name:12` file names from `FILE:LINE` suffixes), so it has
//! no measurable effect on the `< 120 ms` cold-start budget.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Flag that exits after the first frame (handled by [`crate::AppLaunchOptions`]).
pub const PROBE_STARTUP_FLAG: &str = "--probe-startup";
/// Flag that enforces startup budgets (handled by [`crate::AppLaunchOptions`]).
pub const ASSERT_BUDGET_FLAG: &str = "--assert-startup-budget";

const CONFIG_DIR_FLAG: &str = "--config-dir";

/// A file requested on the command line with an optional 1-based cursor position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTarget {
    /// File path exactly as given (relative paths resolve against the working directory).
    pub path: PathBuf,
    /// Optional 1-based line number.
    pub line: Option<usize>,
    /// Optional 1-based column number (characters).
    pub column: Option<usize>,
}

impl FileTarget {
    /// Creates a target without a cursor position.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            line: None,
            column: None,
        }
    }
}

/// Parsed launcher command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliCommand {
    /// Open the editor window, optionally with a file and a configuration directory override.
    Open {
        /// File to open (`None` opens an empty *Untitled* buffer).
        target: Option<FileTarget>,
        /// Explicit configuration directory (`--config-dir`).
        config_dir_override: Option<PathBuf>,
    },
    /// Print usage and exit with code `0`.
    PrintHelp,
    /// Print the package version and exit with code `0`.
    PrintVersion,
}

/// Errors produced by [`parse_args`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CliParseError {
    /// An option starting with `-` is not recognized.
    #[error("unknown option `{0}` (see `kova --help`)")]
    UnknownOption(String),
    /// An option that requires a value was given without one.
    #[error("option `{0}` requires a value")]
    MissingValue(&'static str),
    /// More than one file was given (multi-tab opening lands in Phase 1).
    #[error("only one file can be opened in Phase 0 (unexpected `{0}`)")]
    UnexpectedArgument(String),
    /// An argument is not valid Unicode.
    #[error("argument is not valid UTF-8: {0}")]
    NonUtf8Argument(String),
}

/// Parses process arguments (including the program name in position `0`).
///
/// # Errors
///
/// Returns [`CliParseError`] for unknown options, missing option values, more than one
/// positional file, or arguments that are not valid UTF-8.
pub fn parse_args<I, S>(args: I) -> Result<CliCommand, CliParseError>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    parse_args_with(args, Path::exists)
}

/// Same as [`parse_args`] with an injected `exists` predicate (deterministic tests).
///
/// # Errors
///
/// See [`parse_args`].
pub fn parse_args_with<I, S, F>(args: I, exists: F) -> Result<CliCommand, CliParseError>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
    F: Fn(&Path) -> bool,
{
    let mut iter = args.into_iter().map(Into::into).skip(1);
    let mut target: Option<FileTarget> = None;
    let mut config_dir_override = None;
    let mut positional_only = false;

    while let Some(raw) = iter.next() {
        let arg = raw
            .into_string()
            .map_err(|os| CliParseError::NonUtf8Argument(os.to_string_lossy().into_owned()))?;

        if !positional_only && arg.starts_with('-') && arg != "-" {
            match arg.as_str() {
                "--" => positional_only = true,
                "-h" | "--help" => return Ok(CliCommand::PrintHelp),
                "-V" | "--version" => return Ok(CliCommand::PrintVersion),
                PROBE_STARTUP_FLAG | ASSERT_BUDGET_FLAG => {}
                CONFIG_DIR_FLAG => {
                    let value = iter
                        .next()
                        .ok_or(CliParseError::MissingValue(CONFIG_DIR_FLAG))?;
                    config_dir_override = Some(PathBuf::from(value));
                }
                other => match other.strip_prefix("--config-dir=") {
                    Some("") => return Err(CliParseError::MissingValue(CONFIG_DIR_FLAG)),
                    Some(value) => config_dir_override = Some(PathBuf::from(value)),
                    None => return Err(CliParseError::UnknownOption(other.to_owned())),
                },
            }
            continue;
        }

        if target.is_some() {
            return Err(CliParseError::UnexpectedArgument(arg));
        }
        target = Some(parse_file_target(&arg, &exists));
    }

    Ok(CliCommand::Open {
        target,
        config_dir_override,
    })
}

/// Splits `FILE[:LINE[:COLUMN]]`, preferring an existing file whose name contains colons.
#[must_use]
pub fn parse_file_target<F>(arg: &str, exists: F) -> FileTarget
where
    F: Fn(&Path) -> bool,
{
    if exists(Path::new(arg)) {
        return FileTarget::new(arg);
    }

    // `path:LINE:COLUMN`
    if let Some((rest, col)) = split_numeric_suffix(arg)
        && let Some((path, line)) = split_numeric_suffix(rest)
    {
        return FileTarget {
            path: PathBuf::from(path),
            line: Some(line),
            column: Some(col),
        };
    }
    // `path:LINE`
    if let Some((path, line)) = split_numeric_suffix(arg) {
        return FileTarget {
            path: PathBuf::from(path),
            line: Some(line),
            column: None,
        };
    }
    FileTarget::new(arg)
}

fn split_numeric_suffix(arg: &str) -> Option<(&str, usize)> {
    let (head, tail) = arg.rsplit_once(':')?;
    if head.is_empty() || tail.is_empty() || !tail.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    tail.parse().ok().map(|n| (head, n))
}

/// Human-readable usage text printed by `kova --help`.
#[must_use]
pub fn help_text() -> String {
    format!(
        "Kova IDE {version}\n\
         \n\
         Usage: kova [OPTIONS] [FILE[:LINE[:COLUMN]] | PROJECT_DIR]\n\
         \n\
         Arguments:\n\
         \x20 [FILE[:LINE[:COLUMN]]]   File to open; LINE and COLUMN are 1-based\n\
         \x20 [PROJECT_DIR]            Directory to open as the project\n\
         \n\
         Options:\n\
         \x20 --config-dir <DIR>       Configuration directory (default: $KOVA_CONFIG_HOME,\n\
         \x20                          $XDG_CONFIG_HOME/kova or ~/.config/kova)\n\
         \x20 --probe-startup          Exit after the first frame and print startup metrics\n\
         \x20 --assert-startup-budget  Exit with failure when startup budgets are exceeded\n\
         \x20 -h, --help               Print help\n\
         \x20 -V, --version            Print version",
        version = env!("CARGO_PKG_VERSION")
    )
}

/// Version line printed by `kova --version`.
#[must_use]
pub fn version_text() -> String {
    format!("kova {}", env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<CliCommand, CliParseError> {
        let mut full = vec!["kova"];
        full.extend_from_slice(args);
        parse_args_with(full, |_| false)
    }

    fn open(target: Option<FileTarget>, config: Option<&str>) -> CliCommand {
        CliCommand::Open {
            target,
            config_dir_override: config.map(PathBuf::from),
        }
    }

    #[test]
    fn no_arguments_opens_untitled() {
        assert_eq!(parse(&[]), Ok(open(None, None)));
    }

    #[test]
    fn plain_path_and_positions() {
        assert_eq!(
            parse(&["src/Main.java"]),
            Ok(open(Some(FileTarget::new("src/Main.java")), None))
        );
        assert_eq!(
            parse(&["src/Main.java:42"]),
            Ok(open(
                Some(FileTarget {
                    path: "src/Main.java".into(),
                    line: Some(42),
                    column: None
                }),
                None
            ))
        );
        assert_eq!(
            parse(&["src/Main.java:42:10"]),
            Ok(open(
                Some(FileTarget {
                    path: "src/Main.java".into(),
                    line: Some(42),
                    column: Some(10)
                }),
                None
            ))
        );
    }

    #[test]
    fn paths_with_spaces_and_non_numeric_colons_are_kept() {
        assert_eq!(
            parse(&["My Project/Main.java"]),
            Ok(open(Some(FileTarget::new("My Project/Main.java")), None))
        );
        assert_eq!(
            parse(&["notes:draft.txt"]),
            Ok(open(Some(FileTarget::new("notes:draft.txt")), None))
        );
        assert_eq!(
            parse(&["C:\\src\\Main.java:7"]),
            Ok(open(
                Some(FileTarget {
                    path: "C:\\src\\Main.java".into(),
                    line: Some(7),
                    column: None
                }),
                None
            ))
        );
        assert_eq!(
            parse(&[":12"]),
            Ok(open(Some(FileTarget::new(":12")), None))
        );
    }

    #[test]
    fn existing_file_with_numeric_suffix_wins_over_position() {
        let cmd = parse_args_with(["kova", "log:2024"], |p| p == Path::new("log:2024"));
        assert_eq!(cmd, Ok(open(Some(FileTarget::new("log:2024")), None)));
    }

    #[test]
    fn config_dir_help_version_and_budget_flags() {
        assert_eq!(
            parse(&["--config-dir", "/tmp/kova-cfg", "a.java"]),
            Ok(open(Some(FileTarget::new("a.java")), Some("/tmp/kova-cfg")))
        );
        assert_eq!(
            parse(&["--config-dir=/tmp/x"]),
            Ok(open(None, Some("/tmp/x")))
        );
        assert_eq!(parse(&["--help"]), Ok(CliCommand::PrintHelp));
        assert_eq!(parse(&["-h", "x"]), Ok(CliCommand::PrintHelp));
        assert_eq!(parse(&["-V"]), Ok(CliCommand::PrintVersion));
        assert_eq!(parse(&["--version"]), Ok(CliCommand::PrintVersion));
        assert_eq!(
            parse(&[PROBE_STARTUP_FLAG, ASSERT_BUDGET_FLAG]),
            Ok(open(None, None))
        );
        assert_eq!(
            parse(&["--", "-weird.java"]),
            Ok(open(Some(FileTarget::new("-weird.java")), None))
        );
    }

    #[test]
    fn rejects_unknown_flags_missing_values_and_extra_files() {
        assert_eq!(
            parse(&["--frobnicate"]),
            Err(CliParseError::UnknownOption("--frobnicate".into()))
        );
        assert_eq!(
            parse(&["--config-dir"]),
            Err(CliParseError::MissingValue("--config-dir"))
        );
        assert_eq!(
            parse(&["--config-dir="]),
            Err(CliParseError::MissingValue("--config-dir"))
        );
        assert_eq!(
            parse(&["a.java", "b.java"]),
            Err(CliParseError::UnexpectedArgument("b.java".into()))
        );
    }

    #[test]
    fn help_and_version_mention_package_version() {
        assert!(help_text().contains(env!("CARGO_PKG_VERSION")));
        assert!(help_text().contains("--config-dir"));
        assert_eq!(
            version_text(),
            format!("kova {}", env!("CARGO_PKG_VERSION"))
        );
    }
}
