//! Criterion benchmarks verifying `< 5 ms` edit + `EditLog` delta computation and query latency.
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

use criterion::{Criterion, criterion_group, criterion_main};
use kova_buffer::{
    Buffer, BufferVersion, Point, Selection, SelectionSet, apply_delete_backward, apply_insert_all,
};

fn build_large_buffer(lines: usize) -> Buffer {
    let mut text = String::with_capacity(lines * 64);
    for idx in 0..lines {
        let _ = writeln!(
            text,
            "    private final String field_{idx} = \"zażółć 🚀 value_{idx}\";"
        );
    }
    Buffer::from_text(&text)
}

fn build_500_cursors(buf: &Buffer) -> SelectionSet {
    let snap = buf.snapshot();
    let cursors: Vec<Selection> = (0..500)
        .map(|i| {
            let row = i * 100 + 10;
            let byte = snap
                .point_to_byte(Point::new(row, 12))
                .expect("valid cursor row");
            Selection::caret(byte)
        })
        .collect();
    SelectionSet::from_selections(cursors, 0, &snap)
}

fn bench_single_edit_with_edit_log(c: &mut Criterion) {
    let mut buf = build_large_buffer(100_000);
    let mid_byte = buf
        .snapshot()
        .point_to_byte(Point::new(50_000, 12))
        .expect("middle point valid");

    c.bench_function("edit_log_single_edit_and_query_100k_lines", |b| {
        b.iter(|| {
            let v_before = buf.version();
            let v_after = buf
                .apply_edit(mid_byte..mid_byte, "x")
                .expect("insert succeeds");
            let ts = buf.tree_sitter_edits_since(v_before);
            let lsp = buf.lsp_changes_since(v_before);
            black_box((v_after, ts, lsp));
            buf.apply_edit(mid_byte..(mid_byte + 1), "")
                .expect("revert succeeds");
        });
    });
}

fn bench_500_cursors_with_edit_log(c: &mut Criterion) {
    let mut buf = build_large_buffer(100_000);
    let mut sels = build_500_cursors(&buf);

    c.bench_function("edit_log_500_cursors_and_query_100k_lines", |b| {
        b.iter(|| {
            let v_before: BufferVersion = buf.version();
            let v_after = apply_insert_all(&mut buf, &mut sels, "x").expect("insert succeeds");
            let ts = buf.tree_sitter_edits_since(v_before);
            let lsp = buf.lsp_changes_since(v_before);
            black_box((v_after, ts, lsp));
            apply_delete_backward(&mut buf, &mut sels).expect("delete reverts");
        });
    });
}

criterion_group!(
    benches,
    bench_single_edit_with_edit_log,
    bench_500_cursors_with_edit_log
);
criterion_main!(benches);
