//! Criterion benchmark skeleton for Kova IDE's Section 2 performance budgets.
//!
//! These benchmarks operate strictly in **Headless Core** (`kova_core`) and
//! establish the four named budget groups (`budget_rss_probe_and_baseline`,
//! `budget_input_latency_skeleton`, `budget_cold_start_skeleton`,
//! `budget_go_to_class_skeleton`) to be wired to real subsystems in E2/E3/E7.

#![allow(
    missing_docs,
    reason = "criterion_group! expands to an undocumented pub runner fn in this bench binary"
)]

use std::hint::black_box;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use kova_core::perf::{
    BudgetKind, budgets, check_duration_budget, check_rss_budget, current_process_rss_bytes,
    sample_process_memory,
};

fn bench_rss_probe_and_baseline(c: &mut Criterion) {
    // Verify baseline RSS reporting path once before micro-benchmarking the probe.
    if let Ok(rss_bytes) = current_process_rss_bytes() {
        let report = check_rss_budget(rss_bytes);
        let _ = black_box(report.is_within_budget());
    }

    c.bench_function("budget_rss_probe_and_baseline", |b| {
        b.iter(|| {
            let snapshot = sample_process_memory().ok();
            black_box(snapshot)
        });
    });
}

fn bench_input_latency_skeleton(c: &mut Criterion) {
    c.bench_function("budget_input_latency_skeleton", |b| {
        b.iter(|| {
            // Zero-I/O headless dispatch skeleton representing the < 5 ms budget envelope.
            let simulated_elapsed = black_box(Duration::from_micros(120));
            let report = check_duration_budget(
                BudgetKind::InputLatency,
                simulated_elapsed,
                budgets::INPUT_LATENCY_MAX,
            );
            black_box(report.is_within_budget())
        });
    });
}

fn bench_cold_start_skeleton(c: &mut Criterion) {
    c.bench_function("budget_cold_start_skeleton", |b| {
        b.iter(|| {
            // Headless core bootstrap placeholder; E3 wires full GPUI cold-start timing.
            let crate_id = black_box(kova_core::CRATE_NAME);
            let report = check_duration_budget(
                BudgetKind::ColdStart,
                Duration::from_millis(1),
                budgets::COLD_START_MAX,
            );
            black_box((crate_id, report.margin()))
        });
    });
}

fn bench_go_to_class_skeleton(c: &mut Criterion) {
    let synthetic_classes = [
        "java/lang/String",
        "java/util/concurrent/ConcurrentHashMap",
        "org/springframework/boot/SpringApplication",
        "io/kova/example/MainService",
    ];

    c.bench_function("budget_go_to_class_skeleton", |b| {
        b.iter(|| {
            // In-memory scan placeholder; E7 wires `kova_jvm_index` (< 1.5 s).
            let query = black_box("MainService");
            let matched = synthetic_classes
                .iter()
                .filter(|fqcn| fqcn.contains(query))
                .count();
            let report = check_duration_budget(
                BudgetKind::GoToClassReady,
                Duration::from_millis(10),
                budgets::GO_TO_CLASS_READY_MAX,
            );
            black_box((matched, report.is_within_budget()))
        });
    });
}

criterion_group!(
    benches,
    bench_rss_probe_and_baseline,
    bench_input_latency_skeleton,
    bench_cold_start_skeleton,
    bench_go_to_class_skeleton,
);
criterion_main!(benches);
