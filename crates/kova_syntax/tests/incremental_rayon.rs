//! Headless integration tests for concurrent `rayon` parsing, latest-wins cancellation,
//! `EditLog` delta integration, and incremental AST equivalence.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "integration tests assert expected parse outcomes with explicit diagnostics"
)]

use std::fmt::Write as _;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;

use kova_buffer::{Buffer, BufferId, BufferVersion, TextEdit};
use kova_syntax::{
    LanguageRegistry, ParseRequest, SyntaxEngine, SyntaxEngineConfig, SyntaxError, SyntaxTree,
    SyntaxTreeStore,
};
use kova_theme::SyntaxLanguage;

fn build_json_engine(worker_threads: usize) -> SyntaxEngine {
    let mut registry = LanguageRegistry::new();
    registry
        .register(SyntaxLanguage::Json, tree_sitter_json::LANGUAGE.into())
        .expect("register tree-sitter-json");
    SyntaxEngine::new(
        registry,
        SyntaxEngineConfig {
            worker_threads,
            parse_timeout: Some(Duration::from_secs(5)),
            reuse_thread_local_parsers: true,
        },
    )
    .expect("create SyntaxEngine")
}

fn build_large_json(entries: usize) -> String {
    let mut text = String::with_capacity(entries * 56);
    text.push_str("{\n  \"items\": [\n");
    for idx in 0..entries {
        let comma = if idx + 1 < entries { "," } else { "" };
        let _ = writeln!(
            text,
            "    {{\"id\": {idx}, \"tag\": \"v_{idx}\", \"ok\": true}}{comma}"
        );
    }
    text.push_str("  ]\n}\n");
    text
}

#[test]
fn rayon_cancellation_and_latest_wins() {
    let engine = build_json_engine(2);
    let mut buffer = Buffer::with_id(BufferId::new(100), &build_large_json(800));
    let store = Arc::new(SyntaxTreeStore::new());

    let (tx, rx) = mpsc::channel();
    let total_edits = 50usize;
    let cancelled_count = Arc::new(AtomicUsize::new(0));
    let completed_count = Arc::new(AtomicUsize::new(0));

    // Seed version 0 synchronously so subsequent EditLog deltas can build incrementally.
    let snap0 = buffer.snapshot();
    let tree0 = engine
        .parse_sync(SyntaxLanguage::Json, &snap0, None)
        .expect("initial parse succeeds");
    assert!(store.insert_if_newer(tree0));

    for step in 0..total_edits {
        let pos = buffer.snapshot().text().find("\"v_").expect("tag present");
        let end = pos
            + buffer.snapshot().text()[pos..]
                .find("\", ")
                .expect("end quote");
        buffer
            .apply_edit(pos..end, &format!("\"edit_{step}"))
            .expect("edit applies");

        let snap = buffer.snapshot();
        let prev = store.get(snap.id());
        let req = ParseRequest::from_edit_log(
            SyntaxLanguage::Json,
            snap.clone(),
            prev.as_ref(),
            buffer.edit_log(),
        );

        let store_ref = Arc::clone(&store);
        let cancelled_ref = Arc::clone(&cancelled_count);
        let completed_ref = Arc::clone(&completed_count);
        let tx_clone = tx.clone();

        let _handle = engine.schedule_parse(req, move |res| {
            match res {
                Ok(tree) => {
                    let _inserted = store_ref.insert_if_newer(tree);
                    completed_ref.fetch_add(1, Ordering::AcqRel);
                }
                Err(SyntaxError::ParseCancelled { .. }) => {
                    cancelled_ref.fetch_add(1, Ordering::AcqRel);
                }
                Err(other) => panic!("unexpected syntax error during rapid edits: {other:?}"),
            }
            let _ = tx_clone.send(());
        });
    }
    drop(tx);

    for _ in 0..total_edits {
        rx.recv_timeout(Duration::from_secs(10))
            .expect("worker job should complete within timeout");
    }

    let final_snap = buffer.snapshot();
    assert_eq!(final_snap.version(), BufferVersion::new(total_edits as u64));

    // Schedule one final parse if the last rapid wave already finished or to guarantee final state.
    let final_tree = store
        .get(final_snap.id())
        .expect("store contains final tree");
    assert_eq!(final_tree.version(), final_snap.version());
    assert!(!final_tree.has_error());

    let reference = engine
        .parse_sync(SyntaxLanguage::Json, &final_snap, None)
        .expect("reference full parse");
    assert_eq!(
        final_tree.root_node().unwrap().to_sexp(),
        reference.root_node().unwrap().to_sexp()
    );
    assert!(
        completed_count.load(Ordering::Acquire) >= 1,
        "at least the latest job must complete"
    );
    assert_eq!(
        completed_count.load(Ordering::Acquire) + cancelled_count.load(Ordering::Acquire),
        total_edits
    );
}

#[test]
fn explicit_job_handle_cancellation_on_large_json() {
    let engine = build_json_engine(1);
    let large_json = build_large_json(3_000);
    assert!(large_json.len() > 100 * 1024);

    let buffer = Buffer::with_id(BufferId::new(200), &large_json);
    let snap = buffer.snapshot();

    // Occupy the single worker thread briefly so the queued job can be cancelled deterministically
    // and also verify immediate handle.cancel() behaviour.
    let pair = Arc::new((Mutex::new(false), Condvar::new()));
    let pair_worker = Arc::clone(&pair);
    let dummy_snap = Buffer::with_id(BufferId::new(201), "{}").snapshot();
    let _blocker = engine.schedule_parse(
        ParseRequest::full(SyntaxLanguage::Json, dummy_snap),
        move |_| {
            let (lock, cvar) = &*pair_worker;
            let mut started = lock.lock().unwrap();
            *started = true;
            cvar.notify_all();
        },
    );

    let (lock, cvar) = &*pair;
    let mut started = lock.lock().unwrap();
    while !*started {
        started = cvar.wait(started).unwrap();
    }
    drop(started);

    let (tx, rx) = mpsc::channel();
    let handle = engine.schedule_parse(
        ParseRequest::full(SyntaxLanguage::Json, snap.clone()),
        move |res| {
            drop(tx.send(res));
        },
    );
    handle.cancel();
    assert!(handle.is_cancelled());
    assert_eq!(handle.buffer_id(), BufferId::new(200));
    assert_eq!(handle.target_version(), snap.version());

    let result = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("cancelled job reports completion");
    assert!(matches!(result, Err(SyntaxError::ParseCancelled { .. })));

    // Confirm engine remains completely healthy for a subsequent parse on the same thread.
    let (tx2, rx2) = mpsc::channel();
    let _ok_handle =
        engine.schedule_parse(ParseRequest::full(SyntaxLanguage::Json, snap), move |res| {
            drop(tx2.send(res));
        });
    let ok_tree = rx2
        .recv_timeout(Duration::from_secs(5))
        .expect("follow-up parse finishes")
        .expect("follow-up parse succeeds");
    assert!(!ok_tree.has_error());
}

#[test]
fn incremental_edit_matches_full_reparse_for_insert_and_delete_lines() {
    let engine = build_json_engine(2);
    let initial = "{\n  \"alpha\": 1,\n  \"beta\": 2,\n  \"gamma\": 3\n}\n";
    let mut buffer = Buffer::with_id(BufferId::new(300), initial);
    let snap0 = buffer.snapshot();
    let tree0: SyntaxTree = engine
        .parse_sync(SyntaxLanguage::Json, &snap0, None)
        .expect("v0 parse");
    let tree0_sexp = tree0.root_node().unwrap().to_sexp();

    // Insert two lines in the middle.
    let insert_at = initial.find("  \"beta\"").expect("beta line");
    buffer
        .apply_edit(
            insert_at..insert_at,
            "  \"inserted_one\": \"zażółć\",\n  \"inserted_two\": \"🚀\",\n",
        )
        .expect("insert lines");
    let snap1 = buffer.snapshot();

    let (tx1, rx1) = mpsc::channel();
    let req1 = ParseRequest::from_edit_log(
        SyntaxLanguage::Json,
        snap1.clone(),
        Some(&tree0),
        buffer.edit_log(),
    );
    let _h1 = engine.schedule_parse(req1, move |res| {
        drop(tx1.send(res));
    });
    let tree1 = rx1
        .recv_timeout(Duration::from_secs(5))
        .expect("v1 done")
        .expect("v1 ok");
    assert!(tree1.was_incremental());
    assert!(!tree1.has_error());

    let full1 = engine
        .parse_sync(SyntaxLanguage::Json, &snap1, None)
        .expect("full v1");
    assert_eq!(
        tree1.root_node().unwrap().to_sexp(),
        full1.root_node().unwrap().to_sexp()
    );
    // `tree0` remains completely unchanged.
    assert_eq!(tree0.root_node().unwrap().to_sexp(), tree0_sexp);

    // Now delete a line in the middle using a TextEdit delta.
    let text1 = snap1.text();
    let del_start = text1.find("  \"beta\"").expect("beta start");
    let del_end = del_start + text1[del_start..].find('\n').expect("newline after beta") + 1;
    buffer
        .apply_edits(&[TextEdit::delete(del_start..del_end)])
        .expect("delete line");
    let snap2 = buffer.snapshot();

    let tree2 = engine
        .parse_with_edit_log(
            SyntaxLanguage::Json,
            &snap2,
            Some(&tree1),
            buffer.edit_log(),
        )
        .expect("v2 incremental");
    assert!(tree2.was_incremental());
    assert!(!tree2.has_error());

    let full2 = engine
        .parse_sync(SyntaxLanguage::Json, &snap2, None)
        .expect("full v2");
    assert_eq!(
        tree2.root_node().unwrap().to_sexp(),
        full2.root_node().unwrap().to_sexp()
    );
}

#[test]
fn concurrent_multi_buffer_parsing_and_error_paths() {
    let engine = build_json_engine(4);
    assert_eq!(engine.worker_thread_count(), 4);

    let (tx, rx) = mpsc::channel();
    for id in 1..=8u64 {
        let buf = Buffer::with_id(
            BufferId::new(id),
            &format!("{{\"worker\": {id}, \"ok\": true}}"),
        );
        let snap = buf.snapshot();
        let tx_clone = tx.clone();
        drop(
            engine.schedule_parse(ParseRequest::full(SyntaxLanguage::Json, snap), move |res| {
                drop(tx_clone.send((id, res)));
            }),
        );
    }
    drop(tx);

    let mut seen = Vec::new();
    for _ in 0..8 {
        let (id, res) = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("job completes");
        let tree = res.expect("valid json");
        assert_eq!(tree.buffer_id(), BufferId::new(id));
        assert!(!tree.has_error());
        seen.push(id);
    }
    seen.sort_unstable();
    assert_eq!(seen, vec![1, 2, 3, 4, 5, 6, 7, 8]);

    // Unsupported language error via both sync and async paths.
    let empty_buf = Buffer::with_id(BufferId::new(999), "class Main {}");
    let err = engine
        .parse_sync(SyntaxLanguage::Java, &empty_buf.snapshot(), None)
        .expect_err("Java not registered");
    assert_eq!(
        err,
        SyntaxError::UnsupportedLanguage {
            language: SyntaxLanguage::Java
        }
    );

    let (tx_err, rx_err) = mpsc::channel();
    drop(engine.schedule_parse(
        ParseRequest::full(SyntaxLanguage::Java, empty_buf.snapshot()),
        move |res| {
            drop(tx_err.send(res));
        },
    ));
    let async_err = rx_err
        .recv_timeout(Duration::from_secs(5))
        .expect("error callback fires")
        .expect_err("Java still unsupported");
    assert_eq!(
        async_err,
        SyntaxError::UnsupportedLanguage {
            language: SyntaxLanguage::Java
        }
    );

    // Zero worker threads fails gracefully without panicking.
    let init_err = SyntaxEngine::new(
        LanguageRegistry::new(),
        SyntaxEngineConfig {
            worker_threads: 0,
            ..SyntaxEngineConfig::default()
        },
    )
    .expect_err("0 workers rejected");
    assert!(matches!(init_err, SyntaxError::ThreadPoolInit { .. }));
}
