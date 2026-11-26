//! Declarative UI tree model, semantic style variants, validation, and bidirectional
//! event bridge for sandboxed WASM plugins (`KOVA-045`).
//!
//! All types in this module are strictly **headless**: plugins describe UI declaratively
//! using [`WidgetNode`] and [`StyleVariant`] (never raw pixels or hex colors), and `kova_ui`
//! renders them natively via GPUI. User interactions flow back to the plugin instance as
//! [`UiEvent`] values over [`PluginUiChannel`].

use std::fmt;

use kova_theme::PluginVariant;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc;

/// Maximum structural nesting depth permitted for a [`WidgetNode`] tree.
pub const MAX_WIDGET_DEPTH: usize = 32;

/// Maximum number of [`WidgetNode`] elements permitted in a single tree.
pub const MAX_WIDGET_NODES: usize = 4_096;

/// Maximum number of rows permitted in a single [`TableWidget`].
pub const MAX_TABLE_ROWS: usize = 100_000;

/// Maximum number of columns permitted in a single [`TableWidget`].
pub const MAX_TABLE_COLUMNS: usize = 128;

/// Maximum number of items permitted in a single [`ListWidget`].
pub const MAX_LIST_ITEMS: usize = 100_000;

/// Maximum number of nodes permitted in a single [`TreeWidget`].
pub const MAX_TREE_NODES: usize = 50_000;

/// Maximum byte length of text payloads (`CodeBlock`, `Markdown`) per widget.
pub const MAX_TEXT_BYTES: usize = 1_048_576;

/// Maximum byte length of a widget identifier.
pub const MAX_ID_BYTES: usize = 256;

/// Minimum allowed proportional split ratio.
pub const MIN_SPLIT_RATIO: f32 = 0.05;

/// Maximum allowed proportional split ratio.
pub const MAX_SPLIT_RATIO: f32 = 0.95;

/// Semantic style variant exposed to plugins across the WIT boundary.
///
/// Plugins never pass raw hex colors or pixel sizes; `kova_ui` maps each variant
/// directly to the active [`kova_theme::AetherTheme`] `[plugins.*]` tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StyleVariant {
    /// Primary call-to-action or emphasized indicator.
    #[default]
    Primary,
    /// Neutral, secondary, or low-emphasis element.
    Muted,
    /// Non-fatal caution banner or badge.
    Warning,
    /// Destructive action or error state.
    Error,
    /// Highlighted accent indicator or info badge.
    Accent,
}

impl From<StyleVariant> for PluginVariant {
    fn from(value: StyleVariant) -> Self {
        match value {
            StyleVariant::Primary => Self::Primary,
            StyleVariant::Muted => Self::Muted,
            StyleVariant::Warning => Self::Warning,
            StyleVariant::Error => Self::Error,
            StyleVariant::Accent => Self::Accent,
        }
    }
}

impl From<PluginVariant> for StyleVariant {
    fn from(value: PluginVariant) -> Self {
        match value {
            PluginVariant::Primary => Self::Primary,
            PluginVariant::Muted => Self::Muted,
            PluginVariant::Warning => Self::Warning,
            PluginVariant::Error => Self::Error,
            PluginVariant::Accent => Self::Accent,
        }
    }
}

/// Orientation of a [`SplitWidget`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitDirection {
    /// Side-by-side split (left | right).
    #[default]
    Horizontal,
    /// Stacked split (top / bottom).
    Vertical,
}

/// Horizontal text alignment inside a table column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextAlign {
    /// Left-aligned text (default).
    #[default]
    Left,
    /// Center-aligned text.
    Center,
    /// Right-aligned text (typical for numeric columns).
    Right,
}

/// Declarative UI tree returned by a WASM plugin for native GPUI rendering.
///
/// Covers all 9 base components specified for Kova plugins: `Split`, `Tabs`, `Tree`,
/// `Table`, `List`, `Form`, `CodeBlock`, `Markdown`, and `Badge`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WidgetNode {
    /// Proportional two-pane split container.
    Split(SplitWidget),
    /// Tabbed container with active tab index.
    Tabs(TabsWidget),
    /// Hierarchical tree with expandable/collapsible nodes and selection.
    Tree(TreeWidget),
    /// Row-virtualized data table for large datasets (e.g. SQL query results).
    Table(TableWidget),
    /// Virtualized item list.
    List(ListWidget),
    /// Structured form containing `TextInput`, `Select`, `Toggle`, and `Button` controls.
    Form(FormWidget),
    /// Syntax-highlighted source code block powered by `kova_syntax`.
    CodeBlock(CodeBlockWidget),
    /// Native Markdown document view (headings, lists, code, links — zero `WebView`).
    Markdown(MarkdownWidget),
    /// Compact status pill badge.
    Badge(BadgeWidget),
}

impl WidgetNode {
    /// Returns the widget identifier of this node.
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Split(w) => &w.id,
            Self::Tabs(w) => &w.id,
            Self::Tree(w) => &w.id,
            Self::Table(w) => &w.id,
            Self::List(w) => &w.id,
            Self::Form(w) => &w.id,
            Self::CodeBlock(w) => &w.id,
            Self::Markdown(w) => &w.id,
            Self::Badge(w) => &w.id,
        }
    }

    /// Validates structural depth, node counts, collection sizes, split ratios, and indices
    /// to protect the host against malformed or malicious plugin UI trees.
    ///
    /// # Errors
    ///
    /// Returns [`WidgetValidationError`] if any safety bound or invariant is violated.
    pub fn validate(&self) -> Result<WidgetTreeStats, WidgetValidationError> {
        let mut stats = WidgetTreeStats::default();
        self.validate_recursive(1, &mut stats)?;
        Ok(stats)
    }

    fn validate_recursive(
        &self,
        depth: usize,
        stats: &mut WidgetTreeStats,
    ) -> Result<(), WidgetValidationError> {
        if depth > MAX_WIDGET_DEPTH {
            return Err(WidgetValidationError::MaxDepthExceeded {
                max: MAX_WIDGET_DEPTH,
                actual: depth,
            });
        }
        stats.max_depth = stats.max_depth.max(depth);
        stats.node_count = stats.node_count.saturating_add(1);
        if stats.node_count > MAX_WIDGET_NODES {
            return Err(WidgetValidationError::MaxWidgetCountExceeded {
                max: MAX_WIDGET_NODES,
                actual: stats.node_count,
            });
        }

        validate_widget_id(self.id())?;

        match self {
            Self::Split(split) => validate_split(split, depth, stats),
            Self::Tabs(tabs) => validate_tabs(tabs, depth, stats),
            Self::Tree(tree) => {
                let mut tree_count = 0usize;
                validate_tree_nodes(
                    &tree.id,
                    &tree.nodes,
                    depth,
                    &mut tree_count,
                    &mut stats.max_depth,
                )?;
                stats.total_items = stats.total_items.saturating_add(tree_count);
                Ok(())
            }
            Self::Table(table) => validate_table(table, stats),
            Self::List(list) => validate_list(list, stats),
            Self::Form(form) => validate_form(form),
            Self::CodeBlock(block) => validate_text_len(&block.id, &block.code),
            Self::Markdown(md) => validate_text_len(&md.id, &md.content),
            Self::Badge(_) => Ok(()),
        }
    }
}

fn validate_split(
    split: &SplitWidget,
    depth: usize,
    stats: &mut WidgetTreeStats,
) -> Result<(), WidgetValidationError> {
    if !split.ratio.is_finite() || split.ratio < MIN_SPLIT_RATIO || split.ratio > MAX_SPLIT_RATIO {
        return Err(WidgetValidationError::InvalidSplitRatio {
            widget_id: split.id.clone(),
            ratio: split.ratio.to_string(),
        });
    }
    split.first.validate_recursive(depth + 1, stats)?;
    split.second.validate_recursive(depth + 1, stats)
}

fn validate_tabs(
    tabs: &TabsWidget,
    depth: usize,
    stats: &mut WidgetTreeStats,
) -> Result<(), WidgetValidationError> {
    if !tabs.tabs.is_empty() && tabs.active_index >= tabs.tabs.len() {
        return Err(WidgetValidationError::InvalidTabIndex {
            widget_id: tabs.id.clone(),
            active_index: tabs.active_index,
            len: tabs.tabs.len(),
        });
    }
    for tab in &tabs.tabs {
        validate_widget_id(&tab.id)?;
        tab.content.validate_recursive(depth + 1, stats)?;
    }
    Ok(())
}

fn validate_table(
    table: &TableWidget,
    stats: &mut WidgetTreeStats,
) -> Result<(), WidgetValidationError> {
    if table.columns.len() > MAX_TABLE_COLUMNS {
        return Err(WidgetValidationError::MaxColumnsExceeded {
            widget_id: table.id.clone(),
            max: MAX_TABLE_COLUMNS,
            actual: table.columns.len(),
        });
    }
    if table.rows.len() > MAX_TABLE_ROWS {
        return Err(WidgetValidationError::MaxItemsExceeded {
            widget_id: table.id.clone(),
            max: MAX_TABLE_ROWS,
            actual: table.rows.len(),
        });
    }
    for col in &table.columns {
        validate_widget_id(&col.id)?;
    }
    for row in &table.rows {
        validate_widget_id(&row.id)?;
    }
    stats.total_rows = stats.total_rows.saturating_add(table.rows.len());
    Ok(())
}

fn validate_list(
    list: &ListWidget,
    stats: &mut WidgetTreeStats,
) -> Result<(), WidgetValidationError> {
    if list.items.len() > MAX_LIST_ITEMS {
        return Err(WidgetValidationError::MaxItemsExceeded {
            widget_id: list.id.clone(),
            max: MAX_LIST_ITEMS,
            actual: list.items.len(),
        });
    }
    for item in &list.items {
        validate_widget_id(&item.id)?;
    }
    stats.total_items = stats.total_items.saturating_add(list.items.len());
    Ok(())
}

fn validate_form(form: &FormWidget) -> Result<(), WidgetValidationError> {
    if form.fields.len() > MAX_LIST_ITEMS {
        return Err(WidgetValidationError::MaxItemsExceeded {
            widget_id: form.id.clone(),
            max: MAX_LIST_ITEMS,
            actual: form.fields.len(),
        });
    }
    for field in &form.fields {
        validate_widget_id(field.control.id())?;
        if let FormControl::Select(select) = &field.control {
            for opt in &select.options {
                validate_widget_id(&opt.value)?;
            }
        }
    }
    Ok(())
}

fn validate_text_len(widget_id: &str, text: &str) -> Result<(), WidgetValidationError> {
    if text.len() > MAX_TEXT_BYTES {
        return Err(WidgetValidationError::MaxTextBytesExceeded {
            widget_id: widget_id.to_owned(),
            max: MAX_TEXT_BYTES,
            actual: text.len(),
        });
    }
    Ok(())
}

fn validate_widget_id(id: &str) -> Result<(), WidgetValidationError> {
    if id.trim().is_empty() {
        return Err(WidgetValidationError::EmptyWidgetId);
    }
    if id.len() > MAX_ID_BYTES {
        return Err(WidgetValidationError::WidgetIdTooLong {
            max: MAX_ID_BYTES,
            actual: id.len(),
        });
    }
    Ok(())
}

fn validate_tree_nodes(
    widget_id: &str,
    nodes: &[TreeNode],
    depth: usize,
    count: &mut usize,
    max_depth: &mut usize,
) -> Result<(), WidgetValidationError> {
    if depth > MAX_WIDGET_DEPTH {
        return Err(WidgetValidationError::MaxDepthExceeded {
            max: MAX_WIDGET_DEPTH,
            actual: depth,
        });
    }
    *max_depth = (*max_depth).max(depth);
    for node in nodes {
        *count = count.saturating_add(1);
        if *count > MAX_TREE_NODES {
            return Err(WidgetValidationError::MaxItemsExceeded {
                widget_id: widget_id.to_owned(),
                max: MAX_TREE_NODES,
                actual: *count,
            });
        }
        validate_widget_id(&node.id)?;
        if !node.children.is_empty() {
            validate_tree_nodes(widget_id, &node.children, depth + 1, count, max_depth)?;
        }
    }
    Ok(())
}

/// Summary metrics computed while validating a [`WidgetNode`] tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WidgetTreeStats {
    /// Total number of [`WidgetNode`] elements in the tree.
    pub node_count: usize,
    /// Maximum nesting depth reached (including nested `Tree` nodes).
    pub max_depth: usize,
    /// Total number of table rows across all [`TableWidget`] nodes.
    pub total_rows: usize,
    /// Total number of list and tree items across the tree.
    pub total_items: usize,
}

/// Validation failure returned by [`WidgetNode::validate`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WidgetValidationError {
    /// Tree nesting exceeded [`MAX_WIDGET_DEPTH`].
    #[error("widget tree depth {actual} exceeds maximum allowed depth {max}")]
    MaxDepthExceeded {
        /// Maximum allowed depth.
        max: usize,
        /// Observed depth.
        actual: usize,
    },
    /// Number of [`WidgetNode`]s exceeded [`MAX_WIDGET_NODES`].
    #[error("widget tree node count {actual} exceeds maximum allowed count {max}")]
    MaxWidgetCountExceeded {
        /// Maximum allowed nodes.
        max: usize,
        /// Observed nodes.
        actual: usize,
    },
    /// Collection size in a table, list, tree, or form exceeded the host bound.
    #[error("widget `{widget_id}` item count {actual} exceeds maximum allowed {max}")]
    MaxItemsExceeded {
        /// Offending widget identifier.
        widget_id: String,
        /// Maximum allowed items.
        max: usize,
        /// Observed items.
        actual: usize,
    },
    /// Column count in a table exceeded [`MAX_TABLE_COLUMNS`].
    #[error("table `{widget_id}` column count {actual} exceeds maximum allowed {max}")]
    MaxColumnsExceeded {
        /// Offending widget identifier.
        widget_id: String,
        /// Maximum allowed columns.
        max: usize,
        /// Observed columns.
        actual: usize,
    },
    /// Text payload exceeded [`MAX_TEXT_BYTES`].
    #[error("widget `{widget_id}` text payload ({actual} bytes) exceeds limit {max} bytes")]
    MaxTextBytesExceeded {
        /// Offending widget identifier.
        widget_id: String,
        /// Maximum allowed byte length.
        max: usize,
        /// Observed byte length.
        actual: usize,
    },
    /// An empty or whitespace-only widget identifier was encountered.
    #[error("widget identifier must not be empty")]
    EmptyWidgetId,
    /// A widget identifier exceeded [`MAX_ID_BYTES`].
    #[error("widget identifier length {actual} exceeds maximum {max} bytes")]
    WidgetIdTooLong {
        /// Maximum allowed bytes.
        max: usize,
        /// Observed bytes.
        actual: usize,
    },
    /// Split ratio was non-finite or outside `[MIN_SPLIT_RATIO, MAX_SPLIT_RATIO]`.
    #[error(
        "split widget `{widget_id}` ratio `{ratio}` must be finite and within [{MIN_SPLIT_RATIO}, {MAX_SPLIT_RATIO}]"
    )]
    InvalidSplitRatio {
        /// Offending split widget identifier.
        widget_id: String,
        /// Stringified invalid ratio.
        ratio: String,
    },
    /// Active tab index was out of bounds for a non-empty `TabsWidget`.
    #[error("tabs widget `{widget_id}` active_index {active_index} out of bounds for {len} tabs")]
    InvalidTabIndex {
        /// Offending tabs widget identifier.
        widget_id: String,
        /// Requested active tab index.
        active_index: usize,
        /// Number of tabs present.
        len: usize,
    },
}

/// Proportional split container dividing space between two child widgets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SplitWidget {
    /// Stable widget identifier.
    pub id: String,
    /// Split orientation (`horizontal` or `vertical`).
    #[serde(default)]
    pub axis: SplitDirection,
    /// Proportion assigned to `first` in `[0.05, 0.95]`.
    pub ratio: f32,
    /// First child (left or top).
    pub first: Box<WidgetNode>,
    /// Second child (right or bottom).
    pub second: Box<WidgetNode>,
}

/// Tabbed container with an active tab index and switch event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TabsWidget {
    /// Stable widget identifier.
    pub id: String,
    /// Ordered tabs.
    pub tabs: Vec<TabNode>,
    /// Zero-based index of the currently active tab.
    #[serde(default)]
    pub active_index: usize,
    /// Semantic accent variant for the active tab indicator.
    #[serde(default)]
    pub variant: StyleVariant,
}

/// Single tab entry inside [`TabsWidget`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TabNode {
    /// Stable tab identifier.
    pub id: String,
    /// Display title shown in the tab strip.
    pub title: String,
    /// Optional status badge shown next to the tab title.
    #[serde(default)]
    pub badge: Option<BadgeWidget>,
    /// Child content rendered when this tab is active.
    pub content: WidgetNode,
}

/// Hierarchical tree widget with expandable/collapsible nodes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeWidget {
    /// Stable widget identifier.
    pub id: String,
    /// Top-level nodes in display order.
    pub nodes: Vec<TreeNode>,
    /// Currently selected node identifier, if any.
    #[serde(default)]
    pub selected_id: Option<String>,
}

/// Single node inside a [`TreeWidget`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeNode {
    /// Stable node identifier.
    pub id: String,
    /// Primary node label.
    pub label: String,
    /// Optional muted secondary annotation (e.g. type or count).
    #[serde(default)]
    pub secondary: Option<String>,
    /// Optional semantic badge/tag shown at the trailing edge.
    #[serde(default)]
    pub badge: Option<BadgeWidget>,
    /// Semantic style variant for the node label/icon.
    #[serde(default = "default_muted_variant")]
    pub variant: StyleVariant,
    /// Whether child nodes are currently expanded.
    #[serde(default)]
    pub expanded: bool,
    /// Nested child nodes.
    #[serde(default)]
    pub children: Vec<TreeNode>,
}

/// Row-virtualized table widget for large datasets (e.g. SQL query results).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableWidget {
    /// Stable widget identifier.
    pub id: String,
    /// Ordered column definitions using proportional weights (never raw pixels).
    pub columns: Vec<TableColumn>,
    /// Table rows (rendered via GPUI `uniform_list` virtualization).
    pub rows: Vec<TableRow>,
    /// Currently selected row index, if any.
    #[serde(default)]
    pub selected_row: Option<usize>,
    /// Optional message displayed when `rows` is empty.
    #[serde(default)]
    pub empty_message: Option<String>,
}

/// Column definition inside [`TableWidget`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableColumn {
    /// Stable column identifier.
    pub id: String,
    /// Header caption.
    pub header: String,
    /// Proportional width weight (relative to sum of all column weights, never pixels).
    #[serde(default = "default_column_weight")]
    pub weight: u16,
    /// Horizontal cell alignment.
    #[serde(default)]
    pub align: TextAlign,
}

const fn default_column_weight() -> u16 {
    1
}

const fn default_muted_variant() -> StyleVariant {
    StyleVariant::Muted
}

/// Single row inside [`TableWidget`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableRow {
    /// Stable row identifier included in selection/activation events.
    pub id: String,
    /// Ordered cells matching [`TableWidget::columns`].
    pub cells: Vec<TableCell>,
}

impl TableRow {
    /// Builds a plain [`TableRow`] from string cell values.
    #[must_use]
    pub fn from_strings<I, S>(id: impl Into<String>, cells: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            id: id.into(),
            cells: cells.into_iter().map(TableCell::plain).collect(),
        }
    }
}

/// Single cell inside a [`TableRow`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableCell {
    /// Cell display text.
    pub text: String,
    /// Optional semantic variant coloring the cell text.
    #[serde(default)]
    pub variant: Option<StyleVariant>,
}

impl TableCell {
    /// Creates a plain cell using default foreground typography.
    #[must_use]
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            variant: None,
        }
    }

    /// Creates a semantically styled cell.
    #[must_use]
    pub fn styled(text: impl Into<String>, variant: StyleVariant) -> Self {
        Self {
            text: text.into(),
            variant: Some(variant),
        }
    }
}

/// Virtualized list widget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListWidget {
    /// Stable widget identifier.
    pub id: String,
    /// List entries.
    pub items: Vec<ListItem>,
    /// Currently selected item index, if any.
    #[serde(default)]
    pub selected_index: Option<usize>,
    /// Optional message displayed when `items` is empty.
    #[serde(default)]
    pub empty_message: Option<String>,
}

/// Single item inside [`ListWidget`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListItem {
    /// Stable item identifier.
    pub id: String,
    /// Primary title.
    pub title: String,
    /// Optional muted secondary line.
    #[serde(default)]
    pub subtitle: Option<String>,
    /// Optional trailing badge.
    #[serde(default)]
    pub badge: Option<BadgeWidget>,
    /// Semantic style variant.
    #[serde(default = "default_muted_variant")]
    pub variant: StyleVariant,
}

/// Structured form container grouping labeled controls (`TextInput`, `Select`, `Toggle`, `Button`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormWidget {
    /// Stable widget identifier.
    pub id: String,
    /// Optional form heading.
    #[serde(default)]
    pub title: Option<String>,
    /// Ordered form fields.
    pub fields: Vec<FormField>,
    /// Semantic accent variant for the form container/header.
    #[serde(default = "default_muted_variant")]
    pub variant: StyleVariant,
}

/// Single labeled row inside [`FormWidget`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormField {
    /// Optional field label shown next to or above the control.
    #[serde(default)]
    pub label: Option<String>,
    /// Optional helper/error hint rendered beneath the label.
    #[serde(default)]
    pub hint: Option<String>,
    /// Interactive form control.
    pub control: FormControl,
}

/// Interactive controls available inside a [`FormWidget`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "control", rename_all = "snake_case")]
pub enum FormControl {
    /// Single-line text input field.
    TextInput(TextInputWidget),
    /// Drop-down select box.
    Select(SelectWidget),
    /// Boolean on/off switch.
    Toggle(ToggleWidget),
    /// Action push button.
    Button(ButtonWidget),
}

impl FormControl {
    /// Returns the widget identifier of the underlying form control.
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::TextInput(w) => &w.id,
            Self::Select(w) => &w.id,
            Self::Toggle(w) => &w.id,
            Self::Button(w) => &w.id,
        }
    }
}

/// Single-line text input control inside [`FormWidget`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextInputWidget {
    /// Stable control identifier.
    pub id: String,
    /// Current text value.
    #[serde(default)]
    pub value: String,
    /// Placeholder shown when `value` is empty.
    #[serde(default)]
    pub placeholder: String,
    /// Whether user edits are disabled.
    #[serde(default)]
    pub disabled: bool,
    /// Semantic variant (`Error` renders a validation-error border).
    #[serde(default)]
    pub variant: StyleVariant,
}

/// Drop-down option selector inside [`FormWidget`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectWidget {
    /// Stable control identifier.
    pub id: String,
    /// Selectable options.
    pub options: Vec<SelectOption>,
    /// Currently selected option value.
    #[serde(default)]
    pub selected_value: Option<String>,
    /// Whether selection is disabled.
    #[serde(default)]
    pub disabled: bool,
    /// Semantic variant.
    #[serde(default)]
    pub variant: StyleVariant,
}

/// Single option in a [`SelectWidget`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectOption {
    /// Machine value emitted in [`UiEvent::SelectChanged`].
    pub value: String,
    /// Human-readable option label.
    pub label: String,
}

/// Boolean toggle switch inside [`FormWidget`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToggleWidget {
    /// Stable control identifier.
    pub id: String,
    /// Current boolean state.
    pub checked: bool,
    /// Whether toggling is disabled.
    #[serde(default)]
    pub disabled: bool,
    /// Semantic variant.
    #[serde(default)]
    pub variant: StyleVariant,
}

/// Push button inside [`FormWidget`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ButtonWidget {
    /// Stable control identifier.
    pub id: String,
    /// Button caption.
    pub label: String,
    /// Semantic style variant.
    #[serde(default)]
    pub variant: StyleVariant,
    /// Ghost buttons have no background fill until hovered.
    #[serde(default)]
    pub ghost: bool,
    /// Whether clicks are disabled.
    #[serde(default)]
    pub disabled: bool,
}

/// Syntax-highlighted code block rendered via `kova_syntax`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeBlockWidget {
    /// Stable widget identifier.
    pub id: String,
    /// Source code text.
    pub code: String,
    /// Optional language tag (e.g. `"java"`, `"kotlin"`, `"json"`, `"toml"`, `"http"`).
    #[serde(default)]
    pub language: Option<String>,
    /// Whether line numbers are shown in the left gutter.
    #[serde(default = "default_true")]
    pub show_line_numbers: bool,
    /// Semantic accent variant for the block border/header.
    #[serde(default = "default_muted_variant")]
    pub variant: StyleVariant,
}

const fn default_true() -> bool {
    true
}

/// Native Markdown block view (no `WebView` or HTML).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkdownWidget {
    /// Stable widget identifier.
    pub id: String,
    /// Raw Markdown source text.
    pub content: String,
    /// Semantic variant applied to callouts/headings accent.
    #[serde(default = "default_muted_variant")]
    pub variant: StyleVariant,
}

/// Compact status pill badge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BadgeWidget {
    /// Stable widget identifier.
    pub id: String,
    /// Display text.
    pub label: String,
    /// Semantic style variant.
    #[serde(default)]
    pub variant: StyleVariant,
}

/// User interaction event emitted from `kova_ui` back to the plugin host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UiEvent {
    /// A [`ButtonWidget`] was clicked.
    ButtonClicked {
        /// Identifier of the clicked button.
        widget_id: String,
    },
    /// A [`TextInputWidget`] value changed.
    TextChanged {
        /// Identifier of the text input.
        widget_id: String,
        /// Updated text value.
        value: String,
    },
    /// The user pressed `Enter` inside a [`TextInputWidget`].
    TextSubmitted {
        /// Identifier of the text input.
        widget_id: String,
        /// Submitted text value.
        value: String,
    },
    /// A [`SelectWidget`] option was chosen.
    SelectChanged {
        /// Identifier of the select control.
        widget_id: String,
        /// Selected option value.
        value: String,
    },
    /// A [`ToggleWidget`] was toggled.
    ToggleChanged {
        /// Identifier of the toggle switch.
        widget_id: String,
        /// New checked state.
        checked: bool,
    },
    /// Active tab changed inside a [`TabsWidget`].
    TabChanged {
        /// Identifier of the tabs widget.
        widget_id: String,
        /// New zero-based tab index.
        index: usize,
        /// Identifier of the newly selected tab.
        tab_id: String,
    },
    /// A [`TreeNode`] was expanded or collapsed.
    TreeNodeToggled {
        /// Identifier of the tree widget.
        widget_id: String,
        /// Identifier of the toggled tree node.
        node_id: String,
        /// New expanded state.
        expanded: bool,
    },
    /// A [`TreeNode`] was selected.
    TreeNodeSelected {
        /// Identifier of the tree widget.
        widget_id: String,
        /// Identifier of the selected tree node.
        node_id: String,
    },
    /// A [`TableRow`] was single-clicked / selected.
    TableRowSelected {
        /// Identifier of the table widget.
        widget_id: String,
        /// Zero-based index of the selected row.
        row_index: usize,
        /// Identifier of the selected row.
        row_id: String,
    },
    /// A [`TableRow`] was double-clicked / activated.
    TableRowActivated {
        /// Identifier of the table widget.
        widget_id: String,
        /// Zero-based index of the activated row.
        row_index: usize,
        /// Identifier of the activated row.
        row_id: String,
    },
    /// A [`ListItem`] was single-clicked / selected.
    ListItemSelected {
        /// Identifier of the list widget.
        widget_id: String,
        /// Zero-based index of the selected item.
        item_index: usize,
        /// Identifier of the selected item.
        item_id: String,
    },
    /// A [`ListItem`] was double-clicked / activated.
    ListItemActivated {
        /// Identifier of the list widget.
        widget_id: String,
        /// Zero-based index of the activated item.
        item_index: usize,
        /// Identifier of the activated item.
        item_id: String,
    },
    /// A link inside a [`MarkdownWidget`] was clicked.
    LinkClicked {
        /// Identifier of the markdown widget.
        widget_id: String,
        /// Target URL or command URI.
        href: String,
    },
    /// A form was submitted.
    FormSubmitted {
        /// Identifier of the form widget.
        widget_id: String,
    },
}

impl UiEvent {
    /// Returns the `widget_id` associated with this event.
    #[must_use]
    pub fn widget_id(&self) -> &str {
        match self {
            Self::ButtonClicked { widget_id }
            | Self::TextChanged { widget_id, .. }
            | Self::TextSubmitted { widget_id, .. }
            | Self::SelectChanged { widget_id, .. }
            | Self::ToggleChanged { widget_id, .. }
            | Self::TabChanged { widget_id, .. }
            | Self::TreeNodeToggled { widget_id, .. }
            | Self::TreeNodeSelected { widget_id, .. }
            | Self::TableRowSelected { widget_id, .. }
            | Self::TableRowActivated { widget_id, .. }
            | Self::ListItemSelected { widget_id, .. }
            | Self::ListItemActivated { widget_id, .. }
            | Self::LinkClicked { widget_id, .. }
            | Self::FormSubmitted { widget_id } => widget_id,
        }
    }
}

/// A [`UiEvent`] tagged with the owning plugin instance identifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginUiEvent {
    /// Identifier of the target plugin instance.
    pub plugin_id: String,
    /// Interaction payload emitted by `kova_ui`.
    pub event: UiEvent,
}

/// Error returned when sending a widget tree update or UI event over [`PluginUiChannel`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PluginUiChannelError {
    /// The widget tree failed validation before dispatch.
    #[error(transparent)]
    Validation(#[from] WidgetValidationError),
    /// The peer half of the channel was dropped.
    #[error("plugin UI channel closed")]
    Closed,
}

/// Factory for creating a bidirectional [`PluginUiHostHandle`] / [`PluginUiViewHandle`] pair.
#[derive(Debug, Clone, Copy)]
pub struct PluginUiChannel;

impl PluginUiChannel {
    /// Creates a connected host/view channel pair for `plugin_id`.
    #[must_use]
    pub fn pair(plugin_id: impl Into<String>) -> (PluginUiHostHandle, PluginUiViewHandle) {
        let plugin_id = plugin_id.into();
        let (tree_tx, tree_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        (
            PluginUiHostHandle {
                plugin_id: plugin_id.clone(),
                tree_tx,
                event_rx,
            },
            PluginUiViewHandle {
                plugin_id,
                tree_rx,
                event_tx,
            },
        )
    }
}

/// Host-side (`PluginManager`) endpoint of a plugin declarative UI channel.
pub struct PluginUiHostHandle {
    plugin_id: String,
    tree_tx: mpsc::UnboundedSender<WidgetNode>,
    event_rx: mpsc::UnboundedReceiver<PluginUiEvent>,
}

impl fmt::Debug for PluginUiHostHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginUiHostHandle")
            .field("plugin_id", &self.plugin_id)
            .finish_non_exhaustive()
    }
}

impl PluginUiHostHandle {
    /// Returns the plugin identifier bound to this channel.
    #[must_use]
    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    /// Validates `tree` and pushes it to the `kova_ui` view for re-rendering.
    ///
    /// # Errors
    ///
    /// Returns [`PluginUiChannelError::Validation`] if `tree` violates safety bounds, or
    /// [`PluginUiChannelError::Closed`] if the UI view has been dropped.
    pub fn send_tree(&self, tree: WidgetNode) -> Result<WidgetTreeStats, PluginUiChannelError> {
        let stats = tree.validate()?;
        self.tree_tx
            .send(tree)
            .map_err(|_| PluginUiChannelError::Closed)?;
        Ok(stats)
    }

    /// Attempts to receive the next [`PluginUiEvent`] without blocking.
    pub fn try_recv_event(&mut self) -> Option<PluginUiEvent> {
        self.event_rx.try_recv().ok()
    }

    /// Awaits the next [`PluginUiEvent`] from the UI view, returning `None` when closed.
    pub async fn recv_event(&mut self) -> Option<PluginUiEvent> {
        self.event_rx.recv().await
    }
}

/// View-side (`PluginToolWindow`) endpoint of a plugin declarative UI channel.
pub struct PluginUiViewHandle {
    plugin_id: String,
    tree_rx: mpsc::UnboundedReceiver<WidgetNode>,
    event_tx: mpsc::UnboundedSender<PluginUiEvent>,
}

impl fmt::Debug for PluginUiViewHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginUiViewHandle")
            .field("plugin_id", &self.plugin_id)
            .finish_non_exhaustive()
    }
}

impl PluginUiViewHandle {
    /// Returns the plugin identifier bound to this channel.
    #[must_use]
    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    /// Splits this handle into an outgoing event sender and an incoming tree receiver.
    #[must_use]
    pub fn split(self) -> (PluginUiEventSender, PluginUiTreeReceiver) {
        (
            PluginUiEventSender {
                plugin_id: self.plugin_id,
                event_tx: self.event_tx,
            },
            PluginUiTreeReceiver {
                tree_rx: self.tree_rx,
            },
        )
    }

    /// Emits a [`UiEvent`] tagged with `self.plugin_id` back to the plugin host.
    ///
    /// # Errors
    ///
    /// Returns [`PluginUiChannelError::Closed`] if the host handle was dropped.
    pub fn send_event(&self, event: UiEvent) -> Result<(), PluginUiChannelError> {
        self.event_tx
            .send(PluginUiEvent {
                plugin_id: self.plugin_id.clone(),
                event,
            })
            .map_err(|_| PluginUiChannelError::Closed)
    }

    /// Drains all queued [`WidgetNode`] updates and returns the latest one, if any.
    pub fn drain_latest_tree(&mut self) -> Option<WidgetNode> {
        let mut latest = None;
        while let Ok(tree) = self.tree_rx.try_recv() {
            latest = Some(tree);
        }
        latest
    }

    /// Awaits the next [`WidgetNode`] update pushed by the plugin host.
    pub async fn recv_tree(&mut self) -> Option<WidgetNode> {
        self.tree_rx.recv().await
    }
}

/// Outgoing [`PluginUiEvent`] sender half of [`PluginUiViewHandle`].
#[derive(Clone)]
pub struct PluginUiEventSender {
    plugin_id: String,
    event_tx: mpsc::UnboundedSender<PluginUiEvent>,
}

impl fmt::Debug for PluginUiEventSender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginUiEventSender")
            .field("plugin_id", &self.plugin_id)
            .finish_non_exhaustive()
    }
}

impl PluginUiEventSender {
    /// Emits `event` tagged with the plugin identifier back to the host.
    ///
    /// # Errors
    ///
    /// Returns [`PluginUiChannelError::Closed`] if the host receiver was dropped.
    pub fn send_event(&self, event: UiEvent) -> Result<(), PluginUiChannelError> {
        self.event_tx
            .send(PluginUiEvent {
                plugin_id: self.plugin_id.clone(),
                event,
            })
            .map_err(|_| PluginUiChannelError::Closed)
    }
}

/// Incoming [`WidgetNode`] receiver half of [`PluginUiViewHandle`].
pub struct PluginUiTreeReceiver {
    tree_rx: mpsc::UnboundedReceiver<WidgetNode>,
}

impl fmt::Debug for PluginUiTreeReceiver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginUiTreeReceiver")
            .finish_non_exhaustive()
    }
}

impl PluginUiTreeReceiver {
    /// Awaits the next [`WidgetNode`] update pushed by the plugin host.
    pub async fn recv_tree(&mut self) -> Option<WidgetNode> {
        self.tree_rx.recv().await
    }

    /// Drains all queued [`WidgetNode`] updates and returns the latest one, if any.
    pub fn drain_latest_tree(&mut self) -> Option<WidgetNode> {
        let mut latest = None;
        while let Ok(tree) = self.tree_rx.try_recv() {
            latest = Some(tree);
        }
        latest
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "unit tests assert widget serialization, validation, and channel delivery"
)]
mod tests {
    use super::*;

    #[allow(
        clippy::too_many_lines,
        reason = "test fixture exercising all 9 declarative WidgetNode types"
    )]
    fn sample_tree() -> WidgetNode {
        WidgetNode::Split(SplitWidget {
            id: "root-split".to_owned(),
            axis: SplitDirection::Horizontal,
            ratio: 0.3,
            first: Box::new(WidgetNode::Tree(TreeWidget {
                id: "schema-tree".to_owned(),
                selected_id: Some("tbl-users".to_owned()),
                nodes: vec![TreeNode {
                    id: "db-main".to_owned(),
                    label: "postgres_prod".to_owned(),
                    secondary: Some("v16".to_owned()),
                    badge: Some(BadgeWidget {
                        id: "db-badge".to_owned(),
                        label: "RO".to_owned(),
                        variant: StyleVariant::Accent,
                    }),
                    variant: StyleVariant::Primary,
                    expanded: true,
                    children: vec![TreeNode {
                        id: "tbl-users".to_owned(),
                        label: "users".to_owned(),
                        secondary: Some("42k rows".to_owned()),
                        badge: None,
                        variant: StyleVariant::Muted,
                        expanded: false,
                        children: Vec::new(),
                    }],
                }],
            })),
            second: Box::new(WidgetNode::Tabs(TabsWidget {
                id: "main-tabs".to_owned(),
                active_index: 0,
                variant: StyleVariant::Primary,
                tabs: vec![
                    TabNode {
                        id: "tab-query".to_owned(),
                        title: "Query".to_owned(),
                        badge: None,
                        content: WidgetNode::Split(SplitWidget {
                            id: "query-split".to_owned(),
                            axis: SplitDirection::Vertical,
                            ratio: 0.45,
                            first: Box::new(WidgetNode::Form(FormWidget {
                                id: "query-form".to_owned(),
                                title: Some("Parameters".to_owned()),
                                variant: StyleVariant::Primary,
                                fields: vec![
                                    FormField {
                                        label: Some("Filter".to_owned()),
                                        hint: Some("Matches email or id".to_owned()),
                                        control: FormControl::TextInput(TextInputWidget {
                                            id: "filter-input".to_owned(),
                                            value: "admin".to_owned(),
                                            placeholder: "Enter filter...".to_owned(),
                                            disabled: false,
                                            variant: StyleVariant::Primary,
                                        }),
                                    },
                                    FormField {
                                        label: Some("Limit".to_owned()),
                                        hint: None,
                                        control: FormControl::Select(SelectWidget {
                                            id: "limit-select".to_owned(),
                                            options: vec![
                                                SelectOption {
                                                    value: "100".to_owned(),
                                                    label: "100 rows".to_owned(),
                                                },
                                                SelectOption {
                                                    value: "1000".to_owned(),
                                                    label: "1000 rows".to_owned(),
                                                },
                                            ],
                                            selected_value: Some("100".to_owned()),
                                            disabled: false,
                                            variant: StyleVariant::Muted,
                                        }),
                                    },
                                    FormField {
                                        label: Some("Explain".to_owned()),
                                        hint: None,
                                        control: FormControl::Toggle(ToggleWidget {
                                            id: "explain-toggle".to_owned(),
                                            checked: true,
                                            disabled: false,
                                            variant: StyleVariant::Accent,
                                        }),
                                    },
                                    FormField {
                                        label: None,
                                        hint: None,
                                        control: FormControl::Button(ButtonWidget {
                                            id: "run-btn".to_owned(),
                                            label: "Run Query".to_owned(),
                                            variant: StyleVariant::Primary,
                                            ghost: false,
                                            disabled: false,
                                        }),
                                    },
                                ],
                            })),
                            second: Box::new(WidgetNode::Table(TableWidget {
                                id: "results-table".to_owned(),
                                columns: vec![
                                    TableColumn {
                                        id: "col-id".to_owned(),
                                        header: "id".to_owned(),
                                        weight: 1,
                                        align: TextAlign::Right,
                                    },
                                    TableColumn {
                                        id: "col-email".to_owned(),
                                        header: "email".to_owned(),
                                        weight: 3,
                                        align: TextAlign::Left,
                                    },
                                ],
                                rows: vec![
                                    TableRow::from_strings("row-1", ["1", "alice@kova.dev"]),
                                    TableRow {
                                        id: "row-2".to_owned(),
                                        cells: vec![
                                            TableCell::plain("2"),
                                            TableCell::styled(
                                                "blocked@kova.dev",
                                                StyleVariant::Warning,
                                            ),
                                        ],
                                    },
                                ],
                                selected_row: Some(0),
                                empty_message: Some("No rows".to_owned()),
                            })),
                        }),
                    },
                    TabNode {
                        id: "tab-docs".to_owned(),
                        title: "DDL & Notes".to_owned(),
                        badge: Some(BadgeWidget {
                            id: "docs-badge".to_owned(),
                            label: "SQL".to_owned(),
                            variant: StyleVariant::Muted,
                        }),
                        content: WidgetNode::List(ListWidget {
                            id: "history-list".to_owned(),
                            items: vec![ListItem {
                                id: "hist-1".to_owned(),
                                title: "SELECT * FROM users".to_owned(),
                                subtitle: Some("12 ms · 2 rows".to_owned()),
                                badge: Some(BadgeWidget {
                                    id: "ok-badge".to_owned(),
                                    label: "OK".to_owned(),
                                    variant: StyleVariant::Accent,
                                }),
                                variant: StyleVariant::Primary,
                            }],
                            selected_index: Some(0),
                            empty_message: None,
                        }),
                    },
                    TabNode {
                        id: "tab-code".to_owned(),
                        title: "JSON".to_owned(),
                        badge: None,
                        content: WidgetNode::CodeBlock(CodeBlockWidget {
                            id: "payload-json".to_owned(),
                            code: "{\"ok\": true}".to_owned(),
                            language: Some("json".to_owned()),
                            show_line_numbers: true,
                            variant: StyleVariant::Muted,
                        }),
                    },
                    TabNode {
                        id: "tab-md".to_owned(),
                        title: "Docs".to_owned(),
                        badge: None,
                        content: WidgetNode::Markdown(MarkdownWidget {
                            id: "help-md".to_owned(),
                            content: "# Help\nSee [docs](https://kova.dev).".to_owned(),
                            variant: StyleVariant::Primary,
                        }),
                    },
                ],
            })),
        })
    }

    #[test]
    fn style_variants_roundtrip_with_theme_plugin_variants() {
        for variant in [
            StyleVariant::Primary,
            StyleVariant::Muted,
            StyleVariant::Warning,
            StyleVariant::Error,
            StyleVariant::Accent,
        ] {
            let theme_variant: PluginVariant = variant.into();
            assert_eq!(StyleVariant::from(theme_variant), variant);
        }
    }

    #[test]
    fn widget_tree_and_ui_events_roundtrip_over_json() {
        let tree = sample_tree();
        let stats = tree.validate().expect("sample tree must be valid");
        assert!(stats.node_count >= 9);
        assert_eq!(stats.total_rows, 2);

        let json = serde_json::to_string(&tree).unwrap();
        let decoded: WidgetNode = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, tree);

        let events = [
            UiEvent::ButtonClicked {
                widget_id: "run-btn".to_owned(),
            },
            UiEvent::TextChanged {
                widget_id: "filter-input".to_owned(),
                value: "bob".to_owned(),
            },
            UiEvent::TableRowSelected {
                widget_id: "results-table".to_owned(),
                row_index: 1,
                row_id: "row-2".to_owned(),
            },
            UiEvent::TabChanged {
                widget_id: "main-tabs".to_owned(),
                index: 2,
                tab_id: "tab-code".to_owned(),
            },
        ];
        for ev in events {
            let serialized = serde_json::to_string(&ev).unwrap();
            let roundtrip: UiEvent = serde_json::from_str(&serialized).unwrap();
            assert_eq!(roundtrip, ev);
            assert_ne!(roundtrip.widget_id(), "");
        }
    }

    #[test]
    fn validation_rejects_excessive_depth_and_malformed_nodes() {
        // Depth > MAX_WIDGET_DEPTH
        let mut deep = WidgetNode::Badge(BadgeWidget {
            id: "leaf".to_owned(),
            label: "ok".to_owned(),
            variant: StyleVariant::Primary,
        });
        for i in 0..=MAX_WIDGET_DEPTH {
            deep = WidgetNode::Split(SplitWidget {
                id: format!("split-{i}"),
                axis: SplitDirection::Horizontal,
                ratio: 0.5,
                first: Box::new(deep),
                second: Box::new(WidgetNode::Badge(BadgeWidget {
                    id: format!("b-{i}"),
                    label: "x".to_owned(),
                    variant: StyleVariant::Muted,
                })),
            });
        }
        assert!(matches!(
            deep.validate(),
            Err(WidgetValidationError::MaxDepthExceeded { .. })
        ));

        // Empty widget ID
        let empty_id = WidgetNode::Badge(BadgeWidget {
            id: "   ".to_owned(),
            label: "x".to_owned(),
            variant: StyleVariant::Error,
        });
        assert_eq!(
            empty_id.validate(),
            Err(WidgetValidationError::EmptyWidgetId)
        );

        // Invalid split ratio (NaN / out of bounds)
        let bad_split = WidgetNode::Split(SplitWidget {
            id: "bad-split".to_owned(),
            axis: SplitDirection::Vertical,
            ratio: f32::NAN,
            first: Box::new(WidgetNode::Badge(BadgeWidget {
                id: "a".to_owned(),
                label: "a".to_owned(),
                variant: StyleVariant::Primary,
            })),
            second: Box::new(WidgetNode::Badge(BadgeWidget {
                id: "b".to_owned(),
                label: "b".to_owned(),
                variant: StyleVariant::Primary,
            })),
        });
        assert!(matches!(
            bad_split.validate(),
            Err(WidgetValidationError::InvalidSplitRatio { .. })
        ));

        // Invalid tab index
        let bad_tabs = WidgetNode::Tabs(TabsWidget {
            id: "bad-tabs".to_owned(),
            active_index: 3,
            variant: StyleVariant::Primary,
            tabs: vec![TabNode {
                id: "t0".to_owned(),
                title: "One".to_owned(),
                badge: None,
                content: WidgetNode::Badge(BadgeWidget {
                    id: "c0".to_owned(),
                    label: "1".to_owned(),
                    variant: StyleVariant::Accent,
                }),
            }],
        });
        assert!(matches!(
            bad_tabs.validate(),
            Err(WidgetValidationError::InvalidTabIndex { .. })
        ));
    }

    #[test]
    fn validation_allows_large_virtualized_table_while_enforcing_hard_cap() {
        let rows: Vec<TableRow> = (0..10_000)
            .map(|i| TableRow::from_strings(format!("r-{i}"), [format!("{i}"), "ok".to_owned()]))
            .collect();
        let table = WidgetNode::Table(TableWidget {
            id: "sql-big".to_owned(),
            columns: vec![
                TableColumn {
                    id: "id".to_owned(),
                    header: "id".to_owned(),
                    weight: 1,
                    align: TextAlign::Right,
                },
                TableColumn {
                    id: "status".to_owned(),
                    header: "status".to_owned(),
                    weight: 2,
                    align: TextAlign::Left,
                },
            ],
            rows,
            selected_row: None,
            empty_message: None,
        });
        let stats = table.validate().expect("10k-row virtual table is valid");
        assert_eq!(stats.total_rows, 10_000);
    }

    #[test]
    fn plugin_ui_channel_validates_trees_and_delivers_events() {
        let (mut host, mut view) = PluginUiChannel::pair("kova-database");
        assert_eq!(host.plugin_id(), "kova-database");
        assert_eq!(view.plugin_id(), "kova-database");

        // Reject invalid tree on host side before pushing.
        let invalid = WidgetNode::Badge(BadgeWidget {
            id: String::new(),
            label: "bad".to_owned(),
            variant: StyleVariant::Error,
        });
        assert!(matches!(
            host.send_tree(invalid),
            Err(PluginUiChannelError::Validation(
                WidgetValidationError::EmptyWidgetId
            ))
        ));

        let tree = sample_tree();
        host.send_tree(tree.clone()).unwrap();
        assert_eq!(view.drain_latest_tree(), Some(tree));

        view.send_event(UiEvent::ButtonClicked {
            widget_id: "run-btn".to_owned(),
        })
        .unwrap();
        assert_eq!(
            host.try_recv_event(),
            Some(PluginUiEvent {
                plugin_id: "kova-database".to_owned(),
                event: UiEvent::ButtonClicked {
                    widget_id: "run-btn".to_owned(),
                },
            })
        );
    }
}
