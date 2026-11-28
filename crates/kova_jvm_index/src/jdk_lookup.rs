//! On-demand `< 15 ms` JDK `.jmod` / `rt.jar` bytecode lookup and inheritance traversal (`KOVA-052`).
//!
//! Reads only the requested `classes/<internal>.class` entry from the matching `.jmod` archive
//! via ZIP central directory seek + in-memory [`ParsedClassUnit`] cache, avoiding full-JDK
//! scanning on startup.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{BufReader, Cursor, Read as _, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, PoisonError, RwLock};

use zip::ZipArchive;

use crate::bytecode::{
    BytecodeScanOptions, DEFAULT_MAX_CLASS_BYTES, JvmTypeSummary, parse_class_bytes,
    summarize_class_bytes,
};
use crate::model::{JvmSourceOrigin, JvmSymbolKind, JvmVisibility, ParsedClassUnit};
use crate::snapshot::JvmIndexSnapshot;
use crate::source::{SourceScanOptions, parse_java_source};

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

/// Package prefixes whose public JDK types are offered by as-you-type class completion.
const API_PACKAGE_PREFIXES: &[&str] = &["java.", "javax.", "org.w3c.", "org.xml."];

/// Returns `true` for a completable top-level type name (no `$`, not `package-info` /
/// `module-info`, simple name starting with a letter).
#[must_use]
pub fn is_completable_type_name(fqcn: &str) -> bool {
    if fqcn.contains('$') {
        return false;
    }
    let simple = fqcn.rsplit('.').next().unwrap_or(fqcn);
    simple != "package-info"
        && simple != "module-info"
        && simple.chars().next().is_some_and(char::is_alphabetic)
}

/// Returns `true` when `fqcn` is a completable type in a public JDK API package (`java.*`,
/// `javax.*`, `org.w3c.*`, `org.xml.*`).
#[must_use]
pub fn is_public_api_type_name(fqcn: &str) -> bool {
    API_PACKAGE_PREFIXES.iter().any(|p| fqcn.starts_with(p)) && is_completable_type_name(fqcn)
}

/// Type names listed from the JDK archives with the index of the archive holding each one.
#[derive(Debug, Default)]
struct JdkTypeNameTable {
    archives: Vec<PathBuf>,
    names: Vec<(String, usize)>,
}

/// Cached `(method name, named parameter list)` pairs parsed from `lib/src.zip`.
type SourceParamTable = Arc<Vec<(String, String)>>;

/// Fast, cached, on-demand reader for JDK `.jmod` / `rt.jar` classes.
#[derive(Debug, Default)]
pub struct JdkOnDemandIndex {
    jmods_dir: Option<PathBuf>,
    rt_jar: Option<PathBuf>,
    src_zip: Option<PathBuf>,
    jmod_files: RwLock<Option<Vec<PathBuf>>>,
    cache: RwLock<HashMap<String, Option<ParsedClassUnit>>>,
    type_names: OnceLock<JdkTypeNameTable>,
    summaries: RwLock<HashMap<String, Option<JvmTypeSummary>>>,
    source_entries: OnceLock<HashMap<String, String>>,
    source_params: RwLock<HashMap<String, SourceParamTable>>,
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
        let src_zip = [jdk_home.join("lib/src.zip"), jdk_home.join("src.zip")]
            .into_iter()
            .find(|p| p.is_file());
        Self {
            jmods_dir,
            rt_jar,
            src_zip,
            ..Self::default()
        }
    }

    /// Creates an index reading directly from `jmods_dir`.
    #[must_use]
    pub fn from_jmods_dir(jmods_dir: PathBuf) -> Self {
        Self {
            jmods_dir: jmods_dir.is_dir().then_some(jmods_dir),
            ..Self::default()
        }
    }

    /// Uses `src_zip` (a JDK `lib/src.zip`) for parameter names in completion signatures.
    #[must_use]
    pub fn with_src_zip(mut self, src_zip: PathBuf) -> Self {
        self.src_zip = src_zip.is_file().then_some(src_zip);
        self
    }

    /// Whether the public JDK type-name table has already been listed (no disk access needed).
    #[must_use]
    pub fn type_names_ready(&self) -> bool {
        self.type_names.get().is_some()
    }

    /// Lists (once, from ZIP central directories only) every public-API top-level type name in the
    /// configured `.jmod` / `rt.jar` archives (`java.util.Objects`, `java.util.function.ObjIntConsumer`).
    pub fn type_names(&self) -> impl Iterator<Item = &str> + '_ {
        self.type_name_table().names.iter().map(|(n, _)| n.as_str())
    }

    fn type_name_table(&self) -> &JdkTypeNameTable {
        self.type_names.get_or_init(|| {
            let mut archives: Vec<PathBuf> = self
                .jmods_dir
                .as_ref()
                .map(|_| self.ordered_jmods_for(""))
                .unwrap_or_default();
            if let Some(rt) = &self.rt_jar {
                archives.push(rt.clone());
            }
            let mut seen: HashSet<String> = HashSet::new();
            let mut names = Vec::new();
            for (idx, path) in archives.iter().enumerate() {
                let Some(archive) = open_zip_archive(path) else {
                    continue;
                };
                let prefix = archive_class_prefix(path);
                for raw in archive.file_names() {
                    let Some(internal) = raw
                        .strip_prefix(prefix)
                        .and_then(|r| r.strip_suffix(".class"))
                    else {
                        continue;
                    };
                    let fqcn = internal.replace('/', ".");
                    if is_public_api_type_name(&fqcn) && seen.insert(fqcn.clone()) {
                        names.push((fqcn, idx));
                    }
                }
            }
            JdkTypeNameTable { archives, names }
        })
    }

    /// Returns class-header summaries for `fqcns` (listed by [`Self::type_names`]), opening each
    /// archive at most once per call and caching results. Unknown names are skipped.
    #[must_use]
    pub fn type_summaries(&self, fqcns: &[&str]) -> Vec<JvmTypeSummary> {
        let mut missing: Vec<&str> = Vec::new();
        {
            let cache = self
                .summaries
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            for fqcn in fqcns {
                if !cache.contains_key(*fqcn) {
                    missing.push(fqcn);
                }
            }
        }
        if !missing.is_empty() {
            let table = self.type_name_table();
            let wanted: HashSet<&str> = missing.iter().copied().collect();
            let mut by_archive: HashMap<usize, Vec<&str>> = HashMap::new();
            for (name, idx) in &table.names {
                if wanted.contains(name.as_str()) {
                    by_archive.entry(*idx).or_default().push(name.as_str());
                }
            }
            let mut loaded: HashMap<String, Option<JvmTypeSummary>> =
                missing.iter().map(|f| ((*f).to_owned(), None)).collect();
            for (idx, names) in by_archive {
                let Some(path) = table.archives.get(idx) else {
                    continue;
                };
                let Some(mut archive) = open_zip_archive(path) else {
                    continue;
                };
                let prefix = archive_class_prefix(path);
                for name in names {
                    let entry_name = format!("{prefix}{}.class", name.replace('.', "/"));
                    let bytes = {
                        let Ok(mut entry) = archive.by_name(&entry_name) else {
                            continue;
                        };
                        read_zip_entry_bounded(&mut entry, DEFAULT_MAX_CLASS_BYTES)
                    };
                    let summary = bytes.as_deref().and_then(summarize_class_bytes);
                    loaded.insert(name.to_owned(), summary);
                }
            }
            self.summaries
                .write()
                .unwrap_or_else(PoisonError::into_inner)
                .extend(loaded);
        }
        let cache = self
            .summaries
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        fqcns
            .iter()
            .filter_map(|f| cache.get(*f).cloned().flatten())
            .collect()
    }

    /// Returns `(method name, named parameter list)` pairs for `fqcn` parsed from `lib/src.zip`
    /// (e.g. `("checkIndex", "(int index, int length)")`), or an empty list without sources.
    #[must_use]
    pub fn source_parameter_lists(&self, fqcn: &str) -> SourceParamTableView {
        let Some(src_zip) = &self.src_zip else {
            return SourceParamTableView(Arc::new(Vec::new()));
        };
        if let Some(hit) = self
            .source_params
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(fqcn)
        {
            return SourceParamTableView(Arc::clone(hit));
        }
        let table = Arc::new(load_source_params(
            src_zip,
            self.source_entries
                .get_or_init(|| list_source_entries(src_zip)),
            fqcn,
        ));
        self.source_params
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(fqcn.to_owned(), Arc::clone(&table));
        SourceParamTableView(table)
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

            let (members, super_fqcn, interfaces, from_project_snapshot, from_jdk) =
                if let Some(snap) = snapshot
                    && let Some(ty) = snap.resolve_fqcn(&current_fqcn)
                {
                    let is_project = matches!(ty.origin, JvmSourceOrigin::ProjectSource { .. });
                    (
                        snap.members_of(&current_fqcn).to_vec(),
                        ty.super_fqcn.clone(),
                        ty.interfaces.clone(),
                        is_project,
                        false,
                    )
                } else if let Some(unit) = self.lookup_class(&current_fqcn) {
                    (
                        unit.members,
                        unit.ty.super_fqcn,
                        unit.ty.interfaces,
                        false,
                        true,
                    )
                } else {
                    continue;
                };
            let source_params = if from_jdk {
                self.source_parameter_lists(&current_fqcn)
            } else {
                SourceParamTableView(Arc::new(Vec::new()))
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

                let (mut params_signature, return_type) =
                    split_display_signature(member.kind, &member.display_signature);
                if let Some(named) = params_signature
                    .as_deref()
                    .and_then(|p| source_params.match_params(&member.name, p))
                {
                    params_signature = Some(named);
                }
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

fn read_from_archive<R: std::io::Read + Seek>(
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

/// Read-only view over parameter lists parsed from `lib/src.zip` for one class.
#[derive(Debug, Clone)]
pub struct SourceParamTableView(SourceParamTable);

impl SourceParamTableView {
    /// All `(method name, named parameter list)` pairs.
    #[must_use]
    pub fn entries(&self) -> &[(String, String)] {
        &self.0
    }

    /// Finds the named source parameter list matching bytecode `params` (`"(int, int)"`) of
    /// method `name`: same arity and, when overloaded, the same erased simple parameter types.
    #[must_use]
    pub fn match_params(&self, name: &str, params: &str) -> Option<String> {
        let byte_types: Vec<String> = split_param_list(params)
            .iter()
            .map(|p| normalize_param_type(p))
            .collect();
        let same_arity: Vec<&(String, String)> = self
            .0
            .iter()
            .filter(|(n, p)| n == name && split_param_list(p).len() == byte_types.len())
            .collect();
        if byte_types.is_empty() {
            return None;
        }
        let exact = same_arity.iter().find(|(_, p)| {
            split_param_list(p)
                .iter()
                .map(|param| normalize_param_type(strip_param_name(param)))
                .eq(byte_types.iter().cloned())
        });
        exact
            .or_else(|| (same_arity.len() == 1).then(|| &same_arity[0]))
            .map(|(_, p)| p.clone())
    }
}

/// Splits `"(A a, Map<K, V> m)"` into top-level parameters (`["A a", "Map<K, V> m"]`).
fn split_param_list(params: &str) -> Vec<&str> {
    let trimmed = params.trim();
    let inner = trimmed
        .find('(')
        .and_then(|open| trimmed.rfind(')').map(|close| &trimmed[open + 1..close]))
        .unwrap_or(trimmed);
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (idx, ch) in inner.char_indices() {
        match ch {
            '<' | '(' | '[' => depth += 1,
            '>' | ')' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                out.push(inner[start..idx].trim());
                start = idx + 1;
            }
            _ => {}
        }
    }
    let last = inner[start..].trim();
    if !last.is_empty() {
        out.push(last);
    }
    out
}

/// `"final Map<K, V> map"` → `"final Map<K, V>"` (drops the trailing parameter name).
fn strip_param_name(param: &str) -> &str {
    let trimmed = param.trim();
    trimmed
        .rfind(|c: char| c.is_whitespace())
        .map_or(trimmed, |idx| trimmed[..idx].trim_end())
}

/// Erases generics, annotations, `final` and package qualifiers: `"java.util.List<T>"` → `"List"`,
/// `"Object..."` → `"Object[]"`.
fn normalize_param_type(ty: &str) -> String {
    let mut out = String::new();
    let mut depth = 0usize;
    for ch in ty.chars() {
        match ch {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    let words: Vec<&str> = out
        .split_whitespace()
        .filter(|w| !w.starts_with('@') && *w != "final")
        .collect();
    let joined = words.join("").replace("...", "[]");
    joined.rsplit('.').next().unwrap_or(&joined).to_owned()
}

/// Maps `java/util/Objects.java` → entry name (`java.base/java/util/Objects.java`) for `src.zip`.
fn list_source_entries(src_zip: &Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Some(archive) = open_zip_archive(src_zip) else {
        return out;
    };
    for raw in archive.file_names() {
        if !Path::new(raw)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("java"))
        {
            continue;
        }
        let rel = match raw.split_once('/') {
            Some((first, rest)) if first.contains('.') => rest,
            _ => raw,
        };
        out.entry(rel.to_owned()).or_insert_with(|| raw.to_owned());
    }
    out
}

/// Maximum `.java` size read from `src.zip` for parameter names.
const MAX_JDK_SOURCE_BYTES: usize = 2 * 1024 * 1024;

fn load_source_params(
    src_zip: &Path,
    entries: &HashMap<String, String>,
    fqcn: &str,
) -> Vec<(String, String)> {
    let rel = format!("{}.java", fqcn.replace('.', "/"));
    let Some(entry_name) = entries.get(&rel) else {
        return Vec::new();
    };
    let Some(mut archive) = open_zip_archive(src_zip) else {
        return Vec::new();
    };
    let Some(bytes) = archive
        .by_name(entry_name)
        .ok()
        .and_then(|mut e| read_zip_entry_bounded(&mut e, MAX_JDK_SOURCE_BYTES))
    else {
        return Vec::new();
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return Vec::new();
    };
    let opts = SourceScanOptions {
        max_source_bytes: MAX_JDK_SOURCE_BYTES,
        ..SourceScanOptions::default()
    };
    let Ok(units) = parse_java_source(Path::new(&rel), &text, &opts) else {
        return Vec::new();
    };
    units
        .into_iter()
        .filter(|u| u.ty.fqcn == fqcn)
        .flat_map(|u| u.members)
        .filter(|m| matches!(m.kind, JvmSymbolKind::Method | JvmSymbolKind::Constructor))
        .filter_map(|m| {
            let (params, _) = split_display_signature(m.kind, &m.display_signature);
            params.map(|p| (m.name, p))
        })
        .collect()
}

/// `classes/` for `.jmod`, `""` for `.jar` / `rt.jar`.
fn archive_class_prefix(path: &Path) -> &'static str {
    if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("jmod"))
    {
        "classes/"
    } else {
        ""
    }
}

/// `Read + Seek` over a file that hides a leading header (the 4-byte `JM\x01\x00` of `.jmod`).
struct OffsetReader {
    inner: BufReader<File>,
    offset: u64,
}

impl std::io::Read for OffsetReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Seek for OffsetReader {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let abs = match pos {
            SeekFrom::Start(n) => self.inner.seek(SeekFrom::Start(n + self.offset))?,
            other => self.inner.seek(other)?,
        };
        abs.checked_sub(self.offset).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek before archive start",
            )
        })
    }
}

/// Opens a `.jmod` / `.jar` / `.zip` reading only its central directory (no full-file buffering).
///
/// Tries the file as-is first (the `zip` crate detects prepended data whose offsets are absolute),
/// then hides the 4-byte `.jmod` header for archives whose offsets are relative to the ZIP start.
fn open_zip_archive(path: &Path) -> Option<ZipArchive<OffsetReader>> {
    let open = |offset: u64| {
        let mut file = File::open(path).ok()?;
        file.seek(SeekFrom::Start(offset)).ok()?;
        ZipArchive::new(OffsetReader {
            inner: BufReader::new(file),
            offset,
        })
        .ok()
    };
    if let Some(archive) = open(0) {
        return Some(archive);
    }
    let mut magic = [0u8; 4];
    let has_header =
        File::open(path).is_ok_and(|mut f| f.read_exact(&mut magic).is_ok() && magic == JMOD_MAGIC);
    if has_header { open(4) } else { None }
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

    fn build_class(internal: &str, access: u16, methods: &[(&str, &str, u16)]) -> Vec<u8> {
        let mut cp: Vec<Vec<u8>> = Vec::new();
        let utf8 = |cp: &mut Vec<Vec<u8>>, s: &str| {
            let mut e = vec![1];
            e.extend_from_slice(&u16::try_from(s.len()).unwrap().to_be_bytes());
            e.extend_from_slice(s.as_bytes());
            cp.push(e);
            u16::try_from(cp.len()).unwrap()
        };
        let class = |cp: &mut Vec<Vec<u8>>, idx: u16| {
            let mut e = vec![7];
            e.extend_from_slice(&idx.to_be_bytes());
            cp.push(e);
            u16::try_from(cp.len()).unwrap()
        };
        let this_u = utf8(&mut cp, internal);
        let this_c = class(&mut cp, this_u);
        let sup_u = utf8(&mut cp, "java/lang/Object");
        let sup_c = class(&mut cp, sup_u);
        let mut ms = Vec::new();
        for (name, desc, flags) in methods {
            let n = utf8(&mut cp, name);
            let d = utf8(&mut cp, desc);
            ms.push((n, d, *flags));
        }
        let mut out = Vec::new();
        out.extend_from_slice(&0xCAFE_BABEu32.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&61u16.to_be_bytes());
        out.extend_from_slice(&u16::try_from(cp.len() + 1).unwrap().to_be_bytes());
        for e in cp {
            out.extend_from_slice(&e);
        }
        out.extend_from_slice(&access.to_be_bytes());
        out.extend_from_slice(&this_c.to_be_bytes());
        out.extend_from_slice(&sup_c.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&u16::try_from(ms.len()).unwrap().to_be_bytes());
        for (n, d, f) in ms {
            out.extend_from_slice(&f.to_be_bytes());
            out.extend_from_slice(&n.to_be_bytes());
            out.extend_from_slice(&d.to_be_bytes());
            out.extend_from_slice(&0u16.to_be_bytes());
        }
        out.extend_from_slice(&0u16.to_be_bytes());
        out
    }

    fn write_zip(path: &Path, jmod_header: bool, entries: &[(&str, Vec<u8>)]) {
        use std::io::Write as _;
        let mut file = File::create(path).unwrap();
        if jmod_header {
            file.write_all(&JMOD_MAGIC).unwrap();
        }
        let mut zip = zip::ZipWriter::new(file);
        for (name, bytes) in entries {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn lists_public_api_type_names_and_summaries_from_jmod_central_directory() {
        let dir = tempfile::tempdir().unwrap();
        let jmods = dir.path().join("jmods");
        std::fs::create_dir_all(&jmods).unwrap();
        write_zip(
            &jmods.join("java.base.jmod"),
            true,
            &[
                (
                    "classes/java/util/Objects.class",
                    build_class("java/util/Objects", 0x0031, &[]),
                ),
                (
                    "classes/java/util/function/ObjIntConsumer.class",
                    build_class("java/util/function/ObjIntConsumer", 0x0601, &[]),
                ),
                (
                    "classes/java/util/ImmutableHidden.class",
                    build_class("java/util/ImmutableHidden", 0x0030, &[]),
                ),
                (
                    "classes/java/util/Map$Entry.class",
                    build_class("java/util/Map$Entry", 0x0601, &[]),
                ),
                (
                    "classes/jdk/internal/misc/Unsafe.class",
                    build_class("jdk/internal/misc/Unsafe", 0x0031, &[]),
                ),
                ("classes/module-info.class", vec![0]),
            ],
        );
        let index = JdkOnDemandIndex::from_jmods_dir(jmods);
        assert!(!index.type_names_ready());
        let mut names: Vec<&str> = index.type_names().collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec![
                "java.util.ImmutableHidden",
                "java.util.Objects",
                "java.util.function.ObjIntConsumer"
            ]
        );
        assert!(index.type_names_ready());

        let summaries = index.type_summaries(&[
            "java.util.Objects",
            "java.util.function.ObjIntConsumer",
            "java.util.ImmutableHidden",
            "java.util.Missing",
        ]);
        assert_eq!(summaries.len(), 3);
        assert_eq!(summaries[0].kind, JvmSymbolKind::Class);
        assert!(summaries[0].is_public);
        assert_eq!(summaries[1].kind, JvmSymbolKind::Interface);
        assert!(!summaries[2].is_public);
    }

    #[test]
    fn src_zip_supplies_parameter_names_for_jdk_bytecode_members() {
        let dir = tempfile::tempdir().unwrap();
        let jdk = dir.path().join("jdk");
        std::fs::create_dir_all(jdk.join("jmods")).unwrap();
        std::fs::create_dir_all(jdk.join("lib")).unwrap();
        write_zip(
            &jdk.join("jmods/java.base.jmod"),
            true,
            &[(
                "classes/java/util/Objects.class",
                build_class(
                    "java/util/Objects",
                    0x0031,
                    &[
                        ("checkIndex", "(II)I", 0x0009),
                        ("checkIndex", "(JJ)J", 0x0009),
                        ("isNull", "(Ljava/lang/Object;)Z", 0x0009),
                    ],
                ),
            )],
        );
        let source = "package java.util;\n\npublic final class Objects {\n    \
public static int checkIndex(int index, int length) { return index; }\n    \
public static long checkIndex(long index, long length) { return index; }\n    \
public static boolean isNull(Object obj) { return obj == null; }\n}\n";
        write_zip(
            &jdk.join("lib/src.zip"),
            false,
            &[(
                "java.base/java/util/Objects.java",
                source.as_bytes().to_vec(),
            )],
        );
        let index = JdkOnDemandIndex::from_jdk_home(&jdk);
        let members = index.collect_members("java.util.Objects", true, None);
        let sig = |name: &str, needle: &str| {
            members
                .iter()
                .find(|m| {
                    m.name == name
                        && m.params_signature
                            .as_deref()
                            .is_some_and(|p| p.contains(needle))
                })
                .and_then(|m| m.params_signature.clone())
        };
        assert_eq!(
            sig("checkIndex", "int").as_deref(),
            Some("(int index, int length)")
        );
        assert_eq!(
            sig("checkIndex", "long").as_deref(),
            Some("(long index, long length)")
        );
        assert_eq!(sig("isNull", "Object").as_deref(), Some("(Object obj)"));
        assert!(members.iter().all(|m| m.is_static));
    }

    #[test]
    fn source_param_matching_erases_generics_varargs_and_names() {
        let view = SourceParamTableView(Arc::new(vec![
            ("hash".to_owned(), "(Object... values)".to_owned()),
            ("of".to_owned(), "(K k1, V v1)".to_owned()),
            (
                "of".to_owned(),
                "(List<? extends T> list, int size)".to_owned(),
            ),
        ]));
        assert_eq!(
            view.match_params("hash", "(Object[])").as_deref(),
            Some("(Object... values)")
        );
        assert_eq!(
            view.match_params("of", "(java.util.List<T>, int)")
                .as_deref(),
            Some("(List<? extends T> list, int size)")
        );
        assert_eq!(view.match_params("of", "(String)"), None);
        assert_eq!(view.match_params("none", "()"), None);
        assert_eq!(normalize_param_type("final java.util.Map<K, V>"), "Map");
        assert_eq!(strip_param_name("@NonNull String s"), "@NonNull String");
    }
}
