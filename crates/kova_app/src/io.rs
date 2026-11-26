//! Non-blocking file loading and atomic, streaming saves on `tokio` (`KOVA-016`).
//!
//! All disk I/O for the editor happens here, on the `tokio` runtime owned by `kova_app`;
//! `kova_ui` never touches the filesystem (*Headless Core* rule, `KOVA-003`).

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use kova_buffer::{Buffer, BufferSnapshot};
use tokio::io::{AsyncWriteExt as _, BufWriter};

/// Status-bar message for files that cannot be decoded as UTF-8.
pub const NON_UTF8_MESSAGE: &str = "Binary or non-UTF-8 file is not supported in Phase 0";

/// Write buffer size used when streaming rope chunks to disk.
const WRITE_BUFFER_BYTES: usize = 64 * 1024;

static TEMP_FILE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Result of [`load_file_for_editor`].
#[derive(Debug, Clone)]
pub enum LoadedFile {
    /// The file exists and was decoded into a ready-to-edit [`Buffer`].
    Existing {
        /// Path that was read.
        path: PathBuf,
        /// Buffer holding the file contents (line endings preserved).
        buffer: Buffer,
    },
    /// The file does not exist yet; it will be created on the first save.
    NewFile {
        /// Path the new file will be written to.
        path: PathBuf,
    },
}

impl LoadedFile {
    /// Returns the path associated with this result.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::Existing { path, .. } | Self::NewFile { path } => path,
        }
    }
}

/// Errors returned by [`load_file_for_editor`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FileLoadError {
    /// The path refers to a directory.
    #[error("`{}` is a directory", .0.display())]
    IsDirectory(PathBuf),
    /// The file is binary or not valid UTF-8.
    #[error("{NON_UTF8_MESSAGE}: `{}`", .0.display())]
    InvalidUtf8(PathBuf),
    /// Any other I/O failure (permissions, device errors, ...).
    #[error("cannot read `{}`: {message}", path.display())]
    Io {
        /// Path that failed.
        path: PathBuf,
        /// Underlying error description.
        message: String,
    },
}

/// Errors returned by [`atomic_write_snapshot`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FileSaveError {
    /// The parent directory of the target does not exist (no deep trees are created silently).
    #[error("parent directory `{}` does not exist", .0.display())]
    ParentMissing(PathBuf),
    /// The target path refers to a directory.
    #[error("`{}` is a directory", .0.display())]
    IsDirectory(PathBuf),
    /// Any other I/O failure (permissions, disk full, ...).
    #[error("cannot write `{}`: {message}", path.display())]
    Io {
        /// Path that failed.
        path: PathBuf,
        /// Underlying error description.
        message: String,
    },
}

fn load_io_error(path: &Path, err: &std::io::Error) -> FileLoadError {
    FileLoadError::Io {
        path: path.to_path_buf(),
        message: err.to_string(),
    }
}

fn save_io_error(path: &Path, err: &std::io::Error) -> FileSaveError {
    FileSaveError::Io {
        path: path.to_path_buf(),
        message: err.to_string(),
    }
}

/// Reads `path` without blocking the GPUI thread and builds an editor [`Buffer`].
///
/// A missing file yields [`LoadedFile::NewFile`] so the editor can create it on save.
///
/// # Errors
///
/// Returns [`FileLoadError::IsDirectory`] for directories, [`FileLoadError::InvalidUtf8`]
/// for binary / non-UTF-8 content and [`FileLoadError::Io`] for other read failures.
pub async fn load_file_for_editor(path: &Path) -> Result<LoadedFile, FileLoadError> {
    let metadata = match tokio::fs::metadata(path).await {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            return Ok(LoadedFile::NewFile {
                path: path.to_path_buf(),
            });
        }
        Err(err) => return Err(load_io_error(path, &err)),
    };
    if metadata.is_dir() {
        return Err(FileLoadError::IsDirectory(path.to_path_buf()));
    }

    let bytes = tokio::fs::read(path)
        .await
        .map_err(|err| load_io_error(path, &err))?;
    // `from_utf8` reuses the allocation; NUL bytes mark binary files such as `.class`.
    let text = String::from_utf8(bytes)
        .ok()
        .filter(|text| !text.as_bytes().contains(&0))
        .ok_or_else(|| FileLoadError::InvalidUtf8(path.to_path_buf()))?;
    let buffer = Buffer::from_text(&text);
    drop(text);

    Ok(LoadedFile::Existing {
        path: path.to_path_buf(),
        buffer,
    })
}

/// Atomically writes `snapshot` to `path`.
///
/// Rope chunks are streamed into a temporary sibling file (`.kova-tmp-<pid>-<seq>`)
/// without materializing the whole document as one `String`; the temp file inherits
/// the permissions of an existing target, is flushed and `fsync`ed, then renamed over
/// `path`. On any failure the temp file is removed and `path` is left untouched. When
/// `path` is a symlink, the link target is replaced and the link itself is preserved.
///
/// # Errors
///
/// Returns [`FileSaveError::ParentMissing`] when the parent directory does not exist,
/// [`FileSaveError::IsDirectory`] when `path` is a directory and [`FileSaveError::Io`]
/// for any other failure.
pub async fn atomic_write_snapshot(
    path: &Path,
    snapshot: BufferSnapshot,
) -> Result<(), FileSaveError> {
    let target = resolve_symlink_target(path).await?;
    let parent = match target.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    match tokio::fs::metadata(&parent).await {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Err(FileSaveError::ParentMissing(parent)),
        Err(err) if err.kind() == ErrorKind::NotFound => {
            return Err(FileSaveError::ParentMissing(parent));
        }
        Err(err) => return Err(save_io_error(&parent, &err)),
    }

    let existing_permissions = match tokio::fs::metadata(&target).await {
        Ok(meta) if meta.is_dir() => return Err(FileSaveError::IsDirectory(target)),
        Ok(meta) => Some(meta.permissions()),
        Err(err) if err.kind() == ErrorKind::NotFound => None,
        Err(err) => return Err(save_io_error(&target, &err)),
    };

    let seq = TEMP_FILE_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp_path = parent.join(format!(".kova-tmp-{}-{seq}", std::process::id()));

    let result = write_temp_and_rename(&tmp_path, &target, &snapshot, existing_permissions).await;
    if result.is_err() {
        drop(tokio::fs::remove_file(&tmp_path).await);
    }
    result
}

async fn resolve_symlink_target(path: &Path) -> Result<PathBuf, FileSaveError> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(meta) if meta.file_type().is_symlink() => match tokio::fs::canonicalize(path).await {
            Ok(resolved) => Ok(resolved),
            // Dangling link: write through to the link's own location.
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(path.to_path_buf()),
            Err(err) => Err(save_io_error(path, &err)),
        },
        _ => Ok(path.to_path_buf()),
    }
}

async fn write_temp_and_rename(
    tmp_path: &Path,
    target: &Path,
    snapshot: &BufferSnapshot,
    permissions: Option<std::fs::Permissions>,
) -> Result<(), FileSaveError> {
    let file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(tmp_path)
        .await
        .map_err(|err| save_io_error(tmp_path, &err))?;

    let mut writer = BufWriter::with_capacity(WRITE_BUFFER_BYTES, file);
    for chunk in snapshot.rope().chunks() {
        writer
            .write_all(chunk.as_bytes())
            .await
            .map_err(|err| save_io_error(tmp_path, &err))?;
    }
    writer
        .flush()
        .await
        .map_err(|err| save_io_error(tmp_path, &err))?;
    let mut file = writer.into_inner();
    file.flush()
        .await
        .map_err(|err| save_io_error(tmp_path, &err))?;
    file.sync_data()
        .await
        .map_err(|err| save_io_error(tmp_path, &err))?;
    drop(file);

    if let Some(permissions) = permissions {
        tokio::fs::set_permissions(tmp_path, permissions)
            .await
            .map_err(|err| save_io_error(tmp_path, &err))?;
    }

    tokio::fs::rename(tmp_path, target)
        .await
        .map_err(|err| save_io_error(target, &err))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    fn temp_files(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .expect("read_dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".kova-tmp-"))
            .collect()
    }

    #[test]
    fn load_reports_new_file_directory_and_binary() {
        let rt = runtime();
        let dir = tempfile::tempdir().expect("tempdir");

        let missing = dir.path().join("New.java");
        let loaded = rt
            .block_on(load_file_for_editor(&missing))
            .expect("new file");
        assert!(matches!(loaded, LoadedFile::NewFile { ref path } if *path == missing));

        assert_eq!(
            rt.block_on(load_file_for_editor(dir.path())).err(),
            Some(FileLoadError::IsDirectory(dir.path().to_path_buf()))
        );

        let class = dir.path().join("Main.class");
        std::fs::write(&class, [0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00]).expect("write class");
        let err = rt
            .block_on(load_file_for_editor(&class))
            .expect_err("binary must fail");
        assert_eq!(err, FileLoadError::InvalidUtf8(class));
        assert!(err.to_string().contains(NON_UTF8_MESSAGE));
    }

    #[test]
    fn load_existing_preserves_crlf_text() {
        let rt = runtime();
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("Win.java");
        std::fs::write(&file, "class A {}\r\n// zażółć\r\n").expect("write");
        let LoadedFile::Existing { buffer, .. } =
            rt.block_on(load_file_for_editor(&file)).expect("load")
        else {
            panic!("expected existing file");
        };
        assert_eq!(buffer.snapshot().text(), "class A {}\r\n// zażółć\r\n");
    }

    #[test]
    fn atomic_write_creates_and_replaces_without_temp_leftovers() {
        let rt = runtime();
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("Main.java");

        let snap = Buffer::from_text("first\n").snapshot();
        rt.block_on(atomic_write_snapshot(&file, snap))
            .expect("create");
        assert_eq!(std::fs::read_to_string(&file).expect("read"), "first\n");

        let snap = Buffer::from_text("second\r\nline\r\n").snapshot();
        rt.block_on(atomic_write_snapshot(&file, snap))
            .expect("replace");
        assert_eq!(
            std::fs::read_to_string(&file).expect("read"),
            "second\r\nline\r\n"
        );
        assert_eq!(temp_files(dir.path()), Vec::<String>::new());
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_permissions_and_symlinks() {
        use std::os::unix::fs::PermissionsExt as _;

        let rt = runtime();
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("run.sh");
        std::fs::write(&file, "old").expect("write");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o750)).expect("chmod");

        let link = dir.path().join("link.sh");
        std::os::unix::fs::symlink(&file, &link).expect("symlink");

        rt.block_on(atomic_write_snapshot(
            &link,
            Buffer::from_text("#!/bin/sh\n").snapshot(),
        ))
        .expect("save through symlink");

        assert!(
            std::fs::symlink_metadata(&link)
                .expect("lstat")
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&file).expect("read"), "#!/bin/sh\n");
        let mode = std::fs::metadata(&file).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o750);
    }

    #[test]
    fn atomic_write_fails_cleanly_for_missing_parent_and_directory() {
        let rt = runtime();
        let dir = tempfile::tempdir().expect("tempdir");
        let deep = dir.path().join("no/such/dir/Main.java");
        assert_eq!(
            rt.block_on(atomic_write_snapshot(&deep, Buffer::new().snapshot())),
            Err(FileSaveError::ParentMissing(dir.path().join("no/such/dir")))
        );
        assert_eq!(
            rt.block_on(atomic_write_snapshot(dir.path(), Buffer::new().snapshot())),
            Err(FileSaveError::IsDirectory(dir.path().to_path_buf()))
        );
        assert_eq!(temp_files(dir.path()), Vec::<String>::new());
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_keeps_original_when_directory_is_read_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let rt = runtime();
        let dir = tempfile::tempdir().expect("tempdir");
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).expect("mkdir");
        let file = locked.join("Main.java");
        std::fs::write(&file, "original").expect("write");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).expect("chmod");

        let result = rt.block_on(atomic_write_snapshot(
            &file,
            Buffer::from_text("changed").snapshot(),
        ));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        // Running as root bypasses directory permissions; only assert when the write failed.
        if result.is_err() {
            assert!(matches!(result, Err(FileSaveError::Io { .. })));
            assert_eq!(std::fs::read_to_string(&file).expect("read"), "original");
        }
        assert_eq!(temp_files(&locked), Vec::<String>::new());
    }
}
