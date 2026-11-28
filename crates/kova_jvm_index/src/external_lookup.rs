//! Lazy central-directory index for external `.jar` dependencies and unified
//! JDK + dependency class bytecode locator (`KOVA-061`).
//!
//! Never spawns `gradle`, `mvn`, or `java` subprocesses.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, PoisonError, RwLock};
use std::time::UNIX_EPOCH;

use zip::ZipArchive;

use crate::bytecode::{
    DEFAULT_MAX_CLASS_BYTES, DEFAULT_TARGET_JAVA_VERSION, JvmTypeSummary, summarize_class_bytes,
};
use crate::classpath::ClasspathOrigin;
use crate::jdk_lookup::{
    JdkOnDemandIndex, LocatedClassBytes, fqcn_entry_candidates, is_completable_type_name,
    read_zip_entry_bounded,
};
use crate::model::is_ignored_internal_name;

/// Default upper bound on `.jar` archives indexed by [`DependencyArchiveIndex`].
pub const DEFAULT_MAX_DEPENDENCY_JARS: usize = 4_000;

/// Configuration for [`DependencyArchiveIndex`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyArchiveConfig {
    /// Optional project root directory (scans `<project_root>/libs/**/*.jar`).
    pub project_root: Option<PathBuf>,
    /// Explicitly supplied `.jar` paths.
    pub explicit_jars: Vec<PathBuf>,
    /// Optional Gradle cache root (`~/.gradle/caches/modules-2/files-2.1`).
    pub gradle_cache_dir: Option<PathBuf>,
    /// Optional Maven local repository root (`~/.m2/repository`).
    pub maven_repo_dir: Option<PathBuf>,
    /// Maximum number of `.jar` archives indexed after priority/mtime sorting.
    pub max_jar_count: usize,
    /// Target Java release version for Multi-Release JAR (`META-INF/versions/<N>/`) entries.
    pub target_java_version: u16,
}

impl Default for DependencyArchiveConfig {
    fn default() -> Self {
        Self {
            project_root: None,
            explicit_jars: Vec::new(),
            gradle_cache_dir: None,
            maven_repo_dir: None,
            max_jar_count: DEFAULT_MAX_DEPENDENCY_JARS,
            target_java_version: DEFAULT_TARGET_JAVA_VERSION,
        }
    }
}

impl DependencyArchiveConfig {
    /// Builds a configuration pointing at `<project_root>/libs` and standard user-home
    /// Gradle (`~/.gradle/caches/modules-2/files-2.1`) and Maven (`~/.m2/repository`) caches.
    #[must_use]
    pub fn from_host_env(project_root: Option<PathBuf>) -> Self {
        let home = user_home_dir();
        Self::from_home_and_project(home.as_deref(), project_root)
    }

    /// Builds a configuration rooted at an explicit `home` directory (useful in isolated tests).
    #[must_use]
    pub fn from_home_and_project(home: Option<&Path>, project_root: Option<PathBuf>) -> Self {
        let gradle_cache_dir = home
            .map(|h| h.join(".gradle/caches/modules-2/files-2.1"))
            .filter(|p| p.is_dir());
        let maven_repo_dir = home
            .map(|h| h.join(".m2/repository"))
            .filter(|p| p.is_dir());
        Self {
            project_root,
            explicit_jars: Vec::new(),
            gradle_cache_dir,
            maven_repo_dir,
            max_jar_count: DEFAULT_MAX_DEPENDENCY_JARS,
            target_java_version: DEFAULT_TARGET_JAVA_VERSION,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ArchiveClassRef {
    jar_path: PathBuf,
    entry_name: String,
    jar_rank: usize,
    mr_version: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiscoveredJar {
    path: PathBuf,
    priority: u8,
    modified_unix_ns: u64,
}

/// Lazy, central-directory-only index mapping logical `.class` paths (`com/example/Foo.class`)
/// to their winning `.jar` archive and entry name (`O(1)` lookup after initialization).
#[derive(Debug, Default)]
pub struct DependencyArchiveIndex {
    config: DependencyArchiveConfig,
    entries: OnceLock<HashMap<String, ArchiveClassRef>>,
    type_names: OnceLock<Vec<String>>,
    summaries: RwLock<HashMap<String, Option<JvmTypeSummary>>>,
}

impl DependencyArchiveIndex {
    /// Creates an empty index with no dependency directories configured.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Creates a lazy dependency index from `config`.
    ///
    /// No disk scanning or ZIP reading occurs until the first lookup.
    #[must_use]
    pub fn new(config: DependencyArchiveConfig) -> Self {
        Self {
            config,
            ..Self::default()
        }
    }

    /// Whether the `.jar` central directories have already been scanned (lookups won't block).
    #[must_use]
    pub fn is_warm(&self) -> bool {
        self.entries.get().is_some()
    }

    /// Scans the configured `.jar` central directories now (call from a background thread).
    pub fn warm(&self) {
        let _ = self.get_or_init_entries();
    }

    /// Top-level completable type FQCNs found in the dependency `.jar`s (`org.javers.core.Javers`),
    /// initializing the index if needed.
    #[must_use]
    pub fn type_names(&self) -> &[String] {
        self.type_names.get_or_init(|| {
            let mut names: Vec<String> = self
                .get_or_init_entries()
                .keys()
                .filter_map(|k| k.strip_suffix(".class"))
                .map(|internal| internal.replace('/', "."))
                .filter(|fqcn| is_completable_type_name(fqcn))
                .collect();
            names.sort_unstable();
            names
        })
    }

    /// Cached class-header summary of `fqcn` from the dependency `.jar`s.
    #[must_use]
    pub fn type_summary(&self, fqcn: &str) -> Option<JvmTypeSummary> {
        if let Some(hit) = self
            .summaries
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(fqcn)
        {
            return hit.clone();
        }
        let summary = self
            .locate_class_bytes(fqcn)
            .and_then(|located| summarize_class_bytes(&located.bytes));
        self.summaries
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(fqcn.to_owned(), summary.clone());
        summary
    }

    /// Returns the number of indexed `.class` entries, initializing the index if needed.
    #[must_use]
    pub fn entry_count(&self) -> usize {
        self.get_or_init_entries().len()
    }

    /// Locates and reads raw `.class` bytes for `fqcn` from the indexed `.jar` archives.
    #[must_use]
    pub fn locate_class_bytes(&self, fqcn: &str) -> Option<LocatedClassBytes> {
        let map = self.get_or_init_entries();
        if map.is_empty() {
            return None;
        }
        for rel_internal in fqcn_entry_candidates(fqcn) {
            let logical_key = format!("{rel_internal}.class");
            let Some(class_ref) = map.get(&logical_key) else {
                continue;
            };
            let Ok(file) = File::open(&class_ref.jar_path) else {
                continue;
            };
            let Ok(mut archive) = ZipArchive::new(BufReader::new(file)) else {
                continue;
            };
            let bytes = {
                let Ok(mut entry) = archive.by_name(&class_ref.entry_name) else {
                    continue;
                };
                read_zip_entry_bounded(&mut entry, DEFAULT_MAX_CLASS_BYTES)?
            };

            let outer_entry_name = if let Some((outer_internal, _)) = rel_internal.split_once('$') {
                let outer_key = format!("{outer_internal}.class");
                map.get(&outer_key)
                    .filter(|outer_ref| outer_ref.jar_path == class_ref.jar_path)
                    .map_or_else(|| class_ref.entry_name.clone(), |r| r.entry_name.clone())
            } else {
                class_ref.entry_name.clone()
            };

            return Some(LocatedClassBytes {
                fqcn: rel_internal.replace('/', "."),
                archive_path: class_ref.jar_path.clone(),
                entry_name: class_ref.entry_name.clone(),
                outer_entry_name,
                bytes,
            });
        }
        None
    }

    fn get_or_init_entries(&self) -> &HashMap<String, ArchiveClassRef> {
        self.entries
            .get_or_init(|| build_dependency_entry_map(&self.config))
    }
}

/// Unified class bytecode locator combining [`JdkOnDemandIndex`] and [`DependencyArchiveIndex`].
#[derive(Debug, Clone)]
pub struct ExternalClassLocator {
    jdk_index: Arc<JdkOnDemandIndex>,
    dependency_index: Arc<DependencyArchiveIndex>,
}

impl ExternalClassLocator {
    /// Creates a new locator over `jdk_index` and `dependency_index`.
    #[must_use]
    pub fn new(
        jdk_index: Arc<JdkOnDemandIndex>,
        dependency_index: Arc<DependencyArchiveIndex>,
    ) -> Self {
        Self {
            jdk_index,
            dependency_index,
        }
    }

    /// Reference to the underlying JDK on-demand index.
    #[must_use]
    pub fn jdk_index(&self) -> &JdkOnDemandIndex {
        &self.jdk_index
    }

    /// Reference to the underlying dependency `.jar` archive index.
    #[must_use]
    pub fn dependency_index(&self) -> &DependencyArchiveIndex {
        &self.dependency_index
    }

    /// Locates the first matching class across `candidates` following JDK vs dependency priority.
    ///
    /// Standard JDK package prefixes (`java.*`, `javax.*`, `jdk.*`, `sun.*`, `org.w3c.dom.*`,
    /// `org.xml.sax.*`, `org.ietf.jgss.*`) probe the JDK index first; all other candidates probe
    /// dependency `.jar` archives first and fall back to the JDK index.
    #[must_use]
    pub fn locate_candidates(&self, candidates: &[String]) -> Option<LocatedClassBytes> {
        for fqcn in candidates {
            if is_jdk_package_candidate(fqcn) {
                if let Some(hit) = self.jdk_index.locate_class_bytes(fqcn) {
                    return Some(hit);
                }
                if let Some(hit) = self.dependency_index.locate_class_bytes(fqcn) {
                    return Some(hit);
                }
            } else {
                if let Some(hit) = self.dependency_index.locate_class_bytes(fqcn) {
                    return Some(hit);
                }
                if let Some(hit) = self.jdk_index.locate_class_bytes(fqcn) {
                    return Some(hit);
                }
            }
        }
        None
    }
}

/// Returns `true` when `fqcn` belongs to a standard JDK module package prefix.
#[must_use]
pub fn is_jdk_package_candidate(fqcn: &str) -> bool {
    fqcn.starts_with("java.")
        || fqcn.starts_with("javax.")
        || fqcn.starts_with("jdk.")
        || fqcn.starts_with("sun.")
        || fqcn.starts_with("org.w3c.dom.")
        || fqcn.starts_with("org.xml.sax.")
        || fqcn.starts_with("org.ietf.jgss.")
}

fn build_dependency_entry_map(
    config: &DependencyArchiveConfig,
) -> HashMap<String, ArchiveClassRef> {
    let jars = collect_prioritized_jars(config);
    let mut map: HashMap<String, ArchiveClassRef> = HashMap::new();

    for (jar_rank, jar) in jars.into_iter().enumerate() {
        let Ok(file) = File::open(&jar.path) else {
            continue;
        };
        let Ok(archive) = ZipArchive::new(BufReader::new(file)) else {
            continue;
        };
        for raw_name in archive.file_names() {
            if raw_name.ends_with('/')
                || !Path::new(raw_name)
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("class"))
            {
                continue;
            }
            let Some((logical_path, mr_version)) =
                classify_jar_class_entry(raw_name, config.target_java_version)
            else {
                continue;
            };
            let internal = logical_path.strip_suffix(".class").unwrap_or(logical_path);
            if is_ignored_internal_name(internal) {
                continue;
            }
            match map.get_mut(logical_path) {
                None => {
                    map.insert(
                        logical_path.to_owned(),
                        ArchiveClassRef {
                            jar_path: jar.path.clone(),
                            entry_name: raw_name.to_owned(),
                            jar_rank,
                            mr_version,
                        },
                    );
                }
                Some(existing)
                    if existing.jar_rank == jar_rank && mr_version > existing.mr_version =>
                {
                    *existing = ArchiveClassRef {
                        jar_path: jar.path.clone(),
                        entry_name: raw_name.to_owned(),
                        jar_rank,
                        mr_version,
                    };
                }
                Some(_) => {}
            }
        }
    }

    map
}

fn collect_prioritized_jars(config: &DependencyArchiveConfig) -> Vec<DiscoveredJar> {
    let mut jars = Vec::new();
    let mut seen = HashSet::new();

    if let Some(root) = &config.project_root {
        let libs_dir = root.join("libs");
        if libs_dir.is_dir() {
            collect_jars_recursive(
                &libs_dir,
                origin_priority(ClasspathOrigin::ProjectLocalLibs),
                &mut seen,
                &mut jars,
            );
        }
    }

    let explicit_prio = origin_priority(ClasspathOrigin::Explicit);
    for jar in &config.explicit_jars {
        push_if_valid_jar(jar, explicit_prio, &mut seen, &mut jars);
    }

    if let Some(gradle_dir) = &config.gradle_cache_dir
        && gradle_dir.is_dir()
    {
        collect_jars_recursive(
            gradle_dir,
            origin_priority(ClasspathOrigin::GradleCache),
            &mut seen,
            &mut jars,
        );
    }

    if let Some(maven_dir) = &config.maven_repo_dir
        && maven_dir.is_dir()
    {
        collect_jars_recursive(
            maven_dir,
            origin_priority(ClasspathOrigin::MavenLocalRepo),
            &mut seen,
            &mut jars,
        );
    }

    jars.sort_by(|a, b| {
        a.priority
            .cmp(&b.priority)
            .then_with(|| Reverse(a.modified_unix_ns).cmp(&Reverse(b.modified_unix_ns)))
            .then_with(|| a.path.cmp(&b.path))
    });
    jars.truncate(config.max_jar_count);
    jars
}

const fn origin_priority(origin: ClasspathOrigin) -> u8 {
    match origin {
        ClasspathOrigin::ProjectLocalLibs => 0,
        ClasspathOrigin::Explicit => 1,
        ClasspathOrigin::ProjectOutput => 2,
        ClasspathOrigin::GradleCache => 3,
        ClasspathOrigin::MavenLocalRepo => 4,
        ClasspathOrigin::JdkModules => 5,
    }
}

fn is_valid_dependency_jar(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("jar"))
        && !lower.ends_with("-sources.jar")
        && !lower.ends_with("-javadoc.jar")
}

fn push_if_valid_jar(
    path: &Path,
    priority: u8,
    seen: &mut HashSet<PathBuf>,
    out: &mut Vec<DiscoveredJar>,
) {
    if !path.is_file() || !is_valid_dependency_jar(path) {
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
    out.push(DiscoveredJar {
        path: path.to_path_buf(),
        priority,
        modified_unix_ns,
    });
}

fn collect_jars_recursive(
    dir: &Path,
    priority: u8,
    seen: &mut HashSet<PathBuf>,
    out: &mut Vec<DiscoveredJar>,
) {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jars_recursive(&path, priority, seen, out);
        } else {
            push_if_valid_jar(&path, priority, seen, out);
        }
    }
}

fn classify_jar_class_entry(entry_name: &str, target_java_version: u16) -> Option<(&str, u16)> {
    if let Some(rest) = entry_name.strip_prefix("META-INF/versions/") {
        let (ver_str, logical) = rest.split_once('/')?;
        if logical.is_empty() {
            return None;
        }
        let version = ver_str.parse::<u16>().ok()?;
        if version == 0 || version > target_java_version {
            return None;
        }
        Some((logical, version))
    } else if entry_name.starts_with("META-INF/") {
        None
    } else {
        Some((entry_name, 0))
    }
}

fn user_home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use tempfile::TempDir;
    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    use super::*;

    fn write_test_archive(path: &Path, entries: &[(&str, &[u8])]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        let file = File::create(path).expect("create archive");
        let mut zip = ZipWriter::new(file);
        let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, data) in entries {
            zip.start_file(*name, opts).expect("start file");
            zip.write_all(data).expect("write bytes");
        }
        zip.finish().expect("finish zip");
    }

    #[test]
    fn jdk_index_locates_jmod_and_nested_outer_entries() {
        let dir = TempDir::new().expect("tempdir");
        let jmods_dir = dir.path().join("jmods");
        write_test_archive(
            &jmods_dir.join("java.base.jmod"),
            &[
                ("classes/java/util/Map.class", b"OUTER_BYTES"),
                ("classes/java/util/Map$Entry.class", b"INNER_BYTES"),
            ],
        );

        let jdk = JdkOnDemandIndex::from_jmods_dir(jmods_dir);
        let located = jdk
            .locate_class_bytes("java.util.Map.Entry")
            .expect("must find nested entry");
        assert_eq!(located.fqcn, "java.util.Map$Entry");
        assert_eq!(located.entry_name, "classes/java/util/Map$Entry.class");
        assert_eq!(located.outer_entry_name, "classes/java/util/Map.class");
        assert_eq!(located.bytes, b"INNER_BYTES");
    }

    #[test]
    fn dependency_index_handles_gradle_maven_libs_mrjar_and_skips_aux_jars() {
        let dir = TempDir::new().expect("tempdir");
        let project = dir.path().join("proj");
        let gradle = dir.path().join("gradle");
        let maven = dir.path().join("m2");

        // Sources & javadoc jars must be ignored
        write_test_archive(
            &project.join("libs/helper-sources.jar"),
            &[("com/example/Helper.class", b"BAD_SOURCES")],
        );
        write_test_archive(
            &project.join("libs/helper-javadoc.jar"),
            &[("com/example/Helper.class", b"BAD_JAVADOC")],
        );
        // Project libs overrides Gradle/Maven
        write_test_archive(
            &project.join("libs/helper.jar"),
            &[("com/example/Helper.class", b"LIBS_HELPER")],
        );
        // Gradle cache with Multi-Release entry
        write_test_archive(
            &gradle.join("com.acme/acme-core/1.0/abc/acme-core-1.0.jar"),
            &[
                ("com/acme/Acme.class", b"ACME_BASE"),
                ("META-INF/versions/17/com/acme/Acme.class", b"ACME_V17"),
                ("META-INF/versions/99/com/acme/Acme.class", b"ACME_FUTURE"),
                ("com/example/Helper.class", b"GRADLE_HELPER"),
            ],
        );
        // Maven repo
        write_test_archive(
            &maven.join("org/sample/sample-api/2.0/sample-api-2.0.jar"),
            &[
                ("org/sample/Outer.class", b"OUTER"),
                ("org/sample/Outer$Inner.class", b"INNER"),
            ],
        );

        let dep_idx = DependencyArchiveIndex::new(DependencyArchiveConfig {
            project_root: Some(project),
            gradle_cache_dir: Some(gradle),
            maven_repo_dir: Some(maven),
            ..DependencyArchiveConfig::default()
        });

        let helper = dep_idx
            .locate_class_bytes("com.example.Helper")
            .expect("helper");
        assert_eq!(helper.bytes, b"LIBS_HELPER");

        let acme = dep_idx.locate_class_bytes("com.acme.Acme").expect("acme");
        assert_eq!(acme.bytes, b"ACME_V17");
        assert_eq!(acme.entry_name, "META-INF/versions/17/com/acme/Acme.class");

        let inner = dep_idx
            .locate_class_bytes("org.sample.Outer.Inner")
            .expect("inner");
        assert_eq!(inner.fqcn, "org.sample.Outer$Inner");
        assert_eq!(inner.outer_entry_name, "org/sample/Outer.class");
        assert_eq!(inner.bytes, b"INNER");
    }

    #[test]
    fn dependency_type_names_skip_nested_and_warm_flag_tracks_scan() {
        let dir = TempDir::new().expect("tempdir");
        let project = dir.path().join("proj");
        write_test_archive(
            &project.join("libs/javers-core.jar"),
            &[
                ("org/javers/core/graph/ObjectAccessHook.class", b"X"),
                ("org/javers/core/graph/ObjectAccessor$1.class", b"X"),
                ("org/javers/core/package-info.class", b"X"),
            ],
        );
        let dep_idx = DependencyArchiveIndex::new(DependencyArchiveConfig {
            project_root: Some(project),
            gradle_cache_dir: None,
            maven_repo_dir: None,
            ..DependencyArchiveConfig::default()
        });
        assert!(!dep_idx.is_warm());
        dep_idx.warm();
        assert!(dep_idx.is_warm());
        assert_eq!(
            dep_idx.type_names(),
            ["org.javers.core.graph.ObjectAccessHook".to_owned()]
        );
        // Not a valid classfile: no summary, cached as `None`.
        assert!(
            dep_idx
                .type_summary("org.javers.core.graph.ObjectAccessHook")
                .is_none()
        );
    }

    #[test]
    fn locator_prioritizes_jdk_for_java_packages_and_deps_otherwise() {
        let dir = TempDir::new().expect("tempdir");
        let jmods = dir.path().join("jmods");
        write_test_archive(
            &jmods.join("java.base.jmod"),
            &[("classes/java/lang/String.class", b"JDK_STRING")],
        );
        let project = dir.path().join("proj");
        write_test_archive(
            &project.join("libs/ext.jar"),
            &[
                ("com/example/Lib.class", b"DEP_LIB"),
                ("javax/inject/Inject.class", b"DEP_INJECT"),
            ],
        );

        let locator = ExternalClassLocator::new(
            Arc::new(JdkOnDemandIndex::from_jmods_dir(jmods)),
            Arc::new(DependencyArchiveIndex::new(DependencyArchiveConfig {
                project_root: Some(project),
                ..DependencyArchiveConfig::default()
            })),
        );

        let s = locator
            .locate_candidates(&["com.app.String".to_owned(), "java.lang.String".to_owned()])
            .expect("jdk string");
        assert_eq!(s.bytes, b"JDK_STRING");

        let lib = locator
            .locate_candidates(&["com.example.Lib".to_owned()])
            .expect("dep lib");
        assert_eq!(lib.bytes, b"DEP_LIB");

        // `javax.inject.Inject` is not in JDK jmods, so locator falls back to dependency jar.
        let inj = locator
            .locate_candidates(&["javax.inject.Inject".to_owned()])
            .expect("dep inject");
        assert_eq!(inj.bytes, b"DEP_INJECT");
    }
}
