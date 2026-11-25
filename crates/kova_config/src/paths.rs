//! Deterministic resolver for Kova's Omarchy directory layout (`KOVA-005`).
//!
//! On both macOS and Linux, Kova stores user-editable dotfiles under
//! `~/.config/kova/`, managed JVM toolchains and WASM plugins under
//! `~/.local/share/kova/`, and ephemeral workspace caches under
//! `~/.cache/kova/workspaces/<hash>/`, leaving user project repositories
//! completely free of hidden folders.

use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

/// Errors returned when resolving or initializing Omarchy system paths.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathsError {
    /// `HOME` environment variable is missing, empty, or not an absolute path.
    #[error("cannot determine user home directory: HOME is missing or invalid")]
    MissingHomeDir,
    /// An explicit `KOVA_*_HOME` override or root path is not absolute.
    #[error("path override for `{var}` must be absolute, got `{path}`")]
    RelativeOverridePath {
        /// Name of the environment variable or root parameter.
        var: &'static str,
        /// Invalid path value supplied.
        path: String,
    },
    /// Invalid theme identifier passed to [`KovaPaths::theme_file`].
    #[error("invalid theme identifier `{id}`")]
    InvalidThemeId {
        /// Rejected theme identifier string.
        id: String,
    },
    /// Failed to canonicalize a project root path.
    #[error("failed to canonicalize project path `{path}`: {message}")]
    CanonicalizeFailed {
        /// Input project path.
        path: String,
        /// Underlying error description.
        message: String,
    },
    /// Filesystem I/O failure while creating Omarchy directories.
    #[error("failed to create directory `{path}`: {message}")]
    CreateDirFailed {
        /// Directory path that could not be created.
        path: String,
        /// Underlying I/O error description.
        message: String,
    },
}

/// Deterministic resolver for Kova's Omarchy directory layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KovaPaths {
    config: PathBuf,
    data: PathBuf,
    cache: PathBuf,
}

impl KovaPaths {
    /// Resolves paths from the current process environment (`std::env::var_os`).
    ///
    /// # Errors
    ///
    /// Returns [`PathsError::MissingHomeDir`] if `HOME` is unset or invalid when
    /// needed, or [`PathsError::RelativeOverridePath`] if a `KOVA_*_HOME`
    /// override is relative.
    pub fn resolve() -> Result<Self, PathsError> {
        Self::resolve_with_env(|key| std::env::var_os(key))
    }

    /// Resolves paths using an injected environment lookup (safe for parallel
    /// Rust 2024 tests without mutating global environment state).
    ///
    /// # Errors
    ///
    /// Returns [`PathsError::MissingHomeDir`] if `HOME` is missing or relative
    /// when a fallback is required, or [`PathsError::RelativeOverridePath`] if
    /// an explicit `KOVA_*_HOME` override is relative.
    pub fn resolve_with_env<F>(lookup: F) -> Result<Self, PathsError>
    where
        F: Fn(&str) -> Option<OsString>,
    {
        let home_dir = lookup("HOME")
            .filter(|val| !val.is_empty())
            .map(PathBuf::from)
            .filter(|path| path.is_absolute());

        let config = resolve_base_dir(
            &lookup,
            "KOVA_CONFIG_HOME",
            "XDG_CONFIG_HOME",
            home_dir.as_deref(),
            ".config/kova",
        )?;
        let data = resolve_base_dir(
            &lookup,
            "KOVA_DATA_HOME",
            "XDG_DATA_HOME",
            home_dir.as_deref(),
            ".local/share/kova",
        )?;
        let cache = resolve_base_dir(
            &lookup,
            "KOVA_CACHE_HOME",
            "XDG_CACHE_HOME",
            home_dir.as_deref(),
            ".cache/kova",
        )?;

        Ok(Self {
            config,
            data,
            cache,
        })
    }

    /// Constructs paths from explicit absolute root directories (for example,
    /// isolated `TempDir` directories in tests).
    ///
    /// # Errors
    ///
    /// Returns [`PathsError::RelativeOverridePath`] if any supplied root path
    /// is not absolute.
    pub fn from_roots(
        config_dir: PathBuf,
        data_dir: PathBuf,
        cache_dir: PathBuf,
    ) -> Result<Self, PathsError> {
        validate_absolute("config_dir", &config_dir)?;
        validate_absolute("data_dir", &data_dir)?;
        validate_absolute("cache_dir", &cache_dir)?;

        Ok(Self {
            config: config_dir,
            data: data_dir,
            cache: cache_dir,
        })
    }

    /// Returns `~/.config/kova`.
    #[must_use]
    pub fn config_dir(&self) -> &Path {
        &self.config
    }

    /// Returns `~/.config/kova/settings.toml`.
    #[must_use]
    pub fn settings_file(&self) -> PathBuf {
        self.config.join("settings.toml")
    }

    /// Returns `~/.config/kova/keymap.toml`.
    #[must_use]
    pub fn keymap_file(&self) -> PathBuf {
        self.config.join("keymap.toml")
    }

    /// Returns `~/.config/kova/themes`.
    #[must_use]
    pub fn themes_dir(&self) -> PathBuf {
        self.config.join("themes")
    }

    /// Returns `~/.config/kova/themes/<theme_id>.toml` after validating `theme_id`.
    ///
    /// # Errors
    ///
    /// Returns [`PathsError::InvalidThemeId`] if `theme_id` is empty, equals
    /// `.` or `..`, or contains path separators, `..`, or NUL bytes.
    pub fn theme_file(&self, theme_id: &str) -> Result<PathBuf, PathsError> {
        if theme_id.is_empty()
            || theme_id == "."
            || theme_id == ".."
            || theme_id.contains(['/', '\\', '\0'])
            || theme_id.contains("..")
        {
            return Err(PathsError::InvalidThemeId {
                id: theme_id.to_owned(),
            });
        }
        Ok(self.themes_dir().join(format!("{theme_id}.toml")))
    }

    /// Returns `~/.local/share/kova`.
    #[must_use]
    pub fn data_dir(&self) -> &Path {
        &self.data
    }

    /// Returns `~/.local/share/kova/omakase`.
    #[must_use]
    pub fn omakase_dir(&self) -> PathBuf {
        self.data.join("omakase")
    }

    /// Returns `~/.local/share/kova/omakase/jdks`.
    #[must_use]
    pub fn jdks_dir(&self) -> PathBuf {
        self.omakase_dir().join("jdks")
    }

    /// Returns `~/.local/share/kova/omakase/jdtls`.
    #[must_use]
    pub fn jdtls_dir(&self) -> PathBuf {
        self.omakase_dir().join("jdtls")
    }

    /// Returns `~/.local/share/kova/omakase/java-debug`.
    #[must_use]
    pub fn java_debug_dir(&self) -> PathBuf {
        self.omakase_dir().join("java-debug")
    }

    /// Returns `~/.local/share/kova/omakase/vineflower`.
    #[must_use]
    pub fn vineflower_dir(&self) -> PathBuf {
        self.omakase_dir().join("vineflower")
    }

    /// Returns `~/.local/share/kova/plugins`.
    #[must_use]
    pub fn plugins_dir(&self) -> PathBuf {
        self.data.join("plugins")
    }

    /// Returns `~/.cache/kova`.
    #[must_use]
    pub fn cache_dir(&self) -> &Path {
        &self.cache
    }

    /// Returns `~/.cache/kova/workspaces`.
    #[must_use]
    pub fn workspaces_cache_dir(&self) -> PathBuf {
        self.cache.join("workspaces")
    }

    /// Computes a deterministic 32-character lowercase hex SHA-256 hash prefix
    /// for the canonicalized `project_root`.
    ///
    /// # Errors
    ///
    /// Returns [`PathsError::CanonicalizeFailed`] if `project_root` cannot be
    /// canonicalized (for example, a relative path that does not exist on disk).
    pub fn workspace_hash(&self, project_root: &Path) -> Result<String, PathsError> {
        let _ = self;
        compute_workspace_hash(project_root)
    }

    /// Returns `~/.cache/kova/workspaces/<hash>` for the given `project_root`.
    ///
    /// # Errors
    ///
    /// Propagates [`PathsError::CanonicalizeFailed`] if `project_root` cannot
    /// be normalized/canonicalized.
    pub fn workspace_cache_dir(&self, project_root: &Path) -> Result<PathBuf, PathsError> {
        let hash = self.workspace_hash(project_root)?;
        Ok(self.workspaces_cache_dir().join(hash))
    }

    /// Returns `<project_root>/kova.toml` without creating any file or directory.
    #[must_use]
    pub fn project_config_file(project_root: &Path) -> PathBuf {
        project_root.join("kova.toml")
    }

    /// Creates Kova's Omarchy system directories under `config_dir`, `data_dir`,
    /// and `cache_dir`. Never touches any user project repository.
    ///
    /// # Errors
    ///
    /// Returns [`PathsError::CreateDirFailed`] if any directory cannot be created.
    pub fn ensure_omarchy_dirs(&self) -> Result<(), PathsError> {
        let dirs = [
            self.config.clone(),
            self.themes_dir(),
            self.data.clone(),
            self.omakase_dir(),
            self.jdks_dir(),
            self.jdtls_dir(),
            self.java_debug_dir(),
            self.vineflower_dir(),
            self.plugins_dir(),
            self.cache.clone(),
            self.workspaces_cache_dir(),
        ];

        for dir in &dirs {
            create_dir_all(dir)?;
        }

        Ok(())
    }

    /// Creates `~/.cache/kova/workspaces/<hash>` for `project_root` without
    /// creating any files or hidden directories inside `project_root`.
    ///
    /// # Errors
    ///
    /// Returns [`PathsError::CanonicalizeFailed`] if `project_root` cannot be
    /// resolved, or [`PathsError::CreateDirFailed`] if creating the cache
    /// directory fails.
    pub fn ensure_workspace_cache_dir(&self, project_root: &Path) -> Result<PathBuf, PathsError> {
        let dir = self.workspace_cache_dir(project_root)?;
        create_dir_all(&dir)?;
        Ok(dir)
    }
}

fn resolve_base_dir<F>(
    lookup: &F,
    kova_var: &'static str,
    xdg_var: &'static str,
    home_dir: Option<&Path>,
    default_rel: &str,
) -> Result<PathBuf, PathsError>
where
    F: Fn(&str) -> Option<OsString>,
{
    if let Some(raw) = lookup(kova_var).filter(|val| !val.is_empty()) {
        let path = PathBuf::from(raw);
        validate_absolute(kova_var, &path)?;
        return Ok(path);
    }

    if let Some(raw) = lookup(xdg_var).filter(|val| !val.is_empty()) {
        let path = PathBuf::from(raw);
        if path.is_absolute() {
            return Ok(path.join("kova"));
        }
    }

    home_dir
        .map(|home| home.join(default_rel))
        .ok_or(PathsError::MissingHomeDir)
}

fn validate_absolute(var: &'static str, path: &Path) -> Result<(), PathsError> {
    if !path.is_absolute() {
        return Err(PathsError::RelativeOverridePath {
            var,
            path: path.to_string_lossy().into_owned(),
        });
    }
    Ok(())
}

fn create_dir_all(dir: &Path) -> Result<(), PathsError> {
    std::fs::create_dir_all(dir).map_err(|err| PathsError::CreateDirFailed {
        path: dir.to_string_lossy().into_owned(),
        message: err.to_string(),
    })
}

fn compute_workspace_hash(project_root: &Path) -> Result<String, PathsError> {
    let normalized = normalize_project_path(project_root)?;
    let digest = Sha256::digest(normalized.as_os_str().as_encoded_bytes());
    let mut hex = String::with_capacity(32);
    for &byte in digest.iter().take(16) {
        hex.push(HEX_DIGITS[usize::from(byte >> 4)] as char);
        hex.push(HEX_DIGITS[usize::from(byte & 0x0f)] as char);
    }
    Ok(hex)
}

fn normalize_project_path(project_root: &Path) -> Result<PathBuf, PathsError> {
    match std::fs::canonicalize(project_root) {
        Ok(canonical) => Ok(canonical),
        Err(err) if err.kind() == ErrorKind::NotFound && project_root.is_absolute() => {
            Ok(lexical_normalize_absolute(project_root))
        }
        Err(err) => Err(PathsError::CanonicalizeFailed {
            path: project_root.to_string_lossy().into_owned(),
            message: err.to_string(),
        }),
    }
}

fn lexical_normalize_absolute(path: &Path) -> PathBuf {
    let mut stack: Vec<Component<'_>> = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                stack.push(component);
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(stack.last(), Some(Component::Normal(_))) {
                    stack.pop();
                }
            }
        }
    }
    stack.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    use super::{KovaPaths, PathsError, lexical_normalize_absolute};

    fn lookup_from<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        let map: HashMap<&'a str, OsString> =
            pairs.iter().map(|(k, v)| (*k, OsString::from(v))).collect();
        move |key| map.get(key).cloned()
    }

    #[test]
    fn resolves_omarchy_defaults_from_home() {
        let paths = KovaPaths::resolve_with_env(lookup_from(&[("HOME", "/Users/tester")]))
            .expect("resolve with HOME should succeed");

        assert_eq!(paths.config_dir(), Path::new("/Users/tester/.config/kova"));
        assert_eq!(
            paths.settings_file(),
            PathBuf::from("/Users/tester/.config/kova/settings.toml")
        );
        assert_eq!(
            paths.keymap_file(),
            PathBuf::from("/Users/tester/.config/kova/keymap.toml")
        );
        assert_eq!(
            paths.themes_dir(),
            PathBuf::from("/Users/tester/.config/kova/themes")
        );
        assert_eq!(
            paths.data_dir(),
            Path::new("/Users/tester/.local/share/kova")
        );
        assert_eq!(
            paths.omakase_dir(),
            PathBuf::from("/Users/tester/.local/share/kova/omakase")
        );
        assert_eq!(
            paths.jdks_dir(),
            PathBuf::from("/Users/tester/.local/share/kova/omakase/jdks")
        );
        assert_eq!(
            paths.jdtls_dir(),
            PathBuf::from("/Users/tester/.local/share/kova/omakase/jdtls")
        );
        assert_eq!(
            paths.java_debug_dir(),
            PathBuf::from("/Users/tester/.local/share/kova/omakase/java-debug")
        );
        assert_eq!(
            paths.vineflower_dir(),
            PathBuf::from("/Users/tester/.local/share/kova/omakase/vineflower")
        );
        assert_eq!(
            paths.plugins_dir(),
            PathBuf::from("/Users/tester/.local/share/kova/plugins")
        );
        assert_eq!(paths.cache_dir(), Path::new("/Users/tester/.cache/kova"));
        assert_eq!(
            paths.workspaces_cache_dir(),
            PathBuf::from("/Users/tester/.cache/kova/workspaces")
        );
    }

    #[test]
    fn xdg_env_vars_override_home_defaults_with_kova_suffix() {
        let paths = KovaPaths::resolve_with_env(lookup_from(&[
            ("HOME", "/home/alice"),
            ("XDG_CONFIG_HOME", "/etc/xdg-custom"),
            ("XDG_DATA_HOME", "/opt/share"),
            ("XDG_CACHE_HOME", "/var/cache"),
        ]))
        .expect("XDG overrides should resolve");

        assert_eq!(paths.config_dir(), Path::new("/etc/xdg-custom/kova"));
        assert_eq!(paths.data_dir(), Path::new("/opt/share/kova"));
        assert_eq!(paths.cache_dir(), Path::new("/var/cache/kova"));
    }

    #[test]
    fn kova_env_vars_take_precedence_over_xdg_without_extra_suffix() {
        let paths = KovaPaths::resolve_with_env(lookup_from(&[
            ("XDG_CONFIG_HOME", "/ignored/xdg-cfg"),
            ("XDG_DATA_HOME", "/ignored/xdg-data"),
            ("XDG_CACHE_HOME", "/ignored/xdg-cache"),
            ("KOVA_CONFIG_HOME", "/custom/kova-cfg"),
            ("KOVA_DATA_HOME", "/custom/kova-data"),
            ("KOVA_CACHE_HOME", "/custom/kova-cache"),
        ]))
        .expect("KOVA_* overrides should succeed even without HOME");

        assert_eq!(paths.config_dir(), Path::new("/custom/kova-cfg"));
        assert_eq!(paths.data_dir(), Path::new("/custom/kova-data"));
        assert_eq!(paths.cache_dir(), Path::new("/custom/kova-cache"));
    }

    #[test]
    fn relative_xdg_is_ignored_while_relative_kova_errors() {
        let paths = KovaPaths::resolve_with_env(lookup_from(&[
            ("HOME", "/home/alice"),
            ("XDG_CONFIG_HOME", "relative/cfg"),
        ]))
        .expect("relative XDG should fall back to HOME");
        assert_eq!(paths.config_dir(), Path::new("/home/alice/.config/kova"));

        let err = KovaPaths::resolve_with_env(lookup_from(&[
            ("HOME", "/home/alice"),
            ("KOVA_CONFIG_HOME", "relative/kova"),
        ]))
        .expect_err("relative KOVA_CONFIG_HOME must fail");
        assert_eq!(
            err,
            PathsError::RelativeOverridePath {
                var: "KOVA_CONFIG_HOME",
                path: "relative/kova".to_owned(),
            }
        );
    }

    #[test]
    fn missing_or_relative_home_fails_cleanly() {
        assert_eq!(
            KovaPaths::resolve_with_env(lookup_from(&[])),
            Err(PathsError::MissingHomeDir)
        );
        assert_eq!(
            KovaPaths::resolve_with_env(lookup_from(&[("HOME", "relative/home")])),
            Err(PathsError::MissingHomeDir)
        );
    }

    #[test]
    fn from_roots_rejects_relative_paths() {
        let err = KovaPaths::from_roots(
            PathBuf::from("rel/cfg"),
            PathBuf::from("/abs/data"),
            PathBuf::from("/abs/cache"),
        )
        .expect_err("relative root must fail");
        assert_eq!(
            err,
            PathsError::RelativeOverridePath {
                var: "config_dir",
                path: "rel/cfg".to_owned(),
            }
        );
    }

    #[test]
    fn theme_file_validates_theme_id_against_traversal() {
        let paths = KovaPaths::from_roots(
            PathBuf::from("/tmp/cfg"),
            PathBuf::from("/tmp/data"),
            PathBuf::from("/tmp/cache"),
        )
        .expect("valid roots");

        assert_eq!(
            paths.theme_file("kova-islands-dark").expect("valid theme"),
            PathBuf::from("/tmp/cfg/themes/kova-islands-dark.toml")
        );

        for invalid in ["", ".", "..", "../evil", "dir/theme", "dir\\theme", "a..b"] {
            assert_eq!(
                paths.theme_file(invalid),
                Err(PathsError::InvalidThemeId {
                    id: invalid.to_owned(),
                }),
                "expected `{invalid}` to be rejected"
            );
        }
    }

    #[test]
    fn lexical_normalization_never_escapes_root_dir() {
        assert_eq!(
            lexical_normalize_absolute(Path::new("/..")),
            PathBuf::from("/")
        );
        assert_eq!(
            lexical_normalize_absolute(Path::new("/../../a/./b/../c/")),
            PathBuf::from("/a/c")
        );
    }

    #[test]
    fn project_config_file_points_to_explicit_kova_toml() {
        assert_eq!(
            KovaPaths::project_config_file(Path::new("/workspace/my-java-app")),
            PathBuf::from("/workspace/my-java-app/kova.toml")
        );
    }
}
