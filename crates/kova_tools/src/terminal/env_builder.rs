//! Omakase JVM environment for terminal sessions.
//!
//! A session inherits the IDE process environment and gets these overrides on top:
//!
//! * `JAVA_HOME` — the project JDK selected by `kova_toolchain` (`kova.toml` `jdk`, then
//!   `settings.toml` `jvm.preferred_jdk`, then the best discovered JDK);
//! * `PATH` — `$JAVA_HOME/bin` prepended, so `java`, `javac` and `./gradlew` use that JDK;
//! * `TERM=xterm-256color`, `COLORTERM=truecolor`, `TERM_PROGRAM=Kova`.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use kova_config::{EffectiveConfig, KovaPaths, KovaSettings, ProjectConfig};
use kova_toolchain::{JdkDiscoveryRoots, build_augmented_path, discover_jdks, select_project_jdk};

/// `TERM` advertised to programs running inside the terminal.
pub const TERM_VALUE: &str = "xterm-256color";

/// Environment overrides applied on top of the inherited process environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalEnv {
    vars: Vec<(OsString, OsString)>,
}

impl TerminalEnv {
    /// Builds the overrides for a session, reading the base `PATH` through `lookup`.
    ///
    /// `java_home` is the project JDK; `None` (or injection disabled in settings) leaves
    /// `JAVA_HOME` and `PATH` untouched.
    #[must_use]
    pub fn build<F>(java_home: Option<&Path>, lookup: F) -> Self
    where
        F: Fn(&str) -> Option<OsString>,
    {
        let mut env = Self::default();
        env.set("TERM", TERM_VALUE);
        env.set("COLORTERM", "truecolor");
        env.set("TERM_PROGRAM", "Kova");
        if let Some(home) = java_home {
            env.set("JAVA_HOME", home.as_os_str());
            env.set(
                "PATH",
                prepend_path(&home.join("bin"), lookup("PATH").as_deref()),
            );
        }
        env
    }

    /// Builds the overrides from the real process environment, injecting an augmented `PATH`
    /// (Homebrew, SDKMAN, Cargo, `<JAVA_HOME>/bin`) consistent with the build/test runners.
    #[must_use]
    pub fn from_process(java_home: Option<&Path>) -> Self {
        let mut env = Self::build(java_home, |key| std::env::var_os(key));
        let base_path = std::env::var_os("PATH");
        let home_dir = std::env::var_os("HOME").map(PathBuf::from);
        let augmented =
            build_augmented_path(java_home, base_path.as_deref(), home_dir.as_deref(), &[]);
        env.set("PATH", augmented);
        env
    }

    /// Sets (or replaces) one variable.
    pub fn set(&mut self, key: impl Into<OsString>, value: impl AsRef<OsStr>) {
        let key = key.into();
        let value = value.as_ref().to_os_string();
        if let Some(slot) = self.vars.iter_mut().find(|(k, _)| *k == key) {
            slot.1 = value;
        } else {
            self.vars.push((key, value));
        }
    }

    /// Drops the override for `key`, if any.
    pub fn remove(&mut self, key: &str) {
        self.vars.retain(|(k, _)| k != key);
    }

    /// Returns the override for `key`, if any.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&OsStr> {
        self.vars
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_os_str())
    }

    /// Iterates over the overrides in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&OsStr, &OsStr)> {
        self.vars
            .iter()
            .map(|(k, v)| (k.as_os_str(), v.as_os_str()))
    }
}

/// Puts `bin` in front of `base`, dropping any later duplicate of `bin`.
fn prepend_path(bin: &Path, base: Option<&OsStr>) -> OsString {
    let mut entries = vec![bin.to_path_buf()];
    if let Some(base) = base {
        entries.extend(std::env::split_paths(base).filter(|entry| entry != bin));
    }
    std::env::join_paths(&entries).unwrap_or_else(|_| bin.as_os_str().to_os_string())
}

/// Resolves the project JDK home for `project_root`.
///
/// Reads `kova.toml` (if any) merged with `settings`, discovers installed JDKs and applies
/// `kova_toolchain`'s selection rules. Touches the disk: call it off the UI thread.
/// Returns `None` when no installed JDK satisfies the requirement.
#[must_use]
pub fn resolve_project_java_home(project_root: &Path, settings: &KovaSettings) -> Option<PathBuf> {
    let project = ProjectConfig::load_optional(project_root).ok().flatten();
    let config = EffectiveConfig::merge(settings, project.as_ref());
    let roots = KovaPaths::resolve().map_or_else(
        |_| JdkDiscoveryRoots::isolated(),
        |paths| JdkDiscoveryRoots::from_env_and_paths(&paths),
    );
    let installations = discover_jdks(&roots);
    select_project_jdk(&installations, &config)
        .ok()
        .map(|jdk| jdk.home_dir.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup_path(value: &str) -> impl Fn(&str) -> Option<OsString> + '_ {
        move |key| (key == "PATH").then(|| OsString::from(value))
    }

    #[test]
    fn injects_java_home_and_prepends_its_bin_to_path() {
        let env = TerminalEnv::build(
            Some(Path::new("/opt/jdk-21")),
            lookup_path("/usr/bin:/opt/jdk-21/bin:/bin"),
        );
        assert_eq!(env.get("JAVA_HOME"), Some(OsStr::new("/opt/jdk-21")));
        assert_eq!(
            env.get("PATH"),
            Some(OsStr::new("/opt/jdk-21/bin:/usr/bin:/bin"))
        );
        assert_eq!(env.get("TERM"), Some(OsStr::new("xterm-256color")));
        assert_eq!(env.get("COLORTERM"), Some(OsStr::new("truecolor")));
    }

    #[test]
    fn without_jdk_leaves_java_home_and_path_alone() {
        let env = TerminalEnv::build(None, lookup_path("/usr/bin"));
        assert_eq!(env.get("JAVA_HOME"), None);
        assert_eq!(env.get("PATH"), None);
        assert_eq!(env.get("TERM_PROGRAM"), Some(OsStr::new("Kova")));
    }

    #[test]
    fn missing_base_path_yields_only_jdk_bin() {
        let env = TerminalEnv::build(Some(Path::new("/jdk")), |_| None);
        assert_eq!(env.get("PATH"), Some(OsStr::new("/jdk/bin")));
    }

    #[test]
    fn set_replaces_existing_values() {
        let mut env = TerminalEnv::default();
        env.set("A", "1");
        env.set("A", "2");
        assert_eq!(env.iter().count(), 1);
        assert_eq!(env.get("A"), Some(OsStr::new("2")));
    }
}
