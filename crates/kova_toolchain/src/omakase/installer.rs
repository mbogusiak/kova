//! Idempotent Omakase installer ([`OmakaseManager`]) and resolution of installed `jdt.ls` paths.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use kova_config::KovaPaths;

use crate::download::{
    ArchiveFormat, ArtifactTransport, DEFAULT_MAX_TOOL_UNPACKED_BYTES, ProgressCallback,
    ProgressEvent, extract_archive_safe, verify_sha256_file,
};
use crate::error::ToolchainError;
use crate::omakase::manifest::{
    INSTALLED_STATE_FILE_NAME, InstalledOmakaseState, InstalledToolRecord, OmakaseManifest,
    OmakasePackageFormat, OmakaseTool, PinnedArtifactSpec,
};

const LAUNCHER_PREFIX: &str = "org.eclipse.equinox.launcher_";
const LOMBOK_JAR: &str = "lombok.jar";
const VINEFLOWER_JAR: &str = "vineflower.jar";
const JAVA_DEBUG_JAR: &str = "com.microsoft.java.debug.plugin.jar";
const JAVA_DEBUG_PREFIX: &str = "com.microsoft.java.debug.plugin";

/// Resolved paths of an installed `jdt.ls` distribution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledJdtlsPaths {
    /// `~/.local/share/kova/omakase/jdtls`.
    pub root_dir: PathBuf,
    /// The unique `plugins/org.eclipse.equinox.launcher_*.jar`.
    pub launcher_jar: PathBuf,
    /// Platform-specific read-only base Equinox configuration directory (`config_*`).
    pub base_config_dir: PathBuf,
    /// `jdtls/lombok.jar`, when installed.
    pub lombok_jar: Option<PathBuf>,
    /// Installed `jdt.ls` version (used to refresh the per-workspace Equinox configuration).
    pub version: String,
}

/// Complete set of installed tool paths needed to run a Java session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OmakaseToolkit {
    /// `jdt.ls` installation.
    pub jdtls: InstalledJdtlsPaths,
    /// `vineflower/vineflower.jar`.
    pub vineflower_jar: PathBuf,
    /// `java-debug` plugin jar, when requested.
    pub java_debug_jar: Option<PathBuf>,
}

/// Candidate `config_*` directory names for the current OS/architecture, in priority order.
#[must_use]
pub fn jdtls_config_dir_candidates() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            &["config_mac_arm", "config_mac_arm64", "config_mac"]
        } else {
            &["config_mac"]
        }
    } else if cfg!(target_os = "windows") {
        &["config_win"]
    } else if cfg!(target_arch = "aarch64") {
        &["config_linux_arm", "config_linux_arm64", "config_linux"]
    } else {
        &["config_linux"]
    }
}

/// Resolves the launcher jar, platform configuration directory and optional `lombok.jar`
/// inside an installed `jdt.ls` directory.
///
/// # Errors
/// Returns [`ToolchainError::ToolNotInstalled`] if no launcher jar exists, or
/// [`ToolchainError::InvalidToolInstallation`] if the launcher is ambiguous or no
/// configuration directory matches the current platform.
pub fn resolve_installed_jdtls_paths(
    jdtls_dir: &Path,
    version: &str,
) -> Result<InstalledJdtlsPaths, ToolchainError> {
    let plugins = jdtls_dir.join("plugins");
    let not_installed = || ToolchainError::ToolNotInstalled {
        tool: OmakaseTool::JdtLs.as_str().to_owned(),
        expected_path: plugins
            .join(format!("{LAUNCHER_PREFIX}*.jar"))
            .display()
            .to_string(),
    };
    let entries = fs::read_dir(&plugins).map_err(|_| not_installed())?;
    let mut launchers: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(LAUNCHER_PREFIX) && has_extension(n, "jar"))
        })
        .collect();
    launchers.sort();
    let launcher_jar = match launchers.len() {
        0 => return Err(not_installed()),
        1 => launchers.remove(0),
        n => {
            return Err(ToolchainError::InvalidToolInstallation {
                tool: OmakaseTool::JdtLs.as_str().to_owned(),
                reason: format!(
                    "ambiguous equinox launcher: {n} candidates in `{}`",
                    plugins.display()
                ),
            });
        }
    };

    let candidates = jdtls_config_dir_candidates();
    let base_config_dir = candidates
        .iter()
        .map(|name| jdtls_dir.join(name))
        .find(|dir| dir.is_dir())
        .ok_or_else(|| ToolchainError::InvalidToolInstallation {
            tool: OmakaseTool::JdtLs.as_str().to_owned(),
            reason: format!(
                "no platform configuration directory ({}) in `{}`",
                candidates.join(", "),
                jdtls_dir.display()
            ),
        })?;

    let lombok = jdtls_dir.join(LOMBOK_JAR);
    Ok(InstalledJdtlsPaths {
        root_dir: jdtls_dir.to_path_buf(),
        launcher_jar,
        base_config_dir,
        lombok_jar: lombok.is_file().then_some(lombok),
        version: version.to_owned(),
    })
}

/// Idempotent installer of pinned Omakase tools into `KovaPaths::omakase_dir()`.
#[derive(Debug, Clone)]
pub struct OmakaseManager {
    paths: KovaPaths,
    manifest: OmakaseManifest,
}

impl OmakaseManager {
    /// Creates a manager installing `manifest` artifacts under `paths`.
    #[must_use]
    pub fn new(paths: KovaPaths, manifest: OmakaseManifest) -> Self {
        Self { paths, manifest }
    }

    /// Kova paths used by this manager.
    #[must_use]
    pub fn paths(&self) -> &KovaPaths {
        &self.paths
    }

    /// Pinned artifact manifest used by this manager.
    #[must_use]
    pub fn manifest(&self) -> &OmakaseManifest {
        &self.manifest
    }

    /// Path of `installed.toml`.
    #[must_use]
    pub fn state_file(&self) -> PathBuf {
        self.paths.omakase_dir().join(INSTALLED_STATE_FILE_NAME)
    }

    /// Reads the current installation state (empty when nothing is installed yet).
    ///
    /// # Errors
    /// Returns [`ToolchainError::Io`] if `installed.toml` exists but is unreadable.
    pub fn inspect_installed(&self) -> Result<InstalledOmakaseState, ToolchainError> {
        InstalledOmakaseState::load(&self.state_file())
    }

    /// Ensures `tool` is installed at its pinned version and returns its entrypoint path.
    ///
    /// When `installed.toml` records the pinned version and checksum and the entrypoint
    /// exists on disk, returns immediately without any network access. Otherwise downloads
    /// the artifact through `transport`, verifies its SHA-256, installs it atomically and
    /// updates `installed.toml`.
    ///
    /// # Errors
    /// Propagates download, checksum, extraction and filesystem errors.
    pub async fn ensure_tool(
        &self,
        tool: OmakaseTool,
        transport: &dyn ArtifactTransport,
        progress: Option<&ProgressCallback>,
    ) -> Result<PathBuf, ToolchainError> {
        let spec = self
            .manifest
            .get(tool)
            .ok_or_else(|| ToolchainError::InvalidToolInstallation {
                tool: tool.as_str().to_owned(),
                reason: "no pinned artifact in manifest".to_owned(),
            })?
            .clone();
        let omakase_dir = self.paths.omakase_dir();
        let mut state = self.inspect_installed()?;

        if let Some(record) = state.get(tool)
            && record.version == spec.version
            && record.sha256.eq_ignore_ascii_case(spec.sha256.trim())
            && let Some(entry) = join_rel(&omakase_dir, &record.entrypoint_rel_path)
            && entry.is_file()
        {
            emit(progress, ProgressEvent::Completed);
            return Ok(entry);
        }

        fs::create_dir_all(&omakase_dir)
            .map_err(|err| ToolchainError::io(omakase_dir.display().to_string(), &err))?;
        let staging = omakase_dir.join(format!(".staging-{}-{}", tool.as_str(), unique_suffix()));
        fs::create_dir_all(&staging)
            .map_err(|err| ToolchainError::io(staging.display().to_string(), &err))?;
        let installed = self
            .install_into_place(tool, &spec, &staging, transport, progress)
            .await;
        fs::remove_dir_all(&staging).ok();
        let entry = installed?;

        let rel = entry
            .strip_prefix(&omakase_dir)
            .map(to_slash_path)
            .map_err(|_| ToolchainError::InvalidToolInstallation {
                tool: tool.as_str().to_owned(),
                reason: format!("entrypoint `{}` escapes omakase dir", entry.display()),
            })?;
        state.set(
            tool,
            InstalledToolRecord {
                version: spec.version.clone(),
                sha256: spec.sha256.trim().to_ascii_lowercase(),
                entrypoint_rel_path: rel,
                installed_unix_s: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs()),
            },
        );
        state.save_atomic(&self.state_file())?;
        emit(progress, ProgressEvent::Completed);
        Ok(entry)
    }

    /// Installs everything needed for a Java session: `jdt.ls`, Lombok, Vineflower and,
    /// when `include_java_debug` is set, the `java-debug` plugin.
    ///
    /// # Errors
    /// Propagates errors of [`Self::ensure_tool`] and [`resolve_installed_jdtls_paths`].
    pub async fn ensure_jvm_lsp_toolkit(
        &self,
        include_java_debug: bool,
        transport: &dyn ArtifactTransport,
        progress: Option<&ProgressCallback>,
    ) -> Result<OmakaseToolkit, ToolchainError> {
        self.ensure_tool(OmakaseTool::JdtLs, transport, progress)
            .await?;
        self.ensure_tool(OmakaseTool::Lombok, transport, progress)
            .await?;
        let vineflower_jar = self
            .ensure_tool(OmakaseTool::Vineflower, transport, progress)
            .await?;
        let java_debug_jar = if include_java_debug {
            Some(
                self.ensure_tool(OmakaseTool::JavaDebug, transport, progress)
                    .await?,
            )
        } else {
            None
        };
        let version = self
            .manifest
            .get(OmakaseTool::JdtLs)
            .map(|spec| spec.version.clone())
            .unwrap_or_default();
        let jdtls = resolve_installed_jdtls_paths(&self.paths.jdtls_dir(), &version)?;
        Ok(OmakaseToolkit {
            jdtls,
            vineflower_jar,
            java_debug_jar,
        })
    }

    async fn install_into_place(
        &self,
        tool: OmakaseTool,
        spec: &PinnedArtifactSpec,
        staging: &Path,
        transport: &dyn ArtifactTransport,
        progress: Option<&ProgressCallback>,
    ) -> Result<PathBuf, ToolchainError> {
        let download = staging.join("artifact.download");
        transport
            .download_to_path(&spec.url, &download, progress)
            .await?;

        emit(progress, ProgressEvent::VerifyingChecksum);
        let verify_path = download.clone();
        let expected = spec.sha256.clone();
        run_blocking(&download, move || {
            verify_sha256_file(&verify_path, &expected)
        })
        .await?;

        let archive_format = match spec.format {
            OmakasePackageFormat::RawJar => {
                let target = self.raw_jar_target(tool)?;
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|err| ToolchainError::io(parent.display().to_string(), &err))?;
                }
                fs::rename(&download, &target)
                    .map_err(|err| ToolchainError::io(target.display().to_string(), &err))?;
                return Ok(target);
            }
            OmakasePackageFormat::TarGz => ArchiveFormat::TarGz,
            OmakasePackageFormat::Zip => ArchiveFormat::Zip,
        };

        emit(progress, ProgressEvent::Extracting);
        let unpacked = staging.join("unpacked");
        let (archive, dest) = (download.clone(), unpacked.clone());
        run_blocking(&download, move || {
            extract_archive_safe(
                &archive,
                &dest,
                archive_format,
                DEFAULT_MAX_TOOL_UNPACKED_BYTES,
            )
        })
        .await?;
        let root = single_root_dir(&unpacked);

        let (target_dir, entry_rel) = match tool {
            OmakaseTool::JdtLs => {
                let resolved = resolve_installed_jdtls_paths(&root, &spec.version)?;
                let rel = relative_to(&resolved.launcher_jar, &root, tool)?;
                (self.paths.jdtls_dir(), rel)
            }
            OmakaseTool::Vineflower => (
                self.paths.vineflower_dir(),
                find_jar(&root, tool, |name| name.starts_with("vineflower"))?,
            ),
            OmakaseTool::JavaDebug => (
                self.paths.java_debug_dir(),
                find_jar(&root, tool, |name| name.starts_with(JAVA_DEBUG_PREFIX))?,
            ),
            OmakaseTool::Lombok => {
                return Err(ToolchainError::InvalidToolInstallation {
                    tool: tool.as_str().to_owned(),
                    reason: "Lombok must be pinned as a raw jar".to_owned(),
                });
            }
        };

        let preserve: &[&str] = if tool == OmakaseTool::JdtLs {
            &[LOMBOK_JAR]
        } else {
            &[]
        };
        swap_dir(&root, &target_dir, preserve)?;
        Ok(target_dir.join(entry_rel))
    }

    fn raw_jar_target(&self, tool: OmakaseTool) -> Result<PathBuf, ToolchainError> {
        match tool {
            OmakaseTool::Lombok => Ok(self.paths.jdtls_dir().join(LOMBOK_JAR)),
            OmakaseTool::Vineflower => Ok(self.paths.vineflower_dir().join(VINEFLOWER_JAR)),
            OmakaseTool::JavaDebug => Ok(self.paths.java_debug_dir().join(JAVA_DEBUG_JAR)),
            OmakaseTool::JdtLs => Err(ToolchainError::InvalidToolInstallation {
                tool: tool.as_str().to_owned(),
                reason: "jdt.ls must be pinned as a tar.gz or zip archive".to_owned(),
            }),
        }
    }
}

/// Case-insensitive file extension check on a file name.
pub(crate) fn has_extension(file_name: &str, ext: &str) -> bool {
    Path::new(file_name)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case(ext))
}

fn emit(progress: Option<&ProgressCallback>, event: ProgressEvent) {
    if let Some(callback) = progress {
        callback(event);
    }
}

async fn run_blocking<F>(label: &Path, job: F) -> Result<(), ToolchainError>
where
    F: FnOnce() -> Result<(), ToolchainError> + Send + 'static,
{
    tokio::task::spawn_blocking(job)
        .await
        .map_err(|err| ToolchainError::Io {
            path: label.display().to_string(),
            message: format!("blocking task failed: {err}"),
        })?
}

pub(crate) fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{}-{nanos}", std::process::id())
}

/// Joins a `/`-separated relative path, rejecting absolute, empty or `..` segments.
fn join_rel(base: &Path, rel: &str) -> Option<PathBuf> {
    let mut out = base.to_path_buf();
    for segment in rel.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." || segment.contains('\\') {
            return None;
        }
        out.push(segment);
    }
    Some(out)
}

fn to_slash_path(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

fn relative_to(path: &Path, root: &Path, tool: OmakaseTool) -> Result<PathBuf, ToolchainError> {
    path.strip_prefix(root).map(Path::to_path_buf).map_err(|_| {
        ToolchainError::InvalidToolInstallation {
            tool: tool.as_str().to_owned(),
            reason: format!("`{}` is outside `{}`", path.display(), root.display()),
        }
    })
}

/// Descends into a lone top-level directory (archives wrapped in a single folder).
fn single_root_dir(unpacked: &Path) -> PathBuf {
    let Ok(entries) = fs::read_dir(unpacked) else {
        return unpacked.to_path_buf();
    };
    let entries: Vec<_> = entries.filter_map(Result::ok).collect();
    match entries.as_slice() {
        [only] if only.file_type().is_ok_and(|t| t.is_dir()) => only.path(),
        _ => unpacked.to_path_buf(),
    }
}

/// Finds the first (sorted) `.jar` below `root` whose file name satisfies `accept`.
fn find_jar(
    root: &Path,
    tool: OmakaseTool,
    accept: impl Fn(&str) -> bool,
) -> Result<PathBuf, ToolchainError> {
    let mut stack = vec![root.to_path_buf()];
    let mut found = Vec::new();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file()
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| has_extension(n, "jar") && accept(n))
            {
                found.push(path);
            }
        }
    }
    found.sort();
    let first =
        found
            .into_iter()
            .next()
            .ok_or_else(|| ToolchainError::InvalidToolInstallation {
                tool: tool.as_str().to_owned(),
                reason: format!("no matching jar found in `{}`", root.display()),
            })?;
    relative_to(&first, root, tool)
}

/// Atomically replaces `target` with `new_root`, carrying over `preserve` files.
fn swap_dir(new_root: &Path, target: &Path, preserve: &[&str]) -> Result<(), ToolchainError> {
    let io =
        |path: &Path, err: &std::io::Error| ToolchainError::io(path.display().to_string(), err);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|err| io(parent, &err))?;
    }
    if !target.exists() {
        return fs::rename(new_root, target).map_err(|err| io(target, &err));
    }
    let name = target
        .file_name()
        .map_or_else(|| "tool".to_owned(), |n| n.to_string_lossy().into_owned());
    let old = target.with_file_name(format!(".{name}.old-{}", unique_suffix()));
    fs::rename(target, &old).map_err(|err| io(target, &err))?;
    for file in preserve {
        let (from, to) = (old.join(file), new_root.join(file));
        if from.is_file() && !to.exists() {
            fs::rename(&from, &to).map_err(|err| io(&to, &err))?;
        }
    }
    if let Err(err) = fs::rename(new_root, target) {
        fs::rename(&old, target).ok();
        return Err(io(target, &err));
    }
    fs::remove_dir_all(&old).ok();
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "unit test assertions")]

    use super::*;

    #[test]
    fn join_rel_rejects_traversal() {
        let base = Path::new("/base");
        assert_eq!(
            join_rel(base, "jdtls/plugins/a.jar"),
            Some(PathBuf::from("/base/jdtls/plugins/a.jar"))
        );
        assert_eq!(join_rel(base, "../etc/passwd"), None);
        assert_eq!(join_rel(base, "/abs"), None);
        assert_eq!(join_rel(base, "a//b"), None);
    }
}
