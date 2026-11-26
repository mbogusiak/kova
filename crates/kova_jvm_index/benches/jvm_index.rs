//! Criterion benchmarks for `kova_jvm_index` (KOVA-033): cold indexing of 20,000
//! `.jar` classes, warm cache load, and `nucleo` queries on 50,000 classes.
//!
//! Benches are not covered by `allow-unwrap-in-tests`, so fixture errors are
//! reported on stderr and the affected benchmark is skipped instead of panicking.
#![allow(
    missing_docs,
    reason = "criterion_group! macro generates an undocumented benchmark harness function"
)]

use std::fs::{self, File};
use std::hint::black_box;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use kova_config::KovaPaths;
use kova_jvm_index::{
    ClasspathDiscoveryConfig, IndexerConfig, JvmIndexService, JvmIndexSnapshot, JvmModifiers,
    JvmQueryEngine, JvmSourceOrigin, JvmSymbolKind, JvmTypeEntry, JvmVisibility, ParsedClassUnit,
    QueryScratch, SearchQueryOptions,
};
use tempfile::TempDir;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

const JARS: usize = 40;
const CLASSES_PER_JAR: usize = 500;
const SOURCE_FILES: usize = 1_000;

#[allow(
    clippy::print_stderr,
    reason = "benches report fixture failures on stderr instead of panicking"
)]
fn report(context: &str, err: &dyn std::fmt::Display) {
    eprintln!("skipping benchmark: {context}: {err}");
}

fn minimal_classfile_bytes(internal_name: &str) -> io::Result<Vec<u8>> {
    let name_len = u16::try_from(internal_name.len())
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
    let obj = b"java/lang/Object";
    let mut out = Vec::with_capacity(64 + internal_name.len());
    out.extend_from_slice(&0xCAFE_BABEu32.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes()); // minor
    out.extend_from_slice(&52u16.to_be_bytes()); // major (Java 8)
    out.extend_from_slice(&5u16.to_be_bytes()); // constant_pool_count
    out.push(1); // #1 Utf8 internal_name
    out.extend_from_slice(&name_len.to_be_bytes());
    out.extend_from_slice(internal_name.as_bytes());
    out.push(7); // #2 Class -> #1
    out.extend_from_slice(&1u16.to_be_bytes());
    out.push(1); // #3 Utf8 java/lang/Object
    out.extend_from_slice(&16u16.to_be_bytes());
    out.extend_from_slice(obj);
    out.push(7); // #4 Class -> #3
    out.extend_from_slice(&3u16.to_be_bytes());
    out.extend_from_slice(&0x0021u16.to_be_bytes()); // ACC_PUBLIC | ACC_SUPER
    out.extend_from_slice(&2u16.to_be_bytes()); // this_class
    out.extend_from_slice(&4u16.to_be_bytes()); // super_class
    out.extend_from_slice(&[0; 8]); // interfaces, fields, methods, attributes counts
    Ok(out)
}

fn write_jar(path: &Path, jar: usize) -> io::Result<()> {
    let mut zip = ZipWriter::new(File::create(path)?);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for cls in 0..CLASSES_PER_JAR {
        let internal = format!("com/lib{jar}/LibClass{jar}x{cls}");
        zip.start_file(format!("{internal}.class"), options)
            .map_err(io::Error::other)?;
        zip.write_all(&minimal_classfile_bytes(&internal)?)?;
    }
    zip.finish().map_err(io::Error::other)?;
    Ok(())
}

/// Builds a project with 1,000 sources and 40 jars (20,000 classes) under `root`.
fn build_project(root: &Path) -> io::Result<()> {
    let src = root.join("src/main/java/com/acme");
    let libs = root.join("libs");
    fs::create_dir_all(&src)?;
    fs::create_dir_all(&libs)?;
    for i in 0..SOURCE_FILES {
        fs::write(
            src.join(format!("Service{i:04}.java")),
            format!(
                "package com.acme;\npublic class Service{i:04} {{ public void run{i}() {{}} }}\n"
            ),
        )?;
    }
    for jar in 0..JARS {
        write_jar(&libs.join(format!("dep{jar:02}.jar")), jar)?;
    }
    Ok(())
}

fn kova_paths(base: &Path) -> Option<KovaPaths> {
    KovaPaths::from_roots(base.join("config"), base.join("data"), base.join("cache"))
        .map_err(|err| report("kova paths", &err))
        .ok()
}

struct ProjectFixture {
    _temp: TempDir,
    root: PathBuf,
    service: JvmIndexService,
}

fn project_fixture() -> Option<ProjectFixture> {
    let temp = TempDir::new()
        .map_err(|err| report("temp dir", &err))
        .ok()?;
    let root = temp.path().join("project");
    build_project(&root)
        .map_err(|err| report("project fixture", &err))
        .ok()?;
    let service = JvmIndexService::new(IndexerConfig::default())
        .map_err(|err| report("index service", &err))
        .ok()?;
    Some(ProjectFixture {
        _temp: temp,
        root,
        service,
    })
}

fn bench_cold_and_warm_open(c: &mut Criterion) {
    let Some(fixture) = project_fixture() else {
        return;
    };
    let cp_cfg = ClasspathDiscoveryConfig::default();

    let mut group = c.benchmark_group("jvm_index_open");
    group.sample_size(10);

    group.bench_function("cold_index_20k_jar_classes", |b| {
        b.iter_batched(
            // A fresh, empty cache home per iteration keeps every open cold.
            TempDir::new,
            |cache_home| {
                let Ok(cache_home) = cache_home else {
                    return;
                };
                let Some(paths) = kova_paths(cache_home.path()) else {
                    return;
                };
                let metrics = fixture
                    .service
                    .open_project_sync(&fixture.root, &paths, &cp_cfg);
                black_box(metrics.map(|m| m.type_count).ok());
            },
            BatchSize::PerIteration,
        );
    });

    let warm_home = TempDir::new().map_err(|err| report("warm cache dir", &err));
    let warm_paths = warm_home
        .as_ref()
        .ok()
        .and_then(|home| kova_paths(home.path()));
    if let Some(paths) = warm_paths {
        // Populate the cache once; every measured open is then a warm hit.
        if let Err(err) = fixture
            .service
            .open_project_sync(&fixture.root, &paths, &cp_cfg)
        {
            report("warm cache population", &err);
        } else {
            group.bench_function("warm_cache_load_20k_classes", |b| {
                b.iter(|| {
                    let metrics = fixture
                        .service
                        .open_project_sync(&fixture.root, &paths, &cp_cfg);
                    black_box(metrics.map(|m| m.warm_cache).ok());
                });
            });
        }
    }
    group.finish();
}

fn build_50k_snapshot() -> JvmIndexSnapshot {
    let units: Vec<ParsedClassUnit> = (0..50_000)
        .map(|i| {
            let package_name = format!("com.example.mod{}", i % 100);
            let simple_name = format!("OrderServiceHandler{i}");
            let fqcn = format!("{package_name}.{simple_name}");
            ParsedClassUnit {
                ty: JvmTypeEntry {
                    binary_name: fqcn.clone(),
                    fqcn,
                    simple_name,
                    package_name,
                    kind: JvmSymbolKind::Class,
                    modifiers: JvmModifiers {
                        visibility: JvmVisibility::Public,
                        ..JvmModifiers::default()
                    },
                    super_fqcn: None,
                    interfaces: Vec::new(),
                    origin: JvmSourceOrigin::JarEntry {
                        jar_path: PathBuf::from("libs/huge.jar"),
                        entry_name: format!("com/example/OrderServiceHandler{i}.class"),
                    },
                },
                members: Vec::new(),
            }
        })
        .collect();
    JvmIndexSnapshot::from_units(&units)
}

fn bench_query_50k_classes(c: &mut Criterion) {
    let snapshot = build_50k_snapshot();
    let engine = JvmQueryEngine::new();
    let opts = SearchQueryOptions::default();
    let mut scratch = QueryScratch::new();

    for (name, query) in [
        ("nucleo_query_50k_classes_camel_humps", "OrdSerHan499"),
        ("nucleo_query_50k_classes_match_all", "osh"),
    ] {
        c.bench_function(name, |b| {
            b.iter(|| {
                let hits = engine.search_classes(
                    black_box(&snapshot),
                    black_box(query),
                    black_box(&opts),
                    &mut scratch,
                );
                black_box(hits);
            });
        });
    }
}

criterion_group!(benches, bench_cold_and_warm_open, bench_query_50k_classes);
criterion_main!(benches);
