//! Language Server Protocol client for Eclipse `jdt.ls` (KOVA-036) and the Java semantics
//! built on top of it (KOVA-037).

pub mod definition;
pub mod diagnostics;
pub mod features;
pub mod lifecycle;
pub mod session;
pub mod sync;
pub mod transport;

pub use definition::{
    ClassArchiveLocator, DefinitionResolver, DefinitionTarget, ParsedClassUri, find_declaration,
    go_to_definition, parse_class_uri,
};
pub use diagnostics::{
    DiagnosticEntry, DiagnosticFileSnapshot, DiagnosticSeverity, DiagnosticSnapshot, DiagnosticTag,
    DiagnosticsStore, DiagnosticsSummary,
};
pub use features::{
    ApplyEditCollector, CodeActionChoice, CodeActionCommand, CodeActionOutcome, CollectSession,
    CompletionCandidate, CompletionResult, CompletionTrigger, FileOperationEdit, HoverInfo,
    InlayHintEntry, InlayHintKind, NormalizedWorkspaceEdit, PrepareRenameTarget, TextFileEdit,
    Utf16TextEdit, apply_code_action, code_actions, completion, hover, inlay_hints,
    is_java_reserved, normalize_workspace_edit, prepare_rename, rename_symbol,
    resolve_completion_item, validate_new_name, writable_path,
};
pub use lifecycle::{
    ClientRuntimeConfig, JdtlsClient, LspEvent, LspLifecycleState, build_client_capabilities,
};
pub use session::{
    DefaultServerRequestHandler, LspSession, NotificationHook, ServerRequestFuture,
    ServerRequestHandler, SessionOptions,
};
pub use sync::{
    DocumentSyncTracker, OpenDocumentState, SyncModeApplied, lsp_uri_to_path, path_to_lsp_uri,
};
pub use transport::{
    IncomingMessage, JsonRpcError, JsonRpcId, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest,
    JsonRpcResponse, MAX_LSP_MESSAGE_BYTES, read_lsp_frame, write_lsp_frame,
};
