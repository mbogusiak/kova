//! Headless integration tests for parallel `WorkspaceIndexer`, determinism,
//! `.gitignore` filtering, `ProjectSource > JarEntry` precedence, incremental updates,
//! and cancellation.

#![allow(
    clippy::expect_used,
    clippy::cast_possible_truncation,
    reason = "test fixture helpers"
)]

use std::io::Write;
use std::path::Path;

use kova_jvm_index::{
    ClasspathDiscoveryConfig, IndexCancelToken, IndexerConfig, JvmIndexError, JvmSourceOrigin,
    SourceScanOptions, WorkspaceIndexer, discover_classpath,
};
use tempfile::TempDir;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

fn minimal_class_bytes(internal_name: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0xCAFE_BABEu32.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&52u16.to_be_bytes());
    // Constant pool count = 5:
    // 1: Utf8 internal_name
    // 2: Class #1
    // 3: Utf8 "java/lang/Object"
    // 4: Class #3
    out.extend_from_slice(&5u16.to_be_bytes());

    out.push(1);
    out.extend_from_slice(&(internal_name.len() as u16).to_be_bytes());
    out.extend_from_slice(internal_name.as_bytes());

    out.push(7);
    out.extend_from_slice(&1u16.to_be_bytes());

    let obj = b"java/lang/Object";
    out.push(1);
    out.extend_from_slice(&(obj.len() as u16).to_be_bytes());
    out.extend_from_slice(obj);

    out.push(7);
    out.extend_from_slice(&3u16.to_be_bytes());

    // access_flags = ACC_PUBLIC (0x0001)
    out.extend_from_slice(&0x0001u16.to_be_bytes());
    // this_class = #2, super_class = #4
    out.extend_from_slice(&2u16.to_be_bytes());
    out.extend_from_slice(&4u16.to_be_bytes());
    // interfaces, fields, methods, attributes = 0
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out
}

fn write_test_jar(path: &Path, classes: &[&str]) {
    let file = std::fs::File::create(path).expect("create jar");
    let mut zip = ZipWriter::new(file);
    let opts = SimpleFileOptions::default();
    for cls in classes {
        zip.start_file(format!("{cls}.class"), opts)
            .expect("start entry");
        zip.write_all(&minimal_class_bytes(cls))
            .expect("write class");
    }
    zip.finish().expect("finish jar");
}

#[test]
fn parallel_determinism_precedence_incremental_and_cancel() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();

    std::fs::write(root.join(".gitignore"), "ignored/\n").expect("write gitignore");
    std::fs::create_dir_all(root.join("src/com/example")).expect("mkdir src");
    std::fs::create_dir_all(root.join("ignored")).expect("mkdir ignored");
    std::fs::create_dir_all(root.join("libs")).expect("mkdir libs");

    std::fs::write(
        root.join("src/com/example/App.java"),
        "package com.example;\npublic class App { public void run() {} }\n",
    )
    .expect("write App.java");
    std::fs::write(
        root.join("src/com/example/Model.kt"),
        "package com.example\ndata class Model(val id: String)\n",
    )
    .expect("write Model.kt");
    std::fs::write(
        root.join("ignored/Skip.java"),
        "package ignored;\npublic class Skip {}\n",
    )
    .expect("write Skip.java");

    // Jar contains both a library-only class and a shadowed com.example.App class.
    write_test_jar(
        &root.join("libs/deps.jar"),
        &["org/lib/Helper", "com/example/App"],
    );

    let classpath = discover_classpath(
        root,
        &ClasspathDiscoveryConfig {
            scan_project_libs: true,
            ..ClasspathDiscoveryConfig::default()
        },
    )
    .expect("discover classpath");
    assert_eq!(classpath.len(), 1);

    let cancel = IndexCancelToken::new();
    let idx_1 = WorkspaceIndexer::new(IndexerConfig {
        worker_threads: 1,
        ..IndexerConfig::default()
    })
    .expect("indexer 1")
    .index_project(root, &classpath, &cancel)
    .expect("index 1");

    let idx_4 = WorkspaceIndexer::new(IndexerConfig {
        worker_threads: 4,
        ..IndexerConfig::default()
    })
    .expect("indexer 4")
    .index_project(root, &classpath, &cancel)
    .expect("index 4");

    assert_eq!(idx_1.types(), idx_4.types());
    assert!(idx_4.find_by_fqcn("ignored.Skip").is_none());
    assert!(idx_4.find_by_fqcn("org.lib.Helper").is_some());
    assert!(idx_4.find_by_fqcn("com.example.Model").is_some());

    // ProjectSource must shadow JarEntry for com.example.App.
    let app = idx_4.find_by_fqcn("com.example.App").expect("App exists");
    assert!(matches!(
        app.ty.origin,
        JvmSourceOrigin::ProjectSource { .. }
    ));

    // Incremental reindex and remove.
    let mut mutable_idx = idx_4;
    mutable_idx
        .reindex_source_file(
            Path::new("src/com/example/App.java"),
            "package com.example;\npublic class AppRenamed {}\n",
            &SourceScanOptions::default(),
        )
        .expect("reindex");
    assert!(mutable_idx.find_by_fqcn("com.example.AppRenamed").is_some());
    // Shadowed JarEntry should be restored when ProjectSource no longer defines com.example.App.
    let restored_app = mutable_idx
        .find_by_fqcn("com.example.App")
        .expect("JarEntry App restored");
    assert!(matches!(
        restored_app.ty.origin,
        JvmSourceOrigin::JarEntry { .. }
    ));

    mutable_idx.remove_source_file(Path::new("src/com/example/App.java"));
    assert!(mutable_idx.find_by_fqcn("com.example.AppRenamed").is_none());

    // Cancellation.
    let cancelled = IndexCancelToken::new();
    cancelled.cancel();
    let res = WorkspaceIndexer::new(IndexerConfig::default())
        .expect("indexer")
        .index_project(root, &classpath, &cancelled);
    assert!(matches!(res, Err(JvmIndexError::Cancelled)));
}
