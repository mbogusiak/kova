//! Budget gate for Go to Class readiness (`< 1.5 s` cold, `< 50 ms` warm), process
//! RSS (`< 150 MiB`) and query latency on a 21,000-class project — KOVA-033 AC-4.
//!
//! Fixture generation is never part of a measurement. Hard time budgets are
//! asserted in optimised builds (`cargo test --release`); debug builds run the same
//! full-scale scenario with RSS and functional checks plus a loose sanity bound.

#![allow(
    clippy::expect_used,
    reason = "integration tests use expect for fixture setup"
)]

mod support;

use std::time::{Duration, Instant};

use kova_core::perf::{
    BudgetKind, assert_within_budget, budgets::INPUT_LATENCY_MAX, check_duration_budget,
    check_rss_budget, current_process_rss_bytes,
};
use kova_jvm_index::{
    ClasspathDiscoveryConfig, IndexerConfig, JvmIndexService, JvmQueryEngine, QueryScratch,
    SearchQueryOptions,
};
use tempfile::TempDir;

use support::{test_paths, write_jar, write_source};

const SOURCE_FILES: usize = 1_000;
const JARS: usize = 40;
const CLASSES_PER_JAR: usize = 500;
/// Warm start budget from the spec (NFR-1).
const WARM_START_MAX: Duration = Duration::from_millis(50);
/// Number of warm opens; the fastest one is checked against [`WARM_START_MAX`].
const WARM_RUNS: usize = 3;
/// Loose bound for unoptimised builds: catches hangs and gross regressions only.
const DEBUG_SANITY_MAX: Duration = Duration::from_secs(30);

fn build_large_project(root: &std::path::Path) {
    write_source(
        root,
        "src/main/java/com/acme/OrderService.java",
        "package com.acme;\npublic class OrderService { public void placeOrder() {} }\n",
    );
    for i in 0..SOURCE_FILES / 2 {
        write_source(
            root,
            &format!("src/main/java/com/acme/svc/Service{i:04}.java"),
            &format!(
                "package com.acme.svc;\npublic class Service{i:04} {{\n  private int count{i};\n  public void run{i}() {{}}\n}}\n"
            ),
        );
        write_source(
            root,
            &format!("src/main/kotlin/com/acme/kt/KtService{i:04}.kt"),
            &format!("package com.acme.kt\n\nclass KtService{i:04} {{\n  fun run{i}() {{}}\n}}\n"),
        );
    }
    for jar in 0..JARS {
        let names: Vec<String> = (0..CLASSES_PER_JAR)
            .map(|cls| format!("com/lib{jar}/LibClass{jar}x{cls}"))
            .collect();
        write_jar(&root.join(format!("libs/dep{jar:02}.jar")), &names);
    }
}

fn assert_rss_within_budget() {
    let rss = current_process_rss_bytes().expect("process RSS must be measurable");
    assert_within_budget(&check_rss_budget(rss));
}

#[test]
fn budget_go_to_class_cold_warm_and_rss() {
    let temp = TempDir::new().expect("temp");
    let project_root = temp.path().join("large_project");
    build_large_project(&project_root);
    let paths = test_paths(&temp);
    let cp_cfg = ClasspathDiscoveryConfig::default();
    let service = JvmIndexService::new(IndexerConfig::default()).expect("service");

    // Cold open: no cache yet. `metrics.elapsed` covers only the open itself.
    let cold = service
        .open_project_sync(&project_root, &paths, &cp_cfg)
        .expect("cold open");
    eprintln!(
        "cold open: {:?} ({} types, {} members)",
        cold.elapsed, cold.type_count, cold.member_count
    );
    assert!(!cold.warm_cache);
    assert!(cold.type_count > SOURCE_FILES + JARS * CLASSES_PER_JAR);
    let cold_rss = cold.rss_report.as_ref().expect("RSS report");
    assert_within_budget(cold_rss);

    // Warm open from the binary cache. Best of `WARM_RUNS` filters out scheduler
    // noise from other tests running in parallel on the same machine.
    let mut warm_best = Duration::MAX;
    for _ in 0..WARM_RUNS {
        let warm = service
            .open_project_sync(&project_root, &paths, &cp_cfg)
            .expect("warm open");
        assert!(warm.warm_cache);
        assert_eq!(warm.type_count, cold.type_count);
        assert_eq!(warm.member_count, cold.member_count);
        warm_best = warm_best.min(warm.elapsed);
    }
    eprintln!("warm open (best of {WARM_RUNS}): {warm_best:?}");

    // Query the published snapshot.
    let snapshot = service.snapshot();
    let engine = JvmQueryEngine::new();
    let mut scratch = QueryScratch::new();
    let opts = SearchQueryOptions::default();
    let _warmup = engine.search_classes(&snapshot, "OrdSer", &opts, &mut scratch);
    let query_start = Instant::now();
    let hits = engine.search_classes(&snapshot, "OrdSer", &opts, &mut scratch);
    let query_elapsed = query_start.elapsed();
    eprintln!(
        "query OrdSer on {} types: {query_elapsed:?}",
        snapshot.type_count()
    );
    assert_eq!(
        hits.first().map(|hit| hit.fqcn.as_str()),
        Some("com.acme.OrderService")
    );
    assert_rss_within_budget();

    let query_report =
        check_duration_budget(BudgetKind::InputLatency, query_elapsed, INPUT_LATENCY_MAX);
    if cfg!(debug_assertions) {
        assert!(
            cold.elapsed < DEBUG_SANITY_MAX,
            "cold open {:?}",
            cold.elapsed
        );
        assert!(warm_best < DEBUG_SANITY_MAX, "warm open {warm_best:?}");
        eprintln!(
            "debug build: hard budgets skipped (cold within budget: {}, warm < {WARM_START_MAX:?}: {}, query within budget: {})",
            cold.duration_report.is_within_budget(),
            warm_best < WARM_START_MAX,
            query_report.is_within_budget()
        );
    } else {
        assert_within_budget(&cold.duration_report);
        assert!(
            warm_best < WARM_START_MAX,
            "warm start {warm_best:?} exceeds {WARM_START_MAX:?}"
        );
        assert_within_budget(&query_report);
    }
}
