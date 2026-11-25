//! Zero-JVM `.class` bytecode parser and `.jar`/`.jmod` archive scanner built on `cafebabe` and `zip`.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use cafebabe::attributes::{AttributeData, AttributeInfo, InnerClassAccessFlags, InnerClassEntry};
use cafebabe::{
    ClassAccessFlags, ClassFile, FieldAccessFlags, FieldInfo, MethodAccessFlags, MethodInfo,
    ParseOptions,
};
use zip::ZipArchive;

use crate::error::JvmIndexError;
use crate::model::{
    JvmMemberEntry, JvmModifiers, JvmSourceOrigin, JvmSymbolKind, JvmTypeEntry, JvmVisibility,
    ParsedClassUnit, internal_name_to_binary, internal_name_to_fqcn, is_ignored_internal_name,
    split_package_and_simple,
};
use crate::signature::{
    decode_constructor_descriptor, decode_constructor_signature, decode_field_descriptor,
    decode_field_signature, decode_method_descriptor, decode_method_signature,
};

/// Default per-classfile decompression safety limit (`16 MiB`) to guard against zip bombs.
pub const DEFAULT_MAX_CLASS_BYTES: usize = 16 * 1024 * 1024;

/// Default Java release version used when resolving Multi-Release JAR (`META-INF/versions/<N>`) entries.
pub const DEFAULT_TARGET_JAVA_VERSION: u16 = 21;

/// Threshold above which the reusable archive decompression buffer shrinks back to keep RSS bounded.
const REUSABLE_BUFFER_SHRINK_THRESHOLD: usize = 512 * 1024;

/// Initial capacity for the single reusable `.class` decompression buffer in [`scan_jar_archive`].
const INITIAL_BUFFER_CAPACITY: usize = 32 * 1024;

/// Configuration options controlling `.class` parsing and `.jar`/`.jmod` archive scanning.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "Scan options expose independent boolean toggles specified by KOVA-030"
)]
pub struct BytecodeScanOptions {
    /// Whether to extract non-synthetic fields, methods, and constructors for each type.
    pub include_members: bool,
    /// Whether to retain `private` members (disabled by default for library archives to save RAM).
    pub include_private_members: bool,
    /// Whether to retain package-private (non-public) top-level and nested types.
    pub include_package_private_classes: bool,
    /// Whether to skip `ACC_SYNTHETIC`, `module-info`, `package-info`, anonymous (`$1`), and lambda classes.
    pub skip_anonymous_and_synthetic: bool,
    /// Target Java version for Multi-Release JAR resolution (`None` disables `META-INF/versions/`).
    pub target_java_version: Option<u16>,
    /// When `true`, abort archive scanning on the first corrupt/oversized `.class` entry;
    /// when `false`, record a warning in [`ArchiveScanStats::warnings`] and continue.
    pub strict: bool,
    /// Maximum allowed uncompressed size in bytes for a single `.class` entry.
    pub max_class_bytes: usize,
}

impl Default for BytecodeScanOptions {
    fn default() -> Self {
        Self {
            include_members: true,
            include_private_members: false,
            include_package_private_classes: true,
            skip_anonymous_and_synthetic: true,
            target_java_version: Some(DEFAULT_TARGET_JAVA_VERSION),
            strict: false,
            max_class_bytes: DEFAULT_MAX_CLASS_BYTES,
        }
    }
}

/// Summary counters and diagnostics produced by [`scan_jar_archive`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArchiveScanStats {
    /// Total ZIP entries present in the archive.
    pub total_entries: usize,
    /// Total `.class` entries encountered in valid class roots (`classes/` for `.jmod`).
    pub class_entries: usize,
    /// Number of [`ParsedClassUnit`] types produced.
    pub indexed_types: usize,
    /// Total number of [`JvmMemberEntry`] items produced across all indexed types.
    pub indexed_members: usize,
    /// Number of archive entries skipped (non-class, shadowed MR-JAR, filtered, or warned).
    pub skipped_entries: usize,
    /// Non-fatal diagnostic warnings recorded when `strict == false`.
    pub warnings: Vec<String>,
}

/// Output of scanning a single `.jar`, `.zip`, or `.jmod` archive.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArchiveScanResult {
    /// Deterministically ordered parsed class units (sorted by `binary_name`).
    pub units: Vec<ParsedClassUnit>,
    /// Scan statistics and non-fatal warnings.
    pub stats: ArchiveScanStats,
}

/// Parses raw `.class` bytes into a [`ParsedClassUnit`], returning `Ok(None)` if the class
/// is filtered out by `options` (e.g. `ACC_SYNTHETIC`, anonymous class, or `module-info`).
///
/// Method `Code` instruction opcodes are intentionally skipped via
/// [`cafebabe::ParseOptions::parse_bytecode`] for speed and minimal heap allocation.
///
/// # Errors
///
/// Returns [`JvmIndexError::InvalidClassFile`] if `bytes` exceed `options.max_class_bytes` or
/// fail JVM classfile format validation.
pub fn parse_class_bytes(
    bytes: &[u8],
    origin: JvmSourceOrigin,
    options: &BytecodeScanOptions,
) -> Result<Option<ParsedClassUnit>, JvmIndexError> {
    if bytes.len() > options.max_class_bytes {
        return Err(JvmIndexError::InvalidClassFile {
            origin: origin_label(&origin),
            reason: format!(
                "classfile size {} bytes exceeds max_class_bytes ({})",
                bytes.len(),
                options.max_class_bytes
            ),
        });
    }

    let mut parse_opts = ParseOptions::default();
    parse_opts.parse_bytecode(false);

    let class_file = cafebabe::parse_class_with_options(bytes, &parse_opts).map_err(|err| {
        JvmIndexError::InvalidClassFile {
            origin: origin_label(&origin),
            reason: err.to_string(),
        }
    })?;

    let inner_entry = find_self_inner_class(&class_file);
    let has_synthetic_attr = has_synthetic_attribute(&class_file.attributes);
    let is_synthetic = class_file
        .access_flags
        .contains(ClassAccessFlags::SYNTHETIC)
        || has_synthetic_attr
        || inner_entry.is_some_and(|entry| {
            entry
                .access_flags
                .contains(InnerClassAccessFlags::SYNTHETIC)
        });
    let is_anonymous_inner = inner_entry.is_some_and(|entry| entry.inner_name.is_none());

    if options.skip_anonymous_and_synthetic
        && (is_synthetic
            || is_anonymous_inner
            || class_file.access_flags.contains(ClassAccessFlags::MODULE)
            || is_ignored_internal_name(&class_file.this_class))
    {
        return Ok(None);
    }

    let modifiers = resolve_class_modifiers(&class_file, inner_entry, is_synthetic);
    if !options.include_package_private_classes
        && matches!(
            modifiers.visibility,
            JvmVisibility::PackagePrivate | JvmVisibility::Private
        )
    {
        return Ok(None);
    }

    let kind = classify_symbol_kind(&class_file);
    let internal_name = class_file.this_class.as_ref();
    let fqcn = internal_name_to_fqcn(internal_name);
    let binary_name = internal_name_to_binary(internal_name);
    let (package_name, simple_name) = split_package_and_simple(&binary_name);
    let super_fqcn = class_file.super_class.as_deref().map(internal_name_to_fqcn);
    let interfaces = class_file
        .interfaces
        .iter()
        .map(|iface| internal_name_to_fqcn(iface))
        .collect();

    let ty = JvmTypeEntry {
        fqcn: fqcn.clone(),
        binary_name,
        simple_name: simple_name.clone(),
        package_name,
        kind,
        modifiers,
        super_fqcn,
        interfaces,
        origin,
    };

    let members = if options.include_members {
        extract_members(&class_file, &fqcn, &simple_name, options)
    } else {
        Vec::new()
    };

    Ok(Some(ParsedClassUnit { ty, members }))
}

/// Scans all `.class` entries inside a `.jar`, `.zip`, or `.jmod` archive using a single
/// reusable decompression buffer and bounded entry reads.
///
/// - For `.jmod` archives, only entries under `classes/` ending with `.class` are indexed.
/// - For `.jar`/`.zip` archives, Multi-Release entries (`META-INF/versions/<N>/...`) are
///   resolved against `options.target_java_version` without producing duplicate `binary_name`s.
///
/// # Errors
///
/// Returns:
/// - [`JvmIndexError::Io`] if the archive file cannot be opened,
/// - [`JvmIndexError::InvalidArchive`] if the file is not a valid ZIP/JAR/JMOD archive,
/// - [`JvmIndexError::ArchiveEntryRead`] or [`JvmIndexError::InvalidClassFile`] if `options.strict`
///   is `true` and an entry is corrupt, unreadable, or exceeds `options.max_class_bytes`.
pub fn scan_jar_archive(
    archive_path: &Path,
    options: &BytecodeScanOptions,
) -> Result<ArchiveScanResult, JvmIndexError> {
    let format = ArchiveFormat::from_path(archive_path);
    let file = File::open(archive_path).map_err(|err| JvmIndexError::Io {
        path: archive_path.display().to_string(),
        message: err.to_string(),
    })?;
    let reader = BufReader::new(file);
    let mut archive = ZipArchive::new(reader).map_err(|err| JvmIndexError::InvalidArchive {
        path: archive_path.display().to_string(),
        reason: err.to_string(),
    })?;

    let total_entries = archive.len();
    let mut stats = ArchiveScanStats {
        total_entries,
        ..ArchiveScanStats::default()
    };

    let selected = select_archive_entries(&archive, format, options, &mut stats);
    let mut units = Vec::with_capacity(selected.len());
    let mut buf = Vec::with_capacity(INITIAL_BUFFER_CAPACITY);
    let archive_path_str = archive_path.display().to_string();
    let read_limit = u64::try_from(options.max_class_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(1);

    for (zip_index, entry_name) in selected {
        buf.clear();
        let read_outcome = {
            let mut entry =
                archive
                    .by_index(zip_index)
                    .map_err(|err| JvmIndexError::ArchiveEntryRead {
                        archive: archive_path_str.clone(),
                        entry: entry_name.clone(),
                        reason: err.to_string(),
                    })?;
            entry.by_ref().take(read_limit).read_to_end(&mut buf)
        };

        if let Err(err) = read_outcome {
            let index_err = JvmIndexError::ArchiveEntryRead {
                archive: archive_path_str.clone(),
                entry: entry_name,
                reason: err.to_string(),
            };
            if options.strict {
                return Err(index_err);
            }
            stats.warnings.push(index_err.to_string());
            stats.skipped_entries += 1;
            shrink_reusable_buffer(&mut buf);
            continue;
        }

        if buf.len() > options.max_class_bytes {
            let index_err = JvmIndexError::ArchiveEntryRead {
                archive: archive_path_str.clone(),
                entry: entry_name,
                reason: format!(
                    "decompressed entry exceeds max_class_bytes ({} bytes)",
                    options.max_class_bytes
                ),
            };
            if options.strict {
                return Err(index_err);
            }
            stats.warnings.push(index_err.to_string());
            stats.skipped_entries += 1;
            shrink_reusable_buffer(&mut buf);
            continue;
        }

        let origin = match format {
            ArchiveFormat::Jar => JvmSourceOrigin::JarEntry {
                jar_path: archive_path.to_path_buf(),
                entry_name,
            },
            ArchiveFormat::Jmod => JvmSourceOrigin::JmodEntry {
                jmod_path: archive_path.to_path_buf(),
                entry_name,
            },
        };

        match parse_class_bytes(&buf, origin, options) {
            Ok(Some(unit)) => {
                stats.indexed_types += 1;
                stats.indexed_members += unit.members.len();
                units.push(unit);
            }
            Ok(None) => {
                stats.skipped_entries += 1;
            }
            Err(err) => {
                if options.strict {
                    return Err(err);
                }
                stats.warnings.push(err.to_string());
                stats.skipped_entries += 1;
            }
        }

        shrink_reusable_buffer(&mut buf);
    }

    units.sort_by(|left, right| left.ty.binary_name.cmp(&right.ty.binary_name));
    Ok(ArchiveScanResult { units, stats })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveFormat {
    Jar,
    Jmod,
}

impl ArchiveFormat {
    fn from_path(path: &Path) -> Self {
        match path.extension().and_then(|ext| ext.to_str()) {
            Some(ext) if ext.eq_ignore_ascii_case("jmod") => Self::Jmod,
            _ => Self::Jar,
        }
    }
}

#[derive(Debug, Clone)]
struct CandidateEntry {
    zip_index: usize,
    entry_name: String,
    mr_version: u16,
}

fn select_archive_entries<R: Read + std::io::Seek>(
    archive: &ZipArchive<R>,
    format: ArchiveFormat,
    options: &BytecodeScanOptions,
    stats: &mut ArchiveScanStats,
) -> Vec<(usize, String)> {
    let mut chosen: HashMap<String, CandidateEntry> = HashMap::new();

    for idx in 0..archive.len() {
        let Some(name) = archive.name_for_index(idx) else {
            stats.skipped_entries += 1;
            continue;
        };
        if name.ends_with('/')
            || !Path::new(name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("class"))
        {
            stats.skipped_entries += 1;
            continue;
        }

        let Some((logical_path, mr_version)) = classify_archive_class_path(name, format, options)
        else {
            stats.skipped_entries += 1;
            continue;
        };

        stats.class_entries += 1;

        if options.skip_anonymous_and_synthetic {
            let internal = logical_path.strip_suffix(".class").unwrap_or(logical_path);
            if is_ignored_internal_name(internal) {
                stats.skipped_entries += 1;
                continue;
            }
        }

        let entry_owned = name.to_owned();
        if let Some(existing) = chosen.get_mut(logical_path) {
            // Keep the highest Multi-Release version (or later entry on equal version).
            if mr_version >= existing.mr_version {
                *existing = CandidateEntry {
                    zip_index: idx,
                    entry_name: entry_owned,
                    mr_version,
                };
            }
            stats.skipped_entries += 1;
        } else {
            chosen.insert(
                logical_path.to_owned(),
                CandidateEntry {
                    zip_index: idx,
                    entry_name: entry_owned,
                    mr_version,
                },
            );
        }
    }

    let mut result: Vec<(usize, String)> = chosen
        .into_values()
        .map(|candidate| (candidate.zip_index, candidate.entry_name))
        .collect();
    result.sort_by_key(|(zip_index, _)| *zip_index);
    result
}

fn classify_archive_class_path<'a>(
    entry_name: &'a str,
    format: ArchiveFormat,
    options: &BytecodeScanOptions,
) -> Option<(&'a str, u16)> {
    match format {
        ArchiveFormat::Jmod => {
            let logical = entry_name.strip_prefix("classes/")?;
            if logical.is_empty() {
                None
            } else {
                Some((logical, 0))
            }
        }
        ArchiveFormat::Jar => {
            if let Some(rest) = entry_name.strip_prefix("META-INF/versions/") {
                let target_version = options.target_java_version?;
                let (ver_str, logical) = rest.split_once('/')?;
                if logical.is_empty() {
                    return None;
                }
                let version = ver_str.parse::<u16>().ok()?;
                if version == 0 || version > target_version {
                    return None;
                }
                Some((logical, version))
            } else if entry_name.starts_with("META-INF/") {
                None
            } else {
                Some((entry_name, 0))
            }
        }
    }
}

fn shrink_reusable_buffer(buf: &mut Vec<u8>) {
    buf.clear();
    if buf.capacity() > REUSABLE_BUFFER_SHRINK_THRESHOLD {
        buf.shrink_to(REUSABLE_BUFFER_SHRINK_THRESHOLD);
    }
}

fn origin_label(origin: &JvmSourceOrigin) -> String {
    match origin {
        JvmSourceOrigin::ProjectSource { rel_path, .. } => rel_path.display().to_string(),
        JvmSourceOrigin::JarEntry {
            jar_path,
            entry_name,
        } => format!("{}!/{entry_name}", jar_path.display()),
        JvmSourceOrigin::JmodEntry {
            jmod_path,
            entry_name,
        } => format!("{}!/{entry_name}", jmod_path.display()),
        JvmSourceOrigin::ClassFile { path } => path.display().to_string(),
    }
}

fn find_self_inner_class<'a>(class_file: &'a ClassFile<'a>) -> Option<&'a InnerClassEntry<'a>> {
    class_file.attributes.iter().find_map(|attr| {
        if let AttributeData::InnerClasses(entries) = &attr.data {
            entries
                .iter()
                .find(|entry| entry.inner_class_info == class_file.this_class)
        } else {
            None
        }
    })
}

fn resolve_class_modifiers(
    class_file: &ClassFile<'_>,
    inner_entry: Option<&InnerClassEntry<'_>>,
    is_synthetic: bool,
) -> JvmModifiers {
    let visibility = match inner_entry {
        Some(entry) => {
            if entry.access_flags.contains(InnerClassAccessFlags::PUBLIC) {
                JvmVisibility::Public
            } else if entry
                .access_flags
                .contains(InnerClassAccessFlags::PROTECTED)
            {
                JvmVisibility::Protected
            } else if entry.access_flags.contains(InnerClassAccessFlags::PRIVATE) {
                JvmVisibility::Private
            } else {
                JvmVisibility::PackagePrivate
            }
        }
        None => {
            if class_file.access_flags.contains(ClassAccessFlags::PUBLIC) {
                JvmVisibility::Public
            } else {
                JvmVisibility::PackagePrivate
            }
        }
    };

    let is_static =
        inner_entry.is_some_and(|e| e.access_flags.contains(InnerClassAccessFlags::STATIC));
    let is_final = class_file.access_flags.contains(ClassAccessFlags::FINAL)
        || inner_entry.is_some_and(|e| e.access_flags.contains(InnerClassAccessFlags::FINAL));
    let is_abstract = class_file.access_flags.contains(ClassAccessFlags::ABSTRACT)
        || inner_entry.is_some_and(|e| e.access_flags.contains(InnerClassAccessFlags::ABSTRACT));
    let is_deprecated = has_deprecated_attribute(&class_file.attributes);
    let is_sealed = class_file
        .attributes
        .iter()
        .any(|attr| matches!(attr.data, AttributeData::PermittedSubclasses(_)));

    JvmModifiers {
        visibility,
        is_static,
        is_final,
        is_abstract,
        is_synthetic,
        is_deprecated,
        is_sealed,
    }
}

fn classify_symbol_kind(class_file: &ClassFile<'_>) -> JvmSymbolKind {
    // Check ANNOTATION before INTERFACE because annotation classfiles set both bits in JVMS.
    if class_file
        .access_flags
        .contains(ClassAccessFlags::ANNOTATION)
    {
        JvmSymbolKind::Annotation
    } else if class_file
        .access_flags
        .contains(ClassAccessFlags::INTERFACE)
    {
        JvmSymbolKind::Interface
    } else if class_file.access_flags.contains(ClassAccessFlags::ENUM) {
        JvmSymbolKind::Enum
    } else if is_record_class(class_file) {
        JvmSymbolKind::Record
    } else {
        JvmSymbolKind::Class
    }
}

fn is_record_class(class_file: &ClassFile<'_>) -> bool {
    class_file.super_class.as_deref() == Some("java/lang/Record")
        || class_file
            .attributes
            .iter()
            .any(|attr| matches!(attr.data, AttributeData::Record(_)))
}

fn extract_members(
    class_file: &ClassFile<'_>,
    owner_fqcn: &str,
    owner_simple_name: &str,
    options: &BytecodeScanOptions,
) -> Vec<JvmMemberEntry> {
    let mut members = Vec::with_capacity(class_file.fields.len() + class_file.methods.len());

    for field in &class_file.fields {
        if let Some(entry) = convert_field(field, owner_fqcn, options) {
            members.push(entry);
        }
    }

    for method in &class_file.methods {
        if let Some(entry) = convert_method(method, owner_fqcn, owner_simple_name, options) {
            members.push(entry);
        }
    }

    members
}

fn convert_field(
    field: &FieldInfo<'_>,
    owner_fqcn: &str,
    options: &BytecodeScanOptions,
) -> Option<JvmMemberEntry> {
    let is_synthetic = field.access_flags.contains(FieldAccessFlags::SYNTHETIC)
        || has_synthetic_attribute(&field.attributes);
    if options.skip_anonymous_and_synthetic && (is_synthetic || field.name == "$VALUES") {
        return None;
    }

    let visibility = if field.access_flags.contains(FieldAccessFlags::PUBLIC) {
        JvmVisibility::Public
    } else if field.access_flags.contains(FieldAccessFlags::PROTECTED) {
        JvmVisibility::Protected
    } else if field.access_flags.contains(FieldAccessFlags::PRIVATE) {
        JvmVisibility::Private
    } else {
        JvmVisibility::PackagePrivate
    };

    if !options.include_private_members && visibility == JvmVisibility::Private {
        return None;
    }

    let descriptor = field.descriptor.to_string();
    let display_signature = find_signature_attribute(&field.attributes)
        .and_then(|sig| decode_field_signature(sig).ok())
        .unwrap_or_else(|| {
            decode_field_descriptor(&descriptor).unwrap_or_else(|_| descriptor.clone())
        });

    Some(JvmMemberEntry {
        owner_fqcn: owner_fqcn.to_owned(),
        name: field.name.to_string(),
        kind: JvmSymbolKind::Field,
        modifiers: JvmModifiers {
            visibility,
            is_static: field.access_flags.contains(FieldAccessFlags::STATIC),
            is_final: field.access_flags.contains(FieldAccessFlags::FINAL),
            is_abstract: false,
            is_synthetic,
            is_deprecated: has_deprecated_attribute(&field.attributes),
            is_sealed: false,
        },
        descriptor,
        display_signature,
        line: None,
    })
}

fn convert_method(
    method: &MethodInfo<'_>,
    owner_fqcn: &str,
    owner_simple_name: &str,
    options: &BytecodeScanOptions,
) -> Option<JvmMemberEntry> {
    if method.name == "<clinit>" {
        return None;
    }

    let is_synthetic = method.access_flags.contains(MethodAccessFlags::SYNTHETIC)
        || method.access_flags.contains(MethodAccessFlags::BRIDGE)
        || has_synthetic_attribute(&method.attributes);
    if options.skip_anonymous_and_synthetic && is_synthetic {
        return None;
    }

    let visibility = if method.access_flags.contains(MethodAccessFlags::PUBLIC) {
        JvmVisibility::Public
    } else if method.access_flags.contains(MethodAccessFlags::PROTECTED) {
        JvmVisibility::Protected
    } else if method.access_flags.contains(MethodAccessFlags::PRIVATE) {
        JvmVisibility::Private
    } else {
        JvmVisibility::PackagePrivate
    };

    if !options.include_private_members && visibility == JvmVisibility::Private {
        return None;
    }

    let descriptor = method.descriptor.to_string();
    let generic_sig = find_signature_attribute(&method.attributes);
    let is_constructor = method.name == "<init>";

    let (kind, name, display_signature) = if is_constructor {
        let sig = generic_sig
            .and_then(|raw| decode_constructor_signature(raw).ok())
            .unwrap_or_else(|| {
                decode_constructor_descriptor(&descriptor).unwrap_or_else(|_| descriptor.clone())
            });
        (
            JvmSymbolKind::Constructor,
            owner_simple_name.to_owned(),
            sig,
        )
    } else {
        let sig = generic_sig
            .and_then(|raw| decode_method_signature(raw).ok())
            .unwrap_or_else(|| {
                decode_method_descriptor(&descriptor).unwrap_or_else(|_| descriptor.clone())
            });
        (JvmSymbolKind::Method, method.name.to_string(), sig)
    };

    Some(JvmMemberEntry {
        owner_fqcn: owner_fqcn.to_owned(),
        name,
        kind,
        modifiers: JvmModifiers {
            visibility,
            is_static: method.access_flags.contains(MethodAccessFlags::STATIC),
            is_final: method.access_flags.contains(MethodAccessFlags::FINAL),
            is_abstract: method.access_flags.contains(MethodAccessFlags::ABSTRACT),
            is_synthetic,
            is_deprecated: has_deprecated_attribute(&method.attributes),
            is_sealed: false,
        },
        descriptor,
        display_signature,
        line: extract_first_line_number(&method.attributes),
    })
}

fn find_signature_attribute<'a>(attributes: &'a [AttributeInfo<'a>]) -> Option<&'a str> {
    attributes.iter().find_map(|attr| {
        if let AttributeData::Signature(sig) = &attr.data {
            Some(sig.as_ref())
        } else {
            None
        }
    })
}

fn has_synthetic_attribute(attributes: &[AttributeInfo<'_>]) -> bool {
    attributes
        .iter()
        .any(|attr| matches!(attr.data, AttributeData::Synthetic))
}

fn has_deprecated_attribute(attributes: &[AttributeInfo<'_>]) -> bool {
    attributes
        .iter()
        .any(|attr| matches!(attr.data, AttributeData::Deprecated))
}

fn extract_first_line_number(attributes: &[AttributeInfo<'_>]) -> Option<u32> {
    attributes.iter().find_map(|attr| {
        if let AttributeData::Code(code) = &attr.data {
            code.attributes.iter().find_map(|code_attr| {
                if let AttributeData::LineNumberTable(entries) = &code_attr.data {
                    entries
                        .iter()
                        .map(|entry| u32::from(entry.line_number))
                        .min()
                } else {
                    None
                }
            })
        } else {
            None
        }
    })
}
