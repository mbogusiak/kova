//! KOVA-038 integration tests: `JdtlsSessionManager` auto-hibernation, RSS budget, LRU and
//! single-flight wake-up, driven by a mock `jdt.ls` on `tokio::io::duplex` and the paused Tokio
//! clock (15 virtual minutes elapse in milliseconds).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    reason = "integration tests"
)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kova_buffer::{Buffer, BufferId};
use kova_config::{EffectiveConfig, KovaPaths, KovaSettings};
use kova_core::perf::budgets::DEFAULT_JVM_HIBERNATION_IDLE;
use kova_lsp_dap::LspDapError;
use kova_lsp_dap::lsp::{
    ClientRuntimeConfig, JdtlsClient, path_to_lsp_uri, read_lsp_frame, write_lsp_frame,
};
use kova_lsp_dap::manager::{
    ActivityKind, HibernationStore, JdtlsLauncher, JdtlsSessionManager, LaunchFuture,
    LaunchedJdtls, MemoryPressureLevel, MockProcessMemoryProbe, OpenBuffer, OpenBufferProvider,
    ProjectId, ProjectSessionPhase, SessionEvent, SessionManagerConfig,
};
use serde_json::{Value, json};
use tokio::io::{BufReader, DuplexStream};
use tokio::sync::broadcast;

const STEP: Duration = Duration::from_secs(5);
const GIB: u64 = 1024 * 1024 * 1024;

/// Messages received by one mock server instance.
#[derive(Debug, Default)]
struct ServerLog {
    messages: Mutex<Vec<Value>>,
}

impl ServerLog {
    fn methods(&self) -> Vec<String> {
        self.messages
            .lock()
            .unwrap()
            .iter()
            .filter_map(|m| m["method"].as_str().map(str::to_owned))
            .collect()
    }

    fn did_open_texts(&self) -> Vec<String> {
        self.messages
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m["method"] == "textDocument/didOpen")
            .map(|m| {
                m["params"]["textDocument"]["text"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect()
    }
}

async fn serve(stream: DuplexStream, log: Arc<ServerLog>, diagnostics: Option<Value>) {
    let (read, mut write) = tokio::io::split(stream);
    let mut read = BufReader::new(read);
    while let Ok(Some(frame)) = read_lsp_frame(&mut read).await {
        let message: Value = serde_json::from_slice(&frame).unwrap();
        log.messages.lock().unwrap().push(message.clone());
        let service_ready = json!({
            "jsonrpc": "2.0", "method": "language/status",
            "params": { "type": "ServiceReady", "message": "ServiceReady" }
        });
        // Readiness follows the initial build: right after `initialized` when the mock reports
        // no problems, otherwise after the diagnostics of each opened document.
        let replies = match message["method"].as_str() {
            Some("initialize") => vec![json!({
                "jsonrpc": "2.0", "id": message["id"],
                "result": { "capabilities": { "textDocumentSync": 2 } }
            })],
            Some("initialized") if diagnostics.is_none() => vec![service_ready],
            Some("shutdown") => {
                vec![json!({ "jsonrpc": "2.0", "id": message["id"], "result": null })]
            }
            Some("exit") => break,
            Some("textDocument/didOpen") => match &diagnostics {
                Some(d) => vec![
                    json!({
                        "jsonrpc": "2.0",
                        "method": "textDocument/publishDiagnostics",
                        "params": {
                            "uri": message["params"]["textDocument"]["uri"],
                            "version": message["params"]["textDocument"]["version"],
                            "diagnostics": d,
                        }
                    }),
                    service_ready,
                ],
                None => Vec::new(),
            },
            _ => Vec::new(),
        };
        for reply in replies {
            let bytes = serde_json::to_vec(&reply).unwrap();
            if write_lsp_frame(&mut write, &bytes).await.is_err() {
                return;
            }
        }
    }
}

/// Launcher backed by in-memory mock servers.
#[derive(Debug)]
struct MockLauncher {
    pid_base: u32,
    launches: AtomicUsize,
    fail_next: Mutex<usize>,
    panic_next: Mutex<usize>,
    delay: Duration,
    diagnostics_on_open: Mutex<Option<Value>>,
    logs: Mutex<Vec<Arc<ServerLog>>>,
}

impl MockLauncher {
    fn new(pid_base: u32) -> Arc<Self> {
        Self::with_delay(pid_base, Duration::ZERO)
    }

    fn with_delay(pid_base: u32, delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            pid_base,
            launches: AtomicUsize::new(0),
            fail_next: Mutex::new(0),
            panic_next: Mutex::new(0),
            delay,
            diagnostics_on_open: Mutex::new(None),
            logs: Mutex::new(Vec::new()),
        })
    }

    fn launches(&self) -> usize {
        self.launches.load(Ordering::SeqCst)
    }

    fn log(&self, index: usize) -> Arc<ServerLog> {
        Arc::clone(&self.logs.lock().unwrap()[index])
    }

    fn pid_of_launch(&self, launch: usize) -> u32 {
        self.pid_base + u32::try_from(launch).unwrap()
    }

    fn set_diagnostics(&self, diagnostics: Option<Value>) {
        *self.diagnostics_on_open.lock().unwrap() = diagnostics;
    }
}

impl JdtlsLauncher for MockLauncher {
    fn launch<'a>(&'a self, project_root: &'a Path, _: &'a EffectiveConfig) -> LaunchFuture<'a> {
        Box::pin(async move {
            let launch = self.launches.fetch_add(1, Ordering::SeqCst) + 1;
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            let panic = {
                let mut remaining = self.panic_next.lock().unwrap();
                let panic = *remaining > 0;
                *remaining = remaining.saturating_sub(1);
                panic
            };
            assert!(!panic, "simulated launcher panic");
            let fail = {
                let mut remaining = self.fail_next.lock().unwrap();
                let fail = *remaining > 0;
                *remaining = remaining.saturating_sub(1);
                fail
            };
            if fail {
                return Err(LspDapError::SpawnFailed {
                    program: "mock-jdtls".to_owned(),
                    reason: "simulated launch failure".to_owned(),
                });
            }
            let (client_io, server_io) = tokio::io::duplex(1 << 16);
            let log = Arc::new(ServerLog::default());
            self.logs.lock().unwrap().push(Arc::clone(&log));
            let diagnostics = self.diagnostics_on_open.lock().unwrap().clone();
            tokio::spawn(serve(server_io, log, diagnostics));
            let (reader, writer) = tokio::io::split(client_io);
            let mut config = ClientRuntimeConfig::new(project_root);
            config.initialize_timeout = STEP;
            config.request_timeout = STEP;
            let client = JdtlsClient::start_with_streams(reader, writer, config).await?;
            Ok(LaunchedJdtls {
                client,
                pid: Some(self.pid_of_launch(launch)),
            })
        })
    }
}

/// Editor buffers of one project.
#[derive(Debug, Default)]
struct EditorBuffers {
    buffers: Mutex<Vec<(PathBuf, Buffer)>>,
}

impl EditorBuffers {
    fn open(&self, path: PathBuf, text: &str) {
        let mut buffers = self.buffers.lock().unwrap();
        let id = BufferId::new(u64::try_from(buffers.len()).unwrap() + 1);
        buffers.push((path, Buffer::with_id(id, text)));
    }

    fn close(&self, index: usize) {
        self.buffers.lock().unwrap().remove(index);
    }

    fn append(&self, index: usize, text: &str) {
        let mut buffers = self.buffers.lock().unwrap();
        let buffer = &mut buffers[index].1;
        let end = buffer.snapshot().len_bytes();
        buffer.apply_edit(end..end, text).unwrap();
    }
}

impl OpenBufferProvider for EditorBuffers {
    fn open_buffers(&self) -> Vec<OpenBuffer> {
        self.buffers
            .lock()
            .unwrap()
            .iter()
            .map(|(path, buffer)| OpenBuffer {
                path: path.clone(),
                language_id: "java".to_owned(),
                snapshot: buffer.snapshot(),
            })
            .collect()
    }
}

struct Fixture {
    _tmp: tempfile::TempDir,
    paths: KovaPaths,
    probe: Arc<MockProcessMemoryProbe>,
    manager: JdtlsSessionManager,
    root: PathBuf,
}

impl Fixture {
    fn new(config: SessionManagerConfig) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let paths = KovaPaths::from_roots(
            root.join("home/config"),
            root.join("home/data"),
            root.join("home/cache"),
        )
        .unwrap();
        let probe = Arc::new(MockProcessMemoryProbe::new());
        let manager = JdtlsSessionManager::new(paths.clone(), config, probe.clone());
        Self {
            _tmp: tmp,
            paths,
            probe,
            manager,
            root,
        }
    }

    fn project_dir(&self, name: &str) -> PathBuf {
        let dir = self.root.join("projects").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn register(
        &self,
        name: &str,
        effective: EffectiveConfig,
        launcher: &Arc<MockLauncher>,
    ) -> ProjectId {
        let launcher: Arc<dyn JdtlsLauncher> = launcher.clone();
        self.manager
            .register_project(self.project_dir(name), effective, launcher)
            .unwrap()
    }

    fn phase(&self, id: &ProjectId) -> ProjectSessionPhase {
        self.manager.phase(id).unwrap()
    }
}

fn effective(auto_hibernate_minutes: u32) -> EffectiveConfig {
    let mut effective = EffectiveConfig::merge(&KovaSettings::default(), None);
    effective.settings.jvm.auto_hibernate_minutes = auto_hibernate_minutes;
    effective
}

/// Lets spawned tasks progress without advancing the (paused) clock.
async fn settle_until(mut condition: impl FnMut() -> bool) {
    for _ in 0..10_000 {
        if condition() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("condition not reached");
}

fn drain(events: &mut broadcast::Receiver<SessionEvent>) -> Vec<SessionEvent> {
    let mut out = Vec::new();
    while let Ok(event) = events.try_recv() {
        out.push(event);
    }
    out
}

fn error_diagnostic(message: &str) -> Value {
    json!([{ "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 5 } },
             "severity": 1, "message": message }])
}

#[tokio::test(start_paused = true)]
async fn auto_hibernation_after_15_minutes() {
    let fx = Fixture::new(SessionManagerConfig::default());
    let launcher_a = MockLauncher::new(1000);
    let launcher_b = MockLauncher::new(2000);
    launcher_b.set_diagnostics(Some(error_diagnostic("cannot find symbol")));
    let proj_a = fx.register("proj_a", effective(15), &launcher_a);
    let proj_b = fx.register("proj_b", effective(15), &launcher_b);
    assert_eq!(
        fx.manager
            .project_config(&proj_b)
            .unwrap()
            .auto_hibernate_idle,
        Some(DEFAULT_JVM_HIBERNATION_IDLE)
    );

    let buffers_b = Arc::new(EditorBuffers::default());
    let source_b = fx.project_dir("proj_b").join("src/Main.java");
    buffers_b.open(source_b.clone(), "class Main { Strin x; }");
    fx.manager.set_project_focus(&proj_a, true).unwrap();
    fx.manager.ensure_ready(&proj_a, None).await.unwrap();
    // Held like an in-flight request would; keeps the mock transport alive to observe `exit`.
    let _client_b = fx
        .manager
        .ensure_ready(&proj_b, Some(buffers_b.clone()))
        .await
        .unwrap();
    let store_b = fx.manager.diagnostics(&proj_b).unwrap();
    settle_until(|| store_b.total_count() == 1).await;

    let mut events = fx.manager.subscribe_events();
    fx.manager
        .record_activity(&proj_a, ActivityKind::EditorEdit)
        .unwrap();
    fx.manager
        .record_activity(&proj_b, ActivityKind::FileOpen)
        .unwrap();
    let started = tokio::time::Instant::now();

    tokio::time::advance(DEFAULT_JVM_HIBERNATION_IDLE.saturating_sub(Duration::from_secs(1))).await;
    let report = fx.manager.run_watchdog_tick().await.unwrap();
    assert_eq!(report.idle_hibernated.len(), 0);
    assert!(fx.phase(&proj_b).is_ready());

    tokio::time::advance(Duration::from_secs(2)).await;
    let report = fx.manager.run_watchdog_tick().await.unwrap();
    assert_eq!(report.idle_hibernated, vec![proj_b.clone()]);
    assert!(started.elapsed() >= DEFAULT_JVM_HIBERNATION_IDLE);

    // proj_b hibernated: jdt.ls received shutdown + exit, diagnostics persisted outside the repo.
    match fx.phase(&proj_b) {
        ProjectSessionPhase::Hibernated {
            diagnostics_count, ..
        } => assert_eq!(diagnostics_count, 1),
        other => panic!("unexpected phase {other:?}"),
    }
    let log = launcher_b.log(0);
    settle_until(|| {
        log.methods()
            .ends_with(&["shutdown".to_owned(), "exit".to_owned()])
    })
    .await;
    let state_file = HibernationStore::snapshot_path(&fx.paths, &fx.project_dir("proj_b")).unwrap();
    assert!(state_file.starts_with(fx.paths.workspaces_cache_dir()));
    let saved = HibernationStore::load_hibernated_state(&fx.paths, &fx.project_dir("proj_b"))
        .unwrap()
        .unwrap();
    assert_eq!(saved.diagnostics.diagnostics_count(), 1);
    assert_eq!(saved.open_documents.len(), 1);
    assert_eq!(saved.open_documents[0].file_path, source_b);
    let project_entries: Vec<_> = std::fs::read_dir(fx.project_dir("proj_b"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(
        project_entries.len(),
        0,
        "project root polluted: {project_entries:?}"
    );

    // AC-2: diagnostics still served while hibernated.
    let uri = path_to_lsp_uri(&source_b).unwrap();
    let kept = store_b.diagnostics_for_uri(uri.as_str());
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].message, "cannot find symbol");
    assert_eq!(store_b.summary().total_errors, 1);

    // The focused project is untouched.
    assert!(fx.phase(&proj_a).is_ready());
    assert_eq!(
        launcher_a.log(0).methods(),
        vec!["initialize", "initialized"]
    );

    let events = drain(&mut events);
    assert!(events.contains(&SessionEvent::Hibernated {
        project_id: proj_b.clone(),
        diagnostics_saved: 1,
        rss_freed_bytes: None,
    }));
}

#[tokio::test(start_paused = true)]
async fn auto_hibernate_minutes_zero_disables_hibernation() {
    let fx = Fixture::new(SessionManagerConfig::default());
    let launcher = MockLauncher::new(1000);
    let id = fx.register("disabled", effective(0), &launcher);
    assert_eq!(
        fx.manager.project_config(&id).unwrap().auto_hibernate_idle,
        None
    );
    fx.manager.ensure_ready(&id, None).await.unwrap();

    tokio::time::advance(Duration::from_secs(60 * 60)).await;
    let report = fx.manager.run_watchdog_tick().await.unwrap();
    assert_eq!(report.idle_hibernated.len(), 0);
    assert!(fx.phase(&id).is_ready());
    assert_eq!(launcher.launches(), 1);
}

#[tokio::test(start_paused = true)]
async fn focused_project_is_kept_unless_configured() {
    let fx = Fixture::new(SessionManagerConfig {
        hibernate_focused_project: true,
        ..SessionManagerConfig::default()
    });
    let launcher = MockLauncher::new(1000);
    let id = fx.register("focused", effective(15), &launcher);
    fx.manager.set_project_focus(&id, true).unwrap();
    fx.manager.ensure_ready(&id, None).await.unwrap();
    tokio::time::advance(DEFAULT_JVM_HIBERNATION_IDLE + Duration::from_secs(1)).await;
    let report = fx.manager.run_watchdog_tick().await.unwrap();
    assert_eq!(report.idle_hibernated, vec![id.clone()]);

    let fx = Fixture::new(SessionManagerConfig::default());
    let id = fx.register("focused", effective(15), &launcher);
    fx.manager.set_project_focus(&id, true).unwrap();
    fx.manager.ensure_ready(&id, None).await.unwrap();
    tokio::time::advance(DEFAULT_JVM_HIBERNATION_IDLE * 4).await;
    fx.manager.run_watchdog_tick().await.unwrap();
    assert!(fx.phase(&id).is_ready());
}

#[tokio::test(start_paused = true)]
async fn wake_coalescing_replays_buffers_and_refreshes_diagnostics() {
    let fx = Fixture::new(SessionManagerConfig::default());
    let launcher = MockLauncher::new(2000);
    launcher.set_diagnostics(Some(error_diagnostic("old error")));
    let id = fx.register("proj_b", effective(15), &launcher);
    let buffers = Arc::new(EditorBuffers::default());
    let source = fx.project_dir("proj_b").join("Main.java");
    buffers.open(source.clone(), "class Main {");
    fx.manager
        .set_buffer_provider(&id, Some(buffers.clone()))
        .unwrap();
    fx.manager.ensure_ready(&id, None).await.unwrap();
    let store = fx.manager.diagnostics(&id).unwrap();
    let uri = path_to_lsp_uri(&source).unwrap();
    settle_until(|| store.total_count() == 1).await;

    fx.manager.hibernate_project(&id).await.unwrap();
    assert!(fx.phase(&id).is_hibernated());
    assert_eq!(
        store.diagnostics_for_uri(uri.as_str())[0].message,
        "old error"
    );

    // Edit made while jdt.ls sleeps; the new server reports a fresh diagnostic.
    buffers.append(0, " }");
    launcher.set_diagnostics(Some(error_diagnostic("fresh error")));
    let mut events = fx.manager.subscribe_events();

    let mut tasks = Vec::new();
    for _ in 0..10 {
        let manager = fx.manager.clone();
        let id = id.clone();
        tasks.push(tokio::spawn(async move {
            manager.ensure_ready(&id, None).await
        }));
    }
    let mut clients = Vec::new();
    for task in tasks {
        clients.push(task.await.unwrap().unwrap());
    }
    assert_eq!(launcher.launches(), 2, "exactly one wake launch");
    assert!(clients.iter().all(|c| Arc::ptr_eq(c, &clients[0])));
    assert!(fx.phase(&id).is_ready());

    assert_eq!(
        launcher.log(1).did_open_texts(),
        vec!["class Main { }".to_owned()]
    );
    settle_until(|| store.diagnostics_for_uri(uri.as_str())[0].message == "fresh error").await;
    assert_eq!(store.total_count(), 1);
    // Cleared once the woken server reports readiness.
    settle_until(|| {
        HibernationStore::load_hibernated_state(&fx.paths, &fx.project_dir("proj_b"))
            .unwrap()
            .is_none()
    })
    .await;
    let events = drain(&mut events);
    assert!(events.contains(&SessionEvent::Woken {
        project_id: id.clone(),
        restored_documents: 1,
    }));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(
                e,
                SessionEvent::PhaseChanged {
                    current: ProjectSessionPhase::Waking,
                    ..
                }
            ))
            .count(),
        1
    );
}

#[tokio::test(start_paused = true)]
async fn focus_and_edit_wake_hibernated_project_in_background() {
    let fx = Fixture::new(SessionManagerConfig::default());
    let launcher = MockLauncher::new(2000);
    let id = fx.register("proj", effective(15), &launcher);
    let buffers = Arc::new(EditorBuffers::default());
    buffers.open(fx.project_dir("proj").join("A.java"), "class A {}");
    fx.manager
        .set_buffer_provider(&id, Some(buffers.clone()))
        .unwrap();
    fx.manager.ensure_ready(&id, None).await.unwrap();
    fx.manager.hibernate_project(&id).await.unwrap();

    buffers.append(0, " // edited");
    fx.manager.set_project_focus(&id, true).unwrap();
    settle_until(|| fx.phase(&id).is_ready()).await;
    assert_eq!(launcher.launches(), 2);
    assert_eq!(
        launcher.log(1).did_open_texts(),
        vec!["class A {} // edited".to_owned()]
    );

    fx.manager.set_project_focus(&id, false).unwrap();
    fx.manager.hibernate_project(&id).await.unwrap();
    fx.manager
        .record_activity(&id, ActivityKind::FocusGained)
        .unwrap();
    tokio::task::yield_now().await;
    assert!(
        fx.phase(&id).is_hibernated(),
        "focus activity alone does not wake"
    );
    fx.manager
        .record_activity(&id, ActivityKind::EditorEdit)
        .unwrap();
    settle_until(|| fx.phase(&id).is_ready()).await;
    assert_eq!(launcher.launches(), 3);
}

#[tokio::test(start_paused = true)]
async fn failed_wake_is_delivered_to_all_waiters_and_retried() {
    let fx = Fixture::new(SessionManagerConfig::default());
    let launcher = MockLauncher::new(1000);
    let id = fx.register("proj", effective(15), &launcher);
    *launcher.fail_next.lock().unwrap() = 1;

    let mut tasks = Vec::new();
    for _ in 0..5 {
        let manager = fx.manager.clone();
        let id = id.clone();
        tasks.push(tokio::spawn(async move {
            manager.ensure_ready(&id, None).await
        }));
    }
    for task in tasks {
        assert!(matches!(
            task.await.unwrap(),
            Err(LspDapError::SpawnFailed { .. })
        ));
    }
    assert_eq!(launcher.launches(), 1);
    assert!(matches!(fx.phase(&id), ProjectSessionPhase::Failed(_)));

    fx.manager.ensure_ready(&id, None).await.unwrap();
    assert_eq!(launcher.launches(), 2);
    assert!(fx.phase(&id).is_ready());
}

#[tokio::test(start_paused = true)]
async fn rss_budget_hibernates_background_and_restarts_focused() {
    let fx = Fixture::new(SessionManagerConfig::default());
    let launcher_a = MockLauncher::new(1000);
    let launcher_b = MockLauncher::new(2000);
    let proj_a = fx.register("proj_a", effective(15), &launcher_a);
    let proj_b = fx.register("proj_b", effective(15), &launcher_b);
    fx.manager.set_project_focus(&proj_a, true).unwrap();
    fx.manager.ensure_ready(&proj_a, None).await.unwrap();
    fx.manager.ensure_ready(&proj_b, None).await.unwrap();
    let mut events = fx.manager.subscribe_events();

    // jdtls_max_ram_mb = 2048: soft 2.5 GiB, hard 3 GiB.
    fx.probe
        .set_rss(launcher_a.pid_of_launch(1), 2 * GIB + GIB / 2 + 1);
    fx.probe.set_rss(launcher_b.pid_of_launch(1), 4 * GIB);
    let report = fx.manager.run_watchdog_tick().await.unwrap();
    assert_eq!(report.samples.len(), 2);
    assert_eq!(
        report.memory_actions,
        vec![(proj_b.clone(), "hibernated".to_owned())]
    );
    assert!(fx.phase(&proj_b).is_hibernated());
    assert!(fx.phase(&proj_a).is_ready());
    let events = drain(&mut events);
    assert!(events.contains(&SessionEvent::MemorySampled {
        project_id: proj_a.clone(),
        pid: launcher_a.pid_of_launch(1),
        rss_bytes: 2 * GIB + GIB / 2 + 1,
        pressure: MemoryPressureLevel::Warning,
    }));
    assert!(events.contains(&SessionEvent::MemoryLimitAction {
        project_id: proj_b.clone(),
        rss_bytes: 4 * GIB,
        action_taken: "hibernated".to_owned(),
    }));
    assert!(events.contains(&SessionEvent::Hibernated {
        project_id: proj_b.clone(),
        diagnostics_saved: 0,
        rss_freed_bytes: Some(4 * GIB),
    }));

    // The focused project over the hard limit is restarted with a fresh process.
    fx.probe.set_rss(launcher_a.pid_of_launch(1), 5 * GIB);
    let report = fx.manager.run_watchdog_tick().await.unwrap();
    assert_eq!(
        report.memory_actions,
        vec![(proj_a.clone(), "restarted".to_owned())]
    );
    settle_until(|| fx.phase(&proj_a).is_ready()).await;
    assert_eq!(launcher_a.launches(), 2);
    assert_eq!(
        fx.phase(&proj_a),
        ProjectSessionPhase::Ready {
            pid: Some(launcher_a.pid_of_launch(2)),
            last_rss_bytes: None
        }
    );
}

#[tokio::test(start_paused = true)]
async fn max_active_jvms_hibernates_least_recently_used_background_project() {
    let fx = Fixture::new(SessionManagerConfig {
        max_active_jvms: Some(2),
        ..SessionManagerConfig::default()
    });
    let launchers = [
        MockLauncher::new(1000),
        MockLauncher::new(2000),
        MockLauncher::new(3000),
    ];
    let proj_a = fx.register("proj_a", effective(0), &launchers[0]);
    let proj_b = fx.register("proj_b", effective(0), &launchers[1]);
    let proj_c = fx.register("proj_c", effective(0), &launchers[2]);
    fx.manager.set_project_focus(&proj_a, true).unwrap();
    let mut events = fx.manager.subscribe_events();

    fx.manager.ensure_ready(&proj_b, None).await.unwrap();
    tokio::time::advance(Duration::from_secs(1)).await;
    fx.manager.ensure_ready(&proj_a, None).await.unwrap();
    tokio::time::advance(Duration::from_secs(1)).await;
    fx.manager.ensure_ready(&proj_c, None).await.unwrap();

    // The third JVM pushes the oldest background project (proj_b) into hibernation.
    settle_until(|| fx.phase(&proj_b).is_hibernated()).await;
    assert!(fx.phase(&proj_a).is_ready());
    assert!(fx.phase(&proj_c).is_ready());
    let events = drain(&mut events);
    assert!(events.contains(&SessionEvent::MemoryLimitAction {
        project_id: proj_b.clone(),
        rss_bytes: 0,
        action_taken: "lru_hibernated".to_owned(),
    }));

    // A watchdog tick keeps the limit satisfied without touching the remaining sessions.
    let report = fx.manager.run_watchdog_tick().await.unwrap();
    assert_eq!(report.memory_actions.len(), 0);
}

#[tokio::test(start_paused = true)]
async fn unregister_during_wake_does_not_leak_the_jvm() {
    let fx = Fixture::new(SessionManagerConfig::default());
    let launcher = MockLauncher::with_delay(1000, Duration::from_secs(10));
    let id = fx.register("proj", effective(15), &launcher);
    let manager = fx.manager.clone();
    let pending = {
        let id = id.clone();
        tokio::spawn(async move { manager.ensure_ready(&id, None).await })
    };
    settle_until(|| fx.phase(&id) == ProjectSessionPhase::Starting).await;

    fx.manager.unregister_project(&id).await.unwrap();
    assert!(matches!(
        pending.await.unwrap(),
        Err(LspDapError::SessionShutDown { .. })
    ));
    assert_eq!(launcher.launches(), 1);
    // The discarded client is dropped right after `shutdown`, so only `shutdown` is guaranteed
    // to reach the in-memory transport (a real process is awaited until it exits).
    let log = launcher.log(0);
    settle_until(|| log.methods().contains(&"shutdown".to_owned())).await;
    assert!(matches!(
        fx.manager.phase(&id),
        Err(LspDapError::UnknownProject { .. })
    ));
}

#[tokio::test(start_paused = true)]
async fn register_restores_diagnostics_of_previous_hibernation_and_shutdown_all_stops_servers() {
    let fx = Fixture::new(SessionManagerConfig::default());
    let launcher = MockLauncher::new(1000);
    launcher.set_diagnostics(Some(error_diagnostic("persisted")));
    let id = fx.register("proj", effective(15), &launcher);
    let buffers = Arc::new(EditorBuffers::default());
    buffers.open(fx.project_dir("proj").join("A.java"), "class A {");
    fx.manager.ensure_ready(&id, Some(buffers)).await.unwrap();
    let store = fx.manager.diagnostics(&id).unwrap();
    settle_until(|| store.total_count() == 1).await;
    fx.manager.hibernate_project(&id).await.unwrap();

    // A new IDE session: diagnostics are back before jdt.ls starts.
    let manager = JdtlsSessionManager::new(
        fx.paths.clone(),
        SessionManagerConfig::default(),
        fx.probe.clone(),
    );
    let fresh = MockLauncher::new(5000);
    let launcher_dyn: Arc<dyn JdtlsLauncher> = fresh.clone();
    let again = manager
        .register_project(fx.project_dir("proj"), effective(15), launcher_dyn)
        .unwrap();
    assert_eq!(again, id);
    assert_eq!(manager.phase(&again).unwrap(), ProjectSessionPhase::Cold);
    assert_eq!(
        manager.diagnostics(&again).unwrap().summary().total_errors,
        1
    );

    let _client = manager.ensure_ready(&again, None).await.unwrap();
    manager.shutdown_all().await;
    let log = fresh.log(0);
    settle_until(|| {
        log.methods()
            .ends_with(&["shutdown".to_owned(), "exit".to_owned()])
    })
    .await;
    assert_eq!(manager.project_ids().len(), 0);
}

#[tokio::test(start_paused = true)]
async fn restored_diagnostics_not_republished_after_wake_are_dropped() {
    let fx = Fixture::new(SessionManagerConfig::default());
    let launcher = MockLauncher::new(1000);
    launcher.set_diagnostics(Some(error_diagnostic("broken")));
    let id = fx.register("proj", effective(15), &launcher);
    let buffers = Arc::new(EditorBuffers::default());
    let file_a = fx.project_dir("proj").join("A.java");
    let file_b = fx.project_dir("proj").join("B.java");
    buffers.open(file_a.clone(), "class A {");
    buffers.open(file_b.clone(), "class B {");
    fx.manager
        .ensure_ready(&id, Some(buffers.clone()))
        .await
        .unwrap();
    let store = fx.manager.diagnostics(&id).unwrap();
    settle_until(|| store.total_count() == 2).await;
    fx.manager.hibernate_project(&id).await.unwrap();
    assert_eq!(store.total_count(), 2);

    // B.java was fixed and closed while jdt.ls slept: the new server only reports A.java.
    buffers.close(1);
    fx.manager.ensure_ready(&id, None).await.unwrap();
    let uri_a = path_to_lsp_uri(&file_a).unwrap();
    let uri_b = path_to_lsp_uri(&file_b).unwrap();
    settle_until(|| store.diagnostics_for_uri(uri_b.as_str()).is_empty()).await;
    assert_eq!(store.diagnostics_for_uri(uri_a.as_str()).len(), 1);
    assert_eq!(store.uris(), vec![uri_a.as_str().to_owned()]);
}

#[tokio::test(start_paused = true)]
async fn panicking_wake_task_fails_waiters_and_next_call_relaunches() {
    let fx = Fixture::new(SessionManagerConfig::default());
    let launcher = MockLauncher::new(1000);
    let id = fx.register("proj", effective(15), &launcher);
    *launcher.panic_next.lock().unwrap() = 1;

    let mut tasks = Vec::new();
    for _ in 0..3 {
        let manager = fx.manager.clone();
        let id = id.clone();
        tasks.push(tokio::spawn(async move {
            manager.ensure_ready(&id, None).await
        }));
    }
    for task in tasks {
        assert!(matches!(
            task.await.unwrap(),
            Err(LspDapError::SpawnFailed { .. })
        ));
    }
    assert!(matches!(fx.phase(&id), ProjectSessionPhase::Failed(_)));

    fx.manager.ensure_ready(&id, None).await.unwrap();
    assert_eq!(launcher.launches(), 2);
    assert!(fx.phase(&id).is_ready());
}

#[tokio::test(start_paused = true)]
async fn crashed_server_is_relaunched_by_ensure_ready() {
    let fx = Fixture::new(SessionManagerConfig::default());
    let launcher = MockLauncher::new(1000);
    let id = fx.register("proj", effective(15), &launcher);
    let first = fx.manager.ensure_ready(&id, None).await.unwrap();

    // Simulates a crash: the server process is gone and the client ends `Stopped`.
    first.shutdown_graceful(STEP).await.unwrap();
    let second = fx.manager.ensure_ready(&id, None).await.unwrap();
    assert!(!Arc::ptr_eq(&first, &second));
    assert_eq!(launcher.launches(), 2);
    assert_eq!(
        fx.phase(&id),
        ProjectSessionPhase::Ready {
            pid: Some(launcher.pid_of_launch(2)),
            last_rss_bytes: None
        }
    );
}
