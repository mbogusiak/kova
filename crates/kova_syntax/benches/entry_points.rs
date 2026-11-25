//! Criterion benchmarks for `detect_entry_points` across large Java `JUnit` and `.http` files.
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
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use kova_buffer::{Buffer, BufferId};
use kova_syntax::{LanguageRegistry, SyntaxEngine, SyntaxEngineConfig, detect_entry_points};
use kova_theme::SyntaxLanguage;

fn build_large_junit_java_buffer() -> Buffer {
    let mut text = String::with_capacity(50 * 240);
    text.push_str(
        "package dev.kova.bench;\n\nimport org.junit.jupiter.api.Test;\n\npublic class LargeSuiteTest {\n    public static void main(String[] args) {}\n\n",
    );
    for idx in 0..50 {
        let _ = writeln!(
            text,
            "    @Test\n    void shouldHandleCase{idx}() {{\n        int value = {idx} * 2;\n        if (value < 0) {{\n            throw new IllegalStateException();\n        }}\n    }}\n"
        );
    }
    text.push_str("}\n");
    Buffer::with_id(BufferId::new(20), &text)
}

fn build_100_requests_http_buffer() -> Buffer {
    let mut text = String::with_capacity(100 * 120);
    for idx in 0..100 {
        let _ = writeln!(
            text,
            "### Request {idx}\n# @name Request{idx}\nPOST https://api.kova.dev/v1/items/{idx}\nContent-Type: application/json\n\n{{\"id\": {idx}}}\n"
        );
    }
    Buffer::with_id(BufferId::new(21), &text)
}

fn bench_detect_entry_points_java_50_tests(c: &mut Criterion) {
    let registry = LanguageRegistry::builtin().expect("builtin registry initializes");
    let engine = SyntaxEngine::new(
        registry.clone(),
        SyntaxEngineConfig {
            worker_threads: 1,
            parse_timeout: Some(Duration::from_secs(10)),
            reuse_thread_local_parsers: true,
        },
    )
    .expect("SyntaxEngine initializes");

    let buffer = build_large_junit_java_buffer();
    let snapshot = buffer.snapshot();
    let tree = engine
        .parse_sync(SyntaxLanguage::Java, &snapshot, None)
        .expect("Java parse succeeds");

    c.bench_function("detect_entry_points_java_50_tests", |b| {
        b.iter(|| {
            let eps = detect_entry_points(black_box(&tree), black_box(&snapshot));
            black_box(eps.len());
        });
    });
}

fn bench_detect_entry_points_http_100_requests(c: &mut Criterion) {
    let registry = LanguageRegistry::builtin().expect("builtin registry initializes");
    let engine = SyntaxEngine::new(
        registry.clone(),
        SyntaxEngineConfig {
            worker_threads: 1,
            parse_timeout: Some(Duration::from_secs(10)),
            reuse_thread_local_parsers: true,
        },
    )
    .expect("SyntaxEngine initializes");

    let buffer = build_100_requests_http_buffer();
    let snapshot = buffer.snapshot();
    let tree = engine
        .parse_sync(SyntaxLanguage::Http, &snapshot, None)
        .expect("HTTP parse succeeds");

    c.bench_function("detect_entry_points_http_100_requests", |b| {
        b.iter(|| {
            let eps = detect_entry_points(black_box(&tree), black_box(&snapshot));
            black_box(eps.len());
        });
    });
}

criterion_group!(
    benches,
    bench_detect_entry_points_java_50_tests,
    bench_detect_entry_points_http_100_requests
);
criterion_main!(benches);
