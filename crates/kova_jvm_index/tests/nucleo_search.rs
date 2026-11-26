//! Integration tests for `nucleo-matcher` Go to Class / Symbol search and `JvmIndexService` (KOVA-033).

#![allow(
    clippy::expect_used,
    reason = "integration tests use expect for fixture setup"
)]

mod support;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use kova_jvm_index::{
    ClasspathDiscoveryConfig, IndexPhase, IndexUpdateCallback, IndexerConfig, JvmIndexService,
    JvmIndexSnapshot, JvmMemberEntry, JvmModifiers, JvmQueryEngine, JvmSourceOrigin, JvmSymbolKind,
    JvmTypeEntry, JvmVisibility, ParsedClassUnit, QueryScratch, SearchQueryOptions,
    parse_class_query,
};
use tempfile::TempDir;

use support::{test_paths, write_jar, write_source};

fn make_type(fqcn: &str, origin: JvmSourceOrigin, is_deprecated: bool) -> JvmTypeEntry {
    let (package_name, simple_name) = kova_jvm_index::split_package_and_simple(fqcn);
    JvmTypeEntry {
        fqcn: fqcn.to_owned(),
        binary_name: fqcn.to_owned(),
        simple_name,
        package_name,
        kind: JvmSymbolKind::Class,
        modifiers: JvmModifiers {
            visibility: JvmVisibility::Public,
            is_deprecated,
            ..JvmModifiers::default()
        },
        super_fqcn: None,
        interfaces: Vec::new(),
        origin,
    }
}

fn make_method(owner_fqcn: &str, name: &str, is_deprecated: bool) -> JvmMemberEntry {
    JvmMemberEntry {
        owner_fqcn: owner_fqcn.to_owned(),
        name: name.to_owned(),
        kind: JvmSymbolKind::Method,
        modifiers: JvmModifiers {
            visibility: JvmVisibility::Public,
            is_deprecated,
            ..JvmModifiers::default()
        },
        descriptor: String::from("()V"),
        display_signature: String::from("(): void"),
        line: None,
    }
}

fn project(rel: &str) -> JvmSourceOrigin {
    JvmSourceOrigin::ProjectSource {
        rel_path: PathBuf::from(rel),
        line: 0,
        column: 0,
    }
}

fn sample_snapshot() -> JvmIndexSnapshot {
    let jar = JvmSourceOrigin::JarEntry {
        jar_path: PathBuf::from("libs/orders-1.0.jar"),
        entry_name: String::from("com/vendor/OrderService.class"),
    };
    let jdk = JvmSourceOrigin::JmodEntry {
        jmod_path: PathBuf::from("jmods/java.base.jmod"),
        entry_name: String::from("classes/java/util/ArrayList.class"),
    };

    JvmIndexSnapshot::from_units(&[
        ParsedClassUnit {
            ty: make_type(
                "com.example.OrderService",
                project("src/main/java/com/example/OrderService.java"),
                false,
            ),
            members: vec![make_method("com.example.OrderService", "placeOrder", false)],
        },
        ParsedClassUnit {
            ty: make_type("com.vendor.OrderService", jar, false),
            members: vec![make_method("com.vendor.OrderService", "placeOrder", false)],
        },
        ParsedClassUnit {
            ty: make_type(
                "com.legacy.OrderService",
                project("src/main/java/com/legacy/OrderService.java"),
                true,
            ),
            members: vec![make_method("com.legacy.OrderService", "placeOrder", false)],
        },
        ParsedClassUnit {
            ty: make_type("java.util.ArrayList", jdk, false),
            members: Vec::new(),
        },
        ParsedClassUnit {
            ty: make_type(
                "com.example.\u{17B}\u{F3}\u{142}\u{107}Service",
                project("src/main/java/com/example/ZolcService.java"),
                false,
            ),
            members: Vec::new(),
        },
    ])
}

#[test]
fn camel_humps_package_prefix_line_suffix_and_utf8_indices() {
    let snap = sample_snapshot();
    let engine = JvmQueryEngine::new();
    let mut scratch = QueryScratch::new();
    let opts = SearchQueryOptions::default();

    for query in ["OS", "OrdSer"] {
        let hits = engine.search_classes(&snap, query, &opts, &mut scratch);
        assert_eq!(hits.len(), 3, "query {query}");
        assert_eq!(hits[0].fqcn, "com.example.OrderService", "query {query}");
    }
    let os = engine.search_classes(&snap, "OS", &opts, &mut scratch);
    assert_eq!(os[0].matched_char_indices, vec![0, 5]);
    let ord_ser = engine.search_classes(&snap, "OrdSer", &opts, &mut scratch);
    assert_eq!(ord_ser[0].matched_char_indices, vec![0, 1, 2, 5, 6, 7]);

    let pkg_hits = engine.search_classes(&snap, "j.u.ArrLi", &opts, &mut scratch);
    assert_eq!(pkg_hits.len(), 1);
    assert_eq!(pkg_hits[0].fqcn, "java.util.ArrayList");
    assert_eq!(pkg_hits[0].matched_char_indices, vec![0, 1, 2, 5, 6]);

    let parsed = parse_class_query("OrderService:42");
    assert_eq!(parsed.target_line, Some(42));
    assert_eq!(parsed.type_pattern, "OrderService");
    let line_hits = engine.search_classes(&snap, "OrderService:42", &opts, &mut scratch);
    assert_eq!(line_hits[0].fqcn, "com.example.OrderService");
    assert!(line_hits.iter().all(|hit| hit.target_line == Some(42)));

    let project_only = SearchQueryOptions {
        include_jdk_and_libs: false,
        ..SearchQueryOptions::default()
    };
    assert_eq!(
        engine
            .search_classes(&snap, "ArrLi", &project_only, &mut scratch)
            .len(),
        0
    );
    let project_hits = engine.search_classes(&snap, "OrderService", &project_only, &mut scratch);
    assert!(
        project_hits
            .iter()
            .all(|hit| matches!(hit.origin, JvmSourceOrigin::ProjectSource { .. }))
    );

    // Multi-byte UTF-8 name: indices are Unicode character positions, not bytes.
    let utf8_hits = engine.search_classes(&snap, "\u{17B}Ser", &opts, &mut scratch);
    assert_eq!(utf8_hits.len(), 1);
    assert_eq!(
        utf8_hits[0].simple_name,
        "\u{17B}\u{F3}\u{142}\u{107}Service"
    );
    assert_eq!(utf8_hits[0].matched_char_indices, vec![0, 4, 5, 6]);
}

#[test]
fn ranking_prefers_project_then_libraries_and_penalises_deprecated() {
    let snap = sample_snapshot();
    let engine = JvmQueryEngine::new();
    let mut scratch = QueryScratch::new();
    let opts = SearchQueryOptions::default();

    let hits = engine.search_classes(&snap, "OrderService", &opts, &mut scratch);
    let order: Vec<&str> = hits.iter().map(|hit| hit.fqcn.as_str()).collect();
    assert_eq!(
        order,
        vec![
            "com.example.OrderService",
            "com.legacy.OrderService",
            "com.vendor.OrderService",
        ]
    );
    assert!(hits[0].score > hits[1].score);
    assert!(hits[1].is_deprecated);

    let no_deprecated = SearchQueryOptions {
        include_deprecated: false,
        ..SearchQueryOptions::default()
    };
    let filtered = engine.search_classes(&snap, "OrderService", &no_deprecated, &mut scratch);
    assert!(filtered.iter().all(|hit| !hit.is_deprecated));
    assert_eq!(filtered.len(), 2);

    let limited = SearchQueryOptions {
        limit: 1,
        ..SearchQueryOptions::default()
    };
    let top = engine.search_classes(&snap, "OrderService", &limited, &mut scratch);
    assert_eq!(top.len(), 1);
    assert_eq!(top[0].fqcn, "com.example.OrderService");

    // Empty query: project classes first, then by name.
    let all = engine.search_classes(&snap, "", &opts, &mut scratch);
    assert_eq!(all.len(), 5);
    assert!(matches!(
        all[0].origin,
        JvmSourceOrigin::ProjectSource { .. }
    ));
    assert_eq!(all[0].fqcn, "com.example.OrderService");
    assert_eq!(all[4].fqcn, "java.util.ArrayList");
}

#[test]
fn go_to_symbol_supports_owner_hash_member_and_ranking() {
    let snap = sample_snapshot();
    let engine = JvmQueryEngine::new();
    let mut scratch = QueryScratch::new();
    let opts = SearchQueryOptions::default();

    let hits = engine.search_symbols(&snap, "OrderService#placeOrder", &opts, &mut scratch);
    assert_eq!(hits.len(), 3);
    assert_eq!(hits[0].owner_fqcn, "com.example.OrderService");
    assert_eq!(hits[0].owner_simple_name, "OrderService");
    assert_eq!(hits[0].member_name, "placeOrder");
    assert_eq!(hits[0].kind, JvmSymbolKind::Method);
    assert_eq!(hits[0].matched_char_indices, (0..10).collect::<Vec<u32>>());
    // A deprecated project owner is penalised but still outranks a library owner.
    assert_eq!(hits[1].owner_fqcn, "com.legacy.OrderService");
    assert_eq!(hits[2].owner_fqcn, "com.vendor.OrderService");

    let bare = engine.search_symbols(&snap, "placeOrder", &opts, &mut scratch);
    assert_eq!(bare.len(), 3);
    assert_eq!(bare[0].owner_fqcn, "com.example.OrderService");

    let qualified = engine.search_symbols(&snap, "c.v.OrderService#plOr", &opts, &mut scratch);
    assert_eq!(qualified.len(), 1);
    assert_eq!(qualified[0].owner_fqcn, "com.vendor.OrderService");

    assert_eq!(
        engine
            .search_symbols(&snap, "ArrayList#placeOrder", &opts, &mut scratch)
            .len(),
        0
    );
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Observed {
    ready: bool,
    has_project_class: bool,
    has_jar_class: bool,
}

fn recording_callback(
    service: &Arc<JvmIndexService>,
    log: &Arc<Mutex<Vec<Observed>>>,
) -> IndexUpdateCallback {
    let service = Arc::downgrade(service);
    let log = Arc::clone(log);
    Arc::new(move |phase: &IndexPhase| {
        let Some(service) = service.upgrade() else {
            return;
        };
        let snap = service.snapshot();
        log.lock().expect("log").push(Observed {
            ready: matches!(phase, IndexPhase::Ready { .. }),
            has_project_class: snap.resolve_fqcn("com.example.InitialClass").is_some(),
            has_jar_class: snap.resolve_fqcn("com.lib.LibOnly").is_some(),
        });
    })
}

#[test]
fn background_service_two_phase_warm_restart_and_live_update() {
    let temp = TempDir::new().expect("temp");
    let project_root = temp.path().join("proj");
    write_source(
        &project_root,
        "src/main/java/com/example/InitialClass.java",
        "package com.example;\npublic class InitialClass {}\n",
    );
    write_jar(
        &project_root.join("libs/lib.jar"),
        &[String::from("com/lib/LibOnly")],
    );
    let paths = test_paths(&temp);
    let service = Arc::new(JvmIndexService::new(IndexerConfig::default()).expect("service"));
    assert_eq!(service.phase(), IndexPhase::Idle);

    // Cold start: sources-only snapshot first, then the full one.
    let log = Arc::new(Mutex::new(Vec::new()));
    let handle = service.start_project_indexing(
        project_root.clone(),
        paths.clone(),
        ClasspathDiscoveryConfig::default(),
        Some(recording_callback(&service, &log)),
    );
    assert!(handle.join());
    assert_eq!(
        *log.lock().expect("log"),
        vec![
            Observed {
                ready: false,
                has_project_class: true,
                has_jar_class: false,
            },
            Observed {
                ready: true,
                has_project_class: true,
                has_jar_class: true,
            },
        ]
    );
    assert!(matches!(
        service.phase(),
        IndexPhase::Ready {
            warm_cache: false,
            ..
        }
    ));
    let metrics = service.last_metrics().expect("metrics");
    assert!(!metrics.warm_cache);
    assert_eq!(metrics.type_count, 2);

    // Warm start: revalidated cache published directly as Ready.
    let warm_log = Arc::new(Mutex::new(Vec::new()));
    let handle = service.start_project_indexing(
        project_root.clone(),
        paths,
        ClasspathDiscoveryConfig::default(),
        Some(recording_callback(&service, &warm_log)),
    );
    assert!(handle.join());
    assert_eq!(
        *warm_log.lock().expect("log"),
        vec![Observed {
            ready: true,
            has_project_class: true,
            has_jar_class: true,
        }]
    );
    assert!(service.last_metrics().expect("metrics").warm_cache);

    // Live incremental update reuses search names of unchanged types.
    let before = service.snapshot();
    let rel = PathBuf::from("src/main/java/com/example/LiveUpdated.java");
    let content = "package com.example;\npublic class LiveUpdated {}\n";
    service
        .on_file_saved_or_changed(&project_root, &rel, content)
        .expect("live update");
    let after = service.snapshot();
    assert!(after.resolve_fqcn("com.example.LiveUpdated").is_some());
    assert!(after.resolve_fqcn("com.lib.LibOnly").is_some());
    assert_eq!(after.type_count(), before.type_count() + 1);
    assert!(after.shares_search_names_with(&before, "com.lib.LibOnly"));
    assert!(after.shares_search_names_with(&before, "com.example.InitialClass"));

    let hits = JvmQueryEngine::new().search_classes(
        &after,
        "LivUpd",
        &SearchQueryOptions::default(),
        &mut QueryScratch::new(),
    );
    assert_eq!(hits[0].fqcn, "com.example.LiveUpdated");
}

#[test]
fn superseded_job_never_overwrites_newer_results() {
    let temp = TempDir::new().expect("temp");
    let old_root = temp.path().join("old");
    let new_root = temp.path().join("new");
    write_source(
        &old_root,
        "src/com/old/OldOnly.java",
        "package com.old;\npublic class OldOnly {}\n",
    );
    write_jar(
        &old_root.join("libs/old.jar"),
        &[String::from("com/old/OldLib")],
    );
    write_source(
        &new_root,
        "src/com/fresh/NewOnly.java",
        "package com.fresh;\npublic class NewOnly {}\n",
    );
    let paths = test_paths(&temp);
    let service = Arc::new(JvmIndexService::new(IndexerConfig::default()).expect("service"));

    // While the old job is between its two phases, a newer synchronous open supersedes it.
    let weak = Arc::downgrade(&service);
    let sync_paths = paths.clone();
    let sync_root = new_root.clone();
    let callback: IndexUpdateCallback = Arc::new(move |phase: &IndexPhase| {
        if matches!(phase, IndexPhase::Indexing { .. })
            && let Some(service) = weak.upgrade()
        {
            service
                .open_project_sync(
                    &sync_root,
                    &sync_paths,
                    &ClasspathDiscoveryConfig::default(),
                )
                .expect("newer open");
        }
    });
    let old_job = service.start_project_indexing(
        old_root,
        paths,
        ClasspathDiscoveryConfig::default(),
        Some(callback),
    );
    assert!(old_job.join());

    let snap = service.snapshot();
    assert!(snap.resolve_fqcn("com.fresh.NewOnly").is_some());
    assert!(snap.resolve_fqcn("com.old.OldOnly").is_none());
    assert!(snap.resolve_fqcn("com.old.OldLib").is_none());
    assert!(matches!(
        service.phase(),
        IndexPhase::Ready { type_count: 1, .. }
    ));
    assert_eq!(service.last_metrics().expect("metrics").type_count, 1);
}

#[test]
fn cancelled_job_reports_cancelled_phase() {
    let temp = TempDir::new().expect("temp");
    let root = temp.path().join("proj");
    write_source(
        &root,
        "src/com/a/A.java",
        "package com.a;\npublic class A {}\n",
    );
    let paths = test_paths(&temp);
    let service = Arc::new(JvmIndexService::new(IndexerConfig::default()).expect("service"));

    let weak = Arc::downgrade(&service);
    let callback: IndexUpdateCallback = Arc::new(move |phase: &IndexPhase| {
        if matches!(phase, IndexPhase::Indexing { .. })
            && let Some(service) = weak.upgrade()
        {
            service.cancel_active_job();
        }
    });
    let job = service.start_project_indexing(
        root,
        paths,
        ClasspathDiscoveryConfig::default(),
        Some(callback),
    );
    assert!(job.join());
    assert_eq!(service.phase(), IndexPhase::Cancelled);
    assert!(service.last_metrics().is_none());
}
