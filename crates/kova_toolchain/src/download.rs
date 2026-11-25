//! Secure artifact transport, SHA-256 verification, and Zip-Slip / Tar-Slip safe extraction.

use std::fmt;
use std::fs::{self, File};
use std::future::Future;
use std::io::{self, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use flate2::read::GzDecoder;
use reqwest::Client;
use sha2::{Digest, Sha256};
use tar::{Archive, EntryType};
use tokio::io::AsyncWriteExt;
use zip::ZipArchive;

use crate::error::ToolchainError;

/// Default maximum unpacked size for JDK archives (2 GiB).
pub const DEFAULT_MAX_JDK_UNPACKED_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Default maximum unpacked size for tool archives (1 GiB).
pub const DEFAULT_MAX_TOOL_UNPACKED_BYTES: u64 = 1024 * 1024 * 1024;

const HASH_BUFFER_SIZE: usize = 64 * 1024;

/// Supported archive formats for toolchain downloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    /// Gzip-compressed tar archive (`.tar.gz` / `.tgz`).
    TarGz,
    /// ZIP archive (`.zip`).
    Zip,
}

/// Progress events emitted during artifact download, verification, and extraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressEvent {
    /// Download started with an optional known content length.
    Started {
        /// Total expected bytes, if reported by the server.
        total_bytes: Option<u64>,
    },
    /// A chunk of bytes was written to disk.
    Chunk {
        /// Cumulative bytes downloaded so far.
        downloaded_bytes: u64,
        /// Total expected bytes, if known.
        total_bytes: Option<u64>,
    },
    /// Verifying SHA-256 checksum of the downloaded archive.
    VerifyingChecksum,
    /// Extracting the verified archive into its destination directory.
    Extracting,
    /// Operation completed successfully.
    Completed,
}

/// Callback type for receiving [`ProgressEvent`] updates.
pub type ProgressCallback = dyn Fn(ProgressEvent) + Send + Sync;

/// Object-safe asynchronous transport trait for fetching JSON metadata and downloading artifacts.
pub trait ArtifactTransport: Send + Sync {
    /// Fetches raw JSON bytes from `url`.
    fn fetch_json<'a>(
        &'a self,
        url: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, ToolchainError>> + Send + 'a>>;

    /// Downloads `url` to `dest`, optionally reporting [`ProgressEvent`] updates.
    fn download_to_path<'a>(
        &'a self,
        url: &'a str,
        dest: &'a Path,
        progress: Option<&'a ProgressCallback>,
    ) -> Pin<Box<dyn Future<Output = Result<(), ToolchainError>> + Send + 'a>>;
}

/// Production HTTP transport backed by `reqwest` with `rustls-tls` (no OpenSSL dependency).
#[derive(Clone)]
pub struct ReqwestTransport {
    client: Client,
    cancelled: Arc<AtomicBool>,
}

impl fmt::Debug for ReqwestTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReqwestTransport")
            .field("cancelled", &self.cancelled.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl ReqwestTransport {
    /// Creates a new [`ReqwestTransport`] with a 30-second connect timeout and `kova-toolchain/0.1` User-Agent.
    ///
    /// # Errors
    /// Returns [`ToolchainError::DownloadFailed`] if the underlying HTTP client cannot be constructed.
    pub fn new(cancelled: Arc<AtomicBool>) -> Result<Self, ToolchainError> {
        let client = Client::builder()
            .user_agent("kova-toolchain/0.1")
            .connect_timeout(Duration::from_secs(30))
            .build()
            .map_err(|err| ToolchainError::DownloadFailed {
                url: String::new(),
                reason: err.to_string(),
            })?;
        Ok(Self { client, cancelled })
    }

    fn check_cancelled(&self) -> Result<(), ToolchainError> {
        if self.cancelled.load(Ordering::Relaxed) {
            return Err(ToolchainError::Cancelled);
        }
        Ok(())
    }
}

impl ArtifactTransport for ReqwestTransport {
    fn fetch_json<'a>(
        &'a self,
        url: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, ToolchainError>> + Send + 'a>> {
        Box::pin(async move {
            self.check_cancelled()?;
            let response = self
                .client
                .get(url)
                .header(reqwest::header::ACCEPT, "application/json")
                .send()
                .await
                .map_err(|err| ToolchainError::DownloadFailed {
                    url: url.to_owned(),
                    reason: err.to_string(),
                })?;

            let status = response.status();
            if !status.is_success() {
                return Err(ToolchainError::DownloadFailed {
                    url: url.to_owned(),
                    reason: format!("HTTP {status}"),
                });
            }

            let bytes = response
                .bytes()
                .await
                .map_err(|err| ToolchainError::DownloadFailed {
                    url: url.to_owned(),
                    reason: err.to_string(),
                })?;
            Ok(bytes.to_vec())
        })
    }

    fn download_to_path<'a>(
        &'a self,
        url: &'a str,
        dest: &'a Path,
        progress: Option<&'a ProgressCallback>,
    ) -> Pin<Box<dyn Future<Output = Result<(), ToolchainError>> + Send + 'a>> {
        Box::pin(async move {
            self.check_cancelled()?;
            if let Some(parent) = dest.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|err| ToolchainError::io(parent.display().to_string(), &err))?;
            }

            let mut response = self.client.get(url).send().await.map_err(|err| {
                ToolchainError::DownloadFailed {
                    url: url.to_owned(),
                    reason: err.to_string(),
                }
            })?;

            let status = response.status();
            if !status.is_success() {
                return Err(ToolchainError::DownloadFailed {
                    url: url.to_owned(),
                    reason: format!("HTTP {status}"),
                });
            }

            let total_bytes = response.content_length();
            if let Some(cb) = progress {
                cb(ProgressEvent::Started { total_bytes });
            }

            let part_path = dest.with_extension("part");
            let mut file = tokio::fs::File::create(&part_path)
                .await
                .map_err(|err| ToolchainError::io(part_path.display().to_string(), &err))?;

            let mut downloaded_bytes: u64 = 0;
            while let Some(chunk) =
                response
                    .chunk()
                    .await
                    .map_err(|err| ToolchainError::DownloadFailed {
                        url: url.to_owned(),
                        reason: err.to_string(),
                    })?
            {
                self.check_cancelled()?;
                file.write_all(&chunk)
                    .await
                    .map_err(|err| ToolchainError::io(part_path.display().to_string(), &err))?;
                downloaded_bytes = downloaded_bytes.saturating_add(chunk.len() as u64);
                if let Some(cb) = progress {
                    cb(ProgressEvent::Chunk {
                        downloaded_bytes,
                        total_bytes,
                    });
                }
            }

            file.sync_all()
                .await
                .map_err(|err| ToolchainError::io(part_path.display().to_string(), &err))?;
            drop(file);

            tokio::fs::rename(&part_path, dest)
                .await
                .map_err(|err| ToolchainError::io(dest.display().to_string(), &err))?;

            Ok(())
        })
    }
}

/// Verifies the SHA-256 checksum of `path` using streaming 64 KiB reads.
///
/// # Errors
/// Returns [`ToolchainError::Io`] if the file cannot be read, or
/// [`ToolchainError::ChecksumMismatch`] if the computed digest does not match `expected_hex`.
pub fn verify_sha256_file(path: &Path, expected_hex: &str) -> Result<(), ToolchainError> {
    let file =
        File::open(path).map_err(|err| ToolchainError::io(path.display().to_string(), &err))?;
    let mut reader = BufReader::with_capacity(HASH_BUFFER_SIZE, file);
    let mut hasher = Sha256::new();
    let mut buf = vec![0_u8; HASH_BUFFER_SIZE];

    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|err| ToolchainError::io(path.display().to_string(), &err))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    let digest = hasher.finalize();
    let actual_hex = encode_hex_lower(&digest);
    let expected_lower = expected_hex.trim().to_ascii_lowercase();

    if actual_hex != expected_lower {
        return Err(ToolchainError::ChecksumMismatch {
            artifact: path.display().to_string(),
            expected: expected_lower,
            actual: actual_hex,
        });
    }

    Ok(())
}

fn encode_hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Safely extracts a `.tar.gz` or `.zip` archive into `dest_dir`.
///
/// Enforces:
/// - Zip-Slip / Tar-Slip protection (rejects `..`, `/`, drive prefixes, and escaping symlinks/hardlinks).
/// - Archive bomb protection via `max_unpacked_bytes` counted on actual decompressed bytes.
/// - Preservation of Unix executable permissions.
///
/// # Errors
/// Returns [`ToolchainError::PathTraversalBlocked`] on unsafe paths/links, or
/// [`ToolchainError::ArchiveExtraction`] on I/O, format, or size-limit errors.
pub fn extract_archive_safe(
    archive_path: &Path,
    dest_dir: &Path,
    format: ArchiveFormat,
    max_unpacked_bytes: u64,
) -> Result<(), ToolchainError> {
    fs::create_dir_all(dest_dir)
        .map_err(|err| ToolchainError::io(dest_dir.display().to_string(), &err))?;

    match format {
        ArchiveFormat::TarGz => extract_tar_gz_safe(archive_path, dest_dir, max_unpacked_bytes),
        ArchiveFormat::Zip => extract_zip_safe(archive_path, dest_dir, max_unpacked_bytes),
    }
}

fn extract_tar_gz_safe(
    archive_path: &Path,
    dest_dir: &Path,
    max_unpacked_bytes: u64,
) -> Result<(), ToolchainError> {
    let archive_label = archive_path.display().to_string();
    let file = File::open(archive_path)
        .map_err(|err| ToolchainError::io(archive_path.display().to_string(), &err))?;
    let decoder = GzDecoder::new(BufReader::new(file));
    let mut archive = Archive::new(decoder);

    let mut total_unpacked: u64 = 0;
    let entries = archive
        .entries()
        .map_err(|err| ToolchainError::ArchiveExtraction {
            archive: archive_label.clone(),
            reason: err.to_string(),
        })?;

    for entry_res in entries {
        let mut entry = entry_res.map_err(|err| ToolchainError::ArchiveExtraction {
            archive: archive_label.clone(),
            reason: err.to_string(),
        })?;
        let raw_path = entry
            .path()
            .map_err(|err| ToolchainError::ArchiveExtraction {
                archive: archive_label.clone(),
                reason: err.to_string(),
            })?
            .into_owned();

        let rel_path = sanitize_relative_archive_path(&raw_path, &archive_label)?;
        let target_path = dest_dir.join(&rel_path);
        let entry_type = entry.header().entry_type();

        if entry_type == EntryType::Symlink || entry_type == EntryType::Link {
            let link_target = entry
                .link_name()
                .map_err(|err| ToolchainError::ArchiveExtraction {
                    archive: archive_label.clone(),
                    reason: err.to_string(),
                })?
                .ok_or_else(|| ToolchainError::PathTraversalBlocked {
                    archive: archive_label.clone(),
                    entry: raw_path.display().to_string(),
                })?;
            validate_link_target(&rel_path, &link_target, &archive_label)?;
            continue;
        }

        if entry_type.is_dir() {
            fs::create_dir_all(&target_path)
                .map_err(|err| ToolchainError::io(target_path.display().to_string(), &err))?;
            continue;
        }

        if entry_type.is_file() {
            if let Some(parent) = target_path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|err| ToolchainError::io(parent.display().to_string(), &err))?;
            }
            let remaining = max_unpacked_bytes.saturating_sub(total_unpacked);
            let mut out_file = File::create(&target_path)
                .map_err(|err| ToolchainError::io(target_path.display().to_string(), &err))?;
            let written = copy_bounded(&mut entry, &mut out_file, remaining, &archive_label)?;
            total_unpacked = total_unpacked.saturating_add(written);

            #[cfg(unix)]
            if let Ok(mode) = entry.header().mode() {
                use std::os::unix::fs::PermissionsExt;
                let perms = fs::Permissions::from_mode(mode & 0o777);
                drop(fs::set_permissions(&target_path, perms));
            }
        }
    }

    Ok(())
}

fn extract_zip_safe(
    archive_path: &Path,
    dest_dir: &Path,
    max_unpacked_bytes: u64,
) -> Result<(), ToolchainError> {
    let archive_label = archive_path.display().to_string();
    let file = File::open(archive_path)
        .map_err(|err| ToolchainError::io(archive_path.display().to_string(), &err))?;
    let mut archive =
        ZipArchive::new(BufReader::new(file)).map_err(|err| ToolchainError::ArchiveExtraction {
            archive: archive_label.clone(),
            reason: err.to_string(),
        })?;

    let mut total_unpacked: u64 = 0;
    for idx in 0..archive.len() {
        let mut entry = archive
            .by_index(idx)
            .map_err(|err| ToolchainError::ArchiveExtraction {
                archive: archive_label.clone(),
                reason: err.to_string(),
            })?;
        let raw_path = PathBuf::from(entry.name());
        let rel_path = sanitize_relative_archive_path(&raw_path, &archive_label)?;
        let target_path = dest_dir.join(&rel_path);

        if entry.is_symlink() {
            return Err(ToolchainError::PathTraversalBlocked {
                archive: archive_label,
                entry: entry.name().to_owned(),
            });
        }

        if entry.is_dir() {
            fs::create_dir_all(&target_path)
                .map_err(|err| ToolchainError::io(target_path.display().to_string(), &err))?;
            continue;
        }

        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| ToolchainError::io(parent.display().to_string(), &err))?;
        }

        let remaining = max_unpacked_bytes.saturating_sub(total_unpacked);
        let mut out_file = File::create(&target_path)
            .map_err(|err| ToolchainError::io(target_path.display().to_string(), &err))?;
        let written = copy_bounded(&mut entry, &mut out_file, remaining, &archive_label)?;
        total_unpacked = total_unpacked.saturating_add(written);

        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            let perms = fs::Permissions::from_mode(mode & 0o777);
            drop(fs::set_permissions(&target_path, perms));
        }
    }

    Ok(())
}

fn copy_bounded<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    remaining_budget: u64,
    archive_label: &str,
) -> Result<u64, ToolchainError> {
    let mut limited = reader.take(remaining_budget.saturating_add(1));
    let copied =
        io::copy(&mut limited, writer).map_err(|err| ToolchainError::ArchiveExtraction {
            archive: archive_label.to_owned(),
            reason: err.to_string(),
        })?;
    if copied > remaining_budget {
        return Err(ToolchainError::ArchiveExtraction {
            archive: archive_label.to_owned(),
            reason: "archive exceeds maximum unpacked size limit".to_owned(),
        });
    }
    Ok(copied)
}

fn sanitize_relative_archive_path(
    entry_path: &Path,
    archive_label: &str,
) -> Result<PathBuf, ToolchainError> {
    let mut clean = PathBuf::new();
    for component in entry_path.components() {
        match component {
            Component::Normal(part) => clean.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(ToolchainError::PathTraversalBlocked {
                    archive: archive_label.to_owned(),
                    entry: entry_path.display().to_string(),
                });
            }
        }
    }
    if clean.as_os_str().is_empty() {
        return Err(ToolchainError::PathTraversalBlocked {
            archive: archive_label.to_owned(),
            entry: entry_path.display().to_string(),
        });
    }
    Ok(clean)
}

fn validate_link_target(
    entry_rel_path: &Path,
    link_target: &Path,
    archive_label: &str,
) -> Result<(), ToolchainError> {
    if link_target.is_absolute() {
        return Err(ToolchainError::PathTraversalBlocked {
            archive: archive_label.to_owned(),
            entry: link_target.display().to_string(),
        });
    }
    let mut depth: i32 = 0;
    if let Some(parent) = entry_rel_path.parent() {
        for component in parent.components() {
            if matches!(component, Component::Normal(_)) {
                depth += 1;
            }
        }
    }
    for component in link_target.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return Err(ToolchainError::PathTraversalBlocked {
                        archive: archive_label.to_owned(),
                        entry: link_target.display().to_string(),
                    });
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(ToolchainError::PathTraversalBlocked {
                    archive: archive_label.to_owned(),
                    entry: link_target.display().to_string(),
                });
            }
        }
    }
    Ok(())
}
