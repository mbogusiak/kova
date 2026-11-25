//! Focus context hierarchy, active focus stack, dynamic context state, and
//! predicate expression evaluator (`when` / `context` clauses).

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

/// Logical UI focus scope arranged in an inheritance hierarchy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum FocusContext {
    /// Root application scope active everywhere.
    Global,
    /// Open workspace / project window scope.
    Workspace,
    /// Active code editor container (tabs, gutter, search bar).
    Editor,
    /// Standard editable text surface inside an editor.
    EditorText,
    /// Vim modal Normal mode inside an editor text surface.
    EditorVimNormal,
    /// Vim modal Insert mode inside an editor text surface.
    EditorVimInsert,
    /// Vim modal character-wise Visual mode inside an editor text surface.
    EditorVimVisual,
    /// Vim modal line-wise Visual mode inside an editor text surface.
    EditorVimVisualLine,
    /// Project explorer tool window tree.
    ProjectTree,
    /// File structure outline tool window.
    StructureView,
    /// Integrated PTY terminal tool window.
    Terminal,
    /// Generic tool window scope.
    ToolWindow,
    /// Modal *Search Everywhere* popup barrier.
    SearchEverywhere,
    /// Modal *Settings* dialog barrier.
    SettingsModal,
}

impl FocusContext {
    /// All supported [`FocusContext`] variants in deterministic order.
    pub const ALL: [Self; 14] = [
        Self::Global,
        Self::Workspace,
        Self::Editor,
        Self::EditorText,
        Self::EditorVimNormal,
        Self::EditorVimInsert,
        Self::EditorVimVisual,
        Self::EditorVimVisualLine,
        Self::ProjectTree,
        Self::StructureView,
        Self::Terminal,
        Self::ToolWindow,
        Self::SearchEverywhere,
        Self::SettingsModal,
    ];

    /// Returns the immediate parent context in the focus inheritance tree.
    ///
    /// Modal overlays ([`Self::SearchEverywhere`], [`Self::SettingsModal`])
    /// inherit directly from [`Self::Global`] so editor/workspace bindings do
    /// not leak through modal barriers. Vim modes inherit from
    /// [`Self::EditorText`] so IDE editor bindings remain discoverable while
    /// Vim-specific bindings take precedence at greater depth.
    #[must_use]
    pub const fn parent(self) -> Option<Self> {
        match self {
            Self::Global => None,
            Self::Workspace | Self::SearchEverywhere | Self::SettingsModal => Some(Self::Global),
            Self::Editor
            | Self::ProjectTree
            | Self::StructureView
            | Self::Terminal
            | Self::ToolWindow => Some(Self::Workspace),
            Self::EditorText => Some(Self::Editor),
            Self::EditorVimNormal
            | Self::EditorVimInsert
            | Self::EditorVimVisual
            | Self::EditorVimVisualLine => Some(Self::EditorText),
        }
    }

    /// Returns the inheritance depth of this context (`Global == 0`).
    #[must_use]
    pub const fn depth(self) -> u8 {
        match self {
            Self::Global => 0,
            Self::Workspace | Self::SearchEverywhere | Self::SettingsModal => 1,
            Self::Editor
            | Self::ProjectTree
            | Self::StructureView
            | Self::Terminal
            | Self::ToolWindow => 2,
            Self::EditorText => 3,
            Self::EditorVimNormal
            | Self::EditorVimInsert
            | Self::EditorVimVisual
            | Self::EditorVimVisualLine => 4,
        }
    }

    /// Returns the full ancestor chain from [`Self::Global`] down to `self`.
    #[must_use]
    pub fn ancestors(self) -> Vec<Self> {
        let mut chain = Vec::with_capacity(usize::from(self.depth()) + 1);
        let mut current = Some(self);
        while let Some(ctx) = current {
            chain.push(ctx);
            current = ctx.parent();
        }
        chain.reverse();
        chain
    }

    /// Returns the canonical `PascalCase` name used in `keymap.toml`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Global => "Global",
            Self::Workspace => "Workspace",
            Self::Editor => "Editor",
            Self::EditorText => "EditorText",
            Self::EditorVimNormal => "EditorVimNormal",
            Self::EditorVimInsert => "EditorVimInsert",
            Self::EditorVimVisual => "EditorVimVisual",
            Self::EditorVimVisualLine => "EditorVimVisualLine",
            Self::ProjectTree => "ProjectTree",
            Self::StructureView => "StructureView",
            Self::Terminal => "Terminal",
            Self::ToolWindow => "ToolWindow",
            Self::SearchEverywhere => "SearchEverywhere",
            Self::SettingsModal => "SettingsModal",
        }
    }

    /// Parses a context name accepting canonical `PascalCase`, `snake_case`,
    /// or case-insensitive equivalents.
    #[must_use]
    pub fn parse_name(name: &str) -> Option<Self> {
        let trimmed = name.trim();
        match trimmed {
            "Global" | "global" => Some(Self::Global),
            "Workspace" | "workspace" => Some(Self::Workspace),
            "Editor" | "editor" => Some(Self::Editor),
            "EditorText" | "editor_text" => Some(Self::EditorText),
            "EditorVimNormal" | "editor_vim_normal" => Some(Self::EditorVimNormal),
            "EditorVimInsert" | "editor_vim_insert" => Some(Self::EditorVimInsert),
            "EditorVimVisual" | "editor_vim_visual" => Some(Self::EditorVimVisual),
            "EditorVimVisualLine" | "editor_vim_visual_line" => Some(Self::EditorVimVisualLine),
            "ProjectTree" | "project_tree" => Some(Self::ProjectTree),
            "StructureView" | "structure_view" => Some(Self::StructureView),
            "Terminal" | "terminal" => Some(Self::Terminal),
            "ToolWindow" | "tool_window" => Some(Self::ToolWindow),
            "SearchEverywhere" | "search_everywhere" => Some(Self::SearchEverywhere),
            "SettingsModal" | "settings_modal" => Some(Self::SettingsModal),
            _ => Self::ALL.iter().copied().find(|ctx| {
                ctx.as_str().eq_ignore_ascii_case(trimmed) || snake_case_eq(ctx.as_str(), trimmed)
            }),
        }
    }
}

fn snake_case_eq(pascal: &str, candidate: &str) -> bool {
    let mut normalized = String::with_capacity(candidate.len());
    for ch in candidate.chars() {
        if ch != '_' && ch != '-' {
            normalized.push(ch.to_ascii_lowercase());
        }
    }
    pascal.eq_ignore_ascii_case(&normalized)
}

impl fmt::Display for FocusContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for FocusContext {
    type Err = ContextExprError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse_name(s).ok_or_else(|| ContextExprError::UnknownIdentifier {
            ident: s.trim().to_owned(),
            pos: 0,
        })
    }
}

/// Active chain of focus scopes ordered from outermost root to active leaf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusStack {
    chain: SmallVec<[FocusContext; 6]>,
}

impl FocusStack {
    /// Expands the inheritance chain for `leaf` from [`FocusContext::Global`]
    /// down to `leaf`.
    #[must_use]
    pub fn from_leaf(leaf: FocusContext) -> Self {
        let mut chain = SmallVec::new();
        let mut current = Some(leaf);
        while let Some(ctx) = current {
            chain.push(ctx);
            current = ctx.parent();
        }
        chain.reverse();
        Self { chain }
    }

    /// Builds a focus stack from an explicit chain of nested contexts.
    ///
    /// If `chain` is empty, defaults to `[FocusContext::Global]`. Duplicate
    /// consecutive entries are omitted while preserving order.
    #[must_use]
    pub fn with_explicit_chain(chain: impl IntoIterator<Item = FocusContext>) -> Self {
        let mut collected: SmallVec<[FocusContext; 6]> = SmallVec::new();
        for ctx in chain {
            if !collected.contains(&ctx) {
                collected.push(ctx);
            }
        }
        if collected.is_empty() {
            collected.push(FocusContext::Global);
        }
        Self { chain: collected }
    }

    /// Returns `true` if `ctx` is active anywhere in this stack.
    #[must_use]
    pub fn contains(&self, ctx: FocusContext) -> bool {
        self.chain.contains(&ctx)
    }

    /// Returns the zero-based depth of `ctx` within this stack, or `None` if
    /// `ctx` is not present.
    #[must_use]
    pub fn depth_of(&self, ctx: FocusContext) -> Option<u8> {
        self.chain
            .iter()
            .rposition(|entry| *entry == ctx)
            .and_then(|idx| u8::try_from(idx).ok())
    }

    /// Returns the innermost (leaf) focus context.
    #[must_use]
    pub fn leaf(&self) -> FocusContext {
        self.chain.last().copied().unwrap_or(FocusContext::Global)
    }

    /// Returns the full active context slice from root to leaf.
    #[must_use]
    pub fn as_slice(&self) -> &[FocusContext] {
        &self.chain
    }
}

impl Default for FocusStack {
    fn default() -> Self {
        Self::from_leaf(FocusContext::Global)
    }
}

/// Dynamic boolean flags and string attributes evaluated by [`ContextExpr`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "fixed boolean context flags avoid hash lookups on the hot path"
)]
pub struct ContextState {
    /// Whether Vim modal editing is enabled.
    pub vim_enabled: bool,
    /// Whether Vim is currently in Normal mode.
    pub vim_mode_normal: bool,
    /// Whether Vim is currently in Insert mode.
    pub vim_mode_insert: bool,
    /// Whether Vim is currently in Visual or `VisualLine` mode.
    pub vim_mode_visual: bool,
    /// Whether the active editor has a non-empty selection.
    pub editor_has_selection: bool,
    /// Whether the active editor has more than one cursor.
    pub editor_has_multiple_cursors: bool,
    /// Whether the active editor buffer is read-only.
    pub editor_read_only: bool,
    /// Whether the completion popup is currently visible.
    pub in_autocomplete: bool,
    /// Whether the active editor or workspace has diagnostics.
    pub has_diagnostics: bool,
    /// Whether an inline search/replace bar is open.
    pub search_open: bool,
    /// Whether Omarchy distraction-free focus mode is active.
    pub focus_mode: bool,
    custom_flags: BTreeMap<String, bool>,
    attributes: BTreeMap<String, String>,
}

impl ContextState {
    /// Creates a [`ContextState`] with all flags `false` and no attributes.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets a boolean flag by name (known built-in field or custom flag).
    pub fn set_flag(&mut self, key: impl Into<String>, value: bool) {
        let key = key.into();
        if !self.set_builtin_flag(&key, value) {
            self.custom_flags.insert(key, value);
        }
    }

    /// Builder helper that sets a boolean flag and returns `self`.
    #[must_use]
    pub fn with_flag(mut self, key: impl Into<String>, value: bool) -> Self {
        self.set_flag(key, value);
        self
    }

    /// Returns the value of a boolean flag (`false` when unset).
    #[must_use]
    pub fn get_flag(&self, key: &str) -> bool {
        if let Some(builtin) = BuiltinFlag::from_name(key) {
            return self.read_builtin_flag(builtin);
        }
        self.custom_flags.get(key).copied().unwrap_or(false)
    }

    /// Sets a string attribute (for example `language` or `os`).
    pub fn set_attr(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.attributes.insert(key.into(), value.into());
    }

    /// Builder helper that sets a string attribute and returns `self`.
    #[must_use]
    pub fn with_attr(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.set_attr(key, value);
        self
    }

    /// Returns the string value of `key`, if present.
    #[must_use]
    pub fn get_attr(&self, key: &str) -> Option<&str> {
        self.attributes.get(key).map(String::as_str)
    }

    fn set_builtin_flag(&mut self, key: &str, value: bool) -> bool {
        let Some(flag) = BuiltinFlag::from_name(key) else {
            return false;
        };
        match flag {
            BuiltinFlag::VimEnabled => self.vim_enabled = value,
            BuiltinFlag::VimModeNormal => self.vim_mode_normal = value,
            BuiltinFlag::VimModeInsert => self.vim_mode_insert = value,
            BuiltinFlag::VimModeVisual => self.vim_mode_visual = value,
            BuiltinFlag::EditorHasSelection => self.editor_has_selection = value,
            BuiltinFlag::EditorHasMultipleCursors => self.editor_has_multiple_cursors = value,
            BuiltinFlag::EditorReadOnly => self.editor_read_only = value,
            BuiltinFlag::InAutocomplete => self.in_autocomplete = value,
            BuiltinFlag::HasDiagnostics => self.has_diagnostics = value,
            BuiltinFlag::SearchOpen => self.search_open = value,
            BuiltinFlag::FocusMode => self.focus_mode = value,
        }
        true
    }

    const fn read_builtin_flag(&self, flag: BuiltinFlag) -> bool {
        match flag {
            BuiltinFlag::VimEnabled => self.vim_enabled,
            BuiltinFlag::VimModeNormal => self.vim_mode_normal,
            BuiltinFlag::VimModeInsert => self.vim_mode_insert,
            BuiltinFlag::VimModeVisual => self.vim_mode_visual,
            BuiltinFlag::EditorHasSelection => self.editor_has_selection,
            BuiltinFlag::EditorHasMultipleCursors => self.editor_has_multiple_cursors,
            BuiltinFlag::EditorReadOnly => self.editor_read_only,
            BuiltinFlag::InAutocomplete => self.in_autocomplete,
            BuiltinFlag::HasDiagnostics => self.has_diagnostics,
            BuiltinFlag::SearchOpen => self.search_open,
            BuiltinFlag::FocusMode => self.focus_mode,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum BuiltinFlag {
    VimEnabled,
    VimModeNormal,
    VimModeInsert,
    VimModeVisual,
    EditorHasSelection,
    EditorHasMultipleCursors,
    EditorReadOnly,
    InAutocomplete,
    HasDiagnostics,
    SearchOpen,
    FocusMode,
}

impl BuiltinFlag {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "vim_enabled" => Some(Self::VimEnabled),
            "vim_mode_normal" => Some(Self::VimModeNormal),
            "vim_mode_insert" => Some(Self::VimModeInsert),
            "vim_mode_visual" => Some(Self::VimModeVisual),
            "editor_has_selection" => Some(Self::EditorHasSelection),
            "editor_has_multiple_cursors" => Some(Self::EditorHasMultipleCursors),
            "editor_read_only" => Some(Self::EditorReadOnly),
            "in_autocomplete" => Some(Self::InAutocomplete),
            "has_diagnostics" => Some(Self::HasDiagnostics),
            "search_open" => Some(Self::SearchOpen),
            "focus_mode" => Some(Self::FocusMode),
            _ => None,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::VimEnabled => "vim_enabled",
            Self::VimModeNormal => "vim_mode_normal",
            Self::VimModeInsert => "vim_mode_insert",
            Self::VimModeVisual => "vim_mode_visual",
            Self::EditorHasSelection => "editor_has_selection",
            Self::EditorHasMultipleCursors => "editor_has_multiple_cursors",
            Self::EditorReadOnly => "editor_read_only",
            Self::InAutocomplete => "in_autocomplete",
            Self::HasDiagnostics => "has_diagnostics",
            Self::SearchOpen => "search_open",
            Self::FocusMode => "focus_mode",
        }
    }
}

/// Errors produced when parsing a [`ContextExpr`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ContextExprError {
    /// Input expression string was empty or whitespace-only.
    #[error("context expression cannot be empty")]
    Empty,
    /// Unexpected token encountered at byte offset `pos`.
    #[error("unexpected token `{found}` at byte {pos}")]
    UnexpectedToken {
        /// Token text encountered.
        found: String,
        /// Byte offset in the source expression.
        pos: usize,
    },
    /// Input ended while expecting another token.
    #[error("unexpected end of context expression: expected {expected}")]
    UnexpectedEof {
        /// Description of the expected grammar element.
        expected: &'static str,
    },
    /// Opening `(` without a matching `)`.
    #[error("unclosed parenthesis starting at byte {pos}")]
    UnclosedParen {
        /// Byte offset of the unclosed `(`.
        pos: usize,
    },
    /// String literal missing a closing quote.
    #[error("unterminated string literal starting at byte {pos}")]
    UnterminatedString {
        /// Byte offset of the opening quote.
        pos: usize,
    },
    /// Unknown or malformed operator starting at `pos`.
    #[error("invalid operator `{found}` at byte {pos}")]
    InvalidOperator {
        /// Offending character or operator text.
        found: String,
        /// Byte offset in the source expression.
        pos: usize,
    },
    /// Unknown identifier rejected in strict parsing mode.
    #[error("unknown context or flag identifier `{ident}` at byte {pos}")]
    UnknownIdentifier {
        /// Identifier text.
        ident: String,
        /// Byte offset in the source expression.
        pos: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum FlagKey {
    Builtin(BuiltinFlag),
    Custom(String),
}

impl FlagKey {
    fn from_ident(ident: &str) -> Self {
        match BuiltinFlag::from_name(ident) {
            Some(flag) => Self::Builtin(flag),
            None => Self::Custom(ident.to_owned()),
        }
    }

    fn as_str(&self) -> &str {
        match self {
            Self::Builtin(flag) => flag.as_str(),
            Self::Custom(name) => name.as_str(),
        }
    }
}

/// Compiled boolean predicate expression evaluated against [`FocusStack`] and
/// [`ContextState`] without heap allocations on the hot path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContextExpr(ExprNode);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ExprNode {
    Focus(FocusContext),
    Flag(FlagKey),
    AttrEq { key: String, value: String },
    AttrNe { key: String, value: String },
    Not(Box<Self>),
    And(Box<Self>, Box<Self>),
    Or(Box<Self>, Box<Self>),
}

impl ContextExpr {
    /// Constructs a predicate checking whether `ctx` is in the active [`FocusStack`].
    #[must_use]
    pub const fn focus(ctx: FocusContext) -> Self {
        Self(ExprNode::Focus(ctx))
    }

    /// Constructs a boolean flag predicate.
    #[must_use]
    pub fn flag(name: &str) -> Self {
        Self(ExprNode::Flag(FlagKey::from_ident(name)))
    }

    /// Constructs a negated predicate (`!expr`).
    #[must_use]
    pub fn negated(expr: Self) -> Self {
        Self(ExprNode::Not(Box::new(expr.0)))
    }

    /// Constructs a logical AND (`lhs && rhs`).
    #[must_use]
    pub fn and(lhs: Self, rhs: Self) -> Self {
        Self(ExprNode::And(Box::new(lhs.0), Box::new(rhs.0)))
    }

    /// Constructs a logical OR (`lhs || rhs`).
    #[must_use]
    pub fn or(lhs: Self, rhs: Self) -> Self {
        Self(ExprNode::Or(Box::new(lhs.0), Box::new(rhs.0)))
    }

    /// Parses a context predicate expression, treating unrecognized flag names
    /// as custom boolean flags that default to `false`.
    ///
    /// # Errors
    ///
    /// Returns [`ContextExprError`] if the input is empty or syntactically invalid.
    pub fn parse(input: &str) -> Result<Self, ContextExprError> {
        parse_with_mode(input, false)
    }

    /// Parses a context predicate expression in strict mode, rejecting unknown
    /// focus/flag/attribute identifiers.
    ///
    /// # Errors
    ///
    /// Returns [`ContextExprError`] on syntax errors or unknown identifiers.
    pub fn parse_strict(input: &str) -> Result<Self, ContextExprError> {
        parse_with_mode(input, true)
    }

    /// Evaluates this predicate against `stack` and `state`.
    #[must_use]
    pub fn matches(&self, stack: &FocusStack, state: &ContextState) -> bool {
        self.0.matches(stack, state)
    }

    /// Returns static specificity (`atomic_conditions + max_focus_depth`) used
    /// to break ties deterministically in `KeymapResolver`.
    #[must_use]
    pub fn specificity(&self) -> u16 {
        let (atoms, max_depth) = self.0.atom_count_and_max_depth();
        atoms.saturating_add(u16::from(max_depth))
    }

    /// Returns the greatest depth among [`FocusContext`] nodes in this
    /// expression that are present in `stack`.
    #[must_use]
    pub fn max_matched_context_depth(&self, stack: &FocusStack) -> Option<u8> {
        self.0.max_matched_context_depth(stack)
    }
}

impl ExprNode {
    fn matches(&self, stack: &FocusStack, state: &ContextState) -> bool {
        match self {
            Self::Focus(ctx) => stack.contains(*ctx),
            Self::Flag(FlagKey::Builtin(flag)) => state.read_builtin_flag(*flag),
            Self::Flag(FlagKey::Custom(name)) => state.get_flag(name),
            Self::AttrEq { key, value } => state.get_attr(key) == Some(value.as_str()),
            Self::AttrNe { key, value } => state.get_attr(key) != Some(value.as_str()),
            Self::Not(inner) => !inner.matches(stack, state),
            Self::And(lhs, rhs) => lhs.matches(stack, state) && rhs.matches(stack, state),
            Self::Or(lhs, rhs) => lhs.matches(stack, state) || rhs.matches(stack, state),
        }
    }

    fn max_matched_context_depth(&self, stack: &FocusStack) -> Option<u8> {
        match self {
            Self::Focus(ctx) => stack.depth_of(*ctx),
            Self::Flag(_) | Self::AttrEq { .. } | Self::AttrNe { .. } | Self::Not(_) => None,
            Self::And(lhs, rhs) | Self::Or(lhs, rhs) => {
                match (
                    lhs.max_matched_context_depth(stack),
                    rhs.max_matched_context_depth(stack),
                ) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (Some(a), None) => Some(a),
                    (None, Some(b)) => Some(b),
                    (None, None) => None,
                }
            }
        }
    }

    fn atom_count_and_max_depth(&self) -> (u16, u8) {
        match self {
            Self::Focus(ctx) => (1, ctx.depth()),
            Self::Flag(_) | Self::AttrEq { .. } | Self::AttrNe { .. } => (1, 0),
            Self::Not(inner) => inner.atom_count_and_max_depth(),
            Self::And(lhs, rhs) | Self::Or(lhs, rhs) => {
                let (la, ld) = lhs.atom_count_and_max_depth();
                let (ra, rd) = rhs.atom_count_and_max_depth();
                (la.saturating_add(ra), ld.max(rd))
            }
        }
    }
}

impl fmt::Display for ContextExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt_with_prec(f, 0)
    }
}

impl ExprNode {
    fn fmt_with_prec(&self, f: &mut fmt::Formatter<'_>, parent_prec: u8) -> fmt::Result {
        match self {
            Self::Focus(ctx) => f.write_str(ctx.as_str()),
            Self::Flag(flag) => f.write_str(flag.as_str()),
            Self::AttrEq { key, value } => write!(f, "{key} == \"{value}\""),
            Self::AttrNe { key, value } => write!(f, "{key} != \"{value}\""),
            Self::Not(inner) => {
                f.write_str("!")?;
                if matches!(inner.as_ref(), Self::And(_, _) | Self::Or(_, _)) {
                    f.write_str("(")?;
                    inner.fmt_with_prec(f, 0)?;
                    f.write_str(")")
                } else {
                    inner.fmt_with_prec(f, 3)
                }
            }
            Self::And(lhs, rhs) => {
                let wrap = parent_prec > 2;
                if wrap {
                    f.write_str("(")?;
                }
                lhs.fmt_with_prec(f, 2)?;
                f.write_str(" && ")?;
                rhs.fmt_with_prec(f, 2)?;
                if wrap {
                    f.write_str(")")?;
                }
                Ok(())
            }
            Self::Or(lhs, rhs) => {
                let wrap = parent_prec > 1;
                if wrap {
                    f.write_str("(")?;
                }
                lhs.fmt_with_prec(f, 1)?;
                f.write_str(" || ")?;
                rhs.fmt_with_prec(f, 1)?;
                if wrap {
                    f.write_str(")")?;
                }
                Ok(())
            }
        }
    }
}

impl FromStr for ContextExpr {
    type Err = ContextExprError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for ContextExpr {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ContextExpr {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TokenKind {
    Ident(String),
    StringLit(String),
    And,
    Or,
    Bang,
    Eq,
    Ne,
    LParen,
    RParen,
}

#[derive(Debug, Clone)]
struct Token {
    kind: TokenKind,
    pos: usize,
}

fn tokenize(input: &str) -> Result<Vec<Token>, ContextExprError> {
    let mut tokens = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let (tok, next_i) = scan_next_token(input, i)?;
        tokens.push(tok);
        i = next_i;
    }

    Ok(tokens)
}

fn scan_next_token(input: &str, start: usize) -> Result<(Token, usize), ContextExprError> {
    let bytes = input.as_bytes();
    let b = bytes[start];
    match b {
        b'(' => Ok((
            Token {
                kind: TokenKind::LParen,
                pos: start,
            },
            start + 1,
        )),
        b')' => Ok((
            Token {
                kind: TokenKind::RParen,
                pos: start,
            },
            start + 1,
        )),
        b'&' if bytes.get(start + 1) == Some(&b'&') => Ok((
            Token {
                kind: TokenKind::And,
                pos: start,
            },
            start + 2,
        )),
        b'&' => Err(ContextExprError::InvalidOperator {
            found: "&".to_owned(),
            pos: start,
        }),
        b'|' if bytes.get(start + 1) == Some(&b'|') => Ok((
            Token {
                kind: TokenKind::Or,
                pos: start,
            },
            start + 2,
        )),
        b'|' => Err(ContextExprError::InvalidOperator {
            found: "|".to_owned(),
            pos: start,
        }),
        b'=' if bytes.get(start + 1) == Some(&b'=') => Ok((
            Token {
                kind: TokenKind::Eq,
                pos: start,
            },
            start + 2,
        )),
        b'=' => Err(ContextExprError::InvalidOperator {
            found: "=".to_owned(),
            pos: start,
        }),
        b'!' if bytes.get(start + 1) == Some(&b'=') => Ok((
            Token {
                kind: TokenKind::Ne,
                pos: start,
            },
            start + 2,
        )),
        b'!' => Ok((
            Token {
                kind: TokenKind::Bang,
                pos: start,
            },
            start + 1,
        )),
        b'"' | b'\'' => {
            let (lit, next_i) = scan_string_literal(input, start)?;
            Ok((
                Token {
                    kind: TokenKind::StringLit(lit),
                    pos: start,
                },
                next_i,
            ))
        }
        _ if is_ident_start(b) => {
            let mut end = start + 1;
            while end < bytes.len() && is_ident_continue(bytes[end]) {
                end += 1;
            }
            Ok((
                Token {
                    kind: TokenKind::Ident(input[start..end].to_owned()),
                    pos: start,
                },
                end,
            ))
        }
        _ => {
            let ch = input[start..]
                .chars()
                .next()
                .map_or_else(|| "?".to_owned(), |c| c.to_string());
            Err(ContextExprError::UnexpectedToken {
                found: ch,
                pos: start,
            })
        }
    }
}

fn scan_string_literal(input: &str, start: usize) -> Result<(String, usize), ContextExprError> {
    let bytes = input.as_bytes();
    let quote = bytes[start];
    let mut i = start + 1;
    let mut lit = String::new();
    while i < bytes.len() {
        let cur = bytes[i];
        if cur == b'\\' && i + 1 < bytes.len() {
            let escaped = bytes[i + 1] as char;
            lit.push(escaped);
            i += 2;
            continue;
        }
        if cur == quote {
            return Ok((lit, i + 1));
        }
        let ch = input[i..]
            .chars()
            .next()
            .ok_or(ContextExprError::UnterminatedString { pos: start })?;
        lit.push(ch);
        i += ch.len_utf8();
    }
    Err(ContextExprError::UnterminatedString { pos: start })
}

const fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

const fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-'
}

fn parse_with_mode(input: &str, strict: bool) -> Result<ContextExpr, ContextExprError> {
    let tokens = tokenize(input)?;
    if tokens.is_empty() {
        return Err(ContextExprError::Empty);
    }
    let mut parser = Parser {
        tokens: &tokens,
        cursor: 0,
        strict,
    };
    let expr = parser.parse_or()?;
    if let Some(leftover) = parser.peek() {
        return Err(ContextExprError::UnexpectedToken {
            found: token_label(&leftover.kind),
            pos: leftover.pos,
        });
    }
    Ok(expr)
}

struct Parser<'a> {
    tokens: &'a [Token],
    cursor: usize,
    strict: bool,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&'a Token> {
        self.tokens.get(self.cursor)
    }

    fn advance(&mut self) -> Option<&'a Token> {
        let tok = self.tokens.get(self.cursor)?;
        self.cursor += 1;
        Some(tok)
    }

    fn parse_or(&mut self) -> Result<ContextExpr, ContextExprError> {
        let mut lhs = self.parse_and()?;
        while matches!(
            self.peek(),
            Some(Token {
                kind: TokenKind::Or,
                ..
            })
        ) {
            self.advance();
            let rhs = self.parse_and()?;
            lhs = ContextExpr::or(lhs, rhs);
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<ContextExpr, ContextExprError> {
        let mut lhs = self.parse_unary()?;
        while matches!(
            self.peek(),
            Some(Token {
                kind: TokenKind::And,
                ..
            })
        ) {
            self.advance();
            let rhs = self.parse_unary()?;
            lhs = ContextExpr::and(lhs, rhs);
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<ContextExpr, ContextExprError> {
        if matches!(
            self.peek(),
            Some(Token {
                kind: TokenKind::Bang,
                ..
            })
        ) {
            self.advance();
            let inner = self.parse_unary()?;
            return Ok(ContextExpr::negated(inner));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<ContextExpr, ContextExprError> {
        let Some(tok) = self.advance() else {
            return Err(ContextExprError::UnexpectedEof {
                expected: "identifier, '!', or '('",
            });
        };

        match &tok.kind {
            TokenKind::LParen => {
                let open_pos = tok.pos;
                let inner = self.parse_or()?;
                match self.advance() {
                    Some(Token {
                        kind: TokenKind::RParen,
                        ..
                    }) => Ok(inner),
                    Some(other) => Err(ContextExprError::UnexpectedToken {
                        found: token_label(&other.kind),
                        pos: other.pos,
                    }),
                    None => Err(ContextExprError::UnclosedParen { pos: open_pos }),
                }
            }
            TokenKind::Ident(ident) => {
                if matches!(
                    self.peek(),
                    Some(Token {
                        kind: TokenKind::Eq | TokenKind::Ne,
                        ..
                    })
                ) {
                    let op = self.advance();
                    let is_eq = matches!(
                        op,
                        Some(Token {
                            kind: TokenKind::Eq,
                            ..
                        })
                    );
                    let value = match self.advance() {
                        Some(Token {
                            kind: TokenKind::StringLit(s) | TokenKind::Ident(s),
                            ..
                        }) => s.clone(),
                        Some(other) => {
                            return Err(ContextExprError::UnexpectedToken {
                                found: token_label(&other.kind),
                                pos: other.pos,
                            });
                        }
                        None => {
                            return Err(ContextExprError::UnexpectedEof {
                                expected: "string literal",
                            });
                        }
                    };
                    if self.strict && !is_known_attribute(ident) {
                        return Err(ContextExprError::UnknownIdentifier {
                            ident: ident.clone(),
                            pos: tok.pos,
                        });
                    }
                    return Ok(ContextExpr(if is_eq {
                        ExprNode::AttrEq {
                            key: ident.clone(),
                            value,
                        }
                    } else {
                        ExprNode::AttrNe {
                            key: ident.clone(),
                            value,
                        }
                    }));
                }

                if let Some(ctx) = FocusContext::parse_name(ident) {
                    return Ok(ContextExpr::focus(ctx));
                }
                if self.strict && BuiltinFlag::from_name(ident).is_none() {
                    return Err(ContextExprError::UnknownIdentifier {
                        ident: ident.clone(),
                        pos: tok.pos,
                    });
                }
                Ok(ContextExpr::flag(ident))
            }
            other => Err(ContextExprError::UnexpectedToken {
                found: token_label(other),
                pos: tok.pos,
            }),
        }
    }
}

fn is_known_attribute(ident: &str) -> bool {
    matches!(ident, "language" | "os" | "vim_mode" | "file_ext")
}

fn token_label(kind: &TokenKind) -> String {
    match kind {
        TokenKind::Ident(s) => s.clone(),
        TokenKind::StringLit(s) => format!("\"{s}\""),
        TokenKind::And => "&&".to_owned(),
        TokenKind::Or => "||".to_owned(),
        TokenKind::Bang => "!".to_owned(),
        TokenKind::Eq => "==".to_owned(),
        TokenKind::Ne => "!=".to_owned(),
        TokenKind::LParen => "(".to_owned(),
        TokenKind::RParen => ")".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_context_hierarchy_and_depths() {
        assert_eq!(FocusContext::Global.parent(), None);
        assert_eq!(FocusContext::Global.depth(), 0);
        assert_eq!(FocusContext::Workspace.parent(), Some(FocusContext::Global));
        assert_eq!(FocusContext::Workspace.depth(), 1);
        assert_eq!(FocusContext::Editor.parent(), Some(FocusContext::Workspace));
        assert_eq!(FocusContext::Editor.depth(), 2);
        assert_eq!(
            FocusContext::EditorText.parent(),
            Some(FocusContext::Editor)
        );
        assert_eq!(FocusContext::EditorText.depth(), 3);
        assert_eq!(
            FocusContext::EditorVimNormal.parent(),
            Some(FocusContext::EditorText)
        );
        assert_eq!(FocusContext::EditorVimNormal.depth(), 4);

        assert_eq!(
            FocusContext::SearchEverywhere.parent(),
            Some(FocusContext::Global)
        );
        assert_eq!(
            FocusContext::SettingsModal.parent(),
            Some(FocusContext::Global)
        );

        assert_eq!(
            FocusContext::EditorVimNormal.ancestors(),
            vec![
                FocusContext::Global,
                FocusContext::Workspace,
                FocusContext::Editor,
                FocusContext::EditorText,
                FocusContext::EditorVimNormal,
            ]
        );
    }

    #[test]
    fn focus_context_parses_pascal_and_snake_case() {
        for ctx in FocusContext::ALL {
            assert_eq!(FocusContext::parse_name(ctx.as_str()), Some(ctx));
        }
        assert_eq!(
            FocusContext::parse_name("editor_text"),
            Some(FocusContext::EditorText)
        );
        assert_eq!(
            FocusContext::parse_name("editor_vim_visual_line"),
            Some(FocusContext::EditorVimVisualLine)
        );
        assert_eq!(FocusContext::parse_name("nonexistent_panel"), None);
    }

    #[test]
    fn focus_stack_enforces_modal_barrier() {
        let editor_stack = FocusStack::from_leaf(FocusContext::EditorText);
        assert!(editor_stack.contains(FocusContext::Global));
        assert!(editor_stack.contains(FocusContext::Workspace));
        assert!(editor_stack.contains(FocusContext::Editor));
        assert!(editor_stack.contains(FocusContext::EditorText));
        assert_eq!(editor_stack.depth_of(FocusContext::EditorText), Some(3));
        assert_eq!(editor_stack.leaf(), FocusContext::EditorText);

        let modal_stack = FocusStack::from_leaf(FocusContext::SearchEverywhere);
        assert!(modal_stack.contains(FocusContext::Global));
        assert!(modal_stack.contains(FocusContext::SearchEverywhere));
        assert!(!modal_stack.contains(FocusContext::Editor));
        assert!(!modal_stack.contains(FocusContext::EditorText));
        assert!(!modal_stack.contains(FocusContext::Workspace));
    }

    #[test]
    fn context_expr_parses_precedence_and_evaluates() {
        let expr = ContextExpr::parse(
            "EditorText && !vim_enabled && (language == \"java\" || language == \"kotlin\")",
        )
        .expect("valid expression");

        let stack = FocusStack::from_leaf(FocusContext::EditorText);
        let java_no_vim = ContextState::new().with_attr("language", "java");
        assert!(expr.matches(&stack, &java_no_vim));

        let java_with_vim = ContextState::new()
            .with_flag("vim_enabled", true)
            .with_attr("language", "java");
        assert!(!expr.matches(&stack, &java_with_vim));

        let rust_no_vim = ContextState::new().with_attr("language", "rust");
        assert!(!expr.matches(&stack, &rust_no_vim));

        let tree_stack = FocusStack::from_leaf(FocusContext::ProjectTree);
        assert!(!expr.matches(&tree_stack, &java_no_vim));
    }

    #[test]
    fn context_expr_precedence_binds_and_tighter_than_or() {
        // `a || b && c` == `a || (b && c)`
        let expr = ContextExpr::parse("vim_enabled || editor_has_selection && in_autocomplete")
            .expect("valid expression");
        let stack = FocusStack::default();
        let state = ContextState::new().with_flag("vim_enabled", true);
        assert!(expr.matches(&stack, &state));

        let state_sel_only = ContextState::new().with_flag("editor_has_selection", true);
        assert!(!expr.matches(&stack, &state_sel_only));
    }

    #[test]
    fn context_expr_specificity_ranks_more_specific_conditions_higher() {
        let simple = ContextExpr::parse("EditorText").expect("valid");
        let guarded = ContextExpr::parse("EditorText && !vim_enabled").expect("valid");
        let vim = ContextExpr::parse("EditorVimNormal").expect("valid");
        assert!(guarded.specificity() > simple.specificity());
        assert!(vim.specificity() > simple.specificity());
    }

    #[test]
    fn context_expr_rejects_malformed_inputs_and_strict_unknowns() {
        assert_eq!(ContextExpr::parse("   "), Err(ContextExprError::Empty));
        assert!(matches!(
            ContextExpr::parse("(EditorText && !vim_enabled"),
            Err(ContextExprError::UnclosedParen { .. })
        ));
        assert!(matches!(
            ContextExpr::parse("language == \"java"),
            Err(ContextExprError::UnterminatedString { .. })
        ));
        assert!(matches!(
            ContextExpr::parse("EditorText & vim_enabled"),
            Err(ContextExprError::InvalidOperator { .. })
        ));
        assert!(matches!(
            ContextExpr::parse("EditorText &&"),
            Err(ContextExprError::UnexpectedEof { .. })
        ));
        assert!(matches!(
            ContextExpr::parse("EditorText )"),
            Err(ContextExprError::UnexpectedToken { .. })
        ));
        assert!(matches!(
            ContextExpr::parse_strict("unknown_custom_flag"),
            Err(ContextExprError::UnknownIdentifier { .. })
        ));

        // Non-strict mode evaluates unknown custom flags as false unless set.
        let custom = ContextExpr::parse("my_plugin_flag").expect("custom flag allowed");
        assert!(!custom.matches(&FocusStack::default(), &ContextState::new()));
        assert!(custom.matches(
            &FocusStack::default(),
            &ContextState::new().with_flag("my_plugin_flag", true),
        ));
    }
}
