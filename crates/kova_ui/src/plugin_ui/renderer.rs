//! GPUI element renderer for plugin declarative widgets (`KOVA-045`).
//!
//! Maps [`WidgetNode`] variants (`Split`, `Tabs`, `Tree`, `List`, `Form`, `Markdown`,
//! `Badge`, plus delegating `Table` and `CodeBlock`) onto native GPUI elements styled
//! strictly from active [`KovaTheme`] semantic tokens.

use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;

use gpui::{
    AnyElement, App, ClickEvent, Div, FontStyle, FontWeight, HighlightStyle,
    InteractiveElement as _, IntoElement as _, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, StyledText, UniformListScrollHandle, Window, div,
    px, relative, uniform_list,
};
use kova_plugin_host::ui::{
    BadgeWidget, ButtonWidget, CodeBlockWidget, FormControl, FormField, FormWidget, ListWidget,
    MarkdownWidget, SelectWidget, SplitDirection, SplitWidget, StyleVariant, TabsWidget,
    TextInputWidget, ToggleWidget, TreeNode, TreeWidget, UiEvent, WidgetNode,
};
use kova_theme::PluginVariant;

use crate::controls::{
    Badge, Button, ControlPalette, SelectInput, TextInput, TextInputEvent, TextInputState,
    ToggleSwitch,
};
use crate::plugin_ui::code_block::{PreparedCodeBlock, prepare_code_block, render_code_block};
use crate::plugin_ui::virtual_table::{TableEventCallback, render_virtual_table};
use crate::theme::{KovaTheme, hsla_from_rgba};

const TREE_ROW_HEIGHT_PX: f32 = 24.0;
const TREE_INDENT_PX: f32 = 16.0;
const LIST_ROW_HEIGHT_PX: f32 = 28.0;
const LIST_SUBTITLE_ROW_HEIGHT_PX: f32 = 42.0;

/// Internal UI action produced by rendered plugin widgets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginRendererAction {
    /// Forward a [`UiEvent`] to the owning plugin host.
    Emit(UiEvent),
    /// Move keyboard focus to the identified [`TextInputWidget`].
    FocusTextInput(String),
    /// Toggle the drop-down option list of the identified [`SelectWidget`].
    ToggleSelectDropdown(String),
}

/// Callback invoked when any rendered widget produces a [`PluginRendererAction`].
pub type PluginActionCallback = Rc<dyn Fn(&PluginRendererAction, &mut Window, &mut App)>;

/// Single visible row produced by flattening a [`TreeWidget`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlattenedTreeNode {
    /// Stable node identifier.
    pub id: String,
    /// Display label.
    pub label: String,
    /// Optional muted secondary annotation.
    pub secondary: Option<String>,
    /// Optional trailing badge.
    pub badge: Option<BadgeWidget>,
    /// Semantic style variant.
    pub variant: StyleVariant,
    /// Zero-based indentation depth.
    pub depth: u16,
    /// Whether this node has nested children.
    pub has_children: bool,
    /// Effective expanded state (including local user toggles).
    pub expanded: bool,
}

/// Flattens hierarchical [`TreeNode`]s into visible rows respecting `expand_overrides`.
#[must_use]
pub fn flatten_tree<S: std::hash::BuildHasher>(
    nodes: &[TreeNode],
    expand_overrides: &HashMap<String, bool, S>,
) -> Vec<FlattenedTreeNode> {
    let mut out = Vec::new();
    flatten_tree_recursive(nodes, 0, expand_overrides, &mut out);
    out
}

fn flatten_tree_recursive<S: std::hash::BuildHasher>(
    nodes: &[TreeNode],
    depth: u16,
    expand_overrides: &HashMap<String, bool, S>,
    out: &mut Vec<FlattenedTreeNode>,
) {
    for node in nodes {
        let has_children = !node.children.is_empty();
        let expanded = expand_overrides
            .get(&node.id)
            .copied()
            .unwrap_or(node.expanded);
        out.push(FlattenedTreeNode {
            id: node.id.clone(),
            label: node.label.clone(),
            secondary: node.secondary.clone(),
            badge: node.badge.clone(),
            variant: node.variant,
            depth,
            has_children,
            expanded,
        });
        if has_children && expanded {
            flatten_tree_recursive(
                &node.children,
                depth.saturating_add(1),
                expand_overrides,
                out,
            );
        }
    }
}

/// Block-level element parsed from a [`MarkdownWidget`] source string (no HTML/`WebView`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkdownBlock {
    /// Heading (`#` through `######`).
    Heading {
        /// Heading level (`1..=6`).
        level: u8,
        /// Inline styled spans.
        spans: Vec<InlineSpan>,
    },
    /// Paragraph of inline spans.
    Paragraph(Vec<InlineSpan>),
    /// Unordered (`-`, `*`, `+`) or ordered (`1.`) list item.
    ListItem {
        /// `Some(n)` for ordered list items, `None` for bullet items.
        ordered_index: Option<usize>,
        /// Inline styled spans.
        spans: Vec<InlineSpan>,
    },
    /// Blockquote (`> `).
    Quote(Vec<InlineSpan>),
    /// Fenced code block rendered via `kova_syntax`.
    Code {
        /// Optional language tag.
        language: Option<String>,
        /// Code body.
        code: String,
    },
    /// Horizontal divider (`---`, `***`).
    Rule,
}

/// Inline styled span inside a [`MarkdownBlock`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InlineSpan {
    /// Unstyled text.
    Plain(String),
    /// Bold (`**text**`).
    Bold(String),
    /// Italic (`*text*`).
    Italic(String),
    /// Inline monospace code (`` `code` ``).
    Code(String),
    /// Clickable link (`[label](href)`).
    Link {
        /// Visible anchor label.
        label: String,
        /// Target URL or plugin command URI.
        href: String,
    },
}

/// Parses a Markdown document into native [`MarkdownBlock`]s without HTML or a `WebView`.
#[must_use]
pub fn parse_markdown(input: &str) -> Vec<MarkdownBlock> {
    let mut blocks = Vec::new();
    let lines: Vec<&str> = input.lines().collect();
    let mut idx = 0usize;

    while idx < lines.len() {
        let line = lines[idx].trim_end();
        let trimmed = line.trim();

        if trimmed.is_empty() {
            idx += 1;
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("```") {
            let lang = rest.trim();
            let language = (!lang.is_empty()).then(|| lang.to_owned());
            idx += 1;
            let mut code_lines = Vec::new();
            while idx < lines.len() && !lines[idx].trim().starts_with("```") {
                code_lines.push(lines[idx]);
                idx += 1;
            }
            if idx < lines.len() {
                idx += 1;
            }
            blocks.push(MarkdownBlock::Code {
                language,
                code: code_lines.join("\n"),
            });
            continue;
        }

        if is_horizontal_rule(trimmed) {
            blocks.push(MarkdownBlock::Rule);
            idx += 1;
            continue;
        }

        if let Some((level, text)) = parse_heading(trimmed) {
            blocks.push(MarkdownBlock::Heading {
                level,
                spans: parse_inline_spans(text),
            });
            idx += 1;
            continue;
        }

        if let Some(quote) = trimmed
            .strip_prefix("> ")
            .or_else(|| trimmed.strip_prefix('>'))
        {
            blocks.push(MarkdownBlock::Quote(parse_inline_spans(quote.trim())));
            idx += 1;
            continue;
        }

        if let Some(item) = parse_unordered_item(trimmed) {
            blocks.push(MarkdownBlock::ListItem {
                ordered_index: None,
                spans: parse_inline_spans(item),
            });
            idx += 1;
            continue;
        }

        if let Some((number, item)) = parse_ordered_item(trimmed) {
            blocks.push(MarkdownBlock::ListItem {
                ordered_index: Some(number),
                spans: parse_inline_spans(item),
            });
            idx += 1;
            continue;
        }

        let mut para_lines = vec![trimmed];
        idx += 1;
        while idx < lines.len() {
            let next = lines[idx].trim();
            if next.is_empty()
                || next.starts_with("```")
                || is_horizontal_rule(next)
                || parse_heading(next).is_some()
                || next.starts_with('>')
                || parse_unordered_item(next).is_some()
                || parse_ordered_item(next).is_some()
            {
                break;
            }
            para_lines.push(next);
            idx += 1;
        }
        blocks.push(MarkdownBlock::Paragraph(parse_inline_spans(
            &para_lines.join(" "),
        )));
    }

    blocks
}

fn is_horizontal_rule(trimmed: &str) -> bool {
    trimmed.len() >= 3
        && (trimmed.chars().all(|c| c == '-')
            || trimmed.chars().all(|c| c == '*')
            || trimmed.chars().all(|c| c == '_'))
}

fn parse_heading(trimmed: &str) -> Option<(u8, &str)> {
    let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = trimmed[hashes..].strip_prefix(' ')?;
    let level = u8::try_from(hashes).ok()?;
    Some((level, rest.trim()))
}

fn parse_unordered_item(trimmed: &str) -> Option<&str> {
    trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))
        .map(str::trim)
}

fn parse_ordered_item(trimmed: &str) -> Option<(usize, &str)> {
    let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let rest = trimmed[digits..].strip_prefix(". ")?;
    let num = trimmed[..digits].parse::<usize>().ok()?;
    Some((num, rest.trim()))
}

/// Parses inline Markdown formatting (`**bold**`, `*italic*`, `` `code` ``, `[label](href)`).
#[must_use]
pub fn parse_inline_spans(text: &str) -> Vec<InlineSpan> {
    let mut spans = Vec::new();
    let mut plain = String::new();
    let mut rest = text;

    while !rest.is_empty() {
        if let Some(after_open) = rest.strip_prefix("**")
            && let Some(end) = after_open.find("**")
            && end > 0
        {
            flush_plain(&mut plain, &mut spans);
            spans.push(InlineSpan::Bold(after_open[..end].to_owned()));
            rest = &after_open[end + 2..];
            continue;
        }

        if let Some(after_open) = rest.strip_prefix('`')
            && let Some(end) = after_open.find('`')
            && end > 0
        {
            flush_plain(&mut plain, &mut spans);
            spans.push(InlineSpan::Code(after_open[..end].to_owned()));
            rest = &after_open[end + 1..];
            continue;
        }

        if rest.starts_with('[')
            && let Some((label, href, consumed)) = try_parse_link(rest)
        {
            flush_plain(&mut plain, &mut spans);
            spans.push(InlineSpan::Link {
                label: label.to_owned(),
                href: href.to_owned(),
            });
            rest = &rest[consumed..];
            continue;
        }

        if let Some(after_open) = rest.strip_prefix('*')
            && !after_open.starts_with('*')
            && let Some(end) = after_open.find('*')
            && end > 0
        {
            flush_plain(&mut plain, &mut spans);
            spans.push(InlineSpan::Italic(after_open[..end].to_owned()));
            rest = &after_open[end + 1..];
            continue;
        }

        if let Some(ch) = rest.chars().next() {
            plain.push(ch);
            rest = &rest[ch.len_utf8()..];
        }
    }

    flush_plain(&mut plain, &mut spans);
    spans
}

fn flush_plain(plain: &mut String, spans: &mut Vec<InlineSpan>) {
    if !plain.is_empty() {
        spans.push(InlineSpan::Plain(std::mem::take(plain)));
    }
}

fn try_parse_link(input: &str) -> Option<(&str, &str, usize)> {
    let after_bracket = input.strip_prefix('[')?;
    let close_bracket = after_bracket.find(']')?;
    let label = &after_bracket[..close_bracket];
    let after_label = &after_bracket[close_bracket + 1..];
    let after_paren = after_label.strip_prefix('(')?;
    let close_paren = after_paren.find(')')?;
    let href = &after_paren[..close_paren];
    let total_len = 1 + close_bracket + 1 + 1 + close_paren + 1;
    Some((label, href, total_len))
}

/// Mutable local interaction and cache state owned by [`super::PluginToolWindow`].
#[derive(Default)]
pub struct PluginUiState {
    tab_overrides: HashMap<String, usize>,
    tree_expanded: HashMap<String, HashMap<String, bool>>,
    tree_selected: HashMap<String, String>,
    table_selected: HashMap<String, usize>,
    list_selected: HashMap<String, usize>,
    text_inputs: HashMap<String, TextInputState>,
    focused_input: Option<String>,
    open_select: Option<String>,
    select_overrides: HashMap<String, String>,
    toggle_overrides: HashMap<String, bool>,
    scroll_handles: HashMap<String, UniformListScrollHandle>,
    code_cache: HashMap<(String, Option<String>), PreparedCodeBlock>,
}

impl fmt::Debug for PluginUiState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginUiState")
            .field("focused_input", &self.focused_input)
            .field("open_select", &self.open_select)
            .field("scroll_handles", &self.scroll_handles.len())
            .field("code_cache", &self.code_cache.len())
            .finish_non_exhaustive()
    }
}

impl PluginUiState {
    /// Synchronizes cached scroll handles, code blocks, and form inputs with `root`.
    pub fn prepare_for_tree(&mut self, root: &WidgetNode) {
        self.prepare_recursive(root);
    }

    /// Reconciles local state when a new authoritative [`WidgetNode`] arrives from the plugin.
    pub fn sync_with_tree(&mut self, root: &WidgetNode) {
        self.tab_overrides.clear();
        self.toggle_overrides.clear();
        self.select_overrides.clear();
        self.tree_selected.clear();
        self.table_selected.clear();
        self.list_selected.clear();
        self.open_select = None;
        self.sync_inputs_recursive(root);
        self.prepare_recursive(root);
    }

    fn sync_inputs_recursive(&mut self, node: &WidgetNode) {
        match node {
            WidgetNode::Split(split) => {
                self.sync_inputs_recursive(&split.first);
                self.sync_inputs_recursive(&split.second);
            }
            WidgetNode::Tabs(tabs) => {
                for tab in &tabs.tabs {
                    self.sync_inputs_recursive(&tab.content);
                }
            }
            WidgetNode::Form(form) => {
                for field in &form.fields {
                    if let FormControl::TextInput(input) = &field.control {
                        let needs_reset = self
                            .text_inputs
                            .get(&input.id)
                            .is_none_or(|state| state.text() != input.value);
                        if needs_reset {
                            self.text_inputs
                                .insert(input.id.clone(), TextInputState::new(&input.value));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn prepare_recursive(&mut self, node: &WidgetNode) {
        match node {
            WidgetNode::Split(split) => {
                self.prepare_recursive(&split.first);
                self.prepare_recursive(&split.second);
            }
            WidgetNode::Tabs(tabs) => {
                for tab in &tabs.tabs {
                    self.prepare_recursive(&tab.content);
                }
            }
            WidgetNode::Tree(tree) => {
                self.scroll_handles
                    .entry(format!("tree:{}", tree.id))
                    .or_default();
            }
            WidgetNode::Table(table) => {
                self.scroll_handles
                    .entry(format!("table:{}", table.id))
                    .or_default();
            }
            WidgetNode::List(list) => {
                self.scroll_handles
                    .entry(format!("list:{}", list.id))
                    .or_default();
            }
            WidgetNode::Form(form) => {
                for field in &form.fields {
                    if let FormControl::TextInput(input) = &field.control {
                        self.text_inputs
                            .entry(input.id.clone())
                            .or_insert_with(|| TextInputState::new(&input.value));
                    }
                }
            }
            WidgetNode::CodeBlock(block) => {
                self.ensure_code_cached(&block.code, block.language.as_deref());
            }
            WidgetNode::Markdown(md) => {
                for block in parse_markdown(&md.content) {
                    if let MarkdownBlock::Code { language, code } = block {
                        self.ensure_code_cached(&code, language.as_deref());
                    }
                }
            }
            WidgetNode::Badge(_) => {}
        }
    }

    fn ensure_code_cached(&mut self, code: &str, language: Option<&str>) {
        let key = (code.to_owned(), language.map(str::to_owned));
        self.code_cache
            .entry(key)
            .or_insert_with(|| prepare_code_block(code, language));
    }

    /// Applies a local UI interaction optimistically before the plugin returns an updated tree.
    pub fn apply_action(&mut self, action: &PluginRendererAction) {
        match action {
            PluginRendererAction::FocusTextInput(id) => {
                self.open_select = None;
                self.focused_input = Some(id.clone());
            }
            PluginRendererAction::ToggleSelectDropdown(id) => {
                self.focused_input = None;
                if self.open_select.as_deref() == Some(id.as_str()) {
                    self.open_select = None;
                } else {
                    self.open_select = Some(id.clone());
                }
            }
            PluginRendererAction::Emit(event) => self.apply_event(event),
        }
    }

    fn apply_event(&mut self, event: &UiEvent) {
        match event {
            UiEvent::TabChanged {
                widget_id, index, ..
            } => {
                self.tab_overrides.insert(widget_id.clone(), *index);
            }
            UiEvent::TreeNodeToggled {
                widget_id,
                node_id,
                expanded,
            } => {
                self.tree_expanded
                    .entry(widget_id.clone())
                    .or_default()
                    .insert(node_id.clone(), *expanded);
            }
            UiEvent::TreeNodeSelected { widget_id, node_id } => {
                self.tree_selected
                    .insert(widget_id.clone(), node_id.clone());
            }
            UiEvent::TableRowSelected {
                widget_id,
                row_index,
                ..
            }
            | UiEvent::TableRowActivated {
                widget_id,
                row_index,
                ..
            } => {
                self.table_selected.insert(widget_id.clone(), *row_index);
            }
            UiEvent::ListItemSelected {
                widget_id,
                item_index,
                ..
            }
            | UiEvent::ListItemActivated {
                widget_id,
                item_index,
                ..
            } => {
                self.list_selected.insert(widget_id.clone(), *item_index);
            }
            UiEvent::SelectChanged { widget_id, value } => {
                self.select_overrides
                    .insert(widget_id.clone(), value.clone());
                self.open_select = None;
            }
            UiEvent::ToggleChanged { widget_id, checked } => {
                self.toggle_overrides.insert(widget_id.clone(), *checked);
            }
            UiEvent::ButtonClicked { .. }
            | UiEvent::TextChanged { .. }
            | UiEvent::TextSubmitted { .. }
            | UiEvent::LinkClicked { .. }
            | UiEvent::FormSubmitted { .. } => {}
        }
    }

    /// Processes a keyboard stroke for the currently focused [`TextInputWidget`] or open select.
    ///
    /// Returns a [`UiEvent`] when the text changes or is submitted.
    pub fn handle_key_stroke(
        &mut self,
        key: &str,
        key_char: Option<&str>,
        command: bool,
    ) -> Option<UiEvent> {
        if command {
            return None;
        }
        if key == "escape" {
            if self.open_select.take().is_some() || self.focused_input.take().is_some() {
                return None;
            }
            return None;
        }
        let input_id = self.focused_input.clone()?;
        let state = self.text_inputs.entry(input_id.clone()).or_default();
        let outcome = match state.handle_named_key(key) {
            TextInputEvent::Ignored => {
                key_char.map_or(TextInputEvent::Ignored, |s| state.insert(s))
            }
            other => other,
        };
        match outcome {
            TextInputEvent::Edited => Some(UiEvent::TextChanged {
                widget_id: input_id,
                value: state.text().to_owned(),
            }),
            TextInputEvent::Submit => Some(UiEvent::TextSubmitted {
                widget_id: input_id,
                value: state.text().to_owned(),
            }),
            TextInputEvent::Cancel => {
                self.focused_input = None;
                None
            }
            TextInputEvent::Ignored => None,
        }
    }

    /// Returns the currently focused [`TextInputWidget`] identifier, if any.
    #[must_use]
    pub fn focused_input(&self) -> Option<&str> {
        self.focused_input.as_deref()
    }

    /// Returns the currently open [`SelectWidget`] identifier, if any.
    #[must_use]
    pub fn open_select(&self) -> Option<&str> {
        self.open_select.as_deref()
    }
}

/// Renders any [`WidgetNode`] into a native GPUI [`AnyElement`] styled by `theme`.
#[must_use]
pub fn render_widget_node(
    node: &WidgetNode,
    state: &PluginUiState,
    theme: &KovaTheme,
    on_action: &PluginActionCallback,
) -> AnyElement {
    match node {
        WidgetNode::Split(split) => render_split(split, state, theme, on_action),
        WidgetNode::Tabs(tabs) => render_tabs(tabs, state, theme, on_action),
        WidgetNode::Tree(tree) => render_tree(tree, state, theme, on_action),
        WidgetNode::Table(table) => render_table(table, state, theme, on_action),
        WidgetNode::List(list) => render_list(list, state, theme, on_action),
        WidgetNode::Form(form) => render_form(form, state, theme, on_action),
        WidgetNode::CodeBlock(block) => {
            let key = (block.code.clone(), block.language.clone());
            let prepared = state
                .code_cache
                .get(&key)
                .cloned()
                .unwrap_or_else(|| prepare_code_block(&block.code, block.language.as_deref()));
            render_code_block(block, &prepared, theme).into_any_element()
        }
        WidgetNode::Markdown(md) => render_markdown(md, state, theme, on_action),
        WidgetNode::Badge(badge) => render_badge(badge, theme).into_any_element(),
    }
}

fn render_split(
    split: &SplitWidget,
    state: &PluginUiState,
    theme: &KovaTheme,
    on_action: &PluginActionCallback,
) -> AnyElement {
    let border_subtle = hsla_from_rgba(theme.borders().subtle);
    let ratio = split.ratio.clamp(0.05, 0.95);
    let first = render_widget_node(&split.first, state, theme, on_action);
    let second = render_widget_node(&split.second, state, theme, on_action);

    match split.axis {
        SplitDirection::Horizontal => div()
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .flex()
            .flex_row()
            .child(
                div()
                    .w(relative(ratio))
                    .h_full()
                    .min_w(px(0.0))
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .child(first),
            )
            .child(div().w(px(1.0)).h_full().flex_none().bg(border_subtle))
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .min_w(px(0.0))
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .child(second),
            )
            .into_any_element(),
        SplitDirection::Vertical => div()
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .child(
                div()
                    .h(relative(ratio))
                    .w_full()
                    .min_w(px(0.0))
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .child(first),
            )
            .child(div().h(px(1.0)).w_full().flex_none().bg(border_subtle))
            .child(
                div()
                    .flex_1()
                    .w_full()
                    .min_w(px(0.0))
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .child(second),
            )
            .into_any_element(),
    }
}

fn render_tabs(
    tabs: &TabsWidget,
    state: &PluginUiState,
    theme: &KovaTheme,
    on_action: &PluginActionCallback,
) -> AnyElement {
    let palette = ControlPalette::from_theme(theme);
    let surface_island = hsla_from_rgba(theme.surfaces().island);
    let border_subtle = hsla_from_rgba(theme.borders().subtle);
    let text_muted = hsla_from_rgba(theme.text().muted);
    let indicator = if tabs.variant == StyleVariant::Primary {
        hsla_from_rgba(theme.borders().tab_indicator)
    } else {
        palette.variant(tabs.variant.into()).background
    };

    let active_idx = state
        .tab_overrides
        .get(&tabs.id)
        .copied()
        .unwrap_or(tabs.active_index);
    let clamped_idx = if tabs.tabs.is_empty() {
        0
    } else {
        active_idx.min(tabs.tabs.len() - 1)
    };

    let mut bar = div()
        .w_full()
        .h(px(30.0))
        .flex_none()
        .flex()
        .items_center()
        .px(px(8.0))
        .gap(px(4.0))
        .bg(surface_island)
        .border_b_1()
        .border_color(border_subtle);

    for (index, tab) in tabs.tabs.iter().enumerate() {
        let is_active = index == clamped_idx;
        bar = bar.child(render_tab_item(
            &tabs.id, index, tab, is_active, indicator, theme, on_action,
        ));
    }

    let body = tabs.tabs.get(clamped_idx).map_or_else(
        || {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(12.0))
                .text_color(text_muted)
                .child("No tabs.")
                .into_any_element()
        },
        |active_tab| render_widget_node(&active_tab.content, state, theme, on_action),
    );

    div()
        .size_full()
        .min_w(px(0.0))
        .min_h(px(0.0))
        .flex()
        .flex_col()
        .child(bar)
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .min_h(px(0.0))
                .overflow_hidden()
                .child(body),
        )
        .into_any_element()
}

fn render_tab_item(
    group_id: &str,
    index: usize,
    tab: &kova_plugin_host::ui::TabNode,
    is_active: bool,
    indicator: gpui::Hsla,
    theme: &KovaTheme,
    on_action: &PluginActionCallback,
) -> gpui::Stateful<Div> {
    let surface_hover = hsla_from_rgba(theme.surfaces().hover);
    let text_primary = hsla_from_rgba(theme.text().primary);
    let text_muted = hsla_from_rgba(theme.text().muted);
    let widget_id = group_id.to_owned();
    let tab_id = tab.id.clone();
    let cb = Rc::clone(on_action);

    let mut tab_el = div()
        .id((SharedString::from(format!("tab-{group_id}")), index))
        .h_full()
        .px(px(10.0))
        .flex()
        .items_center()
        .gap(px(6.0))
        .text_size(px(12.0))
        .font_weight(if is_active {
            FontWeight::MEDIUM
        } else {
            FontWeight::NORMAL
        })
        .text_color(if is_active { text_primary } else { text_muted })
        .cursor_pointer()
        .hover(move |s| s.bg(surface_hover))
        .on_click(move |_: &ClickEvent, window, cx| {
            cb(
                &PluginRendererAction::Emit(UiEvent::TabChanged {
                    widget_id: widget_id.clone(),
                    index,
                    tab_id: tab_id.clone(),
                }),
                window,
                cx,
            );
        })
        .child(tab.title.clone());

    if is_active {
        tab_el = tab_el.border_b_2().border_color(indicator);
    }
    if let Some(badge) = &tab.badge {
        tab_el = tab_el.child(render_badge(badge, theme));
    }
    tab_el
}

fn render_tree(
    tree: &TreeWidget,
    state: &PluginUiState,
    theme: &KovaTheme,
    on_action: &PluginActionCallback,
) -> AnyElement {
    let text_muted = hsla_from_rgba(theme.text().muted);
    let empty_overrides = HashMap::new();
    let expand_overrides = state
        .tree_expanded
        .get(&tree.id)
        .unwrap_or(&empty_overrides);
    let rows = flatten_tree(&tree.nodes, expand_overrides);

    if rows.is_empty() {
        return div()
            .size_full()
            .p(px(12.0))
            .text_size(px(12.0))
            .text_color(text_muted)
            .child("Empty tree.")
            .into_any_element();
    }

    let selected_id = state
        .tree_selected
        .get(&tree.id)
        .cloned()
        .or_else(|| tree.selected_id.clone());
    let scroll_handle = state
        .scroll_handles
        .get(&format!("tree:{}", tree.id))
        .cloned()
        .unwrap_or_default();
    let widget_id = tree.id.clone();
    let on_action = Rc::clone(on_action);
    let row_count = rows.len();

    uniform_list(
        SharedString::from(format!("plugin-tree-{}", tree.id)),
        row_count,
        move |range, _window, cx| {
            let theme = cx.global::<KovaTheme>();
            let end = range.end.min(rows.len());
            let start = range.start.min(end);
            rows[start..end]
                .iter()
                .enumerate()
                .map(|(offset, row)| {
                    render_tree_row(
                        &widget_id,
                        start + offset,
                        row,
                        selected_id.as_deref() == Some(row.id.as_str()),
                        theme,
                        &on_action,
                    )
                })
                .collect()
        },
    )
    .track_scroll(scroll_handle)
    .size_full()
    .py(px(4.0))
    .into_any_element()
}

fn render_tree_row(
    widget_id: &str,
    index: usize,
    row: &FlattenedTreeNode,
    is_selected: bool,
    theme: &KovaTheme,
    on_action: &PluginActionCallback,
) -> gpui::Stateful<Div> {
    let palette = ControlPalette::from_theme(theme);
    let hover = hsla_from_rgba(theme.surfaces().hover);
    let active = hsla_from_rgba(theme.surfaces().active);
    let text_primary = hsla_from_rgba(theme.text().primary);
    let text_secondary = hsla_from_rgba(theme.text().secondary);
    let muted = hsla_from_rgba(theme.text().muted);

    let indent = 8.0 + f32::from(row.depth) * TREE_INDENT_PX;
    let tree_id_sel = widget_id.to_owned();
    let node_id_sel = row.id.clone();
    let cb_sel = Rc::clone(on_action);

    let chevron: AnyElement = if row.has_children {
        let tree_id_tog = widget_id.to_owned();
        let node_id_tog = row.id.clone();
        let next_expanded = !row.expanded;
        let cb_tog = Rc::clone(on_action);
        div()
            .id((SharedString::from(format!("tree-tog-{widget_id}")), index))
            .w(px(16.0))
            .h(px(16.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(3.0))
            .text_color(muted)
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .on_click(move |_: &ClickEvent, window, cx| {
                cx.stop_propagation();
                cb_tog(
                    &PluginRendererAction::Emit(UiEvent::TreeNodeToggled {
                        widget_id: tree_id_tog.clone(),
                        node_id: node_id_tog.clone(),
                        expanded: next_expanded,
                    }),
                    window,
                    cx,
                );
            })
            .child(if row.expanded { "▾" } else { "▸" })
            .into_any_element()
    } else {
        div().w(px(16.0)).flex_none().into_any_element()
    };

    let label_color = if row.variant == StyleVariant::Muted {
        text_primary
    } else {
        palette.variant(row.variant.into()).subtle_fg
    };

    let mut el = div()
        .id((SharedString::from(format!("tree-row-{widget_id}")), index))
        .w_full()
        .h(px(TREE_ROW_HEIGHT_PX))
        .pl(px(indent))
        .pr(px(8.0))
        .flex()
        .items_center()
        .gap(px(6.0))
        .text_size(px(12.0))
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .on_click(move |_: &ClickEvent, window, cx| {
            cb_sel(
                &PluginRendererAction::Emit(UiEvent::TreeNodeSelected {
                    widget_id: tree_id_sel.clone(),
                    node_id: node_id_sel.clone(),
                }),
                window,
                cx,
            );
        })
        .child(chevron)
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_color(label_color)
                .child(row.label.clone()),
        );

    if is_selected {
        el = el.bg(active);
    }
    if let Some(secondary) = &row.secondary {
        el = el.child(
            div()
                .flex_none()
                .text_size(px(11.0))
                .text_color(text_secondary)
                .child(secondary.clone()),
        );
    }
    if let Some(badge) = &row.badge {
        el = el.child(render_badge(badge, theme));
    }
    el
}

fn render_table(
    table: &kova_plugin_host::ui::TableWidget,
    state: &PluginUiState,
    theme: &KovaTheme,
    on_action: &PluginActionCallback,
) -> AnyElement {
    let selected_row = state
        .table_selected
        .get(&table.id)
        .copied()
        .or(table.selected_row);
    let scroll_handle = state
        .scroll_handles
        .get(&format!("table:{}", table.id))
        .cloned()
        .unwrap_or_default();
    let cb = Rc::clone(on_action);
    let table_cb: TableEventCallback =
        Rc::new(move |ev: UiEvent, window: &mut Window, cx: &mut App| {
            cb(&PluginRendererAction::Emit(ev), window, cx);
        });
    render_virtual_table(table, selected_row, scroll_handle, theme, &table_cb).into_any_element()
}

fn render_list(
    list: &ListWidget,
    state: &PluginUiState,
    theme: &KovaTheme,
    on_action: &PluginActionCallback,
) -> AnyElement {
    let text_muted = hsla_from_rgba(theme.text().muted);
    if list.items.is_empty() {
        let msg = list
            .empty_message
            .as_deref()
            .unwrap_or("No items.")
            .to_owned();
        return div()
            .size_full()
            .p(px(12.0))
            .text_size(px(12.0))
            .text_color(text_muted)
            .child(msg)
            .into_any_element();
    }

    let has_subtitles = list.items.iter().any(|item| item.subtitle.is_some());
    let row_height = if has_subtitles {
        LIST_SUBTITLE_ROW_HEIGHT_PX
    } else {
        LIST_ROW_HEIGHT_PX
    };
    let selected_index = state
        .list_selected
        .get(&list.id)
        .copied()
        .or(list.selected_index);
    let scroll_handle = state
        .scroll_handles
        .get(&format!("list:{}", list.id))
        .cloned()
        .unwrap_or_default();
    let items = list.items.clone();
    let widget_id = list.id.clone();
    let on_action = Rc::clone(on_action);

    uniform_list(
        SharedString::from(format!("plugin-list-{}", list.id)),
        items.len(),
        move |range, _window, cx| {
            let theme = cx.global::<KovaTheme>();
            let end = range.end.min(items.len());
            let start = range.start.min(end);
            items[start..end]
                .iter()
                .enumerate()
                .map(|(offset, item)| {
                    let item_index = start + offset;
                    render_list_row(
                        &widget_id,
                        item_index,
                        item,
                        row_height,
                        selected_index == Some(item_index),
                        theme,
                        &on_action,
                    )
                })
                .collect()
        },
    )
    .track_scroll(scroll_handle)
    .size_full()
    .into_any_element()
}

fn render_list_row(
    widget_id: &str,
    item_index: usize,
    item: &kova_plugin_host::ui::ListItem,
    row_height: f32,
    is_selected: bool,
    theme: &KovaTheme,
    on_action: &PluginActionCallback,
) -> gpui::Stateful<Div> {
    let palette = ControlPalette::from_theme(theme);
    let hover = hsla_from_rgba(theme.surfaces().hover);
    let active = hsla_from_rgba(theme.surfaces().active);
    let border_subtle = hsla_from_rgba(theme.borders().subtle);
    let text_primary = hsla_from_rgba(theme.text().primary);
    let text_secondary = hsla_from_rgba(theme.text().secondary);

    let list_id = widget_id.to_owned();
    let item_id = item.id.clone();
    let cb = Rc::clone(on_action);

    let title_color = if item.variant == StyleVariant::Muted {
        text_primary
    } else {
        palette.variant(item.variant.into()).subtle_fg
    };

    let mut text_col = div()
        .flex_1()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .justify_center()
        .child(
            div()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(px(12.0))
                .text_color(title_color)
                .child(item.title.clone()),
        );

    if let Some(subtitle) = &item.subtitle {
        text_col = text_col.child(
            div()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(px(10.5))
                .text_color(text_secondary)
                .child(subtitle.clone()),
        );
    }

    let mut row = div()
        .id((
            SharedString::from(format!("list-row-{widget_id}")),
            item_index,
        ))
        .w_full()
        .h(px(row_height))
        .px(px(10.0))
        .flex()
        .items_center()
        .justify_between()
        .gap(px(8.0))
        .border_b_1()
        .border_color(border_subtle)
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .on_click(move |event: &ClickEvent, window, cx| {
            let ui_event = if event.click_count() >= 2 {
                UiEvent::ListItemActivated {
                    widget_id: list_id.clone(),
                    item_index,
                    item_id: item_id.clone(),
                }
            } else {
                UiEvent::ListItemSelected {
                    widget_id: list_id.clone(),
                    item_index,
                    item_id: item_id.clone(),
                }
            };
            cb(&PluginRendererAction::Emit(ui_event), window, cx);
        })
        .child(text_col);

    if is_selected {
        row = row.bg(active);
    }
    if let Some(badge) = &item.badge {
        row = row.child(render_badge(badge, theme));
    }
    row
}

fn render_form(
    form: &FormWidget,
    state: &PluginUiState,
    theme: &KovaTheme,
    on_action: &PluginActionCallback,
) -> AnyElement {
    let palette = ControlPalette::from_theme(theme);
    let border_subtle = hsla_from_rgba(theme.borders().subtle);
    let surface_island = hsla_from_rgba(theme.surfaces().island);

    let mut container = div()
        .id(SharedString::from(format!("form-{}", form.id)))
        .size_full()
        .p(px(12.0))
        .flex()
        .flex_col()
        .gap(px(10.0))
        .bg(surface_island)
        .overflow_y_scroll();

    if let Some(title) = &form.title {
        let heading_color = if form.variant == StyleVariant::Muted {
            palette.text
        } else {
            palette.variant(form.variant.into()).subtle_fg
        };
        container = container.child(
            div()
                .pb(px(6.0))
                .border_b_1()
                .border_color(border_subtle)
                .text_size(px(13.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(heading_color)
                .child(title.clone()),
        );
    }

    for field in &form.fields {
        container = container.child(render_form_field(field, state, &palette, on_action));
    }

    container.into_any_element()
}

fn render_form_field(
    field: &FormField,
    state: &PluginUiState,
    palette: &ControlPalette,
    on_action: &PluginActionCallback,
) -> Div {
    let mut row = div().w_full().flex().flex_col().gap(px(4.0));

    let control_el = match &field.control {
        FormControl::TextInput(input) => render_text_input(input, state, palette, on_action),
        FormControl::Select(select) => render_select(select, state, palette, on_action),
        FormControl::Toggle(toggle) => render_toggle(toggle, state, palette, on_action),
        FormControl::Button(button) => render_button(button, palette, on_action),
    };

    if field.label.is_some() || field.hint.is_some() {
        let mut label_col = div().flex_1().min_w(px(0.0)).flex().flex_col().gap(px(2.0));
        if let Some(label) = &field.label {
            label_col = label_col.child(
                div()
                    .text_size(px(12.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(palette.text)
                    .child(label.clone()),
            );
        }
        if let Some(hint) = &field.hint {
            label_col = label_col.child(
                div()
                    .text_size(px(11.0))
                    .text_color(palette.text_secondary)
                    .child(hint.clone()),
            );
        }
        row = row.child(
            div()
                .w_full()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(12.0))
                .child(label_col)
                .child(control_el),
        );
    } else {
        row = row.child(div().w_full().flex().items_center().child(control_el));
    }

    if let FormControl::Select(select) = &field.control
        && state.open_select.as_deref() == Some(select.id.as_str())
    {
        row = row.child(render_select_dropdown(select, state, palette, on_action));
    }

    row
}

fn render_text_input(
    input: &TextInputWidget,
    state: &PluginUiState,
    palette: &ControlPalette,
    on_action: &PluginActionCallback,
) -> AnyElement {
    let focused = !input.disabled && state.focused_input.as_deref() == Some(input.id.as_str());
    let invalid = input.variant == StyleVariant::Error;
    let mut control = if focused {
        state
            .text_inputs
            .get(&input.id)
            .map_or_else(|| TextInput::new(&input.value), TextInput::editing)
    } else {
        let current = state
            .text_inputs
            .get(&input.id)
            .map_or(input.value.as_str(), TextInputState::text);
        TextInput::new(current)
    }
    .placeholder(&input.placeholder)
    .invalid(invalid);
    control.focused = focused;

    let mut el = control.render(
        SharedString::from(format!("input-{}", input.id)),
        220.0,
        palette,
    );
    if !input.disabled {
        let id = input.id.clone();
        let cb = Rc::clone(on_action);
        el = el.on_click(move |_: &ClickEvent, window, cx| {
            cb(
                &PluginRendererAction::FocusTextInput(id.clone()),
                window,
                cx,
            );
        });
    }
    el.into_any_element()
}

fn render_select(
    select: &SelectWidget,
    state: &PluginUiState,
    palette: &ControlPalette,
    on_action: &PluginActionCallback,
) -> AnyElement {
    let current = state
        .select_overrides
        .get(&select.id)
        .or(select.selected_value.as_ref());
    let label = current
        .and_then(|val| select.options.iter().find(|o| &o.value == val))
        .map_or_else(
            || {
                select
                    .options
                    .first()
                    .map_or_else(|| "Select...".to_owned(), |o| o.label.clone())
            },
            |o| o.label.clone(),
        );
    let open = !select.disabled && state.open_select.as_deref() == Some(select.id.as_str());
    let mut el = SelectInput {
        label,
        focused: open,
        open,
    }
    .render(SharedString::from(format!("select-{}", select.id)), palette);

    if !select.disabled {
        let id = select.id.clone();
        let cb = Rc::clone(on_action);
        el = el.on_click(move |_: &ClickEvent, window, cx| {
            cb(
                &PluginRendererAction::ToggleSelectDropdown(id.clone()),
                window,
                cx,
            );
        });
    }
    el.into_any_element()
}

fn render_select_dropdown(
    select: &SelectWidget,
    state: &PluginUiState,
    palette: &ControlPalette,
    on_action: &PluginActionCallback,
) -> Div {
    let current = state
        .select_overrides
        .get(&select.id)
        .or(select.selected_value.as_ref());
    let mut list = div()
        .ml_auto()
        .w(px(220.0))
        .p(px(4.0))
        .flex()
        .flex_col()
        .gap(px(2.0))
        .rounded(px(6.0))
        .border_1()
        .border_color(palette.border)
        .bg(palette.elevated);

    for (idx, opt) in select.options.iter().enumerate() {
        let selected = current.is_some_and(|v| v == &opt.value);
        let select_id = select.id.clone();
        let value = opt.value.clone();
        let cb = Rc::clone(on_action);

        list = list.child(
            SelectInput::option_row(
                (SharedString::from(format!("opt-{}", select.id)), idx),
                opt.label.clone(),
                selected,
                palette,
            )
            .on_click(move |_: &ClickEvent, window, cx| {
                cb(
                    &PluginRendererAction::Emit(UiEvent::SelectChanged {
                        widget_id: select_id.clone(),
                        value: value.clone(),
                    }),
                    window,
                    cx,
                );
            }),
        );
    }
    list
}

fn render_toggle(
    toggle: &ToggleWidget,
    state: &PluginUiState,
    palette: &ControlPalette,
    on_action: &PluginActionCallback,
) -> AnyElement {
    let checked = state
        .toggle_overrides
        .get(&toggle.id)
        .copied()
        .unwrap_or(toggle.checked);
    let mut el = ToggleSwitch {
        on: checked,
        focused: false,
    }
    .render(SharedString::from(format!("toggle-{}", toggle.id)), palette);

    if !toggle.disabled {
        let id = toggle.id.clone();
        let next = !checked;
        let cb = Rc::clone(on_action);
        el = el.on_click(move |_: &ClickEvent, window, cx| {
            cb(
                &PluginRendererAction::Emit(UiEvent::ToggleChanged {
                    widget_id: id.clone(),
                    checked: next,
                }),
                window,
                cx,
            );
        });
    }
    el.into_any_element()
}

fn render_button(
    button: &ButtonWidget,
    palette: &ControlPalette,
    on_action: &PluginActionCallback,
) -> AnyElement {
    let variant: PluginVariant = button.variant.into();
    let btn = if button.ghost {
        Button::ghost(button.label.clone(), variant)
    } else {
        Button::new(button.label.clone(), variant)
    };
    let mut el = btn.render(SharedString::from(format!("btn-{}", button.id)), palette);
    if !button.disabled {
        let id = button.id.clone();
        let cb = Rc::clone(on_action);
        el = el.on_click(move |_: &ClickEvent, window, cx| {
            cb(
                &PluginRendererAction::Emit(UiEvent::ButtonClicked {
                    widget_id: id.clone(),
                }),
                window,
                cx,
            );
        });
    }
    el.into_any_element()
}

fn render_markdown(
    md: &MarkdownWidget,
    state: &PluginUiState,
    theme: &KovaTheme,
    on_action: &PluginActionCallback,
) -> AnyElement {
    let blocks = parse_markdown(&md.content);

    let mut container = div()
        .id(SharedString::from(format!("md-{}", md.id)))
        .size_full()
        .p(px(12.0))
        .flex()
        .flex_col()
        .gap(px(8.0))
        .overflow_y_scroll();

    for (block_idx, block) in blocks.into_iter().enumerate() {
        container = container.child(render_markdown_block(
            md, block_idx, block, state, theme, on_action,
        ));
    }

    container.into_any_element()
}

fn render_markdown_block(
    md: &MarkdownWidget,
    block_idx: usize,
    block: MarkdownBlock,
    state: &PluginUiState,
    theme: &KovaTheme,
    on_action: &PluginActionCallback,
) -> AnyElement {
    let palette = ControlPalette::from_theme(theme);
    let border_subtle = hsla_from_rgba(theme.borders().subtle);
    let accent_color = palette.variant(md.variant.into()).border;

    match block {
        MarkdownBlock::Heading { level, spans } => {
            let size = match level {
                1 => 18.0,
                2 => 15.0,
                3 => 13.5,
                _ => 12.5,
            };
            div()
                .pt(px(if level == 1 { 2.0 } else { 4.0 }))
                .text_size(px(size))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(palette.text)
                .child(render_inline_spans(
                    &md.id, block_idx, &spans, theme, on_action,
                ))
                .into_any_element()
        }
        MarkdownBlock::Paragraph(spans) => div()
            .text_size(px(12.5))
            .text_color(palette.text)
            .child(render_inline_spans(
                &md.id, block_idx, &spans, theme, on_action,
            ))
            .into_any_element(),
        MarkdownBlock::ListItem {
            ordered_index,
            spans,
        } => {
            let marker = ordered_index.map_or_else(|| "•".to_owned(), |n| format!("{n}."));
            div()
                .pl(px(8.0))
                .flex()
                .items_start()
                .gap(px(6.0))
                .text_size(px(12.5))
                .text_color(palette.text)
                .child(
                    div()
                        .flex_none()
                        .text_color(palette.text_secondary)
                        .child(marker),
                )
                .child(div().flex_1().min_w(px(0.0)).child(render_inline_spans(
                    &md.id, block_idx, &spans, theme, on_action,
                )))
                .into_any_element()
        }
        MarkdownBlock::Quote(spans) => div()
            .pl(px(10.0))
            .py(px(4.0))
            .border_l_2()
            .border_color(accent_color)
            .bg(palette.variant(md.variant.into()).subtle_bg)
            .text_size(px(12.0))
            .text_color(palette.text_secondary)
            .child(render_inline_spans(
                &md.id, block_idx, &spans, theme, on_action,
            ))
            .into_any_element(),
        MarkdownBlock::Code { language, code } => {
            let key = (code.clone(), language.clone());
            let prepared = state
                .code_cache
                .get(&key)
                .cloned()
                .unwrap_or_else(|| prepare_code_block(&code, language.as_deref()));
            let block_widget = CodeBlockWidget {
                id: format!("{}-code-{block_idx}", md.id),
                code,
                language,
                show_line_numbers: true,
                variant: StyleVariant::Muted,
            };
            render_code_block(&block_widget, &prepared, theme).into_any_element()
        }
        MarkdownBlock::Rule => div()
            .w_full()
            .h(px(1.0))
            .my(px(4.0))
            .bg(border_subtle)
            .into_any_element(),
    }
}

fn render_inline_spans(
    widget_id: &str,
    block_idx: usize,
    spans: &[InlineSpan],
    theme: &KovaTheme,
    on_action: &PluginActionCallback,
) -> AnyElement {
    let has_links = spans.iter().any(|s| matches!(s, InlineSpan::Link { .. }));

    if !has_links {
        let mut full = String::new();
        let mut highlights = Vec::new();
        let inline_code_color = hsla_from_rgba(theme.tokens().plugins.accent.subtle_fg);

        for span in spans {
            let start = full.len();
            match span {
                InlineSpan::Plain(t) => full.push_str(t),
                InlineSpan::Bold(t) => {
                    full.push_str(t);
                    highlights.push((
                        start..full.len(),
                        HighlightStyle {
                            font_weight: Some(FontWeight::BOLD),
                            ..HighlightStyle::default()
                        },
                    ));
                }
                InlineSpan::Italic(t) => {
                    full.push_str(t);
                    highlights.push((
                        start..full.len(),
                        HighlightStyle {
                            font_style: Some(FontStyle::Italic),
                            ..HighlightStyle::default()
                        },
                    ));
                }
                InlineSpan::Code(t) => {
                    full.push_str(t);
                    highlights.push((
                        start..full.len(),
                        HighlightStyle {
                            color: Some(inline_code_color),
                            ..HighlightStyle::default()
                        },
                    ));
                }
                InlineSpan::Link { label, .. } => full.push_str(label),
            }
        }
        return StyledText::new(full)
            .with_highlights(highlights)
            .into_any_element();
    }

    let palette = ControlPalette::from_theme(theme);
    let link_color = palette.accent.subtle_fg;
    let code_bg = palette.muted.subtle_bg;
    let mut row = div().flex().flex_wrap().items_center().gap(px(4.0));

    for (span_idx, span) in spans.iter().enumerate() {
        let child: AnyElement = match span {
            InlineSpan::Plain(t) => div().child(t.clone()).into_any_element(),
            InlineSpan::Bold(t) => div()
                .font_weight(FontWeight::BOLD)
                .child(t.clone())
                .into_any_element(),
            InlineSpan::Italic(t) => div().italic().child(t.clone()).into_any_element(),
            InlineSpan::Code(t) => div()
                .px(px(4.0))
                .rounded(px(3.0))
                .bg(code_bg)
                .font_family("JetBrains Mono")
                .text_size(px(11.5))
                .text_color(link_color)
                .child(t.clone())
                .into_any_element(),
            InlineSpan::Link { label, href } => {
                let md_id = widget_id.to_owned();
                let target = href.clone();
                let cb = Rc::clone(on_action);
                div()
                    .id((
                        SharedString::from(format!("md-link-{widget_id}-{block_idx}")),
                        span_idx,
                    ))
                    .text_color(link_color)
                    .underline()
                    .cursor_pointer()
                    .on_click(move |_: &ClickEvent, window, cx| {
                        cb(
                            &PluginRendererAction::Emit(UiEvent::LinkClicked {
                                widget_id: md_id.clone(),
                                href: target.clone(),
                            }),
                            window,
                            cx,
                        );
                    })
                    .child(label.clone())
                    .into_any_element()
            }
        };
        row = row.child(child);
    }

    row.into_any_element()
}

fn render_badge(badge: &BadgeWidget, theme: &KovaTheme) -> Div {
    let palette = ControlPalette::from_theme(theme);
    Badge::new(badge.label.clone(), badge.variant.into()).render(&palette)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flattens_tree_respecting_expand_state_and_overrides() {
        let nodes = vec![TreeNode {
            id: "root".to_owned(),
            label: "schema".to_owned(),
            secondary: None,
            badge: None,
            variant: StyleVariant::Primary,
            expanded: false,
            children: vec![TreeNode {
                id: "child".to_owned(),
                label: "users".to_owned(),
                secondary: Some("table".to_owned()),
                badge: None,
                variant: StyleVariant::Muted,
                expanded: false,
                children: Vec::new(),
            }],
        }];

        let collapsed = flatten_tree(&nodes, &HashMap::new());
        assert_eq!(collapsed.len(), 1);
        assert!(collapsed[0].has_children);
        assert!(!collapsed[0].expanded);

        let mut overrides = HashMap::new();
        overrides.insert("root".to_owned(), true);
        let expanded = flatten_tree(&nodes, &overrides);
        assert_eq!(expanded.len(), 2);
        assert_eq!(expanded[1].depth, 1);
        assert_eq!(expanded[1].id, "child");
    }

    #[test]
    fn parses_markdown_headings_lists_code_and_links() {
        let md = "# Title\n\nHello **world** with `code` and [Kova](https://kova.dev).\n\n- first\n1. second\n\n> note\n\n```json\n{\"a\": 1}\n```\n---";
        let blocks = parse_markdown(md);
        assert_eq!(
            blocks,
            vec![
                MarkdownBlock::Heading {
                    level: 1,
                    spans: vec![InlineSpan::Plain("Title".to_owned())],
                },
                MarkdownBlock::Paragraph(vec![
                    InlineSpan::Plain("Hello ".to_owned()),
                    InlineSpan::Bold("world".to_owned()),
                    InlineSpan::Plain(" with ".to_owned()),
                    InlineSpan::Code("code".to_owned()),
                    InlineSpan::Plain(" and ".to_owned()),
                    InlineSpan::Link {
                        label: "Kova".to_owned(),
                        href: "https://kova.dev".to_owned(),
                    },
                    InlineSpan::Plain(".".to_owned()),
                ]),
                MarkdownBlock::ListItem {
                    ordered_index: None,
                    spans: vec![InlineSpan::Plain("first".to_owned())],
                },
                MarkdownBlock::ListItem {
                    ordered_index: Some(1),
                    spans: vec![InlineSpan::Plain("second".to_owned())],
                },
                MarkdownBlock::Quote(vec![InlineSpan::Plain("note".to_owned())]),
                MarkdownBlock::Code {
                    language: Some("json".to_owned()),
                    code: "{\"a\": 1}".to_owned(),
                },
                MarkdownBlock::Rule,
            ]
        );
    }

    #[test]
    fn local_state_handles_text_input_typing_and_submit() {
        let mut state = PluginUiState::default();
        state.apply_action(&PluginRendererAction::FocusTextInput("q".to_owned()));
        assert_eq!(state.focused_input(), Some("q"));

        assert_eq!(
            state.handle_key_stroke("a", Some("a"), false),
            Some(UiEvent::TextChanged {
                widget_id: "q".to_owned(),
                value: "a".to_owned(),
            })
        );
        assert_eq!(
            state.handle_key_stroke("enter", None, false),
            Some(UiEvent::TextSubmitted {
                widget_id: "q".to_owned(),
                value: "a".to_owned(),
            })
        );
    }
}
