//! *Go to Definition* with automatic Vineflower decompilation of library classes (KOVA-037).
//!
//! When `jdt.ls` answers `textDocument/definition` with a compiled class (`jdt://contents/...`,
//! `jar:file:///...!/...class`, `jrt:/module/...class`), [`DefinitionResolver`] locates the
//! physical `.jar` / `.jmod`, runs [`VineflowerDecompiler`] (KOVA-035) and returns a read-only
//! [`DefinitionTarget::DecompiledClass`] cached under `~/.cache/kova/decompiled/`. When no
//! archive can be located it asks `jdt.ls` itself through `java/classFileContents`.

use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kova_buffer::{Utf16Point, Utf16Range};
use kova_config::JvmSettings;
use kova_toolchain::{DecompileResult, DecompileTarget, VineflowerDecompiler};
use lsp_types::GotoDefinitionResponse;
use serde_json::json;
use url::Url;

use crate::error::LspDapError;
use crate::lsp::lifecycle::JdtlsClient;
use crate::lsp::sync::{lsp_position, utf16_range};

/// A definition URI as returned by `jdt.ls` or the JDK.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedClassUri {
    /// An ordinary file (`file:///...`).
    File(PathBuf),
    /// A `jdt.ls` virtual document (`jdt://contents/<archive>/<package>/<Type>.class?...`).
    JdtContents {
        /// Archive file name (e.g. `rt.jar`, `guava-33.0.jar`).
        jar_or_archive_hint: Option<String>,
        /// Absolute archive path recovered from the URI query, when present.
        archive_path: Option<PathBuf>,
        /// `/`-separated package path (`java/lang`); empty for the default package.
        package_path: String,
        /// Last segment, `Type.class` or `Type.java` (attached sources).
        class_file_name: String,
        /// Fully qualified (binary) class name, e.g. `java.lang.String`.
        fqcn: String,
        /// The original URI.
        raw_uri: String,
    },
    /// A class inside a jar (`jar:file:///lib.jar!/com/example/Foo.class`).
    JarEntry {
        /// Path of the jar.
        jar_path: PathBuf,
        /// `/`-separated entry name.
        entry_name: String,
    },
    /// A class of the JDK runtime image (`jrt:/java.base/java/util/List.class`).
    JrtModule {
        /// Module name (`java.base`).
        module_name: String,
        /// `/`-separated entry name inside the module.
        entry_name: String,
    },
}

impl ParsedClassUri {
    /// Fully qualified (binary) class name of a class URI (`None` for plain files).
    #[must_use]
    pub fn fqcn(&self) -> Option<String> {
        match self {
            Self::File(_) => None,
            Self::JdtContents { fqcn, .. } => Some(fqcn.clone()),
            Self::JarEntry { entry_name, .. } | Self::JrtModule { entry_name, .. } => {
                Some(entry_to_fqcn(entry_name))
            }
        }
    }

    /// Whether a `jdt://` URI points at attached sources (`.java`) rather than bytecode.
    #[must_use]
    pub fn is_attached_source(&self) -> bool {
        matches!(self, Self::JdtContents { class_file_name, .. }
            if Path::new(class_file_name).extension().is_some_and(|e| e.eq_ignore_ascii_case("java")))
    }
}

fn entry_to_fqcn(entry_name: &str) -> String {
    let entry = entry_name.strip_prefix("classes/").unwrap_or(entry_name);
    entry
        .strip_suffix(".class")
        .or_else(|| entry.strip_suffix(".java"))
        .unwrap_or(entry)
        .replace('/', ".")
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Decodes `%XX` escapes (invalid escapes are kept verbatim).
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let (Some(hi), Some(lo)) = (
                bytes.get(i + 1).copied().and_then(hex_value),
                bytes.get(i + 2).copied().and_then(hex_value),
            )
        {
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn invalid_uri(uri: &str, reason: &str) -> LspDapError {
    LspDapError::InvalidUri {
        uri: uri.to_owned(),
        reason: reason.to_owned(),
    }
}

/// Recovers the archive path from a JDT memento query
/// (`=project/\/home\/u\/lib.jar<java.lang(String.class`): the project name runs up to the
/// first unescaped `/`, the archive path up to the next unescaped memento delimiter.
fn archive_from_jdt_query(query: &str) -> Option<PathBuf> {
    let decoded = percent_decode(query);
    let mut chars = decoded.trim_start_matches('=').chars();
    let mut in_project = true;
    let mut path = String::new();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(escaped) if !in_project => path.push(escaped),
                Some(_) => {}
                None => break,
            }
        } else if in_project {
            in_project = c != '/';
        } else if matches!(
            c,
            '<' | '(' | '[' | '{' | '!' | '^' | '~' | '|' | '#' | '@' | '='
        ) {
            break;
        } else {
            path.push(c);
        }
    }
    let path = PathBuf::from(path);
    let is_archive = path
        .extension()
        .is_some_and(|ext| ext == "jar" || ext == "jmod" || ext == "zip");
    (!in_project && path.is_absolute() && is_archive).then_some(path)
}

fn parse_jdt_contents(uri: &str, rest: &str) -> Result<ParsedClassUri, LspDapError> {
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let segments: Vec<String> = path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(percent_decode)
        .collect();
    let [archive, package @ .., class_file_name] = segments.as_slice() else {
        return Err(invalid_uri(
            uri,
            "expected jdt://contents/<archive>/.../<Type>.class",
        ));
    };
    if package.is_empty() && !class_file_name.contains('.') {
        return Err(invalid_uri(uri, "missing class file name"));
    }
    let package_path = package.join("/").replace('.', "/");
    let stem = class_file_name
        .strip_suffix(".class")
        .or_else(|| class_file_name.strip_suffix(".java"))
        .ok_or_else(|| invalid_uri(uri, "expected a .class or .java document"))?;
    let fqcn = if package_path.is_empty() {
        stem.to_owned()
    } else {
        format!("{}.{stem}", package_path.replace('/', "."))
    };
    Ok(ParsedClassUri::JdtContents {
        jar_or_archive_hint: Some(archive.clone()),
        archive_path: archive_from_jdt_query(query),
        package_path,
        class_file_name: class_file_name.clone(),
        fqcn,
        raw_uri: uri.to_owned(),
    })
}

/// Parses a definition URI.
///
/// # Errors
/// [`LspDapError::InvalidUri`] for unsupported schemes or malformed class URIs.
pub fn parse_class_uri(uri: &str) -> Result<ParsedClassUri, LspDapError> {
    if let Some(rest) = uri.strip_prefix("jdt://contents/") {
        return parse_jdt_contents(uri, rest);
    }
    if let Some(rest) = uri.strip_prefix("jar:") {
        let (archive, entry) = rest
            .split_once("!/")
            .ok_or_else(|| invalid_uri(uri, "missing `!/` entry separator"))?;
        let jar_path = Url::parse(archive)
            .ok()
            .filter(|url| url.scheme() == "file")
            .and_then(|url| url.to_file_path().ok())
            .ok_or_else(|| invalid_uri(uri, "archive must be a local file:// URL"))?;
        return Ok(ParsedClassUri::JarEntry {
            jar_path,
            entry_name: percent_decode(entry),
        });
    }
    if let Some(rest) = uri.strip_prefix("jrt:") {
        let decoded = percent_decode(rest.trim_start_matches('/'));
        let (module_name, entry_name) = decoded
            .split_once('/')
            .filter(|(module, entry)| !module.is_empty() && !entry.is_empty())
            .ok_or_else(|| invalid_uri(uri, "expected jrt:/<module>/<entry>"))?;
        return Ok(ParsedClassUri::JrtModule {
            module_name: module_name.to_owned(),
            entry_name: entry_name.to_owned(),
        });
    }
    let url = Url::parse(uri).map_err(|err| invalid_uri(uri, &err.to_string()))?;
    if url.scheme() == "file" {
        return url
            .to_file_path()
            .map(ParsedClassUri::File)
            .map_err(|()| invalid_uri(uri, "not a local file path"));
    }
    Err(invalid_uri(uri, "unsupported definition URI scheme"))
}

/// Where *Go to Definition* leads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefinitionTarget {
    /// A source file (project file, or read-only attached sources of a library).
    SourceFile {
        /// File to open.
        path: PathBuf,
        /// Range of the symbol.
        range: Utf16Range,
        /// Whether the file must be opened read-only.
        read_only: bool,
    },
    /// A library / JDK class decompiled automatically (always read-only).
    DecompiledClass {
        /// Fully qualified name of the decompiled top-level class.
        fqcn: String,
        /// Archive the class came from, when known.
        archive_path: Option<PathBuf>,
        /// Cache file under `~/.cache/kova/decompiled/`.
        cache_file: PathBuf,
        /// Decompiled source.
        source_text: String,
        /// Range of the symbol in `source_text`.
        range: Utf16Range,
        /// Whether the source was served from the cache.
        from_cache: bool,
        /// Always `true`.
        read_only: bool,
    },
}

/// Resolves a fully qualified class name to a decompilable archive entry
/// (implemented for `kova_jvm_index::JvmIndexSnapshot` by the integration layer).
pub trait ClassArchiveLocator: Send + Sync {
    /// Locates the archive entry of `fqcn`, if known.
    fn resolve_fqcn_archive(&self, fqcn: &str) -> Option<DecompileTarget>;
}

/// Resolves `textDocument/definition` results, decompiling library classes on demand.
#[derive(Clone)]
pub struct DefinitionResolver {
    auto_decompile_class: bool,
    cache_root: PathBuf,
    decompiler: Option<VineflowerDecompiler>,
    jmods_dir: Option<PathBuf>,
    locator: Option<Arc<dyn ClassArchiveLocator>>,
}

impl fmt::Debug for DefinitionResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DefinitionResolver")
            .field("auto_decompile_class", &self.auto_decompile_class)
            .field("cache_root", &self.cache_root)
            .field("decompiler", &self.decompiler.is_some())
            .field("jmods_dir", &self.jmods_dir)
            .field("locator", &self.locator.is_some())
            .finish()
    }
}

impl DefinitionResolver {
    /// Creates a resolver caching `java/classFileContents` sources under `cache_root`
    /// (normally `~/.cache/kova/decompiled`).
    #[must_use]
    pub fn new(cache_root: impl Into<PathBuf>, auto_decompile_class: bool) -> Self {
        Self {
            auto_decompile_class,
            cache_root: cache_root.into(),
            decompiler: None,
            jmods_dir: None,
            locator: None,
        }
    }

    /// Creates a resolver from `[jvm]` settings and a Vineflower decompiler; its cache root
    /// is shared.
    #[must_use]
    pub fn from_settings(settings: &JvmSettings, decompiler: VineflowerDecompiler) -> Self {
        Self::new(decompiler.cache_root(), settings.auto_decompile_class)
            .with_decompiler(decompiler)
    }

    /// Sets the Vineflower decompiler.
    #[must_use]
    pub fn with_decompiler(mut self, decompiler: VineflowerDecompiler) -> Self {
        self.decompiler = Some(decompiler);
        self
    }

    /// Sets the `jmods` directory of the runtime JDK (resolves `jrt:/` URIs).
    #[must_use]
    pub fn with_jmods_dir(mut self, jmods_dir: impl Into<PathBuf>) -> Self {
        self.jmods_dir = Some(jmods_dir.into());
        self
    }

    /// Sets the fallback FQCN → archive locator.
    #[must_use]
    pub fn with_locator(mut self, locator: Arc<dyn ClassArchiveLocator>) -> Self {
        self.locator = Some(locator);
        self
    }

    /// Root of the read-only source cache.
    #[must_use]
    pub fn cache_root(&self) -> &Path {
        &self.cache_root
    }

    /// Sends `textDocument/definition` and resolves every returned location.
    ///
    /// # Errors
    /// Request errors and the first resolution error of [`Self::resolve_location`].
    pub async fn go_to_definition(
        &self,
        client: &JdtlsClient,
        uri: &str,
        pos: Utf16Point,
    ) -> Result<Vec<DefinitionTarget>, LspDapError> {
        let params = json!({
            "textDocument": { "uri": uri },
            "position": lsp_position(pos.row, pos.column)?,
        });
        let response: Option<GotoDefinitionResponse> =
            client.request("textDocument/definition", params).await?;
        let locations: Vec<(String, lsp_types::Range)> = match response {
            None => Vec::new(),
            Some(GotoDefinitionResponse::Scalar(location)) => {
                vec![(location.uri.as_str().to_owned(), location.range)]
            }
            Some(GotoDefinitionResponse::Array(locations)) => locations
                .into_iter()
                .map(|l| (l.uri.as_str().to_owned(), l.range))
                .collect(),
            Some(GotoDefinitionResponse::Link(links)) => links
                .into_iter()
                .map(|l| (l.target_uri.as_str().to_owned(), l.target_selection_range))
                .collect(),
        };
        let mut targets = Vec::with_capacity(locations.len());
        for (target_uri, range) in locations {
            targets.push(
                self.resolve_location(client, &target_uri, utf16_range(range))
                    .await?,
            );
        }
        Ok(targets)
    }

    /// Resolves one definition location (alias of the spec's `resolve_decompiled_uri`).
    ///
    /// # Errors
    /// [`LspDapError::InvalidUri`] for unsupported URIs and [`LspDapError::DecompilationFailed`]
    /// when a class can neither be decompiled nor fetched from `jdt.ls`.
    pub async fn resolve_location(
        &self,
        client: &JdtlsClient,
        uri: &str,
        range: Utf16Range,
    ) -> Result<DefinitionTarget, LspDapError> {
        let parsed = match parse_class_uri(uri) {
            Ok(parsed) => parsed,
            // Unknown `jdt://` flavours are still understood by `jdt.ls` itself.
            Err(_) if uri.starts_with("jdt://") => {
                return self.class_file_contents(client, uri, None, range).await;
            }
            Err(err) => return Err(err),
        };
        if let ParsedClassUri::File(path) = parsed {
            let read_only = path.starts_with(&self.cache_root);
            return Ok(DefinitionTarget::SourceFile {
                path,
                range,
                read_only,
            });
        }
        let is_jdt = matches!(parsed, ParsedClassUri::JdtContents { .. });
        if parsed.is_attached_source() || (is_jdt && !self.auto_decompile_class) {
            return self
                .class_file_contents(client, uri, Some(&parsed), range)
                .await;
        }
        if !self.auto_decompile_class {
            return Err(LspDapError::DecompilationFailed {
                target: uri.to_owned(),
                reason: "jvm.auto_decompile_class is disabled".to_owned(),
            });
        }
        let decompiled = match (self.decompile_target(&parsed), &self.decompiler) {
            (Some(target), Some(decompiler)) => Some(
                decompiler
                    .decompile(&target)
                    .await
                    .map(|result| (target, result))
                    .map_err(|err| LspDapError::DecompilationFailed {
                        target: uri.to_owned(),
                        reason: err.to_string(),
                    }),
            ),
            _ => None,
        };
        match decompiled {
            Some(Ok((target, result))) => Ok(decompiled_target(&parsed, &target, result)),
            Some(Err(_)) | None if is_jdt => {
                self.class_file_contents(client, uri, Some(&parsed), range)
                    .await
            }
            Some(Err(err)) => Err(err),
            None => Err(LspDapError::DecompilationFailed {
                target: uri.to_owned(),
                reason: "no archive or Vineflower decompiler available for this class".to_owned(),
            }),
        }
    }

    fn decompile_target(&self, parsed: &ParsedClassUri) -> Option<DecompileTarget> {
        let direct = match parsed {
            ParsedClassUri::File(_) => None,
            ParsedClassUri::JarEntry {
                jar_path,
                entry_name,
            } => jar_path
                .is_file()
                .then(|| archive_entry(jar_path, entry_name.clone())),
            ParsedClassUri::JrtModule {
                module_name,
                entry_name,
            } => self
                .jmods_dir
                .as_ref()
                .map(|dir| dir.join(format!("{module_name}.jmod")))
                .filter(|jmod| jmod.is_file())
                .map(|jmod| archive_entry(&jmod, entry_name.clone())),
            ParsedClassUri::JdtContents {
                archive_path,
                package_path,
                class_file_name,
                ..
            } => archive_path
                .as_ref()
                .filter(|path| path.is_file())
                .map(|path| {
                    let entry = if package_path.is_empty() {
                        class_file_name.clone()
                    } else {
                        format!("{package_path}/{class_file_name}")
                    };
                    archive_entry(path, entry)
                }),
        };
        direct.or_else(|| {
            let fqcn = parsed.fqcn()?;
            self.locator.as_ref()?.resolve_fqcn_archive(&fqcn)
        })
    }

    /// Fetches the class source from `jdt.ls` (`java/classFileContents`) and caches it.
    async fn class_file_contents(
        &self,
        client: &JdtlsClient,
        uri: &str,
        parsed: Option<&ParsedClassUri>,
        range: Utf16Range,
    ) -> Result<DefinitionTarget, LspDapError> {
        let fqcn = parsed
            .and_then(ParsedClassUri::fqcn)
            .unwrap_or_else(|| "unknown.Unknown".to_owned());
        let cache_file = self.contents_cache_file(uri, &fqcn);
        let (source_text, from_cache) = if cache_file.is_file() {
            let text = fs::read_to_string(&cache_file)
                .map_err(|err| LspDapError::io(cache_file.display().to_string(), &err))?;
            (text, true)
        } else {
            let text: Option<String> = client
                .request("java/classFileContents", json!({ "uri": uri }))
                .await?;
            let text = text.filter(|t| !t.trim().is_empty()).ok_or_else(|| {
                LspDapError::DecompilationFailed {
                    target: uri.to_owned(),
                    reason: "jdt.ls returned no class contents".to_owned(),
                }
            })?;
            write_atomic(&cache_file, &text)?;
            (text, false)
        };
        if parsed.is_some_and(ParsedClassUri::is_attached_source) {
            return Ok(DefinitionTarget::SourceFile {
                path: cache_file,
                range,
                read_only: true,
            });
        }
        let archive_path = match parsed {
            Some(ParsedClassUri::JdtContents { archive_path, .. }) => archive_path.clone(),
            Some(ParsedClassUri::JarEntry { jar_path, .. }) => Some(jar_path.clone()),
            _ => None,
        };
        Ok(DefinitionTarget::DecompiledClass {
            fqcn,
            archive_path,
            cache_file,
            source_text,
            range,
            from_cache,
            read_only: true,
        })
    }

    fn contents_cache_file(&self, uri: &str, fqcn: &str) -> PathBuf {
        let key = format!("{:016x}", fnv1a64(uri.as_bytes()));
        let outer = fqcn.split('$').next().unwrap_or(fqcn);
        let mut path = self.cache_root.join("jdtls").join(key);
        let mut segments: Vec<&str> = outer.split('.').collect();
        let class_name = segments.pop().unwrap_or("Unknown");
        for segment in segments {
            path.push(safe_segment(segment));
        }
        path.push(format!("{}.java", safe_segment(class_name)));
        path
    }
}

/// Convenience wrapper around [`DefinitionResolver::go_to_definition`].
///
/// # Errors
/// See [`DefinitionResolver::go_to_definition`].
pub async fn go_to_definition(
    client: &JdtlsClient,
    resolver: &DefinitionResolver,
    uri: &str,
    pos: Utf16Point,
) -> Result<Vec<DefinitionTarget>, LspDapError> {
    resolver.go_to_definition(client, uri, pos).await
}

fn archive_entry(archive: &Path, entry_name: String) -> DecompileTarget {
    DecompileTarget::ArchiveEntry {
        archive_path: archive.to_path_buf(),
        entry_name,
    }
}

fn decompiled_target(
    parsed: &ParsedClassUri,
    target: &DecompileTarget,
    result: DecompileResult,
) -> DefinitionTarget {
    let requested = parsed.fqcn().unwrap_or_else(|| result.fqcn.clone());
    let simple = simple_class_name(&requested);
    let range = find_declaration(&result.source_code, simple).unwrap_or_default();
    let archive_path = match target {
        DecompileTarget::ArchiveEntry { archive_path, .. } => Some(archive_path.clone()),
        DecompileTarget::ClassFile { .. } => None,
    };
    DefinitionTarget::DecompiledClass {
        fqcn: result.fqcn,
        archive_path,
        cache_file: result.cache_file,
        source_text: result.source_code,
        range,
        from_cache: result.from_cache,
        read_only: true,
    }
}

/// Simple name of a binary class name (`a.b.Outer$Inner` → `Inner`; anonymous → `Outer`).
fn simple_class_name(fqcn: &str) -> &str {
    let simple = fqcn.rsplit('.').next().unwrap_or(fqcn);
    let mut parts = simple.split('$');
    let outer = parts.next().unwrap_or(simple);
    parts
        .rev()
        .find(|p| !p.is_empty() && !p.chars().all(|c| c.is_ascii_digit()))
        .unwrap_or(outer)
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// Finds the declaration of type `name` in `source` (Gate 2 Q-2): the range of the name after
/// `class` / `interface` / `enum` / `record`.
#[must_use]
pub fn find_declaration(source: &str, name: &str) -> Option<Utf16Range> {
    if name.is_empty() {
        return None;
    }
    for (row, line) in source.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*') {
            continue;
        }
        for keyword in ["class ", "interface ", "enum ", "record "] {
            let needle = format!("{keyword}{name}");
            let mut from = 0;
            while let Some(found) = line[from..].find(&needle) {
                let start = from + found;
                let end = start + needle.len();
                let before_ok = line[..start]
                    .chars()
                    .next_back()
                    .is_none_or(|c| !is_ident_char(c));
                let after_ok = line[end..].chars().next().is_none_or(|c| !is_ident_char(c));
                if before_ok && after_ok {
                    let column = line[..start + keyword.len()].encode_utf16().count();
                    let width = name.encode_utf16().count();
                    return Some(Utf16Range::new(
                        Utf16Point::new(row, column),
                        Utf16Point::new(row, column + width),
                    ));
                }
                from = end;
            }
        }
    }
    None
}

fn safe_segment(segment: &str) -> String {
    let cleaned: String = segment
        .chars()
        .map(|c| if is_ident_char(c) || c == '-' { c } else { '_' })
        .collect();
    if cleaned.is_empty() || cleaned.chars().all(|c| c == '.') {
        "_".to_owned()
    } else {
        cleaned
    }
}

/// 64-bit FNV-1a: a stable, dependency-free cache key.
fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn write_atomic(path: &Path, text: &str) -> Result<(), LspDapError> {
    let io_err = |err: &std::io::Error| LspDapError::io(path.display().to_string(), err);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| io_err(&err))?;
    }
    let tmp = path.with_extension(format!("java.tmp-{}", std::process::id()));
    let mut file = fs::File::create(&tmp).map_err(|err| io_err(&err))?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|err| io_err(&err))?;
    drop(file);
    fs::rename(&tmp, path).map_err(|err| {
        let _removed = fs::remove_file(&tmp);
        io_err(&err)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_jdt_contents_with_dotted_and_slashed_packages() {
        let dotted = parse_class_uri(
            "jdt://contents/rt.jar/java.lang/String.class?=demo/%5C/opt%5C/jdk%5C/rt.jar%3Cjava.lang(String.class",
        );
        let Ok(ParsedClassUri::JdtContents {
            jar_or_archive_hint,
            archive_path,
            package_path,
            class_file_name,
            fqcn,
            ..
        }) = dotted
        else {
            panic!("unexpected {dotted:?}");
        };
        assert_eq!(jar_or_archive_hint.as_deref(), Some("rt.jar"));
        assert_eq!(archive_path, Some(PathBuf::from("/opt/jdk/rt.jar")));
        assert_eq!(package_path, "java/lang");
        assert_eq!(class_file_name, "String.class");
        assert_eq!(fqcn, "java.lang.String");

        let slashed = parse_class_uri("jdt://contents/lib%20x.jar/com/example/Foo%24Bar.class");
        assert_eq!(
            slashed
                .as_ref()
                .ok()
                .and_then(ParsedClassUri::fqcn)
                .as_deref(),
            Some("com.example.Foo$Bar")
        );
        assert!(matches!(
            slashed,
            Ok(ParsedClassUri::JdtContents { ref jar_or_archive_hint, archive_path: None, .. })
                if jar_or_archive_hint.as_deref() == Some("lib x.jar")
        ));

        let sources =
            parse_class_uri("jdt://contents/guava.jar/com.google.common.base/Strings.java");
        assert!(sources.is_ok_and(|p| p.is_attached_source()));
    }

    #[test]
    fn parses_jar_jrt_and_file_uris() {
        assert_eq!(
            parse_class_uri("jar:file:///libs/my%20lib.jar!/com/example/Lib.class").ok(),
            Some(ParsedClassUri::JarEntry {
                jar_path: PathBuf::from("/libs/my lib.jar"),
                entry_name: "com/example/Lib.class".to_owned(),
            })
        );
        let jrt = parse_class_uri("jrt:/java.base/java/util/List.class");
        assert_eq!(
            jrt.as_ref().ok().and_then(ParsedClassUri::fqcn).as_deref(),
            Some("java.util.List")
        );
        assert!(
            matches!(jrt, Ok(ParsedClassUri::JrtModule { ref module_name, .. }) if module_name == "java.base")
        );
        assert_eq!(
            parse_class_uri("file:///p/src/A%20B.java").ok(),
            Some(ParsedClassUri::File(PathBuf::from("/p/src/A B.java")))
        );
        assert!(parse_class_uri("jdt://contents/onlyone").is_err());
        assert!(parse_class_uri("jar:file:///x.jar").is_err());
        assert!(parse_class_uri("https://example.com/A.java").is_err());
    }

    #[test]
    fn finds_declarations_for_q2_fallback() {
        let source = "package a;\n\n// class Lib in a comment-ish line\npublic final class Lib {\n  static class Inner {}\n}\n";
        assert_eq!(
            find_declaration(source, "Lib"),
            Some(Utf16Range::new(
                Utf16Point::new(3, 19),
                Utf16Point::new(3, 22)
            ))
        );
        assert_eq!(
            find_declaration(source, "Inner").map(|r| r.start.row),
            Some(4)
        );
        assert_eq!(find_declaration(source, "Li"), None);
        assert_eq!(simple_class_name("a.Outer$Inner"), "Inner");
        assert_eq!(simple_class_name("a.Outer$1"), "Outer");
        assert_eq!(simple_class_name("Top"), "Top");
    }

    #[test]
    fn cache_paths_stay_inside_the_root() {
        let resolver = DefinitionResolver::new("/cache/decompiled", true);
        let path = resolver.contents_cache_file("jdt://x", "com.example.Foo$Bar");
        assert!(path.starts_with("/cache/decompiled/jdtls"));
        assert!(path.ends_with("com/example/Foo.java"));
        let hostile = resolver.contents_cache_file("jdt://y", "...Evil");
        assert!(hostile.starts_with("/cache/decompiled/jdtls"));
        assert!(
            !hostile
                .components()
                .any(|c| c == std::path::Component::ParentDir)
        );
    }
}
