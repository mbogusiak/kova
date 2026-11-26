//! Host side of *Go to Definition* (`KOVA-051`):
//!
//! - **Level 1 (`< 15 ms`):** instant workspace & open-buffer symbol resolution via
//!   `kova_core::navigation::resolve_local_definition` for Java, Kotlin, and Rust.
//! - **Level 2 (`jdt.ls` + Vineflower):** when a Java symbol is not in the workspace (e.g. JDK
//!   or external `.jar` dependency) and a ready [`JdtlsClient`] is available, delegates to
//!   [`DefinitionResolver::go_to_definition`] and opens external sources or Vineflower-decompiled
//!   classes in read-only tabs.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kova_buffer::{Buffer, Utf16Point};
use kova_config::{EffectiveConfig, JvmSettings, KovaPaths, KovaSettings};
use kova_core::navigation::{SourceLanguageKind, extract_symbol_query, resolve_local_definition};
use kova_lsp_dap::lsp::{
    DefinitionResolver, DefinitionTarget, JdtlsClient, LspLifecycleState, path_to_lsp_uri,
};
use kova_lsp_dap::manager::{JdtlsSessionManager, OsProcessMemoryProbe, SessionManagerConfig};
use kova_toolchain::VineflowerDecompiler;
use kova_ui::{DefinitionOutcome, DefinitionRequest, DefinitionSink, DefinitionTargetKind};
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use crate::AppEvent;

/// Timeout applied to Level-2 `textDocument/definition` queries so UI navigation stays snappy.
const LSP_DEFINITION_TIMEOUT: Duration = Duration::from_secs(3);

/// Optional Level-2 LSP + Vineflower backend maintained by the host.
#[derive(Clone, Default)]
pub struct LspDefinitionBackend {
    inner: Arc<Mutex<Option<ActiveLspBackend>>>,
}

impl std::fmt::Debug for LspDefinitionBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspDefinitionBackend")
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
struct ActiveLspBackend {
    client: Arc<JdtlsClient>,
    resolver: DefinitionResolver,
}

impl LspDefinitionBackend {
    /// Creates an empty backend with no active `jdt.ls` client yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Attaches an active [`JdtlsClient`] and [`DefinitionResolver`] for Level-2 queries.
    pub fn attach_client(&self, client: Arc<JdtlsClient>, resolver: DefinitionResolver) {
        *self.inner.lock().unwrap_or_else(PoisonError::into_inner) =
            Some(ActiveLspBackend { client, resolver });
    }

    fn current(&self) -> Option<ActiveLspBackend> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Builds a [`DefinitionResolver`] from default `KovaPaths` and `[jvm]` settings when a
/// `VineflowerDecompiler` is available, or a cache-only resolver otherwise.
#[must_use]
pub fn default_definition_resolver(
    paths: &KovaPaths,
    settings: &JvmSettings,
    decompiler: Option<VineflowerDecompiler>,
) -> DefinitionResolver {
    match decompiler {
        Some(decompiler) => DefinitionResolver::from_settings(settings, decompiler),
        None => DefinitionResolver::new(
            paths.cache_dir().join("decompiled"),
            settings.auto_decompile_class,
        ),
    }
}

/// Creates a [`JdtlsSessionManager`] configured with [`KovaPaths`] for optional background
/// Java language-server management.
#[must_use]
pub fn build_session_manager(paths: KovaPaths) -> JdtlsSessionManager {
    JdtlsSessionManager::new(
        paths,
        SessionManagerConfig::default(),
        Arc::new(OsProcessMemoryProbe),
    )
}

/// Registers `project_root` in `manager` using default `EffectiveConfig` when `jdt.ls` launch
/// configuration is constructed.
#[must_use]
pub fn default_effective_config() -> EffectiveConfig {
    EffectiveConfig::merge(&KovaSettings::default(), None)
}

/// Creates the [`DefinitionSink`] installed on [`kova_ui::WorkspaceShell`].
#[must_use]
pub fn definition_sink(runtime: Handle, events: mpsc::UnboundedSender<AppEvent>) -> DefinitionSink {
    definition_sink_with_backend(runtime, events, LspDefinitionBackend::new())
}

/// Creates a [`DefinitionSink`] backed by an explicit [`LspDefinitionBackend`].
#[must_use]
pub fn definition_sink_with_backend(
    runtime: Handle,
    events: mpsc::UnboundedSender<AppEvent>,
    backend: LspDefinitionBackend,
) -> DefinitionSink {
    Arc::new(move |request: DefinitionRequest| {
        let events = events.clone();
        let backend = backend.clone();
        runtime.spawn(async move {
            let request_id = request.request_id;
            let outcome = resolve_definition_request(request, Some(&backend)).await;
            drop(events.send(AppEvent::DefinitionResolved {
                request_id,
                outcome,
            }));
        });
    })
}

/// Resolves one [`DefinitionRequest`] using Level 1 first, then optional Level 2 (`jdt.ls` +
/// Vineflower) for Java symbols not found in the workspace.
pub async fn resolve_definition_request(
    request: DefinitionRequest,
    backend: Option<&LspDefinitionBackend>,
) -> DefinitionOutcome {
    let source_path = request.source_path.clone();
    let source_text = request.source_text.clone();
    let cursor_byte = request.cursor_byte;
    let project_root = request.project_root.clone();
    let open_buffers = request.open_buffers.clone();
    let workspace_files = request.workspace_files.clone();
    let language = SourceLanguageKind::from_path(&source_path);

    let level1 = tokio::task::spawn_blocking(move || {
        let buffer = Buffer::from_text(&source_text);
        let snapshot = buffer.snapshot();
        let query = extract_symbol_query(&snapshot, &source_path, cursor_byte)?;
        let symbol_name = query.ident.clone();
        let resolved = resolve_local_definition(
            &query,
            &source_text,
            &open_buffers,
            project_root.as_deref(),
            &workspace_files,
        );
        Some((symbol_name, snapshot, resolved))
    })
    .await;

    let Ok(Some((symbol_name, snapshot, resolved))) = level1 else {
        return DefinitionOutcome::NoSymbolAtCursor;
    };

    if let Some(hit) = resolved {
        return DefinitionOutcome::Found {
            target: DefinitionTargetKind::WorkspaceFile { rel_path: hit.path },
            line: hit.line,
            column: hit.column,
            symbol_name: hit.symbol_name,
        };
    }

    // Level 2: query jdt.ls + Vineflower for Java symbols outside workspace (e.g. JDK / .jar).
    if language == SourceLanguageKind::Java
        && let Some(active) = backend.and_then(LspDefinitionBackend::current)
        && matches!(
            active.client.state(),
            LspLifecycleState::Ready | LspLifecycleState::IndexingProject
        )
    {
        let abs_source = match (&request.project_root, &request.source_path) {
            (Some(root), rel) if rel.is_relative() => root.join(rel),
            (_, path) => path.clone(),
        };
        if let Ok(uri) = path_to_lsp_uri(&abs_source) {
            let pos = utf16_point_at_byte(&snapshot, cursor_byte);
            let lsp_result = tokio::time::timeout(
                LSP_DEFINITION_TIMEOUT,
                active
                    .resolver
                    .go_to_definition(&active.client, uri.as_str(), pos),
            )
            .await;
            if let Ok(Ok(mut targets)) = lsp_result
                && let Some(target) = targets.drain(..).next()
            {
                return map_definition_target(target, request.project_root.as_deref(), symbol_name);
            }
        }
    }

    DefinitionOutcome::NotFound { symbol_name }
}

/// Converts a `kova_lsp_dap` [`DefinitionTarget`] into a UI-ready [`DefinitionOutcome`].
#[must_use]
pub fn map_definition_target(
    target: DefinitionTarget,
    project_root: Option<&Path>,
    symbol_name: String,
) -> DefinitionOutcome {
    match target {
        DefinitionTarget::SourceFile {
            path,
            range,
            read_only,
        } => {
            let (line, column) = if let Ok(text) = std::fs::read_to_string(&path) {
                utf16_point_to_line_col(&text, range.start)
            } else {
                (range.start.row + 1, range.start.column + 1)
            };
            let target_kind = if !read_only
                && let Some(root) = project_root
                && let Ok(rel) = path.strip_prefix(root)
            {
                DefinitionTargetKind::WorkspaceFile {
                    rel_path: rel.to_path_buf(),
                }
            } else {
                DefinitionTargetKind::ExternalFile { path, read_only }
            };
            DefinitionOutcome::Found {
                target: target_kind,
                line,
                column,
                symbol_name,
            }
        }
        DefinitionTarget::DecompiledClass {
            fqcn,
            cache_file,
            source_text,
            range,
            ..
        } => {
            let (line, column) = utf16_point_to_line_col(&source_text, range.start);
            DefinitionOutcome::Found {
                target: DefinitionTargetKind::DecompiledClass {
                    fqcn,
                    cache_file,
                    source_text,
                },
                line,
                column,
                symbol_name,
            }
        }
    }
}

pub(crate) fn utf16_point_at_byte(
    snapshot: &kova_buffer::BufferSnapshot,
    byte: usize,
) -> Utf16Point {
    let clamped = snapshot.clamp_byte_to_char_boundary(byte.min(snapshot.len_bytes()));
    let Ok(pt) = snapshot.byte_to_point(clamped) else {
        return Utf16Point::new(0, 0);
    };
    let line_start = snapshot.rope().line_to_byte(pt.row);
    let col_utf16 = snapshot
        .slice_bytes(line_start..clamped)
        .map_or(0, |s| s.chunks().map(|c| c.encode_utf16().count()).sum());
    Utf16Point::new(pt.row, col_utf16)
}

fn utf16_point_to_line_col(text: &str, point: Utf16Point) -> (usize, usize) {
    let line = text.lines().nth(point.row).unwrap_or("");
    let mut utf16_seen = 0usize;
    let mut chars_seen = 0usize;
    for ch in line.chars() {
        if utf16_seen >= point.column {
            break;
        }
        utf16_seen += ch.len_utf16();
        chars_seen += 1;
    }
    (point.row + 1, chars_seen + 1)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use kova_buffer::Utf16Range;
    use kova_lsp_dap::lsp::{
        ClientRuntimeConfig, IncomingMessage, JsonRpcResponse, LspEvent, read_lsp_frame,
        write_lsp_frame,
    };
    use serde_json::json;
    use tokio::io::{AsyncWriteExt, BufReader};
    use tokio::sync::mpsc;

    use super::*;
    use crate::build_io_runtime;

    #[test]
    fn definition_sink_resolves_cross_file_java_import_and_same_file_symbol() {
        let runtime = build_io_runtime().expect("runtime");
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        let app_rel = PathBuf::from("src/main/java/dev/kova/demo/OrderApp.java");
        let svc_rel = PathBuf::from("src/main/java/dev/kova/orders/OrderService.java");
        std::fs::create_dir_all(root.join(app_rel.parent().unwrap())).unwrap();
        std::fs::create_dir_all(root.join(svc_rel.parent().unwrap())).unwrap();

        let app_src = "\
package dev.kova.demo;

import dev.kova.orders.OrderService;

public class OrderApp {
    public void run() {
        OrderService svc = new OrderService();
        helper();
    }
    private void helper() {}
}
";
        let svc_src = "\
package dev.kova.orders;

public class OrderService {
    public OrderService() {}
}
";
        std::fs::write(root.join(&app_rel), app_src).unwrap();
        std::fs::write(root.join(&svc_rel), svc_src).unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = definition_sink(runtime.handle().clone(), tx);

        // 1. Cross-file import jump
        let byte_import_target = app_src.find("OrderService svc").unwrap();
        sink(DefinitionRequest {
            request_id: 1,
            project_root: Some(root.to_path_buf()),
            source_path: app_rel.clone(),
            source_text: app_src.to_owned(),
            cursor_byte: byte_import_target,
            line: 7,
            column: 9,
            open_buffers: vec![(app_rel.clone(), app_src.to_owned())],
            workspace_files: vec![app_rel.clone(), svc_rel.clone()],
        });

        let event1 = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("timeout")
                .expect("event")
        });
        match event1 {
            AppEvent::DefinitionResolved {
                request_id: 1,
                outcome:
                    DefinitionOutcome::Found {
                        target: DefinitionTargetKind::WorkspaceFile { rel_path },
                        line,
                        column: _,
                        symbol_name,
                    },
            } => {
                assert_eq!(rel_path, svc_rel);
                assert_eq!(line, 3);
                assert_eq!(symbol_name, "OrderService");
            }
            other => panic!("unexpected event1: {other:?}"),
        }

        // 2. Same-file method jump
        let byte_helper = app_src.find("helper();").unwrap();
        sink(DefinitionRequest {
            request_id: 2,
            project_root: Some(root.to_path_buf()),
            source_path: app_rel.clone(),
            source_text: app_src.to_owned(),
            cursor_byte: byte_helper,
            line: 8,
            column: 9,
            open_buffers: vec![(app_rel.clone(), app_src.to_owned())],
            workspace_files: vec![app_rel.clone(), svc_rel],
        });
        let event2 = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("timeout")
                .expect("event")
        });
        match event2 {
            AppEvent::DefinitionResolved {
                request_id: 2,
                outcome:
                    DefinitionOutcome::Found {
                        target: DefinitionTargetKind::WorkspaceFile { rel_path },
                        line,
                        column: _,
                        symbol_name,
                    },
            } => {
                assert_eq!(rel_path, app_rel);
                assert_eq!(line, 10);
                assert_eq!(symbol_name, "helper");
            }
            other => panic!("unexpected event2: {other:?}"),
        }
    }

    fn spawn_mock_jdtls_server(
        server_read: tokio::io::DuplexStream,
        mut writer: tokio::io::DuplexStream,
        external_uri: String,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut reader = BufReader::new(server_read);
            while let Ok(Some(frame)) = read_lsp_frame(&mut reader).await {
                let Ok(IncomingMessage::Request(req)) = IncomingMessage::from_slice(&frame) else {
                    continue;
                };
                let resp = match req.method.as_str() {
                    "initialize" => JsonRpcResponse::success(
                        req.id,
                        json!({ "capabilities": { "definitionProvider": true } }),
                    ),
                    "textDocument/definition" => JsonRpcResponse::success(
                        req.id,
                        json!([{
                            "uri": external_uri,
                            "range": {
                                "start": { "line": 1, "character": 19 },
                                "end": { "line": 1, "character": 25 }
                            }
                        }]),
                    ),
                    _ => JsonRpcResponse::success(req.id, json!(null)),
                };
                let payload = serde_json::to_vec(&resp).unwrap();
                write_lsp_frame(&mut writer, &payload).await.unwrap();
                if req.method == "initialize" {
                    let ready = serde_json::to_vec(&json!({
                        "jsonrpc": "2.0",
                        "method": "language/status",
                        "params": { "type": "ServiceReady", "message": "Ready" }
                    }))
                    .unwrap();
                    write_lsp_frame(&mut writer, &ready).await.unwrap();
                }
            }
            drop(writer.shutdown().await);
        })
    }

    #[test]
    fn decompiled_target_maps_to_read_only_outcome() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let cache_root = root.join(".cache");
        let decompiled_cache = cache_root.join("java/lang/String.java");
        let decompiled_src = "package java.lang;\npublic final class String {\n}\n";
        let mapped = map_definition_target(
            DefinitionTarget::DecompiledClass {
                fqcn: "java.lang.String".to_owned(),
                archive_path: None,
                cache_file: decompiled_cache.clone(),
                source_text: decompiled_src.to_owned(),
                range: Utf16Range::new(Utf16Point::new(1, 19), Utf16Point::new(1, 25)),
                from_cache: false,
                read_only: true,
            },
            Some(&root),
            "String".to_owned(),
        );
        assert_eq!(
            mapped,
            DefinitionOutcome::Found {
                target: DefinitionTargetKind::DecompiledClass {
                    fqcn: "java.lang.String".to_owned(),
                    cache_file: decompiled_cache,
                    source_text: decompiled_src.to_owned(),
                },
                line: 2,
                column: 20,
                symbol_name: "String".to_owned(),
            }
        );
    }

    #[test]
    fn level2_lsp_roundtrip_resolves_external_definition() {
        let runtime = build_io_runtime().expect("runtime");
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let cache_root = root.join(".cache");
        let app_rel = PathBuf::from("src/App.java");
        let app_abs = root.join(&app_rel);
        std::fs::create_dir_all(app_abs.parent().unwrap()).unwrap();
        let app_src = "public class App { String name; }\n";
        std::fs::write(&app_abs, app_src).unwrap();

        let decompiled_src = "package java.lang;\npublic final class String {\n}\n";
        let external_dir = tempfile::tempdir().expect("external tempdir");
        let external_jdk_src = external_dir.path().join("java/lang/String.java");
        std::fs::create_dir_all(external_jdk_src.parent().unwrap()).unwrap();
        std::fs::write(&external_jdk_src, decompiled_src).unwrap();
        let external_uri = path_to_lsp_uri(&external_jdk_src).unwrap().to_string();

        runtime.block_on(async {
            let (client_read, server_write) = tokio::io::duplex(64 * 1024);
            let (server_read, client_write) = tokio::io::duplex(64 * 1024);
            let server_task = spawn_mock_jdtls_server(server_read, server_write, external_uri);

            let client = Arc::new(
                JdtlsClient::start_with_streams(
                    client_read,
                    client_write,
                    ClientRuntimeConfig::new(&root),
                )
                .await
                .expect("client start"),
            );
            let mut events = client.subscribe_events();
            while let Ok(ev) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await {
                if matches!(
                    ev,
                    Ok(LspEvent::StateChanged {
                        current: LspLifecycleState::Ready,
                        ..
                    })
                ) {
                    break;
                }
            }

            let backend = LspDefinitionBackend::new();
            backend.attach_client(
                Arc::clone(&client),
                DefinitionResolver::new(&cache_root, true),
            );

            let (tx, mut rx) = mpsc::unbounded_channel();
            let sink = definition_sink_with_backend(Handle::current(), tx, backend);
            sink(DefinitionRequest {
                request_id: 7,
                project_root: Some(root.clone()),
                source_path: app_rel,
                source_text: app_src.to_owned(),
                cursor_byte: app_src.find("String").unwrap(),
                line: 1,
                column: 20,
                open_buffers: vec![],
                workspace_files: vec![],
            });

            let event = tokio::time::timeout(Duration::from_secs(3), rx.recv())
                .await
                .expect("sink timeout")
                .expect("event");
            match event {
                AppEvent::DefinitionResolved {
                    request_id: 7,
                    outcome:
                        DefinitionOutcome::Found {
                            target: DefinitionTargetKind::ExternalFile { path, .. },
                            line,
                            column,
                            symbol_name,
                        },
                } => {
                    assert_eq!(path, external_jdk_src);
                    assert_eq!(line, 2);
                    assert_eq!(column, 20);
                    assert_eq!(symbol_name, "String");
                }
                other => panic!("unexpected LSP outcome: {other:?}"),
            }

            drop(client.shutdown_graceful(Duration::from_secs(1)).await);
            server_task.abort();
        });
    }
}
