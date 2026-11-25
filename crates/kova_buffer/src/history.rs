//! Non-linear Undo/Redo history tree with multi-cursor selection restoration,
//! deterministic time-based coalescing, explicit compound groups, and save-point tracking.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::time::{Duration, Instant};

use crate::buffer::Buffer;
use crate::error::{BufferError, HistoryError};
use crate::position::{BufferVersion, TextEdit};
use crate::selection::SelectionSet;
use crate::snapshot::BufferSnapshot;

/// Opaque identifier of a node inside a [`History`] tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct NodeId(usize);

impl NodeId {
    /// Root node representing the initial baseline state of the buffer.
    pub const ROOT: Self = Self(0);

    /// Creates a [`NodeId`] from a raw `usize`.
    #[must_use]
    pub const fn new(raw: usize) -> Self {
        Self(raw)
    }

    /// Returns the underlying `usize` identifier.
    #[must_use]
    pub const fn as_usize(self) -> usize {
        self.0
    }
}

/// Monotonically increasing identifier assigned to committed history transactions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct TransactionId(u64);

impl TransactionId {
    /// Sentinel transaction ID used by the baseline root node.
    pub const ZERO: Self = Self(0);

    /// Creates a [`TransactionId`] from a raw `u64`.
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the underlying `u64` value.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Returns the next sequential transaction ID (`self + 1`).
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// Classification of an edit source used to govern automatic time-based coalescing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EditOrigin {
    /// Individual character or text input from typing.
    Typing,
    /// Backward deletion (`Backspace`).
    DeleteBackward,
    /// Forward deletion (`Delete`).
    DeleteForward,
    /// Clipboard paste — never time-coalesced with adjacent edits.
    Paste,
    /// Explicit compound operation, refactoring, or formatting pass.
    Compound,
    /// Any other uncategorized single edit (not time-coalesced by default).
    Other,
}

impl EditOrigin {
    /// Returns `true` if consecutive operations of this origin are eligible for
    /// automatic time-based coalescing.
    #[must_use]
    pub const fn is_coalescible(self) -> bool {
        matches!(
            self,
            Self::Typing | Self::DeleteBackward | Self::DeleteForward
        )
    }
}

/// Configuration controlling coalescing windows and memory bounds of a [`History`] tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryConfig {
    /// Maximum elapsed time between consecutive compatible edits to merge them
    /// into a single undo transaction. Defaults to `500 ms`.
    pub grouping_window: Duration,
    /// Maximum number of nodes retained in the tree before oldest inactive branches
    /// or oldest root ancestors are pruned. Defaults to `4096`.
    pub max_nodes: usize,
    /// Maximum total byte cost (`old_text.len() + new_text.len()`) stored across all nodes.
    /// Defaults to `16 MiB`.
    pub max_stored_bytes: usize,
    /// When `true` (default), transitioning from trailing whitespace to non-whitespace
    /// during [`EditOrigin::Typing`] starts a new transaction so `undo` removes natural words.
    pub break_on_whitespace: bool,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            grouping_window: Duration::from_millis(500),
            max_nodes: 4096,
            max_stored_bytes: 16 * 1024 * 1024,
            break_on_whitespace: true,
        }
    }
}

/// A single reversible text replacement storing both pre-edit and post-edit coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReversibleEdit {
    /// Byte range in the buffer immediately before this edit was applied.
    pub before_range: Range<usize>,
    /// Byte range in the buffer immediately after this edit was applied.
    pub after_range: Range<usize>,
    /// Text removed from `before_range` (restored during `undo`).
    pub old_text: String,
    /// Text inserted into `after_range` (re-applied during `redo`).
    pub new_text: String,
}

impl ReversibleEdit {
    /// Returns the forward [`TextEdit`] for applying or redoing this change.
    #[must_use]
    pub fn forward_edit(&self) -> TextEdit {
        TextEdit::new(self.before_range.clone(), self.new_text.clone())
    }

    /// Returns the inverse [`TextEdit`] for undoing this change.
    #[must_use]
    pub fn inverse_edit(&self) -> TextEdit {
        TextEdit::new(self.after_range.clone(), self.old_text.clone())
    }

    /// Returns the combined stored byte length of `old_text` and `new_text`.
    #[must_use]
    pub fn byte_cost(&self) -> usize {
        self.old_text.len().saturating_add(self.new_text.len())
    }
}

/// An atomic batch of non-overlapping [`ReversibleEdit`]s applied together in a single frame.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EditBatch {
    edits: Vec<ReversibleEdit>,
}

impl EditBatch {
    /// Builds a validated [`EditBatch`] from pre-edit `snapshot` and `edits`.
    ///
    /// Validation and sorting match [`Buffer::apply_edits`]. Adjacent pure deletions are
    /// coalesced into a single [`ReversibleEdit`] so `inverse_edits()` never produces
    /// duplicate empty insertions at the same byte offset.
    ///
    /// # Errors
    ///
    /// Returns [`BufferError`] if any edit range is out of bounds, splits a UTF-8 char or
    /// `\r\n`, or collides with another edit in the batch.
    pub fn build(snapshot: &BufferSnapshot, edits: &[TextEdit]) -> Result<Self, BufferError> {
        build_edit_batch(snapshot, edits)
    }

    /// Creates an [`EditBatch`] directly from pre-built reversible edits.
    #[must_use]
    pub fn from_reversible(edits: Vec<ReversibleEdit>) -> Self {
        Self { edits }
    }

    /// Returns the reversible edits in ascending document order.
    #[must_use]
    pub fn edits(&self) -> &[ReversibleEdit] {
        &self.edits
    }

    /// Returns the number of reversible edits in this batch.
    #[must_use]
    pub fn len(&self) -> usize {
        self.edits.len()
    }

    /// Returns `true` if this batch contains no effective edits.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.edits.is_empty()
    }

    /// Converts all edits in this batch into forward [`TextEdit`]s.
    #[must_use]
    pub fn forward_edits(&self) -> Vec<TextEdit> {
        self.edits
            .iter()
            .map(ReversibleEdit::forward_edit)
            .collect()
    }

    /// Converts all edits in this batch into inverse [`TextEdit`]s for `undo`.
    #[must_use]
    pub fn inverse_edits(&self) -> Vec<TextEdit> {
        self.edits
            .iter()
            .map(ReversibleEdit::inverse_edit)
            .collect()
    }

    /// Returns the total byte cost of all edits in this batch.
    #[must_use]
    pub fn byte_cost(&self) -> usize {
        self.edits
            .iter()
            .fold(0usize, |acc, edit| acc.saturating_add(edit.byte_cost()))
    }
}

/// Validates and converts pre-edit [`TextEdit`]s into an [`EditBatch`].
fn build_edit_batch(
    pre_snapshot: &BufferSnapshot,
    edits: &[TextEdit],
) -> Result<EditBatch, BufferError> {
    if edits.is_empty() {
        return Ok(EditBatch::default());
    }

    let len = pre_snapshot.len_bytes();
    let mut indexed: Vec<(usize, &TextEdit)> = edits.iter().enumerate().collect();

    for &(_, edit) in &indexed {
        if edit.range.start > edit.range.end || edit.range.end > len {
            return Err(BufferError::InvalidByteRange {
                start: edit.range.start,
                end: edit.range.end,
                len,
            });
        }
        // Check char and CRLF boundaries via slice_bytes validation semantics.
        let _ = pre_snapshot.slice_bytes(edit.range.clone())?;
    }

    indexed.sort_by(|a, b| {
        a.1.range
            .start
            .cmp(&b.1.range.start)
            .then_with(|| a.1.range.end.cmp(&b.1.range.end))
            .then_with(|| a.0.cmp(&b.0))
    });

    for pair in indexed.windows(2) {
        let prev = &pair[0].1.range;
        let next = &pair[1].1.range;
        let same_empty_point = prev.is_empty() && next.is_empty() && prev.start == next.start;
        if next.start < prev.end || same_empty_point {
            return Err(BufferError::OverlappingEdits {
                first_end: prev.end,
                second_start: next.start,
            });
        }
    }

    let mut reversible: Vec<ReversibleEdit> = Vec::with_capacity(indexed.len());
    let mut cumulative_delta: i128 = 0;

    for (_, edit) in indexed {
        if edit.range.is_empty() && edit.replacement.is_empty() {
            continue;
        }

        let old_text = pre_snapshot.slice_bytes(edit.range.clone())?.to_string();
        let before_start = i128::try_from(edit.range.start).unwrap_or(i128::MAX);
        let after_start = usize::try_from(before_start.saturating_add(cumulative_delta).max(0))
            .unwrap_or(usize::MAX);
        let after_end = after_start.saturating_add(edit.replacement.len());

        let old_len = i128::try_from(edit.range.len()).unwrap_or(i128::MAX);
        let new_len = i128::try_from(edit.replacement.len()).unwrap_or(i128::MAX);
        cumulative_delta = cumulative_delta.saturating_add(new_len - old_len);

        // Merge adjacent pure deletions so their inverse insertions do not collapse
        // onto the same empty offset and trigger `BufferError::OverlappingEdits`.
        if let Some(prev) = reversible.last_mut()
            && prev.new_text.is_empty()
            && edit.replacement.is_empty()
            && prev.before_range.end == edit.range.start
        {
            prev.before_range.end = edit.range.end;
            prev.old_text.push_str(&old_text);
            continue;
        }

        reversible.push(ReversibleEdit {
            before_range: edit.range.clone(),
            after_range: after_start..after_end,
            old_text,
            new_text: edit.replacement.clone(),
        });
    }

    Ok(EditBatch::from_reversible(reversible))
}

/// A node in the [`History`] undo/redo tree representing one user transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryNode {
    id: NodeId,
    transaction_id: TransactionId,
    parent: Option<NodeId>,
    children: Vec<NodeId>,
    active_child: Option<usize>,
    origin: EditOrigin,
    timestamp: Option<Instant>,
    sealed: bool,
    steps: Vec<EditBatch>,
    selections_before: SelectionSet,
    selections_after: SelectionSet,
    byte_cost: usize,
}

impl HistoryNode {
    /// Returns this node's [`NodeId`].
    #[must_use]
    pub const fn id(&self) -> NodeId {
        self.id
    }

    /// Returns this node's [`TransactionId`] (`0` for the baseline root).
    #[must_use]
    pub const fn transaction_id(&self) -> TransactionId {
        self.transaction_id
    }

    /// Returns the parent [`NodeId`], or `None` if this is the root node.
    #[must_use]
    pub const fn parent(&self) -> Option<NodeId> {
        self.parent
    }

    /// Returns the ordered child branches created from this node.
    #[must_use]
    pub fn children(&self) -> &[NodeId] {
        &self.children
    }

    /// Returns the index in [`Self::children`] followed by default `redo()`.
    #[must_use]
    pub const fn active_child(&self) -> Option<usize> {
        self.active_child
    }

    /// Returns the [`EditOrigin`] associated with this transaction.
    #[must_use]
    pub const fn origin(&self) -> EditOrigin {
        self.origin
    }

    /// Returns the timestamp of the most recent edit merged into this node, if any.
    #[must_use]
    pub const fn timestamp(&self) -> Option<Instant> {
        self.timestamp
    }

    /// Returns `true` if this node has been sealed against further coalescing.
    #[must_use]
    pub const fn is_sealed(&self) -> bool {
        self.sealed
    }

    /// Returns the atomic [`EditBatch`] steps stored in this transaction.
    #[must_use]
    pub fn steps(&self) -> &[EditBatch] {
        &self.steps
    }

    /// Returns the multi-cursor [`SelectionSet`] captured immediately before this transaction.
    #[must_use]
    pub const fn selections_before(&self) -> &SelectionSet {
        &self.selections_before
    }

    /// Returns the multi-cursor [`SelectionSet`] captured immediately after this transaction.
    #[must_use]
    pub const fn selections_after(&self) -> &SelectionSet {
        &self.selections_after
    }

    /// Returns the stored text byte cost of this node.
    #[must_use]
    pub const fn byte_cost(&self) -> usize {
        self.byte_cost
    }
}

#[derive(Debug, Clone)]
struct OpenGroup {
    origin: EditOrigin,
    selections_before: SelectionSet,
    started_at: Instant,
    node_id: Option<NodeId>,
}

/// Non-linear Undo/Redo tree preserving all branches, multi-cursor selections,
/// time-based coalescing groups, explicit compound groups, and save-point state.
#[derive(Debug, Clone)]
pub struct History {
    config: HistoryConfig,
    nodes: HashMap<NodeId, HistoryNode>,
    root: NodeId,
    current: NodeId,
    saved_node: Option<NodeId>,
    next_node_id: usize,
    next_transaction_id: TransactionId,
    total_stored_bytes: usize,
    group_depth: usize,
    active_group: Option<OpenGroup>,
}

impl Default for History {
    fn default() -> Self {
        Self::new(HistoryConfig::default())
    }
}

impl History {
    /// Creates a new [`History`] tree with baseline root `NodeId(0)`.
    #[must_use]
    pub fn new(config: HistoryConfig) -> Self {
        let root_node = HistoryNode {
            id: NodeId::ROOT,
            transaction_id: TransactionId::ZERO,
            parent: None,
            children: Vec::new(),
            active_child: None,
            origin: EditOrigin::Other,
            timestamp: None,
            sealed: true,
            steps: Vec::new(),
            selections_before: SelectionSet::caret(0),
            selections_after: SelectionSet::caret(0),
            byte_cost: 0,
        };
        let mut nodes = HashMap::new();
        nodes.insert(NodeId::ROOT, root_node);
        Self {
            config,
            nodes,
            root: NodeId::ROOT,
            current: NodeId::ROOT,
            saved_node: Some(NodeId::ROOT),
            next_node_id: 1,
            next_transaction_id: TransactionId::new(1),
            total_stored_bytes: 0,
            group_depth: 0,
            active_group: None,
        }
    }

    /// Returns the active [`HistoryConfig`].
    #[must_use]
    pub const fn config(&self) -> HistoryConfig {
        self.config
    }

    /// Returns the current root [`NodeId`] (initially [`NodeId::ROOT`]).
    #[must_use]
    pub const fn root_id(&self) -> NodeId {
        self.root
    }

    /// Returns the [`NodeId`] representing the current state of the buffer.
    #[must_use]
    pub const fn current_id(&self) -> NodeId {
        self.current
    }

    /// Returns a reference to the current [`HistoryNode`].
    #[must_use]
    pub fn current_node(&self) -> Option<&HistoryNode> {
        self.nodes.get(&self.current)
    }

    /// Looks up any [`HistoryNode`] currently retained in the tree by `id`.
    #[must_use]
    pub fn node(&self, id: NodeId) -> Option<&HistoryNode> {
        self.nodes.get(&id)
    }

    /// Returns the total number of nodes currently stored in the tree (including root).
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Returns the total byte cost of stored `old_text` and `new_text` across all nodes.
    #[must_use]
    pub const fn stored_bytes(&self) -> usize {
        self.total_stored_bytes
    }

    /// Returns `true` if `undo()` can step backward from the current node.
    #[must_use]
    pub fn can_undo(&self) -> bool {
        self.current != self.root
    }

    /// Returns `true` if `redo()` can step forward along `active_child`.
    #[must_use]
    pub fn can_redo(&self) -> bool {
        self.nodes
            .get(&self.current)
            .is_some_and(|node| node.active_child.is_some())
    }

    /// Returns the number of `undo()` steps from `current` back to the root.
    #[must_use]
    pub fn undo_count(&self) -> usize {
        let mut count = 0;
        let mut cursor = self.current;
        while let Some(node) = self.nodes.get(&cursor) {
            let Some(parent) = node.parent else {
                break;
            };
            count += 1;
            cursor = parent;
        }
        count
    }

    /// Returns the number of `redo()` steps downward along `active_child` from `current`.
    #[must_use]
    pub fn redo_count(&self) -> usize {
        let mut count = 0;
        let mut cursor = self.current;
        while let Some(node) = self.nodes.get(&cursor) {
            let Some(active_idx) = node.active_child else {
                break;
            };
            let Some(&child_id) = node.children.get(active_idx) else {
                break;
            };
            count += 1;
            cursor = child_id;
        }
        count
    }

    /// Returns the number of child branches emerging from `current`.
    #[must_use]
    pub fn branches_at_current(&self) -> usize {
        self.nodes
            .get(&self.current)
            .map_or(0, |node| node.children.len())
    }

    /// Returns the current nesting depth of explicit `begin_group` / `end_group` calls.
    #[must_use]
    pub const fn group_depth(&self) -> usize {
        self.group_depth
    }

    /// Returns the [`NodeId`] marked as the last saved state, if still in the tree.
    #[must_use]
    pub const fn saved_node(&self) -> Option<NodeId> {
        self.saved_node
    }

    /// Marks the current node as the clean on-disk save point.
    pub const fn mark_saved(&mut self) {
        self.saved_node = Some(self.current);
    }

    /// Returns `true` if the current buffer state differs from the recorded save point.
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.saved_node != Some(self.current)
    }

    /// Seals the current transaction node so subsequent edits always start a new node.
    pub fn break_group(&mut self) {
        if let Some(node) = self.nodes.get_mut(&self.current) {
            node.sealed = true;
        }
    }

    /// Opens an explicit compound edit group. Nested calls increment `group_depth`.
    ///
    /// No tree node is created until an effective edit is recorded inside the group, so
    /// empty `begin_group`/`end_group` pairs leave the tree clean.
    pub fn begin_group(
        &mut self,
        origin: EditOrigin,
        selections_before: &SelectionSet,
        now: Instant,
    ) {
        if self.group_depth == 0 {
            self.break_group();
            self.active_group = Some(OpenGroup {
                origin,
                selections_before: selections_before.clone(),
                started_at: now,
                node_id: None,
            });
        }
        self.group_depth += 1;
    }

    /// Closes one level of an explicit edit group.
    ///
    /// When the outermost group closes (`group_depth` returns to `0`), the created node
    /// (if any effective edits occurred) has its `selections_after` updated, is sealed,
    /// and its [`NodeId`] is returned. Empty groups return `Ok(None)`.
    ///
    /// # Errors
    ///
    /// Returns [`HistoryError::UnmatchedEndGroup`] if no group is currently open.
    pub fn end_group(
        &mut self,
        selections_after: &SelectionSet,
    ) -> Result<Option<NodeId>, HistoryError> {
        if self.group_depth == 0 {
            return Err(HistoryError::UnmatchedEndGroup);
        }
        self.group_depth -= 1;
        if self.group_depth > 0 {
            return Ok(None);
        }

        let committed_id = self.active_group.take().and_then(|group| group.node_id);
        if let Some(node_id) = committed_id {
            if let Some(node) = self.nodes.get_mut(&node_id) {
                node.selections_after = selections_after.clone();
                node.sealed = true;
            }
            self.enforce_limits();
        }
        Ok(committed_id)
    }

    /// Atomically validates `edits`, applies them to `buffer`, maps `selections` through
    /// the edits, and records the transaction in the history tree.
    ///
    /// Empty or pure no-op edits validate bounds and return `Ok(buffer.version())` without
    /// creating empty history nodes or breaking an active group.
    ///
    /// # Errors
    ///
    /// Returns [`HistoryError::Buffer`] without modifying `buffer`, `selections`, or `self`
    /// if any edit range is invalid or overlapping.
    pub fn record_and_apply(
        &mut self,
        buffer: &mut Buffer,
        selections: &mut SelectionSet,
        edits: &[TextEdit],
        origin: EditOrigin,
        now: Instant,
    ) -> Result<BufferVersion, HistoryError> {
        let selections_before = selections.clone();
        let pre_snap = buffer.snapshot();
        let batch = build_edit_batch(&pre_snap, edits)?;

        if batch.is_empty() {
            return Ok(buffer.version());
        }

        let version = buffer.apply_edits(edits)?;
        let post_snap = buffer.snapshot();
        selections.map_through_edits(edits, &post_snap);

        let _node_id = self.record_step_with_selections(
            batch,
            origin,
            selections_before,
            selections.clone(),
            now,
        );

        Ok(version)
    }

    /// Convenience wrapper around [`Self::record_and_apply`] using `Instant::now()`.
    ///
    /// # Errors
    ///
    /// Propagates any [`HistoryError`] from [`Self::record_and_apply`].
    pub fn apply_edit_now(
        &mut self,
        buffer: &mut Buffer,
        selections: &mut SelectionSet,
        edits: &[TextEdit],
        origin: EditOrigin,
    ) -> Result<BufferVersion, HistoryError> {
        self.record_and_apply(buffer, selections, edits, origin, Instant::now())
    }

    /// Records a pre-built [`EditBatch`] with explicit `selections_before` and `selections_after`.
    ///
    /// Intended for multi-cursor editing helpers (`apply_insert_all`, `apply_delete_backward`,
    /// clipboard operations) that compute custom post-edit cursor placements outside
    /// `SelectionSet::map_through_edits`.
    ///
    /// Returns `None` if `batch.is_empty()`.
    pub fn record_step_with_selections(
        &mut self,
        batch: EditBatch,
        origin: EditOrigin,
        selections_before: SelectionSet,
        selections_after: SelectionSet,
        now: Instant,
    ) -> Option<NodeId> {
        if batch.is_empty() {
            return None;
        }

        let batch_cost = batch.byte_cost();

        // Explicit group mode: funnel all non-empty batches into the active group node.
        if self.group_depth > 0
            && let Some(mut group) = self.active_group.take()
        {
            let node_id = if let Some(existing_id) = group.node_id {
                if let Some(node) = self.nodes.get_mut(&existing_id) {
                    node.steps.push(batch);
                    node.selections_after = selections_after;
                    node.timestamp = Some(now);
                    node.byte_cost = node.byte_cost.saturating_add(batch_cost);
                    self.total_stored_bytes = self.total_stored_bytes.saturating_add(batch_cost);
                }
                existing_id
            } else {
                let id = self.allocate_child_node(
                    group.origin,
                    Some(group.started_at.max(now)),
                    false,
                    batch,
                    batch_cost,
                    group.selections_before.clone(),
                    selections_after,
                );
                group.node_id = Some(id);
                id
            };
            self.active_group = Some(group);
            return Some(node_id);
        }

        // Non-group mode: check automatic coalescing against `self.current`.
        let should_coalesce = self
            .nodes
            .get(&self.current)
            .is_some_and(|cur| self.can_coalesce(cur, origin, &batch, &selections_before, now));

        if should_coalesce {
            if let Some(cur) = self.nodes.get_mut(&self.current) {
                cur.steps.push(batch);
                cur.selections_after = selections_after;
                cur.timestamp = Some(now);
                cur.byte_cost = cur.byte_cost.saturating_add(batch_cost);
                self.total_stored_bytes = self.total_stored_bytes.saturating_add(batch_cost);
            }
            self.enforce_limits();
            return Some(self.current);
        }

        // Otherwise seal previous node and append a new child branch.
        self.break_group();
        let seals_immediately = origin == EditOrigin::Typing && batch_contains_newline(&batch);
        let new_id = self.allocate_child_node(
            origin,
            Some(now),
            seals_immediately,
            batch,
            batch_cost,
            selections_before,
            selections_after,
        );
        self.enforce_limits();
        Some(new_id)
    }

    /// Undoes the active transaction at `current`, restoring `buffer` text and `selections`.
    ///
    /// Returns `Ok(false)` when already at the root node.
    ///
    /// # Errors
    ///
    /// Returns [`HistoryError::GroupStillOpen`] if an explicit group is still open, or
    /// [`HistoryError::Buffer`] if `buffer` cannot apply the inverse edits cleanly.
    pub fn undo(
        &mut self,
        buffer: &mut Buffer,
        selections: &mut SelectionSet,
    ) -> Result<bool, HistoryError> {
        if self.group_depth > 0 {
            return Err(HistoryError::GroupStillOpen);
        }

        let Some(node) = self.nodes.get(&self.current) else {
            return Ok(false);
        };
        let Some(parent_id) = node.parent else {
            return Ok(false);
        };

        let steps = node.steps.clone();
        let saved_before = node.selections_before.clone();

        apply_steps_inverse_atomically(buffer, &steps)?;

        let post_snap = buffer.snapshot();
        *selections = SelectionSet::from_selections(
            saved_before.as_slice().iter().copied(),
            saved_before.primary_index(),
            &post_snap,
        );
        self.current = parent_id;
        Ok(true)
    }

    /// Redoes the transaction along `current`'s `active_child`, restoring `buffer` and `selections`.
    ///
    /// Returns `Ok(false)` when `current` has no active child branch.
    ///
    /// # Errors
    ///
    /// Returns [`HistoryError::GroupStillOpen`] if an explicit group is still open, or
    /// [`HistoryError::Buffer`] if `buffer` cannot apply the forward edits cleanly.
    pub fn redo(
        &mut self,
        buffer: &mut Buffer,
        selections: &mut SelectionSet,
    ) -> Result<bool, HistoryError> {
        if self.group_depth > 0 {
            return Err(HistoryError::GroupStillOpen);
        }

        let Some(active_idx) = self.nodes.get(&self.current).and_then(|n| n.active_child) else {
            return Ok(false);
        };

        self.redo_branch(active_idx, buffer, selections)
    }

    /// Redoes a specific child branch `child_index` under `current` and sets it as `active_child`.
    ///
    /// # Errors
    ///
    /// Returns [`HistoryError::GroupStillOpen`] if an explicit group is open,
    /// [`HistoryError::InvalidBranchIndex`] if `child_index >= branches_at_current()`,
    /// or [`HistoryError::Buffer`] if applying the branch edits fails.
    pub fn redo_branch(
        &mut self,
        child_index: usize,
        buffer: &mut Buffer,
        selections: &mut SelectionSet,
    ) -> Result<bool, HistoryError> {
        if self.group_depth > 0 {
            return Err(HistoryError::GroupStillOpen);
        }

        let (child_id, available) = match self.nodes.get(&self.current) {
            Some(cur) => (cur.children.get(child_index).copied(), cur.children.len()),
            None => return Ok(false),
        };

        let Some(child_id) = child_id else {
            return Err(HistoryError::InvalidBranchIndex {
                index: child_index,
                available,
            });
        };

        let Some(child_node) = self.nodes.get(&child_id) else {
            return Ok(false);
        };

        let steps = child_node.steps.clone();
        let saved_after = child_node.selections_after.clone();

        apply_steps_forward_atomically(buffer, &steps)?;

        if let Some(cur) = self.nodes.get_mut(&self.current) {
            cur.active_child = Some(child_index);
        }

        let post_snap = buffer.snapshot();
        *selections = SelectionSet::from_selections(
            saved_after.as_slice().iter().copied(),
            saved_after.primary_index(),
            &post_snap,
        );
        self.current = child_id;
        Ok(true)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "internal node constructor grouping all fields cleanly"
    )]
    fn allocate_child_node(
        &mut self,
        origin: EditOrigin,
        timestamp: Option<Instant>,
        sealed: bool,
        first_batch: EditBatch,
        batch_cost: usize,
        selections_before: SelectionSet,
        selections_after: SelectionSet,
    ) -> NodeId {
        let id = NodeId::new(self.next_node_id);
        self.next_node_id += 1;

        let tx_id = self.next_transaction_id;
        self.next_transaction_id = tx_id.next();

        let parent_id = self.current;
        let node = HistoryNode {
            id,
            transaction_id: tx_id,
            parent: Some(parent_id),
            children: Vec::new(),
            active_child: None,
            origin,
            timestamp,
            sealed,
            steps: vec![first_batch],
            selections_before,
            selections_after,
            byte_cost: batch_cost,
        };

        self.nodes.insert(id, node);
        if let Some(parent) = self.nodes.get_mut(&parent_id) {
            parent.children.push(id);
            parent.active_child = Some(parent.children.len() - 1);
            parent.sealed = true;
        }
        self.current = id;
        self.total_stored_bytes = self.total_stored_bytes.saturating_add(batch_cost);
        id
    }

    fn can_coalesce(
        &self,
        current: &HistoryNode,
        origin: EditOrigin,
        batch: &EditBatch,
        selections_before: &SelectionSet,
        now: Instant,
    ) -> bool {
        if current.id == self.root
            || current.sealed
            || !current.children.is_empty()
            || self.saved_node == Some(current.id)
        {
            return false;
        }

        if !origin.is_coalescible() || current.origin != origin {
            return false;
        }

        let Some(prev_ts) = current.timestamp else {
            return false;
        };
        if now.saturating_duration_since(prev_ts) > self.config.grouping_window {
            return false;
        }

        if selections_before.len() != current.selections_after.len() {
            return false;
        }

        if origin == EditOrigin::Typing {
            if batch_contains_newline(batch) {
                return false;
            }
            if self.config.break_on_whitespace {
                let prev_ended_ws = current.steps.last().is_some_and(batch_ends_with_whitespace);
                let next_has_non_ws = batch_contains_non_whitespace(batch);
                if prev_ended_ws && next_has_non_ws {
                    return false;
                }
            }
        }

        true
    }

    fn enforce_limits(&mut self) {
        let max_nodes = self.config.max_nodes.max(1);
        let max_bytes = self.config.max_stored_bytes;

        while self.nodes.len() > max_nodes || self.total_stored_bytes > max_bytes {
            let active_path = self.collect_active_path();

            // 1. Prune oldest inactive side-branch leaf first (preserving saved_node if possible).
            if let Some(leaf_id) = self.find_prunable_leaf(&active_path) {
                self.remove_leaf(leaf_id);
                continue;
            }

            // 2. Otherwise advance `self.root` one step down toward `self.current`.
            if self.current == self.root {
                break;
            }
            if !self.advance_root() {
                break;
            }
        }
    }

    fn collect_active_path(&self) -> HashSet<NodeId> {
        let mut path = HashSet::new();
        let mut cursor = Some(self.current);
        while let Some(id) = cursor {
            path.insert(id);
            cursor = self.nodes.get(&id).and_then(|node| node.parent);
        }
        path
    }

    fn find_prunable_leaf(&self, active_path: &HashSet<NodeId>) -> Option<NodeId> {
        self.nodes
            .values()
            .filter(|node| node.children.is_empty() && !active_path.contains(&node.id))
            .min_by_key(|node| {
                let is_saved = self.saved_node == Some(node.id);
                (is_saved, node.transaction_id, node.id)
            })
            .map(|node| node.id)
    }

    fn remove_leaf(&mut self, leaf_id: NodeId) {
        let Some(leaf) = self.nodes.remove(&leaf_id) else {
            return;
        };
        self.total_stored_bytes = self.total_stored_bytes.saturating_sub(leaf.byte_cost);
        if self.saved_node == Some(leaf_id) {
            self.saved_node = None;
        }

        if let Some(parent_id) = leaf.parent
            && let Some(parent) = self.nodes.get_mut(&parent_id)
            && let Some(pos) = parent.children.iter().position(|&id| id == leaf_id)
        {
            parent.children.remove(pos);
            parent.active_child = match parent.active_child {
                Some(_) if parent.children.is_empty() => None,
                Some(active_idx) if active_idx == pos => Some(parent.children.len() - 1),
                Some(active_idx) if active_idx > pos => Some(active_idx - 1),
                other => other,
            };
        }
    }

    fn advance_root(&mut self) -> bool {
        let Some(next_root_id) = self
            .nodes
            .get(&self.root)
            .and_then(|root| root.children.first().copied())
        else {
            return false;
        };

        let old_root_id = self.root;
        let Some(old_root) = self.nodes.remove(&old_root_id) else {
            return false;
        };
        self.total_stored_bytes = self.total_stored_bytes.saturating_sub(old_root.byte_cost);
        if self.saved_node == Some(old_root_id) {
            self.saved_node = None;
        }

        if let Some(next_root) = self.nodes.get_mut(&next_root_id) {
            self.total_stored_bytes = self.total_stored_bytes.saturating_sub(next_root.byte_cost);
            next_root.parent = None;
            next_root.steps.clear();
            next_root.byte_cost = 0;
            next_root.sealed = true;
        }
        self.root = next_root_id;
        true
    }
}

fn apply_steps_inverse_atomically(
    buffer: &mut Buffer,
    steps: &[EditBatch],
) -> Result<(), BufferError> {
    for (applied_count, batch) in steps.iter().rev().enumerate() {
        let inv = batch.inverse_edits();
        if let Err(err) = buffer.apply_edits(&inv) {
            // Roll back any steps already undone in this transaction so `buffer` stays consistent.
            let start_idx = steps.len() - applied_count;
            for rollback_batch in &steps[start_idx..] {
                let _ = buffer.apply_edits(&rollback_batch.forward_edits());
            }
            return Err(err);
        }
    }
    Ok(())
}

fn apply_steps_forward_atomically(
    buffer: &mut Buffer,
    steps: &[EditBatch],
) -> Result<(), BufferError> {
    for (applied_count, batch) in steps.iter().enumerate() {
        let fwd = batch.forward_edits();
        if let Err(err) = buffer.apply_edits(&fwd) {
            // Roll back any steps already redone in this transaction.
            for rollback_batch in steps[..applied_count].iter().rev() {
                let _ = buffer.apply_edits(&rollback_batch.inverse_edits());
            }
            return Err(err);
        }
    }
    Ok(())
}

fn batch_contains_newline(batch: &EditBatch) -> bool {
    batch
        .edits()
        .iter()
        .any(|edit| edit.new_text.contains(['\n', '\r']))
}

fn batch_ends_with_whitespace(batch: &EditBatch) -> bool {
    batch
        .edits()
        .last()
        .and_then(|edit| edit.new_text.chars().last())
        .is_some_and(char::is_whitespace)
}

fn batch_contains_non_whitespace(batch: &EditBatch) -> bool {
    batch
        .edits()
        .iter()
        .any(|edit| edit.new_text.chars().any(|ch| !ch.is_whitespace()))
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests assert invariants directly"
)]
mod tests {
    use super::*;
    use crate::Selection;

    #[test]
    fn coalesces_within_window_and_splits_on_whitespace_and_timeout() {
        let t0 = Instant::now();
        let mut buf = Buffer::new();
        let mut sels = SelectionSet::caret(0);
        let mut history = History::new(HistoryConfig::default());

        // Type "ab" within 500ms -> single node
        history
            .record_and_apply(
                &mut buf,
                &mut sels,
                &[TextEdit::insert(0, "a")],
                EditOrigin::Typing,
                t0,
            )
            .unwrap();
        history
            .record_and_apply(
                &mut buf,
                &mut sels,
                &[TextEdit::insert(1, "b")],
                EditOrigin::Typing,
                t0 + Duration::from_millis(120),
            )
            .unwrap();
        assert_eq!(history.undo_count(), 1);

        // Trailing space remains in the same word group
        history
            .record_and_apply(
                &mut buf,
                &mut sels,
                &[TextEdit::insert(2, " ")],
                EditOrigin::Typing,
                t0 + Duration::from_millis(200),
            )
            .unwrap();
        assert_eq!(history.undo_count(), 1);

        // Non-whitespace after trailing space starts a new node
        history
            .record_and_apply(
                &mut buf,
                &mut sels,
                &[TextEdit::insert(3, "c")],
                EditOrigin::Typing,
                t0 + Duration::from_millis(250),
            )
            .unwrap();
        assert_eq!(history.undo_count(), 2);

        // Exceeding 500ms grouping window starts a new node
        history
            .record_and_apply(
                &mut buf,
                &mut sels,
                &[TextEdit::insert(4, "d")],
                EditOrigin::Typing,
                t0 + Duration::from_millis(900),
            )
            .unwrap();
        assert_eq!(history.undo_count(), 3);
        assert_eq!(buf.snapshot().text(), "ab cd");

        while history.undo(&mut buf, &mut sels).unwrap() {}
        assert_eq!(buf.snapshot().text(), "");
        assert_eq!(sels, SelectionSet::caret(0));
    }

    #[test]
    fn splits_on_newline_origin_changes_and_paste() {
        let t0 = Instant::now();
        let mut buf = Buffer::from_text("ab");
        let mut sels = SelectionSet::caret(2);
        let mut history = History::new(HistoryConfig::default());

        // Newline starts its own sealed node
        history
            .record_and_apply(
                &mut buf,
                &mut sels,
                &[TextEdit::insert(2, "\n")],
                EditOrigin::Typing,
                t0,
            )
            .unwrap();
        history
            .record_and_apply(
                &mut buf,
                &mut sels,
                &[TextEdit::insert(3, "e")],
                EditOrigin::Typing,
                t0 + Duration::from_millis(10),
            )
            .unwrap();
        assert_eq!(history.undo_count(), 2);

        // Changing origin breaks coalescing
        history
            .record_and_apply(
                &mut buf,
                &mut sels,
                &[TextEdit::delete(3..4)],
                EditOrigin::DeleteBackward,
                t0 + Duration::from_millis(20),
            )
            .unwrap();
        assert_eq!(history.undo_count(), 3);

        // Paste never coalesces with another paste
        history
            .record_and_apply(
                &mut buf,
                &mut sels,
                &[TextEdit::insert(3, "x")],
                EditOrigin::Paste,
                t0 + Duration::from_millis(25),
            )
            .unwrap();
        history
            .record_and_apply(
                &mut buf,
                &mut sels,
                &[TextEdit::insert(4, "y")],
                EditOrigin::Paste,
                t0 + Duration::from_millis(26),
            )
            .unwrap();
        assert_eq!(history.undo_count(), 5);
        assert_eq!(buf.snapshot().text(), "ab\nxy");
    }

    #[test]
    fn adjacent_deletions_in_same_batch_invert_cleanly() {
        let t0 = Instant::now();
        let mut buf = Buffer::from_text("abcdef");
        let mut sels = SelectionSet::from_selections(
            [Selection::new(0, 2), Selection::new(2, 4)],
            1,
            &buf.snapshot(),
        );
        let mut history = History::new(HistoryConfig::default());

        history
            .record_and_apply(
                &mut buf,
                &mut sels,
                &[TextEdit::delete(0..2), TextEdit::delete(2..4)],
                EditOrigin::DeleteBackward,
                t0,
            )
            .unwrap();
        assert_eq!(buf.snapshot().text(), "ef");

        assert!(history.undo(&mut buf, &mut sels).unwrap());
        assert_eq!(buf.snapshot().text(), "abcdef");
        assert_eq!(
            sels.as_slice(),
            &[Selection::new(0, 2), Selection::new(2, 4)]
        );
        assert_eq!(sels.primary_index(), 1);
    }

    #[test]
    fn pruning_enforces_max_nodes_and_max_stored_bytes_safely() {
        let t0 = Instant::now();
        let mut buf = Buffer::new();
        let mut sels = SelectionSet::caret(0);
        let mut history = History::new(HistoryConfig {
            grouping_window: Duration::from_millis(500),
            max_nodes: 4,
            max_stored_bytes: 64,
            break_on_whitespace: true,
        });

        for i in 0..8usize {
            let pos = buf.snapshot().len_bytes();
            let text = format!("{i}");
            history
                .record_and_apply(
                    &mut buf,
                    &mut sels,
                    &[TextEdit::insert(pos, text)],
                    EditOrigin::Paste,
                    t0,
                )
                .unwrap();
            if i == 1 {
                history.mark_saved();
            }
        }

        assert!(history.node_count() <= 4);
        assert!(history.stored_bytes() <= 64);
        // Save point at step 1 was pruned off the root, so dirty must be true.
        assert_eq!(history.saved_node(), None);
        assert!(history.is_dirty());

        // Remaining undo steps still work without errors.
        let mut undone = 0;
        while history.undo(&mut buf, &mut sels).unwrap() {
            undone += 1;
        }
        assert_eq!(undone, 3);
        assert_eq!(buf.snapshot().text(), "01234");
    }
}
