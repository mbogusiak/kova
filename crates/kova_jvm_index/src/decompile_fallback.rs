//! Deterministic pure-Rust `.class` bytecode-to-Java fallback decompiler (`KOVA-061`).
//!
//! Produces syntactically valid, readable Java source (`< 5 ms`, zero JVM subprocesses) when
//! Vineflower or a runtime JDK is unavailable, and caches the output atomically under
//! `<cache_root>/bytecode/<hash>/<package>/<Outer>.java`.

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use cafebabe::attributes::{
    AttributeData, AttributeInfo, InnerClassAccessFlags, InnerClassEntry, MethodParameterEntry,
    RecordComponentEntry,
};
use cafebabe::constant_pool::LiteralConstant;
use cafebabe::{
    ClassAccessFlags, ClassFile, FieldAccessFlags, FieldInfo, MethodAccessFlags, MethodInfo,
    ParseOptions,
};
use sha2::{Digest, Sha256};

use crate::bytecode::DEFAULT_MAX_CLASS_BYTES;
use crate::error::JvmIndexError;
use crate::jdk_lookup::LocatedClassBytes;
use crate::model::{internal_name_to_fqcn, split_package_and_simple};
use crate::signature::{
    TypeDisplayMode, decode_constructor_descriptor, decode_constructor_signature,
    decode_field_descriptor, decode_field_signature, decode_method_descriptor,
    decode_method_signature, parse_class_signature,
};

const HEX: &[u8; 16] = b"0123456789abcdef";
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TypeKind {
    Annotation,
    Interface,
    Enum,
    Record,
    Class,
}

/// Decompiles raw `.class` bytecode into syntactically valid, readable Java source without
/// spawning any external JVM process.
///
/// # Errors
///
/// Returns [`JvmIndexError::InvalidClassFile`] if `bytes` exceeds [`DEFAULT_MAX_CLASS_BYTES`]
/// or cannot be parsed by `cafebabe` (for instance, a classfile version newer than supported).
pub fn decompile_class_bytes_to_java(
    bytes: &[u8],
    fqcn_hint: &str,
    source_label: &str,
) -> Result<String, JvmIndexError> {
    if bytes.len() > DEFAULT_MAX_CLASS_BYTES {
        return Err(JvmIndexError::InvalidClassFile {
            origin: source_label.to_owned(),
            reason: format!(
                "classfile size {} bytes exceeds max_class_bytes ({DEFAULT_MAX_CLASS_BYTES})",
                bytes.len()
            ),
        });
    }

    let mut parse_opts = ParseOptions::default();
    parse_opts.parse_bytecode(false);
    let class_file = cafebabe::parse_class_with_options(bytes, &parse_opts).map_err(|err| {
        JvmIndexError::InvalidClassFile {
            origin: source_label.to_owned(),
            reason: err.to_string(),
        }
    })?;

    Ok(render_class_file(&class_file, fqcn_hint, source_label))
}

/// Computes the deterministic bytecode-fallback cache path and writes `source_text` atomically
/// under `<cache_root>/bytecode/<key>/<package>/<Outer>.java`.
///
/// # Errors
///
/// Returns [`JvmIndexError::Io`] if metadata lookup or atomic file creation fails.
pub fn cache_bytecode_decompilation(
    cache_root: &Path,
    located: &LocatedClassBytes,
    source_text: &str,
) -> Result<PathBuf, JvmIndexError> {
    let key = bytecode_cache_key(&located.archive_path, &located.entry_name)?;
    let (package, simple) = split_package_and_simple(&located.fqcn);
    let outer = simple
        .split('$')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(&simple);

    let mut cache_file = cache_root.join("bytecode").join(key);
    for seg in package.split('.').filter(|s| !s.is_empty()) {
        cache_file.push(seg);
    }
    cache_file.push(format!("{outer}.java"));

    write_atomic(&cache_file, source_text)?;
    Ok(cache_file)
}

fn bytecode_cache_key(archive_path: &Path, entry_name: &str) -> Result<String, JvmIndexError> {
    let canonical = fs::canonicalize(archive_path).map_err(|err| JvmIndexError::Io {
        path: archive_path.display().to_string(),
        message: err.to_string(),
    })?;
    let meta = fs::metadata(&canonical).map_err(|err| JvmIndexError::Io {
        path: archive_path.display().to_string(),
        message: err.to_string(),
    })?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());

    let mut hasher = Sha256::new();
    hasher.update(b"kova-bytecode-fallback-v1\0");
    hasher.update(canonical.to_string_lossy().as_bytes());
    hasher.update([0]);
    hasher.update(meta.len().to_le_bytes());
    hasher.update(mtime.to_le_bytes());
    hasher.update([0]);
    hasher.update(entry_name.as_bytes());

    let digest = hasher.finalize();
    let mut key = String::with_capacity(32);
    for byte in digest.iter().take(16) {
        key.push(char::from(HEX[usize::from(byte >> 4)]));
        key.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok(key)
}

fn write_atomic(path: &Path, contents: &str) -> Result<(), JvmIndexError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|err| JvmIndexError::Io {
        path: parent.display().to_string(),
        message: err.to_string(),
    })?;
    let file_name = path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let seq = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".{file_name}.tmp.{nonce}.{seq}"));

    let write_res = (|| -> std::io::Result<()> {
        let mut file = File::create(&tmp)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();

    write_res.map_err(|err| {
        drop(fs::remove_file(&tmp));
        JvmIndexError::Io {
            path: path.display().to_string(),
            message: err.to_string(),
        }
    })
}

fn render_class_file<'a>(
    class_file: &'a ClassFile<'a>,
    fqcn_hint: &str,
    source_label: &str,
) -> String {
    let fqcn = if class_file.this_class.is_empty() {
        fqcn_hint.to_owned()
    } else {
        internal_name_to_fqcn(&class_file.this_class)
    };
    let (package_name, full_simple) = split_package_and_simple(&fqcn);
    let simple_name = find_self_inner_class(class_file)
        .and_then(|entry| entry.inner_name.as_deref())
        .filter(|s| !s.is_empty())
        .or_else(|| full_simple.rsplit('$').next().filter(|s| !s.is_empty()))
        .unwrap_or(&full_simple)
        .to_owned();

    let imports = collect_imports(class_file, &package_name, &simple_name);
    let kind = classify_type_kind(class_file);

    let mut out = String::with_capacity(2048);
    let _ = writeln!(
        out,
        "// Decompiled by Kova bytecode fallback — install Vineflower for full method bodies"
    );
    if !source_label.is_empty() {
        let _ = writeln!(out, "// Source: {source_label}");
    }
    if !package_name.is_empty() {
        let _ = writeln!(out, "package {package_name};");
        out.push('\n');
    }

    if !imports.is_empty() {
        for imp in &imports {
            let _ = writeln!(out, "import {imp};");
        }
        out.push('\n');
    }

    if has_deprecated(&class_file.attributes) {
        out.push_str("@Deprecated\n");
    }

    render_type_header(class_file, kind, &simple_name, &mut out);
    out.push_str(" {\n");

    let record_components = extract_record_components(&class_file.attributes);
    let record_comp_names: BTreeSet<&str> = record_components
        .iter()
        .map(|comp| comp.name.as_ref())
        .collect();

    render_enum_constants(class_file, kind, &mut out);
    render_fields(class_file, kind, &record_comp_names, &mut out);
    render_constructors_and_methods(class_file, kind, &simple_name, &mut out);

    out.push_str("}\n");
    out
}

fn render_type_header<'a>(
    class_file: &'a ClassFile<'a>,
    kind: TypeKind,
    simple_name: &str,
    out: &mut String,
) {
    let permitted = extract_permitted_subclasses(&class_file.attributes);
    append_type_modifiers(class_file, kind, !permitted.is_empty(), out);
    out.push_str(simple_name);

    let decoded_sig = find_signature(&class_file.attributes)
        .and_then(|raw| parse_class_signature(raw, TypeDisplayMode::Short).ok());

    if let Some(ref sig) = decoded_sig
        && let Some(ref type_params) = sig.type_parameters
    {
        out.push_str(type_params);
    }

    if kind == TypeKind::Record {
        render_record_components(extract_record_components(&class_file.attributes), out);
    }

    append_type_inheritance(class_file, kind, decoded_sig.as_ref(), &permitted, out);
}

fn append_type_modifiers<'a>(
    class_file: &'a ClassFile<'a>,
    kind: TypeKind,
    has_permitted: bool,
    out: &mut String,
) {
    let inner_entry = find_self_inner_class(class_file);
    let is_public = inner_entry.map_or_else(
        || class_file.access_flags.contains(ClassAccessFlags::PUBLIC),
        |e| e.access_flags.contains(InnerClassAccessFlags::PUBLIC),
    );
    let is_protected =
        inner_entry.is_some_and(|e| e.access_flags.contains(InnerClassAccessFlags::PROTECTED));
    let is_private =
        inner_entry.is_some_and(|e| e.access_flags.contains(InnerClassAccessFlags::PRIVATE));
    let is_static =
        inner_entry.is_some_and(|e| e.access_flags.contains(InnerClassAccessFlags::STATIC));
    let is_final = class_file.access_flags.contains(ClassAccessFlags::FINAL)
        || inner_entry.is_some_and(|e| e.access_flags.contains(InnerClassAccessFlags::FINAL));
    let is_abstract = class_file.access_flags.contains(ClassAccessFlags::ABSTRACT)
        || inner_entry.is_some_and(|e| e.access_flags.contains(InnerClassAccessFlags::ABSTRACT));

    if is_public {
        out.push_str("public ");
    } else if is_protected {
        out.push_str("protected ");
    } else if is_private {
        out.push_str("private ");
    }
    if is_static && inner_entry.is_some() {
        out.push_str("static ");
    }
    if has_permitted && !is_final {
        out.push_str("sealed ");
    }
    match kind {
        TypeKind::Class => {
            if is_abstract {
                out.push_str("abstract ");
            }
            if is_final {
                out.push_str("final ");
            }
            out.push_str("class ");
        }
        TypeKind::Interface => out.push_str("interface "),
        TypeKind::Enum => out.push_str("enum "),
        TypeKind::Record => out.push_str("final record "),
        TypeKind::Annotation => out.push_str("@interface "),
    }
}

fn render_record_components(components: &[RecordComponentEntry<'_>], out: &mut String) {
    out.push('(');
    for (idx, comp) in components.iter().enumerate() {
        if idx > 0 {
            out.push_str(", ");
        }
        let comp_ty = find_signature(&comp.attributes)
            .and_then(|s| decode_field_signature(s).ok())
            .unwrap_or_else(|| {
                decode_field_descriptor(&comp.descriptor.to_string())
                    .unwrap_or_else(|_| "Object".to_owned())
            });
        let _ = write!(out, "{comp_ty} {}", comp.name);
    }
    out.push(')');
}

fn append_type_inheritance(
    class_file: &ClassFile<'_>,
    kind: TypeKind,
    decoded_sig: Option<&crate::signature::DecodedClassSignature>,
    permitted: &[String],
    out: &mut String,
) {
    if kind == TypeKind::Class {
        if let Some(sig) = decoded_sig {
            if sig.superclass != "Object" && sig.superclass != "java.lang.Object" {
                let _ = write!(out, " extends {}", sig.superclass);
            }
        } else if let Some(sup) = class_file.super_class.as_deref()
            && sup != "java/lang/Object"
        {
            let _ = write!(out, " extends {}", format_internal_short(sup));
        }
    }

    let ifaces: Vec<String> = if let Some(sig) = decoded_sig {
        sig.interfaces
            .iter()
            .filter(|i| !is_ignored_superinterface(kind, i))
            .cloned()
            .collect()
    } else {
        class_file
            .interfaces
            .iter()
            .map(|i| format_internal_short(i))
            .filter(|i| !is_ignored_superinterface(kind, i))
            .collect()
    };

    if !ifaces.is_empty() {
        let kw = if kind == TypeKind::Interface {
            "extends"
        } else {
            "implements"
        };
        let _ = write!(out, " {kw} {}", ifaces.join(", "));
    }

    if !permitted.is_empty() {
        let _ = write!(out, " permits {}", permitted.join(", "));
    }
}

fn is_ignored_superinterface(kind: TypeKind, iface: &str) -> bool {
    kind == TypeKind::Annotation && iface == "Annotation"
}

fn render_enum_constants(class_file: &ClassFile<'_>, kind: TypeKind, out: &mut String) {
    if kind != TypeKind::Enum {
        return;
    }
    let constants: Vec<&str> = class_file
        .fields
        .iter()
        .filter(|f| f.access_flags.contains(FieldAccessFlags::ENUM))
        .map(|f| f.name.as_ref())
        .collect();
    if constants.is_empty() {
        return;
    }
    let _ = writeln!(out, "    {};\n", constants.join(", "));
}

fn render_fields(
    class_file: &ClassFile<'_>,
    kind: TypeKind,
    record_comp_names: &BTreeSet<&str>,
    out: &mut String,
) {
    let mut emitted_any = false;
    for field in &class_file.fields {
        if field.access_flags.contains(FieldAccessFlags::SYNTHETIC)
            || field.access_flags.contains(FieldAccessFlags::ENUM)
            || has_synthetic(&field.attributes)
            || field.name == "$VALUES"
        {
            continue;
        }
        if kind == TypeKind::Record
            && !field.access_flags.contains(FieldAccessFlags::STATIC)
            && record_comp_names.contains(field.name.as_ref())
        {
            continue;
        }

        if has_deprecated(&field.attributes) {
            out.push_str("    @Deprecated\n");
        }
        out.push_str("    ");
        append_field_modifiers(field, kind, out);

        let desc = field.descriptor.to_string();
        let ty = find_signature(&field.attributes)
            .and_then(|s| decode_field_signature(s).ok())
            .unwrap_or_else(|| decode_field_descriptor(&desc).unwrap_or_else(|_| desc.clone()));
        let _ = write!(out, "{ty} {}", field.name);

        if let Some(const_val) = extract_constant_value(&field.attributes) {
            let _ = write!(out, " = {}", format_literal_constant(const_val, &desc));
        }
        out.push_str(";\n");
        emitted_any = true;
    }
    if emitted_any {
        out.push('\n');
    }
}

fn append_field_modifiers(field: &FieldInfo<'_>, kind: TypeKind, out: &mut String) {
    let flags = field.access_flags;
    if kind != TypeKind::Interface && kind != TypeKind::Annotation {
        if flags.contains(FieldAccessFlags::PUBLIC) {
            out.push_str("public ");
        } else if flags.contains(FieldAccessFlags::PROTECTED) {
            out.push_str("protected ");
        } else if flags.contains(FieldAccessFlags::PRIVATE) {
            out.push_str("private ");
        } else {
            out.push_str("/* package-private */ ");
        }
        if flags.contains(FieldAccessFlags::STATIC) {
            out.push_str("static ");
        }
        if flags.contains(FieldAccessFlags::FINAL) {
            out.push_str("final ");
        }
    } else if flags.contains(FieldAccessFlags::PUBLIC) {
        out.push_str("public ");
    }
    if flags.contains(FieldAccessFlags::TRANSIENT) {
        out.push_str("transient ");
    }
    if flags.contains(FieldAccessFlags::VOLATILE) {
        out.push_str("volatile ");
    }
}

fn render_constructors_and_methods(
    class_file: &ClassFile<'_>,
    kind: TypeKind,
    simple_name: &str,
    out: &mut String,
) {
    let method_count = class_file.methods.len();
    for (idx, method) in class_file.methods.iter().enumerate() {
        if method.name == "<clinit>"
            || method.access_flags.contains(MethodAccessFlags::SYNTHETIC)
            || method.access_flags.contains(MethodAccessFlags::BRIDGE)
            || has_synthetic(&method.attributes)
        {
            continue;
        }
        // Enum classes always synthesize `values()` and `valueOf(String)`; keeping them or not is fine,
        // and keeping public ones makes Cmd+Click on `MyEnum.values()` work too.

        if has_deprecated(&method.attributes) {
            out.push_str("    @Deprecated\n");
        }
        out.push_str("    ");
        append_method_modifiers(method, kind, out);

        let desc = method.descriptor.to_string();
        let param_names = extract_method_param_names(&method.attributes);
        let is_varargs = method.access_flags.contains(MethodAccessFlags::VARARGS);
        let is_ctor = method.name == "<init>";

        if is_ctor {
            let decoded = find_signature(&method.attributes)
                .and_then(|s| decode_constructor_signature(s).ok())
                .unwrap_or_else(|| {
                    decode_constructor_descriptor(&desc).unwrap_or_else(|_| "()".to_owned())
                });
            let parsed = split_decoded_signature(&decoded);
            if let Some(tp) = parsed.type_params {
                let _ = write!(out, "{tp} ");
            }
            out.push_str(simple_name);
            write_params_with_names(&parsed.params, &param_names, is_varargs, out);
            append_throws(&method.attributes, &parsed.throws, out);
        } else {
            let decoded = find_signature(&method.attributes)
                .and_then(|s| decode_method_signature(s).ok())
                .unwrap_or_else(|| {
                    decode_method_descriptor(&desc).unwrap_or_else(|_| "(): void".to_owned())
                });
            let parsed = split_decoded_signature(&decoded);
            if let Some(tp) = parsed.type_params {
                let _ = write!(out, "{tp} ");
            }
            let ret = parsed.return_type.as_deref().unwrap_or("void");
            let _ = write!(out, "{ret} {}", method.name);
            write_params_with_names(&parsed.params, &param_names, is_varargs, out);
            append_throws(&method.attributes, &parsed.throws, out);
        }

        let has_no_body = method.access_flags.contains(MethodAccessFlags::ABSTRACT)
            || method.access_flags.contains(MethodAccessFlags::NATIVE);
        if has_no_body {
            out.push_str(";\n");
        } else {
            out.push_str(" {\n        /* compiled code */\n    }\n");
        }
        if idx + 1 < method_count {
            out.push('\n');
        }
    }
}

fn append_method_modifiers(method: &MethodInfo<'_>, kind: TypeKind, out: &mut String) {
    let flags = method.access_flags;
    if flags.contains(MethodAccessFlags::PUBLIC) {
        out.push_str("public ");
    } else if flags.contains(MethodAccessFlags::PROTECTED) {
        out.push_str("protected ");
    } else if flags.contains(MethodAccessFlags::PRIVATE) {
        out.push_str("private ");
    } else if kind != TypeKind::Interface && kind != TypeKind::Annotation {
        out.push_str("/* package-private */ ");
    }

    if flags.contains(MethodAccessFlags::STATIC) {
        out.push_str("static ");
    } else if matches!(kind, TypeKind::Interface)
        && !flags.contains(MethodAccessFlags::ABSTRACT)
        && !flags.contains(MethodAccessFlags::PRIVATE)
    {
        out.push_str("default ");
    }

    if flags.contains(MethodAccessFlags::FINAL) {
        out.push_str("final ");
    }
    if flags.contains(MethodAccessFlags::ABSTRACT)
        && !matches!(kind, TypeKind::Interface | TypeKind::Annotation)
    {
        out.push_str("abstract ");
    }
    if flags.contains(MethodAccessFlags::SYNCHRONIZED) {
        out.push_str("synchronized ");
    }
    if flags.contains(MethodAccessFlags::NATIVE) {
        out.push_str("native ");
    }
}

struct ParsedDecodedSignature {
    type_params: Option<String>,
    params: Vec<String>,
    return_type: Option<String>,
    throws: Vec<String>,
}

fn split_decoded_signature(decoded: &str) -> ParsedDecodedSignature {
    let bytes = decoded.as_bytes();
    let mut angle = 0usize;
    let mut open_paren = None;
    let mut close_paren = None;
    let mut paren_depth = 0usize;

    for (idx, &b) in bytes.iter().enumerate() {
        match b {
            b'<' => angle += 1,
            b'>' => angle = angle.saturating_sub(1),
            b'(' if angle == 0 => {
                if paren_depth == 0 && open_paren.is_none() {
                    open_paren = Some(idx);
                }
                paren_depth += 1;
            }
            b')' if angle == 0 && paren_depth > 0 => {
                paren_depth -= 1;
                if paren_depth == 0 {
                    close_paren = Some(idx);
                    break;
                }
            }
            _ => {}
        }
    }

    let (Some(open), Some(close)) = (open_paren, close_paren) else {
        return ParsedDecodedSignature {
            type_params: None,
            params: Vec::new(),
            return_type: None,
            throws: Vec::new(),
        };
    };

    let tp_raw = decoded[..open].trim();
    let type_params = (!tp_raw.is_empty()).then(|| tp_raw.to_owned());
    let params_raw = decoded[open + 1..close].trim();
    let params = if params_raw.is_empty() {
        Vec::new()
    } else {
        split_top_level_commas(params_raw)
    };

    let tail = decoded[close + 1..].trim();
    let mut return_type = None;
    let mut throws = Vec::new();
    if let Some(after_colon) = tail.strip_prefix(':') {
        let after_colon = after_colon.trim();
        if let Some((ret_part, throws_part)) = after_colon.split_once(" throws ") {
            return_type = Some(ret_part.trim().to_owned());
            throws = split_top_level_commas(throws_part);
        } else if !after_colon.is_empty() {
            return_type = Some(after_colon.to_owned());
        }
    }

    ParsedDecodedSignature {
        type_params,
        params,
        return_type,
        throws,
    }
}

fn split_top_level_commas(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut angle = 0usize;
    let mut start = 0usize;
    for (idx, b) in input.bytes().enumerate() {
        match b {
            b'<' => angle += 1,
            b'>' => angle = angle.saturating_sub(1),
            b',' if angle == 0 => {
                let part = input[start..idx].trim();
                if !part.is_empty() {
                    out.push(part.to_owned());
                }
                start = idx + 1;
            }
            _ => {}
        }
    }
    let last = input[start..].trim();
    if !last.is_empty() {
        out.push(last.to_owned());
    }
    out
}

fn write_params_with_names(
    params: &[String],
    param_names: &[Option<String>],
    is_varargs: bool,
    out: &mut String,
) {
    out.push('(');
    for (idx, param_ty) in params.iter().enumerate() {
        if idx > 0 {
            out.push_str(", ");
        }
        let rendered_ty = if is_varargs && idx + 1 == params.len() {
            if let Some(stripped) = param_ty.strip_suffix("[]") {
                format!("{stripped}...")
            } else {
                param_ty.clone()
            }
        } else {
            param_ty.clone()
        };
        let fallback_name = format!("arg{idx}");
        let name = param_names
            .get(idx)
            .and_then(|opt| opt.as_deref())
            .filter(|s| !s.is_empty())
            .unwrap_or(&fallback_name);
        let _ = write!(out, "{rendered_ty} {name}");
    }
    out.push(')');
}

fn append_throws(attributes: &[AttributeInfo<'_>], sig_throws: &[String], out: &mut String) {
    if !sig_throws.is_empty() {
        let _ = write!(out, " throws {}", sig_throws.join(", "));
        return;
    }
    for attr in attributes {
        if let AttributeData::Exceptions(excs) = &attr.data
            && !excs.is_empty()
        {
            let names: Vec<String> = excs.iter().map(|e| format_internal_short(e)).collect();
            let _ = write!(out, " throws {}", names.join(", "));
            return;
        }
    }
}

fn extract_method_param_names(attributes: &[AttributeInfo<'_>]) -> Vec<Option<String>> {
    for attr in attributes {
        if let AttributeData::MethodParameters(entries) = &attr.data {
            return entries
                .iter()
                .map(|MethodParameterEntry { name, .. }| name.as_deref().map(str::to_owned))
                .collect();
        }
    }
    Vec::new()
}

fn extract_record_components<'a>(
    attributes: &'a [AttributeInfo<'a>],
) -> &'a [RecordComponentEntry<'a>] {
    for attr in attributes {
        if let AttributeData::Record(entries) = &attr.data {
            return entries;
        }
    }
    &[]
}

fn extract_permitted_subclasses(attributes: &[AttributeInfo<'_>]) -> Vec<String> {
    for attr in attributes {
        if let AttributeData::PermittedSubclasses(subs) = &attr.data {
            return subs.iter().map(|s| format_internal_short(s)).collect();
        }
    }
    Vec::new()
}

fn extract_constant_value<'a>(
    attributes: &'a [AttributeInfo<'a>],
) -> Option<&'a LiteralConstant<'a>> {
    attributes.iter().find_map(|attr| {
        if let AttributeData::ConstantValue(val) = &attr.data {
            Some(val)
        } else {
            None
        }
    })
}

fn format_literal_constant(val: &LiteralConstant<'_>, descriptor: &str) -> String {
    match val {
        LiteralConstant::Integer(v) => match descriptor {
            "Z" => if *v != 0 { "true" } else { "false" }.to_owned(),
            "C" => {
                if let Ok(code) = u32::try_from(*v)
                    && let Some(ch) = char::from_u32(code)
                {
                    format!("'{}'", escape_java_char(ch))
                } else {
                    format!("(char) {v}")
                }
            }
            _ => v.to_string(),
        },
        LiteralConstant::Long(v) => format!("{v}L"),
        LiteralConstant::Float(v) => {
            if v.is_nan() {
                "Float.NaN".to_owned()
            } else if v.is_infinite() {
                if v.is_sign_negative() {
                    "Float.NEGATIVE_INFINITY".to_owned()
                } else {
                    "Float.POSITIVE_INFINITY".to_owned()
                }
            } else {
                format!("{v}f")
            }
        }
        LiteralConstant::Double(v) => {
            if v.is_nan() {
                "Double.NaN".to_owned()
            } else if v.is_infinite() {
                if v.is_sign_negative() {
                    "Double.NEGATIVE_INFINITY".to_owned()
                } else {
                    "Double.POSITIVE_INFINITY".to_owned()
                }
            } else {
                let s = v.to_string();
                if s.contains('.') || s.contains('e') || s.contains('E') {
                    s
                } else {
                    format!("{s}.0")
                }
            }
        }
        LiteralConstant::String(s) => format!("\"{}\"", escape_java_string(s)),
        LiteralConstant::StringBytes(bytes) => {
            let lossy = String::from_utf8_lossy(bytes);
            format!("\"{}\"", escape_java_string(&lossy))
        }
    }
}

fn escape_java_char(ch: char) -> String {
    match ch {
        '\'' => "\\'".to_owned(),
        '\\' => "\\\\".to_owned(),
        '\n' => "\\n".to_owned(),
        '\r' => "\\r".to_owned(),
        '\t' => "\\t".to_owned(),
        c if c.is_control() => format!("\\u{:04x}", u32::from(c)),
        c => c.to_string(),
    }
}

fn escape_java_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out
}

fn classify_type_kind(class_file: &ClassFile<'_>) -> TypeKind {
    if class_file
        .access_flags
        .contains(ClassAccessFlags::ANNOTATION)
    {
        TypeKind::Annotation
    } else if class_file
        .access_flags
        .contains(ClassAccessFlags::INTERFACE)
    {
        TypeKind::Interface
    } else if class_file.access_flags.contains(ClassAccessFlags::ENUM) {
        TypeKind::Enum
    } else if class_file.super_class.as_deref() == Some("java/lang/Record")
        || class_file
            .attributes
            .iter()
            .any(|attr| matches!(attr.data, AttributeData::Record(_)))
    {
        TypeKind::Record
    } else {
        TypeKind::Class
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

fn find_signature<'a>(attributes: &'a [AttributeInfo<'a>]) -> Option<&'a str> {
    attributes.iter().find_map(|attr| {
        if let AttributeData::Signature(sig) = &attr.data {
            Some(sig.as_ref())
        } else {
            None
        }
    })
}

fn has_synthetic(attributes: &[AttributeInfo<'_>]) -> bool {
    attributes
        .iter()
        .any(|attr| matches!(attr.data, AttributeData::Synthetic))
}

fn has_deprecated(attributes: &[AttributeInfo<'_>]) -> bool {
    attributes
        .iter()
        .any(|attr| matches!(attr.data, AttributeData::Deprecated))
}

fn format_internal_short(internal: &str) -> String {
    let last = internal.rsplit('/').next().unwrap_or(internal);
    last.replace('$', ".")
}

fn collect_imports(
    class_file: &ClassFile<'_>,
    current_package: &str,
    self_simple_name: &str,
) -> Vec<String> {
    let mut raw_internals = BTreeSet::new();

    if let Some(sup) = class_file.super_class.as_deref() {
        raw_internals.insert(sup.to_owned());
    }
    for iface in &class_file.interfaces {
        raw_internals.insert(iface.to_string());
    }
    for attr in &class_file.attributes {
        match &attr.data {
            AttributeData::Signature(sig) => {
                collect_internal_names_from_text(sig, &mut raw_internals);
            }
            AttributeData::PermittedSubclasses(subs) => {
                for sub in subs {
                    raw_internals.insert(sub.to_string());
                }
            }
            AttributeData::Record(comps) => {
                for comp in comps {
                    collect_internal_names_from_text(
                        &comp.descriptor.to_string(),
                        &mut raw_internals,
                    );
                    if let Some(sig) = find_signature(&comp.attributes) {
                        collect_internal_names_from_text(sig, &mut raw_internals);
                    }
                }
            }
            _ => {}
        }
    }
    for field in &class_file.fields {
        if field.access_flags.contains(FieldAccessFlags::SYNTHETIC)
            || has_synthetic(&field.attributes)
        {
            continue;
        }
        collect_internal_names_from_text(&field.descriptor.to_string(), &mut raw_internals);
        if let Some(sig) = find_signature(&field.attributes) {
            collect_internal_names_from_text(sig, &mut raw_internals);
        }
    }
    for method in &class_file.methods {
        if method.name == "<clinit>"
            || method.access_flags.contains(MethodAccessFlags::SYNTHETIC)
            || method.access_flags.contains(MethodAccessFlags::BRIDGE)
            || has_synthetic(&method.attributes)
        {
            continue;
        }
        collect_internal_names_from_text(&method.descriptor.to_string(), &mut raw_internals);
        if let Some(sig) = find_signature(&method.attributes) {
            collect_internal_names_from_text(sig, &mut raw_internals);
        }
        for attr in &method.attributes {
            if let AttributeData::Exceptions(excs) = &attr.data {
                for exc in excs {
                    raw_internals.insert(exc.to_string());
                }
            }
        }
    }

    let mut by_simple: HashMap<String, String> = HashMap::new();
    for internal in raw_internals {
        let top_internal = internal.split('$').next().unwrap_or(&internal);
        if !top_internal.contains('/') {
            continue;
        }
        let fqcn = top_internal.replace('/', ".");
        let (pkg, simple) = split_package_and_simple(&fqcn);
        if pkg.is_empty()
            || pkg == "java.lang"
            || pkg == current_package
            || simple == self_simple_name
        {
            continue;
        }
        by_simple.entry(simple).or_insert(fqcn);
    }

    let mut imports: Vec<String> = by_simple.into_values().collect();
    imports.sort();
    imports
}

fn collect_internal_names_from_text(sig: &str, out: &mut BTreeSet<String>) {
    let bytes = sig.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'L' {
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() && !matches!(bytes[j], b';' | b'<') {
                j += 1;
            }
            if j > start && j < bytes.len() {
                let candidate = &sig[start..j];
                if candidate.contains('/') {
                    out.insert(candidate.to_owned());
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use kova_syntax::{LanguageRegistry, detect_language};
    use tempfile::TempDir;

    use super::*;

    fn cp_utf8(cp: &mut Vec<Vec<u8>>, s: &str) -> u16 {
        let mut e = vec![1];
        e.extend_from_slice(&u16::try_from(s.len()).unwrap().to_be_bytes());
        e.extend_from_slice(s.as_bytes());
        cp.push(e);
        u16::try_from(cp.len()).unwrap()
    }

    fn cp_class(cp: &mut Vec<Vec<u8>>, utf8_idx: u16) -> u16 {
        let mut e = vec![7];
        e.extend_from_slice(&utf8_idx.to_be_bytes());
        cp.push(e);
        u16::try_from(cp.len()).unwrap()
    }

    fn cp_string(cp: &mut Vec<Vec<u8>>, utf8_idx: u16) -> u16 {
        let mut e = vec![8];
        e.extend_from_slice(&utf8_idx.to_be_bytes());
        cp.push(e);
        u16::try_from(cp.len()).unwrap()
    }

    fn cp_int(cp: &mut Vec<Vec<u8>>, val: i32) -> u16 {
        let mut e = vec![3];
        e.extend_from_slice(&val.to_be_bytes());
        cp.push(e);
        u16::try_from(cp.len()).unwrap()
    }

    fn build_rich_classfile_bytes() -> Vec<u8> {
        let mut cp: Vec<Vec<u8>> = Vec::new();
        let this_utf8 = cp_utf8(&mut cp, "com/example/SampleBox");
        let this_cls = cp_class(&mut cp, this_utf8);
        let obj_utf8 = cp_utf8(&mut cp, "java/lang/Object");
        let obj_cls = cp_class(&mut cp, obj_utf8);
        let ser_utf8 = cp_utf8(&mut cp, "java/io/Serializable");
        let ser_cls = cp_class(&mut cp, ser_utf8);
        let io_exc_utf8 = cp_utf8(&mut cp, "java/io/IOException");
        let io_exc_cls = cp_class(&mut cp, io_exc_utf8);

        let attr_const = cp_utf8(&mut cp, "ConstantValue");
        let attr_sig = cp_utf8(&mut cp, "Signature");
        let attr_exc = cp_utf8(&mut cp, "Exceptions");
        let attr_params = cp_utf8(&mut cp, "MethodParameters");
        let attr_depr = cp_utf8(&mut cp, "Deprecated");

        let f_ver_name = cp_utf8(&mut cp, "VERSION");
        let f_ver_desc = cp_utf8(&mut cp, "Ljava/lang/String;");
        let f_ver_lit_u = cp_utf8(&mut cp, "1.0");
        let f_ver_lit = cp_string(&mut cp, f_ver_lit_u);

        let f_max_name = cp_utf8(&mut cp, "MAX_ITEMS");
        let f_max_desc = cp_utf8(&mut cp, "I");
        let f_max_lit = cp_int(&mut cp, 42);

        let ctor_name = cp_utf8(&mut cp, "<init>");
        let ctor_desc = cp_utf8(&mut cp, "(Ljava/util/List;)V");
        let ctor_sig = cp_utf8(&mut cp, "(Ljava/util/List<Ljava/lang/String;>;)V");
        let p_items = cp_utf8(&mut cp, "items");

        let m_name = cp_utf8(&mut cp, "firstOrDefault");
        let m_desc = cp_utf8(
            &mut cp,
            "(Ljava/util/List;Ljava/lang/Object;)Ljava/lang/Object;",
        );
        let m_sig = cp_utf8(
            &mut cp,
            "<T::Ljava/lang/Comparable<TT;>;>(Ljava/util/List<TT;>;TT;)TT;",
        );
        let p_list = cp_utf8(&mut cp, "list");
        let p_fallback = cp_utf8(&mut cp, "fallback");

        let mut out = Vec::new();
        out.extend_from_slice(&0xCAFE_BABEu32.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&61u16.to_be_bytes());
        out.extend_from_slice(&u16::try_from(cp.len() + 1).unwrap().to_be_bytes());
        for entry in cp {
            out.extend_from_slice(&entry);
        }
        out.extend_from_slice(&0x0031u16.to_be_bytes());
        out.extend_from_slice(&this_cls.to_be_bytes());
        out.extend_from_slice(&obj_cls.to_be_bytes());
        out.extend_from_slice(&1u16.to_be_bytes());
        out.extend_from_slice(&ser_cls.to_be_bytes());

        // Fields: VERSION, MAX_ITEMS
        out.extend_from_slice(&2u16.to_be_bytes());
        for (name, desc, attr_idx, val_idx, depr) in [
            (f_ver_name, f_ver_desc, attr_const, f_ver_lit, false),
            (f_max_name, f_max_desc, attr_const, f_max_lit, true),
        ] {
            out.extend_from_slice(&0x0019u16.to_be_bytes());
            out.extend_from_slice(&name.to_be_bytes());
            out.extend_from_slice(&desc.to_be_bytes());
            out.extend_from_slice(&(if depr { 2u16 } else { 1u16 }).to_be_bytes());
            out.extend_from_slice(&attr_idx.to_be_bytes());
            out.extend_from_slice(&2u32.to_be_bytes());
            out.extend_from_slice(&val_idx.to_be_bytes());
            if depr {
                out.extend_from_slice(&attr_depr.to_be_bytes());
                out.extend_from_slice(&0u32.to_be_bytes());
            }
        }

        // Methods: <init>, firstOrDefault
        out.extend_from_slice(&2u16.to_be_bytes());
        out.extend_from_slice(&0x0001u16.to_be_bytes());
        out.extend_from_slice(&ctor_name.to_be_bytes());
        out.extend_from_slice(&ctor_desc.to_be_bytes());
        out.extend_from_slice(&3u16.to_be_bytes());
        out.extend_from_slice(&attr_sig.to_be_bytes());
        out.extend_from_slice(&2u32.to_be_bytes());
        out.extend_from_slice(&ctor_sig.to_be_bytes());
        out.extend_from_slice(&attr_exc.to_be_bytes());
        out.extend_from_slice(&4u32.to_be_bytes());
        out.extend_from_slice(&1u16.to_be_bytes());
        out.extend_from_slice(&io_exc_cls.to_be_bytes());
        out.extend_from_slice(&attr_params.to_be_bytes());
        out.extend_from_slice(&5u32.to_be_bytes());
        out.push(1);
        out.extend_from_slice(&p_items.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());

        out.extend_from_slice(&0x0009u16.to_be_bytes());
        out.extend_from_slice(&m_name.to_be_bytes());
        out.extend_from_slice(&m_desc.to_be_bytes());
        out.extend_from_slice(&2u16.to_be_bytes());
        out.extend_from_slice(&attr_sig.to_be_bytes());
        out.extend_from_slice(&2u32.to_be_bytes());
        out.extend_from_slice(&m_sig.to_be_bytes());
        out.extend_from_slice(&attr_params.to_be_bytes());
        out.extend_from_slice(&9u32.to_be_bytes());
        out.push(2);
        out.extend_from_slice(&p_list.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&p_fallback.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());

        out.extend_from_slice(&0u16.to_be_bytes());
        out
    }

    #[test]
    fn bytecode_fallback_produces_valid_java_and_parses_cleanly_with_treesitter() {
        let bytes = build_rich_classfile_bytes();
        let java = decompile_class_bytes_to_java(
            &bytes,
            "com.example.SampleBox",
            "sample.jar!/com/example/SampleBox.class",
        )
        .expect("decompile");

        assert!(java.contains("package com.example;"), "{java}");
        assert!(java.contains("import java.io.IOException;"), "{java}");
        assert!(java.contains("import java.io.Serializable;"), "{java}");
        assert!(java.contains("import java.util.List;"), "{java}");
        assert!(
            java.contains("public final class SampleBox implements Serializable"),
            "{java}"
        );
        assert!(
            java.contains("public static final String VERSION = \"1.0\";"),
            "{java}"
        );
        assert!(
            java.contains("@Deprecated\n    public static final int MAX_ITEMS = 42;"),
            "{java}"
        );
        assert!(
            java.contains("public SampleBox(List<String> items) throws IOException"),
            "{java}"
        );
        assert!(
            java.contains(
                "public static <T extends Comparable<T>> T firstOrDefault(List<T> list, T fallback)"
            ),
            "{java}"
        );

        // Verify with Kova's Tree-sitter Java grammar that there are zero syntax errors!
        let reg = LanguageRegistry::builtin().expect("registry");
        let lang = detect_language(Path::new("SampleBox.java")).expect("java lang");
        let ts_lang = reg
            .get(lang)
            .and_then(|entry| entry.ts_language())
            .expect("ts java");
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(ts_lang).expect("set language");
        let tree = parser.parse(&java, None).expect("parse java");
        assert!(
            !tree.root_node().has_error(),
            "expected 0 syntax errors in:\n{java}\nsexp: {}",
            tree.root_node().to_sexp()
        );
    }

    #[test]
    fn bytecode_fallback_caches_atomically_under_bytecode_subdir() {
        let dir = TempDir::new().expect("tempdir");
        let jar_path = dir.path().join("sample.jar");
        fs::write(&jar_path, b"PK\x03\x04").expect("write dummy jar");

        let located = LocatedClassBytes {
            fqcn: "com.example.SampleBox".to_owned(),
            archive_path: jar_path,
            entry_name: "com/example/SampleBox.class".to_owned(),
            outer_entry_name: "com/example/SampleBox.class".to_owned(),
            bytes: build_rich_classfile_bytes(),
        };
        let cache_root = dir.path().join("decompiled");
        let java = decompile_class_bytes_to_java(&located.bytes, &located.fqcn, "sample.jar")
            .expect("decompile");
        let cached_path =
            cache_bytecode_decompilation(&cache_root, &located, &java).expect("cache write");

        assert!(cached_path.starts_with(cache_root.join("bytecode")));
        assert!(cached_path.ends_with("com/example/SampleBox.java"));
        assert_eq!(fs::read_to_string(&cached_path).unwrap(), java);
    }
}
