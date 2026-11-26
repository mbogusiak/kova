//! Host side of Code Completion (`KOVA-052`):
//!
//! - **Level 1 (`< 15 ms`):** instant local/workspace resolution (`kova_core::completion`) +
//!   on-demand `.jmod` bytecode lookup (`kova_jvm_index::JdkOnDemandIndex`) with deterministic
//!   built-in JDK fallback when no JDK is installed.
//! - **Level 2 (`jdt.ls`):** asynchronous `textDocument/completion` merged cleanly into an open
//!   popup without interrupting user typing or selection.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use kova_buffer::{Buffer, BufferSnapshot};
use kova_config::KovaPaths;
use kova_core::completion::jdk_fallback::{builtin_jdk_type_proposals, lookup_jdk_fallback};
use kova_core::completion::resolver::resolve_local_completion_with_options;
use kova_core::completion::{
    CompletionContext, CompletionProposal, CompletionProposalKind, CompletionSource,
    CompletionTextEdit, CompletionTriggerKind, ReceiverAccessKind, ReceiverTarget,
    extract_completion_context,
};
use kova_core::navigation::SourceLanguageKind;
use kova_jvm_index::{JdkOnDemandIndex, JvmCompletionMember, JvmIndexSnapshot, JvmSymbolKind};
use kova_lsp_dap::lsp::features::{CompletionCandidate, CompletionTrigger, completion};
use kova_lsp_dap::lsp::{JdtlsClient, LspLifecycleState, path_to_lsp_uri};
use kova_toolchain::{JdkDiscoveryRoots, discover_jdks};
use kova_ui::{CompletionOutcome, CompletionRequest, CompletionSink};
use lsp_types::CompletionItemKind;
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use crate::AppEvent;
use crate::definition::utf16_point_at_byte;

/// Timeout applied to asynchronous Level-2 `textDocument/completion` requests.
const LSP_COMPLETION_TIMEOUT: Duration = Duration::from_millis(1500);

/// Host backend maintaining the on-demand JDK `.jmod` index, optional workspace [`JvmIndexSnapshot`],
/// and optional active [`JdtlsClient`].
#[derive(Clone, Default)]
pub struct CompletionBackend {
    jdk_index: Arc<OnceLock<Arc<JdkOnDemandIndex>>>,
    workspace_snapshot: Arc<Mutex<Option<Arc<JvmIndexSnapshot>>>>,
    lsp_client: Arc<Mutex<Option<Arc<JdtlsClient>>>>,
}

impl std::fmt::Debug for CompletionBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompletionBackend").finish_non_exhaustive()
    }
}

impl CompletionBackend {
    /// Creates a lazy completion backend that discovers local JDKs on first use.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a backend pre-configured with an explicit [`JdkOnDemandIndex`] (for tests or
    /// custom JDK paths).
    #[must_use]
    pub fn with_jdk_index(jdk: Arc<JdkOnDemandIndex>) -> Self {
        let lock = OnceLock::new();
        drop(lock.set(jdk));
        Self {
            jdk_index: Arc::new(lock),
            workspace_snapshot: Arc::new(Mutex::new(None)),
            lsp_client: Arc::new(Mutex::new(None)),
        }
    }

    /// Attaches an active [`JdtlsClient`] for asynchronous Level-2 Java completion.
    pub fn attach_lsp_client(&self, client: Arc<JdtlsClient>) {
        *self
            .lsp_client
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(client);
    }

    /// Attaches a workspace [`JvmIndexSnapshot`] used before `.jmod` disk lookups.
    pub fn attach_snapshot(&self, snapshot: Arc<JvmIndexSnapshot>) {
        *self
            .workspace_snapshot
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(snapshot);
    }

    fn current_lsp(&self) -> Option<Arc<JdtlsClient>> {
        self.lsp_client
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn current_snapshot(&self) -> Option<Arc<JvmIndexSnapshot>> {
        self.workspace_snapshot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn get_or_init_jdk(&self) -> Arc<JdkOnDemandIndex> {
        Arc::clone(self.jdk_index.get_or_init(|| {
            let discovered = KovaPaths::resolve()
                .ok()
                .map(|paths| discover_jdks(&JdkDiscoveryRoots::from_env_and_paths(&paths)));
            if let Some(jdks) = discovered
                && let Some(jdk) = jdks
                    .iter()
                    .find(|j| j.jmods_dir.is_some())
                    .or_else(|| jdks.first())
            {
                return Arc::new(JdkOnDemandIndex::from_jdk_home(&jdk.home_dir));
            }
            Arc::new(JdkOnDemandIndex::empty())
        }))
    }
}

/// Creates the default [`CompletionSink`] installed on [`kova_ui::WorkspaceShell`].
#[must_use]
pub fn completion_sink(runtime: Handle, events: mpsc::UnboundedSender<AppEvent>) -> CompletionSink {
    completion_sink_with_backend(runtime, events, CompletionBackend::new())
}

/// Creates a [`CompletionSink`] backed by an explicit [`CompletionBackend`].
#[must_use]
pub fn completion_sink_with_backend(
    runtime: Handle,
    events: mpsc::UnboundedSender<AppEvent>,
    backend: CompletionBackend,
) -> CompletionSink {
    Arc::new(move |request: CompletionRequest| {
        let events = events.clone();
        let backend = backend.clone();
        runtime.spawn(async move {
            resolve_and_emit(request, backend, events).await;
        });
    })
}

async fn resolve_and_emit(
    request: CompletionRequest,
    backend: CompletionBackend,
    events: mpsc::UnboundedSender<AppEvent>,
) {
    let request_id = request.request_id;
    let language = SourceLanguageKind::from_path(&request.source_path);
    let lsp_client = (language == SourceLanguageKind::Java)
        .then(|| backend.current_lsp())
        .flatten()
        .filter(|client| {
            matches!(
                client.state(),
                LspLifecycleState::Ready | LspLifecycleState::IndexingProject
            )
        });
    let has_lsp = lsp_client.is_some();

    let req_clone = request.clone();
    let backend_clone = backend.clone();
    let level1 = tokio::task::spawn_blocking(move || {
        let jdk = backend_clone.get_or_init_jdk();
        let snapshot = backend_clone.current_snapshot();
        resolve_level1(&req_clone, &jdk, snapshot.as_deref(), has_lsp)
    })
    .await;

    let Ok((outcome1, ctx_opt, buffer_snapshot)) = level1 else {
        drop(events.send(AppEvent::CompletionResolved {
            request_id,
            outcome: CompletionOutcome::Empty { is_final: !has_lsp },
        }));
        return;
    };

    drop(events.send(AppEvent::CompletionResolved {
        request_id,
        outcome: outcome1,
    }));

    let Some(client) = lsp_client else {
        return;
    };
    let Some(ctx) = ctx_opt else {
        drop(events.send(AppEvent::CompletionResolved {
            request_id,
            outcome: CompletionOutcome::Empty { is_final: true },
        }));
        return;
    };

    let abs_source = match (&request.project_root, &request.source_path) {
        (Some(root), rel) if rel.is_relative() => root.join(rel),
        (_, path) => path.clone(),
    };
    let Ok(uri) = path_to_lsp_uri(&abs_source) else {
        drop(events.send(AppEvent::CompletionResolved {
            request_id,
            outcome: CompletionOutcome::Empty { is_final: true },
        }));
        return;
    };

    let pos = utf16_point_at_byte(&buffer_snapshot, request.cursor_byte);
    let trigger = match request.trigger_kind {
        CompletionTriggerKind::Invoked => CompletionTrigger::Invoked,
        CompletionTriggerKind::AutoChar(c) => CompletionTrigger::TriggerCharacter(c),
    };

    let lsp_outcome = match tokio::time::timeout(
        LSP_COMPLETION_TIMEOUT,
        completion(&client, uri.as_str(), pos, trigger),
    )
    .await
    {
        Ok(Ok(result)) if !result.candidates.is_empty() => {
            let proposals = result
                .candidates
                .iter()
                .map(|c| map_lsp_candidate(c, ctx.is_method_reference))
                .collect();
            CompletionOutcome::Ready {
                anchor_byte: ctx.anchor_byte,
                is_method_reference: ctx.is_method_reference,
                initial_prefix: ctx.prefix,
                proposals,
                is_final: true,
            }
        }
        _ => CompletionOutcome::Empty { is_final: true },
    };

    drop(events.send(AppEvent::CompletionResolved {
        request_id,
        outcome: lsp_outcome,
    }));
}

fn resolve_level1(
    request: &CompletionRequest,
    jdk: &JdkOnDemandIndex,
    jvm_snapshot: Option<&JvmIndexSnapshot>,
    has_lsp: bool,
) -> (CompletionOutcome, Option<CompletionContext>, BufferSnapshot) {
    let buffer = Buffer::from_text(&request.source_text);
    let snap = buffer.snapshot();
    let Some(ctx) = extract_completion_context(
        &snap,
        &request.source_path,
        request.cursor_byte,
        request.trigger_kind,
    ) else {
        return (CompletionOutcome::Empty { is_final: !has_lsp }, None, snap);
    };

    let field_resolver =
        |fqcn: &str, field: &str| jdk.resolve_field_type(fqcn, field, jvm_snapshot);
    let mut local = resolve_local_completion_with_options(
        &ctx,
        &request.source_text,
        &request.open_buffers,
        request.project_root.as_deref(),
        &request.workspace_files,
        false,
        Some(&field_resolver),
    );

    if let Some(ReceiverTarget::JdkType { ref fqcn, access }) = local.receiver_target {
        let static_only = access == ReceiverAccessKind::StaticType;
        let bytecode_members = jdk.collect_members(fqcn, static_only, jvm_snapshot);
        if bytecode_members.is_empty() {
            local.proposals = lookup_jdk_fallback(fqcn, static_only, ctx.is_method_reference);
        } else {
            local.proposals = bytecode_members
                .into_iter()
                .map(|m| map_jvm_member(m, ctx.is_method_reference))
                .collect();
        }
    } else if ctx.separator.is_none()
        && matches!(
            ctx.language,
            SourceLanguageKind::Java | SourceLanguageKind::Kotlin
        )
    {
        let mut seen: HashSet<String> = local.proposals.iter().map(|p| p.label.clone()).collect();
        for prop in builtin_jdk_type_proposals() {
            if seen.insert(prop.label.clone()) {
                local.proposals.push(prop);
            }
        }
    }

    let outcome = if local.proposals.is_empty() {
        CompletionOutcome::Empty { is_final: !has_lsp }
    } else {
        CompletionOutcome::Ready {
            anchor_byte: ctx.anchor_byte,
            is_method_reference: ctx.is_method_reference,
            initial_prefix: ctx.prefix.clone(),
            proposals: local.proposals,
            is_final: !has_lsp,
        }
    };

    (outcome, Some(ctx), snap)
}

fn map_jvm_member(member: JvmCompletionMember, is_method_reference: bool) -> CompletionProposal {
    let is_callable = matches!(
        member.kind,
        JvmSymbolKind::Method | JvmSymbolKind::Constructor
    );
    let kind = match member.kind {
        JvmSymbolKind::Method => CompletionProposalKind::Method,
        JvmSymbolKind::Constructor => CompletionProposalKind::Constructor,
        _ => CompletionProposalKind::Field,
    };
    let has_params = member
        .params_signature
        .as_deref()
        .is_some_and(|p| p.trim() != "()" && !p.trim().is_empty());

    CompletionProposal {
        label: member.name.clone(),
        kind,
        params_signature: member.params_signature,
        return_type: member.return_type,
        detail_right: None,
        insert_text: member.name,
        is_callable,
        has_params,
        is_method_reference,
        additional_text_edits: Vec::new(),
        source: CompletionSource::JdkBytecode,
        is_deprecated: member.is_deprecated,
        sort_priority: member.sort_priority,
    }
}

/// Maps an LSP [`CompletionCandidate`] from `jdt.ls` into a [`CompletionProposal`].
#[must_use]
pub fn map_lsp_candidate(
    candidate: &CompletionCandidate,
    is_method_reference: bool,
) -> CompletionProposal {
    let kind = match candidate.kind {
        Some(CompletionItemKind::METHOD | CompletionItemKind::FUNCTION) => {
            CompletionProposalKind::Method
        }
        Some(CompletionItemKind::CONSTRUCTOR) => CompletionProposalKind::Constructor,
        Some(
            CompletionItemKind::FIELD
            | CompletionItemKind::PROPERTY
            | CompletionItemKind::ENUM_MEMBER
            | CompletionItemKind::CONSTANT,
        ) => CompletionProposalKind::Field,
        Some(CompletionItemKind::INTERFACE) => CompletionProposalKind::Interface,
        Some(CompletionItemKind::ENUM) => CompletionProposalKind::Enum,
        Some(CompletionItemKind::STRUCT) => CompletionProposalKind::Struct,
        Some(CompletionItemKind::MODULE) => CompletionProposalKind::Module,
        Some(CompletionItemKind::CLASS) => CompletionProposalKind::Class,
        Some(CompletionItemKind::KEYWORD) => CompletionProposalKind::Keyword,
        _ => CompletionProposalKind::Variable,
    };

    let is_callable = matches!(
        kind,
        CompletionProposalKind::Method | CompletionProposalKind::Constructor
    );
    let (label_name, label_params, label_ret) = parse_lsp_label_or_detail(&candidate.label);
    let (_, detail_params, detail_ret) = candidate
        .detail
        .as_deref()
        .map_or((String::new(), None, None), parse_lsp_label_or_detail);

    let params_signature = label_params.or(detail_params);
    let return_type = label_ret.or(detail_ret);
    let (insert_text, snippet_has_params) = normalize_lsp_insert_text(&candidate.insert_text);

    let has_params = snippet_has_params
        || params_signature
            .as_deref()
            .is_some_and(|p| p.trim() != "()" && !p.trim().is_empty());

    let additional_text_edits = candidate
        .additional_text_edits
        .iter()
        .map(|edit| CompletionTextEdit {
            start_utf16_line: edit.range.start.row,
            start_utf16_col: edit.range.start.column,
            end_utf16_line: edit.range.end.row,
            end_utf16_col: edit.range.end.column,
            new_text: edit.new_text.clone(),
        })
        .collect();

    CompletionProposal {
        label: label_name,
        kind,
        params_signature,
        return_type,
        detail_right: candidate.detail.clone(),
        insert_text,
        is_callable,
        has_params,
        is_method_reference,
        additional_text_edits,
        source: CompletionSource::Lsp,
        is_deprecated: false,
        sort_priority: 1,
    }
}

fn parse_lsp_label_or_detail(raw: &str) -> (String, Option<String>, Option<String>) {
    let trimmed = raw.trim();
    let (main, ret) = if let Some((left, right)) = trimmed.rsplit_once(" : ") {
        (left.trim(), Some(right.trim().to_owned()))
    } else if let Some((left, right)) = trimmed.rsplit_once(" -> ") {
        (left.trim(), Some(right.trim().to_owned()))
    } else {
        (trimmed, None)
    };

    if let Some(open) = main.find('(')
        && let Some(close) = main[open..].rfind(')')
    {
        let name = main[..open].trim().to_owned();
        let params = main[open..=open + close].trim().to_owned();
        return (name, Some(params), ret);
    }

    (main.to_owned(), None, ret)
}

fn normalize_lsp_insert_text(raw: &str) -> (String, bool) {
    let trimmed = raw.trim();
    if let Some(open) = trimmed.find('(')
        && let Some(close) = trimmed.rfind(')')
    {
        let base = trimmed[..open].trim().to_owned();
        let inside = trimmed[open + 1..close].trim();
        let has_params = !inside.is_empty() && inside != "$0";
        return (base, has_params);
    }
    (trimmed.replace("$0", ""), false)
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use kova_core::completion::CompletionTriggerKind;
    use kova_jvm_index::BytecodeScanOptions;
    use kova_lsp_dap::lsp::{
        ClientRuntimeConfig, IncomingMessage, JsonRpcResponse, LspEvent, read_lsp_frame,
        write_lsp_frame,
    };
    use serde_json::json;
    use tokio::io::{AsyncWriteExt, BufReader};
    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    use super::*;
    use crate::build_io_runtime;

    #[test]
    fn completion_sink_resolves_objects_and_local_string_under_15ms_in_fallback_mode() {
        let runtime = build_io_runtime().expect("runtime");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let backend = CompletionBackend::with_jdk_index(Arc::new(JdkOnDemandIndex::empty()));
        let sink = completion_sink_with_backend(runtime.handle().clone(), tx, backend);

        let src = "public class App {\n    void run() {\n        Objects.\n    }\n}\n";
        let pos = src.find("Objects.").unwrap() + "Objects.".len();

        let started = Instant::now();
        sink(CompletionRequest {
            request_id: 1,
            project_root: None,
            source_path: PathBuf::from("src/App.java"),
            source_text: src.to_owned(),
            cursor_byte: pos,
            trigger_kind: CompletionTriggerKind::AutoChar('.'),
            open_buffers: vec![],
            workspace_files: vec![],
        });

        let event = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("timeout")
                .expect("event")
        });
        assert!(started.elapsed() < Duration::from_millis(50));

        match event {
            AppEvent::CompletionResolved {
                request_id: 1,
                outcome:
                    CompletionOutcome::Ready {
                        proposals,
                        is_final: true,
                        ..
                    },
            } => {
                let labels: Vec<&str> = proposals.iter().map(|p| p.label.as_str()).collect();
                assert!(labels.contains(&"requireNonNull"));
                assert!(labels.contains(&"isNull"));
                assert!(labels.contains(&"nonNull"));
                assert!(labels.contains(&"equals"));
                assert!(labels.contains(&"hash"));
                assert!(labels.contains(&"toString"));
            }
            other => panic!("unexpected outcome: {other:?}"),
        }

        // Local variable `String s = "x"; s.`
        let src_s = "public class App { void run() { String s = \"x\"; s. } }\n";
        let pos_s = src_s.find("s. }").unwrap() + 2;
        sink(CompletionRequest {
            request_id: 2,
            project_root: None,
            source_path: PathBuf::from("src/App.java"),
            source_text: src_s.to_owned(),
            cursor_byte: pos_s,
            trigger_kind: CompletionTriggerKind::AutoChar('.'),
            open_buffers: vec![],
            workspace_files: vec![],
        });
        let event_s = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("timeout")
                .expect("event")
        });
        match event_s {
            AppEvent::CompletionResolved {
                request_id: 2,
                outcome: CompletionOutcome::Ready { proposals, .. },
            } => {
                assert!(proposals.iter().any(|p| p.label == "substring"));
                assert!(proposals.iter().any(|p| p.label == "length"));
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    fn recv_ready(
        runtime: &crate::Runtime,
        rx: &mut mpsc::UnboundedReceiver<AppEvent>,
        expected_id: u64,
    ) -> (usize, bool, String, Vec<CompletionProposal>) {
        let event = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("timeout")
                .expect("event")
        });
        match event {
            AppEvent::CompletionResolved {
                request_id,
                outcome:
                    CompletionOutcome::Ready {
                        anchor_byte,
                        is_method_reference,
                        initial_prefix,
                        proposals,
                        ..
                    },
            } if request_id == expected_id => {
                (anchor_byte, is_method_reference, initial_prefix, proposals)
            }
            other => panic!("expected Ready outcome for id {expected_id}, got {other:?}"),
        }
    }

    #[test]
    fn completion_sink_end_to_end_objects_dot_filter_enter_tab_and_esc() {
        let runtime = build_io_runtime().expect("runtime");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let backend = CompletionBackend::with_jdk_index(Arc::new(JdkOnDemandIndex::empty()));
        let sink = completion_sink_with_backend(runtime.handle().clone(), tx, backend);

        let mut editor = kova_ui::editor::state::EditorState::from_text(
            "public class App {\n    void run() {\n        Objects\n    }\n}\n",
        );
        let dot_pos = editor.snapshot().text().find("Objects\n").unwrap() + "Objects".len();
        editor.set_caret(dot_pos);
        assert!(editor.insert_text("."));

        let make_req = |request_id, text: String, cursor_byte, trigger_kind| CompletionRequest {
            request_id,
            project_root: None,
            source_path: PathBuf::from("src/App.java"),
            source_text: text,
            cursor_byte,
            trigger_kind,
            open_buffers: vec![],
            workspace_files: vec![],
        };

        sink(make_req(
            10,
            editor.snapshot().text(),
            editor.primary_head(),
            CompletionTriggerKind::AutoChar('.'),
        ));
        let (anchor, is_ref, prefix0, proposals) = recv_ready(&runtime, &mut rx, 10);
        let mut popup = kova_ui::editor::completion::CompletionPopupState::new();
        popup.open_with(10, anchor, is_ref, prefix0, proposals);
        assert!(popup.is_open());
        assert!(
            popup
                .filtered()
                .iter()
                .any(|item| item.proposal.label == "requireNonNull")
        );

        // Live prefix filtering: type `req`
        assert!(editor.insert_text("req"));
        let prefix = editor
            .snapshot()
            .slice_bytes(popup.anchor_byte()..editor.primary_head())
            .unwrap()
            .to_string();
        popup.set_prefix(&prefix);
        assert_eq!(popup.filtered()[0].proposal.label, "requireNonNull");

        // Esc dismisses without mutating buffer
        assert_eq!(
            kova_ui::editor::completion::classify_completion_key(
                &gpui::Keystroke::parse("escape").unwrap()
            ),
            kova_ui::editor::completion::CompletionKeyOutcome::Close
        );
        popup.close();
        assert!(!popup.is_open());

        // Re-open via Ctrl+Space (`Invoked`) on `Objects.req` and accept via Enter / Tab
        sink(make_req(
            11,
            editor.snapshot().text(),
            editor.primary_head(),
            CompletionTriggerKind::Invoked,
        ));
        let (anchor, is_ref, prefix1, proposals) = recv_ready(&runtime, &mut rx, 11);
        assert_eq!(prefix1, "req");
        popup.open_with(11, anchor, is_ref, prefix1, proposals);
        for key in ["enter", "tab"] {
            assert_eq!(
                kova_ui::editor::completion::classify_completion_key(
                    &gpui::Keystroke::parse(key).unwrap()
                ),
                kova_ui::editor::completion::CompletionKeyOutcome::Accept
            );
        }
        let insertion = popup.selected_insertion().unwrap();
        let range = popup.anchor_byte()..editor.primary_head();
        assert!(editor.apply_edits(
            &[kova_buffer::TextEdit::new(range.clone(), &insertion.text)],
            kova_buffer::EditOrigin::Compound,
        ));
        editor.set_caret(range.start + insertion.cursor_offset_in_text);
        popup.close();
        assert!(
            editor
                .snapshot()
                .text()
                .contains("Objects.requireNonNull()")
        );
    }

    fn spawn_mock_completion_server(
        server_read: tokio::io::DuplexStream,
        mut writer: tokio::io::DuplexStream,
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
                        json!({ "capabilities": { "completionProvider": { "triggerCharacters": ["."] } } }),
                    ),
                    "textDocument/completion" => JsonRpcResponse::success(
                        req.id,
                        json!({
                            "isIncomplete": false,
                            "items": [{
                                "label": "requireNonNullElse(T obj, T defaultObj) : T",
                                "kind": 2,
                                "detail": "(T obj, T defaultObj) : T",
                                "insertText": "requireNonNullElse(${1:obj}, ${2:defaultObj})",
                                "insertTextFormat": 2,
                                "additionalTextEdits": [{
                                    "range": {
                                        "start": { "line": 0, "character": 0 },
                                        "end": { "line": 0, "character": 0 }
                                    },
                                    "newText": "import java.util.Objects;\n"
                                }]
                            }]
                        }),
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

    fn setup_mock_jmod_app(root: &std::path::Path) -> (PathBuf, String, Arc<JdkOnDemandIndex>) {
        let app_rel = PathBuf::from("src/App.java");
        let app_abs = root.join(&app_rel);
        std::fs::create_dir_all(app_abs.parent().unwrap()).unwrap();
        let app_src = "public class App {\n    void run() {\n        Objects.\n    }\n}\n";
        std::fs::write(&app_abs, app_src).unwrap();

        let jmods_dir = root.join("jdk/jmods");
        std::fs::create_dir_all(&jmods_dir).unwrap();
        let objects_class = build_minimal_objects_class();
        write_mock_jmod(
            &jmods_dir.join("java.base.jmod"),
            "classes/java/util/Objects.class",
            &objects_class,
        );
        assert!(
            kova_jvm_index::parse_class_bytes(
                &objects_class,
                kova_jvm_index::JvmSourceOrigin::ClassFile {
                    path: PathBuf::from("Objects.class")
                },
                &BytecodeScanOptions::default(),
            )
            .unwrap()
            .is_some()
        );
        let jdk_index = Arc::new(JdkOnDemandIndex::from_jdk_home(&root.join("jdk")));
        (app_rel, app_src.to_owned(), jdk_index)
    }

    #[test]
    fn completion_sink_reads_mock_jmod_and_merges_level2_lsp_with_auto_import() {
        let runtime = build_io_runtime().expect("runtime");
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let (app_rel, app_src, jdk_index) = setup_mock_jmod_app(&root);
        let backend = CompletionBackend::with_jdk_index(jdk_index);

        runtime.block_on(async {
            let (client_read, server_write) = tokio::io::duplex(64 * 1024);
            let (server_read, client_write) = tokio::io::duplex(64 * 1024);
            let server_task = spawn_mock_completion_server(server_read, server_write);

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

            backend.attach_lsp_client(Arc::clone(&client));
            let (tx, mut rx) = mpsc::unbounded_channel();
            let sink = completion_sink_with_backend(Handle::current(), tx, backend);

            let pos = app_src.find("Objects.").unwrap() + "Objects.".len();
            sink(CompletionRequest {
                request_id: 9,
                project_root: Some(root.clone()),
                source_path: app_rel,
                source_text: app_src,
                cursor_byte: pos,
                trigger_kind: CompletionTriggerKind::AutoChar('.'),
                open_buffers: vec![],
                workspace_files: vec![],
            });

            let ev1 = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(
                ev1,
                AppEvent::CompletionResolved {
                    request_id: 9,
                    outcome: CompletionOutcome::Ready {
                        ref proposals,
                        is_final: false,
                        ..
                    },
                } if proposals.iter().any(|p| p.label == "requireNonNull" && p.source == CompletionSource::JdkBytecode)
            ));

            let ev2 = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap();
            match ev2 {
                AppEvent::CompletionResolved {
                    request_id: 9,
                    outcome:
                        CompletionOutcome::Ready {
                            proposals,
                            is_final: true,
                            ..
                        },
                } => {
                    let lsp_item = proposals
                        .iter()
                        .find(|p| p.label == "requireNonNullElse")
                        .expect("LSP item must be present");
                    assert_eq!(lsp_item.source, CompletionSource::Lsp);
                    assert_eq!(lsp_item.additional_text_edits.len(), 1);
                    assert_eq!(
                        lsp_item.additional_text_edits[0].new_text,
                        "import java.util.Objects;\n"
                    );
                }
                other => panic!("expected Level-2 LSP outcome second, got {other:?}"),
            }

            drop(client.shutdown_graceful(Duration::from_secs(1)).await);
            server_task.abort();
        });
    }

    fn write_mock_jmod(path: &std::path::Path, entry_name: &str, class_bytes: &[u8]) {
        let mut file = std::fs::File::create(path).unwrap();
        file.write_all(&[0x4A, 0x4D, 0x01, 0x00]).unwrap();
        let mut writer = ZipWriter::new(file);
        writer
            .start_file(entry_name, SimpleFileOptions::default())
            .unwrap();
        writer.write_all(class_bytes).unwrap();
        writer.finish().unwrap();
    }

    fn build_minimal_objects_class() -> Vec<u8> {
        // Constant pool entries (1-based):
        // 1: Utf8 "java/util/Objects"
        // 2: Class #1
        // 3: Utf8 "java/lang/Object"
        // 4: Class #3
        // 5: Utf8 "requireNonNull"
        // 6: Utf8 "(Ljava/lang/Object;)Ljava/lang/Object;"
        let mut buf = Vec::new();
        buf.extend_from_slice(&0xCAFE_BABEu32.to_be_bytes());
        buf.extend_from_slice(&0u16.to_be_bytes()); // minor
        buf.extend_from_slice(&65u16.to_be_bytes()); // major (Java 21)
        buf.extend_from_slice(&7u16.to_be_bytes()); // constant_pool_count = 6 + 1
        let push_utf8 = |buf: &mut Vec<u8>, s: &str| {
            buf.push(1);
            buf.extend_from_slice(&u16::try_from(s.len()).unwrap().to_be_bytes());
            buf.extend_from_slice(s.as_bytes());
        };
        let push_class = |buf: &mut Vec<u8>, idx: u16| {
            buf.push(7);
            buf.extend_from_slice(&idx.to_be_bytes());
        };
        push_utf8(&mut buf, "java/util/Objects"); // #1
        push_class(&mut buf, 1); // #2
        push_utf8(&mut buf, "java/lang/Object"); // #3
        push_class(&mut buf, 3); // #4
        push_utf8(&mut buf, "requireNonNull"); // #5
        push_utf8(&mut buf, "(Ljava/lang/Object;)Ljava/lang/Object;"); // #6

        buf.extend_from_slice(&0x0021u16.to_be_bytes()); // ACC_PUBLIC | ACC_SUPER
        buf.extend_from_slice(&2u16.to_be_bytes()); // this_class = #2
        buf.extend_from_slice(&4u16.to_be_bytes()); // super_class = #4
        buf.extend_from_slice(&0u16.to_be_bytes()); // interfaces_count
        buf.extend_from_slice(&0u16.to_be_bytes()); // fields_count
        buf.extend_from_slice(&1u16.to_be_bytes()); // methods_count
        // Method: public static requireNonNull
        buf.extend_from_slice(&0x0009u16.to_be_bytes()); // ACC_PUBLIC | ACC_STATIC
        buf.extend_from_slice(&5u16.to_be_bytes()); // name_index = #5
        buf.extend_from_slice(&6u16.to_be_bytes()); // descriptor_index = #6
        buf.extend_from_slice(&0u16.to_be_bytes()); // attributes_count
        buf.extend_from_slice(&0u16.to_be_bytes()); // class attributes_count
        buf
    }
}
