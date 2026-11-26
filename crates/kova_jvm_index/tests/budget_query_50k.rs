//! Query latency gate (`< 5 ms` on 50,000 classes) and process RSS (`< 150 MiB`)
//! with 150,000 indexed members — KOVA-033 AC-4 / NFR-2 / NFR-3.
//!
//! Kept in its own test binary so the process-wide RSS measurement covers only
//! this scenario, not the large-project fixture of `budget_go_to_class`.

#![allow(
    clippy::expect_used,
    reason = "integration tests use expect for fixture setup"
)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use kova_core::perf::{
    BudgetKind, assert_within_budget, budgets::INPUT_LATENCY_MAX, check_duration_budget,
    check_rss_budget, current_process_rss_bytes,
};
use kova_jvm_index::{
    JvmIndexSnapshot, JvmMemberEntry, JvmModifiers, JvmQueryEngine, JvmSourceOrigin, JvmSymbolKind,
    JvmTypeEntry, JvmVisibility, ParsedClassUnit, QueryScratch, SearchQueryOptions,
};

/// Loose bound for unoptimised builds: catches hangs and gross regressions only.
const DEBUG_SANITY_MAX: Duration = Duration::from_secs(30);

fn assert_rss_within_budget() {
    let rss = current_process_rss_bytes().expect("process RSS must be measurable");
    assert_within_budget(&check_rss_budget(rss));
}

/// Members per synthetic class: 50k classes x 3 = 150k members (NFR-2 scale).
const MEMBERS_PER_SYNTHETIC_CLASS: usize = 3;

fn synthetic_50k_snapshot() -> JvmIndexSnapshot {
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
                members: (0..MEMBERS_PER_SYNTHETIC_CLASS)
                    .map(|m| JvmMemberEntry {
                        owner_fqcn: format!("com.example.mod{}.OrderServiceHandler{i}", i % 100),
                        name: format!("handle{m}"),
                        kind: JvmSymbolKind::Method,
                        modifiers: JvmModifiers::default(),
                        descriptor: String::from("(Ljava/lang/String;)V"),
                        display_signature: String::from("(String): void"),
                        line: None,
                    })
                    .collect(),
            }
        })
        .collect();
    JvmIndexSnapshot::from_units(&units)
}

/// Number of timed repetitions per query in the 50k-class latency test.
const QUERY_SAMPLES: usize = 9;

#[test]
fn query_50k_classes_within_input_latency() {
    let snapshot = synthetic_50k_snapshot();
    assert!(snapshot.member_count() >= 100_000);
    let engine = JvmQueryEngine::new();
    let mut scratch = QueryScratch::new();
    let opts = SearchQueryOptions::default();

    for query in ["OrdSerHan499", "osh", "com.ex.mod7.Handler42"] {
        let _warmup = engine.search_classes(&snapshot, query, &opts, &mut scratch);
        // Median of repeated keystroke queries, so one scheduler hiccup on a
        // shared CI machine does not decide the gate.
        let mut samples = Vec::with_capacity(QUERY_SAMPLES);
        let mut hits = Vec::new();
        for _ in 0..QUERY_SAMPLES {
            let start = Instant::now();
            hits = engine.search_classes(&snapshot, query, &opts, &mut scratch);
            samples.push(start.elapsed());
        }
        samples.sort_unstable();
        let elapsed = samples[QUERY_SAMPLES / 2];
        eprintln!(
            "query {query:?} on 50k classes: median {elapsed:?}, max {:?} ({} hits)",
            samples[QUERY_SAMPLES - 1],
            hits.len()
        );
        assert!(!hits.is_empty(), "query {query}");
        assert!(hits.len() <= opts.limit);

        let report = check_duration_budget(BudgetKind::InputLatency, elapsed, INPUT_LATENCY_MAX);
        if cfg!(debug_assertions) {
            assert!(elapsed < DEBUG_SANITY_MAX, "query {query} took {elapsed:?}");
        } else {
            assert_within_budget(&report);
        }
    }

    // Parallel chunked scoring must merge correctly: exact names from the first
    // and last chunk win, and results are fully ordered.
    for name in ["OrderServiceHandler0", "OrderServiceHandler49999"] {
        let hits = engine.search_classes(&snapshot, name, &opts, &mut scratch);
        assert_eq!(hits.first().map(|hit| hit.simple_name.as_str()), Some(name));
        assert!(hits.windows(2).all(|pair| pair[0].score >= pair[1].score));
    }
    assert_rss_within_budget();
}
