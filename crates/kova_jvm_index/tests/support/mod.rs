//! Shared fixtures for KOVA-033 integration tests (synthetic jars and projects).

#![allow(
    dead_code,
    reason = "each integration test binary uses a different subset of the helpers"
)]
#![allow(
    clippy::expect_used,
    reason = "integration test fixtures fail fast on setup errors"
)]

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use kova_config::KovaPaths;
use tempfile::TempDir;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

/// Isolated `KovaPaths` rooted inside `temp`.
pub(crate) fn test_paths(temp: &TempDir) -> KovaPaths {
    let base = temp.path().join("kova_home");
    KovaPaths::from_roots(base.join("config"), base.join("data"), base.join("cache"))
        .expect("paths")
}

/// Writes a `.jar` holding one minimal public class per internal name.
pub(crate) fn write_jar(path: &Path, internal_names: &[String]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("jar dir");
    }
    let file = File::create(path).expect("create jar");
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for internal in internal_names {
        zip.start_file(format!("{internal}.class"), options)
            .expect("start entry");
        zip.write_all(&minimal_classfile_bytes(internal))
            .expect("write class bytes");
    }
    zip.finish().expect("finish zip");
}

/// Writes `content` to `root/rel`, creating parent directories.
pub(crate) fn write_source(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("source dir");
    }
    fs::write(path, content).expect("write source");
}

/// Minimal valid Java 8 classfile: `public class <internal_name> extends java.lang.Object`.
pub(crate) fn minimal_classfile_bytes(internal_name: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + internal_name.len());
    out.extend_from_slice(&0xCAFE_BABEu32.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes()); // minor
    out.extend_from_slice(&52u16.to_be_bytes()); // major (Java 8)
    out.extend_from_slice(&5u16.to_be_bytes()); // constant_pool_count
    out.push(1); // #1 Utf8 internal_name
    out.extend_from_slice(
        &u16::try_from(internal_name.len())
            .expect("name len")
            .to_be_bytes(),
    );
    out.extend_from_slice(internal_name.as_bytes());
    out.push(7); // #2 Class -> #1
    out.extend_from_slice(&1u16.to_be_bytes());
    let obj = b"java/lang/Object";
    out.push(1); // #3 Utf8 java/lang/Object
    out.extend_from_slice(&u16::try_from(obj.len()).expect("obj len").to_be_bytes());
    out.extend_from_slice(obj);
    out.push(7); // #4 Class -> #3
    out.extend_from_slice(&3u16.to_be_bytes());
    out.extend_from_slice(&0x0021u16.to_be_bytes()); // ACC_PUBLIC | ACC_SUPER
    out.extend_from_slice(&2u16.to_be_bytes()); // this_class
    out.extend_from_slice(&4u16.to_be_bytes()); // super_class
    out.extend_from_slice(&0u16.to_be_bytes()); // interfaces_count
    out.extend_from_slice(&0u16.to_be_bytes()); // fields_count
    out.extend_from_slice(&0u16.to_be_bytes()); // methods_count
    out.extend_from_slice(&0u16.to_be_bytes()); // attributes_count
    out
}
