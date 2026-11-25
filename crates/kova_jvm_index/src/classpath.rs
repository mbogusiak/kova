//! Non-invasive discovery of project libraries, Maven/Gradle caches, and JDK modules.
//!
//! Never spawns `gradle`, `mvn`, or `java` subprocesses.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::error::JvmIndexError;

/// Default maximum number of `.jar`/`.jmod` archives returned by [`discover_classpath`].
pub const DEFAULT_MAX_JAR_COUNT: usize = 2_000;

/// Origin category of a discovered classpath archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ClasspathOrigin {
    /// Explicitly supplied by BSP, `kova_toolchain`, or caller.
    Explicit,
    /// Compiled project output directory (`target/classes`, `build/classes`).
    ProjectOutput,
    /// Project-local `<project_root>/libs/*.jar`.
    ProjectLocalLibs,
    /// Local Gradle module cache.
    GradleCache,
    /// Local Maven repository (`~/.m2/repository`).
    MavenLocalRepo,
    /// JDK modules (`<JAVA_HOME>/jmods/*.jmod` or `rt.jar`).
    JdkModules,
}

/// A discovered `.jar` or `.jmod` archive on the classpath.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClasspathEntry {
    /// Path to the archive on disk.
    pub path: PathBuf,
    /// Origin category of this archive.
    pub origin: ClasspathOrigin,
    /// File size in bytes.
    pub size_bytes: u64,
    /// Last modification timestamp in nanoseconds since `UNIX_EPOCH`.
    pub modified_unix_ns: u64,
}

/// Configuration for [`discover_classpath`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClasspathDiscoveryConfig {
    /// Explicitly provided `.jar` / `.jmod` paths.
    pub explicit_jars: Vec<PathBuf>,
    /// Whether to scan `<project_root>/libs` for `.jar` files.
    pub scan_project_libs: bool,
    /// Optional `JAVA_HOME` directory to scan `jmods/*.jmod` or `jre/lib/rt.jar`.
    pub scan_jdk_home: Option<PathBuf>,
    /// Optional Gradle cache directory.
    pub gradle_cache_dir: Option<PathBuf>,
    /// Optional Maven local repository directory.
    pub maven_repo_dir: Option<PathBuf>,
    /// Maximum number of archives to return.
    pub max_jar_count: usize,
}

impl Default for ClasspathDiscoveryConfig {
    fn default() -> Self {
        Self {
            explicit_jars: Vec::new(),
            scan_project_libs: true,
            scan_jdk_home: None,
            gradle_cache_dir: None,
            maven_repo_dir: None,
            max_jar_count: DEFAULT_MAX_JAR_COUNT,
        }
    }
}

/// Discovers `.jar` and `.jmod` archives for `project_root` without spawning any subprocesses.
///
/// Skips `*-sources.jar` and `*-javadoc.jar`, deduplicates by canonical path, and sorts
/// deterministically by `(origin, path)`.
///
/// # Errors
///
/// Returns [`JvmIndexError`] if an I/O error prevents reading configured directories.
pub fn discover_classpath(
    project_root: &Path,
    config: &ClasspathDiscoveryConfig,
) -> Result<Vec<ClasspathEntry>, JvmIndexError> {
    let mut entries = Vec::new();
    let mut seen_paths = HashSet::new();

    for jar in &config.explicit_jars {
        push_if_valid_archive(
            jar,
            ClasspathOrigin::Explicit,
            &mut seen_paths,
            &mut entries,
        );
    }

    if config.scan_project_libs {
        let libs_dir = project_root.join("libs");
        if libs_dir.is_dir() {
            collect_archives_recursive(
                &libs_dir,
                ClasspathOrigin::ProjectLocalLibs,
                &mut seen_paths,
                &mut entries,
                config.max_jar_count,
            );
        }
    }

    if let Some(jdk_home) = &config.scan_jdk_home {
        let jmods = jdk_home.join("jmods");
        if jmods.is_dir() {
            collect_archives_recursive(
                &jmods,
                ClasspathOrigin::JdkModules,
                &mut seen_paths,
                &mut entries,
                config.max_jar_count,
            );
        }
        let rt_jar = jdk_home.join("jre").join("lib").join("rt.jar");
        if rt_jar.is_file() {
            push_if_valid_archive(
                &rt_jar,
                ClasspathOrigin::JdkModules,
                &mut seen_paths,
                &mut entries,
            );
        }
    }

    if let Some(gradle_dir) = &config.gradle_cache_dir {
        if gradle_dir.is_dir() {
            collect_archives_recursive(
                gradle_dir,
                ClasspathOrigin::GradleCache,
                &mut seen_paths,
                &mut entries,
                config.max_jar_count,
            );
        }
    }

    if let Some(maven_dir) = &config.maven_repo_dir {
        if maven_dir.is_dir() {
            collect_archives_recursive(
                maven_dir,
                ClasspathOrigin::MavenLocalRepo,
                &mut seen_paths,
                &mut entries,
                config.max_jar_count,
            );
        }
    }

    entries.sort_by(|a, b| a.origin.cmp(&b.origin).then_with(|| a.path.cmp(&b.path)));
    entries.truncate(config.max_jar_count);
    Ok(entries)
}

fn is_ignored_auxiliary_jar(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    lower.ends_with("-sources.jar") || lower.ends_with("-javadoc.jar")
}

fn is_archive_file(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    (ext.eq_ignore_ascii_case("jar") || ext.eq_ignore_ascii_case("jmod"))
        && !is_ignored_auxiliary_jar(path)
}

fn push_if_valid_archive(
    path: &Path,
    origin: ClasspathOrigin,
    seen: &mut HashSet<PathBuf>,
    out: &mut Vec<ClasspathEntry>,
) {
    if !path.is_file() || !is_archive_file(path) {
        return;
    }
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if !seen.insert(canonical) {
        return;
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    let modified_unix_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .and_then(|d| u64::try_from(d.as_nanos()).ok())
        .unwrap_or(0);
    out.push(ClasspathEntry {
        path: path.to_path_buf(),
        origin,
        size_bytes: meta.len(),
        modified_unix_ns,
    });
}

fn collect_archives_recursive(
    dir: &Path,
    origin: ClasspathOrigin,
    seen: &mut HashSet<PathBuf>,
    out: &mut Vec<ClasspathEntry>,
    max_count: usize,
) {
    if out.len() >= max_count {
        return;
    }
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read_dir.flatten() {
        if out.len() >= max_count {
            break;
        }
        let path = entry.path();
        if path.is_dir() {
            collect_archives_recursive(&path, origin, seen, out, max_count);
        } else {
            push_if_valid_archive(&path, origin, seen, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn discovers_libs_jdk_and_skips_sources_javadoc_jars() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("libs")).expect("mkdir libs");
        std::fs::create_dir_all(root.join("jdk/jmods")).expect("mkdir jmods");

        std::fs::write(root.join("libs/app-lib.jar"), b"PK").expect("write lib");
        std::fs::write(root.join("libs/app-lib-sources.jar"), b"PK").expect("write sources");
        std::fs::write(root.join("libs/app-lib-javadoc.jar"), b"PK").expect("write javadoc");
        std::fs::write(root.join("jdk/jmods/java.base.jmod"), b"JM").expect("write jmod");

        let cfg = ClasspathDiscoveryConfig {
            scan_project_libs: true,
            scan_jdk_home: Some(root.join("jdk")),
            ..ClasspathDiscoveryConfig::default()
        };
        let entries = discover_classpath(root, &cfg).expect("discover classpath");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].origin, ClasspathOrigin::ProjectLocalLibs);
        assert_eq!(entries[1].origin, ClasspathOrigin::JdkModules);
    }
}
