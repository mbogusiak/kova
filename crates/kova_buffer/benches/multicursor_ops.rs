//! Criterion benchmarks for multi-cursor motion and editing (`< 5 ms` budget).
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
    Buffer, Motion, MotionConfig, Point, Selection, SelectionSet, apply_delete_backward,
    apply_insert_all,
};

fn build_java_buffer(lines: usize) -> Buffer {
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

fn bench_move_down_500_cursors(c: &mut Criterion) {
    let buf = build_java_buffer(100_000);
    let snap = buf.snapshot();
    let cfg = MotionConfig::default();
    let base_sels = build_500_cursors(&buf);

    c.bench_function("move_down_500_cursors_100k_lines", |b| {
        b.iter(|| {
            let mut sels = base_sels.clone();
            sels.move_by(&snap, Motion::Down, false, cfg);
            black_box(sels);
        });
    });
}

fn bench_insert_delete_500_cursors(c: &mut Criterion) {
    let mut buf = build_java_buffer(100_000);
    let mut sels = build_500_cursors(&buf);

    c.bench_function("insert_char_500_cursors_100k_lines", |b| {
        b.iter(|| {
            let v = apply_insert_all(&mut buf, &mut sels, "x").expect("insert succeeds");
            black_box(v);
            apply_delete_backward(&mut buf, &mut sels).expect("delete reverts");
        });
    });
}

criterion_group!(
    benches,
    bench_move_down_500_cursors,
    bench_insert_delete_500_cursors
);
criterion_main!(benches);
