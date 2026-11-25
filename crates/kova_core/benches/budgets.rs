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
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use kova_core::action::ActionId;
use kova_core::context::{ContextExpr, ContextState, FocusContext, FocusStack};
use kova_core::keymap::{
    BindingRule, BindingTarget, KeyChord, KeyCode, KeyStep, KeyStroke, KeymapLayer,
    KeymapLayerKind, KeymapPresetMode, KeymapResolver, Modifiers, Platform, build_effective_keymap,
};
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

fn bench_input_latency_keymap_resolver(c: &mut Criterion) {
    let mut rules = Vec::with_capacity(250);
    for i in 0..250 {
        let ch = char::from(b'a' + u8::try_from(i % 26).unwrap_or(0));
        let modifiers = Modifiers {
            ctrl: (i / 26) % 2 == 1,
            alt: (i / 52) % 2 == 1,
            shift: (i / 104) % 2 == 1,
            meta: true,
        };
        rules.push(BindingRule {
            id: i,
            layer: KeymapLayerKind::DefaultPreset,
            platform: Some(Platform::MacOs),
            root_context: Some(if i % 2 == 0 {
                FocusContext::EditorText
            } else {
                FocusContext::Workspace
            }),
            when: Some(
                ContextExpr::parse("!vim_enabled")
                    .unwrap_or_else(|_| ContextExpr::focus(FocusContext::Global)),
            ),
            chord: KeyChord::single(KeyStroke::new(modifiers, KeyCode::Char(ch))),
            target: BindingTarget::Action(ActionId::from_static("editor.duplicate_line")),
        });
    }

    let mut resolver = KeymapResolver::new(Platform::MacOs);
    resolver.add_layer(KeymapLayer::new(
        KeymapLayerKind::DefaultPreset,
        KeymapPresetMode::JetBrains,
        rules,
    ));

    let stack = FocusStack::from_leaf(FocusContext::EditorText);
    let state = ContextState::new();
    let probe = KeyStep::Stroke(KeyStroke::new(Modifiers::META, KeyCode::Char('d')));

    c.bench_function("budget_input_latency_keymap_resolver", |b| {
        b.iter(|| {
            let started = Instant::now();
            let dispatch =
                resolver.resolve_step(black_box(probe), black_box(&stack), black_box(&state));
            let report = check_duration_budget(
                BudgetKind::InputLatency,
                started.elapsed(),
                budgets::INPUT_LATENCY_MAX,
            );
            black_box((dispatch, report.is_within_budget()))
        });
    });
}

fn bench_input_latency_jetbrains_keymap(c: &mut Criterion) {
    let resolver = build_effective_keymap(Platform::MacOs, None)
        .unwrap_or_else(|_| KeymapResolver::new(Platform::MacOs));
    let stack = FocusStack::from_leaf(FocusContext::EditorText);
    let state = ContextState::new();
    let probe = KeyStep::Stroke(KeyStroke::new(Modifiers::META, KeyCode::Char('d')));

    c.bench_function("budget_input_latency_jetbrains_keymap", |b| {
        b.iter(|| {
            let started = Instant::now();
            let dispatch =
                resolver.resolve_step(black_box(probe), black_box(&stack), black_box(&state));
            let report = check_duration_budget(
                BudgetKind::InputLatency,
                started.elapsed(),
                budgets::INPUT_LATENCY_MAX,
            );
            black_box((dispatch, report.is_within_budget()))
        });
    });
}

criterion_group!(
    benches,
    bench_rss_probe_and_baseline,
    bench_input_latency_skeleton,
    bench_input_latency_keymap_resolver,
    bench_input_latency_jetbrains_keymap,
    bench_cold_start_skeleton,
    bench_go_to_class_skeleton,
);
criterion_main!(benches);
