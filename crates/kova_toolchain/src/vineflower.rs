//! On-demand `.class` decompilation through Vineflower with a persistent source cache
//! in `~/.cache/kova/decompiled/<artifact_key>/<package path>/<Outer>.java`.

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{Cursor, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use kova_config::KovaPaths;
use sha2::{Digest, Sha256};
use zip::ZipArchive;

use crate::error::ToolchainError;
use crate::jdk::{JdkInstallation, select_tool_runtime_jdk};
use crate::omakase::installer::{OmakaseManager, has_extension, unique_suffix};
use crate::omakase::manifest::{OmakaseManifest, OmakaseTool};

/// Header preceding the ZIP stream in JDK `.jmod` files.
const JMOD_MAGIC: [u8; 4] = [b'J', b'M', 1, 0];
/// Upper bound of class bytes extracted for a single decompilation.
const MAX_EXTRACTED_CLASS_BYTES: u64 = 64 * 1024 * 1024;
/// Upper bound of captured stderr included in errors.
const MAX_STDERR_CHARS: usize = 4096;

/// Lowercase hex digits for cache keys.
const HEX: &[u8; 16] = b"0123456789abcdef";

static WORK_COUNTER: AtomicU64 = AtomicU64::new(0);

/// What to decompile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecompileTarget {
    /// A single `.class` file on disk.
    ClassFile {
        /// Path to the `.class` file.
        class_path: PathBuf,
    },
    /// A class inside a `.jar` or `.jmod` archive (e.g. `"java/util/ArrayList.class"`).
    ArchiveEntry {
        /// Path to the archive.
        archive_path: PathBuf,
        /// `/`-separated entry name; a `classes/` prefix is tried automatically for `.jmod`.
        entry_name: String,
    },
}

/// Result of a decompilation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecompileResult {
    /// Fully qualified name of the top-level class whose source was produced
    /// (inner classes resolve to their outer class).
    pub fqcn: String,
    /// Decompiled Java source.
    pub source_code: String,
    /// Persistent cache file holding `source_code`.
    pub cache_file: PathBuf,
    /// `true` when served from the cache without running the JVM.
    pub from_cache: bool,
}

/// Vineflower runner with a persistent per-artifact cache.
#[derive(Debug, Clone)]
pub struct VineflowerDecompiler {
    vineflower_jar: PathBuf,
    runtime_jdk: JdkInstallation,
    cache_root: PathBuf,
    timeout: Duration,
}

struct ClassPlan {
    source_path: PathBuf,
    package: Vec<String>,
    outer: String,
    /// Requested class file stem (e.g. `Outer$Inner`).
    stem: String,
    cache_file: PathBuf,
}

impl VineflowerDecompiler {
    /// Default hard timeout of a single decompilation process.
    pub const DEFAULT_DECOMPILE_TIMEOUT: Duration = Duration::from_secs(15);

    /// Creates a decompiler writing its cache under `cache_root`.
    #[must_use]
    pub fn new(vineflower_jar: PathBuf, runtime_jdk: JdkInstallation, cache_root: PathBuf) -> Self {
        Self {
            vineflower_jar,
            runtime_jdk,
            cache_root,
            timeout: Self::DEFAULT_DECOMPILE_TIMEOUT,
        }
    }

    /// Creates a decompiler caching into `paths.cache_dir()/decompiled`.
    #[must_use]
    pub fn for_paths(
        paths: &KovaPaths,
        vineflower_jar: PathBuf,
        runtime_jdk: JdkInstallation,
    ) -> Self {
        Self::new(
            vineflower_jar,
            runtime_jdk,
            paths.cache_dir().join("decompiled"),
        )
    }

    /// Overrides the process timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Root of the decompiled-source cache.
    #[must_use]
    pub fn cache_root(&self) -> &Path {
        &self.cache_root
    }

    /// Decompiles `target`, serving the persistent cache when possible.
    ///
    /// # Errors
    /// Returns [`ToolchainError::PathTraversalBlocked`] for unsafe entry names,
    /// [`ToolchainError::ArchiveExtraction`] for unreadable archives or missing entries,
    /// [`ToolchainError::ProcessFailed`] on process failure or timeout, and
    /// [`ToolchainError::Io`] on filesystem errors.
    pub async fn decompile(
        &self,
        target: &DecompileTarget,
    ) -> Result<DecompileResult, ToolchainError> {
        let plan = self.plan(target)?;
        if plan.cache_file.is_file() {
            let source_code = fs::read_to_string(&plan.cache_file)
                .map_err(|err| ToolchainError::io(plan.cache_file.display().to_string(), &err))?;
            return Ok(DecompileResult {
                fqcn: fqcn_for(&plan, &source_code),
                source_code,
                cache_file: plan.cache_file,
                from_cache: true,
            });
        }

        let work = self.cache_root.join(format!(
            ".work-{}-{}",
            unique_suffix(),
            WORK_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let result = self.decompile_in(target, &plan, &work).await;
        fs::remove_dir_all(&work).ok();
        let source_code = result?;

        write_atomic(&plan.cache_file, &source_code)?;
        Ok(DecompileResult {
            fqcn: fqcn_for(&plan, &source_code),
            source_code,
            cache_file: plan.cache_file,
            from_cache: false,
        })
    }

    fn plan(&self, target: &DecompileTarget) -> Result<ClassPlan, ToolchainError> {
        let (source_path, package, class_file) = match target {
            DecompileTarget::ClassFile { class_path } => {
                let name = class_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .to_owned();
                (class_path.clone(), Vec::new(), name)
            }
            DecompileTarget::ArchiveEntry {
                archive_path,
                entry_name,
            } => {
                let mut segments = validate_entry_name(archive_path, entry_name)?;
                if segments.len() > 1
                    && segments[0] == "classes"
                    && archive_path.extension().is_some_and(|e| e == "jmod")
                {
                    segments.remove(0);
                }
                let class_file = segments.pop().unwrap_or_default();
                (archive_path.clone(), segments, class_file)
            }
        };
        let Some(stem) = class_file.strip_suffix(".class").filter(|s| !s.is_empty()) else {
            return Err(ToolchainError::ArchiveExtraction {
                archive: source_path.display().to_string(),
                reason: format!("`{class_file}` is not a .class file"),
            });
        };
        let outer = stem
            .split('$')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or(stem)
            .to_owned();

        let key = self.cache_key(&source_path)?;
        let mut cache_file = self.cache_root.join(key);
        cache_file.extend(&package);
        cache_file.push(format!("{outer}.java"));
        Ok(ClassPlan {
            source_path,
            package,
            outer,
            stem: stem.to_owned(),
            cache_file,
        })
    }

    /// SHA-256 over the canonical artifact path, size and mtime plus the Vineflower jar identity.
    fn cache_key(&self, source: &Path) -> Result<String, ToolchainError> {
        let mut hasher = Sha256::new();
        hasher.update(b"kova-vineflower-cache-v1\0");
        for path in [source, self.vineflower_jar.as_path()] {
            let canonical = fs::canonicalize(path)
                .map_err(|err| ToolchainError::io(path.display().to_string(), &err))?;
            let meta = fs::metadata(&canonical)
                .map_err(|err| ToolchainError::io(path.display().to_string(), &err))?;
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos());
            hasher.update(canonical.to_string_lossy().as_bytes());
            hasher.update([0]);
            hasher.update(meta.len().to_le_bytes());
            hasher.update(mtime.to_le_bytes());
        }
        let digest = hasher.finalize();
        let mut key = String::with_capacity(32);
        for byte in digest.iter().take(16) {
            key.push(char::from(HEX[usize::from(byte >> 4)]));
            key.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        Ok(key)
    }

    async fn decompile_in(
        &self,
        target: &DecompileTarget,
        plan: &ClassPlan,
        work: &Path,
    ) -> Result<String, ToolchainError> {
        let input_dir = work.join("in");
        let output_dir = work.join("out");
        for dir in [&input_dir, &output_dir] {
            fs::create_dir_all(dir)
                .map_err(|err| ToolchainError::io(dir.display().to_string(), &err))?;
        }

        let (target_owned, package, outer, input) = (
            target.clone(),
            plan.package.clone(),
            plan.outer.clone(),
            input_dir.clone(),
        );
        tokio::task::spawn_blocking(move || stage_input(&target_owned, &package, &outer, &input))
            .await
            .map_err(|err| ToolchainError::Io {
                path: plan.source_path.display().to_string(),
                message: format!("blocking task failed: {err}"),
            })??;

        self.run_vineflower(&input_dir, &output_dir).await?;

        let mut expected = output_dir.clone();
        expected.extend(&plan.package);
        expected.push(format!("{}.java", plan.outer));
        let produced = if expected.is_file() {
            expected
        } else {
            // A lone inner class (no `Outer.class` available) is emitted as `Outer$Inner.java`.
            let produced = list_java_files(&output_dir);
            let named = |name: String| {
                produced
                    .iter()
                    .find(|p| p.file_name().is_some_and(|n| *n == *name))
            };
            named(format!("{}.java", plan.outer))
                .or_else(|| named(format!("{}.java", plan.stem)))
                .or(produced.first().filter(|_| produced.len() == 1))
                .cloned()
                .ok_or_else(|| ToolchainError::ProcessFailed {
                    program: self.runtime_jdk.java_bin.display().to_string(),
                    status: "no output".to_owned(),
                    stderr: format!("Vineflower produced no `{}.java`", plan.outer),
                })?
        };
        fs::read_to_string(&produced)
            .map_err(|err| ToolchainError::io(produced.display().to_string(), &err))
    }

    async fn run_vineflower(
        &self,
        input_dir: &Path,
        output_dir: &Path,
    ) -> Result<(), ToolchainError> {
        let program = self.runtime_jdk.java_bin.display().to_string();
        let mut cmd = tokio::process::Command::new(&self.runtime_jdk.java_bin);
        cmd.arg("-Xmx512m")
            .arg("-jar")
            .arg(&self.vineflower_jar)
            .arg("--decompile-generics=1")
            .arg("--remove-synthetic=1")
            .arg(input_dir)
            .arg(output_dir)
            .env("JAVA_HOME", &self.runtime_jdk.home_dir)
            .current_dir(input_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let output = match tokio::time::timeout(self.timeout, cmd.output()).await {
            Err(_) => {
                return Err(ToolchainError::ProcessFailed {
                    program,
                    status: format!("timed out after {} ms", self.timeout.as_millis()),
                    stderr: String::new(),
                });
            }
            Ok(Err(err)) => {
                return Err(ToolchainError::ProcessFailed {
                    program,
                    status: "failed to start".to_owned(),
                    stderr: err.to_string(),
                });
            }
            Ok(Ok(output)) => output,
        };
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: String = stderr
            .chars()
            .rev()
            .take(MAX_STDERR_CHARS)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        Err(ToolchainError::ProcessFailed {
            program,
            status: output.status.to_string(),
            stderr: tail,
        })
    }
}

fn fqcn_for(plan: &ClassPlan, source: &str) -> String {
    let package = if plan.package.is_empty() {
        source
            .lines()
            .map(str::trim)
            .find_map(|line| line.strip_prefix("package "))
            .map(|rest| rest.trim_end_matches(';').trim().to_owned())
            .unwrap_or_default()
    } else {
        plan.package.join(".")
    };
    if package.is_empty() {
        plan.outer.clone()
    } else {
        format!("{package}.{}", plan.outer)
    }
}

/// Validates an archive entry name and returns its `/`-separated segments.
fn validate_entry_name(archive: &Path, name: &str) -> Result<Vec<String>, ToolchainError> {
    let blocked = || ToolchainError::PathTraversalBlocked {
        archive: archive.display().to_string(),
        entry: name.to_owned(),
    };
    if name.is_empty() || name.starts_with('/') || name.contains(['\0', '\\', ':']) {
        return Err(blocked());
    }
    let segments: Vec<String> = name.split('/').map(str::to_owned).collect();
    if segments
        .iter()
        .any(|s| s.is_empty() || s == "." || s == "..")
    {
        return Err(blocked());
    }
    Ok(segments)
}

/// Copies the requested class and its `Outer$*.class` companions into `input_dir/<package>/`.
fn stage_input(
    target: &DecompileTarget,
    package: &[String],
    outer: &str,
    input_dir: &Path,
) -> Result<(), ToolchainError> {
    let is_companion = |file: &str| {
        file == format!("{outer}.class")
            || (file.starts_with(&format!("{outer}$")) && has_extension(file, "class"))
    };
    match target {
        DecompileTarget::ClassFile { class_path } => {
            let io =
                |p: &Path, err: &std::io::Error| ToolchainError::io(p.display().to_string(), err);
            let file_name = class_path.file_name().unwrap_or_default();
            fs::copy(class_path, input_dir.join(file_name)).map_err(|err| io(class_path, &err))?;
            if let Some(dir) = class_path.parent()
                && let Ok(entries) = fs::read_dir(dir)
            {
                for entry in entries.filter_map(Result::ok) {
                    let name = entry.file_name();
                    let Some(name) = name.to_str() else { continue };
                    if is_companion(name) && entry.file_type().is_ok_and(|t| t.is_file()) {
                        fs::copy(entry.path(), input_dir.join(name))
                            .map_err(|err| io(&entry.path(), &err))?;
                    }
                }
            }
            Ok(())
        }
        DecompileTarget::ArchiveEntry {
            archive_path,
            entry_name,
        } => {
            let label = archive_path.display().to_string();
            let mut file =
                File::open(archive_path).map_err(|err| ToolchainError::io(label.clone(), &err))?;
            let mut magic = [0_u8; 4];
            let is_jmod = file.read_exact(&mut magic).is_ok() && magic == JMOD_MAGIC;
            if is_jmod {
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)
                    .map_err(|err| ToolchainError::io(label.clone(), &err))?;
                stage_from_zip(
                    Cursor::new(bytes),
                    archive_path,
                    entry_name,
                    package,
                    input_dir,
                    &is_companion,
                )
            } else {
                file.rewind()
                    .map_err(|err| ToolchainError::io(label.clone(), &err))?;
                stage_from_zip(
                    file,
                    archive_path,
                    entry_name,
                    package,
                    input_dir,
                    &is_companion,
                )
            }
        }
    }
}

fn stage_from_zip<R: Read + Seek>(
    reader: R,
    archive_path: &Path,
    entry_name: &str,
    package: &[String],
    input_dir: &Path,
    is_companion: &dyn Fn(&str) -> bool,
) -> Result<(), ToolchainError> {
    let label = archive_path.display().to_string();
    let extraction = |reason: String| ToolchainError::ArchiveExtraction {
        archive: label.clone(),
        reason,
    };
    let mut archive = ZipArchive::new(reader).map_err(|err| extraction(err.to_string()))?;
    let names: Vec<String> = archive.file_names().map(str::to_owned).collect();

    // `.jmod` files keep classes under `classes/`; try the plain name first.
    let requested = [entry_name.to_owned(), format!("classes/{entry_name}")]
        .into_iter()
        .find(|candidate| names.iter().any(|n| n == candidate))
        .ok_or_else(|| extraction(format!("entry `{entry_name}` not found")))?;
    let class_dir = requested
        .rfind('/')
        .map_or_else(String::new, |idx| requested[..=idx].to_owned());

    let dest_dir = package
        .iter()
        .fold(input_dir.to_path_buf(), |dir, seg| dir.join(seg));
    fs::create_dir_all(&dest_dir)
        .map_err(|err| ToolchainError::io(dest_dir.display().to_string(), &err))?;

    let mut budget = MAX_EXTRACTED_CLASS_BYTES;
    for name in names {
        let Some(file) = name.strip_prefix(&class_dir) else {
            continue;
        };
        if file.contains('/') || !is_companion(file) {
            continue;
        }
        validate_entry_name(archive_path, &name)?;
        let mut entry = archive
            .by_name(&name)
            .map_err(|err| extraction(err.to_string()))?;
        let mut bytes = Vec::new();
        (&mut entry)
            .take(budget + 1)
            .read_to_end(&mut bytes)
            .map_err(|err| extraction(err.to_string()))?;
        let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if len > budget {
            return Err(extraction("extracted class bytes exceed limit".to_owned()));
        }
        budget -= len;
        let dest = dest_dir.join(file);
        fs::write(&dest, &bytes)
            .map_err(|err| ToolchainError::io(dest.display().to_string(), &err))?;
    }
    Ok(())
}

/// Lists all `.java` files below `root`, sorted.
fn list_java_files(root: &Path) -> Vec<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    let mut found = Vec::new();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            match entry.file_type() {
                Ok(t) if t.is_dir() => stack.push(path),
                Ok(t) if t.is_file() && path.extension().is_some_and(|e| e == "java") => {
                    found.push(path);
                }
                _ => {}
            }
        }
    }
    found.sort();
    found
}

fn write_atomic(path: &Path, contents: &str) -> Result<(), ToolchainError> {
    let label = path.display().to_string();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|err| ToolchainError::io(parent.display().to_string(), &err))?;
    let file_name = path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let tmp = parent.join(format!(".{file_name}.tmp.{}", unique_suffix()));
    let write = || -> std::io::Result<()> {
        let mut file = File::create(&tmp)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    };
    write().map_err(|err| {
        fs::remove_file(&tmp).ok();
        ToolchainError::io(label, &err)
    })
}

/// Discovers a locally installed `vineflower.jar` (without performing any network calls)
/// and pairs it with a compatible runtime JDK (`KOVA-061`).
///
/// Search order:
/// 1. `KOVA_VINEFLOWER_JAR` environment variable (if it points to an existing file),
/// 2. Omakase `installed.toml` record for [`OmakaseTool::Vineflower`],
/// 3. `<omakase>/vineflower/vineflower.jar` or the first `vineflower*.jar` in that directory.
#[must_use]
pub fn discover_vineflower_decompiler(
    paths: &KovaPaths,
    jdks: &[JdkInstallation],
) -> Option<VineflowerDecompiler> {
    let jar = find_vineflower_jar(paths)?;
    let jdk = select_tool_runtime_jdk(jdks, 17)
        .ok()
        .or_else(|| jdks.first())?
        .clone();
    Some(VineflowerDecompiler::for_paths(paths, jar, jdk))
}

fn find_vineflower_jar(paths: &KovaPaths) -> Option<PathBuf> {
    if let Some(env_jar) = std::env::var_os("KOVA_VINEFLOWER_JAR")
        && !env_jar.is_empty()
    {
        let candidate = PathBuf::from(env_jar);
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    let manager = OmakaseManager::new(paths.clone(), OmakaseManifest::default());
    if let Ok(state) = manager.inspect_installed()
        && let Some(record) = state.get(OmakaseTool::Vineflower)
    {
        let rel = Path::new(&record.entrypoint_rel_path);
        if !rel.is_absolute()
            && !rel
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            let candidate = paths.omakase_dir().join(rel);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    let vineflower_dir = paths.vineflower_dir();
    let canonical_jar = vineflower_dir.join("vineflower.jar");
    if canonical_jar.is_file() {
        return Some(canonical_jar);
    }

    let mut jars: Vec<PathBuf> = fs::read_dir(&vineflower_dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                    n.to_ascii_lowercase().starts_with("vineflower") && has_extension(n, "jar")
                })
        })
        .collect();
    jars.sort();
    jars.into_iter().next()
}

/// Emergency `javap -p -constants` fallback used only when `cafebabe` cannot parse a `.class`
/// file (for example, bytecode compiled by a future JDK release).
pub async fn decompile_with_javap_fallback(
    jdk: &JdkInstallation,
    archive_path: &Path,
    entry_name: &str,
    bytes: &[u8],
) -> Option<String> {
    let javap_name = if cfg!(windows) { "javap.exe" } else { "javap" };
    let javap_bin = jdk.home_dir.join("bin").join(javap_name);
    if !javap_bin.is_file() {
        return None;
    }

    let class_filename = entry_name
        .rsplit('/')
        .next()
        .filter(|s| has_extension(s, "class") && !s.contains(['\\', '\0']))?;
    let work_dir = std::env::temp_dir().join(format!(
        "kova-javap-{}-{}",
        unique_suffix(),
        WORK_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&work_dir).ok()?;
    let class_path = work_dir.join(class_filename);
    if fs::write(&class_path, bytes).is_err() {
        fs::remove_dir_all(&work_dir).ok();
        return None;
    }

    let mut cmd = tokio::process::Command::new(&javap_bin);
    cmd.arg("-p")
        .arg("-constants")
        .arg(&class_path)
        .env("JAVA_HOME", &jdk.home_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    let output = tokio::time::timeout(Duration::from_secs(2), cmd.output())
        .await
        .ok()?
        .ok()?;
    fs::remove_dir_all(&work_dir).ok();

    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8(output.stdout).ok()?;
    if raw.trim().is_empty() {
        return None;
    }

    Some(format_javap_output(&raw, archive_path, entry_name))
}

fn format_javap_output(raw: &str, archive_path: &Path, entry_name: &str) -> String {
    let logical = entry_name
        .strip_prefix("classes/")
        .unwrap_or_else(|| strip_mr_prefix(entry_name));
    let fqcn = logical
        .strip_suffix(".class")
        .unwrap_or(logical)
        .replace(['/', '$'], ".");
    let package = fqcn.rsplit_once('.').map_or("", |(pkg, _)| pkg);

    let mut out = String::with_capacity(raw.len() + 256);
    out.push_str(
        "// Decompiled by Kova javap fallback — install Vineflower for full method bodies\n",
    );
    let _ = writeln!(out, "// Source: {}!/{entry_name}", archive_path.display());
    if !package.is_empty() {
        let _ = writeln!(out, "package {package};\n");
    }

    let mut in_interface = false;
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Compiled from ") || trimmed.is_empty() {
            continue;
        }
        if trimmed.contains("interface ") && trimmed.ends_with('{') {
            in_interface = true;
        }
        if !in_interface
            && line.starts_with("  ")
            && trimmed.contains('(')
            && trimmed.ends_with(';')
            && !trimmed.contains(" abstract ")
            && !trimmed.starts_with("abstract ")
            && !trimmed.contains(" native ")
            && !trimmed.starts_with("native ")
        {
            let without_semi = trimmed.trim_end_matches(';');
            out.push_str("    ");
            out.push_str(without_semi);
            out.push_str(" {\n        /* compiled code */\n    }\n");
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn strip_mr_prefix(entry_name: &str) -> &str {
    if let Some(rest) = entry_name.strip_prefix("META-INF/versions/")
        && let Some((_, logical)) = rest.split_once('/')
    {
        return logical;
    }
    entry_name
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jdk::{JavaVersion, JdkSource, JdkVendor};
    use tempfile::TempDir;

    fn dummy_jdk(home: &Path, major: u16) -> JdkInstallation {
        JdkInstallation {
            home_dir: home.to_path_buf(),
            java_bin: home.join("bin/java"),
            jmods_dir: None,
            version: JavaVersion {
                major,
                minor: 0,
                patch: 0,
                build: None,
            },
            vendor: JdkVendor::Unknown("Test".to_owned()),
            arch: "aarch64".to_owned(),
            source: JdkSource::EnvJavaHome,
        }
    }

    #[test]
    fn discovers_vineflower_from_omakase_dir_without_network() {
        let dir = TempDir::new().expect("tempdir");
        let base = dir.path();
        let paths =
            KovaPaths::from_roots(base.join("config"), base.join("data"), base.join("cache"))
                .expect("paths");
        let vf_dir = paths.vineflower_dir();
        fs::create_dir_all(&vf_dir).expect("mkdir");
        let vf_jar = vf_dir.join("vineflower-1.10.1.jar");
        fs::write(&vf_jar, b"PK\x03\x04").expect("write jar");

        let jdk_home = base.join("jdk-21");
        fs::create_dir_all(jdk_home.join("bin")).expect("mkdir bin");
        fs::write(jdk_home.join("bin/java"), b"").expect("write java");
        let jdks = vec![dummy_jdk(&jdk_home, 21)];

        // Note: only inspect without KOVA_VINEFLOWER_JAR interference when unset.
        if std::env::var_os("KOVA_VINEFLOWER_JAR").is_none() {
            let discovered = discover_vineflower_decompiler(&paths, &jdks)
                .expect("must discover vineflower jar");
            assert_eq!(discovered.vineflower_jar, vf_jar);
            assert_eq!(
                discovered.cache_root(),
                paths.cache_dir().join("decompiled")
            );
        }
    }

    #[test]
    fn format_javap_output_produces_valid_java_skeleton() {
        let raw = "\
Compiled from \"Objects.java\"
public final class java.util.Objects {
  private java.util.Objects();
  public static boolean equals(java.lang.Object, java.lang.Object);
}
";
        let formatted = format_javap_output(
            raw,
            Path::new("/jdk/jmods/java.base.jmod"),
            "classes/java/util/Objects.class",
        );
        assert!(formatted.contains("package java.util;"), "{formatted}");
        assert!(
            formatted.contains("public static boolean equals(java.lang.Object, java.lang.Object) {\n        /* compiled code */\n    }"),
            "{formatted}"
        );
    }

    #[test]
    fn entry_name_validation_blocks_traversal() {
        let archive = Path::new("/lib/a.jar");
        assert!(validate_entry_name(archive, "com/example/A.class").is_ok());
        for bad in [
            "../../evil.class",
            "/abs/A.class",
            "a/../../b.class",
            "a\\b.class",
            "a//b.class",
            "C:/x.class",
            "a\0b.class",
        ] {
            assert!(
                matches!(
                    validate_entry_name(archive, bad),
                    Err(ToolchainError::PathTraversalBlocked { .. })
                ),
                "{bad} must be blocked"
            );
        }
    }
}
