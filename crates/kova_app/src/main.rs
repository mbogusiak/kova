//! `kova` — Kova IDE launcher binary.
//!
//! `kova [OPTIONS] [FILE[:LINE[:COLUMN]] | PROJECT_DIR]` — see `kova --help`.

use std::process::ExitCode;
use std::time::Instant;

use kova_app::cli::{self, CliCommand};

/// Exit code for invalid command-line usage.
const USAGE_EXIT_CODE: u8 = 2;

fn main() -> ExitCode {
    let process_start = Instant::now();
    match cli::parse_args(std::env::args_os()) {
        Ok(CliCommand::PrintHelp) => write_line(&mut std::io::stdout().lock(), &cli::help_text()),
        Ok(CliCommand::PrintVersion) => {
            write_line(&mut std::io::stdout().lock(), &cli::version_text())
        }
        Ok(CliCommand::Open {
            target,
            config_dir_override,
        }) => {
            // A directory opens as the project root (`kova <project-dir>`); a file opens in
            // the editor.
            let options = kova_app::AppLaunchOptions::from_env_and_args();
            kova_app::run_cli(
                process_start,
                options,
                kova_app::LaunchRequest::from_cli(target, config_dir_override),
            )
        }
        Err(err) => {
            let _ = write_line(&mut std::io::stderr().lock(), &format!("kova: {err}"));
            ExitCode::from(USAGE_EXIT_CODE)
        }
    }
}

fn write_line(out: &mut dyn std::io::Write, line: &str) -> ExitCode {
    if writeln!(out, "{line}").and_then(|()| out.flush()).is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
