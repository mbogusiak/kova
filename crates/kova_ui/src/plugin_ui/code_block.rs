//! Native GPUI syntax-highlighted code block for plugin declarative UI (`KOVA-045`).
//!
//! Uses [`kova_syntax`] (`SyntaxEngine`, `LanguageRegistry`, `Highlighter`) for
//! syntax highlighting when the block's language is one of Kova's built-in
//! languages (`java`, `kotlin`, `http`, `toml`, `json`, `yaml`, `xml`, `groovy`), and
//! falls back to plain theme-styled monospace text otherwise.

use std::ops::Range;
use std::sync::{Arc, OnceLock};

use gpui::{
    Div, FontStyle, FontWeight, HighlightStyle, ParentElement as _, SharedString, Styled as _,
    StyledText, UnderlineStyle, div, px,
};
use kova_buffer::Buffer;
use kova_plugin_host::ui::{CodeBlockWidget, StyleVariant};
use kova_syntax::{Highlighter, LanguageRegistry, SyntaxEngine, SyntaxEngineConfig};
use kova_theme::{PluginVariant, SyntaxLanguage, SyntaxTokenKind};

use crate::controls::ControlPalette;
use crate::theme::{KovaTheme, hsla_from_rgba};

const CODE_FONT_FAMILY: &str = "JetBrains Mono";
const CODE_FONT_SIZE_PX: f32 = 12.0;
const CODE_LINE_HEIGHT_PX: f32 = 18.0;

/// Single line of a [`PreparedCodeBlock`] with line-local highlight spans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HighlightedCodeLine {
    /// Line text without trailing `\n` or `\r\n`.
    pub text: SharedString,
    /// Disjoint, sorted highlight spans within `text`.
    pub spans: Vec<(Range<usize>, SyntaxTokenKind)>,
}

/// Pre-parsed and highlighted code block ready for 120 FPS GPUI painting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedCodeBlock {
    /// Normalized display label for the block header (e.g. `"json"`, `"java"`, `"sql"`).
    pub language_label: Option<String>,
    /// Highlighted display lines.
    pub lines: Arc<[HighlightedCodeLine]>,
}

/// Maps a plugin-supplied language tag to a built-in [`SyntaxLanguage`], if supported.
#[must_use]
pub fn parse_code_language(language: Option<&str>) -> Option<SyntaxLanguage> {
    let raw = language?.trim();
    if raw.eq_ignore_ascii_case("java") {
        Some(SyntaxLanguage::Java)
    } else if raw.eq_ignore_ascii_case("kotlin")
        || raw.eq_ignore_ascii_case("kt")
        || raw.eq_ignore_ascii_case("kts")
    {
        Some(SyntaxLanguage::Kotlin)
    } else if raw.eq_ignore_ascii_case("http") || raw.eq_ignore_ascii_case("rest") {
        Some(SyntaxLanguage::Http)
    } else if raw.eq_ignore_ascii_case("toml") {
        Some(SyntaxLanguage::Toml)
    } else if raw.eq_ignore_ascii_case("json") || raw.eq_ignore_ascii_case("jsonc") {
        Some(SyntaxLanguage::Json)
    } else if raw.eq_ignore_ascii_case("yaml") || raw.eq_ignore_ascii_case("yml") {
        Some(SyntaxLanguage::Yaml)
    } else if raw.eq_ignore_ascii_case("xml")
        || raw.eq_ignore_ascii_case("pom")
        || raw.eq_ignore_ascii_case("xsd")
    {
        Some(SyntaxLanguage::Xml)
    } else if raw.eq_ignore_ascii_case("groovy") || raw.eq_ignore_ascii_case("gradle") {
        Some(SyntaxLanguage::Groovy)
    } else {
        None
    }
}

fn syntax_engine() -> Option<&'static SyntaxEngine> {
    static ENGINE: OnceLock<Option<SyntaxEngine>> = OnceLock::new();
    ENGINE
        .get_or_init(|| {
            let registry = LanguageRegistry::builtin().ok()?.clone();
            let config = SyntaxEngineConfig {
                worker_threads: 1,
                ..SyntaxEngineConfig::default()
            };
            SyntaxEngine::new(registry, config).ok()
        })
        .as_ref()
}

/// Tokenizes and highlights `code` using [`kova_syntax`] for supported languages.
///
/// Unsupported or omitted languages produce plain lines without spans.
#[must_use]
pub fn prepare_code_block(code: &str, language: Option<&str>) -> PreparedCodeBlock {
    let normalized_label = language
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let spans = parse_code_language(language)
        .and_then(|syntax_lang| {
            let engine = syntax_engine()?;
            let snapshot = Buffer::from_text(code).snapshot();
            let tree = engine.parse_sync(syntax_lang, &snapshot, None).ok()?;
            let highlighter = Highlighter::from_builtin().ok()?;
            highlighter
                .highlight_range(&tree, &snapshot, 0..snapshot.len_bytes())
                .ok()
        })
        .unwrap_or_default();

    let lines = split_highlighted_lines(code, spans.iter().map(|s| (s.range.clone(), s.kind)));
    PreparedCodeBlock {
        language_label: normalized_label,
        lines,
    }
}

/// Splits `text` on `\n` (trimming trailing `\r`) and clips absolute byte spans per line.
#[must_use]
pub fn split_highlighted_lines(
    text: &str,
    spans: impl IntoIterator<Item = (Range<usize>, SyntaxTokenKind)>,
) -> Arc<[HighlightedCodeLine]> {
    let mut spans = spans.into_iter().peekable();
    let mut out = Vec::new();
    let mut line_start = 0usize;

    for raw_line in text.split('\n') {
        let raw_end = line_start + raw_line.len();
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        let content_end = line_start + line.len();
        let mut line_spans = Vec::new();

        while let Some((range, kind)) = spans.peek().cloned() {
            if range.start >= raw_end {
                break;
            }
            let start = range.start.max(line_start);
            let end = range.end.min(content_end);
            if start < end {
                line_spans.push((start - line_start..end - line_start, kind));
            }
            if range.end <= raw_end {
                spans.next();
            } else {
                break;
            }
        }

        out.push(HighlightedCodeLine {
            text: SharedString::from(line.to_owned()),
            spans: line_spans,
        });
        line_start = raw_end + 1;
    }

    out.into()
}

/// Renders a [`CodeBlockWidget`] natively in GPUI using active [`KovaTheme`] tokens.
#[must_use]
pub fn render_code_block(
    widget: &CodeBlockWidget,
    prepared: &PreparedCodeBlock,
    theme: &KovaTheme,
) -> Div {
    let palette = ControlPalette::from_theme(theme);
    let variant: PluginVariant = widget.variant.into();
    let variant_colors = palette.variant(variant);
    let border_color = if widget.variant == StyleVariant::Muted {
        hsla_from_rgba(theme.borders().subtle)
    } else {
        variant_colors.border
    };
    let header_bg = hsla_from_rgba(theme.surfaces().island);
    let code_surface = hsla_from_rgba(theme.editor().background);
    let code_text = hsla_from_rgba(theme.editor().foreground);
    let line_number_fg = hsla_from_rgba(theme.gutter().line_number);
    let muted_fg = hsla_from_rgba(theme.text().muted);
    let subtle_border = hsla_from_rgba(theme.borders().subtle);

    let line_count = prepared.lines.len();
    let digits = line_count.max(1).ilog10() as usize + 1;

    let lang_caption = prepared
        .language_label
        .as_deref()
        .unwrap_or("plain")
        .to_owned();
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
        .bg(header_bg)
        .border_b_1()
        .border_color(subtle_border)
        .child(
            div()
                .text_size(px(11.0))
                .font_weight(FontWeight::MEDIUM)
                .text_color(if widget.variant == StyleVariant::Muted {
                    muted_fg
                } else {
                    variant_colors.subtle_fg
                })
                .child(lang_caption),
        )
        .child(
            div()
                .text_size(px(10.5))
                .text_color(muted_fg)
                .child(lines_caption),
        );

    let mut body = div()
        .w_full()
        .py(px(6.0))
        .bg(code_surface)
        .font_family(CODE_FONT_FAMILY)
        .text_size(px(CODE_FONT_SIZE_PX))
        .text_color(code_text)
        .flex()
        .flex_col();

    for (index, line) in prepared.lines.iter().enumerate() {
        body = body.child(render_code_line(
            index,
            line,
            digits,
            widget.show_line_numbers,
            line_number_fg,
            theme,
        ));
    }

    div()
        .w_full()
        .rounded(px(6.0))
        .border_1()
        .border_color(border_color)
        .overflow_hidden()
        .flex()
        .flex_col()
        .child(header)
        .child(body)
}

fn render_code_line(
    index: usize,
    line: &HighlightedCodeLine,
    digits: usize,
    show_line_numbers: bool,
    line_number_fg: gpui::Hsla,
    theme: &KovaTheme,
) -> Div {
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

    let text_content = if line.text.is_empty() {
        div().h(px(CODE_LINE_HEIGHT_PX)).flex_1()
    } else {
        div()
            .h(px(CODE_LINE_HEIGHT_PX))
            .flex_1()
            .min_w(px(0.0))
            .whitespace_nowrap()
            .overflow_hidden()
            .child(StyledText::new(line.text.clone()).with_highlights(highlights))
    };

    let mut row = div()
        .w_full()
        .h(px(CODE_LINE_HEIGHT_PX))
        .px(px(10.0))
        .flex()
        .items_center()
        .gap(px(10.0));

    if show_line_numbers {
        row = row.child(div().flex_none().text_color(line_number_fg).child(format!(
            "{:>width$}",
            index + 1,
            width = digits
        )));
    }

    row.child(text_content)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "unit tests assert kova_syntax language mapping and span highlighting"
)]
mod tests {
    use super::*;

    #[test]
    fn maps_supported_code_languages() {
        assert_eq!(
            parse_code_language(Some("java")),
            Some(SyntaxLanguage::Java)
        );
        assert_eq!(
            parse_code_language(Some("Kotlin")),
            Some(SyntaxLanguage::Kotlin)
        );
        assert_eq!(
            parse_code_language(Some("kt")),
            Some(SyntaxLanguage::Kotlin)
        );
        assert_eq!(
            parse_code_language(Some("http")),
            Some(SyntaxLanguage::Http)
        );
        assert_eq!(
            parse_code_language(Some("toml")),
            Some(SyntaxLanguage::Toml)
        );
        assert_eq!(
            parse_code_language(Some("JSON")),
            Some(SyntaxLanguage::Json)
        );
        assert_eq!(
            parse_code_language(Some("yaml")),
            Some(SyntaxLanguage::Yaml)
        );
        assert_eq!(parse_code_language(Some("yml")), Some(SyntaxLanguage::Yaml));
        assert_eq!(parse_code_language(Some("XML")), Some(SyntaxLanguage::Xml));
        assert_eq!(parse_code_language(Some("pom")), Some(SyntaxLanguage::Xml));
        assert_eq!(parse_code_language(Some("xsd")), Some(SyntaxLanguage::Xml));
        assert_eq!(
            parse_code_language(Some("groovy")),
            Some(SyntaxLanguage::Groovy)
        );
        assert_eq!(
            parse_code_language(Some("gradle")),
            Some(SyntaxLanguage::Groovy)
        );
        assert_eq!(parse_code_language(Some("sql")), None);
        assert_eq!(parse_code_language(None), None);
    }

    #[test]
    fn highlights_java_and_json_using_kova_syntax() {
        let java = prepare_code_block(
            "public class Demo {\n    String name = \"kova\";\n}",
            Some("java"),
        );
        assert_eq!(java.language_label.as_deref(), Some("java"));
        assert_eq!(java.lines.len(), 3);
        assert!(
            java.lines[0]
                .spans
                .iter()
                .any(|(_, kind)| *kind == SyntaxTokenKind::Keyword)
        );
        assert!(
            java.lines[1]
                .spans
                .iter()
                .any(|(_, kind)| *kind == SyntaxTokenKind::String)
        );

        let json = prepare_code_block("{\"ok\": true}", Some("json"));
        assert_eq!(json.lines.len(), 1);
        assert!(
            json.lines[0]
                .spans
                .iter()
                .any(|(_, kind)| *kind == SyntaxTokenKind::Boolean)
        );

        let plain = prepare_code_block("SELECT 1;\r\nSELECT 2;", Some("sql"));
        assert_eq!(plain.lines.len(), 2);
        assert_eq!(plain.lines[0].text.as_ref(), "SELECT 1;");
        assert_eq!(plain.lines[0].spans, Vec::new());

        let yaml = prepare_code_block("server:\n  port: 8080", Some("yaml"));
        assert_eq!(yaml.lines.len(), 2);
        assert!(
            yaml.lines[0]
                .spans
                .iter()
                .any(|(_, kind)| *kind == SyntaxTokenKind::Property)
        );
        assert!(
            yaml.lines[1]
                .spans
                .iter()
                .any(|(_, kind)| *kind == SyntaxTokenKind::Number)
        );

        let xml = prepare_code_block("<project id=\"kova\"/>", Some("xml"));
        assert_eq!(xml.lines.len(), 1);
        assert!(
            xml.lines[0]
                .spans
                .iter()
                .any(|(_, kind)| *kind == SyntaxTokenKind::Keyword)
        );
        assert!(
            xml.lines[0]
                .spans
                .iter()
                .any(|(_, kind)| *kind == SyntaxTokenKind::Property)
        );

        let groovy = prepare_code_block("plugins {\n    id 'java'\n}", Some("gradle"));
        assert_eq!(groovy.lines.len(), 3);
        assert!(
            groovy.lines[0]
                .spans
                .iter()
                .any(|(_, kind)| *kind == SyntaxTokenKind::Function)
        );
    }
}
