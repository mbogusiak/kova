//! Deterministic, process-free JDK discovery and selection.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use kova_config::{EffectiveConfig, KovaPaths};
use serde::{Deserialize, Serialize};

use crate::error::ToolchainError;
use crate::jdk::version::{
    JavaVersion, JdkRequirement, JdkSource, JdkVendor, parse_jdk_requirement, parse_release_file,
};

/// Minimum Java major version required to run `jdt.ls` and Kova JVM tools.
pub const JDTLS_MIN_JAVA_MAJOR: u16 = 21;

/// A validated local JDK installation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JdkInstallation {
    /// Canonical `JAVA_HOME` directory (e.g. `.../Contents/Home` on macOS).
    pub home_dir: PathBuf,
    /// Path to the `java` executable (`<home_dir>/bin/java`).
    pub java_bin: PathBuf,
    /// Optional `<home_dir>/jmods` directory for modular JDKs (Java 9+).
    pub jmods_dir: Option<PathBuf>,
    /// Parsed Java version from `<home_dir>/release`.
    pub version: JavaVersion,
    /// Detected JDK vendor.
    pub vendor: JdkVendor,
    /// Target architecture from `OS_ARCH` (e.g. `"aarch64"`, `"x86_64"`).
    pub arch: String,
    /// Discovery source used for priority ordering.
    pub source: JdkSource,
}

/// Injectable filesystem roots for JDK discovery, enabling 100% isolated unit tests.
#[derive(Debug, Clone, Default)]
pub struct JdkDiscoveryRoots {
    /// Explicit `JAVA_HOME` paths (highest priority).
    pub explicit_paths: Vec<PathBuf>,
    /// Kova Omakase managed JDKs directory (`KovaPaths::jdks_dir()`).
    pub omakase_jdks_dir: Option<PathBuf>,
    /// `JAVA_HOME` environment variable value.
    pub env_java_home: Option<PathBuf>,
    /// SDKMAN candidates directory (`~/.sdkman/candidates/java`).
    pub sdkman_candidates_dir: Option<PathBuf>,
    /// System-wide JVM directories (`/Library/Java/JavaVirtualMachines`, `/usr/lib/jvm`, etc.).
    pub system_dirs: Vec<SystemJvmRoot>,
    /// Homebrew `opt` roots (`/opt/homebrew/opt`, `/usr/local/opt`) scanned for `openjdk*`.
    pub homebrew_opt_dirs: Vec<PathBuf>,
    /// `PATH` entries for fallback `java` binary discovery.
    pub path_entries: Vec<PathBuf>,
}

/// A system JVM root directory paired with its [`JdkSource`] classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemJvmRoot {
    /// Directory containing JDK installations or macOS `.jdk` bundles.
    pub dir: PathBuf,
    /// Source classification (`SystemMacOs` or `SystemLinux`).
    pub source: JdkSource,
}

impl JdkDiscoveryRoots {
    /// Builds discovery roots from the host environment and [`KovaPaths`].
    #[must_use]
    pub fn from_env_and_paths(paths: &KovaPaths) -> Self {
        Self::from_env_lookup_and_paths(paths, |key| std::env::var_os(key).map(PathBuf::from))
    }

    /// Builds discovery roots using a custom environment lookup closure (for deterministic testing).
    #[must_use]
    pub fn from_env_lookup_and_paths<F>(paths: &KovaPaths, env_lookup: F) -> Self
    where
        F: Fn(&str) -> Option<PathBuf>,
    {
        let home = env_lookup("HOME");
        let env_java_home = env_lookup("JAVA_HOME").filter(|p| !p.as_os_str().is_empty());
        let sdkman_candidates_dir = home
            .as_ref()
            .map(|h| h.join(".sdkman").join("candidates").join("java"));
        let mut path_entries: Vec<PathBuf> = env_lookup("PATH")
            .map(|p| std::env::split_paths(&p).collect())
            .unwrap_or_default();

        let mut system_dirs = Vec::new();
        let mut homebrew_opt_dirs = Vec::new();
        if cfg!(target_os = "macos") {
            system_dirs.push(SystemJvmRoot {
                dir: PathBuf::from("/Library/Java/JavaVirtualMachines"),
                source: JdkSource::SystemMacOs,
            });
            if let Some(h) = &home {
                system_dirs.push(SystemJvmRoot {
                    dir: h.join("Library/Java/JavaVirtualMachines"),
                    source: JdkSource::SystemMacOs,
                });
            }
            homebrew_opt_dirs.push(PathBuf::from("/opt/homebrew/opt"));
            homebrew_opt_dirs.push(PathBuf::from("/usr/local/opt"));
            for extra in ["/opt/homebrew/bin", "/usr/local/bin"] {
                let extra_path = PathBuf::from(extra);
                if !path_entries.contains(&extra_path) {
                    path_entries.push(extra_path);
                }
            }
        } else if cfg!(target_os = "linux") {
            system_dirs.push(SystemJvmRoot {
                dir: PathBuf::from("/usr/lib/jvm"),
                source: JdkSource::SystemLinux,
            });
            system_dirs.push(SystemJvmRoot {
                dir: PathBuf::from("/usr/lib64/jvm"),
                source: JdkSource::SystemLinux,
            });
        }

        Self {
            explicit_paths: Vec::new(),
            omakase_jdks_dir: Some(paths.jdks_dir()),
            env_java_home,
            sdkman_candidates_dir,
            system_dirs,
            homebrew_opt_dirs,
            path_entries,
        }
    }

    /// Creates an empty, isolated [`JdkDiscoveryRoots`] that never touches host paths or env vars.
    #[must_use]
    pub fn isolated() -> Self {
        Self::default()
    }
}

/// Discovers and validates all JDK installations under `roots`, deduplicated by canonical `home_dir`.
///
/// Reads `<home_dir>/release` directly without spawning `java -version`.
#[must_use]
pub fn discover_jdks(roots: &JdkDiscoveryRoots) -> Vec<JdkInstallation> {
    let mut results = Vec::new();
    let mut seen_homes = HashSet::new();

    for path in &roots.explicit_paths {
        try_add_candidate(path, JdkSource::Explicit, &mut seen_homes, &mut results);
    }

    if let Some(omakase_dir) = &roots.omakase_jdks_dir {
        scan_child_dirs(
            omakase_dir,
            JdkSource::KovaOmakase,
            false,
            &mut seen_homes,
            &mut results,
        );
    }

    if let Some(java_home) = &roots.env_java_home {
        try_add_candidate(
            java_home,
            JdkSource::EnvJavaHome,
            &mut seen_homes,
            &mut results,
        );
    }

    if let Some(sdkman_dir) = &roots.sdkman_candidates_dir {
        scan_child_dirs(
            sdkman_dir,
            JdkSource::Sdkman,
            true,
            &mut seen_homes,
            &mut results,
        );
    }

    for sys_root in &roots.system_dirs {
        scan_child_dirs(
            &sys_root.dir,
            sys_root.source,
            false,
            &mut seen_homes,
            &mut results,
        );
    }

    for homebrew_opt in &roots.homebrew_opt_dirs {
        scan_homebrew_openjdk_dirs(homebrew_opt, &mut seen_homes, &mut results);
    }

    for bin_dir in &roots.path_entries {
        let java_bin = bin_dir.join(java_executable_name());
        if is_executable_file(&java_bin)
            && let Ok(resolved) = fs::canonicalize(&java_bin)
            && let Some(home) = resolved.parent().and_then(Path::parent)
        {
            try_add_candidate(home, JdkSource::SystemPath, &mut seen_homes, &mut results);
        }
    }

    results
}

/// Validates a single directory as a [`JdkInstallation`] (normalizing macOS `Contents/Home`).
#[must_use]
pub fn inspect_jdk_dir(candidate: &Path, source: JdkSource) -> Option<JdkInstallation> {
    let home_dir = normalize_jdk_home(candidate)?;
    let canonical_home = fs::canonicalize(&home_dir).unwrap_or(home_dir);
    let java_bin = canonical_home.join("bin").join(java_executable_name());
    if !is_executable_file(&java_bin) {
        return None;
    }

    let release_path = canonical_home.join("release");
    let release_contents = fs::read_to_string(&release_path).ok()?;
    let dir_name = canonical_home
        .file_name()
        .and_then(|n| n.to_str())
        .or_else(|| candidate.file_name().and_then(|n| n.to_str()));
    let meta = parse_release_file(&release_contents, dir_name).ok()?;

    let jmods_candidate = canonical_home.join("jmods");
    let jmods_dir = jmods_candidate.is_dir().then_some(jmods_candidate);

    Some(JdkInstallation {
        home_dir: canonical_home,
        java_bin,
        jmods_dir,
        version: meta.version,
        vendor: meta.vendor,
        arch: meta.arch,
        source,
    })
}

/// Selects the best JDK for a project according to [`EffectiveConfig`].
///
/// Precedence:
/// 1. `kova.toml` (`project.jdk`) if set
/// 2. `settings.toml` (`settings.jvm.preferred_jdk`) if set
/// 3. Any discovered JDK, ordered by [`JdkSource`] priority then highest [`JavaVersion`].
///
/// # Errors
/// Returns [`ToolchainError::InvalidJavaVersion`] if the configured requirement is malformed,
/// or [`ToolchainError::JdkNotFound`] if no installed JDK satisfies it.
pub fn select_project_jdk<'a>(
    installations: &'a [JdkInstallation],
    config: &EffectiveConfig,
) -> Result<&'a JdkInstallation, ToolchainError> {
    let raw_spec = config.settings.jvm.preferred_jdk.as_deref();

    let requirement = match raw_spec {
        Some(spec) => parse_jdk_requirement(spec)?,
        None => JdkRequirement::Any,
    };

    select_matching_jdk(installations, &requirement)
}

/// Selects a JDK suitable for running Kova's own JVM tools (such as `jdt.ls` requiring `>= 21`).
///
/// # Errors
/// Returns [`ToolchainError::JdkNotFound`] if no JDK is available, or
/// [`ToolchainError::UnsupportedJdkRuntime`] if all available JDKs are older than `min_major`.
pub fn select_tool_runtime_jdk(
    installations: &[JdkInstallation],
    min_major: u16,
) -> Result<&JdkInstallation, ToolchainError> {
    if installations.is_empty() {
        return Err(ToolchainError::JdkNotFound {
            requirement: format!(">={min_major}"),
        });
    }

    let req = JdkRequirement::MinMajor(min_major);
    if let Ok(found) = select_matching_jdk(installations, &req) {
        return Ok(found);
    }

    // All installed JDKs are below `min_major`; report the highest found version.
    let highest = installations
        .iter()
        .max_by_key(|jdk| jdk.version)
        .ok_or_else(|| ToolchainError::JdkNotFound {
            requirement: format!(">={min_major}"),
        })?;

    Err(ToolchainError::UnsupportedJdkRuntime {
        path: highest.home_dir.display().to_string(),
        found_major: highest.version.major,
        min_major,
    })
}

fn select_matching_jdk<'a>(
    installations: &'a [JdkInstallation],
    requirement: &JdkRequirement,
) -> Result<&'a JdkInstallation, ToolchainError> {
    installations
        .iter()
        .filter(|jdk| requirement.matches(jdk.version, &jdk.vendor, &jdk.home_dir))
        .min_by(|a, b| {
            a.source
                .cmp(&b.source)
                .then_with(|| b.version.cmp(&a.version))
        })
        .ok_or_else(|| ToolchainError::JdkNotFound {
            requirement: requirement.to_string(),
        })
}

fn scan_child_dirs(
    root: &Path,
    source: JdkSource,
    skip_sdkman_current: bool,
    seen_homes: &mut HashSet<PathBuf>,
    results: &mut Vec<JdkInstallation>,
) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let mut children: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    children.sort();

    for child in children {
        if skip_sdkman_current
            && child
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == "current")
        {
            continue;
        }
        try_add_candidate(&child, source, seen_homes, results);
    }
}

fn scan_homebrew_openjdk_dirs(
    opt_root: &Path,
    seen_homes: &mut HashSet<PathBuf>,
    results: &mut Vec<JdkInstallation>,
) {
    let Ok(entries) = fs::read_dir(opt_root) else {
        return;
    };
    let mut children: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("openjdk"))
        })
        .collect();
    children.sort();

    for child in children {
        let libexec_bundle = child.join("libexec").join("openjdk.jdk");
        try_add_candidate(&libexec_bundle, JdkSource::SystemMacOs, seen_homes, results);
        try_add_candidate(&child, JdkSource::SystemMacOs, seen_homes, results);
    }
}

fn try_add_candidate(
    candidate: &Path,
    source: JdkSource,
    seen_homes: &mut HashSet<PathBuf>,
    results: &mut Vec<JdkInstallation>,
) {
    if let Some(jdk) = inspect_jdk_dir(candidate, source)
        && seen_homes.insert(jdk.home_dir.clone())
    {
        results.push(jdk);
    }
}

fn normalize_jdk_home(candidate: &Path) -> Option<PathBuf> {
    let macos_home = candidate.join("Contents").join("Home");
    if macos_home.join("bin").join(java_executable_name()).exists() {
        return Some(macos_home);
    }
    if candidate.join("bin").join(java_executable_name()).exists() {
        return Some(candidate.to_path_buf());
    }
    None
}

const fn java_executable_name() -> &'static str {
    if cfg!(windows) { "java.exe" } else { "java" }
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
