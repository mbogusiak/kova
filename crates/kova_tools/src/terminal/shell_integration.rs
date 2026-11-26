//! Shell integration that keeps the Omakase `JAVA_HOME` after the user's startup files.
//!
//! An interactive login shell sources `~/.zprofile`, `~/.zshrc`, `~/.bash_profile` …, where
//! tools such as SDKMAN, jenv or macOS `path_helper` reset `JAVA_HOME` and reorder `PATH`.
//! Exporting the variables at spawn time is therefore not enough: for `zsh` and `bash` the
//! session starts through small wrapper startup files that source the user's own files first
//! and then re-apply `KOVA_JAVA_HOME` (`JAVA_HOME` plus `$JAVA_HOME/bin` in front of `PATH`).
//! Other shells keep the spawn-time environment only.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use super::env_builder::TerminalEnv;

/// Variable carrying the project JDK home into the wrapper startup files.
pub const KOVA_JAVA_HOME_VAR: &str = "KOVA_JAVA_HOME";

const ORIG_ZDOTDIR_VAR: &str = "KOVA_ORIG_ZDOTDIR";

const APPLY: &str = r#"if [ -n "$KOVA_JAVA_HOME" ]; then
  export JAVA_HOME="$KOVA_JAVA_HOME"
  export PATH="$KOVA_JAVA_HOME/bin:$PATH"
fi
"#;

const ZSHENV: &str = r#"KOVA_ZDOTDIR="$ZDOTDIR"
ZDOTDIR="$KOVA_ORIG_ZDOTDIR"
[ -f "$ZDOTDIR/.zshenv" ] && . "$ZDOTDIR/.zshenv"
ZDOTDIR="$KOVA_ZDOTDIR"
"#;

const ZPROFILE: &str = r#"[ -f "$KOVA_ORIG_ZDOTDIR/.zprofile" ] && . "$KOVA_ORIG_ZDOTDIR/.zprofile"
"#;

const ZSHRC_HEAD: &str = r#"[ -f "$KOVA_ORIG_ZDOTDIR/.zshrc" ] && . "$KOVA_ORIG_ZDOTDIR/.zshrc"
"#;

const ZLOGIN_HEAD: &str = r#"ZDOTDIR="$KOVA_ORIG_ZDOTDIR"
unset KOVA_ZDOTDIR KOVA_ORIG_ZDOTDIR
[ -f "$ZDOTDIR/.zlogin" ] && . "$ZDOTDIR/.zlogin"
"#;

const BASHRC_HEAD: &str = r#"[ -f /etc/profile ] && . /etc/profile
for kova_rc in "$HOME/.bash_profile" "$HOME/.bash_login" "$HOME/.profile"; do
  if [ -f "$kova_rc" ]; then . "$kova_rc"; break; fi
done
unset kova_rc
"#;

/// Shell families with integration scripts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellKind {
    Zsh,
    Bash,
    Other,
}

fn shell_kind(program: &Path) -> ShellKind {
    match program.file_name().and_then(|name| name.to_str()) {
        Some("zsh") => ShellKind::Zsh,
        Some("bash") => ShellKind::Bash,
        _ => ShellKind::Other,
    }
}

/// Directory holding the wrapper startup files (content is static, so it is shared).
fn integration_dir() -> PathBuf {
    std::env::temp_dir().join(format!(
        "kova-shell-integration-{}",
        env!("CARGO_PKG_VERSION")
    ))
}

fn write_if_changed(path: &Path, content: &str) -> io::Result<()> {
    if std::fs::read_to_string(path).is_ok_and(|existing| existing == content) {
        return Ok(());
    }
    std::fs::write(path, content)
}

fn install_zsh(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    write_if_changed(&dir.join(".zshenv"), ZSHENV)?;
    write_if_changed(&dir.join(".zprofile"), ZPROFILE)?;
    write_if_changed(&dir.join(".zshrc"), &format!("{ZSHRC_HEAD}{APPLY}"))?;
    write_if_changed(&dir.join(".zlogin"), &format!("{ZLOGIN_HEAD}{APPLY}"))
}

fn install_bash(dir: &Path) -> io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let rc = dir.join("bashrc");
    write_if_changed(&rc, &format!("{BASHRC_HEAD}{APPLY}"))?;
    Ok(rc)
}

/// Prepares a login shell so `java_home` wins over the user's startup files.
///
/// Updates `env` and returns the shell arguments to use. `lookup` reads the inherited process
/// environment (`ZDOTDIR`, `HOME`). Falls back to a plain login shell (`-l`) for unknown shells,
/// when no JDK is injected, or when the wrapper files cannot be written.
pub fn prepare_login_shell<F>(
    program: &Path,
    java_home: Option<&Path>,
    env: &mut TerminalEnv,
    lookup: F,
) -> Vec<String>
where
    F: Fn(&str) -> Option<OsString>,
{
    let login = vec!["-l".to_owned()];
    let Some(java_home) = java_home else {
        return login;
    };
    let dir = integration_dir();
    match shell_kind(program) {
        ShellKind::Zsh => {
            let original = env
                .get("ZDOTDIR")
                .map(OsString::from)
                .or_else(|| lookup("ZDOTDIR"))
                .or_else(|| env.get("HOME").map(OsString::from))
                .or_else(|| lookup("HOME"));
            let (Some(original), Ok(())) = (original, install_zsh(&dir)) else {
                return login;
            };
            env.set(KOVA_JAVA_HOME_VAR, java_home.as_os_str());
            env.set(ORIG_ZDOTDIR_VAR, original);
            env.set("ZDOTDIR", dir.as_os_str());
            login
        }
        ShellKind::Bash => match install_bash(&dir) {
            Ok(rc) => {
                env.set(KOVA_JAVA_HOME_VAR, java_home.as_os_str());
                vec![
                    "--rcfile".to_owned(),
                    rc.to_string_lossy().into_owned(),
                    "-i".to_owned(),
                ]
            }
            Err(_) => login,
        },
        ShellKind::Other => login,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn other_shells_and_missing_jdk_use_a_plain_login_shell() {
        let mut env = TerminalEnv::default();
        let args = prepare_login_shell(
            Path::new("/usr/bin/fish"),
            Some(Path::new("/jdk")),
            &mut env,
            |_| None,
        );
        assert_eq!(args, vec!["-l"]);
        assert_eq!(env.get(KOVA_JAVA_HOME_VAR), None);
        let args = prepare_login_shell(Path::new("/bin/zsh"), None, &mut env, |_| None);
        assert_eq!(args, vec!["-l"]);
    }

    #[test]
    fn zsh_gets_a_wrapper_zdotdir() {
        let mut env = TerminalEnv::default();
        let args = prepare_login_shell(
            Path::new("/bin/zsh"),
            Some(Path::new("/jdk")),
            &mut env,
            |key| (key == "HOME").then(|| OsString::from("/home/dev")),
        );
        assert_eq!(args, vec!["-l"]);
        assert_eq!(
            env.get(KOVA_JAVA_HOME_VAR),
            Some(std::ffi::OsStr::new("/jdk"))
        );
        assert_eq!(
            env.get(ORIG_ZDOTDIR_VAR),
            Some(std::ffi::OsStr::new("/home/dev"))
        );
        let dir = PathBuf::from(env.get("ZDOTDIR").unwrap_or_default());
        assert!(dir.join(".zshrc").is_file());
        assert!(dir.join(".zlogin").is_file());
    }

    #[test]
    fn bash_gets_an_rcfile() {
        let mut env = TerminalEnv::default();
        let args = prepare_login_shell(
            Path::new("/bin/bash"),
            Some(Path::new("/jdk")),
            &mut env,
            |_| None,
        );
        assert_eq!(args.first().map(String::as_str), Some("--rcfile"));
        assert!(Path::new(&args[1]).is_file());
    }
}
