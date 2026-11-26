//! On-demand `< 15 ms` JDK `.jmod` / `rt.jar` bytecode lookup and inheritance traversal (`KOVA-052`).
//!
//! Reads only the requested `classes/<internal>.class` entry from the matching `.jmod` archive
//! via ZIP central directory seek + in-memory [`ParsedClassUnit`] cache, avoiding full-JDK
//! scanning on startup.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{BufReader, Cursor, Read as _};
use std::path::{Path, PathBuf};
use std::sync::{PoisonError, RwLock};

use zip::ZipArchive;

use crate::bytecode::{BytecodeScanOptions, DEFAULT_MAX_CLASS_BYTES, parse_class_bytes};
use crate::model::{JvmSourceOrigin, JvmSymbolKind, JvmVisibility, ParsedClassUnit};
use crate::snapshot::JvmIndexSnapshot;

/// Header preceding the ZIP stream in JDK `.jmod` files.
const JMOD_MAGIC: [u8; 4] = [b'J', b'M', 1, 0];

/// Raw bytecode and archive coordinates for a located `.class` entry (`KOVA-061`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocatedClassBytes {
    /// Canonical or binary FQCN of the located class (e.g. `java.util.Objects`, `java.util.Map$Entry`).
    pub fqcn: String,
    /// Path to the `.jmod` or `.jar` archive on disk.
    pub archive_path: PathBuf,
    /// Exact entry name inside `archive_path` (e.g. `classes/java/util/Map$Entry.class`).
    pub entry_name: String,
    /// Companion outer-class entry name (e.g. `classes/java/util/Map.class`), or `entry_name`
    /// when this is a top-level class or no outer entry exists in the archive.
    pub outer_entry_name: String,
    /// Raw `.class` bytes.
    pub bytes: Vec<u8>,
}

/// Maximum depth when traversing `super_fqcn` and `interfaces` for inherited completion members.
const MAX_INHERITANCE_DEPTH: usize = 8;

/// Completion-ready member extracted from a `.jmod` class or [`JvmIndexSnapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JvmCompletionMember {
    /// Declaring class FQCN (`java.util.Objects`, `java.lang.Object`).
    pub owner_fqcn: String,
    /// Member name (`requireNonNull`, `out`, `toString`).
    pub name: String,
    /// Member kind (`Method`, `Constructor`, or `Field`).
    pub kind: JvmSymbolKind,
    /// Parameter list signature (`Some("(T, String)")`) for methods/constructors.
    pub params_signature: Option<String>,
    /// Return type (`Some("T")`, `Some("void")`) or field type (`Some("PrintStream")`).
    pub return_type: Option<String>,
    /// Raw JVM descriptor (if parsed from bytecode).
    pub descriptor: String,
    /// Whether this member is `static`.
    pub is_static: bool,
    /// Whether this member is marked `@Deprecated`.
    pub is_deprecated: bool,
    /// Base priority (`0` = direct static, `1` = direct instance, `2` = super/interface, `3` = `java.lang.Object`).
    pub sort_priority: u16,
}

/// Splits a decoded `display_signature` from either [`crate::bytecode`] (`"(A, B): R"`) or
/// [`crate::source`] (`"(A, B) -> R"`) into `(params_signature, return_type)`.
#[must_use]
pub fn split_display_signature(
    kind: JvmSymbolKind,
    display_sig: &str,
) -> (Option<String>, Option<String>) {
    let trimmed = display_sig.trim();
    match kind {
        JvmSymbolKind::Field => {
            let ret = (!trimmed.is_empty()).then(|| trimmed.to_owned());
            (None, ret)
        }
        JvmSymbolKind::Constructor => {
            let params = (!trimmed.is_empty()).then(|| trimmed.to_owned());
            (params, None)
        }
        JvmSymbolKind::Method => {
            if let Some((params, ret)) = split_top_level_arrow(trimmed) {
                return (Some(params.to_owned()), Some(ret.to_owned()));
            }
            if let Some((params, ret)) = split_top_level_colon(trimmed) {
                return (Some(params.to_owned()), Some(ret.to_owned()));
            }
            (Some(trimmed.to_owned()), None)
        }
        _ => (None, None),
    }
}

fn split_top_level_arrow(sig: &str) -> Option<(&str, &str)> {
    let bytes = sig.as_bytes();
    let mut paren = 0usize;
    let mut angle = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => paren += 1,
            b')' => paren = paren.saturating_sub(1),
            b'<' => angle += 1,
            b'>' if paren == 0 && angle == 0 => {}
            b'>' => angle = angle.saturating_sub(1),
            b' ' if paren == 0 && angle == 0 && sig[i..].starts_with(" -> ") => {
                let left = sig[..i].trim();
                let right = sig[i + 4..].trim();
                return Some((left, right));
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn split_top_level_colon(sig: &str) -> Option<(&str, &str)> {
    let bytes = sig.as_bytes();
    let mut paren = 0usize;
    let mut angle = 0usize;
    let mut seen_close_paren = false;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => paren += 1,
            b')' => {
                paren = paren.saturating_sub(1);
                if paren == 0 {
                    seen_close_paren = true;
                }
            }
            b'<' => angle += 1,
            b'>' => angle = angle.saturating_sub(1),
            b':' if seen_close_paren && paren == 0 && angle == 0 && sig[i..].starts_with(": ") => {
                let left = sig[..i].trim();
                let right = sig[i + 2..].trim();
                return Some((left, right));
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Fast, cached, on-demand reader for JDK `.jmod` / `rt.jar` classes.
#[derive(Debug, Default)]
pub struct JdkOnDemandIndex {
    jmods_dir: Option<PathBuf>,
    rt_jar: Option<PathBuf>,
    jmod_files: RwLock<Option<Vec<PathBuf>>>,
    cache: RwLock<HashMap<String, Option<ParsedClassUnit>>>,
}

impl JdkOnDemandIndex {
    /// Creates an empty index with no JDK archives configured.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Creates an index backed by `<jdk_home>/jmods` (Java 9+) or `<jdk_home>/jre/lib/rt.jar`.
    #[must_use]
    pub fn from_jdk_home(jdk_home: &Path) -> Self {
        let jmods_candidate = jdk_home.join("jmods");
        let jmods_dir = jmods_candidate.is_dir().then_some(jmods_candidate);
        let rt_jar = [jdk_home.join("jre/lib/rt.jar"), jdk_home.join("lib/rt.jar")]
            .into_iter()
            .find(|p| p.is_file());
        Self {
            jmods_dir,
            rt_jar,
            jmod_files: RwLock::new(None),
            cache: RwLock::new(HashMap::new()),
        }
    }

    /// Creates an index reading directly from `jmods_dir`.
    #[must_use]
    pub fn from_jmods_dir(jmods_dir: PathBuf) -> Self {
        Self {
            jmods_dir: jmods_dir.is_dir().then_some(jmods_dir),
            rt_jar: None,
            jmod_files: RwLock::new(None),
            cache: RwLock::new(HashMap::new()),
        }
    }

    /// Returns `true` when at least one `.jmod` directory or `rt.jar` is configured.
    #[must_use]
    pub const fn has_archives(&self) -> bool {
        self.jmods_dir.is_some() || self.rt_jar.is_some()
    }

    /// Looks up and caches a single class by canonical FQCN (e.g. `java.util.Objects`).
    #[must_use]
    pub fn lookup_class(&self, fqcn: &str) -> Option<ParsedClassUnit> {
        if let Some(cached) = self
            .cache
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(fqcn)
            .cloned()
        {
            return cached;
        }

        let loaded = self.load_class_uncached(fqcn);
        self.cache
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(fqcn.to_owned(), loaded.clone());
        loaded
    }

    /// Collects completion members for `fqcn`, traversing `super_fqcn` and `interfaces` up to the
    /// maximum inheritance depth (8) and filtering by `static_only`.
    ///
    /// Uses `snapshot` first when it already contains a type, falling back to on-demand `.jmod`
    /// reading via [`Self::lookup_class`].
    #[must_use]
    pub fn collect_members(
        &self,
        fqcn: &str,
        static_only: bool,
        snapshot: Option<&JvmIndexSnapshot>,
    ) -> Vec<JvmCompletionMember> {
        let mut out = Vec::new();
        let mut visited_fqcns: HashSet<String> = HashSet::new();
        let mut seen_sigs: HashSet<(String, Option<String>)> = HashSet::new();
        let mut queue: VecDeque<(String, usize)> = VecDeque::new();
        queue.push_back((fqcn.to_owned(), 0));

        while let Some((current_fqcn, depth)) = queue.pop_front() {
            if !visited_fqcns.insert(current_fqcn.clone()) {
                continue;
            }

            let (members, super_fqcn, interfaces, from_project_snapshot) = if let Some(snap) =
                snapshot
                && let Some(ty) = snap.resolve_fqcn(&current_fqcn)
            {
                let is_project = matches!(ty.origin, JvmSourceOrigin::ProjectSource { .. });
                (
                    snap.members_of(&current_fqcn).to_vec(),
                    ty.super_fqcn.clone(),
                    ty.interfaces.clone(),
                    is_project,
                )
            } else if let Some(unit) = self.lookup_class(&current_fqcn) {
                (unit.members, unit.ty.super_fqcn, unit.ty.interfaces, false)
            } else {
                continue;
            };

            let base_priority = if depth == 0 {
                u16::from(!static_only)
            } else if current_fqcn == "java.lang.Object" {
                3
            } else {
                2
            };

            for member in members {
                if member.kind == JvmSymbolKind::Constructor {
                    continue;
                }
                if member.modifiers.is_static != static_only {
                    continue;
                }
                let allowed_vis = if from_project_snapshot {
                    member.modifiers.visibility != JvmVisibility::Private
                } else {
                    member.modifiers.visibility == JvmVisibility::Public
                };
                if !allowed_vis {
                    continue;
                }

                let (params_signature, return_type) =
                    split_display_signature(member.kind, &member.display_signature);
                let key = (member.name.clone(), params_signature.clone());
                if !seen_sigs.insert(key) {
                    continue;
                }

                out.push(JvmCompletionMember {
                    owner_fqcn: member.owner_fqcn,
                    name: member.name,
                    kind: member.kind,
                    params_signature,
                    return_type,
                    descriptor: member.descriptor,
                    is_static: member.modifiers.is_static,
                    is_deprecated: member.modifiers.is_deprecated,
                    sort_priority: base_priority,
                });
            }

            if depth < MAX_INHERITANCE_DEPTH {
                if let Some(sup) = super_fqcn {
                    queue.push_back((sup, depth + 1));
                }
                for iface in interfaces {
                    queue.push_back((iface, depth + 1));
                }
            }
        }

        out
    }

    /// Resolves the canonical field type FQCN of `field_name` on `fqcn` (e.g. `java.lang.System`
    /// + `"out"` → `Some("java.io.PrintStream")`).
    #[must_use]
    pub fn resolve_field_type(
        &self,
        fqcn: &str,
        field_name: &str,
        snapshot: Option<&JvmIndexSnapshot>,
    ) -> Option<String> {
        let mut visited: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<String> = VecDeque::from([fqcn.to_owned()]);
        while let Some(cur) = queue.pop_front() {
            if !visited.insert(cur.clone()) {
                continue;
            }
            let (members, super_fqcn, interfaces) = if let Some(snap) = snapshot
                && let Some(ty) = snap.resolve_fqcn(&cur)
            {
                (
                    snap.members_of(&cur).to_vec(),
                    ty.super_fqcn.clone(),
                    ty.interfaces.clone(),
                )
            } else if let Some(unit) = self.lookup_class(&cur) {
                (unit.members, unit.ty.super_fqcn, unit.ty.interfaces)
            } else {
                continue;
            };

            if let Some(field) = members
                .into_iter()
                .find(|m| m.kind == JvmSymbolKind::Field && m.name == field_name)
            {
                if let Some(fqcn) = descriptor_to_fqcn(&field.descriptor) {
                    return Some(fqcn);
                }
                if !field.display_signature.is_empty() {
                    return Some(field.display_signature);
                }
            }
            if let Some(sup) = super_fqcn {
                queue.push_back(sup);
            }
            queue.extend(interfaces);
        }
        None
    }

    /// Locates and reads raw `.class` bytes for `fqcn` from `.jmod` or `rt.jar` archives without
    /// parsing the full class model.
    #[must_use]
    pub fn locate_class_bytes(&self, fqcn: &str) -> Option<LocatedClassBytes> {
        let entry_candidates = fqcn_entry_candidates(fqcn);
        let jmods = self.ordered_jmods_for(fqcn);

        for jmod_path in jmods {
            if let Some(located) = locate_in_jmod(&jmod_path, &entry_candidates) {
                return Some(located);
            }
        }

        if let Some(rt_path) = &self.rt_jar
            && let Some(located) = locate_in_jar_archive(rt_path, &entry_candidates)
        {
            return Some(located);
        }

        None
    }

    fn load_class_uncached(&self, fqcn: &str) -> Option<ParsedClassUnit> {
        let located = self.locate_class_bytes(fqcn)?;
        let opts = BytecodeScanOptions {
            include_members: true,
            include_private_members: false,
            include_package_private_classes: true,
            skip_anonymous_and_synthetic: true,
            ..BytecodeScanOptions::default()
        };
        let is_jmod = located
            .archive_path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("jmod"));
        let origin = if is_jmod {
            JvmSourceOrigin::JmodEntry {
                jmod_path: located.archive_path,
                entry_name: located.entry_name,
            }
        } else {
            JvmSourceOrigin::JarEntry {
                jar_path: located.archive_path,
                entry_name: located.entry_name,
            }
        };
        parse_class_bytes(&located.bytes, origin, &opts)
            .ok()
            .flatten()
    }

    fn ordered_jmods_for(&self, fqcn: &str) -> Vec<PathBuf> {
        let Some(dir) = &self.jmods_dir else {
            return Vec::new();
        };

        let all_jmods = {
            let cached = self
                .jmod_files
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            if let Some(list) = cached {
                list
            } else {
                let mut list = Vec::new();
                if let Ok(entries) = std::fs::read_dir(dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.extension().and_then(|e| e.to_str()) == Some("jmod") {
                            list.push(path);
                        }
                    }
                }
                list.sort();
                *self
                    .jmod_files
                    .write()
                    .unwrap_or_else(PoisonError::into_inner) = Some(list.clone());
                list
            }
        };

        let preferred_stem = preferred_jmod_stem(fqcn);
        let mut ordered = Vec::with_capacity(all_jmods.len());
        if let Some(pref) = preferred_stem {
            for p in &all_jmods {
                if p.file_stem().and_then(|s| s.to_str()) == Some(pref) {
                    ordered.push(p.clone());
                }
            }
        }
        for p in all_jmods {
            if p.file_stem().and_then(|s| s.to_str()) != preferred_stem {
                ordered.push(p);
            }
        }
        ordered
    }
}

fn preferred_jmod_stem(fqcn: &str) -> Option<&'static str> {
    if fqcn.starts_with("java.lang.")
        || fqcn.starts_with("java.util.")
        || fqcn.starts_with("java.io.")
        || fqcn.starts_with("java.nio.")
        || fqcn.starts_with("java.math.")
        || fqcn.starts_with("java.net.")
        || fqcn.starts_with("java.text.")
        || fqcn.starts_with("java.time.")
        || fqcn.starts_with("java.security.")
        || fqcn.starts_with("javax.crypto.")
        || fqcn.starts_with("javax.net.")
    {
        if fqcn.starts_with("java.net.http.") {
            return Some("java.net.http");
        }
        return Some("java.base");
    }
    if fqcn.starts_with("java.sql.") || fqcn.starts_with("javax.sql.") {
        return Some("java.sql");
    }
    if fqcn.starts_with("java.logging.") {
        return Some("java.logging");
    }
    if fqcn.starts_with("java.xml.") || fqcn.starts_with("javax.xml.") {
        return Some("java.xml");
    }
    if fqcn.starts_with("java.desktop.")
        || fqcn.starts_with("java.awt.")
        || fqcn.starts_with("javax.swing.")
    {
        return Some("java.desktop");
    }
    if fqcn.starts_with("java.management.") || fqcn.starts_with("javax.management.") {
        return Some("java.management");
    }
    None
}

fn locate_in_jmod(jmod_path: &Path, entry_candidates: &[String]) -> Option<LocatedClassBytes> {
    let file = File::open(jmod_path).ok()?;
    if let Ok(mut archive) = ZipArchive::new(BufReader::new(file)) {
        return read_from_archive(&mut archive, jmod_path, "classes/", entry_candidates);
    }

    // Fallback for `.jmod` files whose 4-byte `JM\x01\x00` header causes direct ZIP parsing to fail.
    let mut raw = Vec::new();
    File::open(jmod_path).ok()?.read_to_end(&mut raw).ok()?;
    if raw.starts_with(&JMOD_MAGIC) {
        let mut archive = ZipArchive::new(Cursor::new(&raw[JMOD_MAGIC.len()..])).ok()?;
        return read_from_archive(&mut archive, jmod_path, "classes/", entry_candidates);
    }
    None
}

fn locate_in_jar_archive(
    jar_path: &Path,
    entry_candidates: &[String],
) -> Option<LocatedClassBytes> {
    let file = File::open(jar_path).ok()?;
    let mut archive = ZipArchive::new(BufReader::new(file)).ok()?;
    read_from_archive(&mut archive, jar_path, "", entry_candidates)
}

fn read_from_archive<R: std::io::Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
    archive_path: &Path,
    prefix: &str,
    entry_candidates: &[String],
) -> Option<LocatedClassBytes> {
    for rel_internal in entry_candidates {
        let entry_name = format!("{prefix}{rel_internal}.class");
        let bytes = {
            let Ok(mut entry) = archive.by_name(&entry_name) else {
                continue;
            };
            read_zip_entry_bounded(&mut entry, DEFAULT_MAX_CLASS_BYTES)?
        };
        let outer_entry_name = if let Some((outer_internal, _)) = rel_internal.split_once('$') {
            let outer_candidate = format!("{prefix}{outer_internal}.class");
            if archive.by_name(&outer_candidate).is_ok() {
                outer_candidate
            } else {
                entry_name.clone()
            }
        } else {
            entry_name.clone()
        };
        return Some(LocatedClassBytes {
            fqcn: rel_internal.replace('/', "."),
            archive_path: archive_path.to_path_buf(),
            entry_name,
            outer_entry_name,
            bytes,
        });
    }
    None
}

pub(crate) fn read_zip_entry_bounded<R: std::io::Read>(
    entry: &mut R,
    max_bytes: usize,
) -> Option<Vec<u8>> {
    let limit = u64::try_from(max_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut bytes = Vec::with_capacity(4096);
    entry.take(limit).read_to_end(&mut bytes).ok()?;
    if bytes.len() > max_bytes {
        return None;
    }
    Some(bytes)
}

pub(crate) fn fqcn_entry_candidates(fqcn: &str) -> Vec<String> {
    let slash = fqcn.replace('.', "/");
    let mut out = vec![slash];
    // Also support nested classes (`java.util.Map.Entry` -> `java/util/Map$Entry`).
    let parts: Vec<&str> = fqcn.split('.').collect();
    for split_idx in (1..parts.len()).rev() {
        let pkg_and_outer = parts[..split_idx].join("/");
        let inner = parts[split_idx..].join("$");
        let cand = format!("{pkg_and_outer}${inner}");
        if !out.contains(&cand) {
            out.push(cand);
        }
    }
    out
}

fn descriptor_to_fqcn(descriptor: &str) -> Option<String> {
    let trimmed = descriptor.trim();
    if let Some(inner) = trimmed.strip_prefix('L').and_then(|s| s.strip_suffix(';')) {
        return Some(inner.replace(['/', '$'], "."));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::JvmMemberEntry;

    #[test]
    fn split_display_signature_handles_bytecode_and_source_formats() {
        assert_eq!(
            split_display_signature(JvmSymbolKind::Method, "(Object, Object): boolean"),
            (
                Some("(Object, Object)".to_owned()),
                Some("boolean".to_owned())
            )
        );
        assert_eq!(
            split_display_signature(
                JvmSymbolKind::Method,
                "<T extends Comparable<T>>(String, int): List<T>"
            ),
            (
                Some("<T extends Comparable<T>>(String, int)".to_owned()),
                Some("List<T>".to_owned())
            )
        );
        assert_eq!(
            split_display_signature(JvmSymbolKind::Method, "(String id) -> Optional<Order>"),
            (
                Some("(String id)".to_owned()),
                Some("Optional<Order>".to_owned())
            )
        );
        assert_eq!(
            split_display_signature(JvmSymbolKind::Constructor, "(long)"),
            (Some("(long)".to_owned()), None)
        );
        assert_eq!(
            split_display_signature(JvmSymbolKind::Field, "PrintStream"),
            (None, Some("PrintStream".to_owned()))
        );
    }

    #[test]
    fn collect_members_from_snapshot_traverses_superclass_and_skips_constructors() {
        let object_unit = ParsedClassUnit {
            ty: crate::model::JvmTypeEntry {
                fqcn: "java.lang.Object".to_owned(),
                binary_name: "java.lang.Object".to_owned(),
                simple_name: "Object".to_owned(),
                package_name: "java.lang".to_owned(),
                kind: JvmSymbolKind::Class,
                modifiers: crate::model::JvmModifiers {
                    visibility: JvmVisibility::Public,
                    ..Default::default()
                },
                super_fqcn: None,
                interfaces: Vec::new(),
                origin: JvmSourceOrigin::ClassFile {
                    path: PathBuf::from("Object.class"),
                },
            },
            members: vec![JvmMemberEntry {
                owner_fqcn: "java.lang.Object".to_owned(),
                name: "toString".to_owned(),
                kind: JvmSymbolKind::Method,
                modifiers: crate::model::JvmModifiers {
                    visibility: JvmVisibility::Public,
                    ..Default::default()
                },
                descriptor: "()Ljava/lang/String;".to_owned(),
                display_signature: "(): String".to_owned(),
                line: None,
            }],
        };

        let string_unit = ParsedClassUnit {
            ty: crate::model::JvmTypeEntry {
                fqcn: "java.lang.String".to_owned(),
                binary_name: "java.lang.String".to_owned(),
                simple_name: "String".to_owned(),
                package_name: "java.lang".to_owned(),
                kind: JvmSymbolKind::Class,
                modifiers: crate::model::JvmModifiers {
                    visibility: JvmVisibility::Public,
                    ..Default::default()
                },
                super_fqcn: Some("java.lang.Object".to_owned()),
                interfaces: Vec::new(),
                origin: JvmSourceOrigin::ClassFile {
                    path: PathBuf::from("String.class"),
                },
            },
            members: vec![
                JvmMemberEntry {
                    owner_fqcn: "java.lang.String".to_owned(),
                    name: "String".to_owned(),
                    kind: JvmSymbolKind::Constructor,
                    modifiers: crate::model::JvmModifiers {
                        visibility: JvmVisibility::Public,
                        ..Default::default()
                    },
                    descriptor: "()V".to_owned(),
                    display_signature: "()".to_owned(),
                    line: None,
                },
                JvmMemberEntry {
                    owner_fqcn: "java.lang.String".to_owned(),
                    name: "length".to_owned(),
                    kind: JvmSymbolKind::Method,
                    modifiers: crate::model::JvmModifiers {
                        visibility: JvmVisibility::Public,
                        ..Default::default()
                    },
                    descriptor: "()I".to_owned(),
                    display_signature: "(): int".to_owned(),
                    line: None,
                },
            ],
        };

        let snap = JvmIndexSnapshot::from_units(&[object_unit, string_unit]);
        let index = JdkOnDemandIndex::empty();
        let members = index.collect_members("java.lang.String", false, Some(&snap));
        let names: Vec<&str> = members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["length", "toString"]);
        assert_eq!(members[0].sort_priority, 1);
        assert_eq!(members[1].sort_priority, 3);
    }
}
