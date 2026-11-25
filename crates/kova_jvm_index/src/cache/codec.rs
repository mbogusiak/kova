//! Binary `KIDX`/`KJAR` cache codec with `StringPool` interning and `CRC32` framing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::JvmIndexError;
use crate::indexer::IndexerConfig;
use crate::model::{
    JvmMemberEntry, JvmModifiers, JvmSourceOrigin, JvmSymbolKind, JvmTypeEntry, ParsedClassUnit,
};

/// Magic header bytes for workspace index cache files (`jvm_index.kidx`).
pub const MAGIC_WORKSPACE: [u8; 8] = *b"KOVIDX\x01\x00";

/// Magic header bytes for shared `.jar`/`.jmod` shard cache files (`<sha256>.kjar`).
pub const MAGIC_JAR_SHARD: [u8; 8] = *b"KOVJAR\x01\x00";

/// Current binary cache schema version.
pub const SCHEMA_VERSION: u32 = 1;

/// Fixed size of the binary cache frame header in bytes (`8 + 4 + 8 + 4`).
pub const HEADER_SIZE: usize = 24;

/// Maximum allowed payload size (256 MiB) to guard against corrupted headers.
pub const MAX_CACHE_FILE_BYTES: u64 = 256 * 1024 * 1024;

/// Reason why a cache load did not produce a usable snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheMissReason {
    /// Cache file does not exist on disk.
    NotFound,
    /// File header magic bytes do not match `KOVIDX` or `KOVJAR`.
    InvalidMagic,
    /// Cache file was written with an incompatible schema version.
    UnsupportedSchemaVersion {
        /// Schema version found in the file header.
        found: u32,
        /// Schema version expected by this build.
        expected: u32,
    },
    /// Payload `CRC32` checksum does not match the header.
    ChecksumMismatch,
    /// Payload could not be deserialized or failed structural validation.
    CorruptedPayload(String),
    /// File is shorter than the 24-byte header or shorter than `24 + payload_len`.
    Truncated,
}

/// Result of attempting to load a binary cache file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheLoadOutcome<T> {
    /// Cache was present, valid, and decoded successfully.
    Hit(T),
    /// Cache was missing, stale, or corrupted; caller should fall back to indexing.
    Miss(CacheMissReason),
}

/// File size and modification timestamp fingerprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileFingerprint {
    /// File size in bytes.
    pub size_bytes: u64,
    /// Last modification timestamp in nanoseconds since `UNIX_EPOCH`.
    pub modified_unix_ns: u64,
}

/// Fingerprint of an external `.jar` or `.jmod` classpath artifact.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ArtifactFingerprint {
    /// Path to the archive on disk.
    pub path: PathBuf,
    /// Size and modification timestamp of the archive.
    pub fingerprint: FileFingerprint,
}

impl ArtifactFingerprint {
    /// Computes a deterministic 32-character lowercase hex key from the archive path,
    /// size, and modification timestamp.
    #[must_use]
    pub fn cache_key_hex(&self) -> String {
        let mut hasher = Sha256::new();
        let canonical = std::fs::canonicalize(&self.path).unwrap_or_else(|_| self.path.clone());
        hasher.update(canonical.to_string_lossy().as_bytes());
        hasher.update(b"\0");
        hasher.update(self.fingerprint.size_bytes.to_le_bytes());
        hasher.update(self.fingerprint.modified_unix_ns.to_le_bytes());
        let digest = hasher.finalize();
        let mut hex = String::with_capacity(32);
        for byte in &digest[..16] {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
        }
        hex
    }
}

/// Fingerprint of scanner options that affect indexed symbol output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ScanConfigFingerprint {
    /// Whether private/package-private members are included.
    pub include_private_members: bool,
    /// Target Java version for multi-release JARs.
    pub target_java_version: Option<u16>,
    /// Maximum `.class` entry size in bytes.
    pub max_class_bytes: u64,
    /// Maximum source file size in bytes.
    pub max_source_bytes: u64,
}

impl From<&IndexerConfig> for ScanConfigFingerprint {
    fn from(cfg: &IndexerConfig) -> Self {
        Self {
            include_private_members: cfg.bytecode.include_private_members,
            target_java_version: cfg.bytecode.target_java_version,
            max_class_bytes: cfg.bytecode.max_class_bytes as u64,
            max_source_bytes: cfg.source.max_source_bytes as u64,
        }
    }
}

/// Cached source file entry: relative path, fingerprint, and parsed class units.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedSourceEntry {
    /// Path relative to `project_root`.
    pub rel_path: PathBuf,
    /// Size and modification timestamp at the time of indexing.
    pub fingerprint: FileFingerprint,
    /// Parsed class units from this source file.
    pub units: Vec<ParsedClassUnit>,
}

/// Cached workspace index snapshot stored in `~/.cache/kova/workspaces/<hash>/jvm_index.kidx`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedWorkspaceSnapshot {
    /// Scanner configuration fingerprint.
    pub config: ScanConfigFingerprint,
    /// Indexed project source files and their parsed units.
    pub sources: Vec<CachedSourceEntry>,
    /// Fingerprints of all classpath `.jar`/`.jmod` artifacts.
    pub classpath: Vec<ArtifactFingerprint>,
}

/// Cached `.jar`/`.jmod` shard stored in `~/.cache/kova/jvm_jars/<sha256>.kjar`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedJarShard {
    /// Archive fingerprint validated on load.
    pub artifact: ArtifactFingerprint,
    /// Scanner configuration fingerprint.
    pub config: ScanConfigFingerprint,
    /// Parsed class units from this archive.
    pub units: Vec<ParsedClassUnit>,
}

/// Deduplicated string table indexed by `u32`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StringPool {
    strings: Vec<String>,
}

struct StringInterner {
    pool: StringPool,
    lookup: HashMap<String, u32>,
}

impl StringInterner {
    fn new() -> Self {
        Self {
            pool: StringPool::default(),
            lookup: HashMap::new(),
        }
    }

    fn intern(&mut self, value: &str) -> Result<u32, JvmIndexError> {
        if let Some(&idx) = self.lookup.get(value) {
            return Ok(idx);
        }
        let idx = u32::try_from(self.pool.strings.len()).map_err(|_| JvmIndexError::Cache {
            path: String::from("<memory>"),
            reason: String::from("string pool exceeded u32::MAX entries"),
        })?;
        self.pool.strings.push(value.to_owned());
        self.lookup.insert(value.to_owned(), idx);
        Ok(idx)
    }

    fn intern_path(&mut self, path: &Path) -> Result<u32, JvmIndexError> {
        self.intern(&path.to_string_lossy())
    }

    fn finish(self) -> StringPool {
        self.pool
    }
}

impl StringPool {
    fn resolve(&self, idx: u32) -> Result<&str, CacheMissReason> {
        self.strings
            .get(idx as usize)
            .map(String::as_str)
            .ok_or_else(|| {
                CacheMissReason::CorruptedPayload(format!("string pool index {idx} out of bounds"))
            })
    }

    fn resolve_path(&self, idx: u32) -> Result<PathBuf, CacheMissReason> {
        self.resolve(idx).map(PathBuf::from)
    }
}

#[derive(Debug, Serialize, Deserialize)]
enum WireSourceOrigin {
    ProjectSource {
        rel_path_idx: u32,
        line: u32,
        column: u32,
    },
    JarEntry {
        jar_path_idx: u32,
        entry_name: String,
    },
    JmodEntry {
        jmod_path_idx: u32,
        entry_name: String,
    },
    ClassFile {
        path_idx: u32,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct WireTypeEntry {
    fqcn: String,
    binary_name: String,
    simple_name: String,
    package_idx: u32,
    kind: JvmSymbolKind,
    modifiers: JvmModifiers,
    super_fqcn_idx: Option<u32>,
    interface_idxs: Vec<u32>,
    origin: WireSourceOrigin,
}

#[derive(Debug, Serialize, Deserialize)]
struct WireMemberEntry {
    owner_fqcn_idx: u32,
    name: String,
    kind: JvmSymbolKind,
    modifiers: JvmModifiers,
    descriptor: String,
    display_signature: String,
    line: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
struct WireParsedClassUnit {
    ty: WireTypeEntry,
    members: Vec<WireMemberEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct WireSourceEntry {
    rel_path_idx: u32,
    fingerprint: FileFingerprint,
    units: Vec<WireParsedClassUnit>,
}

#[derive(Debug, Serialize, Deserialize)]
struct WireArtifactFingerprint {
    path_idx: u32,
    fingerprint: FileFingerprint,
}

#[derive(Debug, Serialize, Deserialize)]
struct WireWorkspaceSnapshot {
    pool: StringPool,
    config: ScanConfigFingerprint,
    sources: Vec<WireSourceEntry>,
    classpath: Vec<WireArtifactFingerprint>,
}

#[derive(Debug, Serialize, Deserialize)]
struct WireJarShard {
    pool: StringPool,
    artifact: WireArtifactFingerprint,
    config: ScanConfigFingerprint,
    units: Vec<WireParsedClassUnit>,
}

fn encode_unit(
    interner: &mut StringInterner,
    unit: &ParsedClassUnit,
) -> Result<WireParsedClassUnit, JvmIndexError> {
    let ty = &unit.ty;
    let package_idx = interner.intern(&ty.package_name)?;
    let super_fqcn_idx = ty
        .super_fqcn
        .as_deref()
        .map(|s| interner.intern(s))
        .transpose()?;
    let mut interface_idxs = Vec::with_capacity(ty.interfaces.len());
    for iface in &ty.interfaces {
        interface_idxs.push(interner.intern(iface)?);
    }
    let origin = match &ty.origin {
        JvmSourceOrigin::ProjectSource {
            rel_path,
            line,
            column,
        } => WireSourceOrigin::ProjectSource {
            rel_path_idx: interner.intern_path(rel_path)?,
            line: *line,
            column: *column,
        },
        JvmSourceOrigin::JarEntry {
            jar_path,
            entry_name,
        } => WireSourceOrigin::JarEntry {
            jar_path_idx: interner.intern_path(jar_path)?,
            entry_name: entry_name.clone(),
        },
        JvmSourceOrigin::JmodEntry {
            jmod_path,
            entry_name,
        } => WireSourceOrigin::JmodEntry {
            jmod_path_idx: interner.intern_path(jmod_path)?,
            entry_name: entry_name.clone(),
        },
        JvmSourceOrigin::ClassFile { path } => WireSourceOrigin::ClassFile {
            path_idx: interner.intern_path(path)?,
        },
    };

    let mut members = Vec::with_capacity(unit.members.len());
    for m in &unit.members {
        members.push(WireMemberEntry {
            owner_fqcn_idx: interner.intern(&m.owner_fqcn)?,
            name: m.name.clone(),
            kind: m.kind,
            modifiers: m.modifiers,
            descriptor: m.descriptor.clone(),
            display_signature: m.display_signature.clone(),
            line: m.line,
        });
    }

    Ok(WireParsedClassUnit {
        ty: WireTypeEntry {
            fqcn: ty.fqcn.clone(),
            binary_name: ty.binary_name.clone(),
            simple_name: ty.simple_name.clone(),
            package_idx,
            kind: ty.kind,
            modifiers: ty.modifiers,
            super_fqcn_idx,
            interface_idxs,
            origin,
        },
        members,
    })
}

fn decode_unit(
    pool: &StringPool,
    wire: WireParsedClassUnit,
) -> Result<ParsedClassUnit, CacheMissReason> {
    let package_name = pool.resolve(wire.ty.package_idx)?.to_owned();
    let super_fqcn = wire
        .ty
        .super_fqcn_idx
        .map(|idx| pool.resolve(idx).map(str::to_owned))
        .transpose()?;
    let mut interfaces = Vec::with_capacity(wire.ty.interface_idxs.len());
    for idx in wire.ty.interface_idxs {
        interfaces.push(pool.resolve(idx)?.to_owned());
    }
    let origin = match wire.ty.origin {
        WireSourceOrigin::ProjectSource {
            rel_path_idx,
            line,
            column,
        } => JvmSourceOrigin::ProjectSource {
            rel_path: pool.resolve_path(rel_path_idx)?,
            line,
            column,
        },
        WireSourceOrigin::JarEntry {
            jar_path_idx,
            entry_name,
        } => JvmSourceOrigin::JarEntry {
            jar_path: pool.resolve_path(jar_path_idx)?,
            entry_name,
        },
        WireSourceOrigin::JmodEntry {
            jmod_path_idx,
            entry_name,
        } => JvmSourceOrigin::JmodEntry {
            jmod_path: pool.resolve_path(jmod_path_idx)?,
            entry_name,
        },
        WireSourceOrigin::ClassFile { path_idx } => JvmSourceOrigin::ClassFile {
            path: pool.resolve_path(path_idx)?,
        },
    };

    let mut members = Vec::with_capacity(wire.members.len());
    for m in wire.members {
        members.push(JvmMemberEntry {
            owner_fqcn: pool.resolve(m.owner_fqcn_idx)?.to_owned(),
            name: m.name,
            kind: m.kind,
            modifiers: m.modifiers,
            descriptor: m.descriptor,
            display_signature: m.display_signature,
            line: m.line,
        });
    }

    Ok(ParsedClassUnit {
        ty: JvmTypeEntry {
            fqcn: wire.ty.fqcn,
            binary_name: wire.ty.binary_name,
            simple_name: wire.ty.simple_name,
            package_name,
            kind: wire.ty.kind,
            modifiers: wire.ty.modifiers,
            super_fqcn,
            interfaces,
            origin,
        },
        members,
    })
}

fn frame_payload(magic: [u8; 8], payload: &[u8]) -> Result<Vec<u8>, JvmIndexError> {
    let payload_len = u64::try_from(payload.len()).map_err(|_| JvmIndexError::Cache {
        path: String::from("<memory>"),
        reason: String::from("payload length exceeds u64"),
    })?;
    if payload_len > MAX_CACHE_FILE_BYTES {
        return Err(JvmIndexError::Cache {
            path: String::from("<memory>"),
            reason: format!("payload length {payload_len} exceeds max {MAX_CACHE_FILE_BYTES}"),
        });
    }

    let mut hasher = crc32fast::Hasher::new();
    hasher.update(payload);
    let crc = hasher.finalize();

    let mut out = Vec::with_capacity(HEADER_SIZE + payload.len());
    out.extend_from_slice(&magic);
    out.extend_from_slice(&SCHEMA_VERSION.to_le_bytes());
    out.extend_from_slice(&payload_len.to_le_bytes());
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

fn unframe_payload(expected_magic: [u8; 8], bytes: &[u8]) -> Result<&[u8], CacheMissReason> {
    if bytes.len() < HEADER_SIZE {
        return Err(CacheMissReason::Truncated);
    }
    if bytes[..8] != expected_magic {
        return Err(CacheMissReason::InvalidMagic);
    }

    let schema_version = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    if schema_version != SCHEMA_VERSION {
        return Err(CacheMissReason::UnsupportedSchemaVersion {
            found: schema_version,
            expected: SCHEMA_VERSION,
        });
    }

    let payload_len = u64::from_le_bytes([
        bytes[12], bytes[13], bytes[14], bytes[15], bytes[16], bytes[17], bytes[18], bytes[19],
    ]);
    if payload_len > MAX_CACHE_FILE_BYTES {
        return Err(CacheMissReason::CorruptedPayload(format!(
            "payload length {payload_len} exceeds limit {MAX_CACHE_FILE_BYTES}"
        )));
    }
    let payload_len_usize = usize::try_from(payload_len).map_err(|_| {
        CacheMissReason::CorruptedPayload(String::from("payload length overflows usize"))
    })?;
    let expected_total = HEADER_SIZE
        .checked_add(payload_len_usize)
        .ok_or_else(|| CacheMissReason::CorruptedPayload(String::from("total size overflow")))?;
    if bytes.len() < expected_total {
        return Err(CacheMissReason::Truncated);
    }

    let expected_crc = u32::from_le_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    let payload = &bytes[HEADER_SIZE..expected_total];
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(payload);
    if hasher.finalize() != expected_crc {
        return Err(CacheMissReason::ChecksumMismatch);
    }

    Ok(payload)
}

/// Encodes a [`CachedWorkspaceSnapshot`] into a framed `KIDX` byte buffer.
///
/// # Errors
///
/// Returns [`JvmIndexError::Cache`] if serialization or framing fails.
pub fn encode_workspace_snapshot(
    snapshot: &CachedWorkspaceSnapshot,
) -> Result<Vec<u8>, JvmIndexError> {
    let mut interner = StringInterner::new();
    let mut sources = Vec::with_capacity(snapshot.sources.len());
    for src in &snapshot.sources {
        let rel_path_idx = interner.intern_path(&src.rel_path)?;
        let mut units = Vec::with_capacity(src.units.len());
        for unit in &src.units {
            units.push(encode_unit(&mut interner, unit)?);
        }
        sources.push(WireSourceEntry {
            rel_path_idx,
            fingerprint: src.fingerprint,
            units,
        });
    }

    let mut classpath = Vec::with_capacity(snapshot.classpath.len());
    for cp in &snapshot.classpath {
        classpath.push(WireArtifactFingerprint {
            path_idx: interner.intern_path(&cp.path)?,
            fingerprint: cp.fingerprint,
        });
    }

    let wire = WireWorkspaceSnapshot {
        pool: interner.finish(),
        config: snapshot.config,
        sources,
        classpath,
    };
    let payload = postcard::to_stdvec(&wire).map_err(|err| JvmIndexError::Cache {
        path: String::from("<memory>"),
        reason: err.to_string(),
    })?;
    frame_payload(MAGIC_WORKSPACE, &payload)
}

/// Decodes a framed `KIDX` byte buffer into a [`CachedWorkspaceSnapshot`].
#[must_use]
pub fn decode_workspace_snapshot(bytes: &[u8]) -> CacheLoadOutcome<CachedWorkspaceSnapshot> {
    let payload = match unframe_payload(MAGIC_WORKSPACE, bytes) {
        Ok(p) => p,
        Err(reason) => return CacheLoadOutcome::Miss(reason),
    };
    let wire: WireWorkspaceSnapshot = match postcard::from_bytes(payload) {
        Ok(w) => w,
        Err(err) => {
            return CacheLoadOutcome::Miss(CacheMissReason::CorruptedPayload(err.to_string()));
        }
    };

    let mut sources = Vec::with_capacity(wire.sources.len());
    for src in wire.sources {
        let rel_path = match wire.pool.resolve_path(src.rel_path_idx) {
            Ok(p) => p,
            Err(reason) => return CacheLoadOutcome::Miss(reason),
        };
        let mut units = Vec::with_capacity(src.units.len());
        for unit in src.units {
            match decode_unit(&wire.pool, unit) {
                Ok(u) => units.push(u),
                Err(reason) => return CacheLoadOutcome::Miss(reason),
            }
        }
        sources.push(CachedSourceEntry {
            rel_path,
            fingerprint: src.fingerprint,
            units,
        });
    }

    let mut classpath = Vec::with_capacity(wire.classpath.len());
    for cp in wire.classpath {
        let path = match wire.pool.resolve_path(cp.path_idx) {
            Ok(p) => p,
            Err(reason) => return CacheLoadOutcome::Miss(reason),
        };
        classpath.push(ArtifactFingerprint {
            path,
            fingerprint: cp.fingerprint,
        });
    }

    CacheLoadOutcome::Hit(CachedWorkspaceSnapshot {
        config: wire.config,
        sources,
        classpath,
    })
}

/// Encodes a [`CachedJarShard`] into a framed `KJAR` byte buffer.
///
/// # Errors
///
/// Returns [`JvmIndexError::Cache`] if serialization or framing fails.
pub fn encode_jar_shard(shard: &CachedJarShard) -> Result<Vec<u8>, JvmIndexError> {
    let mut interner = StringInterner::new();
    let artifact = WireArtifactFingerprint {
        path_idx: interner.intern_path(&shard.artifact.path)?,
        fingerprint: shard.artifact.fingerprint,
    };
    let mut units = Vec::with_capacity(shard.units.len());
    for unit in &shard.units {
        units.push(encode_unit(&mut interner, unit)?);
    }

    let wire = WireJarShard {
        pool: interner.finish(),
        artifact,
        config: shard.config,
        units,
    };
    let payload = postcard::to_stdvec(&wire).map_err(|err| JvmIndexError::Cache {
        path: String::from("<memory>"),
        reason: err.to_string(),
    })?;
    frame_payload(MAGIC_JAR_SHARD, &payload)
}

/// Decodes a framed `KJAR` byte buffer into a [`CachedJarShard`].
#[must_use]
pub fn decode_jar_shard(bytes: &[u8]) -> CacheLoadOutcome<CachedJarShard> {
    let payload = match unframe_payload(MAGIC_JAR_SHARD, bytes) {
        Ok(p) => p,
        Err(reason) => return CacheLoadOutcome::Miss(reason),
    };
    let wire: WireJarShard = match postcard::from_bytes(payload) {
        Ok(w) => w,
        Err(err) => {
            return CacheLoadOutcome::Miss(CacheMissReason::CorruptedPayload(err.to_string()));
        }
    };

    let path = match wire.pool.resolve_path(wire.artifact.path_idx) {
        Ok(p) => p,
        Err(reason) => return CacheLoadOutcome::Miss(reason),
    };
    let mut units = Vec::with_capacity(wire.units.len());
    for unit in wire.units {
        match decode_unit(&wire.pool, unit) {
            Ok(u) => units.push(u),
            Err(reason) => return CacheLoadOutcome::Miss(reason),
        }
    }

    CacheLoadOutcome::Hit(CachedJarShard {
        artifact: ArtifactFingerprint {
            path,
            fingerprint: wire.artifact.fingerprint,
        },
        config: wire.config,
        units,
    })
}
