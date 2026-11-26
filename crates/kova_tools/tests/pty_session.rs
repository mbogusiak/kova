//! Headless PTY session tests (`KOVA-039`): start, `JAVA_HOME` injection, output, resize, exit.
#![cfg(unix)]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "test harness: failures must abort the test with context"
)]

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use kova_tools::terminal::{
    PtySession, PtySpawnConfig, TerminalEnv, TerminalSize, prepare_login_shell,
};

const TIMEOUT: Duration = Duration::from_secs(10);

fn sh(script: &str) -> PtySpawnConfig {
    PtySpawnConfig::command("/bin/sh", &["-c", script])
}

/// Polls the visible grid until `needle` appears or the deadline passes.
fn wait_for_text(session: &PtySession, needle: &str) -> String {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let text = session.snapshot().text();
        if text.contains(needle) {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {needle:?}; screen:\n{text}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_until(mut condition: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + TIMEOUT;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .is_ok_and(|status| status.success())
}

#[test]
fn echo_output_reaches_the_grid() {
    let session = PtySession::spawn(&sh("echo kova-pty-ok")).expect("spawn");
    wait_for_text(&session, "kova-pty-ok");
    wait_until(|| session.is_closed(), "EOF after the child exits");
    assert!(session.take_dirty(), "new output raises the dirty flag");
}

#[test]
fn java_home_and_path_are_injected() {
    let mut config = sh("echo \"JH=$JAVA_HOME\"; echo \"P=$PATH\"; echo \"T=$TERM\"");
    config.env = TerminalEnv::build(Some(Path::new("/opt/kova-jdk-21")), |key| {
        (key == "PATH").then(|| "/usr/bin:/bin".into())
    });
    let session = PtySession::spawn(&config).expect("spawn");
    let text = wait_for_text(&session, "T=xterm-256color");
    assert!(text.contains("JH=/opt/kova-jdk-21"), "screen:\n{text}");
    assert!(
        text.contains("P=/opt/kova-jdk-21/bin:/usr/bin:/bin"),
        "screen:\n{text}"
    );
}

#[test]
fn working_directory_is_the_project_root() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonical");
    let mut config = sh("pwd -P");
    config.cwd = Some(root.clone());
    let session = PtySession::spawn(&config).expect("spawn");
    wait_for_text(&session, &root.to_string_lossy());
}

#[test]
fn input_is_echoed_back_by_the_program() {
    let session = PtySession::spawn(&sh("read line; echo \"got:$line\"")).expect("spawn");
    session.write_bytes(b"hello\r").expect("write");
    wait_for_text(&session, "got:hello");
}

#[test]
fn resize_updates_the_kernel_window_size() {
    let mut config = sh("read _; stty size");
    config.size = TerminalSize::new(80, 24);
    let session = PtySession::spawn(&config).expect("spawn");
    session.resize(TerminalSize::new(100, 30)).expect("resize");
    assert_eq!(session.size(), TerminalSize::new(100, 30));
    session.write_bytes(b"\r").expect("write");
    wait_for_text(&session, "30 100");
}

#[test]
fn dropping_the_session_terminates_the_shell() {
    let session = PtySession::spawn(&sh("echo ready; sleep 60")).expect("spawn");
    wait_for_text(&session, "ready");
    let pid = session.process_id().expect("pid");
    assert!(process_alive(pid));
    drop(session);
    // A reaped child no longer answers `kill -0`.
    wait_until(|| !process_alive(pid), "child to be terminated and reaped");
}

#[test]
fn spawn_failure_is_reported() {
    let err = PtySession::spawn(&PtySpawnConfig::command("/nonexistent/kova-shell", &[]))
        .expect_err("missing program must fail");
    assert!(err.to_string().contains("kova-shell"), "{err}");
}

/// Startup files that reset `JAVA_HOME` / `PATH` (SDKMAN style) must not win over Kova.
fn assert_injection_survives_startup_files(shell: &str, rc_files: &[&str]) {
    if !Path::new(shell).exists() {
        return;
    }
    let home = tempfile::tempdir().expect("tempdir");
    for rc in rc_files {
        std::fs::write(
            home.path().join(rc),
            "export RC_SEEN=1\nexport JAVA_HOME=/wrong/jdk\nexport PATH=\"/wrong/jdk/bin:$PATH\"\n",
        )
        .expect("write rc");
    }
    let java_home = Path::new("/opt/kova-jdk-21");
    let home_dir = home.path().to_path_buf();
    let lookup = |key: &str| match key {
        "HOME" => Some(home_dir.clone().into_os_string()),
        "PATH" => std::env::var_os("PATH"),
        _ => None,
    };
    let mut env = TerminalEnv::build(Some(java_home), lookup);
    env.set("HOME", home.path());
    let mut config = PtySpawnConfig::command(shell, &[]);
    config.args = prepare_login_shell(Path::new(shell), Some(java_home), &mut env, lookup);
    config.env = env;
    config.cwd = Some(home.path().to_path_buf());
    let session = PtySession::spawn(&config).expect("spawn");
    session
        .write_bytes(b"echo \"RC=$RC_SEEN JH=$JAVA_HOME\"; echo \"P1=${PATH%%:*}\"; exit\r")
        .expect("write");
    let text = wait_for_text(&session, "P1=/");
    assert!(text.contains("RC=1 JH=/opt/kova-jdk-21"), "screen:\n{text}");
    assert!(text.contains("P1=/opt/kova-jdk-21/bin"), "screen:\n{text}");
}

#[test]
fn zsh_startup_files_cannot_override_the_project_jdk() {
    assert_injection_survives_startup_files(
        "/bin/zsh",
        &[".zshenv", ".zprofile", ".zshrc", ".zlogin"],
    );
}

#[test]
fn bash_startup_files_cannot_override_the_project_jdk() {
    assert_injection_survives_startup_files("/bin/bash", &[".bash_profile"]);
}
