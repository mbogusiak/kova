//! Native GPUI live preview renderer for Markdown documents (`KOVA-057`).
//!
//! Renders [`kova_core::markdown::MarkdownDocument`] directly with GPUI primitives
//! (`StyledText`, `InteractiveText`, `ScrollHandle`, and [`KovaTheme`] semantic
//! tokens) without HTML or a `WebView`, caching parsed ASTs and highlighted
//! code blocks by [`kova_buffer::BufferVersion`].

use std::collections::HashMap;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    AnyElement, Context, Div, EventEmitter, FocusHandle, FontStyle, FontWeight, HighlightStyle,
    Hsla, InteractiveElement as _, InteractiveText, IntoElement, MouseButton, ParentElement as _,
    Render, ScrollHandle, SharedString, StatefulInteractiveElement as _, StrikethroughStyle,
    Styled as _, StyledText, UnderlineStyle, Window, div, px,
};
use kova_core::markdown::{
    InlineSpan, ListItem, ListMarkerKind, MarkdownBlock as CoreMarkdownBlock, MarkdownDocument,
    TableAlignment, TableBlock, parse_markdown,
};

use crate::editor::shared::SharedBuffer;
use crate::plugin_ui::code_block::{HighlightedCodeLine, PreparedCodeBlock, prepare_code_block};
use crate::theme::{KovaTheme, hsla_from_rgba};

const CODE_FONT_FAMILY: &str = "JetBrains Mono";
const CODE_FONT_SIZE_PX: f32 = 12.5;
const CODE_LINE_HEIGHT_PX: f32 = 18.5;
const BODY_FONT_SIZE_PX: f32 = 13.5;
const BODY_LINE_HEIGHT_PX: f32 = 21.0;

/// Events emitted by [`MarkdownPreviewView`] to the hosting [`crate::WorkspaceShell`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkdownPreviewEvent {
    /// User clicked inside the preview pane, requesting active pane focus.
    Focused,
    /// User clicked a relative file link (e.g. `docs/SPEC.md`) inside the document.
    OpenRelativePath(PathBuf),
}

/// Revision-keyed cache holding the latest parsed Markdown AST and highlighted code blocks.
///
/// Separated from GPUI's `Context`/`FocusHandle` so cache invalidation and reuse can be
/// unit-tested headlessly without creating a window.
#[derive(Debug, Clone)]
pub struct MarkdownPreviewCache {
    cached_version: Option<u64>,
    cached_document: Arc<MarkdownDocument>,
    code_cache: HashMap<(Option<String>, String), PreparedCodeBlock>,
}

impl Default for MarkdownPreviewCache {
    fn default() -> Self {
        Self::new()
    }
}

impl MarkdownPreviewCache {
    /// Creates an empty preview cache with no parsed buffer revision yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            cached_version: None,
            cached_document: Arc::new(MarkdownDocument::default()),
            code_cache: HashMap::new(),
        }
    }

    /// Synchronizes the cache with `buffer` when its [`kova_buffer::BufferVersion`] changed.
    ///
    /// Returns `true` when the document was re-parsed, or `false` in $O(1)$ time when
    /// `buffer.version().as_u64()` matches the cached revision.
    pub fn sync_from_buffer(&mut self, buffer: &SharedBuffer) -> bool {
        let version = buffer.version().as_u64();
        if self.cached_version == Some(version) {
            return false;
        }
        let text = buffer.snapshot().text();
        let doc = parse_markdown(&text);
        let mut next_code_cache = HashMap::new();
        collect_code_blocks(&doc.blocks, &self.code_cache, &mut next_code_cache);
        self.code_cache = next_code_cache;
        self.cached_document = Arc::new(doc);
        self.cached_version = Some(version);
        true
    }

    /// Returns the last [`kova_buffer::BufferVersion`] number synchronized into this cache.
    #[must_use]
    pub const fn cached_version(&self) -> Option<u64> {
        self.cached_version
    }

    /// Returns the cached parsed [`MarkdownDocument`].
    #[must_use]
    pub fn document(&self) -> &Arc<MarkdownDocument> {
        &self.cached_document
    }

    /// Looks up a cached [`PreparedCodeBlock`] by optional language tag and code body.
    #[must_use]
    pub fn prepared_code_block(
        &self,
        language: Option<&str>,
        code: &str,
    ) -> Option<&PreparedCodeBlock> {
        self.code_cache
            .get(&(language.map(str::to_owned), code.to_owned()))
    }
}

fn collect_code_blocks(
    blocks: &[CoreMarkdownBlock],
    prev: &HashMap<(Option<String>, String), PreparedCodeBlock>,
    next: &mut HashMap<(Option<String>, String), PreparedCodeBlock>,
) {
    for block in blocks {
        match block {
            CoreMarkdownBlock::CodeBlock { language, code } => {
                let key = (language.clone(), code.clone());
                next.entry(key).or_insert_with(|| {
                    prev.get(&(language.clone(), code.clone()))
                        .cloned()
                        .unwrap_or_else(|| prepare_code_block(code, language.as_deref()))
                });
            }
            CoreMarkdownBlock::BlockQuote(inner) => {
                collect_code_blocks(inner, prev, next);
            }
            CoreMarkdownBlock::Heading { .. }
            | CoreMarkdownBlock::Paragraph(_)
            | CoreMarkdownBlock::List(_)
            | CoreMarkdownBlock::Table(_)
            | CoreMarkdownBlock::ThematicBreak => {}
        }
    }
}

/// Semantic colors resolved from [`KovaTheme`] for rendering Markdown blocks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MarkdownPalette {
    /// Preview background (`editor.background`).
    pub bg: Hsla,
    /// Default code foreground (`editor.foreground`).
    pub fg: Hsla,
    /// Primary body and heading text (`text.primary`).
    pub text_primary: Hsla,
    /// Secondary text for quotes and list markers (`text.secondary`).
    pub text_secondary: Hsla,
    /// Muted placeholder and caption text (`text.muted`).
    pub text_muted: Hsla,
    /// Inverse text used inside checked task checkboxes (`text.inverse`).
    pub text_inverse: Hsla,
    /// Elevated island surface for code headers, quotes, and table headers (`surfaces.island`).
    pub surface_island: Hsla,
    /// Subtle border and divider color (`borders.subtle`).
    pub border_subtle: Hsla,
    /// Stronger border for unchecked task checkboxes (`borders.strong`).
    pub border_strong: Hsla,
    /// Accent color for quote bars (`borders.focus`).
    pub quote_border: Hsla,
    /// Clickable link color (`tokens.plugins.accent.subtle_fg`).
    pub link_fg: Hsla,
    /// Inline code text color (`tokens.syntax.string.fg`).
    pub inline_code_fg: Hsla,
    /// Inline code chip background (`surfaces.island`).
    pub inline_code_bg: Hsla,
    /// Checked task box fill (`tokens.plugins.primary.background`).
    pub checkbox_fill: Hsla,
}

impl MarkdownPalette {
    /// Resolves all preview colors from the active [`KovaTheme`].
    #[must_use]
    pub fn from_theme(theme: &KovaTheme) -> Self {
        Self {
            bg: hsla_from_rgba(theme.editor().background),
            fg: hsla_from_rgba(theme.editor().foreground),
            text_primary: hsla_from_rgba(theme.text().primary),
            text_secondary: hsla_from_rgba(theme.text().secondary),
            text_muted: hsla_from_rgba(theme.text().muted),
            text_inverse: hsla_from_rgba(theme.text().inverse),
            surface_island: hsla_from_rgba(theme.surfaces().island),
            border_subtle: hsla_from_rgba(theme.borders().subtle),
            border_strong: hsla_from_rgba(theme.borders().strong),
            quote_border: hsla_from_rgba(theme.borders().focus),
            link_fg: hsla_from_rgba(theme.tokens().plugins.accent.subtle_fg),
            inline_code_fg: hsla_from_rgba(theme.tokens().syntax.string.fg),
            inline_code_bg: hsla_from_rgba(theme.surfaces().island),
            checkbox_fill: hsla_from_rgba(theme.tokens().plugins.primary.background),
        }
    }
}

/// Clickable link range inside flattened inline text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlattenedLink {
    /// UTF-8 byte range inside [`FlattenedInline::text`].
    pub range: Range<usize>,
    /// Destination URL or relative file path.
    pub url: String,
}

/// Flattened text, GPUI highlight spans, and clickable link ranges for a sequence of [`InlineSpan`]s.
#[derive(Debug, Clone, Default)]
pub struct FlattenedInline {
    /// Concatenated plain text passed to `StyledText::new`.
    pub text: String,
    /// Disjoint, sorted highlight styles aligned to UTF-8 byte boundaries in `text`.
    pub highlights: Vec<(Range<usize>, HighlightStyle)>,
    /// Disjoint, sorted clickable link ranges aligned to UTF-8 byte boundaries in `text`.
    pub links: Vec<FlattenedLink>,
}

#[derive(Debug, Clone, Copy, Default)]
struct InlineStyleState(u8);

impl InlineStyleState {
    const BOLD: u8 = 1 << 0;
    const ITALIC: u8 = 1 << 1;
    const STRIKETHROUGH: u8 = 1 << 2;
    const IN_LINK: u8 = 1 << 3;

    const fn has(self, flag: u8) -> bool {
        (self.0 & flag) != 0
    }

    const fn with(self, flag: u8) -> Self {
        Self(self.0 | flag)
    }

    const fn has_any(self) -> bool {
        self.0 != 0
    }
}

/// Flattens nested [`InlineSpan`]s into a single UTF-8 string with disjoint GPUI [`HighlightStyle`]
/// and clickable link ranges.
#[must_use]
pub fn flatten_inline_spans(spans: &[InlineSpan], palette: &MarkdownPalette) -> FlattenedInline {
    let mut out = FlattenedInline::default();
    flatten_recursive(spans, InlineStyleState::default(), palette, &mut out);
    out
}

fn flatten_recursive(
    spans: &[InlineSpan],
    state: InlineStyleState,
    palette: &MarkdownPalette,
    out: &mut FlattenedInline,
) {
    for span in spans {
        match span {
            InlineSpan::Text(text) => {
                let start = out.text.len();
                out.text.push_str(text);
                let end = out.text.len();
                if start < end && state.has_any() {
                    out.highlights
                        .push((start..end, style_for_state(state, false, palette)));
                }
            }
            InlineSpan::Code(code) => {
                let start = out.text.len();
                out.text.push_str(code);
                let end = out.text.len();
                if start < end {
                    out.highlights
                        .push((start..end, style_for_state(state, true, palette)));
                }
            }
            InlineSpan::Bold(children) => {
                flatten_recursive(children, state.with(InlineStyleState::BOLD), palette, out);
            }
            InlineSpan::Italic(children) => {
                flatten_recursive(children, state.with(InlineStyleState::ITALIC), palette, out);
            }
            InlineSpan::Strikethrough(children) => {
                flatten_recursive(
                    children,
                    state.with(InlineStyleState::STRIKETHROUGH),
                    palette,
                    out,
                );
            }
            InlineSpan::Link { label, url } => {
                let start = out.text.len();
                flatten_recursive(label, state.with(InlineStyleState::IN_LINK), palette, out);
                let end = out.text.len();
                if start < end && !state.has(InlineStyleState::IN_LINK) && !url.is_empty() {
                    out.links.push(FlattenedLink {
                        range: start..end,
                        url: url.clone(),
                    });
                }
            }
        }
    }
}

fn style_for_state(
    state: InlineStyleState,
    is_code: bool,
    palette: &MarkdownPalette,
) -> HighlightStyle {
    let in_link = state.has(InlineStyleState::IN_LINK);
    let strikethrough = state.has(InlineStyleState::STRIKETHROUGH);
    let color = if in_link {
        Some(palette.link_fg)
    } else if is_code {
        Some(palette.inline_code_fg)
    } else if strikethrough {
        Some(palette.text_secondary)
    } else {
        None
    };

    HighlightStyle {
        color,
        background_color: is_code.then_some(palette.inline_code_bg),
        font_weight: state
            .has(InlineStyleState::BOLD)
            .then_some(FontWeight::BOLD),
        font_style: state
            .has(InlineStyleState::ITALIC)
            .then_some(FontStyle::Italic),
        strikethrough: strikethrough.then_some(StrikethroughStyle {
            thickness: px(1.0),
            color: Some(palette.text_muted),
        }),
        underline: in_link.then_some(UnderlineStyle {
            thickness: px(1.0),
            color: Some(palette.link_fg),
            wavy: false,
        }),
        ..HighlightStyle::default()
    }
}

/// Returns `true` when `url` should be opened by the OS browser/handler rather than inside Kova.
#[must_use]
pub fn is_external_url(url: &str) -> bool {
    let trimmed = url.trim();
    let lower = trimmed.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("mailto:")
}

/// Extracts a relative file path from a non-external Markdown link, stripping any `#anchor`.
#[must_use]
pub fn relative_link_target(url: &str) -> Option<PathBuf> {
    let trimmed = url.trim();
    if trimmed.is_empty() || is_external_url(trimmed) || trimmed.starts_with('#') {
        return None;
    }
    let path_part = trimmed.split('#').next().unwrap_or(trimmed).trim();
    if path_part.is_empty() {
        None
    } else {
        Some(PathBuf::from(path_part))
    }
}

/// Native scrollable GPUI view rendering a live-synced Markdown preview for a [`SharedBuffer`].
pub struct MarkdownPreviewView {
    doc_path: Option<PathBuf>,
    buffer: SharedBuffer,
    cache: MarkdownPreviewCache,
    scroll_handle: ScrollHandle,
    focus_handle: FocusHandle,
    element_id_prefix: String,
}

impl std::fmt::Debug for MarkdownPreviewView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MarkdownPreviewView")
            .field("doc_path", &self.doc_path)
            .field("cached_version", &self.cache.cached_version())
            .field("element_id_prefix", &self.element_id_prefix)
            .finish_non_exhaustive()
    }
}

impl EventEmitter<MarkdownPreviewEvent> for MarkdownPreviewView {}

impl MarkdownPreviewView {
    /// Creates a new preview view bound to `buffer` (shared with its [`crate::EditorView`]).
    #[must_use]
    pub fn new(
        doc_path: Option<PathBuf>,
        buffer: SharedBuffer,
        element_id_prefix: impl Into<String>,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let mut view = Self {
            doc_path,
            buffer,
            cache: MarkdownPreviewCache::new(),
            scroll_handle: ScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            element_id_prefix: element_id_prefix.into(),
        };
        let _ = view.sync_from_buffer();
        view
    }

    /// Returns the shared document handle backing this preview.
    #[must_use]
    pub fn shared_buffer(&self) -> &SharedBuffer {
        &self.buffer
    }

    /// Returns the document path used to resolve relative links, if known.
    #[must_use]
    pub fn doc_path(&self) -> Option<&PathBuf> {
        self.doc_path.as_ref()
    }

    /// Updates the document path (e.g. after *Save As* or moving a tab).
    pub fn set_doc_path(&mut self, doc_path: Option<PathBuf>) {
        self.doc_path = doc_path;
    }

    /// Returns the view's GPUI [`FocusHandle`].
    #[must_use]
    pub const fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    /// Returns the last synchronized [`kova_buffer::BufferVersion`] number.
    #[must_use]
    pub const fn cached_version(&self) -> Option<u64> {
        self.cache.cached_version()
    }

    /// Re-parses the buffer only when its [`kova_buffer::BufferVersion`] differs from cache.
    pub fn sync_from_buffer(&mut self) -> bool {
        self.cache.sync_from_buffer(&self.buffer)
    }

    /// Refreshes the cached AST when the underlying [`SharedBuffer`] changed and requests repaint.
    pub fn refresh_if_needed(&mut self, cx: &mut Context<'_, Self>) {
        if self.sync_from_buffer() {
            cx.notify();
        }
    }

    fn render_inline(
        &self,
        id_suffix: &str,
        spans: &[InlineSpan],
        palette: &MarkdownPalette,
        cx: &Context<'_, Self>,
    ) -> AnyElement {
        let flattened = flatten_inline_spans(spans, palette);
        if flattened.text.is_empty() {
            return div().into_any_element();
        }
        let styled = StyledText::new(flattened.text).with_highlights(flattened.highlights);
        if flattened.links.is_empty() {
            return styled.into_any_element();
        }

        let ranges: Vec<Range<usize>> = flattened
            .links
            .iter()
            .map(|link| link.range.clone())
            .collect();
        let urls: Vec<String> = flattened.links.into_iter().map(|link| link.url).collect();
        let view = cx.weak_entity();
        let element_id = SharedString::from(format!("{}-{id_suffix}", self.element_id_prefix));

        InteractiveText::new(element_id, styled)
            .on_click(ranges, move |ix, _window, cx| {
                let Some(url) = urls.get(ix) else {
                    return;
                };
                if is_external_url(url) {
                    cx.open_url(url);
                } else if let Some(rel) = relative_link_target(url) {
                    view.update(cx, |_, cx| {
                        cx.emit(MarkdownPreviewEvent::OpenRelativePath(rel));
                    })
                    .ok();
                }
            })
            .into_any_element()
    }

    fn render_block(
        &self,
        path_id: &str,
        block: &CoreMarkdownBlock,
        palette: &MarkdownPalette,
        theme: &KovaTheme,
        cx: &Context<'_, Self>,
    ) -> AnyElement {
        match block {
            CoreMarkdownBlock::Heading { level, spans } => {
                let (font_size, weight, color, bottom_rule) = match level {
                    1 => (24.0, FontWeight::BOLD, palette.text_primary, true),
                    2 => (19.5, FontWeight::BOLD, palette.text_primary, true),
                    3 => (16.5, FontWeight::SEMIBOLD, palette.text_primary, false),
                    4 => (14.5, FontWeight::SEMIBOLD, palette.text_primary, false),
                    5 => (13.5, FontWeight::SEMIBOLD, palette.text_secondary, false),
                    _ => (12.5, FontWeight::SEMIBOLD, palette.text_muted, false),
                };
                let mut el = div()
                    .w_full()
                    .min_w(px(0.0))
                    .pt(px(if *level <= 2 { 6.0 } else { 4.0 }))
                    .text_size(px(font_size))
                    .font_weight(weight)
                    .text_color(color)
                    .child(self.render_inline(&format!("{path_id}-h"), spans, palette, cx));
                if bottom_rule {
                    el = el
                        .pb(px(6.0))
                        .border_b_1()
                        .border_color(palette.border_subtle);
                }
                el.into_any_element()
            }
            CoreMarkdownBlock::Paragraph(spans) => div()
                .w_full()
                .min_w(px(0.0))
                .text_size(px(BODY_FONT_SIZE_PX))
                .line_height(px(BODY_LINE_HEIGHT_PX))
                .text_color(palette.text_primary)
                .child(self.render_inline(&format!("{path_id}-p"), spans, palette, cx))
                .into_any_element(),
            CoreMarkdownBlock::CodeBlock { language, code } => {
                self.render_code_block(path_id, language.as_deref(), code, palette, theme)
            }
            CoreMarkdownBlock::BlockQuote(inner) => {
                let mut quote = div()
                    .w_full()
                    .min_w(px(0.0))
                    .pl(px(12.0))
                    .pr(px(10.0))
                    .py(px(8.0))
                    .border_l_2()
                    .border_color(palette.quote_border)
                    .bg(palette.surface_island)
                    .rounded(px(4.0))
                    .text_color(palette.text_secondary)
                    .flex()
                    .flex_col()
                    .gap(px(8.0));
                for (child_idx, child) in inner.iter().enumerate() {
                    quote = quote.child(self.render_block(
                        &format!("{path_id}-q{child_idx}"),
                        child,
                        palette,
                        theme,
                        cx,
                    ));
                }
                quote.into_any_element()
            }
            CoreMarkdownBlock::List(items) => {
                let mut list = div().w_full().min_w(px(0.0)).flex().flex_col().gap(px(5.0));
                for (item_idx, item) in items.iter().enumerate() {
                    list = list.child(self.render_list_item(
                        &format!("{path_id}-li{item_idx}"),
                        item,
                        palette,
                        cx,
                    ));
                }
                list.into_any_element()
            }
            CoreMarkdownBlock::Table(table) => self.render_table(path_id, table, palette, cx),
            CoreMarkdownBlock::ThematicBreak => div()
                .w_full()
                .h(px(1.0))
                .my(px(6.0))
                .bg(palette.border_subtle)
                .into_any_element(),
        }
    }

    fn render_list_item(
        &self,
        item_id: &str,
        item: &ListItem,
        palette: &MarkdownPalette,
        cx: &Context<'_, Self>,
    ) -> AnyElement {
        let indent_px = f32::from(item.indent) * 16.0;
        let marker: AnyElement = match item.kind {
            ListMarkerKind::Bullet => div()
                .w(px(14.0))
                .flex_none()
                .text_color(palette.text_secondary)
                .child(if item.indent % 2 == 0 { "•" } else { "◦" })
                .into_any_element(),
            ListMarkerKind::Ordered(number) => div()
                .min_w(px(18.0))
                .flex_none()
                .text_color(palette.text_secondary)
                .child(format!("{number}."))
                .into_any_element(),
            ListMarkerKind::Task { checked } => {
                let mut box_el = div()
                    .mt(px(3.5))
                    .w(px(14.0))
                    .h(px(14.0))
                    .flex_none()
                    .rounded(px(3.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(10.0))
                    .font_weight(FontWeight::BOLD);
                if checked {
                    box_el = box_el
                        .bg(palette.checkbox_fill)
                        .border_1()
                        .border_color(palette.checkbox_fill)
                        .text_color(palette.text_inverse)
                        .child("✓");
                } else {
                    box_el = box_el.border_1().border_color(palette.border_strong);
                }
                box_el.into_any_element()
            }
        };

        let text_color = match item.kind {
            ListMarkerKind::Task { checked: true } => palette.text_secondary,
            _ => palette.text_primary,
        };

        div()
            .w_full()
            .min_w(px(0.0))
            .pl(px(indent_px))
            .flex()
            .items_start()
            .gap(px(8.0))
            .text_size(px(BODY_FONT_SIZE_PX))
            .line_height(px(BODY_LINE_HEIGHT_PX))
            .text_color(text_color)
            .child(marker)
            .child(div().flex_1().min_w(px(0.0)).child(self.render_inline(
                item_id,
                &item.spans,
                palette,
                cx,
            )))
            .into_any_element()
    }

    fn render_code_block(
        &self,
        path_id: &str,
        language: Option<&str>,
        code: &str,
        palette: &MarkdownPalette,
        theme: &KovaTheme,
    ) -> AnyElement {
        let prepared = self
            .cache
            .prepared_code_block(language, code)
            .cloned()
            .unwrap_or_else(|| prepare_code_block(code, language));
        let line_count = prepared.lines.len();
        let lang_caption = prepared
            .language_label
            .as_deref()
            .unwrap_or("plain")
            .to_owned();
        let lang_color = if prepared.language_label.is_some() {
            palette.link_fg
        } else {
            palette.text_secondary
        };
        let lines_caption = if line_count == 1 {
            "1 line".to_owned()
        } else {
            format!("{line_count} lines")
        };

        let header = div()
            .w_full()
            .h(px(24.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .justify_between()
            .bg(palette.surface_island)
            .border_b_1()
            .border_color(palette.border_subtle)
            .child(
                div()
                    .text_size(px(11.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(lang_color)
                    .child(lang_caption),
            )
            .child(
                div()
                    .text_size(px(10.5))
                    .text_color(palette.text_muted)
                    .child(lines_caption),
            );

        let mut code_lines = div()
            .min_w_full()
            .py(px(8.0))
            .px(px(12.0))
            .font_family(CODE_FONT_FAMILY)
            .text_size(px(CODE_FONT_SIZE_PX))
            .line_height(px(CODE_LINE_HEIGHT_PX))
            .text_color(palette.fg)
            .flex()
            .flex_col();
        for line in prepared.lines.iter() {
            code_lines = code_lines.child(render_highlighted_code_line(line, theme));
        }

        let body = div()
            .id(SharedString::from(format!(
                "{}-{path_id}-code-scroll",
                self.element_id_prefix
            )))
            .w_full()
            .min_w(px(0.0))
            .overflow_x_scroll()
            .bg(palette.bg)
            .child(code_lines);

        div()
            .w_full()
            .min_w(px(0.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(palette.border_subtle)
            .overflow_hidden()
            .flex()
            .flex_col()
            .child(header)
            .child(body)
            .into_any_element()
    }

    fn render_table(
        &self,
        path_id: &str,
        table: &TableBlock,
        palette: &MarkdownPalette,
        cx: &Context<'_, Self>,
    ) -> AnyElement {
        let mut wrapper = div()
            .w_full()
            .min_w(px(0.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(palette.border_subtle)
            .overflow_hidden()
            .flex()
            .flex_col();

        let mut header_row = div()
            .w_full()
            .min_w(px(0.0))
            .bg(palette.surface_island)
            .border_b_1()
            .border_color(palette.border_subtle)
            .flex();

        for (col_idx, cell) in table.headers.iter().enumerate() {
            let align = table
                .alignments
                .get(col_idx)
                .copied()
                .unwrap_or(TableAlignment::Left);
            header_row = header_row.child(self.render_table_cell(
                &format!("{path_id}-th-{col_idx}"),
                col_idx,
                align,
                cell,
                true,
                palette,
                cx,
            ));
        }
        wrapper = wrapper.child(header_row);

        let mut alt_bg = palette.surface_island;
        alt_bg.a *= 0.45;

        for (row_idx, row) in table.rows.iter().enumerate() {
            let mut row_el = div().w_full().min_w(px(0.0)).flex();
            if row_idx % 2 == 1 {
                row_el = row_el.bg(alt_bg);
            }
            if row_idx + 1 < table.rows.len() {
                row_el = row_el.border_b_1().border_color(palette.border_subtle);
            }
            for (col_idx, cell) in row.iter().enumerate() {
                let align = table
                    .alignments
                    .get(col_idx)
                    .copied()
                    .unwrap_or(TableAlignment::Left);
                row_el = row_el.child(self.render_table_cell(
                    &format!("{path_id}-td-{row_idx}-{col_idx}"),
                    col_idx,
                    align,
                    cell,
                    false,
                    palette,
                    cx,
                ));
            }
            wrapper = wrapper.child(row_el);
        }

        wrapper.into_any_element()
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "table cell helper needs stable id, column index, alignment, spans, header flag, palette, and context"
    )]
    fn render_table_cell(
        &self,
        cell_id: &str,
        col_idx: usize,
        align: TableAlignment,
        spans: &[InlineSpan],
        is_header: bool,
        palette: &MarkdownPalette,
        cx: &Context<'_, Self>,
    ) -> Div {
        let mut cell = div()
            .flex_1()
            .min_w(px(0.0))
            .px(px(10.0))
            .py(px(6.0))
            .flex()
            .items_center()
            .text_size(px(12.5))
            .line_height(px(18.0))
            .text_color(palette.text_primary);
        if is_header {
            cell = cell.font_weight(FontWeight::SEMIBOLD);
        }
        if col_idx > 0 {
            cell = cell.border_l_1().border_color(palette.border_subtle);
        }
        cell = match align {
            TableAlignment::Left => cell.justify_start().text_left(),
            TableAlignment::Center => cell.justify_center().text_center(),
            TableAlignment::Right => cell.justify_end().text_right(),
        };
        cell.child(self.render_inline(cell_id, spans, palette, cx))
    }
}

fn render_highlighted_code_line(line: &HighlightedCodeLine, theme: &KovaTheme) -> Div {
    if line.text.is_empty() {
        return div().h(px(CODE_LINE_HEIGHT_PX)).w_full();
    }
    let highlights: Vec<(Range<usize>, HighlightStyle)> = line
        .spans
        .iter()
        .map(|(range, kind)| {
            let style = theme.tokens().syntax.style(*kind);
            let color = hsla_from_rgba(style.fg);
            (
                range.clone(),
                HighlightStyle {
                    color: Some(color),
                    font_weight: style.bold.then_some(FontWeight::BOLD),
                    font_style: style.italic.then_some(FontStyle::Italic),
                    underline: style.underline.then_some(UnderlineStyle {
                        thickness: px(1.0),
                        color: Some(color),
                        wavy: false,
                    }),
                    ..HighlightStyle::default()
                },
            )
        })
        .collect();

    div()
        .h(px(CODE_LINE_HEIGHT_PX))
        .whitespace_nowrap()
        .child(StyledText::new(line.text.clone()).with_highlights(highlights))
}

impl Render for MarkdownPreviewView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let _ = self.sync_from_buffer();
        let theme = cx.global::<KovaTheme>();
        let palette = MarkdownPalette::from_theme(theme);

        let mut scroll_body = div()
            .id(SharedString::from(format!(
                "{}-scroll",
                self.element_id_prefix
            )))
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .overflow_y_scroll()
            .track_scroll(&self.scroll_handle)
            .px(px(24.0))
            .py(px(20.0))
            .flex()
            .flex_col()
            .gap(px(12.0));

        if self.cache.document().blocks.is_empty() {
            scroll_body = scroll_body.child(
                div()
                    .text_size(px(13.0))
                    .text_color(palette.text_muted)
                    .child("Empty Markdown document — start typing to preview."),
            );
        } else {
            for (idx, block) in self.cache.document().blocks.iter().enumerate() {
                scroll_body = scroll_body.child(self.render_block(
                    &format!("b{idx}"),
                    block,
                    &palette,
                    theme,
                    cx,
                ));
            }
        }

        div()
            .id(SharedString::from(format!(
                "{}-root",
                self.element_id_prefix
            )))
            .track_focus(&self.focus_handle)
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .bg(palette.bg)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, _, cx| {
                    cx.emit(MarkdownPreviewEvent::Focused);
                }),
            )
            .child(scroll_body)
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "unit tests assert cache revision matching, inline span flattening, and theme palettes"
)]
mod tests {
    use std::sync::Arc;

    use kova_buffer::Buffer;
    use kova_core::markdown::MarkdownBlock as CoreMarkdownBlock;
    use kova_theme::{AetherTheme, BuiltinTheme};

    use super::*;

    #[test]
    fn cache_parses_once_per_buffer_version_and_reuses_unchanged_code_blocks() {
        let buffer = SharedBuffer::new(Buffer::from_text(
            "# Title\n\n```json\n{\"ok\": true}\n```\n\nInitial text.",
        ));
        let mut cache = MarkdownPreviewCache::new();
        assert_eq!(cache.cached_version(), None);

        assert!(cache.sync_from_buffer(&buffer));
        let first_ver = buffer.version().as_u64();
        assert_eq!(cache.cached_version(), Some(first_ver));
        assert_eq!(cache.document().blocks.len(), 3);
        let first_doc_ptr = Arc::as_ptr(cache.document());
        let first_code_lines_ptr = cache
            .prepared_code_block(Some("json"), "{\"ok\": true}")
            .unwrap()
            .lines
            .as_ptr();

        // Second sync without buffer mutation must be an O(1) no-op.
        assert!(!cache.sync_from_buffer(&buffer));
        assert_eq!(Arc::as_ptr(cache.document()), first_doc_ptr);

        // Edit paragraph only; code block remains identical and reuses its PreparedCodeBlock Arc.
        let end = buffer.snapshot().len_bytes();
        buffer.apply_edit(end..end, " Updated.").unwrap();
        assert!(cache.sync_from_buffer(&buffer));
        assert_eq!(cache.cached_version(), Some(buffer.version().as_u64()));
        assert_ne!(cache.cached_version(), Some(first_ver));
        assert_eq!(
            cache.document().blocks[2],
            CoreMarkdownBlock::Paragraph(vec![InlineSpan::Text(
                "Initial text. Updated.".to_owned()
            )])
        );
        let second_code_lines_ptr = cache
            .prepared_code_block(Some("json"), "{\"ok\": true}")
            .unwrap()
            .lines
            .as_ptr();
        assert_eq!(second_code_lines_ptr, first_code_lines_ptr);
    }

    #[test]
    fn flattens_inline_spans_with_utf8_byte_offsets_and_links() {
        let palette = MarkdownPalette::from_theme(&KovaTheme::islands_dark());
        let spans = vec![
            InlineSpan::Text("Zażółć ".to_owned()),
            InlineSpan::Bold(vec![InlineSpan::Text("gęślą".to_owned())]),
            InlineSpan::Text(" ".to_owned()),
            InlineSpan::Strikethrough(vec![InlineSpan::Text("stare".to_owned())]),
            InlineSpan::Text(" ".to_owned()),
            InlineSpan::Code("kod()".to_owned()),
            InlineSpan::Text(" ".to_owned()),
            InlineSpan::Link {
                label: vec![
                    InlineSpan::Bold(vec![InlineSpan::Text("link".to_owned())]),
                    InlineSpan::Text(" doc".to_owned()),
                ],
                url: "docs/SPEC.md#sec".to_owned(),
            },
        ];

        let flat = flatten_inline_spans(&spans, &palette);
        assert_eq!(flat.text, "Zażółć gęślą stare kod() link doc");
        assert_eq!(flat.links.len(), 1);
        assert_eq!(&flat.text[flat.links[0].range.clone()], "link doc");
        assert_eq!(flat.links[0].url, "docs/SPEC.md#sec");
        assert!(
            flat.highlights
                .iter()
                .all(|(r, _)| flat.text.is_char_boundary(r.start)
                    && flat.text.is_char_boundary(r.end))
        );
        assert_eq!(flat.highlights.len(), 5);
        assert_eq!(flat.highlights[0].1.font_weight, Some(FontWeight::BOLD));
        assert!(flat.highlights[1].1.strikethrough.is_some());
        assert_eq!(
            flat.highlights[2].1.background_color,
            Some(palette.inline_code_bg)
        );
        assert!(flat.highlights[3].1.underline.is_some());
        assert_eq!(flat.highlights[3].1.font_weight, Some(FontWeight::BOLD));
    }

    #[test]
    fn resolves_dark_and_light_theme_palettes_and_link_targets() {
        let dark = MarkdownPalette::from_theme(&KovaTheme::islands_dark());
        let light = MarkdownPalette::from_theme(&KovaTheme::from_aether(
            Arc::new(AetherTheme::builtin(BuiltinTheme::KovaLight)),
            1,
        ));
        assert_ne!(dark.bg, light.bg);
        assert_ne!(dark.text_primary, light.text_primary);

        assert!(is_external_url("https://kova.dev"));
        assert!(is_external_url("HTTP://example.com"));
        assert!(is_external_url("mailto:dev@kova.dev"));
        assert!(!is_external_url("docs/README.md"));

        assert_eq!(
            relative_link_target("docs/SPEC.md#ac-1"),
            Some(PathBuf::from("docs/SPEC.md"))
        );
        assert_eq!(relative_link_target("#only-anchor"), None);
        assert_eq!(relative_link_target("https://kova.dev"), None);
    }
}
