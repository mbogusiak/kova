//! Chat state and declarative Tool Window rendering for `kova-ai` (host-independent).

use std::fmt::Write as _;

use kova_plugin_rs::ui::{self, UiBuilder};
use kova_plugin_rs::{
    EditorSnapshot, SplitDirection, StyleVariant, UiEvent, UiEventKind, WidgetTree,
};

/// Tool Window id of the chat panel.
pub const CHAT_PANEL_ID: &str = "kova-ai.chat";
/// Widget id of the provider `Select`.
pub const PROVIDER_SELECT_ID: &str = "kova-ai.provider";
/// Widget id of the prompt `TextInput`.
pub const PROMPT_INPUT_ID: &str = "kova-ai.prompt";
/// Widget id of the send `Button`.
pub const SEND_BUTTON_ID: &str = "kova-ai.send";
/// Widget id of the prompt `Form`.
pub const PROMPT_FORM_ID: &str = "kova-ai.prompt-form";
/// Widget id of the transcript `Markdown`.
pub const TRANSCRIPT_ID: &str = "kova-ai.transcript";
/// Widget id of the context `CodeBlock`.
pub const CONTEXT_ID: &str = "kova-ai.context";
/// Widget id of the provider status `Badge`.
pub const STATUS_ID: &str = "kova-ai.status";

/// Maximum number of buffer lines shown and attached as context.
pub const CONTEXT_EXCERPT_LINES: usize = 40;

/// Model provider selectable in the chat form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// Local Ollama server.
    Ollama,
    /// `OpenAI` API with the user's own key.
    OpenAi,
    /// Anthropic API with the user's own key.
    Anthropic,
    /// External agent speaking the Agent Client Protocol.
    Acp,
}

impl Provider {
    /// All providers in `Select` order.
    pub const ALL: [Self; 4] = [Self::Ollama, Self::OpenAi, Self::Anthropic, Self::Acp];

    /// Label shown in the provider `Select`.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Ollama => "Ollama (local)",
            Self::OpenAi => "OpenAI (BYOK)",
            Self::Anthropic => "Anthropic (BYOK)",
            Self::Acp => "ACP agent",
        }
    }

    /// Network destination as declared in `plugin.toml` (`None` for ACP over stdio).
    #[must_use]
    pub const fn endpoint(self) -> Option<&'static str> {
        match self {
            Self::Ollama => Some("localhost:11434"),
            Self::OpenAi => Some("api.openai.com:443"),
            Self::Anthropic => Some("api.anthropic.com:443"),
            Self::Acp => None,
        }
    }

    fn from_index(index: u32) -> Option<Self> {
        usize::try_from(index)
            .ok()
            .and_then(|i| Self::ALL.get(i).copied())
    }
}

/// Editor context attached to prompts, built from the host snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EditorContext {
    /// Active file path relative to the workspace (if any).
    pub file: Option<String>,
    /// Language identifier of the active buffer.
    pub language: Option<String>,
    /// First [`CONTEXT_EXCERPT_LINES`] lines of the buffer.
    pub excerpt: String,
    /// Total line count of the buffer.
    pub line_count: u32,
    /// Symbol names reported by the host AST index.
    pub symbols: Vec<String>,
}

impl EditorContext {
    /// Converts a host snapshot into chat context.
    #[must_use]
    pub fn from_snapshot(snapshot: &EditorSnapshot) -> Self {
        let excerpt = snapshot
            .content
            .as_deref()
            .map(|content| {
                content
                    .lines()
                    .take(CONTEXT_EXCERPT_LINES)
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        Self {
            file: snapshot.active_file.clone(),
            language: snapshot.language.clone(),
            excerpt,
            line_count: snapshot.line_count,
            symbols: snapshot
                .symbols
                .iter()
                .map(|symbol| format!("{} {}", symbol.kind, symbol.name))
                .collect(),
        }
    }
}

/// Author of a chat message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The IDE user.
    User,
    /// The assistant (or the plugin on its behalf).
    Assistant,
}

/// One message in the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    /// Author.
    pub role: Role,
    /// Markdown text.
    pub text: String,
}

/// Whole chat Tool Window state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatState {
    provider: Provider,
    draft: String,
    messages: Vec<ChatMessage>,
    context: Option<EditorContext>,
    context_error: Option<String>,
}

impl Default for ChatState {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatState {
    /// Empty conversation using the local Ollama provider.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            provider: Provider::Ollama,
            draft: String::new(),
            messages: Vec::new(),
            context: None,
            context_error: None,
        }
    }

    /// Selected provider.
    #[must_use]
    pub const fn provider(&self) -> Provider {
        self.provider
    }

    /// Current prompt draft.
    #[must_use]
    pub fn draft(&self) -> &str {
        &self.draft
    }

    /// Transcript messages.
    #[must_use]
    pub fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    /// Replaces the editor context (clears any previous read error).
    pub fn set_context(&mut self, context: Option<EditorContext>) {
        self.context = context;
        self.context_error = None;
    }

    /// Records that the editor snapshot could not be read.
    pub fn set_context_error(&mut self, message: String) {
        self.context = None;
        self.context_error = Some(message);
    }

    /// Clears the conversation and the draft.
    pub fn clear(&mut self) {
        self.messages.clear();
        self.draft.clear();
    }

    /// Fills the draft with a question about the active file and submits it.
    pub fn ask_about_active_file(&mut self) {
        let file = self
            .context
            .as_ref()
            .and_then(|ctx| ctx.file.as_deref())
            .unwrap_or("the current buffer");
        self.draft = format!("Explain what `{file}` does.");
        self.submit();
    }

    /// Submits the draft; returns `false` if it was blank.
    pub fn submit(&mut self) -> bool {
        let prompt = self.draft.trim();
        if prompt.is_empty() {
            return false;
        }
        self.messages.push(ChatMessage {
            role: Role::User,
            text: prompt.to_owned(),
        });
        let reply = self.pending_reply();
        self.messages.push(ChatMessage {
            role: Role::Assistant,
            text: reply,
        });
        self.draft.clear();
        true
    }

    fn pending_reply(&self) -> String {
        let target = self.provider.endpoint().map_or_else(
            || "stdio agent".to_owned(),
            |endpoint| format!("`{endpoint}`"),
        );
        let context = match &self.context {
            Some(ctx) => {
                let symbols = if ctx.symbols.is_empty() {
                    "no AST symbols".to_owned()
                } else {
                    format!("AST symbols: {}", ctx.symbols.join(", "))
                };
                format!(
                    "{} lines of `{}` ({symbols})",
                    ctx.line_count,
                    ctx.file.as_deref().unwrap_or("untitled buffer")
                )
            }
            None => "no editor context".to_owned(),
        };
        format!(
            "Prompt prepared for **{}** ({target}) with {context}. \
             Sending requires the plugin network API planned after WIT 0.1.",
            self.provider.label()
        )
    }

    /// Applies a UI event; returns whether the panel must be re-rendered.
    pub fn handle_ui_event(&mut self, event: &UiEvent) -> bool {
        match (event.widget_id.as_str(), &event.kind) {
            (PROMPT_INPUT_ID, UiEventKind::TextChanged(text)) => {
                self.draft.clone_from(text);
                true
            }
            (PROVIDER_SELECT_ID, UiEventKind::SelectedIndex(index)) => {
                match Provider::from_index(*index) {
                    Some(provider) => {
                        self.provider = provider;
                        true
                    }
                    None => false,
                }
            }
            (SEND_BUTTON_ID, UiEventKind::Clicked)
            | (PROMPT_FORM_ID | PROMPT_INPUT_ID, UiEventKind::Submitted) => self.submit(),
            _ => false,
        }
    }

    fn transcript_markdown(&self) -> String {
        if self.messages.is_empty() {
            return "### AI Assistant\n\nAsk about the active file. Kova attaches the \
                    file excerpt and AST symbols as context."
                .to_owned();
        }
        let mut out = String::new();
        for message in &self.messages {
            let who = match message.role {
                Role::User => "You",
                Role::Assistant => "Assistant",
            };
            let _ = write!(out, "**{who}:** {}\n\n", message.text);
        }
        out.trim_end().to_owned()
    }

    fn context_view(&self) -> (String, String) {
        match (&self.context, &self.context_error) {
            (Some(ctx), _) => {
                let language = ctx.language.clone().unwrap_or_else(|| "text".to_owned());
                let code = if ctx.excerpt.is_empty() {
                    "// active buffer is empty".to_owned()
                } else {
                    ctx.excerpt.clone()
                };
                (language, code)
            }
            (None, Some(error)) => ("text".to_owned(), format!("context unavailable: {error}")),
            (None, None) => ("text".to_owned(), "no active editor".to_owned()),
        }
    }

    /// Renders the declarative chat Tool Window.
    #[must_use]
    pub fn render(&self) -> WidgetTree {
        let mut ui = UiBuilder::new();

        let transcript = ui.leaf(TRANSCRIPT_ID, ui::markdown(self.transcript_markdown()));
        let (language, code) = self.context_view();
        let context = ui.styled(
            CONTEXT_ID,
            ui::code_block(&language, &code),
            StyleVariant::Muted,
        );
        let conversation = ui.container(
            "kova-ai.conversation",
            ui::split(SplitDirection::Vertical, 65),
            &[transcript, context],
        );

        let labels = Provider::ALL.map(Provider::label);
        let selected = Provider::ALL
            .iter()
            .position(|p| *p == self.provider)
            .and_then(|i| u32::try_from(i).ok());
        let provider = ui.leaf(
            PROVIDER_SELECT_ID,
            ui::select("Provider", &labels, selected),
        );
        let (status_label, status_style) = match self.provider.endpoint() {
            Some(endpoint) => (format!("network: {endpoint}"), StyleVariant::Accent),
            None => ("stdio agent".to_owned(), StyleVariant::Muted),
        };
        let status = ui.styled(STATUS_ID, ui::badge(status_label), status_style);
        let prompt = ui.leaf(
            PROMPT_INPUT_ID,
            ui::text_input("Prompt", &self.draft, "Ask about the active file…"),
        );
        let send = ui.leaf(
            SEND_BUTTON_ID,
            ui::button("Send", self.draft.trim().is_empty()),
        );
        let form = ui.container(
            PROMPT_FORM_ID,
            ui::form(Some("Ask Kova AI")),
            &[provider, status, prompt, send],
        );

        let root = ui.container(
            "kova-ai.root",
            ui::split(SplitDirection::Vertical, 75),
            &[conversation, form],
        );
        ui.finish(root)
    }
}

#[cfg(test)]
mod tests {
    use kova_plugin_rs::ui::find_node;
    use kova_plugin_rs::{AstSymbol, EditorSnapshot, UiEvent, UiEventKind, WidgetKind};

    use super::{
        CONTEXT_ID, ChatState, EditorContext, PROMPT_INPUT_ID, PROVIDER_SELECT_ID, Provider, Role,
        SEND_BUTTON_ID, TRANSCRIPT_ID,
    };

    fn event(widget_id: &str, kind: UiEventKind) -> UiEvent {
        UiEvent {
            widget_id: widget_id.to_owned(),
            kind,
        }
    }

    fn snapshot() -> EditorSnapshot {
        EditorSnapshot {
            workspace_root: Some("/ws".to_owned()),
            active_file: Some("src/Main.java".to_owned()),
            language: Some("java".to_owned()),
            buffer_version: 3,
            content: Some("class Main {\n  void run() {}\n}".to_owned()),
            line_count: 3,
            cursor_line: 0,
            cursor_column: 0,
            symbols: vec![AstSymbol {
                name: "Main".to_owned(),
                kind: "class".to_owned(),
                line: 0,
                column: 0,
            }],
            jvm_classpath_count: 0,
        }
    }

    #[test]
    fn context_comes_from_editor_snapshot() {
        let ctx = EditorContext::from_snapshot(&snapshot());
        assert_eq!(ctx.file.as_deref(), Some("src/Main.java"));
        assert_eq!(ctx.symbols, vec!["class Main"]);
        assert_eq!(ctx.excerpt.lines().count(), 3);

        let mut state = ChatState::new();
        state.set_context(Some(ctx));
        let tree = state.render();
        assert!(matches!(
            find_node(&tree, CONTEXT_ID).map(|n| &n.kind),
            Some(WidgetKind::CodeBlock(cb)) if cb.language == "java" && cb.code.starts_with("class Main")
        ));
    }

    #[test]
    fn typing_selecting_and_sending_updates_transcript() {
        let mut state = ChatState::new();
        assert!(!state.handle_ui_event(&event(SEND_BUTTON_ID, UiEventKind::Clicked)));
        assert!(state.handle_ui_event(&event(PROVIDER_SELECT_ID, UiEventKind::SelectedIndex(2))));
        assert_eq!(state.provider(), Provider::Anthropic);
        assert!(!state.handle_ui_event(&event(PROVIDER_SELECT_ID, UiEventKind::SelectedIndex(9))));
        assert!(state.handle_ui_event(&event(
            PROMPT_INPUT_ID,
            UiEventKind::TextChanged("Why?".to_owned())
        )));
        assert!(state.handle_ui_event(&event(SEND_BUTTON_ID, UiEventKind::Clicked)));

        assert_eq!(state.draft(), "");
        assert_eq!(state.messages().len(), 2);
        assert_eq!(state.messages()[0].role, Role::User);
        assert!(state.messages()[1].text.contains("api.anthropic.com:443"));
        let tree = state.render();
        assert!(matches!(
            find_node(&tree, TRANSCRIPT_ID).map(|n| &n.kind),
            Some(WidgetKind::Markdown(md)) if md.content.contains("**You:** Why?")
        ));
    }

    #[test]
    fn ask_about_file_and_clear() {
        let mut state = ChatState::new();
        state.set_context(Some(EditorContext::from_snapshot(&snapshot())));
        state.ask_about_active_file();
        assert!(state.messages()[0].text.contains("src/Main.java"));
        assert!(state.messages()[1].text.contains("AST symbols: class Main"));
        state.clear();
        assert_eq!(state.messages(), []);
    }

    #[test]
    fn context_error_is_rendered() {
        let mut state = ChatState::new();
        state.set_context_error("permission denied: editor_read".to_owned());
        let tree = state.render();
        assert!(matches!(
            find_node(&tree, CONTEXT_ID).map(|n| &n.kind),
            Some(WidgetKind::CodeBlock(cb)) if cb.code.contains("editor_read")
        ));
    }
}
