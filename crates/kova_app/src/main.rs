//! `kova` — Kova IDE launcher binary.

use std::process::ExitCode;
use std::time::Instant;

fn main() -> ExitCode {
    let process_start = Instant::now();
    let options = kova_app::AppLaunchOptions::from_env_and_args();
    kova_app::run_with_options(process_start, options)
}
