//! Eclipse Temurin (Adoptium API v3) asset resolution and Omakase provisioning.

use std::fs;
use std::path::{Path, PathBuf};

use kova_config::KovaPaths;
use serde::Deserialize;

use crate::download::{
    ArchiveFormat, ArtifactTransport, DEFAULT_MAX_JDK_UNPACKED_BYTES, ProgressCallback,
    ProgressEvent, extract_archive_safe, verify_sha256_file,
};
use crate::error::ToolchainError;
use crate::jdk::discovery::{JdkInstallation, inspect_jdk_dir};
use crate::jdk::version::{JavaVersion, JdkSource};

const ADOPTIUM_API_BASE: &str = "https://api.adoptium.net/v3/assets/latest";

/// Supported target platforms for Eclipse Temurin downloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemurinPlatform {
    /// macOS Apple Silicon (`mac` / `aarch64`).
    MacAarch64,
    /// macOS Intel (`mac` / `x64`).
    MacX64,
    /// Linux ARM64 (`linux` / `aarch64`).
    LinuxAarch64,
    /// Linux `x86_64` (`linux` / `x64`).
    LinuxX64,
}

impl TemurinPlatform {
    /// Detects the current host [`TemurinPlatform`].
    ///
    /// # Errors
    /// Returns [`ToolchainError::JdkNotFound`] if the host OS/architecture is unsupported.
    pub fn current() -> Result<Self, ToolchainError> {
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("macos", "aarch64") => Ok(Self::MacAarch64),
            ("macos", "x86_64") => Ok(Self::MacX64),
            ("linux", "aarch64") => Ok(Self::LinuxAarch64),
            ("linux", "x86_64") => Ok(Self::LinuxX64),
            (os, arch) => Err(ToolchainError::JdkNotFound {
                requirement: format!("temurin-{os}-{arch}"),
            }),
        }
    }

    /// Adoptium API `os` query parameter.
    #[must_use]
    pub const fn os_slug(self) -> &'static str {
        match self {
            Self::MacAarch64 | Self::MacX64 => "mac",
            Self::LinuxAarch64 | Self::LinuxX64 => "linux",
        }
    }

    /// Adoptium API `architecture` query parameter.
    #[must_use]
    pub const fn arch_slug(self) -> &'static str {
        match self {
            Self::MacAarch64 | Self::LinuxAarch64 => "aarch64",
            Self::MacX64 | Self::LinuxX64 => "x64",
        }
    }
}

/// Resolved Eclipse Temurin release asset from Adoptium API v3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemurinAsset {
    /// Exact semantic Java version.
    pub version: JavaVersion,
    /// HTTPS download URL for the archive.
    pub download_url: String,
    /// Expected lowercase SHA-256 hex digest.
    pub sha256: String,
    /// Archive filename (e.g. `OpenJDK21U-jdk_aarch64_mac_hotspot_21.0.5_11.tar.gz`).
    pub file_name: String,
    /// Archive size in bytes.
    pub size_bytes: u64,
    /// Detected archive format.
    pub archive_format: ArchiveFormat,
}

/// Client for querying Adoptium API v3.
#[derive(Debug, Clone, Copy, Default)]
pub struct TemurinClient;

impl TemurinClient {
    /// Resolves the latest Eclipse Temurin JDK asset for `major` and `platform`.
    ///
    /// # Errors
    /// Returns [`ToolchainError::DownloadFailed`] on network/JSON errors, or
    /// [`ToolchainError::JdkNotFound`] if Adoptium returns no matching assets.
    pub async fn resolve_latest_asset(
        major: u16,
        platform: TemurinPlatform,
        transport: &dyn ArtifactTransport,
    ) -> Result<TemurinAsset, ToolchainError> {
        let url = format!(
            "{ADOPTIUM_API_BASE}/{major}/hotspot?os={}&architecture={}&image_type=jdk&vendor=eclipse",
            platform.os_slug(),
            platform.arch_slug()
        );
        let bytes = transport.fetch_json(&url).await?;
        let releases: Vec<AdoptiumRelease> =
            serde_json::from_slice(&bytes).map_err(|err| ToolchainError::DownloadFailed {
                url: url.clone(),
                reason: format!("invalid Adoptium API JSON: {err}"),
            })?;

        let release = releases
            .into_iter()
            .next()
            .ok_or_else(|| ToolchainError::JdkNotFound {
                requirement: format!("temurin-{major}"),
            })?;

        let package = release.binary.package;
        let archive_format = if Path::new(&package.name)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"))
        {
            ArchiveFormat::Zip
        } else {
            ArchiveFormat::TarGz
        };
        let version = JavaVersion::parse(&release.version.semver)?;

        Ok(TemurinAsset {
            version,
            download_url: package.link,
            sha256: package.checksum,
            file_name: package.name,
            size_bytes: package.size,
            archive_format,
        })
    }
}

/// Ensures Eclipse Temurin `major` is installed in `paths.jdks_dir()/temurin-{major}`.
///
/// If a valid installation already exists at that path, returns it immediately without network access.
/// Otherwise downloads, verifies SHA-256, extracts safely in `.tmp`, and atomically moves into place.
///
/// # Errors
/// Returns [`ToolchainError`] on network, checksum, extraction, or I/O failures.
pub async fn provision_temurin(
    major: u16,
    platform: TemurinPlatform,
    paths: &KovaPaths,
    transport: &dyn ArtifactTransport,
    progress: Option<&ProgressCallback>,
) -> Result<JdkInstallation, ToolchainError> {
    let dest_home = paths.jdks_dir().join(format!("temurin-{major}"));
    if let Some(existing) = inspect_jdk_dir(&dest_home, JdkSource::KovaOmakase) {
        return Ok(existing);
    }

    let asset = TemurinClient::resolve_latest_asset(major, platform, transport).await?;
    let tmp_root = paths.omakase_dir().join(".tmp");
    let archive_path = tmp_root.join(&asset.file_name);
    let extract_dir = tmp_root.join(format!("extract-temurin-{major}"));

    if extract_dir.exists() {
        drop(fs::remove_dir_all(&extract_dir));
    }

    transport
        .download_to_path(&asset.download_url, &archive_path, progress)
        .await?;

    if let Some(cb) = progress {
        cb(ProgressEvent::VerifyingChecksum);
    }
    verify_sha256_file(&archive_path, &asset.sha256)?;

    if let Some(cb) = progress {
        cb(ProgressEvent::Extracting);
    }
    extract_archive_safe(
        &archive_path,
        &extract_dir,
        asset.archive_format,
        DEFAULT_MAX_JDK_UNPACKED_BYTES,
    )?;

    let extracted_home = locate_extracted_jdk_home(&extract_dir).ok_or_else(|| {
        ToolchainError::ArchiveExtraction {
            archive: archive_path.display().to_string(),
            reason: "extracted archive does not contain a valid JDK home with bin/java".to_owned(),
        }
    })?;

    if let Some(parent) = dest_home.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| ToolchainError::io(parent.display().to_string(), &err))?;
    }
    if dest_home.exists() {
        fs::remove_dir_all(&dest_home)
            .map_err(|err| ToolchainError::io(dest_home.display().to_string(), &err))?;
    }
    fs::rename(&extracted_home, &dest_home)
        .map_err(|err| ToolchainError::io(dest_home.display().to_string(), &err))?;

    drop(fs::remove_file(&archive_path));
    drop(fs::remove_dir_all(&extract_dir));

    if let Some(cb) = progress {
        cb(ProgressEvent::Completed);
    }

    inspect_jdk_dir(&dest_home, JdkSource::KovaOmakase).ok_or_else(|| ToolchainError::JdkNotFound {
        requirement: format!("temurin-{major}"),
    })
}

fn locate_extracted_jdk_home(extract_dir: &Path) -> Option<PathBuf> {
    if inspect_jdk_dir(extract_dir, JdkSource::KovaOmakase).is_some() {
        return Some(extract_dir.to_path_buf());
    }
    let entries = fs::read_dir(extract_dir).ok()?;
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir()
            && let Some(jdk) = inspect_jdk_dir(&path, JdkSource::KovaOmakase)
        {
            return Some(jdk.home_dir);
        }
    }
    None
}

#[derive(Deserialize)]
struct AdoptiumRelease {
    binary: AdoptiumBinary,
    version: AdoptiumVersion,
}

#[derive(Deserialize)]
struct AdoptiumBinary {
    package: AdoptiumPackage,
}

#[derive(Deserialize)]
struct AdoptiumPackage {
    name: String,
    link: String,
    checksum: String,
    size: u64,
}

#[derive(Deserialize)]
struct AdoptiumVersion {
    semver: String,
}
