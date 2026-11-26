//! Criterion benchmarks for full vs. incremental `Rope` parsing and `rayon` dispatch latency.
#![allow(
    missing_docs,
    reason = "criterion_group! macro generates an undocumented benchmark harness function"
)]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "benchmark setup panics on invalid synthetic fixture"
)]

use std::fmt::Write as _;
use std::hint::black_box;
use std::sync::mpsc;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use kova_buffer::{Buffer, BufferId, Point, TextEdit};
use kova_syntax::{LanguageRegistry, ParseRequest, SyntaxEdit, SyntaxEngine, SyntaxEngineConfig};
use kova_theme::SyntaxLanguage;

fn build_10k_lines_json_buffer() -> Buffer {
    // 2_500 objects * 4 lines = 10_000+ lines.
    let mut text = String::with_capacity(2_500 * 96);
    text.push_str("{\n  \"records\": [\n");
    for idx in 0..2_500 {
        let comma = if idx + 1 < 2_500 { "," } else { "" };
        let _ = writeln!(
            text,
            "    {{\n      \"id\": {idx},\n      \"label\": \"zażółć_🚀_{idx}\"\n    }}{comma}"
        );
    }
    text.push_str("  ]\n}\n");
    Buffer::with_id(BufferId::new(1), &text)
}

fn build_json_engine() -> SyntaxEngine {
    let mut registry = LanguageRegistry::new();
    registry
        .register(SyntaxLanguage::Json, tree_sitter_json::LANGUAGE.into())
        .expect("register JSON grammar");
    SyntaxEngine::new(
        registry,
        SyntaxEngineConfig {
            worker_threads: 2,
            parse_timeout: Some(Duration::from_secs(5)),
            reuse_thread_local_parsers: true,
        },
    )
    .expect("create SyntaxEngine")
}

fn bench_full_parse_10k_lines_json(c: &mut Criterion) {
    let engine = build_json_engine();
    let buffer = build_10k_lines_json_buffer();
    let snapshot = buffer.snapshot();

    c.bench_function("full_parse_10k_lines_json", |b| {
        b.iter(|| {
            let tree = engine
                .parse_sync(SyntaxLanguage::Json, black_box(&snapshot), None)
                .expect("full parse succeeds");
            black_box(tree.root_node().expect("json root node").end_byte());
        });
    });
}

fn bench_incremental_single_edit_10k_lines_json(c: &mut Criterion) {
    let engine = build_json_engine();
    let mut buffer = build_10k_lines_json_buffer();
    let base_snap = buffer.snapshot();
    let base_tree = engine
        .parse_sync(SyntaxLanguage::Json, &base_snap, None)
        .expect("initial parse succeeds");

    let target_byte = base_snap
        .point_to_byte(Point::new(5_000, 12))
        .expect("middle line valid");
    let edit = TextEdit::new(target_byte..target_byte, "X");
    let syntax_edit = SyntaxEdit::from_text_edit(&base_snap, &edit).expect("valid syntax edit");
    buffer
        .apply_edits(std::slice::from_ref(&edit))
        .expect("edit applies");
    let edited_snap = buffer.snapshot();

    c.bench_function("incremental_single_edit_10k_lines_json", |b| {
        b.iter(|| {
            let tree = engine
                .parse_sync(
                    SyntaxLanguage::Json,
                    black_box(&edited_snap),
                    Some((&base_tree, std::slice::from_ref(&syntax_edit))),
                )
                .expect("incremental parse succeeds");
            black_box(tree.was_incremental());
        });
    });
}

fn bench_schedule_parse_dispatch_latency(c: &mut Criterion) {
    let engine = build_json_engine();
    let buffer = build_10k_lines_json_buffer();
    let snapshot = buffer.snapshot();

    c.bench_function("schedule_parse_dispatch_latency", |b| {
        b.iter(|| {
            let (tx, rx) = mpsc::channel();
            let handle = engine.schedule_parse(
                ParseRequest::full(SyntaxLanguage::Json, snapshot.clone()),
                move |res| {
                    let _ = tx.send(res.is_ok());
                },
            );
            black_box(handle.target_version());
            let _ = rx.recv_timeout(Duration::from_secs(5));
        });
    });
}

criterion_group!(
    benches,
    bench_full_parse_10k_lines_json,
    bench_incremental_single_edit_10k_lines_json,
    bench_schedule_parse_dispatch_latency
);
criterion_main!(benches);
