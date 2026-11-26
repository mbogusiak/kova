//! Pinned Omakase artifact registry ([`OmakaseManifest`]) and on-disk installation state
//! ([`InstalledOmakaseState`], persisted as `~/.local/share/kova/omakase/installed.toml`).

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::ToolchainError;

/// File name of the installation state file inside `KovaPaths::omakase_dir()`.
pub const INSTALLED_STATE_FILE_NAME: &str = "installed.toml";

/// A tool managed by the Omakase installer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OmakaseTool {
    /// Eclipse JDT Language Server.
    JdtLs,
    /// Project Lombok agent jar (`jdtls/lombok.jar`).
    Lombok,
    /// Vineflower `.class` decompiler.
    Vineflower,
    /// Microsoft `java-debug` plugin (JDWP/DAP bundle for `jdt.ls`).
    JavaDebug,
}

impl OmakaseTool {
    /// All tools, in installation order.
    pub const ALL: [Self; 4] = [Self::JdtLs, Self::Lombok, Self::Vineflower, Self::JavaDebug];

    /// Stable `snake_case` identifier used as key in `installed.toml`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::JdtLs => "jdt_ls",
            Self::Lombok => "lombok",
            Self::Vineflower => "vineflower",
            Self::JavaDebug => "java_debug",
        }
    }
}

impl fmt::Display for OmakaseTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Packaging format of a pinned Omakase artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OmakasePackageFormat {
    /// Gzip-compressed tar archive.
    TarGz,
    /// ZIP archive.
    Zip,
    /// A single `.jar` file installed as-is.
    RawJar,
}

/// A pinned, checksum-verified artifact download.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedArtifactSpec {
    /// Tool this artifact provides.
    pub tool: OmakaseTool,
    /// Pinned upstream version.
    pub version: String,
    /// Download URL.
    pub url: String,
    /// Expected lowercase 64-character hex SHA-256 digest of the downloaded file.
    pub sha256: String,
    /// Packaging format.
    pub format: OmakasePackageFormat,
    /// Minimum Java major version required to run the tool, if any.
    pub min_java_major: Option<u16>,
}

/// Registry of pinned Omakase artifacts, one per [`OmakaseTool`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OmakaseManifest {
    artifacts: BTreeMap<OmakaseTool, PinnedArtifactSpec>,
}

impl OmakaseManifest {
    /// Returns the approved, mutually compatible set of pinned artifacts.
    #[must_use]
    pub fn default_pinned() -> Self {
        let pinned = [
            PinnedArtifactSpec {
                tool: OmakaseTool::JdtLs,
                version: "1.40.0-202409261450".to_owned(),
                url: "https://download.eclipse.org/jdtls/milestones/1.40.0/jdt-language-server-1.40.0-202409261450.tar.gz".to_owned(),
                sha256: "7416fc62befa450e32f06ec2b503f2eec5f22f0b1cc12f7b8ee5112bf671cf11".to_owned(),
                format: OmakasePackageFormat::TarGz,
                min_java_major: Some(21),
            },
            PinnedArtifactSpec {
                tool: OmakaseTool::Lombok,
                version: "1.18.34".to_owned(),
                url: "https://repo1.maven.org/maven2/org/projectlombok/lombok/1.18.34/lombok-1.18.34.jar".to_owned(),
                sha256: "c27d6b2aff56241d1b07fcbcc6b183709e6b432c80f7374eeb1d823e86d4b81a".to_owned(),
                format: OmakasePackageFormat::RawJar,
                min_java_major: None,
            },
            PinnedArtifactSpec {
                tool: OmakaseTool::Vineflower,
                version: "1.10.1".to_owned(),
                url: "https://repo1.maven.org/maven2/org/vineflower/vineflower/1.10.1/vineflower-1.10.1.jar".to_owned(),
                sha256: "b9b208e50793b64657a6b6292067526613f549de7405f9243624b02f4276e409".to_owned(),
                format: OmakasePackageFormat::RawJar,
                min_java_major: Some(17),
            },
            PinnedArtifactSpec {
                tool: OmakaseTool::JavaDebug,
                version: "0.53.0".to_owned(),
                url: "https://repo1.maven.org/maven2/com/microsoft/java/com.microsoft.java.debug.plugin/0.53.0/com.microsoft.java.debug.plugin-0.53.0.jar".to_owned(),
                sha256: "27c1c77f4113056b336295d068ac69f6c9e6aa168427935ead2f6e66a1625c6d".to_owned(),
                format: OmakasePackageFormat::RawJar,
                min_java_major: Some(17),
            },
        ];
        Self {
            artifacts: pinned.into_iter().map(|spec| (spec.tool, spec)).collect(),
        }
    }

    /// Replaces the pinned artifact for `tool` (offline tests, corporate mirrors).
    ///
    /// The `spec.tool` field is normalised to `tool`.
    #[must_use]
    pub fn with_override(mut self, tool: OmakaseTool, mut spec: PinnedArtifactSpec) -> Self {
        spec.tool = tool;
        self.artifacts.insert(tool, spec);
        self
    }

    /// Returns the pinned artifact for `tool`, if present.
    #[must_use]
    pub fn get(&self, tool: OmakaseTool) -> Option<&PinnedArtifactSpec> {
        self.artifacts.get(&tool)
    }

    /// Iterates over all pinned artifacts in [`OmakaseTool`] order.
    pub fn iter(&self) -> impl Iterator<Item = &PinnedArtifactSpec> {
        self.artifacts.values()
    }
}

impl Default for OmakaseManifest {
    fn default() -> Self {
        Self::default_pinned()
    }
}

/// Installation record of one tool in `installed.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledToolRecord {
    /// Installed version (matches [`PinnedArtifactSpec::version`]).
    pub version: String,
    /// SHA-256 of the artifact the installation was produced from.
    pub sha256: String,
    /// Entrypoint path relative to `KovaPaths::omakase_dir()` (`/`-separated).
    pub entrypoint_rel_path: String,
    /// Installation time as Unix seconds.
    pub installed_unix_s: u64,
}

/// Contents of `~/.local/share/kova/omakase/installed.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledOmakaseState {
    /// Records keyed by [`OmakaseTool::as_str`].
    #[serde(default)]
    pub tools: BTreeMap<String, InstalledToolRecord>,
}

impl InstalledOmakaseState {
    /// Returns the record for `tool`, if installed.
    #[must_use]
    pub fn get(&self, tool: OmakaseTool) -> Option<&InstalledToolRecord> {
        self.tools.get(tool.as_str())
    }

    /// Inserts or replaces the record for `tool`.
    pub fn set(&mut self, tool: OmakaseTool, record: InstalledToolRecord) {
        self.tools.insert(tool.as_str().to_owned(), record);
    }

    /// Loads the state from `path`; a missing file yields an empty state.
    ///
    /// # Errors
    /// Returns [`ToolchainError::Io`] if the file exists but cannot be read or parsed.
    pub fn load(path: &Path) -> Result<Self, ToolchainError> {
        let raw = match fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => return Err(ToolchainError::io(path.display().to_string(), &err)),
        };
        toml::from_str(&raw).map_err(|err| ToolchainError::Io {
            path: path.display().to_string(),
            message: format!("invalid installed state: {err}"),
        })
    }

    /// Atomically writes the state to `path` (`.tmp` + `sync_all` + `rename`).
    ///
    /// # Errors
    /// Returns [`ToolchainError::Io`] on serialization or filesystem failures.
    pub fn save_atomic(&self, path: &Path) -> Result<(), ToolchainError> {
        let label = path.display().to_string();
        let raw = toml::to_string_pretty(self).map_err(|err| ToolchainError::Io {
            path: label.clone(),
            message: format!("cannot serialize installed state: {err}"),
        })?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| ToolchainError::io(parent.display().to_string(), &err))?;
        }
        let tmp = path.with_file_name(format!(
            ".{INSTALLED_STATE_FILE_NAME}.tmp.{}",
            std::process::id()
        ));
        let write = || -> std::io::Result<()> {
            let mut file = fs::File::create(&tmp)?;
            file.write_all(raw.as_bytes())?;
            file.sync_all()?;
            fs::rename(&tmp, path)
        };
        write().map_err(|err| {
            fs::remove_file(&tmp).ok();
            ToolchainError::io(label, &err)
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "unit test assertions")]

    use super::*;

    #[test]
    fn default_manifest_pins_every_tool_with_valid_checksum() {
        let manifest = OmakaseManifest::default_pinned();
        for tool in OmakaseTool::ALL {
            let spec = manifest.get(tool).expect("pinned");
            assert_eq!(spec.tool, tool);
            assert_eq!(spec.sha256.len(), 64);
            assert!(
                spec.sha256
                    .chars()
                    .all(|c| matches!(c, '0'..='9' | 'a'..='f'))
            );
            assert!(spec.url.starts_with("https://"));
        }
        assert_eq!(
            manifest
                .get(OmakaseTool::JdtLs)
                .and_then(|s| s.min_java_major),
            Some(21)
        );
    }

    #[test]
    fn override_replaces_single_tool() {
        let base = OmakaseManifest::default_pinned();
        let mut spec = base.get(OmakaseTool::Vineflower).expect("pinned").clone();
        spec.url = "https://mirror.local/vf.jar".to_owned();
        spec.tool = OmakaseTool::Lombok;
        let manifest = base.with_override(OmakaseTool::Vineflower, spec);
        let got = manifest.get(OmakaseTool::Vineflower).expect("pinned");
        assert_eq!(got.url, "https://mirror.local/vf.jar");
        assert_eq!(got.tool, OmakaseTool::Vineflower);
    }

    #[test]
    fn state_roundtrips_atomically() {
        let dir = std::env::temp_dir().join(format!("kova-omakase-state-{}", std::process::id()));
        let path = dir.join(INSTALLED_STATE_FILE_NAME);
        assert_eq!(
            InstalledOmakaseState::load(&path).expect("missing is empty"),
            InstalledOmakaseState::default()
        );
        let mut state = InstalledOmakaseState::default();
        state.set(
            OmakaseTool::Vineflower,
            InstalledToolRecord {
                version: "1.10.1".to_owned(),
                sha256: "ab".repeat(32),
                entrypoint_rel_path: "vineflower/vineflower.jar".to_owned(),
                installed_unix_s: 42,
            },
        );
        state.save_atomic(&path).expect("save");
        let loaded = InstalledOmakaseState::load(&path).expect("load");
        assert_eq!(loaded, state);
        fs::remove_dir_all(&dir).ok();
    }
}
