//! Criterion benchmarks for viewport syntax highlighting across 10k-line Java and Kotlin files.
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
use kova_syntax::{Highlighter, LanguageRegistry, SyntaxEngine, SyntaxEngineConfig};
use kova_theme::SyntaxLanguage;

fn build_10k_lines_java_buffer() -> Buffer {
    // ~1_250 methods * 8 lines = ~10_000 lines.
    let mut text = String::with_capacity(1_250 * 220);
    text.push_str("package dev.kova.bench;\n\npublic class LargeService {\n");
    for idx in 0..1_250 {
        let _ = writeln!(
            text,
            "    /** Doc for method {idx}. */\n    @Test\n    public int compute{idx}(int input) {{\n        String label = \"item_{idx}\\n\";\n        if (input > 0 && label != null) {{\n            return input + {idx};\n        }}\n        return 0;\n    }}"
        );
    }
    text.push_str("}\n");
    Buffer::with_id(BufferId::new(10), &text)
}

fn build_10k_lines_kotlin_buffer() -> Buffer {
    // ~1_250 functions * 8 lines = ~10_000 lines.
    let mut text = String::with_capacity(1_250 * 220);
    text.push_str("package dev.kova.bench\n\nclass LargeKotlinService {\n");
    for idx in 0..1_250 {
        let _ = writeln!(
            text,
            "    /** KDoc for fn {idx}. */\n    @Test\n    fun compute{idx}(input: Int): Int {{\n        val label = \"item_${{input}}\\n\"\n        return if (input > 0 && label.isNotEmpty()) {{\n            input + {idx}\n        }} else {{\n            0\n        }}\n    }}"
        );
    }
    text.push_str("}\n");
    Buffer::with_id(BufferId::new(11), &text)
}

fn bench_highlight_viewport_80_lines_java_10k(c: &mut Criterion) {
    let registry = LanguageRegistry::builtin().expect("builtin registry initializes");
    let highlighter = Highlighter::new(registry.clone());
    let engine = SyntaxEngine::new(
        registry.clone(),
        SyntaxEngineConfig {
            worker_threads: 1,
            parse_timeout: Some(Duration::from_secs(10)),
            reuse_thread_local_parsers: true,
        },
    )
    .expect("SyntaxEngine initializes");

    let buffer = build_10k_lines_java_buffer();
    let snapshot = buffer.snapshot();
    assert!(snapshot.len_lines() >= 10_000);
    let tree = engine
        .parse_sync(SyntaxLanguage::Java, &snapshot, None)
        .expect("Java 10k parse succeeds");

    c.bench_function("highlight_viewport_80_lines_java_10k", |b| {
        b.iter(|| {
            let spans = highlighter
                .highlight_lines(black_box(&tree), black_box(&snapshot), 4_960..5_040)
                .expect("highlight_lines succeeds");
            black_box(spans.len());
        });
    });
}

fn bench_highlight_viewport_80_lines_kotlin_10k(c: &mut Criterion) {
    let registry = LanguageRegistry::builtin().expect("builtin registry initializes");
    let highlighter = Highlighter::new(registry.clone());
    let engine = SyntaxEngine::new(
        registry.clone(),
        SyntaxEngineConfig {
            worker_threads: 1,
            parse_timeout: Some(Duration::from_secs(10)),
            reuse_thread_local_parsers: true,
        },
    )
    .expect("SyntaxEngine initializes");

    let buffer = build_10k_lines_kotlin_buffer();
    let snapshot = buffer.snapshot();
    assert!(snapshot.len_lines() >= 10_000);
    let tree = engine
        .parse_sync(SyntaxLanguage::Kotlin, &snapshot, None)
        .expect("Kotlin 10k parse succeeds");

    c.bench_function("highlight_viewport_80_lines_kotlin_10k", |b| {
        b.iter(|| {
            let spans = highlighter
                .highlight_lines(black_box(&tree), black_box(&snapshot), 4_960..5_040)
                .expect("highlight_lines succeeds");
            black_box(spans.len());
        });
    });
}

criterion_group!(
    benches,
    bench_highlight_viewport_80_lines_java_10k,
    bench_highlight_viewport_80_lines_kotlin_10k
);
criterion_main!(benches);
