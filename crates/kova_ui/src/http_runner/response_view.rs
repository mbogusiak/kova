//! Response panel (`HttpResponseView`) shown in a right split next to an `.http` editor:
//! status badge, time and size summary, and **Body** / **Headers** / **Raw** tabs.
//!
//! The view never blocks the UI thread: the request runs on the `kova_tools` worker thread,
//! and formatting plus JSON highlighting run on GPUI's background executor. Line lists are
//! virtualized with `uniform_list`, so large responses keep scrolling smooth.

use std::ops::Range;
use std::sync::Arc;

use gpui::{
    ClickEvent, Context, FontWeight, HighlightStyle, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    StyledText, Task, Window, div, px, uniform_list,
};
use kova_buffer::Buffer;
use kova_syntax::Highlighter;
use kova_theme::{RgbaColor, SyntaxLanguage, SyntaxTokenKind};
use kova_tools::http_runner::{
    HttpBodyKind, HttpCancelToken, HttpRequestHandle, HttpResponseData, HttpRunnerError,
    HttpStatusClass, body_preview, format_duration_ms, format_headers, format_raw, format_size,
    format_status,
};

use crate::http_runner::highlight_engine;
use crate::theme::{KovaTheme, hsla_from_rgba};

const SUMMARY_HEIGHT_PX: f32 = 36.0;
const TAB_BAR_HEIGHT_PX: f32 = 30.0;
const LINE_HEIGHT_PX: f32 = 18.0;
const FONT_SIZE_PX: f32 = 12.0;
const MONO_FONT: &str = "JetBrains Mono";

/// Active tab of the response panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HttpResponseTab {
    /// Formatted (pretty-printed, highlighted) body.
    #[default]
    Body,
    /// Response headers.
    Headers,
    /// Status line, headers and unformatted body.
    Raw,
}

impl HttpResponseTab {
    /// All tabs in display order.
    pub const ALL: [Self; 3] = [Self::Body, Self::Headers, Self::Raw];

    /// Tab caption.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Body => "Body",
            Self::Headers => "Headers",
            Self::Raw => "Raw",
        }
    }
}

/// One display line with syntax spans (byte ranges relative to the line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyledLine {
    /// Line text without the trailing newline.
    pub text: SharedString,
    /// Disjoint, sorted highlight spans within `text`.
    pub spans: Vec<(Range<usize>, SyntaxTokenKind)>,
}

impl StyledLine {
    fn plain(text: &str) -> Self {
        Self {
            text: SharedString::from(text.to_owned()),
            spans: Vec::new(),
        }
    }
}

/// Response formatted for display (built off the UI thread).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedResponse {
    /// Request summary, e.g. `GET https://api/users`.
    pub title: String,
    /// Status label, e.g. `200 OK`.
    pub status_label: String,
    /// Status class driving the badge color.
    pub status_class: HttpStatusClass,
    /// Duration label, e.g. `87 ms`.
    pub duration_label: String,
    /// Size label, e.g. `1.5 KB`.
    pub size_label: String,
    /// Optional notice (preview cap, truncation, binary body).
    pub notice: Option<String>,
    /// Body tab lines.
    pub body: Arc<[StyledLine]>,
    /// Headers tab lines.
    pub headers: Arc<[StyledLine]>,
    /// Raw tab lines.
    pub raw: Arc<[StyledLine]>,
}

impl PreparedResponse {
    /// Formats `response` for display; JSON and XML bodies are highlighted through `kova_syntax`.
    #[must_use]
    pub fn new(title: String, response: &HttpResponseData) -> Self {
        let preview = body_preview(response);
        let body = if preview.kind == HttpBodyKind::Json && preview.pretty {
            highlight_body_lines(&preview.text, SyntaxLanguage::Json)
        } else if preview.kind == HttpBodyKind::Xml && !preview.text.is_empty() {
            highlight_body_lines(&preview.text, SyntaxLanguage::Xml)
        } else {
            plain_lines(&preview.text)
        };
        Self {
            title,
            status_label: format_status(response),
            status_class: HttpStatusClass::from_status(response.status),
            duration_label: format_duration_ms(response.duration_ms),
            size_label: format_size(response.size_bytes),
            notice: preview.notice,
            body,
            headers: plain_lines(format_headers(response).trim_end()),
            raw: plain_lines(&format_raw(response)),
        }
    }

    /// Returns the lines of `tab`.
    #[must_use]
    pub fn lines(&self, tab: HttpResponseTab) -> &Arc<[StyledLine]> {
        match tab {
            HttpResponseTab::Body => &self.body,
            HttpResponseTab::Headers => &self.headers,
            HttpResponseTab::Raw => &self.raw,
        }
    }
}

fn plain_lines(text: &str) -> Arc<[StyledLine]> {
    text.lines().map(StyledLine::plain).collect()
}

/// Splits `text` into lines and attaches `kova_syntax` highlight spans for `language` to each.
#[must_use]
pub fn highlight_body_lines(text: &str, language: SyntaxLanguage) -> Arc<[StyledLine]> {
    let spans = highlight_engine()
        .and_then(|engine| {
            let snapshot = Buffer::from_text(text).snapshot();
            let tree = engine.parse_sync(language, &snapshot, None).ok()?;
            let highlighter = Highlighter::from_builtin().ok()?;
            highlighter
                .highlight_range(&tree, &snapshot, 0..snapshot.len_bytes())
                .ok()
        })
        .unwrap_or_default();
    split_styled_lines(text, spans.iter().map(|s| (s.range.clone(), s.kind)))
}

/// Splits `text` into lines and attaches `kova_syntax` JSON highlight spans to each.
#[must_use]
pub fn highlight_json_lines(text: &str) -> Arc<[StyledLine]> {
    highlight_body_lines(text, SyntaxLanguage::Json)
}

/// Splits `text` on `\n` and distributes absolute `spans` (sorted, disjoint) onto lines.
#[must_use]
pub fn split_styled_lines(
    text: &str,
    spans: impl IntoIterator<Item = (Range<usize>, SyntaxTokenKind)>,
) -> Arc<[StyledLine]> {
    let mut spans = spans.into_iter().peekable();
    let mut out = Vec::new();
    let mut line_start = 0usize;
    for line in text.split('\n') {
        let line_end = line_start + line.len();
        let mut line_spans = Vec::new();
        while let Some((range, kind)) = spans.peek().cloned() {
            if range.start >= line_end {
                break;
            }
            let start = range.start.max(line_start);
            let end = range.end.min(line_end);
            if start < end {
                line_spans.push((start - line_start..end - line_start, kind));
            }
            if range.end <= line_end {
                spans.next();
            } else {
                break;
            }
        }
        out.push(StyledLine {
            text: SharedString::from(line.to_owned()),
            spans: line_spans,
        });
        line_start = line_end + 1;
    }
    out.into()
}

/// Lifecycle of the response panel.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum HttpRunState {
    /// Nothing sent yet.
    #[default]
    Idle,
    /// Request in flight.
    Running {
        /// Request summary.
        title: String,
    },
    /// Response received and formatted.
    Done(Box<PreparedResponse>),
    /// Request failed, was cancelled or could not be prepared.
    Failed {
        /// Request summary.
        title: String,
        /// Error message.
        message: String,
    },
}

/// Badge color for a status class: green `2xx`, orange `4xx`, red `5xx`.
#[must_use]
pub fn status_badge_color(class: HttpStatusClass, theme: &KovaTheme) -> RgbaColor {
    match class {
        HttpStatusClass::Success => theme.tokens().terminal.green,
        HttpStatusClass::Redirection | HttpStatusClass::Informational => {
            theme.editor().diagnostic_info
        }
        HttpStatusClass::ClientError => theme.editor().diagnostic_warning,
        HttpStatusClass::ServerError => theme.editor().diagnostic_error,
        HttpStatusClass::Unknown => theme.text().muted,
    }
}

/// GPUI response panel for `.http` requests.
#[derive(Debug)]
pub struct HttpResponseView {
    state: HttpRunState,
    tab: HttpResponseTab,
    cancel: Option<HttpCancelToken>,
    task: Option<Task<()>>,
    generation: u64,
}

impl HttpResponseView {
    /// Creates an idle panel.
    #[must_use]
    pub fn new(_cx: &mut Context<'_, Self>) -> Self {
        Self {
            state: HttpRunState::Idle,
            tab: HttpResponseTab::Body,
            cancel: None,
            task: None,
            generation: 0,
        }
    }

    /// Returns the current state.
    #[must_use]
    pub const fn state(&self) -> &HttpRunState {
        &self.state
    }

    /// Returns the active tab.
    #[must_use]
    pub const fn tab(&self) -> HttpResponseTab {
        self.tab
    }

    /// Switches the active tab.
    pub fn set_tab(&mut self, tab: HttpResponseTab, cx: &mut Context<'_, Self>) {
        if self.tab != tab {
            self.tab = tab;
            cx.notify();
        }
    }

    /// Shows `title` as running and awaits `handle` without blocking the UI thread.
    ///
    /// Any request still in flight is cancelled first.
    pub fn run(&mut self, title: String, handle: HttpRequestHandle, cx: &mut Context<'_, Self>) {
        self.cancel_in_flight();
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.cancel = Some(handle.cancel_token());
        self.state = HttpRunState::Running {
            title: title.clone(),
        };
        let background = cx.background_executor().clone();
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = handle.result().await;
            let next = background
                .spawn(async move {
                    match result {
                        Ok(response) => {
                            HttpRunState::Done(Box::new(PreparedResponse::new(title, &response)))
                        }
                        Err(err) => HttpRunState::Failed {
                            title,
                            message: err.to_string(),
                        },
                    }
                })
                .await;
            // The view may be gone (window closed); nothing to update then.
            let _ignored = this.update(cx, |view, cx| {
                if view.generation == generation {
                    view.state = next;
                    view.cancel = None;
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    /// Shows an error that happened before sending (e.g. unresolved `{{var}}`).
    pub fn show_error(
        &mut self,
        title: String,
        error: &HttpRunnerError,
        cx: &mut Context<'_, Self>,
    ) {
        self.cancel_in_flight();
        self.generation = self.generation.wrapping_add(1);
        self.state = HttpRunState::Failed {
            title,
            message: error.to_string(),
        };
        cx.notify();
    }

    /// Cancels the in-flight request, if any.
    pub fn cancel(&mut self, cx: &mut Context<'_, Self>) {
        if let HttpRunState::Running { title } = &self.state {
            let title = title.clone();
            self.show_error(title, &HttpRunnerError::Cancelled, cx);
        }
    }

    fn cancel_in_flight(&mut self) {
        if let Some(token) = self.cancel.take() {
            token.cancel();
        }
        self.task = None;
    }

    fn render_summary(&self, theme: &KovaTheme, cx: &mut Context<'_, Self>) -> gpui::Div {
        let text_primary = hsla_from_rgba(theme.text().primary);
        let text_muted = hsla_from_rgba(theme.text().muted);
        let inverse = hsla_from_rgba(theme.text().inverse);
        let subtle_border = hsla_from_rgba(theme.borders().subtle);

        let bar = div()
            .h(px(SUMMARY_HEIGHT_PX))
            .w_full()
            .flex()
            .items_center()
            .gap(px(10.0))
            .px(px(12.0))
            .border_b_1()
            .border_color(subtle_border)
            .text_size(px(12.0));

        let title = |t: &str| {
            div()
                .flex_1()
                .min_w(px(0.0))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_color(text_primary)
                .child(t.to_owned())
        };

        match &self.state {
            HttpRunState::Idle => bar.child(
                div()
                    .text_color(text_muted)
                    .child("Click ▶ next to a request to send it."),
            ),
            HttpRunState::Running { title: t } => bar
                .child(title(t))
                .child(div().text_color(text_muted).child("Sending…"))
                .child(
                    div()
                        .id("http-cancel")
                        .px(px(8.0))
                        .py(px(2.0))
                        .rounded(px(4.0))
                        .border_1()
                        .border_color(subtle_border)
                        .cursor_pointer()
                        .text_color(text_primary)
                        .child("Cancel")
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.cancel(cx))),
                ),
            HttpRunState::Done(resp) => {
                let badge = hsla_from_rgba(status_badge_color(resp.status_class, theme));
                bar.child(
                    div()
                        .px(px(8.0))
                        .py(px(2.0))
                        .rounded(px(4.0))
                        .bg(badge)
                        .text_color(inverse)
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(resp.status_label.clone()),
                )
                .child(title(&resp.title))
                .child(
                    div()
                        .text_color(text_muted)
                        .child(resp.duration_label.clone()),
                )
                .child(div().text_color(text_muted).child(resp.size_label.clone()))
            }
            HttpRunState::Failed { title: t, message } => {
                let error = hsla_from_rgba(theme.editor().diagnostic_error);
                bar.child(title(t))
                    .child(div().text_color(error).child(message.clone()))
            }
        }
    }

    fn render_tabs(&self, theme: &KovaTheme, cx: &mut Context<'_, Self>) -> gpui::Div {
        let text_primary = hsla_from_rgba(theme.text().primary);
        let text_muted = hsla_from_rgba(theme.text().muted);
        let indicator = hsla_from_rgba(theme.borders().tab_indicator);
        let subtle_border = hsla_from_rgba(theme.borders().subtle);

        let mut tabs = div()
            .h(px(TAB_BAR_HEIGHT_PX))
            .w_full()
            .flex()
            .items_center()
            .px(px(8.0))
            .gap(px(4.0))
            .border_b_1()
            .border_color(subtle_border)
            .text_size(px(12.0));
        for tab in HttpResponseTab::ALL {
            let active = tab == self.tab;
            tabs = tabs.child(
                div()
                    .id(tab.label())
                    .h_full()
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .when_active(active, indicator)
                    .text_color(if active { text_primary } else { text_muted })
                    .child(tab.label())
                    .on_click(
                        cx.listener(move |this, _: &ClickEvent, _, cx| this.set_tab(tab, cx)),
                    ),
            );
        }
        tabs
    }
}

trait ActiveTabExt {
    fn when_active(self, active: bool, indicator: gpui::Hsla) -> Self;
}

impl ActiveTabExt for gpui::Stateful<gpui::Div> {
    fn when_active(self, active: bool, indicator: gpui::Hsla) -> Self {
        if active {
            self.border_b_2().border_color(indicator)
        } else {
            self
        }
    }
}

fn render_line(line: &StyledLine, theme: &KovaTheme) -> gpui::Div {
    let highlights: Vec<(Range<usize>, HighlightStyle)> = line
        .spans
        .iter()
        .map(|(range, kind)| {
            let style = theme.tokens().syntax.style(*kind);
            (
                range.clone(),
                HighlightStyle {
                    color: Some(hsla_from_rgba(style.fg)),
                    font_weight: style.bold.then_some(FontWeight::BOLD),
                    ..HighlightStyle::default()
                },
            )
        })
        .collect();
    div()
        .h(px(LINE_HEIGHT_PX))
        .px(px(12.0))
        .whitespace_nowrap()
        .child(StyledText::new(line.text.clone()).with_highlights(highlights))
}

impl Render for HttpResponseView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.global::<KovaTheme>().clone();
        let panel_bg = hsla_from_rgba(theme.surfaces().editor);
        let text_fg = hsla_from_rgba(theme.editor().foreground);
        let text_muted = hsla_from_rgba(theme.text().muted);

        let mut root = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(panel_bg)
            .child(self.render_summary(&theme, cx));

        if let HttpRunState::Done(resp) = &self.state {
            root = root.child(self.render_tabs(&theme, cx));
            if let Some(notice) = &resp.notice
                && self.tab == HttpResponseTab::Body
            {
                root = root.child(
                    div()
                        .px(px(12.0))
                        .py(px(4.0))
                        .text_size(px(11.0))
                        .text_color(text_muted)
                        .child(notice.clone()),
                );
            }
            let lines = Arc::clone(resp.lines(self.tab));
            root = root.child(
                uniform_list(
                    "http-response-lines",
                    lines.len(),
                    move |range, _window, cx| {
                        let theme = cx.global::<KovaTheme>();
                        range
                            .filter_map(|ix| lines.get(ix))
                            .map(|line| render_line(line, theme))
                            .collect()
                    },
                )
                .flex_1()
                .w_full()
                .py(px(6.0))
                .font_family(MONO_FONT)
                .text_size(px(FONT_SIZE_PX))
                .text_color(text_fg),
            );
        }
        root
    }
}

#[cfg(test)]
mod tests {
    use kova_tools::http_runner::HttpHeader;

    use super::*;

    fn response(status: u16, content_type: &str, body: &str) -> HttpResponseData {
        HttpResponseData {
            status,
            reason: Some("OK".to_owned()),
            http_version: "HTTP/1.1".to_owned(),
            final_url: "http://x/".to_owned(),
            duration_ms: 42,
            size_bytes: body.len() as u64,
            headers: vec![HttpHeader::new("content-type", content_type)],
            body: body.as_bytes().to_vec(),
            truncated: false,
        }
    }

    #[test]
    fn split_styled_lines_distributes_spans_across_lines() {
        let text = "ab\n\"cd\nef\"\ngh";
        // Span 3..10 crosses lines 1 and 2.
        let lines = split_styled_lines(
            text,
            [
                (0..1, SyntaxTokenKind::Keyword),
                (3..10, SyntaxTokenKind::String),
            ],
        );
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0].spans, [(0..1, SyntaxTokenKind::Keyword)]);
        assert_eq!(lines[1].spans, [(0..3, SyntaxTokenKind::String)]);
        assert_eq!(lines[2].spans, [(0..3, SyntaxTokenKind::String)]);
        assert_eq!(lines[3].spans.len(), 0);
    }

    #[test]
    fn prepared_json_response_is_pretty_and_highlighted() {
        let prepared = PreparedResponse::new(
            "GET http://x/".to_owned(),
            &response(200, "application/json", r#"{"name":"kova","n":1}"#),
        );
        assert_eq!(prepared.status_label, "200 OK");
        assert_eq!(prepared.status_class, HttpStatusClass::Success);
        assert_eq!(prepared.duration_label, "42 ms");
        let texts: Vec<&str> = prepared.body.iter().map(|l| l.text.as_ref()).collect();
        assert_eq!(texts, ["{", "  \"name\": \"kova\",", "  \"n\": 1", "}"]);
        assert!(
            prepared.body.iter().any(|l| !l.spans.is_empty()),
            "JSON body must carry kova_syntax highlight spans"
        );
        assert_eq!(
            prepared.headers[0].text.as_ref(),
            "content-type: application/json"
        );
        assert_eq!(prepared.raw[0].text.as_ref(), "HTTP/1.1 200 OK");
    }

    #[test]
    fn prepared_xml_response_is_highlighted() {
        let prepared = PreparedResponse::new(
            "GET http://x/".to_owned(),
            &response(
                200,
                "application/xml",
                "<order id=\"42\">\n  <status>OK</status>\n</order>",
            ),
        );
        assert_eq!(prepared.body.len(), 3);
        assert!(
            prepared.body[0]
                .spans
                .iter()
                .any(|(_, kind)| *kind == SyntaxTokenKind::Keyword),
            "XML body must carry kova_syntax highlight spans"
        );
        assert!(
            prepared.body[0]
                .spans
                .iter()
                .any(|(_, kind)| *kind == SyntaxTokenKind::Property),
            "XML attribute must be highlighted"
        );
    }

    #[test]
    fn badge_colors_follow_status_class() {
        let theme = KovaTheme::islands_dark();
        assert_eq!(
            status_badge_color(HttpStatusClass::Success, &theme),
            theme.tokens().terminal.green
        );
        assert_eq!(
            status_badge_color(HttpStatusClass::ClientError, &theme),
            theme.editor().diagnostic_warning
        );
        assert_eq!(
            status_badge_color(HttpStatusClass::ServerError, &theme),
            theme.editor().diagnostic_error
        );
    }

    #[test]
    fn tabs_are_ordered() {
        let labels: Vec<&str> = HttpResponseTab::ALL.iter().map(|t| t.label()).collect();
        assert_eq!(labels, ["Body", "Headers", "Raw"]);
    }
}
