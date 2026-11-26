//! Java editing semantics on top of `jdt.ls` (KOVA-037): completion with auto-imports,
//! inlay hints, hover, *Quick Fixes* (`Alt+Enter`), *Rename* (`Shift+F6`) and the
//! deterministic [`NormalizedWorkspaceEdit`] every refactoring is reduced to.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use kova_buffer::{Utf16Point, Utf16Range};
use lsp_types::{
    CodeAction, CodeActionOrCommand, CompletionItem, CompletionResponse, CompletionTextEdit,
    DocumentChangeOperation, DocumentChanges, Documentation, Hover, HoverContents, InlayHint,
    InlayHintLabel, MarkedString, OneOf, PrepareRenameResponse, ResourceOp, TextDocumentEdit, Uri,
    WorkspaceEdit,
};
use serde_json::{Value, json};
use url::Url;

use crate::error::LspDapError;
use crate::lsp::diagnostics::DiagnosticsStore;
use crate::lsp::lifecycle::JdtlsClient;
use crate::lsp::session::{ServerRequestFuture, ServerRequestHandler};
use crate::lsp::sync::{lsp_position, lsp_range, utf16_point, utf16_range};

// ---------------------------------------------------------------------------
// NormalizedWorkspaceEdit
// ---------------------------------------------------------------------------

/// A text replacement in UTF-16 LSP coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Utf16TextEdit {
    /// Range replaced in the pre-edit document.
    pub range: Utf16Range,
    /// Replacement text.
    pub new_text: String,
}

impl Utf16TextEdit {
    /// Converts an LSP text edit.
    #[must_use]
    pub fn from_lsp(edit: &lsp_types::TextEdit) -> Self {
        Self {
            range: utf16_range(edit.range),
            new_text: edit.new_text.clone(),
        }
    }
}

/// All edits of one file, sorted from the end of the document to its start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextFileEdit {
    /// Target file.
    pub file_path: PathBuf,
    /// Document version the server based the edits on, if any.
    pub expected_version: Option<i32>,
    /// Non-overlapping edits in **descending** order: applying them one after another never
    /// shifts the coordinates of the edits still to apply.
    pub edits: Vec<Utf16TextEdit>,
}

/// A resource operation of a workspace edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileOperationEdit {
    /// Create a file.
    CreateFile {
        /// File to create.
        path: PathBuf,
        /// Overwrite an existing file.
        overwrite: bool,
        /// Do nothing when the file exists.
        ignore_if_exists: bool,
    },
    /// Rename (move) a file.
    RenameFile {
        /// Current path.
        old_path: PathBuf,
        /// New path.
        new_path: PathBuf,
        /// Overwrite an existing target.
        overwrite: bool,
        /// Do nothing when the target exists.
        ignore_if_exists: bool,
    },
    /// Delete a file or directory.
    DeleteFile {
        /// Path to delete.
        path: PathBuf,
        /// Delete directories recursively.
        recursive: bool,
        /// Do nothing when the path does not exist.
        ignore_if_not_exists: bool,
    },
}

/// A workspace edit ready to be applied to `kova_buffer::Buffer`s.
///
/// Apply `file_ops` in order first, then `text_edits`. Text edits that the server issued
/// for a file before renaming it are already re-targeted to the new path.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NormalizedWorkspaceEdit {
    /// Resource operations in server order.
    pub file_ops: Vec<FileOperationEdit>,
    /// Text edits grouped per file, files sorted by path.
    pub text_edits: Vec<TextFileEdit>,
}

impl NormalizedWorkspaceEdit {
    /// Whether the edit changes nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.file_ops.is_empty() && self.text_edits.iter().all(|file| file.edits.is_empty())
    }

    /// Edits of `path`, if any.
    #[must_use]
    pub fn edits_for(&self, path: &Path) -> Option<&TextFileEdit> {
        self.text_edits.iter().find(|file| file.file_path == path)
    }

    /// Combines two edits (e.g. a code action's own edit and a server `workspace/applyEdit`).
    ///
    /// # Errors
    /// [`LspDapError::InvalidWorkspaceEdit`] when the combined edits of a file overlap.
    pub fn merge(self, other: Self) -> Result<Self, LspDapError> {
        let mut acc = EditAccumulator::default();
        for edit in [self, other] {
            for op in edit.file_ops {
                acc.push_op(op);
            }
            for file in edit.text_edits {
                acc.push_edits(file.file_path, file.expected_version, file.edits);
            }
        }
        acc.finish()
    }

    /// Rejects edits touching anything below `read_only_root` (e.g. the decompiled cache).
    ///
    /// # Errors
    /// [`LspDapError::ReadOnlyDocument`] naming the first offending path.
    pub fn ensure_outside(&self, read_only_root: &Path) -> Result<(), LspDapError> {
        let op_paths = self.file_ops.iter().flat_map(|op| match op {
            FileOperationEdit::CreateFile { path, .. }
            | FileOperationEdit::DeleteFile { path, .. } => vec![path],
            FileOperationEdit::RenameFile {
                old_path, new_path, ..
            } => vec![old_path, new_path],
        });
        let edit_paths = self.text_edits.iter().map(|file| &file.file_path);
        match op_paths
            .chain(edit_paths)
            .find(|path| path.starts_with(read_only_root))
        {
            Some(path) => Err(LspDapError::ReadOnlyDocument {
                uri: path.display().to_string(),
            }),
            None => Ok(()),
        }
    }
}

/// Expected version and `(sequence number, edit)` pairs of one file.
type PendingFileEdits = (Option<i32>, Vec<(usize, Utf16TextEdit)>);

#[derive(Default)]
struct EditAccumulator {
    file_ops: Vec<FileOperationEdit>,
    files: BTreeMap<PathBuf, PendingFileEdits>,
    seq: usize,
}

impl EditAccumulator {
    fn push_edits(&mut self, path: PathBuf, version: Option<i32>, edits: Vec<Utf16TextEdit>) {
        let entry = self.files.entry(path).or_insert((version, Vec::new()));
        if entry.0.is_none() {
            entry.0 = version;
        }
        for edit in edits {
            entry.1.push((self.seq, edit));
            self.seq += 1;
        }
    }

    fn push_op(&mut self, op: FileOperationEdit) {
        if let FileOperationEdit::RenameFile {
            old_path, new_path, ..
        } = &op
            && let Some(moved) = self.files.remove(old_path)
        {
            let target = self
                .files
                .entry(new_path.clone())
                .or_insert((moved.0, Vec::new()));
            target.1.extend(moved.1);
        }
        self.file_ops.push(op);
    }

    fn finish(self) -> Result<NormalizedWorkspaceEdit, LspDapError> {
        let mut text_edits = Vec::with_capacity(self.files.len());
        for (file_path, (expected_version, mut edits)) in self.files {
            // Descending by position; among equal ranges the later server edit comes first so
            // that same-point insertions end up in server order.
            edits.sort_by(|(seq_a, a), (seq_b, b)| {
                (b.range.start, b.range.end, seq_b).cmp(&(a.range.start, a.range.end, seq_a))
            });
            for pair in edits.windows(2) {
                let (later, earlier) = (&pair[0].1, &pair[1].1);
                if earlier.range.end > later.range.start {
                    return Err(LspDapError::InvalidWorkspaceEdit {
                        reason: format!(
                            "overlapping edits in `{}` at {}:{} and {}:{}",
                            file_path.display(),
                            earlier.range.start.row,
                            earlier.range.start.column,
                            later.range.start.row,
                            later.range.start.column,
                        ),
                    });
                }
            }
            text_edits.push(TextFileEdit {
                file_path,
                expected_version,
                edits: edits.into_iter().map(|(_, edit)| edit).collect(),
            });
        }
        Ok(NormalizedWorkspaceEdit {
            file_ops: self.file_ops,
            text_edits,
        })
    }
}

/// Converts a writable `file://` URI to a path; any other scheme is read-only.
///
/// # Errors
/// [`LspDapError::ReadOnlyDocument`] for `jdt://`, `jar:`, `jrt:` and other non-file URIs,
/// [`LspDapError::InvalidUri`] for malformed `file://` URIs.
pub fn writable_path(uri: &str) -> Result<PathBuf, LspDapError> {
    let url = Url::parse(uri).map_err(|err| LspDapError::InvalidUri {
        uri: uri.to_owned(),
        reason: err.to_string(),
    })?;
    if url.scheme() != "file" {
        return Err(LspDapError::ReadOnlyDocument {
            uri: uri.to_owned(),
        });
    }
    url.to_file_path().map_err(|()| LspDapError::InvalidUri {
        uri: uri.to_owned(),
        reason: "not a local file path".to_owned(),
    })
}

fn uri_path(uri: &Uri) -> Result<PathBuf, LspDapError> {
    writable_path(uri.as_str())
}

fn push_document_edit(
    acc: &mut EditAccumulator,
    edit: &TextDocumentEdit,
) -> Result<(), LspDapError> {
    let edits = edit
        .edits
        .iter()
        .map(|e| match e {
            OneOf::Left(plain) => Utf16TextEdit::from_lsp(plain),
            OneOf::Right(annotated) => Utf16TextEdit::from_lsp(&annotated.text_edit),
        })
        .collect();
    acc.push_edits(
        uri_path(&edit.text_document.uri)?,
        edit.text_document.version,
        edits,
    );
    Ok(())
}

fn resource_op(op: &ResourceOp) -> Result<FileOperationEdit, LspDapError> {
    Ok(match op {
        ResourceOp::Create(create) => {
            let options = create.options.as_ref();
            FileOperationEdit::CreateFile {
                path: uri_path(&create.uri)?,
                overwrite: options.and_then(|o| o.overwrite).unwrap_or(false),
                ignore_if_exists: options.and_then(|o| o.ignore_if_exists).unwrap_or(false),
            }
        }
        ResourceOp::Rename(rename) => {
            let options = rename.options.as_ref();
            FileOperationEdit::RenameFile {
                old_path: uri_path(&rename.old_uri)?,
                new_path: uri_path(&rename.new_uri)?,
                overwrite: options.and_then(|o| o.overwrite).unwrap_or(false),
                ignore_if_exists: options.and_then(|o| o.ignore_if_exists).unwrap_or(false),
            }
        }
        ResourceOp::Delete(delete) => {
            let options = delete.options.as_ref();
            FileOperationEdit::DeleteFile {
                path: uri_path(&delete.uri)?,
                recursive: options.and_then(|o| o.recursive).unwrap_or(false),
                ignore_if_not_exists: options
                    .and_then(|o| o.ignore_if_not_exists)
                    .unwrap_or(false),
            }
        }
    })
}

/// Normalises an LSP `WorkspaceEdit` (`changes` and/or `documentChanges`).
///
/// `documentChanges` wins over `changes` when both are present (LSP 3.17, `WorkspaceEdit`).
///
/// # Errors
/// [`LspDapError::ReadOnlyDocument`] when any target is not a `file://` URI,
/// [`LspDapError::InvalidWorkspaceEdit`] when the edits of a file overlap.
pub fn normalize_workspace_edit(
    edit: WorkspaceEdit,
) -> Result<NormalizedWorkspaceEdit, LspDapError> {
    let mut acc = EditAccumulator::default();
    match edit.document_changes {
        Some(DocumentChanges::Edits(edits)) => {
            for document_edit in &edits {
                push_document_edit(&mut acc, document_edit)?;
            }
        }
        Some(DocumentChanges::Operations(operations)) => {
            for operation in &operations {
                match operation {
                    DocumentChangeOperation::Edit(document_edit) => {
                        push_document_edit(&mut acc, document_edit)?;
                    }
                    DocumentChangeOperation::Op(op) => acc.push_op(resource_op(op)?),
                }
            }
        }
        None => {
            let mut changes: Vec<_> = edit.changes.unwrap_or_default().into_iter().collect();
            changes.sort_by(|(a, _), (b, _)| a.as_str().cmp(b.as_str()));
            for (uri, edits) in changes {
                acc.push_edits(
                    uri_path(&uri)?,
                    None,
                    edits.iter().map(Utf16TextEdit::from_lsp).collect(),
                );
            }
        }
    }
    acc.finish()
}

// ---------------------------------------------------------------------------
// Request helpers
// ---------------------------------------------------------------------------

fn position_json(pos: Utf16Point) -> Result<Value, LspDapError> {
    Ok(serde_json::to_value(lsp_position(pos.row, pos.column)?)?)
}

fn text_position_params(uri: &str, pos: Utf16Point) -> Result<Value, LspDapError> {
    Ok(json!({ "textDocument": { "uri": uri }, "position": position_json(pos)? }))
}

fn documentation_text(doc: Option<&Documentation>) -> Option<String> {
    doc.map(|doc| match doc {
        Documentation::String(text) => text.clone(),
        Documentation::MarkupContent(markup) => markup.value.clone(),
    })
}

// ---------------------------------------------------------------------------
// Completion
// ---------------------------------------------------------------------------

/// What triggered a completion request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionTrigger {
    /// Explicit invocation (`Ctrl+Space`) or typing an identifier.
    Invoked,
    /// A trigger character such as `.`.
    TriggerCharacter(char),
    /// Re-request of an incomplete list.
    Incomplete,
}

impl CompletionTrigger {
    fn context(self) -> Value {
        match self {
            Self::Invoked => json!({ "triggerKind": 1 }),
            Self::TriggerCharacter(c) => {
                json!({ "triggerKind": 2, "triggerCharacter": c.to_string() })
            }
            Self::Incomplete => json!({ "triggerKind": 3 }),
        }
    }
}

/// One completion proposal.
#[derive(Debug, Clone, PartialEq)]
pub struct CompletionCandidate {
    /// Label shown in the popup.
    pub label: String,
    /// Item kind (method, class, field, ...).
    pub kind: Option<lsp_types::CompletionItemKind>,
    /// Short detail, usually the signature.
    pub detail: Option<String>,
    /// Javadoc (markdown or plain text).
    pub documentation: Option<String>,
    /// Sort key.
    pub sort_text: Option<String>,
    /// Filter key.
    pub filter_text: Option<String>,
    /// Text inserted when no `primary_edit` is given.
    pub insert_text: String,
    /// Whether `insert_text` / `primary_edit` is an LSP snippet (`${1:arg}`).
    pub is_snippet: bool,
    /// Main edit replacing the typed prefix.
    pub primary_edit: Option<Utf16TextEdit>,
    /// Extra edits, e.g. the `import com.example.Foo;` added by `jdt.ls`.
    pub additional_text_edits: Vec<Utf16TextEdit>,
    /// The original item, sent back for `completionItem/resolve`.
    pub raw_item: Value,
}

impl CompletionCandidate {
    /// Converts an LSP completion item.
    #[must_use]
    pub fn from_lsp(item: &CompletionItem) -> Self {
        let primary_edit = item.text_edit.as_ref().map(|edit| match edit {
            CompletionTextEdit::Edit(edit) => Utf16TextEdit::from_lsp(edit),
            CompletionTextEdit::InsertAndReplace(edit) => Utf16TextEdit {
                range: utf16_range(edit.replace),
                new_text: edit.new_text.clone(),
            },
        });
        Self {
            label: item.label.clone(),
            kind: item.kind,
            detail: item.detail.clone(),
            documentation: documentation_text(item.documentation.as_ref()),
            sort_text: item.sort_text.clone(),
            filter_text: item.filter_text.clone(),
            insert_text: item
                .insert_text
                .clone()
                .or_else(|| primary_edit.as_ref().map(|e| e.new_text.clone()))
                .unwrap_or_else(|| item.label.clone()),
            is_snippet: item.insert_text_format == Some(lsp_types::InsertTextFormat::SNIPPET),
            primary_edit,
            additional_text_edits: item
                .additional_text_edits
                .iter()
                .flatten()
                .map(Utf16TextEdit::from_lsp)
                .collect(),
            raw_item: serde_json::to_value(item).unwrap_or(Value::Null),
        }
    }
}

/// Result of [`completion`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CompletionResult {
    /// Whether typing further must re-query the server ([`CompletionTrigger::Incomplete`]).
    pub is_incomplete: bool,
    /// Proposals in server order.
    pub candidates: Vec<CompletionCandidate>,
}

/// Requests `textDocument/completion` at `pos`.
///
/// # Errors
/// Transport, timeout and protocol errors of the request.
pub async fn completion(
    client: &JdtlsClient,
    uri: &str,
    pos: Utf16Point,
    trigger: CompletionTrigger,
) -> Result<CompletionResult, LspDapError> {
    let mut params = text_position_params(uri, pos)?;
    params["context"] = trigger.context();
    let response: Option<CompletionResponse> =
        client.request("textDocument/completion", params).await?;
    Ok(match response {
        None => CompletionResult::default(),
        Some(CompletionResponse::Array(items)) => CompletionResult {
            is_incomplete: false,
            candidates: items.iter().map(CompletionCandidate::from_lsp).collect(),
        },
        Some(CompletionResponse::List(list)) => CompletionResult {
            is_incomplete: list.is_incomplete,
            candidates: list
                .items
                .iter()
                .map(CompletionCandidate::from_lsp)
                .collect(),
        },
    })
}

/// Lazily resolves documentation and `additionalTextEdits` (`completionItem/resolve`).
///
/// # Errors
/// Transport, timeout and protocol errors of the request.
pub async fn resolve_completion_item(
    client: &JdtlsClient,
    candidate: &CompletionCandidate,
) -> Result<CompletionCandidate, LspDapError> {
    let item: CompletionItem = client
        .request("completionItem/resolve", candidate.raw_item.clone())
        .await?;
    Ok(CompletionCandidate::from_lsp(&item))
}

// ---------------------------------------------------------------------------
// Inlay hints
// ---------------------------------------------------------------------------

/// Kind of an inlay hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InlayHintKind {
    /// Inferred type (e.g. after `var`).
    Type,
    /// Parameter name before an argument.
    Parameter,
    /// Unspecified.
    Other,
}

/// One inlay hint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlayHintEntry {
    /// Insertion point.
    pub position: Utf16Point,
    /// Rendered label (label parts concatenated).
    pub label: String,
    /// Hint kind.
    pub kind: InlayHintKind,
    /// Render a space before the label.
    pub padding_left: bool,
    /// Render a space after the label.
    pub padding_right: bool,
}

impl InlayHintEntry {
    /// Converts an LSP inlay hint.
    #[must_use]
    pub fn from_lsp(hint: &InlayHint) -> Self {
        let label = match &hint.label {
            InlayHintLabel::String(text) => text.clone(),
            InlayHintLabel::LabelParts(parts) => parts.iter().map(|p| p.value.as_str()).collect(),
        };
        let kind = match hint.kind {
            Some(k) if k == lsp_types::InlayHintKind::TYPE => InlayHintKind::Type,
            Some(k) if k == lsp_types::InlayHintKind::PARAMETER => InlayHintKind::Parameter,
            _ => InlayHintKind::Other,
        };
        Self {
            position: utf16_point(hint.position),
            label,
            kind,
            padding_left: hint.padding_left.unwrap_or(false),
            padding_right: hint.padding_right.unwrap_or(false),
        }
    }
}

/// Requests `textDocument/inlayHint` for lines `start_line..=end_line`.
///
/// # Errors
/// [`LspDapError::InvalidArgument`] when `start_line > end_line`, plus transport, timeout and
/// protocol errors of the request.
pub async fn inlay_hints(
    client: &JdtlsClient,
    uri: &str,
    start_line: usize,
    end_line: usize,
) -> Result<Vec<InlayHintEntry>, LspDapError> {
    if start_line > end_line {
        return Err(LspDapError::InvalidArgument {
            reason: format!("inlay hint range {start_line}..={end_line} is reversed"),
        });
    }
    let range = lsp_range(Utf16Range::new(
        Utf16Point::new(start_line, 0),
        Utf16Point::new(end_line.saturating_add(1), 0),
    ))?;
    let params = json!({ "textDocument": { "uri": uri }, "range": range });
    let hints: Option<Vec<InlayHint>> = client.request("textDocument/inlayHint", params).await?;
    Ok(hints
        .iter()
        .flatten()
        .map(InlayHintEntry::from_lsp)
        .collect())
}

// ---------------------------------------------------------------------------
// Hover
// ---------------------------------------------------------------------------

/// Hover contents (Javadoc and signature).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoverInfo {
    /// Text to render.
    pub contents: String,
    /// Whether `contents` is markdown.
    pub is_markdown: bool,
    /// Range the hover applies to.
    pub range: Option<Utf16Range>,
}

fn marked_string(marked: &MarkedString) -> String {
    match marked {
        MarkedString::String(text) => text.clone(),
        MarkedString::LanguageString(code) => {
            format!("```{}\n{}\n```", code.language, code.value)
        }
    }
}

/// Requests `textDocument/hover`.
///
/// # Errors
/// Transport, timeout and protocol errors of the request.
pub async fn hover(
    client: &JdtlsClient,
    uri: &str,
    pos: Utf16Point,
) -> Result<Option<HoverInfo>, LspDapError> {
    let response: Option<Hover> = client
        .request("textDocument/hover", text_position_params(uri, pos)?)
        .await?;
    let Some(hover) = response else {
        return Ok(None);
    };
    let (contents, is_markdown) = match &hover.contents {
        HoverContents::Scalar(marked) => (marked_string(marked), true),
        HoverContents::Array(items) => (
            items
                .iter()
                .map(marked_string)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n"),
            true,
        ),
        HoverContents::Markup(markup) => (
            markup.value.clone(),
            markup.kind == lsp_types::MarkupKind::Markdown,
        ),
    };
    if contents.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(HoverInfo {
        contents,
        is_markdown,
        range: hover.range.map(utf16_range),
    }))
}

// ---------------------------------------------------------------------------
// Code actions (Alt+Enter)
// ---------------------------------------------------------------------------

/// A command attached to a code action.
#[derive(Debug, Clone, PartialEq)]
pub struct CodeActionCommand {
    /// Command title.
    pub title: String,
    /// Command identifier for `workspace/executeCommand`.
    pub command: String,
    /// Command arguments.
    pub arguments: Vec<Value>,
}

impl CodeActionCommand {
    fn from_lsp(command: &lsp_types::Command) -> Self {
        Self {
            title: command.title.clone(),
            command: command.command.clone(),
            arguments: command.arguments.clone().unwrap_or_default(),
        }
    }
}

/// One entry of the `Alt+Enter` menu.
#[derive(Debug, Clone, PartialEq)]
pub struct CodeActionChoice {
    /// Menu title.
    pub title: String,
    /// Kind (`quickfix`, `refactor.extract`, `source.organizeImports`, ...).
    pub kind: Option<String>,
    /// Whether this is the default action for `Alt+Enter`.
    pub is_preferred: bool,
    /// Why the action is unavailable (shown greyed out), if it is.
    pub disabled_reason: Option<String>,
    /// Edit already provided by the server (`None` means lazy `codeAction/resolve`).
    pub edit: Option<NormalizedWorkspaceEdit>,
    /// Command to execute after the edit.
    pub command: Option<CodeActionCommand>,
    /// `true` for a bare LSP `Command` (never resolved).
    pub is_command_only: bool,
    /// The original `CodeAction` / `Command` JSON.
    pub raw_action: Value,
}

impl CodeActionChoice {
    fn from_code_action(action: &CodeAction) -> Self {
        let (edit, edit_error) = match action.edit.clone().map(normalize_workspace_edit) {
            None => (None, None),
            Some(Ok(edit)) => (Some(edit), None),
            Some(Err(err)) => (None, Some(err.to_string())),
        };
        Self {
            title: action.title.clone(),
            kind: action.kind.as_ref().map(|k| k.as_str().to_owned()),
            is_preferred: action.is_preferred.unwrap_or(false),
            disabled_reason: action
                .disabled
                .as_ref()
                .map(|d| d.reason.clone())
                .or(edit_error),
            edit,
            command: action.command.as_ref().map(CodeActionCommand::from_lsp),
            is_command_only: false,
            raw_action: serde_json::to_value(action).unwrap_or(Value::Null),
        }
    }

    fn from_command(command: &lsp_types::Command) -> Self {
        Self {
            title: command.title.clone(),
            kind: None,
            is_preferred: false,
            disabled_reason: None,
            edit: None,
            command: Some(CodeActionCommand::from_lsp(command)),
            is_command_only: true,
            raw_action: serde_json::to_value(command).unwrap_or(Value::Null),
        }
    }

    fn needs_resolve(&self) -> bool {
        !self.is_command_only
            && self.edit.is_none()
            && (self.command.is_none() || self.raw_action.get("data").is_some())
    }
}

/// Result of [`apply_code_action`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CodeActionOutcome {
    /// Everything to apply: the action's edit merged with every `workspace/applyEdit` the
    /// server issued while executing its command. All coordinates refer to the documents as
    /// they were before the action, since nothing has been applied yet.
    pub edit: NormalizedWorkspaceEdit,
    /// Command that was executed, if any.
    pub executed_command: Option<String>,
    /// Result of `workspace/executeCommand`.
    pub command_result: Option<Value>,
}

/// Requests `textDocument/codeAction` for `range`, sending the stored diagnostics that
/// intersect it. Preferred actions come first.
///
/// # Errors
/// Transport, timeout and protocol errors of the request.
pub async fn code_actions(
    client: &JdtlsClient,
    store: &DiagnosticsStore,
    uri: &str,
    range: Utf16Range,
) -> Result<Vec<CodeActionChoice>, LspDapError> {
    let diagnostics: Vec<Value> = store
        .diagnostics_in_range(uri, range)
        .into_iter()
        .map(|entry| entry.raw_lsp_diagnostic)
        .collect();
    let params = json!({
        "textDocument": { "uri": uri },
        "range": lsp_range(range)?,
        "context": { "diagnostics": diagnostics, "triggerKind": 1 },
    });
    let response: Option<Vec<CodeActionOrCommand>> =
        client.request("textDocument/codeAction", params).await?;
    let mut choices: Vec<CodeActionChoice> = response
        .iter()
        .flatten()
        .map(|item| match item {
            CodeActionOrCommand::CodeAction(action) => CodeActionChoice::from_code_action(action),
            CodeActionOrCommand::Command(command) => CodeActionChoice::from_command(command),
        })
        .collect();
    // Stable: server order is kept within the preferred and non-preferred groups.
    choices.sort_by_key(|choice| !choice.is_preferred);
    Ok(choices)
}

/// Resolves (when lazy) and executes a code action, collecting every edit to apply.
///
/// Commands such as *Organize Imports* make `jdt.ls` send `workspace/applyEdit` back before
/// answering `workspace/executeCommand`; those edits are captured by the client's
/// [`ApplyEditCollector`] and merged into [`CodeActionOutcome::edit`]. This relies on the
/// server awaiting the `applyEdit` reply before answering the command, as `jdt.ls` does.
///
/// # Errors
/// [`LspDapError::InvalidArgument`] for a disabled action, normalisation errors of the edits
/// and transport, timeout and protocol errors of the requests.
pub async fn apply_code_action(
    client: &JdtlsClient,
    action: CodeActionChoice,
) -> Result<CodeActionOutcome, LspDapError> {
    if let Some(reason) = &action.disabled_reason {
        return Err(LspDapError::InvalidArgument {
            reason: format!("code action `{}` is disabled: {reason}", action.title),
        });
    }
    let session = client.apply_edit_collector().begin().await;
    let mut action = action;
    if action.needs_resolve() {
        let resolved: CodeAction = client
            .request("codeAction/resolve", action.raw_action.clone())
            .await?;
        let resolved = CodeActionChoice::from_code_action(&resolved);
        if let Some(reason) = resolved.disabled_reason {
            return Err(LspDapError::InvalidArgument { reason });
        }
        action.edit = resolved.edit;
        action.command = resolved.command.or(action.command);
    }

    let mut outcome = CodeActionOutcome {
        edit: action.edit.unwrap_or_default(),
        ..CodeActionOutcome::default()
    };
    if let Some(command) = action.command {
        let params = json!({ "command": command.command, "arguments": command.arguments });
        let result: Value = client.request("workspace/executeCommand", params).await?;
        outcome.executed_command = Some(command.command);
        outcome.command_result = Some(result);
    }
    for pushed in session.take_edits() {
        outcome.edit = outcome.edit.merge(pushed)?;
    }
    Ok(outcome)
}

/// Captures server-initiated `workspace/applyEdit` requests while an [`apply_code_action`]
/// is in flight, so the edits reach the caller instead of being silently dropped.
///
/// Outside that window it answers `{"applied": false}`.
pub struct ApplyEditCollector {
    gate: tokio::sync::Mutex<()>,
    active: Mutex<Option<Vec<NormalizedWorkspaceEdit>>>,
}

impl fmt::Debug for ApplyEditCollector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApplyEditCollector")
            .field("collecting", &self.lock().is_some())
            .finish()
    }
}

impl Default for ApplyEditCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl ApplyEditCollector {
    /// Creates an idle collector.
    #[must_use]
    pub fn new() -> Self {
        Self {
            gate: tokio::sync::Mutex::new(()),
            active: Mutex::new(None),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Option<Vec<NormalizedWorkspaceEdit>>> {
        self.active.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts collecting; concurrent callers are serialised.
    pub async fn begin(&self) -> CollectSession<'_> {
        let permit = self.gate.lock().await;
        *self.lock() = Some(Vec::new());
        CollectSession {
            collector: self,
            _permit: permit,
        }
    }

    fn accept(&self, params: &Value) -> Value {
        let mut active = self.lock();
        let Some(collected) = active.as_mut() else {
            return json!({
                "applied": false,
                "failureReason": "no code action is being applied",
            });
        };
        let edit = params
            .get("edit")
            .cloned()
            .ok_or_else(|| LspDapError::InvalidWorkspaceEdit {
                reason: "missing `edit`".to_owned(),
            })
            .and_then(|edit| Ok(serde_json::from_value::<WorkspaceEdit>(edit)?))
            .and_then(normalize_workspace_edit);
        match edit {
            Ok(edit) => {
                collected.push(edit);
                json!({ "applied": true })
            }
            Err(err) => json!({ "applied": false, "failureReason": err.to_string() }),
        }
    }
}

impl ServerRequestHandler for ApplyEditCollector {
    fn handle(&self, _method: &str, params: Value) -> ServerRequestFuture {
        Box::pin(std::future::ready(Ok(self.accept(&params))))
    }
}

/// Active collection window of an [`ApplyEditCollector`]; ends when dropped.
pub struct CollectSession<'a> {
    collector: &'a ApplyEditCollector,
    _permit: tokio::sync::MutexGuard<'a, ()>,
}

impl fmt::Debug for CollectSession<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CollectSession").finish_non_exhaustive()
    }
}

impl CollectSession<'_> {
    /// Takes the edits collected so far.
    #[must_use]
    pub fn take_edits(&self) -> Vec<NormalizedWorkspaceEdit> {
        self.collector
            .lock()
            .as_mut()
            .map(std::mem::take)
            .unwrap_or_default()
    }
}

impl Drop for CollectSession<'_> {
    fn drop(&mut self) {
        *self.collector.lock() = None;
    }
}

// ---------------------------------------------------------------------------
// Rename (Shift+F6)
// ---------------------------------------------------------------------------

/// Java reserved words and literals that can never be a symbol name.
const JAVA_RESERVED: &[&str] = &[
    "abstract",
    "assert",
    "boolean",
    "break",
    "byte",
    "case",
    "catch",
    "char",
    "class",
    "const",
    "continue",
    "default",
    "do",
    "double",
    "else",
    "enum",
    "extends",
    "false",
    "final",
    "finally",
    "float",
    "for",
    "goto",
    "if",
    "implements",
    "import",
    "instanceof",
    "int",
    "interface",
    "long",
    "native",
    "new",
    "null",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "short",
    "static",
    "strictfp",
    "super",
    "switch",
    "synchronized",
    "this",
    "throw",
    "throws",
    "transient",
    "true",
    "try",
    "void",
    "volatile",
    "while",
    "_",
];

/// Whether `name` is a Java reserved word or literal.
#[must_use]
pub fn is_java_reserved(name: &str) -> bool {
    JAVA_RESERVED.contains(&name)
}

/// Validates a new symbol name before `textDocument/rename`.
///
/// # Errors
/// [`LspDapError::InvalidArgument`] for empty names, names with whitespace and reserved words.
pub fn validate_new_name(new_name: &str) -> Result<(), LspDapError> {
    let reason = if new_name.is_empty() {
        Some("the new name is empty".to_owned())
    } else if new_name.chars().any(char::is_whitespace) {
        Some(format!("`{new_name}` contains whitespace"))
    } else if is_java_reserved(new_name) {
        Some(format!("`{new_name}` is a Java reserved word"))
    } else {
        None
    };
    reason.map_or(Ok(()), |reason| {
        Err(LspDapError::InvalidArgument { reason })
    })
}

/// Symbol accepted by `textDocument/prepareRename`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrepareRenameTarget {
    /// Range of the symbol (`None`: the client picks the word under the cursor).
    pub range: Option<Utf16Range>,
    /// Suggested initial text of the rename field.
    pub placeholder: Option<String>,
}

/// Asks the server whether the symbol at `pos` can be renamed.
///
/// Returns `Ok(None)` for read-only documents (`jdt://`, decompiled sources), for symbols the
/// server refuses and for reserved words.
///
/// # Errors
/// Transport, timeout and protocol errors of the request.
pub async fn prepare_rename(
    client: &JdtlsClient,
    uri: &str,
    pos: Utf16Point,
) -> Result<Option<PrepareRenameTarget>, LspDapError> {
    if writable_path(uri).is_err() {
        return Ok(None);
    }
    let response: Option<PrepareRenameResponse> = client
        .request(
            "textDocument/prepareRename",
            text_position_params(uri, pos)?,
        )
        .await?;
    let target = match response {
        None => None,
        Some(PrepareRenameResponse::Range(range)) => Some(PrepareRenameTarget {
            range: Some(utf16_range(range)),
            placeholder: None,
        }),
        Some(PrepareRenameResponse::RangeWithPlaceholder { range, placeholder }) => {
            Some(PrepareRenameTarget {
                range: Some(utf16_range(range)),
                placeholder: Some(placeholder),
            })
        }
        Some(PrepareRenameResponse::DefaultBehavior { default_behavior }) => default_behavior
            .then_some(PrepareRenameTarget {
                range: None,
                placeholder: None,
            }),
    };
    Ok(target.filter(|t| !t.placeholder.as_deref().is_some_and(is_java_reserved)))
}

/// Renames the symbol at `pos` across the workspace.
///
/// # Errors
/// [`LspDapError::InvalidArgument`] for an invalid name and
/// [`LspDapError::ReadOnlyDocument`] for a non-`file://` document (both before any request),
/// normalisation errors of the returned edit and transport, timeout and protocol errors.
pub async fn rename_symbol(
    client: &JdtlsClient,
    uri: &str,
    pos: Utf16Point,
    new_name: &str,
) -> Result<NormalizedWorkspaceEdit, LspDapError> {
    validate_new_name(new_name)?;
    writable_path(uri)?;
    let mut params = text_position_params(uri, pos)?;
    params["newName"] = json!(new_name);
    let edit: Option<WorkspaceEdit> = client.request("textDocument/rename", params).await?;
    edit.map_or_else(
        || Ok(NormalizedWorkspaceEdit::default()),
        normalize_workspace_edit,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(sr: usize, sc: usize, er: usize, ec: usize, text: &str) -> Value {
        json!({
            "range": {
                "start": { "line": sr, "character": sc },
                "end": { "line": er, "character": ec },
            },
            "newText": text,
        })
    }

    fn parse(value: Value) -> WorkspaceEdit {
        serde_json::from_value(value).unwrap_or_else(|err| panic!("{err}"))
    }

    #[test]
    fn sorts_descending_and_keeps_same_point_insertion_order() {
        let normalized = normalize_workspace_edit(parse(json!({
            "changes": {
                "file:///p/B.java": [edit(0, 0, 0, 0, "x")],
                "file:///p/A.java": [
                    edit(0, 0, 0, 0, "import a.X;\n"),
                    edit(5, 2, 5, 6, "bar"),
                    edit(0, 0, 0, 0, "import a.Y;\n"),
                    edit(2, 0, 3, 0, ""),
                ],
            }
        })))
        .unwrap_or_else(|err| panic!("{err}"));
        assert_eq!(normalized.text_edits.len(), 2);
        let a = &normalized.text_edits[0];
        assert_eq!(a.file_path, PathBuf::from("/p/A.java"));
        let texts: Vec<_> = a.edits.iter().map(|e| e.new_text.as_str()).collect();
        assert_eq!(texts, ["bar", "", "import a.Y;\n", "import a.X;\n"]);
        assert_eq!(
            normalized.text_edits[1].file_path,
            PathBuf::from("/p/B.java")
        );
    }

    #[test]
    fn rejects_overlaps_and_read_only_uris() {
        let overlap = normalize_workspace_edit(parse(json!({
            "changes": { "file:///p/A.java": [edit(1, 0, 1, 5, "a"), edit(1, 3, 1, 8, "b")] }
        })));
        assert!(matches!(
            overlap,
            Err(LspDapError::InvalidWorkspaceEdit { .. })
        ));

        let read_only = normalize_workspace_edit(parse(json!({
            "changes": { "jdt://contents/rt.jar/java.lang/String.class": [edit(0, 0, 0, 0, "x")] }
        })));
        assert!(matches!(
            read_only,
            Err(LspDapError::ReadOnlyDocument { .. })
        ));
    }

    #[test]
    fn document_changes_remap_edits_through_renames() {
        let normalized = normalize_workspace_edit(parse(json!({
            "documentChanges": [
                {
                    "textDocument": { "uri": "file:///p/Old.java", "version": 3 },
                    "edits": [edit(0, 13, 0, 16, "New")],
                },
                { "kind": "rename", "oldUri": "file:///p/Old.java", "newUri": "file:///p/New.java" },
                { "kind": "create", "uri": "file:///p/Extra.java", "options": { "overwrite": true } },
            ]
        })))
        .unwrap_or_else(|err| panic!("{err}"));
        assert_eq!(normalized.file_ops.len(), 2);
        assert_eq!(normalized.text_edits.len(), 1);
        assert_eq!(
            normalized.text_edits[0].file_path,
            PathBuf::from("/p/New.java")
        );
        assert_eq!(normalized.text_edits[0].expected_version, Some(3));
        assert!(normalized.ensure_outside(Path::new("/cache")).is_ok());
        assert!(normalized.ensure_outside(Path::new("/p")).is_err());
    }

    #[test]
    fn validates_new_names() {
        assert!(validate_new_name("renamed").is_ok());
        assert!(validate_new_name("").is_err());
        assert!(validate_new_name("two words").is_err());
        assert!(validate_new_name("class").is_err());
    }

    #[tokio::test]
    async fn collector_only_accepts_edits_while_active() {
        let collector = ApplyEditCollector::new();
        let params =
            json!({ "edit": { "changes": { "file:///p/A.java": [edit(0, 0, 0, 0, "x")] } } });
        let idle = collector
            .handle("workspace/applyEdit", params.clone())
            .await;
        assert_eq!(idle.ok().map(|v| v["applied"].clone()), Some(json!(false)));

        let session = collector.begin().await;
        let accepted = collector.handle("workspace/applyEdit", params).await;
        assert_eq!(
            accepted.ok().map(|v| v["applied"].clone()),
            Some(json!(true))
        );
        let rejected = collector
            .handle(
                "workspace/applyEdit",
                json!({ "edit": { "changes": { "jdt://x/Y.class": [edit(0, 0, 0, 0, "x")] } } }),
            )
            .await;
        assert_eq!(
            rejected.ok().map(|v| v["applied"].clone()),
            Some(json!(false))
        );
        assert_eq!(session.take_edits().len(), 1);
        drop(session);
        assert!(format!("{collector:?}").contains("collecting: false"));
    }
}
