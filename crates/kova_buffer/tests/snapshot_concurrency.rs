//! Integration test proving `BufferSnapshot` isolation across concurrent reader threads.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "integration test assertions abort on unexpected buffer failures"
)]

use kova_buffer::{Buffer, BufferVersion, Point};

#[test]
fn concurrent_readers_observe_immutable_snapshot_during_mutations() {
    let mut initial = String::with_capacity(200_000);
    for i in 0..10_000 {
        initial.push_str("public static void line_");
        initial.push_str(&i.to_string());
        initial.push_str("() { /* zażółć 🚀 */ }\n");
    }

    let mut buffer = Buffer::from_text(&initial);
    let snap_v0 = buffer.snapshot();
    let expected_len = snap_v0.len_bytes();
    let expected_lines = snap_v0.len_lines();

    std::thread::scope(|scope| {
        for worker_id in 0..4 {
            let reader_snap = snap_v0.clone();
            scope.spawn(move || {
                assert_eq!(reader_snap.version(), BufferVersion::new(0));
                assert_eq!(reader_snap.len_bytes(), expected_len);
                assert_eq!(reader_snap.len_lines(), expected_lines);
                for row in (worker_id..10_000).step_by(200) {
                    let byte = reader_snap
                        .point_to_byte(Point::new(row, 0))
                        .expect("row start must remain valid");
                    let utf16_pt = reader_snap
                        .byte_to_utf16_point(byte)
                        .expect("utf16 conversion must succeed");
                    assert_eq!(utf16_pt.row, row);
                }
            });
        }

        // Mutate the buffer 500 times while readers are actively reading `snap_v0`.
        for step in 0..500 {
            let text = format!("// edit {step}\n");
            buffer
                .apply_edit(0..0, &text)
                .expect("prefix insertion must succeed");
        }
    });

    assert_eq!(snap_v0.version(), BufferVersion::new(0));
    assert_eq!(snap_v0.len_bytes(), expected_len);

    let snap_v500 = buffer.snapshot();
    assert_eq!(snap_v500.version(), BufferVersion::new(500));
    assert_eq!(snap_v500.len_lines(), expected_lines + 500);
    assert!(snap_v500.len_bytes() > expected_len);
}
