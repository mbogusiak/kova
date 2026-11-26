//! *Evaluate Expression* dialog (`Alt+F8`, `KOVA-041`).
//!
//! [`EvaluateModal`] is the headless dialog state: a single-line expression field with
//! history (`Up` / `Down`), identifier completion from the variables in scope (`Tab`) and the
//! result of the last evaluation. While open it owns every key ([`EvaluateModal::handle_key`]).
//! [`render`] draws it as a centered overlay in *Kova Islands Dark*.

use std::rc::Rc;

use gpui::{
    App, ClickEvent, Div, FontWeight, InteractiveElement as _, ParentElement,
    StatefulInteractiveElement as _, Styled, Window, div, px,
};

use crate::debugger::VariableItem;
use crate::debugger::debugger_panel::DebuggerColors;

/// Maximum number of completion candidates shown.
const MAX_SUGGESTIONS: usize = 8;
/// Maximum number of history entries kept.
const MAX_HISTORY: usize = 50;

/// Java keywords and common members offered next to the names in scope.
const JAVA_COMPLETIONS: &[&str] = &[
    "this",
    "null",
    "true",
    "false",
    "instanceof",
    "new",
    "length",
    "size()",
    "isEmpty()",
    "toString()",
    "hashCode()",
    "getClass()",
    "equals(",
];

/// Result area of the dialog.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum EvaluateState {
    /// Nothing evaluated yet.
    #[default]
    Empty,
    /// Waiting for the adapter.
    Pending,
    /// Evaluation result.
    Result(VariableItem),
    /// Compiler / runtime error.
    Error(String),
}

/// What a key did inside the dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvaluateKeyOutcome {
    /// The key edited the expression or moved the selection.
    Consumed,
    /// `Enter`: evaluate.
    Submit,
    /// `Escape`: close.
    Close,
}

/// Headless *Evaluate Expression* dialog.
#[derive(Debug, Clone, Default)]
pub struct EvaluateModal {
    open: bool,
    expression: String,
    history: Vec<String>,
    history_cursor: Option<usize>,
    names: Vec<String>,
    suggestions: Vec<String>,
    selected_suggestion: usize,
    state: EvaluateState,
}

impl EvaluateModal {
    /// Opens the dialog with `selection` (the editor selection) and the names in scope.
    pub fn open(&mut self, selection: Option<&str>, names: Vec<String>) {
        self.open = true;
        self.names = names;
        if let Some(selection) = selection.map(str::trim).filter(|s| !s.is_empty()) {
            selection.clone_into(&mut self.expression);
        }
        self.history_cursor = None;
        self.state = EvaluateState::Empty;
        self.refresh_suggestions();
    }

    /// Closes the dialog (the expression is kept for the next `Alt+F8`).
    pub fn close(&mut self) {
        self.open = false;
        self.suggestions.clear();
    }

    /// Whether the dialog is visible.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }

    /// Current expression.
    #[must_use]
    pub fn expression(&self) -> &str {
        &self.expression
    }

    /// Completion candidates for the identifier before the caret.
    #[must_use]
    pub fn suggestions(&self) -> &[String] {
        &self.suggestions
    }

    /// Highlighted candidate.
    #[must_use]
    pub const fn selected_suggestion(&self) -> usize {
        self.selected_suggestion
    }

    /// Result area.
    #[must_use]
    pub const fn state(&self) -> &EvaluateState {
        &self.state
    }

    /// Replaces the expression (paste, tests).
    pub fn set_expression(&mut self, expression: impl Into<String>) {
        self.expression = expression.into();
        self.history_cursor = None;
        self.refresh_suggestions();
    }

    /// Takes the expression for evaluation; records it in the history.
    pub fn submit(&mut self) -> Option<String> {
        let expression = self.expression.trim().to_owned();
        if expression.is_empty() {
            return None;
        }
        self.history.retain(|h| *h != expression);
        self.history.push(expression.clone());
        if self.history.len() > MAX_HISTORY {
            self.history.remove(0);
        }
        self.history_cursor = None;
        self.suggestions.clear();
        self.state = EvaluateState::Pending;
        Some(expression)
    }

    /// Shows an evaluation result.
    pub fn set_result(&mut self, result: Result<VariableItem, String>) {
        self.state = match result {
            Ok(item) => EvaluateState::Result(item),
            Err(message) => EvaluateState::Error(message),
        };
    }

    /// Handles one key (GPUI key name and typed text).
    pub fn handle_key(&mut self, key: &str, text: Option<&str>) -> EvaluateKeyOutcome {
        match key {
            "escape" => {
                if self.suggestions.is_empty() {
                    return EvaluateKeyOutcome::Close;
                }
                self.suggestions.clear();
            }
            "enter" => {
                if self.suggestions.is_empty() {
                    return EvaluateKeyOutcome::Submit;
                }
                self.accept_suggestion();
            }
            "tab" => self.accept_suggestion(),
            "backspace" => {
                self.expression.pop();
                self.history_cursor = None;
                self.refresh_suggestions();
            }
            "up" if !self.suggestions.is_empty() => {
                self.selected_suggestion = self.selected_suggestion.saturating_sub(1);
            }
            "down" if !self.suggestions.is_empty() => {
                self.selected_suggestion =
                    (self.selected_suggestion + 1).min(self.suggestions.len() - 1);
            }
            "up" => self.history_step(true),
            "down" => self.history_step(false),
            _ => {
                if let Some(text) = text.filter(|t| !t.chars().any(char::is_control)) {
                    self.expression.push_str(text);
                    self.history_cursor = None;
                    self.refresh_suggestions();
                }
            }
        }
        EvaluateKeyOutcome::Consumed
    }

    fn history_step(&mut self, back: bool) {
        if self.history.is_empty() {
            return;
        }
        let last = self.history.len() - 1;
        let next = match (self.history_cursor, back) {
            (None, true) => Some(last),
            (Some(i), true) => Some(i.saturating_sub(1)),
            (Some(i), false) if i < last => Some(i + 1),
            (_, false) => None,
        };
        self.history_cursor = next;
        self.expression = next.map(|i| self.history[i].clone()).unwrap_or_default();
    }

    /// Identifier being typed at the end of the expression (`order.ite` → `ite`).
    fn current_word(&self) -> &str {
        let start = self
            .expression
            .rfind(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
            .map_or(0, |i| i + 1);
        &self.expression[start..]
    }

    fn refresh_suggestions(&mut self) {
        self.selected_suggestion = 0;
        let word = self.current_word();
        if word.is_empty() {
            self.suggestions.clear();
            return;
        }
        let after_dot = self.expression[..self.expression.len() - word.len()].ends_with('.');
        let names = self.names.iter().map(String::as_str);
        let java = JAVA_COMPLETIONS.iter().copied();
        let mut suggestions: Vec<String> = if after_dot {
            java.filter(|c| c.ends_with(')') || c.ends_with('(') || *c == "length")
                .filter(|c| c.starts_with(word) && *c != word)
                .map(str::to_owned)
                .collect()
        } else {
            names
                .chain(java)
                .filter(|c| c.starts_with(word) && *c != word)
                .map(str::to_owned)
                .collect()
        };
        suggestions.dedup();
        suggestions.truncate(MAX_SUGGESTIONS);
        self.suggestions = suggestions;
    }

    fn accept_suggestion(&mut self) {
        let Some(choice) = self.suggestions.get(self.selected_suggestion).cloned() else {
            return;
        };
        let keep = self.expression.len() - self.current_word().len();
        self.expression.truncate(keep);
        self.expression.push_str(&choice);
        self.suggestions.clear();
    }
}

/// Button callback of the dialog.
pub type EvaluateButton = Rc<dyn Fn(&mut Window, &mut App)>;

/// Renders the dialog overlay. `on_evaluate` / `on_close` run the dialog's buttons.
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "declarative overlay layout; splitting it would only scatter the dialog structure"
)]
pub fn render(
    modal: &EvaluateModal,
    frame_label: Option<&str>,
    colors: DebuggerColors,
    on_evaluate: EvaluateButton,
    on_close: EvaluateButton,
) -> Div {
    let context = frame_label.map_or_else(
        || "No frame selected".to_owned(),
        |frame| format!("in frame {frame}"),
    );
    let mut suggestions = div().flex().flex_col();
    for (index, suggestion) in modal.suggestions().iter().enumerate() {
        let mut row = div()
            .px(px(8.0))
            .h(px(20.0))
            .flex()
            .items_center()
            .text_size(px(12.0))
            .text_color(colors.text_primary)
            .child(suggestion.clone());
        if index == modal.selected_suggestion() {
            row = row.bg(colors.active);
        }
        suggestions = suggestions.child(row);
    }
    let result = match modal.state() {
        EvaluateState::Empty => div()
            .text_color(colors.text_muted)
            .child("Enter evaluates, Tab completes, ↑/↓ browse history, Esc closes."),
        EvaluateState::Pending => div().text_color(colors.text_secondary).child("Evaluating…"),
        EvaluateState::Result(item) => {
            let type_name = item
                .type_name
                .as_deref()
                .map(|t| format!(" ({t})"))
                .unwrap_or_default();
            div()
                .flex()
                .gap(px(6.0))
                .child(div().text_color(colors.text_muted).child("result ="))
                .child(div().text_color(colors.value).child(item.value.clone()))
                .child(div().text_color(colors.text_muted).child(type_name))
        }
        EvaluateState::Error(message) => div().text_color(colors.error).child(message.clone()),
    };
    let button = |id: &'static str, label: &'static str, action: EvaluateButton| {
        div()
            .id(id)
            .h(px(24.0))
            .px(px(12.0))
            .flex()
            .items_center()
            .rounded(px(4.0))
            .text_size(px(12.0))
            .text_color(colors.text_primary)
            .bg(colors.inset)
            .hover(|s| s.bg(colors.hover))
            .on_click(move |_: &ClickEvent, window, cx| action(window, cx))
            .child(label)
    };
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .flex()
        .justify_center()
        .pt(px(96.0))
        .child(
            div()
                .w(px(560.0))
                .max_w_full()
                .flex()
                .flex_col()
                .gap(px(8.0))
                .p(px(12.0))
                .rounded(px(8.0))
                .border_1()
                .border_color(colors.border)
                .bg(colors.surface)
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .child(
                            div()
                                .text_size(px(13.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(colors.text_primary)
                                .child("Evaluate Expression"),
                        )
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(colors.text_muted)
                                .child(context),
                        ),
                )
                .child(
                    div()
                        .h(px(28.0))
                        .px(px(8.0))
                        .flex()
                        .items_center()
                        .rounded(px(4.0))
                        .border_1()
                        .border_color(colors.accent)
                        .bg(colors.inset)
                        .text_size(px(13.0))
                        .text_color(colors.text_primary)
                        .child(format!("{}▏", modal.expression())),
                )
                .child(suggestions)
                .child(div().min_h(px(24.0)).text_size(px(12.0)).child(result))
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap(px(8.0))
                        .child(button("evaluate-close", "Close", on_close))
                        .child(button("evaluate-run", "Evaluate", on_evaluate)),
                ),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_completes_identifiers_and_members() {
        let mut modal = EvaluateModal::default();
        modal.open(None, vec!["orders".to_owned(), "total".to_owned()]);
        for ch in ["o", "r"] {
            assert_eq!(modal.handle_key(ch, Some(ch)), EvaluateKeyOutcome::Consumed);
        }
        assert_eq!(modal.suggestions(), ["orders"]);
        modal.handle_key("tab", None);
        assert_eq!(modal.expression(), "orders");
        modal.handle_key(".", Some("."));
        modal.handle_key("s", Some("s"));
        assert_eq!(modal.suggestions(), ["size()"]);
        modal.handle_key("enter", None);
        assert_eq!(modal.expression(), "orders.size()");
        assert_eq!(modal.handle_key("enter", None), EvaluateKeyOutcome::Submit);
        assert_eq!(modal.submit().as_deref(), Some("orders.size()"));
        assert_eq!(modal.state(), &EvaluateState::Pending);
    }

    #[test]
    fn history_navigation_and_escape() {
        let mut modal = EvaluateModal::default();
        modal.open(Some("a + 1"), Vec::new());
        modal.submit();
        modal.set_expression("b");
        modal.submit();
        modal.set_expression(String::new());
        modal.handle_key("up", None);
        assert_eq!(modal.expression(), "b");
        modal.handle_key("up", None);
        assert_eq!(modal.expression(), "a + 1");
        modal.handle_key("down", None);
        assert_eq!(modal.expression(), "b");
        modal.handle_key("down", None);
        assert_eq!(modal.expression(), "");
        modal.handle_key("backspace", None);
        assert_eq!(modal.handle_key("escape", None), EvaluateKeyOutcome::Close);
        modal.close();
        assert!(!modal.is_open());
        modal.set_result(Err("boom".to_owned()));
        assert_eq!(modal.state(), &EvaluateState::Error("boom".to_owned()));
        assert_eq!(modal.submit(), None);
    }
}
