//! Headless integration tests for `kova_jvm_index::cache` (KOVA-032).

#![allow(
    clippy::expect_used,
    reason = "integration tests use expect for fixture setup"
)]

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use kova_config::KovaPaths;
use kova_jvm_index::cache::{
    ArtifactFingerprint, CacheLoadOutcome, CacheMissReason, CachedSourceEntry,
    CachedWorkspaceSnapshot, FileFingerprint, HEADER_SIZE, JvmCacheLayout, JvmIndexCacheStore,
    ScanConfigFingerprint, decode_workspace_snapshot, encode_workspace_snapshot,
    load_and_revalidate,
};
use kova_jvm_index::{
    ClasspathEntry, ClasspathOrigin, IndexCancelToken, IndexerConfig, JvmMemberEntry, JvmModifiers,
    JvmSourceOrigin, JvmSymbolKind, JvmTypeEntry, JvmVisibility, ParsedClassUnit, WorkspaceIndexer,
};
use tempfile::TempDir;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

fn test_paths(temp: &TempDir) -> KovaPaths {
    let base = temp.path().join("kova_home");
    KovaPaths::from_roots(base.join("config"), base.join("data"), base.join("cache"))
        .expect("paths")
}

fn write_synthetic_jar(path: &Path, entry_name: &str, class_bytes: &[u8]) {
    let file = File::create(path).expect("create jar");
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default();
    zip.start_file(entry_name, options).expect("start entry");
    zip.write_all(class_bytes).expect("write class bytes");
    zip.finish().expect("finish zip");
}

/// Minimal valid JVM classfile (`public class com.example.LibGreeter` with Java 8).
fn minimal_classfile_bytes(internal_name: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0xCAFE_BABEu32.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes()); // minor
    out.extend_from_slice(&52u16.to_be_bytes()); // major (Java 8)
    // constant_pool_count = 5:
    // #1 = Utf8 internal_name
    // #2 = Class #1
    // #3 = Utf8 "java/lang/Object"
    // #4 = Class #3
    out.extend_from_slice(&5u16.to_be_bytes());
    // #1 Utf8
    out.push(1);
    out.extend_from_slice(
        &u16::try_from(internal_name.len())
            .expect("name len")
            .to_be_bytes(),
    );
    out.extend_from_slice(internal_name.as_bytes());
    // #2 Class -> #1
    out.push(7);
    out.extend_from_slice(&1u16.to_be_bytes());
    // #3 Utf8 "java/lang/Object"
    let obj = b"java/lang/Object";
    out.push(1);
    out.extend_from_slice(&u16::try_from(obj.len()).expect("obj len").to_be_bytes());
    out.extend_from_slice(obj);
    // #4 Class -> #3
    out.push(7);
    out.extend_from_slice(&3u16.to_be_bytes());

    out.extend_from_slice(&0x0021u16.to_be_bytes()); // ACC_PUBLIC | ACC_SUPER
    out.extend_from_slice(&2u16.to_be_bytes()); // this_class = #2
    out.extend_from_slice(&4u16.to_be_bytes()); // super_class = #4
    out.extend_from_slice(&0u16.to_be_bytes()); // interfaces_count
    out.extend_from_slice(&0u16.to_be_bytes()); // fields_count
    out.extend_from_slice(&0u16.to_be_bytes()); // methods_count
    out.extend_from_slice(&0u16.to_be_bytes()); // attributes_count
    out
}

#[test]
fn round_trip_kidx_and_omarchy_isolation() {
    let temp = TempDir::new().expect("tempdir");
    let paths = test_paths(&temp);
    let project_root = temp.path().join("project_a");
    fs::create_dir_all(&project_root).expect("create project");

    let unit = ParsedClassUnit {
        ty: JvmTypeEntry {
            fqcn: String::from("com.example.OrderService"),
            binary_name: String::from("com.example.OrderService"),
            simple_name: String::from("OrderService"),
            package_name: String::from("com.example"),
            kind: JvmSymbolKind::Class,
            modifiers: JvmModifiers {
                visibility: JvmVisibility::Public,
                ..JvmModifiers::default()
            },
            super_fqcn: Some(String::from("java.lang.Object")),
            interfaces: vec![String::from("java.io.Serializable")],
            origin: JvmSourceOrigin::ProjectSource {
                rel_path: PathBuf::from("src/OrderService.java"),
                line: 3,
                column: 1,
            },
        },
        members: vec![JvmMemberEntry {
            owner_fqcn: String::from("com.example.OrderService"),
            name: String::from("placeOrder"),
            kind: JvmSymbolKind::Method,
            modifiers: JvmModifiers {
                visibility: JvmVisibility::Public,
                ..JvmModifiers::default()
            },
            descriptor: String::from("()V"),
            display_signature: String::from("(): void"),
            line: Some(5),
        }],
    };

    let snapshot = CachedWorkspaceSnapshot {
        config: ScanConfigFingerprint::from(&IndexerConfig::default()),
        sources: vec![CachedSourceEntry {
            rel_path: PathBuf::from("src/OrderService.java"),
            fingerprint: FileFingerprint {
                size_bytes: 128,
                modified_unix_ns: 1_700_000_000_000_000_000,
            },
            units: vec![unit],
        }],
        classpath: vec![ArtifactFingerprint {
            path: PathBuf::from("/opt/libs/spring.jar"),
            fingerprint: FileFingerprint {
                size_bytes: 4096,
                modified_unix_ns: 1_700_000_000_000_000_000,
            },
        }],
    };

    let saved_path =
        JvmIndexCacheStore::save_workspace_cache(&paths, &project_root, &snapshot).expect("save");
    assert!(saved_path.exists());

    // Omarchy: zero files in project_root.
    let project_entries: Vec<_> = fs::read_dir(&project_root)
        .expect("read project_root")
        .collect();
    assert!(project_entries.is_empty(), "project_root must stay clean");

    // No orphaned .tmp.* files in cache dir.
    let cache_dir = saved_path.parent().expect("parent");
    for entry in fs::read_dir(cache_dir).expect("read cache_dir") {
        let name = entry.expect("entry").file_name();
        assert!(
            !name.to_string_lossy().contains(".tmp."),
            "unexpected temp file: {name:?}"
        );
    }

    let loaded = JvmIndexCacheStore::load_workspace_cache(&paths, &project_root).expect("load");
    assert_eq!(loaded, CacheLoadOutcome::Hit(snapshot));

    JvmIndexCacheStore::clear_workspace_cache(&paths, &project_root).expect("clear");
    let after_clear =
        JvmIndexCacheStore::load_workspace_cache(&paths, &project_root).expect("load after clear");
    assert_eq!(
        after_clear,
        CacheLoadOutcome::Miss(CacheMissReason::NotFound)
    );
}

#[test]
fn corrupted_truncated_and_version_mismatched_cache_return_miss_without_panic() {
    let snapshot = CachedWorkspaceSnapshot {
        config: ScanConfigFingerprint::from(&IndexerConfig::default()),
        sources: Vec::new(),
        classpath: Vec::new(),
    };
    let valid = encode_workspace_snapshot(&snapshot).expect("encode");

    // Truncated header
    assert_eq!(
        decode_workspace_snapshot(&valid[..10]),
        CacheLoadOutcome::Miss(CacheMissReason::Truncated)
    );

    // Invalid magic
    let mut bad_magic = valid.clone();
    bad_magic[0] = b'X';
    assert_eq!(
        decode_workspace_snapshot(&bad_magic),
        CacheLoadOutcome::Miss(CacheMissReason::InvalidMagic)
    );

    // Unsupported schema version
    let mut bad_ver = valid.clone();
    bad_ver[8..12].copy_from_slice(&99u32.to_le_bytes());
    assert_eq!(
        decode_workspace_snapshot(&bad_ver),
        CacheLoadOutcome::Miss(CacheMissReason::UnsupportedSchemaVersion {
            found: 99,
            expected: 1,
        })
    );

    // 1-byte payload corruption -> ChecksumMismatch
    let mut bad_crc = valid.clone();
    let last = bad_crc.len() - 1;
    bad_crc[last] ^= 0x01;
    assert_eq!(
        decode_workspace_snapshot(&bad_crc),
        CacheLoadOutcome::Miss(CacheMissReason::ChecksumMismatch)
    );

    // Crafted u64::MAX payload_len -> CorruptedPayload without overflow/panic
    let mut huge_len = valid;
    huge_len[12..20].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(matches!(
        decode_workspace_snapshot(&huge_len),
        CacheLoadOutcome::Miss(CacheMissReason::CorruptedPayload(_))
    ));
    const { assert!(HEADER_SIZE == 24) };
}

#[test]
fn incremental_revalidation_shared_jar_shards_and_deletions() {
    let temp = TempDir::new().expect("tempdir");
    let paths = test_paths(&temp);
    let project_a = temp.path().join("project_a");
    let project_b = temp.path().join("project_b");
    fs::create_dir_all(project_a.join("src")).expect("mkdir a");
    fs::create_dir_all(project_b.join("src")).expect("mkdir b");

    let java_a1 = project_a.join("src/Alpha.java");
    let java_a2 = project_a.join("src/Beta.java");
    fs::write(&java_a1, "package com.example;\npublic class Alpha {}\n").expect("write Alpha");
    fs::write(&java_a2, "package com.example;\npublic class Beta {}\n").expect("write Beta");

    let jar_path = temp.path().join("shared-lib.jar");
    write_synthetic_jar(
        &jar_path,
        "com/lib/LibGreeter.class",
        &minimal_classfile_bytes("com/lib/LibGreeter"),
    );
    let jar_meta = fs::metadata(&jar_path).expect("jar meta");
    let modified_ns = u64::try_from(
        jar_meta
            .modified()
            .expect("mtime")
            .duration_since(std::time::UNIX_EPOCH)
            .expect("epoch")
            .as_nanos(),
    )
    .expect("ns");
    let classpath = vec![ClasspathEntry {
        path: jar_path.clone(),
        origin: ClasspathOrigin::ProjectLocalLibs,
        size_bytes: jar_meta.len(),
        modified_unix_ns: modified_ns,
    }];

    let indexer = WorkspaceIndexer::new(IndexerConfig::default()).expect("indexer");
    let cancel = IndexCancelToken::new();

    // 1. Cold run on project_a
    let cold = load_and_revalidate(&paths, &project_a, &indexer, &classpath, &cancel)
        .expect("cold revalidate");
    assert!(!cold.cache_hit);
    assert_eq!(cold.jar_shard_misses, 1);
    assert_eq!(cold.jar_shard_hits, 0);
    assert!(cold.index.find_by_fqcn("com.example.Alpha").is_some());
    assert!(cold.index.find_by_fqcn("com.lib.LibGreeter").is_some());
    assert!(
        JvmCacheLayout::workspace_index_file(&paths, &project_a)
            .expect("path")
            .exists()
    );

    // 2. Hot run on project_a (< 100 ms)
    let t0 = Instant::now();
    let hot = load_and_revalidate(&paths, &project_a, &indexer, &classpath, &cancel)
        .expect("hot revalidate");
    let hot_elapsed = t0.elapsed();
    assert!(hot.cache_hit);
    assert!(hot.delta.is_empty());
    assert_eq!(hot.jar_shard_hits, 1);
    assert_eq!(hot.jar_shard_misses, 0);
    assert!(
        hot_elapsed.as_millis() < 100,
        "hot load took {} ms (budget < 100 ms)",
        hot_elapsed.as_millis()
    );

    // 3. Modify 1 .java file -> only that 1 file is reindexed, jar stays unchanged
    fs::write(
        &java_a1,
        "package com.example;\npublic class AlphaRenamed {}\n",
    )
    .expect("modify Alpha");
    let modified = load_and_revalidate(&paths, &project_a, &indexer, &classpath, &cancel)
        .expect("modified revalidate");
    assert!(modified.cache_hit);
    assert_eq!(modified.delta.added_or_modified_sources.len(), 1);
    assert_eq!(modified.delta.unchanged_sources, 1);
    assert_eq!(modified.delta.unchanged_jars, 1);
    assert!(modified.index.find_by_fqcn("com.example.Alpha").is_none());
    assert!(
        modified
            .index
            .find_by_fqcn("com.example.AlphaRenamed")
            .is_some()
    );

    // 4. Delete Beta.java -> removed from index
    fs::remove_file(&java_a2).expect("remove Beta");
    let deleted = load_and_revalidate(&paths, &project_a, &indexer, &classpath, &cancel)
        .expect("deleted revalidate");
    assert_eq!(
        deleted.delta.removed_sources,
        vec![PathBuf::from("src/Beta.java")]
    );
    assert!(deleted.index.find_by_fqcn("com.example.Beta").is_none());

    // 5. Second project using the same shared .jar -> hits shared .kjar shard!
    fs::write(
        project_b.join("src/App.java"),
        "package com.b;\npublic class App {}\n",
    )
    .expect("write App");
    let proj_b = load_and_revalidate(&paths, &project_b, &indexer, &classpath, &cancel)
        .expect("project b revalidate");
    assert!(!proj_b.cache_hit);
    assert_eq!(proj_b.jar_shard_hits, 1);
    assert_eq!(proj_b.jar_shard_misses, 0);
    assert!(proj_b.index.find_by_fqcn("com.lib.LibGreeter").is_some());
}
