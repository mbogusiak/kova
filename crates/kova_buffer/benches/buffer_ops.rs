//! Criterion benchmarks verifying $O(1)$ `BufferSnapshot` cloning and `< 5 ms` edit latency.
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
use kova_buffer::{Buffer, Point, Utf16Point};

fn build_large_java_buffer(lines: usize) -> Buffer {
    let mut text = String::with_capacity(lines * 64);
    for idx in 0..lines {
        let _ = writeln!(
            text,
            "    private final String field_{idx} = \"zażółć 🚀 value_{idx}\";\r"
        );
    }
    Buffer::from_text(&text)
}

fn bench_snapshot_clone(c: &mut Criterion) {
    let small_buf = build_large_java_buffer(20);
    let large_buf = build_large_java_buffer(100_000);
    let small_snap = small_buf.snapshot();
    let large_snap = large_buf.snapshot();

    c.bench_function("snapshot_clone_1kb", |b| {
        b.iter(|| black_box(small_snap.clone()));
    });
    c.bench_function("snapshot_clone_10mb", |b| {
        b.iter(|| black_box(large_snap.clone()));
    });
}

fn bench_apply_edit_100k_lines(c: &mut Criterion) {
    let mut buf = build_large_java_buffer(100_000);
    let mid_byte = buf
        .snapshot()
        .point_to_byte(Point::new(50_000, 10))
        .expect("middle point valid");

    c.bench_function("apply_edit_middle_100k_lines", |b| {
        b.iter(|| {
            let v = buf
                .apply_edit(mid_byte..mid_byte, "x")
                .expect("insert succeeds");
            let snap = buf.snapshot();
            black_box((v, snap));
            buf.apply_edit(mid_byte..(mid_byte + 1), "")
                .expect("revert succeeds");
        });
    });
}

fn bench_coordinate_conversions(c: &mut Criterion) {
    let buf = build_large_java_buffer(100_000);
    let snap = buf.snapshot();
    let target_pt = Point::new(75_000, 48);

    c.bench_function("coordinate_conversions_utf16_point", |b| {
        b.iter(|| {
            let byte = snap
                .point_to_byte(black_box(target_pt))
                .expect("point valid");
            let u16_pt: Utf16Point = snap.byte_to_utf16_point(byte).expect("utf16 point valid");
            let roundtrip = snap.utf16_point_to_byte(u16_pt).expect("roundtrip valid");
            black_box(roundtrip);
        });
    });
}

criterion_group!(
    benches,
    bench_snapshot_clone,
    bench_apply_edit_100k_lines,
    bench_coordinate_conversions
);
criterion_main!(benches);
