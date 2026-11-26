//! Language Server Protocol client for Eclipse `jdt.ls` (KOVA-036).

pub mod lifecycle;
pub mod session;
pub mod sync;
pub mod transport;

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
