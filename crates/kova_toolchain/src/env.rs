//! Deterministic runtime environment resolution (`JAVA_HOME`, augmented `PATH`,
//! and explicit executable lookup) for headless build, test, and BSP runners.
//!
//! On macOS, GUI apps launched from Finder/Dock/Spotlight inherit a minimal
//! `PATH` (`/usr/bin:/bin:/usr/sbin:/sbin`) without `JAVA_HOME`, Homebrew, SDKMAN,
//! or Cargo. In addition, `posix_spawnp` searches bare program names (like `cargo`
//! or `gradle`) against the *parent* process `PATH` even when `.env("PATH", ...)`
//! is passed to `Command::new`. [`resolve_executable`] resolves bare commands against
//! the augmented `PATH` before spawning.

use std::collections::{BTreeMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

use kova_config::{EffectiveConfig, KovaPaths, KovaSettings, ProjectConfig, SettingsDocument};

use crate::jdk::{JdkDiscoveryRoots, discover_jdks, select_project_jdk};

/// Options controlling [`resolve_toolchain_env`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolchainEnvOptions {
    /// Optional workspace root used to read `<project_root>/kova.toml` (`project.jdk`).
    pub project_root: Option<PathBuf>,
    /// Explicit JDK requirement override (e.g. `"21"` or `"temurin-21"`).
    pub preferred_jdk: Option<String>,
    /// Extra directories prepended to `PATH` right after `<JAVA_HOME>/bin`.
    pub extra_path_entries: Vec<PathBuf>,
}

/// Resolved environment variables and augmented `PATH` for child processes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolchainEnv {
    /// Selected `JAVA_HOME` directory, if a JDK was discovered.
    pub java_home: Option<PathBuf>,
    /// Deduplicated, priority-ordered `PATH` suitable for `.env("PATH", ...)`.
    pub path: OsString,
    /// Ready-to-inject environment variable map (`PATH` and, when available, `JAVA_HOME`).
    pub vars: BTreeMap<String, String>,
}

/// Builds a deduplicated, priority-ordered `PATH` value.
///
/// Precedence order:
/// 1. `<java_home>/bin` (if `java_home` is `Some`)
/// 2. `extra_dirs`
/// 3. `$HOME/.sdkman/candidates/{java,gradle,maven}/current/bin` (if `home_dir` is `Some`)
/// 4. `$HOME/.cargo/bin` (if `home_dir` is `Some`)
/// 5. Platform defaults (`/opt/homebrew/bin`, `/opt/homebrew/sbin`, `/usr/local/bin` on macOS;
///    `$HOME/.local/bin`, `/usr/local/bin` on Linux)
/// 6. Existing entries from `base_path`
#[must_use]
pub fn build_augmented_path(
    java_home: Option<&Path>,
    base_path: Option<&OsStr>,
    home_dir: Option<&Path>,
    extra_dirs: &[PathBuf],
) -> OsString {
    let mut entries: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();

    let mut push_entry = |candidate: PathBuf| {
        if candidate.as_os_str().is_empty() {
            return;
        }
        if seen.insert(candidate.clone()) {
            entries.push(candidate);
        }
    };

    if let Some(home) = java_home {
        push_entry(home.join("bin"));
    }

    for dir in extra_dirs {
        push_entry(dir.clone());
    }

    if let Some(home) = home_dir {
        let sdkman = home.join(".sdkman").join("candidates");
        for tool in ["java", "gradle", "maven"] {
            push_entry(sdkman.join(tool).join("current").join("bin"));
        }
        push_entry(home.join(".cargo").join("bin"));
    }

    if cfg!(target_os = "macos") {
        for dir in ["/opt/homebrew/bin", "/opt/homebrew/sbin", "/usr/local/bin"] {
            push_entry(PathBuf::from(dir));
        }
    } else if cfg!(target_os = "linux") {
        if let Some(home) = home_dir {
            push_entry(home.join(".local").join("bin"));
        }
        push_entry(PathBuf::from("/usr/local/bin"));
    }

    if let Some(base) = base_path {
        for entry in std::env::split_paths(base) {
            push_entry(entry);
        }
    }

    std::env::join_paths(&entries)
        .unwrap_or_else(|_| base_path.map(OsStr::to_os_string).unwrap_or_default())
}

/// Resolves `JAVA_HOME` and an augmented `PATH` using the host environment and standard paths.
#[must_use]
pub fn resolve_toolchain_env(opts: &ToolchainEnvOptions) -> ToolchainEnv {
    let paths = KovaPaths::resolve().ok();
    let roots = paths.as_ref().map_or_else(
        JdkDiscoveryRoots::isolated,
        JdkDiscoveryRoots::from_env_and_paths,
    );
    let user_settings = paths
        .as_ref()
        .and_then(|p| fs::read_to_string(p.settings_file()).ok())
        .and_then(|text| SettingsDocument::from_toml_str(&text).ok())
        .map_or_else(KovaSettings::default, |doc| doc.settings().clone());
    resolve_toolchain_env_with_lookup(opts, &roots, &user_settings, |key| std::env::var_os(key))
}

/// Deterministic, environment-injectable variant of [`resolve_toolchain_env`] for unit tests.
#[must_use]
pub fn resolve_toolchain_env_with_lookup<F>(
    opts: &ToolchainEnvOptions,
    roots: &JdkDiscoveryRoots,
    user_settings: &KovaSettings,
    env_lookup: F,
) -> ToolchainEnv
where
    F: Fn(&str) -> Option<OsString>,
{
    let project_cfg = opts
        .project_root
        .as_deref()
        .and_then(|root| ProjectConfig::load_optional(root).ok().flatten());
    let mut effective = EffectiveConfig::merge(user_settings, project_cfg.as_ref());
    if let Some(preferred) = &opts.preferred_jdk {
        effective.settings.jvm.preferred_jdk = Some(preferred.clone());
    }

    let installations = discover_jdks(roots);
    let default_config = EffectiveConfig::merge(&KovaSettings::default(), None);
    let selected_jdk = select_project_jdk(&installations, &effective)
        .ok()
        .or_else(|| select_project_jdk(&installations, &default_config).ok());
    let java_home = selected_jdk.map(|jdk| jdk.home_dir.clone());

    let home_dir = env_lookup("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let base_path = env_lookup("PATH");
    let path = build_augmented_path(
        java_home.as_deref(),
        base_path.as_deref(),
        home_dir.as_deref(),
        &opts.extra_path_entries,
    );

    let mut vars = BTreeMap::new();
    vars.insert("PATH".to_owned(), path.to_string_lossy().into_owned());
    if let Some(home) = &java_home {
        vars.insert("JAVA_HOME".to_owned(), home.to_string_lossy().into_owned());
    }

    ToolchainEnv {
        java_home,
        path,
        vars,
    }
}

/// Resolves a bare executable name (e.g. `cargo`, `gradle`, `mvn`) against `augmented_path`.
///
/// Paths that already contain directory separators (e.g. `./gradlew`, `/usr/bin/env`)
/// are returned unchanged. If no executable match is found in `augmented_path`, the
/// original `program` path is returned as a fallback.
#[must_use]
pub fn resolve_executable(program: &Path, augmented_path: &OsStr) -> PathBuf {
    if program.as_os_str().is_empty() || program.components().count() > 1 {
        return program.to_path_buf();
    }

    for dir in std::env::split_paths(augmented_path) {
        let candidate = dir.join(program);
        if is_executable_file(&candidate) {
            return candidate;
        }
        #[cfg(windows)]
        if program.extension().is_none()
            && let Some(file_name) = program.to_str()
        {
            for ext in ["exe", "cmd", "bat"] {
                let with_ext = dir.join(format!("{file_name}.{ext}"));
                if is_executable_file(&with_ext) {
                    return with_ext;
                }
            }
        }
    }

    program.to_path_buf()
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "unit tests assert deterministic environment resolution"
)]
mod tests {
    use super::*;

    fn create_fake_jdk(root: &Path, version: &str, vendor: &str) {
        let bin = root.join("bin");
        fs::create_dir_all(&bin).expect("bin dir");
        let java = bin.join(if cfg!(windows) { "java.exe" } else { "java" });
        fs::write(&java, "#!/bin/sh\nexit 0\n").expect("write java");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&java, fs::Permissions::from_mode(0o755)).expect("chmod java");
        }
        fs::write(
            root.join("release"),
            format!("JAVA_VERSION=\"{version}\"\nIMPLEMENTOR=\"{vendor}\"\nOS_ARCH=\"aarch64\"\n"),
        )
        .expect("release");
    }

    #[test]
    fn augmented_path_preserves_priority_and_deduplicates() {
        let java_home = Path::new("/jdks/21");
        let home = Path::new("/Users/dev");
        let extra = vec![PathBuf::from("/custom/bin")];
        let base = std::env::join_paths([
            PathBuf::from("/Users/dev/.cargo/bin"),
            PathBuf::from("/usr/bin"),
            PathBuf::from("/jdks/21/bin"),
            PathBuf::from("/bin"),
        ])
        .expect("join base");

        let augmented = build_augmented_path(Some(java_home), Some(&base), Some(home), &extra);
        let parts: Vec<PathBuf> = std::env::split_paths(&augmented).collect();

        assert_eq!(parts[0], PathBuf::from("/jdks/21/bin"));
        assert_eq!(parts[1], PathBuf::from("/custom/bin"));
        assert_eq!(
            parts[2],
            PathBuf::from("/Users/dev/.sdkman/candidates/java/current/bin")
        );
        assert_eq!(
            parts[3],
            PathBuf::from("/Users/dev/.sdkman/candidates/gradle/current/bin")
        );
        assert_eq!(
            parts[4],
            PathBuf::from("/Users/dev/.sdkman/candidates/maven/current/bin")
        );
        assert_eq!(parts[5], PathBuf::from("/Users/dev/.cargo/bin"));
        assert!(parts.contains(&PathBuf::from("/usr/bin")));
        assert!(parts.contains(&PathBuf::from("/bin")));

        // No duplicates.
        let unique: HashSet<&PathBuf> = parts.iter().collect();
        assert_eq!(unique.len(), parts.len());
    }

    #[test]
    fn resolves_project_jdk_from_kova_toml_and_injects_vars() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let project_root = tmp.path().join("project");
        fs::create_dir_all(&project_root).expect("project dir");
        fs::write(
            project_root.join("kova.toml"),
            "schema_version = 1\njdk = \"17\"\n",
        )
        .expect("kova.toml");

        let jdk17 = tmp.path().join("jdks/17");
        let jdk21 = tmp.path().join("jdks/21");
        create_fake_jdk(&jdk17, "17.0.12", "Eclipse Adoptium");
        create_fake_jdk(&jdk21, "21.0.5", "Eclipse Adoptium");

        let mut roots = JdkDiscoveryRoots::isolated();
        roots.explicit_paths = vec![jdk17.clone(), jdk21];

        let opts = ToolchainEnvOptions {
            project_root: Some(project_root),
            ..ToolchainEnvOptions::default()
        };
        let env =
            resolve_toolchain_env_with_lookup(&opts, &roots, &KovaSettings::default(), |key| {
                match key {
                    "HOME" => Some(OsString::from(tmp.path().join("home").as_os_str())),
                    "PATH" => Some(OsString::from("/usr/bin:/bin")),
                    _ => None,
                }
            });

        let canonical_17 = fs::canonicalize(&jdk17).expect("canonical 17");
        assert_eq!(env.java_home.as_ref(), Some(&canonical_17));
        assert_eq!(
            env.vars.get("JAVA_HOME").map(PathBuf::from).as_ref(),
            Some(&canonical_17)
        );
        let resolved_parts: Vec<PathBuf> = std::env::split_paths(&env.path).collect();
        assert_eq!(resolved_parts[0], canonical_17.join("bin"));
    }

    #[test]
    fn resolve_toolchain_env_leaves_java_home_none_when_no_jdk_found() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let env = resolve_toolchain_env_with_lookup(
            &ToolchainEnvOptions::default(),
            &JdkDiscoveryRoots::isolated(),
            &KovaSettings::default(),
            |key| match key {
                "HOME" => Some(OsString::from(tmp.path().as_os_str())),
                "PATH" => Some(OsString::from("/usr/bin")),
                _ => None,
            },
        );
        assert_eq!(env.java_home, None);
        assert!(!env.vars.contains_key("JAVA_HOME"));
        assert!(env.vars.contains_key("PATH"));
    }

    #[test]
    fn resolve_executable_finds_first_executable_and_preserves_relative_or_absolute_paths() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir1 = tmp.path().join("bin1");
        let dir2 = tmp.path().join("bin2");
        fs::create_dir_all(&dir1).expect("dir1");
        fs::create_dir_all(&dir2).expect("dir2");

        // Non-executable file in dir1 must be skipped in favor of executable in dir2.
        let non_exec = dir1.join("mytool");
        fs::write(&non_exec, "not executable").expect("write non-exec");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&non_exec, fs::Permissions::from_mode(0o644)).expect("chmod 644");
        }

        let real_exec = dir2.join("mytool");
        fs::write(&real_exec, "#!/bin/sh\nexit 0\n").expect("write exec");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&real_exec, fs::Permissions::from_mode(0o755)).expect("chmod 755");
        }

        let path = std::env::join_paths([&dir1, &dir2]).expect("join");
        assert_eq!(resolve_executable(Path::new("mytool"), &path), real_exec);
        assert_eq!(
            resolve_executable(Path::new("missing-tool"), &path),
            PathBuf::from("missing-tool")
        );
        assert_eq!(
            resolve_executable(Path::new("./gradlew"), &path),
            PathBuf::from("./gradlew")
        );
        assert_eq!(
            resolve_executable(Path::new("/usr/bin/env"), &path),
            PathBuf::from("/usr/bin/env")
        );
    }
}
