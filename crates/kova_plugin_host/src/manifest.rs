//! Parser and security validator for `plugin.toml` manifests (FR-3).

use std::fs;
use std::net::IpAddr;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};

use crate::error::PluginHostError;

/// Host WIT contract major.minor version supported by this crate.
pub const SUPPORTED_API_MAJOR: u64 = 0;
/// Host WIT contract minor version supported by this crate.
pub const SUPPORTED_API_MINOR: u64 = 1;
/// Canonical string representation of the supported WIT API version.
pub const SUPPORTED_API_VERSION: &str = "0.1.0";

/// Default per-plugin linear memory limit (64 MiB).
pub const DEFAULT_MAX_MEMORY_MB: u64 = 64;
/// Default per-plugin table element limit.
pub const DEFAULT_MAX_TABLES: usize = 1_024;
/// Default per-call execution timeout in milliseconds.
pub const DEFAULT_EXECUTION_TIMEOUT_MS: u64 = 500;

/// Special `fs_read`/`fs_write` permission token granting access to the active workspace root.
pub const WORKSPACE_FS_TOKEN: &str = "workspace";

/// Deserialized representation of `<plugin-dir>/plugin.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginManifest {
    /// Canonical lowercase ASCII plugin identifier (`[a-z0-9-]+`).
    pub id: String,
    /// Human-readable plugin display name.
    pub name: String,
    /// Plugin semantic version string (e.g. `"0.1.0"`).
    pub version: String,
    /// Relative path to the `.wasm` component file inside the plugin directory.
    pub entrypoint: PathBuf,
    /// Target Kova plugin WIT API version (e.g. `"0.1"` or `"0.1.0"`).
    pub api_version: String,
    /// Optional human-readable description.
    #[serde(default)]
    pub description: Option<String>,
    /// Explicit capability-based permission declarations.
    #[serde(default)]
    pub permissions: PluginPermissions,
    /// Optional resource and timeout overrides (bounded by host [`crate::sandbox::SandboxPolicy`]).
    #[serde(default)]
    pub limits: PluginResourceLimits,
}

/// Capability-based permissions declared in `[permissions]` of `plugin.toml`.
///
/// Every capability defaults to denied/empty so untrusted plugins cannot access
/// the filesystem, network, processes, or editor buffers without explicit opt-in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginPermissions {
    /// Allowed read-only filesystem prefixes or `"workspace"`.
    #[serde(default, deserialize_with = "deserialize_bool_false_or_vec")]
    pub fs_read: Vec<String>,
    /// Allowed read-write filesystem prefixes or `"workspace"`.
    #[serde(default, deserialize_with = "deserialize_bool_false_or_vec")]
    pub fs_write: Vec<String>,
    /// Allowed network destinations (`"host"` or `"host:port"`).
    #[serde(default, deserialize_with = "deserialize_bool_false_or_vec")]
    pub network: Vec<String>,
    /// Whether spawning subprocesses is requested (default `false`; disallowed by default policy).
    #[serde(default)]
    pub process_spawn: bool,
    /// Whether read-only access to the active editor snapshot is granted.
    #[serde(default)]
    pub editor_read: bool,
    /// Whether controlled write edits to the active editor buffer are granted.
    #[serde(default)]
    pub editor_write: bool,
}

/// Optional per-plugin resource bounds declared under `[limits]` in `plugin.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginResourceLimits {
    /// Maximum linear memory per store in MiB (defaults to 64 MiB).
    #[serde(default)]
    pub max_memory_mb: Option<u64>,
    /// Maximum number of elements allowed in Wasm tables (defaults to 1024).
    #[serde(default)]
    pub max_tables: Option<usize>,
    /// Maximum wall-clock execution duration per guest invocation in milliseconds.
    #[serde(default)]
    pub execution_timeout_ms: Option<u64>,
    /// Optional deterministic instruction fuel budget per guest invocation.
    #[serde(default)]
    pub fuel: Option<u64>,
}

/// Parsed network destination rule from `[permissions].network`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkRule {
    /// Hostname or IP address (lowercased for hostnames).
    pub host: String,
    /// Optional exact TCP/UDP port restriction (`None` allows any port on that host).
    pub port: Option<u16>,
    /// Parsed IP address if `host` is an IP literal.
    pub ip: Option<IpAddr>,
}

impl PluginManifest {
    /// Parses and validates a manifest from TOML text.
    ///
    /// Note: entrypoint path existence on disk is checked separately by
    /// [`PluginManifest::resolve_entrypoint`] or [`PluginManifest::load_from_dir`].
    ///
    /// # Errors
    ///
    /// Returns [`PluginHostError::ManifestParse`] on invalid TOML, or
    /// [`PluginHostError::InvalidManifest`] / [`PluginHostError::UnsupportedApiVersion`]
    /// if validation fails.
    pub fn from_toml_str(toml_str: &str) -> Result<Self, PluginHostError> {
        let manifest: Self =
            toml::from_str(toml_str).map_err(|err| PluginHostError::ManifestParse {
                path: PathBuf::from("<inline>"),
                message: err.to_string(),
            })?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Loads, parses, and validates `<plugin_dir>/plugin.toml`, verifying that
    /// the declared `entrypoint` exists inside `plugin_dir`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginHostError`] if the manifest is missing, malformed,
    /// invalid, or references an entrypoint outside `plugin_dir`.
    pub fn load_from_dir(plugin_dir: &Path) -> Result<(Self, PathBuf), PluginHostError> {
        let manifest_path = plugin_dir.join("plugin.toml");
        let raw = fs::read_to_string(&manifest_path).map_err(|err| PluginHostError::Io {
            path: manifest_path.clone(),
            message: err.to_string(),
        })?;
        let manifest: Self =
            toml::from_str(&raw).map_err(|err| PluginHostError::ManifestParse {
                path: manifest_path,
                message: err.to_string(),
            })?;
        manifest.validate()?;
        let entrypoint = manifest.resolve_entrypoint(plugin_dir)?;
        Ok((manifest, entrypoint))
    }

    /// Validates all manifest fields, API compatibility, permissions, and limits.
    ///
    /// # Errors
    ///
    /// Returns [`PluginHostError::InvalidManifest`] or
    /// [`PluginHostError::UnsupportedApiVersion`] on invalid configuration.
    pub fn validate(&self) -> Result<(), PluginHostError> {
        validate_plugin_id(&self.id)?;

        if self.name.trim().is_empty() || self.name.contains('\0') {
            return Err(PluginHostError::InvalidManifest {
                plugin_id: self.id.clone(),
                reason: "plugin `name` must be non-empty and free of NUL bytes".to_owned(),
            });
        }
        if self.version.trim().is_empty() || self.version.contains('\0') {
            return Err(PluginHostError::InvalidManifest {
                plugin_id: self.id.clone(),
                reason: "plugin `version` must be non-empty and free of NUL bytes".to_owned(),
            });
        }

        validate_entrypoint_syntax(&self.id, &self.entrypoint)?;
        validate_api_version(&self.id, &self.api_version)?;

        for entry in &self.permissions.fs_read {
            validate_fs_permission(&self.id, "fs_read", entry)?;
        }
        for entry in &self.permissions.fs_write {
            validate_fs_permission(&self.id, "fs_write", entry)?;
        }
        for entry in &self.permissions.network {
            drop(parse_network_rule(&self.id, entry)?);
        }

        if let Some(mb) = self.limits.max_memory_mb
            && mb == 0
        {
            return Err(PluginHostError::InvalidManifest {
                plugin_id: self.id.clone(),
                reason: "`limits.max_memory_mb` must be greater than zero".to_owned(),
            });
        }
        if let Some(tables) = self.limits.max_tables
            && tables == 0
        {
            return Err(PluginHostError::InvalidManifest {
                plugin_id: self.id.clone(),
                reason: "`limits.max_tables` must be greater than zero".to_owned(),
            });
        }
        if let Some(timeout_ms) = self.limits.execution_timeout_ms
            && timeout_ms == 0
        {
            return Err(PluginHostError::InvalidManifest {
                plugin_id: self.id.clone(),
                reason: "`limits.execution_timeout_ms` must be greater than zero".to_owned(),
            });
        }
        if let Some(fuel) = self.limits.fuel
            && fuel == 0
        {
            return Err(PluginHostError::InvalidManifest {
                plugin_id: self.id.clone(),
                reason: "`limits.fuel` must be greater than zero".to_owned(),
            });
        }

        Ok(())
    }

    /// Resolves and canonicalizes `self.entrypoint` within `plugin_dir`,
    /// rejecting symlink or relative escapes outside `plugin_dir`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginHostError::InvalidEntrypoint`] if the path is missing,
    /// escapes `plugin_dir`, or is not a regular file.
    pub fn resolve_entrypoint(&self, plugin_dir: &Path) -> Result<PathBuf, PluginHostError> {
        validate_entrypoint_syntax(&self.id, &self.entrypoint)?;

        let canonical_dir =
            fs::canonicalize(plugin_dir).map_err(|err| PluginHostError::InvalidEntrypoint {
                plugin_id: self.id.clone(),
                entrypoint: self.entrypoint.clone(),
                reason: format!("cannot canonicalize plugin directory: {err}"),
            })?;

        let joined = canonical_dir.join(&self.entrypoint);
        let canonical_entry =
            fs::canonicalize(&joined).map_err(|err| PluginHostError::InvalidEntrypoint {
                plugin_id: self.id.clone(),
                entrypoint: self.entrypoint.clone(),
                reason: format!("entrypoint file not accessible: {err}"),
            })?;

        if !canonical_entry.starts_with(&canonical_dir) {
            return Err(PluginHostError::InvalidEntrypoint {
                plugin_id: self.id.clone(),
                entrypoint: self.entrypoint.clone(),
                reason: "entrypoint escapes plugin directory".to_owned(),
            });
        }
        if !canonical_entry.is_file() {
            return Err(PluginHostError::InvalidEntrypoint {
                plugin_id: self.id.clone(),
                entrypoint: self.entrypoint.clone(),
                reason: "entrypoint must point to a regular file".to_owned(),
            });
        }

        Ok(canonical_entry)
    }

    /// Parses all declared `[permissions].network` entries into structured [`NetworkRule`]s.
    ///
    /// # Errors
    ///
    /// Returns [`PluginHostError::InvalidManifest`] if any network rule is malformed.
    pub fn parsed_network_rules(&self) -> Result<Vec<NetworkRule>, PluginHostError> {
        self.permissions
            .network
            .iter()
            .map(|raw| parse_network_rule(&self.id, raw))
            .collect()
    }
}

fn deserialize_bool_false_or_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum BoolOrList {
        Bool(bool),
        List(Vec<String>),
    }

    match BoolOrList::deserialize(deserializer)? {
        BoolOrList::Bool(false) => Ok(Vec::new()),
        BoolOrList::Bool(true) => Err(serde::de::Error::custom(
            "boolean `true` is too permissive; specify an explicit allowlist",
        )),
        BoolOrList::List(items) => Ok(items),
    }
}

fn validate_plugin_id(id: &str) -> Result<(), PluginHostError> {
    let invalid = |reason: &str| PluginHostError::InvalidManifest {
        plugin_id: if id.is_empty() {
            "<empty>".to_owned()
        } else {
            id.to_owned()
        },
        reason: reason.to_owned(),
    };

    if id.is_empty() || id.len() > 64 {
        return Err(invalid("plugin `id` must be between 1 and 64 characters"));
    }
    let bytes = id.as_bytes();
    if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit() {
        return Err(invalid(
            "plugin `id` must start with lowercase ASCII [a-z0-9]",
        ));
    }
    if bytes[bytes.len() - 1] == b'-' {
        return Err(invalid("plugin `id` must not end with `-`"));
    }
    if id.contains("--") {
        return Err(invalid("plugin `id` must not contain consecutive `--`"));
    }
    if !bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
    {
        return Err(invalid(
            "plugin `id` may only contain lowercase ASCII [a-z0-9-]",
        ));
    }
    Ok(())
}

fn validate_entrypoint_syntax(plugin_id: &str, entrypoint: &Path) -> Result<(), PluginHostError> {
    let err = |reason: &str| PluginHostError::InvalidEntrypoint {
        plugin_id: plugin_id.to_owned(),
        entrypoint: entrypoint.to_path_buf(),
        reason: reason.to_owned(),
    };

    let raw = entrypoint.to_str().ok_or_else(|| err("non-UTF-8 path"))?;
    if raw.is_empty() || raw.contains('\0') {
        return Err(err("entrypoint cannot be empty or contain NUL bytes"));
    }
    if entrypoint.is_absolute() {
        return Err(err("entrypoint must be relative to the plugin directory"));
    }
    for comp in entrypoint.components() {
        match comp {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(err("entrypoint must not contain `..` or root prefixes"));
            }
        }
    }
    Ok(())
}

fn validate_api_version(plugin_id: &str, api_version: &str) -> Result<(), PluginHostError> {
    let trimmed = api_version.trim();
    let mut parts = trimmed.split('.');
    let major = parts
        .next()
        .and_then(|p| p.parse::<u64>().ok())
        .ok_or_else(|| PluginHostError::UnsupportedApiVersion {
            plugin_id: plugin_id.to_owned(),
            found: api_version.to_owned(),
            supported: SUPPORTED_API_VERSION.to_owned(),
        })?;
    let minor = parts
        .next()
        .and_then(|p| p.parse::<u64>().ok())
        .ok_or_else(|| PluginHostError::UnsupportedApiVersion {
            plugin_id: plugin_id.to_owned(),
            found: api_version.to_owned(),
            supported: SUPPORTED_API_VERSION.to_owned(),
        })?;
    if let Some(patch) = parts.next()
        && patch.parse::<u64>().is_err()
    {
        return Err(PluginHostError::UnsupportedApiVersion {
            plugin_id: plugin_id.to_owned(),
            found: api_version.to_owned(),
            supported: SUPPORTED_API_VERSION.to_owned(),
        });
    }
    if parts.next().is_some() || major != SUPPORTED_API_MAJOR || minor != SUPPORTED_API_MINOR {
        return Err(PluginHostError::UnsupportedApiVersion {
            plugin_id: plugin_id.to_owned(),
            found: api_version.to_owned(),
            supported: SUPPORTED_API_VERSION.to_owned(),
        });
    }
    Ok(())
}

fn validate_fs_permission(
    plugin_id: &str,
    field: &str,
    entry: &str,
) -> Result<(), PluginHostError> {
    let invalid = |reason: &str| PluginHostError::InvalidManifest {
        plugin_id: plugin_id.to_owned(),
        reason: format!("invalid `{field}` entry `{entry}`: {reason}"),
    };

    if entry.is_empty() || entry.contains('\0') {
        return Err(invalid("path cannot be empty or contain NUL bytes"));
    }
    if entry == WORKSPACE_FS_TOKEN {
        return Ok(());
    }

    let path = Path::new(entry);
    if !path.is_absolute() {
        return Err(invalid(
            "must be `\"workspace\"` or an explicit absolute path prefix",
        ));
    }
    for comp in path.components() {
        if matches!(comp, Component::ParentDir) {
            return Err(invalid("path traversal (`..`) is forbidden"));
        }
    }
    if path.parent().is_none() {
        return Err(invalid("granting root directory `/` is forbidden"));
    }
    Ok(())
}

pub(crate) fn parse_network_rule(
    plugin_id: &str,
    raw: &str,
) -> Result<NetworkRule, PluginHostError> {
    let invalid = |reason: &str| PluginHostError::InvalidManifest {
        plugin_id: plugin_id.to_owned(),
        reason: format!("invalid `network` entry `{raw}`: {reason}"),
    };

    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.contains('\0') {
        return Err(invalid("destination cannot be empty"));
    }
    if trimmed.contains('*') {
        return Err(invalid("wildcard host rules are forbidden"));
    }
    if trimmed.contains("://") || trimmed.contains('/') || trimmed.contains(char::is_whitespace) {
        return Err(invalid(
            "specify `host` or `host:port` without URL scheme, path, or whitespace",
        ));
    }

    let (host, port) = if let Some(rest) = trimmed.strip_prefix('[') {
        // Bracketed IPv6 literal, optionally with `:port`.
        let (addr_str, port_str) = rest
            .split_once(']')
            .ok_or_else(|| invalid("unclosed IPv6 bracket"))?;
        let port = if port_str.is_empty() {
            None
        } else if let Some(p) = port_str.strip_prefix(':') {
            Some(parse_port(plugin_id, raw, p)?)
        } else {
            return Err(invalid("unexpected characters after IPv6 literal"));
        };
        (addr_str.to_owned(), port)
    } else if let Ok(ip) = trimmed.parse::<IpAddr>() {
        // Bare IPv4 or unbracketed IPv6 without port.
        return Ok(NetworkRule {
            host: ip.to_string(),
            port: None,
            ip: Some(ip),
        });
    } else if let Some((h, p)) = trimmed.rsplit_once(':') {
        if h.is_empty() {
            return Err(invalid("missing host before `:port`"));
        }
        (h.to_ascii_lowercase(), Some(parse_port(plugin_id, raw, p)?))
    } else {
        (trimmed.to_ascii_lowercase(), None)
    };

    if host.is_empty() {
        return Err(invalid("host cannot be empty"));
    }
    let ip = host.parse::<IpAddr>().ok();
    if ip.is_none()
        && !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    {
        return Err(invalid("host contains disallowed characters"));
    }

    Ok(NetworkRule { host, port, ip })
}

fn parse_port(plugin_id: &str, raw: &str, port_str: &str) -> Result<u16, PluginHostError> {
    let port = port_str
        .parse::<u16>()
        .map_err(|_| PluginHostError::InvalidManifest {
            plugin_id: plugin_id.to_owned(),
            reason: format!("invalid `network` entry `{raw}`: invalid port `{port_str}`"),
        })?;
    if port == 0 {
        return Err(PluginHostError::InvalidManifest {
            plugin_id: plugin_id.to_owned(),
            reason: format!("invalid `network` entry `{raw}`: port 0 is not allowed"),
        });
    }
    Ok(port)
}
