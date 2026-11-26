//! `java-debug` adapter provisioning (`KOVA-041`).
//!
//! The Microsoft `java-debug` plugin is the DAP / JDWP bridge loaded into `jdt.ls` as a
//! bundle. It is pinned in the [`OmakaseManifest`](crate::OmakaseManifest)
//! ([`OmakaseTool::JavaDebug`]) and installed under
//! `~/.local/share/kova/omakase/java-debug/`; this module reports whether the installed copy
//! matches the pinned version and installs or updates it through the shared, checksum-verified
//! Omakase installer.

use std::path::{Path, PathBuf};

use crate::download::{ArtifactTransport, ProgressCallback};
use crate::error::ToolchainError;
use crate::omakase::{OmakaseManager, OmakaseTool};

/// Installation status of the `java-debug` adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JavaDebugStatus {
    /// Not installed yet.
    Missing {
        /// Version that will be installed.
        pinned_version: String,
    },
    /// Installed copy differs from the pinned version (or its jar disappeared).
    UpdateAvailable {
        /// Installed version.
        installed_version: String,
        /// Pinned version.
        pinned_version: String,
    },
    /// Installed and matching the pinned version.
    UpToDate {
        /// Installed version.
        version: String,
        /// Path of the plugin jar.
        jar: PathBuf,
    },
}

impl JavaDebugStatus {
    /// Whether [`ensure_java_debug`] would download anything.
    #[must_use]
    pub const fn needs_install(&self) -> bool {
        !matches!(self, Self::UpToDate { .. })
    }
}

/// An installed `java-debug` adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaDebugAdapter {
    /// Plugin jar passed to `jdt.ls` in `initializationOptions.bundles`.
    pub jar: PathBuf,
    /// Installed version.
    pub version: String,
    /// Whether this call downloaded a new or updated copy.
    pub updated: bool,
}

impl JavaDebugAdapter {
    /// `initializationOptions.bundles` entry for `jdt.ls`.
    #[must_use]
    pub fn bundle(&self) -> String {
        self.jar.display().to_string()
    }
}

/// Reports whether `java-debug` is installed at the pinned version, without network access.
///
/// # Errors
/// [`ToolchainError::InvalidToolInstallation`] when the manifest has no `java-debug` entry
/// and [`ToolchainError::Io`] when `installed.toml` is unreadable.
pub fn java_debug_status(manager: &OmakaseManager) -> Result<JavaDebugStatus, ToolchainError> {
    let pinned = pinned_version(manager)?;
    let state = manager.inspect_installed()?;
    let Some(record) = state.get(OmakaseTool::JavaDebug) else {
        return Ok(JavaDebugStatus::Missing {
            pinned_version: pinned,
        });
    };
    let jar = manager
        .paths()
        .omakase_dir()
        .join(Path::new(&record.entrypoint_rel_path));
    let pinned_sha = manager
        .manifest()
        .get(OmakaseTool::JavaDebug)
        .map(|spec| spec.sha256.trim().to_ascii_lowercase())
        .unwrap_or_default();
    if record.version == pinned && record.sha256.eq_ignore_ascii_case(&pinned_sha) && jar.is_file()
    {
        Ok(JavaDebugStatus::UpToDate {
            version: record.version.clone(),
            jar,
        })
    } else {
        Ok(JavaDebugStatus::UpdateAvailable {
            installed_version: record.version.clone(),
            pinned_version: pinned,
        })
    }
}

/// Installs or updates `java-debug` to the pinned version and returns the adapter jar.
///
/// Idempotent: an up-to-date installation returns immediately without network access.
///
/// # Errors
/// Propagates download, checksum and filesystem errors of [`OmakaseManager::ensure_tool`].
pub async fn ensure_java_debug(
    manager: &OmakaseManager,
    transport: &dyn ArtifactTransport,
    progress: Option<&ProgressCallback>,
) -> Result<JavaDebugAdapter, ToolchainError> {
    let updated = java_debug_status(manager)?.needs_install();
    let jar = manager
        .ensure_tool(OmakaseTool::JavaDebug, transport, progress)
        .await?;
    Ok(JavaDebugAdapter {
        jar,
        version: pinned_version(manager)?,
        updated,
    })
}

fn pinned_version(manager: &OmakaseManager) -> Result<String, ToolchainError> {
    manager
        .manifest()
        .get(OmakaseTool::JavaDebug)
        .map(|spec| spec.version.clone())
        .ok_or_else(|| ToolchainError::InvalidToolInstallation {
            tool: OmakaseTool::JavaDebug.as_str().to_owned(),
            reason: "no pinned artifact in manifest".to_owned(),
        })
}
