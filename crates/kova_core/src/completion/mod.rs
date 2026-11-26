//! Headless code-completion model, context extraction, ranking, and Level-1 resolver (`KOVA-052`).
//!
//! Operates purely on [`BufferSnapshot`] and plain UTF-8 source text with zero GPUI or `tokio`
//! dependencies.

pub mod jdk_fallback;
pub mod resolver;

use std::ops::Range;
use std::path::Path;

use kova_buffer::BufferSnapshot;

use crate::navigation::SourceLanguageKind;
use crate::navigation::resolver::{
    ImportEntry, is_ident_char, is_inside_comment_or_string, parse_package_and_imports,
};

pub use jdk_fallback::{
    JdkFallbackClass, JdkFallbackMember, lookup_jdk_fallback, resolve_builtin_jdk_fqcn,
};
pub use resolver::{
    LocalCompletionResult, ReceiverTarget, resolve_local_completion, resolve_receiver_target,
};

/// Visual category of a completion proposal (`M` / `F` / `C` / `K`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CompletionProposalKind {
    /// Instance or static method / function.
    Method,
    /// Constructor (`new Foo(...)` or `Type::new`).
    Constructor,
    /// Field, constant, enum constant, or property.
    Field,
    /// Local variable or function parameter.
    Variable,
    /// Class or record type.
    Class,
    /// Interface type.
    Interface,
    /// Enum type.
    Enum,
    /// Rust struct.
    Struct,
    /// Rust trait.
    Trait,
    /// Package or module.
    Module,
    /// Language keyword.
    Keyword,
}

impl CompletionProposalKind {
    /// Single-character badge displayed in the completion popup row (`M`, `F`, `C`, `K`).
    #[must_use]
    pub const fn badge(self) -> char {
        match self {
            Self::Method | Self::Constructor => 'M',
            Self::Field | Self::Variable => 'F',
            Self::Class
            | Self::Interface
            | Self::Enum
            | Self::Struct
            | Self::Trait
            | Self::Module => 'C',
            Self::Keyword => 'K',
        }
    }
}

/// Origin that produced a [`CompletionProposal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CompletionSource {
    /// Variable, parameter, or enclosing type member in the active file.
    LocalScope,
    /// Type or member parsed from open buffers or workspace files.
    Workspace,
    /// Real JDK / library bytecode parsed from `.jmod`, `.jar`, or `.class`.
    JdkBytecode,
    /// Built-in deterministic JDK fallback catalog.
    JdkBuiltin,
    /// Language keyword.
    Keyword,
    /// Language server (`jdt.ls` `textDocument/completion`).
    Lsp,
}

/// An additional UTF-16 text edit applied atomically on completion acceptance (e.g. auto-import).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CompletionTextEdit {
    /// 0-based start line.
    pub start_utf16_line: usize,
    /// 0-based UTF-16 start column.
    pub start_utf16_col: usize,
    /// 0-based end line.
    pub end_utf16_line: usize,
    /// 0-based UTF-16 end column.
    pub end_utf16_col: usize,
    /// Replacement text.
    pub new_text: String,
}

/// One candidate shown in the completion popup.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "completion items carry independent callable, parameter, method-ref, and deprecation flags"
)]
pub struct CompletionProposal {
    /// Primary display name (e.g. `requireNonNull`).
    pub label: String,
    /// Symbol kind (`Method`, `Field`, `Class`, `Keyword`, ...).
    pub kind: CompletionProposalKind,
    /// Parameter signature shown next to `label` (e.g. `"(T obj, String message)"`).
    pub params_signature: Option<String>,
    /// Return type or field type shown right-aligned (e.g. `"T"`, `"PrintStream"`).
    pub return_type: Option<String>,
    /// Secondary right-aligned label (e.g. `"java.util"` when `return_type` is `None`).
    pub detail_right: Option<String>,
    /// Base identifier inserted into the buffer before optional `()` suffixing.
    pub insert_text: String,
    /// Whether accepting this item should append `()` (unless `is_method_reference` is set).
    pub is_callable: bool,
    /// Whether the callable expects at least one parameter (controls cursor placement inside `(|)` vs `()|`).
    pub has_params: bool,
    /// Whether this item comes from a `::` method reference (`System.out::println`) and skips `()`.
    pub is_method_reference: bool,
    /// Extra edits applied in the same undo transaction (e.g. `import ...;`).
    pub additional_text_edits: Vec<CompletionTextEdit>,
    /// Provenance of the proposal.
    pub source: CompletionSource,
    /// Whether the symbol is marked `@Deprecated`.
    pub is_deprecated: bool,
    /// Base ordering tier (`0` = highest local/static priority, `3` = `Object`, `5` = keyword).
    pub sort_priority: u16,
}

impl CompletionProposal {
    /// Text displayed on the right edge of the completion row (`return_type` or `detail_right`).
    #[must_use]
    pub fn right_label(&self) -> Option<&str> {
        self.return_type
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| self.detail_right.as_deref().filter(|s| !s.is_empty()))
    }
}

/// How code completion was triggered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CompletionTriggerKind {
    /// Explicit `Ctrl+Space` invocation.
    Invoked,
    /// Auto-triggered after typing `.` or `:` (second colon of `::`).
    AutoChar(char),
}

/// Member-access separator immediately before the completion anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CompletionSeparator {
    /// `.` (Java / Kotlin / Rust field & method access).
    Dot,
    /// `::` (Rust path/associated access, or Java/Kotlin method reference).
    DoubleColon,
}

/// Classification of the receiver expression before `.` or `::`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReceiverAccessKind {
    /// Receiver denotes a class, interface, enum, struct, or module (static/associated access).
    StaticType,
    /// Receiver denotes a value/variable/expression instance.
    Instance,
}

/// Extracted context under the caret for Level-1 and Level-2 completion resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionContext {
    /// Source language detected from `source_path`.
    pub language: SourceLanguageKind,
    /// Whether triggered manually or by typing `.` / `:`.
    pub trigger_kind: CompletionTriggerKind,
    /// Raw receiver expression preceding `.` or `::` (e.g. `"Objects"`, `"System.out"`).
    pub receiver: Option<String>,
    /// Segments of `receiver` split on `.` / `::` (e.g. `["System", "out"]`).
    pub receiver_chain: Vec<String>,
    /// Separator before `prefix` (`Dot` or `DoubleColon`), if any.
    pub separator: Option<CompletionSeparator>,
    /// `true` when `separator == Some(DoubleColon)` in Java or Kotlin (`System.out::`).
    pub is_method_reference: bool,
    /// Identifier prefix already typed after the anchor (e.g. `"req"` in `Objects.req`).
    pub prefix: String,
    /// Byte offset where the replacement token starts (`cursor_byte - prefix.len()`).
    pub anchor_byte: usize,
    /// Document byte range replaced when accepting an item (`anchor_byte..cursor_byte`).
    pub replace_range: Range<usize>,
    /// Declared `package` in Java/Kotlin, if any.
    pub package_name: Option<String>,
    /// Parsed `import` / `use` entries in the active document.
    pub imports: Vec<ImportEntry>,
}

/// A proposal paired with its fuzzy/prefix match score and highlighted character indices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RankedProposal {
    /// Candidate proposal.
    pub proposal: CompletionProposal,
    /// Match score (higher is better; `0` when query is empty).
    pub score: u32,
    /// Sorted 0-based character indices in `proposal.label` matched by the query.
    pub matched_char_indices: Vec<u32>,
}

/// Returns `true` when typing `typed_char` (already inserted immediately before `cursor_byte`)
/// should automatically open the completion popup.
///
/// Rejects comments, string/char literals, numeric literals (`3.`), range operators (`..`, `...`),
/// single colons (`:`), and `SourceLanguageKind::Unknown`.
#[must_use]
pub fn should_auto_trigger_completion(
    snapshot: &BufferSnapshot,
    lang: SourceLanguageKind,
    cursor_byte: usize,
    typed_char: char,
) -> bool {
    if lang == SourceLanguageKind::Unknown {
        return false;
    }
    let len = snapshot.len_bytes();
    let cursor = snapshot.clamp_byte_to_char_boundary(cursor_byte.min(len));
    if cursor == 0 {
        return false;
    }

    let Ok(pt) = snapshot.byte_to_point(cursor) else {
        return false;
    };
    let line_start = snapshot.rope().line_to_byte(pt.row);
    let Ok(line_slice) = snapshot.line_slice(pt.row) else {
        return false;
    };
    let line: String = line_slice.chunks().collect();
    let rel = cursor.saturating_sub(line_start).min(line.len());

    match typed_char {
        '.' => {
            if rel == 0 || !line[..rel].ends_with('.') {
                return false;
            }
            let dot_rel = rel - 1;
            let dot_abs = line_start + dot_rel;
            if is_inside_comment_or_string_or_block(snapshot, &line, dot_rel, dot_abs, lang) {
                return false;
            }
            let before = &line[..dot_rel];
            if before.ends_with('.') {
                return false;
            }
            let Some(prev_ch) = before.chars().next_back() else {
                return false;
            };
            if prev_ch == ')' || prev_ch == ']' || prev_ch == '"' || prev_ch == '>' {
                return true;
            }
            if is_ident_char(prev_ch) {
                // Reject numeric literals such as `3.` or `0x1.` while keeping `item1.`.
                let token_start = before
                    .rfind(|c: char| !is_ident_char(c))
                    .map_or(0, |idx| idx + 1);
                let token = &before[token_start..];
                return token
                    .chars()
                    .next()
                    .is_some_and(|first| !first.is_ascii_digit());
            }
            false
        }
        ':' => {
            if rel < 2 || !line[..rel].ends_with("::") || line[..rel].ends_with(":::") {
                return false;
            }
            let colon_rel = rel - 2;
            let colon_abs = line_start + colon_rel;
            if is_inside_comment_or_string_or_block(snapshot, &line, colon_rel, colon_abs, lang) {
                return false;
            }
            let before = &line[..colon_rel];
            before
                .chars()
                .next_back()
                .is_some_and(|ch| is_ident_char(ch) || ch == ')' || ch == '>')
        }
        _ => false,
    }
}

/// Extracts the [`CompletionContext`] at `cursor_byte` for qualified (`.` / `::`) or unqualified
/// (`Ctrl+Space`) completion.
#[must_use]
pub fn extract_completion_context(
    snapshot: &BufferSnapshot,
    source_path: &Path,
    cursor_byte: usize,
    trigger_kind: CompletionTriggerKind,
) -> Option<CompletionContext> {
    let language = SourceLanguageKind::from_path(source_path);
    let len = snapshot.len_bytes();
    let cursor = snapshot.clamp_byte_to_char_boundary(cursor_byte.min(len));
    let pt = snapshot.byte_to_point(cursor).ok()?;
    let line_start = snapshot.rope().line_to_byte(pt.row);
    let line: String = snapshot.line_slice(pt.row).ok()?.chunks().collect();
    let rel = cursor.saturating_sub(line_start).min(line.len());

    if language != SourceLanguageKind::Unknown {
        let check_rel = rel.saturating_sub(1);
        let check_abs = line_start + check_rel;
        if rel > 0
            && is_inside_comment_or_string_or_block(snapshot, &line, check_rel, check_abs, language)
        {
            return None;
        }
    }

    let before_cursor = &line[..rel];
    let mut prefix_start_rel = rel;
    while prefix_start_rel > 0 {
        let ch = before_cursor[..prefix_start_rel].chars().next_back()?;
        if !is_ident_char(ch) {
            break;
        }
        prefix_start_rel -= ch.len_utf8();
    }

    let prefix = before_cursor[prefix_start_rel..].to_owned();
    let anchor_byte = line_start + prefix_start_rel;
    let before_anchor = &before_cursor[..prefix_start_rel];

    let (separator, before_sep) = if let Some(rest) = before_anchor.strip_suffix("::") {
        if rest.ends_with(':') {
            (None, before_anchor)
        } else {
            (Some(CompletionSeparator::DoubleColon), rest)
        }
    } else if let Some(rest) = before_anchor.strip_suffix('.') {
        if rest.ends_with('.') {
            (None, before_anchor)
        } else {
            (Some(CompletionSeparator::Dot), rest)
        }
    } else {
        (None, before_anchor)
    };

    let (receiver, receiver_chain) = if separator.is_some() {
        extract_receiver_expression(before_sep)
    } else {
        (None, Vec::new())
    };

    // Do not return a qualified completion on invalid receivers (e.g. `3.` or bare `.`).
    if separator.is_some() && receiver.is_none() {
        return None;
    }

    let is_method_reference = separator == Some(CompletionSeparator::DoubleColon)
        && matches!(
            language,
            SourceLanguageKind::Java | SourceLanguageKind::Kotlin
        );

    let full_text = snapshot.text();
    let (package_name, imports) = parse_package_and_imports(&full_text, language);

    Some(CompletionContext {
        language,
        trigger_kind,
        receiver,
        receiver_chain,
        separator,
        is_method_reference,
        prefix,
        anchor_byte,
        replace_range: anchor_byte..cursor,
        package_name,
        imports,
    })
}

fn is_inside_comment_or_string_or_block(
    snapshot: &BufferSnapshot,
    line: &str,
    rel: usize,
    abs_byte: usize,
    lang: SourceLanguageKind,
) -> bool {
    if is_inside_comment_or_string(line, rel, lang) {
        return true;
    }
    is_inside_multiline_block_comment(snapshot, abs_byte)
}

fn is_inside_multiline_block_comment(snapshot: &BufferSnapshot, abs_byte: usize) -> bool {
    let prefix = snapshot
        .slice_bytes(0..abs_byte.min(snapshot.len_bytes()))
        .map(|s| s.to_string())
        .unwrap_or_default();
    let bytes = prefix.as_bytes();
    let mut i = 0;
    let mut in_block = false;
    let mut in_double = false;
    let mut in_line_comment = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_line_comment {
            if b == b'\n' {
                in_line_comment = false;
            }
            i += 1;
            continue;
        }
        if in_block {
            if b == b'*' && bytes.get(i + 1) == Some(&b'/') {
                in_block = false;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if in_double {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        if b == b'/' && bytes.get(i + 1) == Some(&b'/') {
            in_line_comment = true;
            i += 2;
            continue;
        }
        if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            in_block = true;
            i += 2;
            continue;
        }
        if b == b'"' {
            in_double = true;
        }
        i += 1;
    }
    in_block
}

fn extract_receiver_expression(before_sep: &str) -> (Option<String>, Vec<String>) {
    let trimmed = before_sep.trim_end();
    if trimmed.is_empty() {
        return (None, Vec::new());
    }
    let bytes = trimmed.as_bytes();
    // Handle string literal receiver `"abc".`
    if bytes.last() == Some(&b'"') {
        let mut i = bytes.len() - 1;
        while i > 0 {
            i -= 1;
            if bytes[i] == b'"' && (i == 0 || bytes[i - 1] != b'\\') {
                let raw = trimmed[i..].to_owned();
                return (Some(raw.clone()), vec![raw]);
            }
        }
        return (None, Vec::new());
    }

    let mut idx = bytes.len();
    let mut segments_rev: Vec<String> = Vec::new();

    loop {
        while idx > 0 && bytes[idx - 1].is_ascii_whitespace() {
            idx -= 1;
        }
        // Skip balanced `(...)` (method call or constructor invocation like `new ArrayList<>()`)
        let had_parens = if idx > 0 && bytes[idx - 1] == b')' {
            let Some(open) = find_matching_open(bytes, idx - 1, b'(', b')') else {
                break;
            };
            idx = open;
            while idx > 0 && bytes[idx - 1].is_ascii_whitespace() {
                idx -= 1;
            }
            true
        } else {
            false
        };
        // Skip balanced `<...>` (generic type args like `List<String>` or `Vec::<i32>`)
        if idx > 0 && bytes[idx - 1] == b'>' {
            if let Some(open) = find_matching_open(bytes, idx - 1, b'<', b'>') {
                idx = open;
                while idx > 0 && bytes[idx - 1].is_ascii_whitespace() {
                    idx -= 1;
                }
                // Handle Rust turbofish `::<>`
                if idx >= 2 && &bytes[idx - 2..idx] == b"::" {
                    idx -= 2;
                }
            }
        }

        let seg_end = idx;
        while idx > 0 && is_ident_char(bytes[idx - 1] as char) {
            idx -= 1;
        }
        let seg = &trimmed[idx..seg_end];
        if seg.is_empty() {
            break;
        }
        // Reject numeric literal receiver like `3.` on the rightmost segment.
        if segments_rev.is_empty()
            && !had_parens
            && seg.chars().next().is_some_and(|c| c.is_ascii_digit())
        {
            return (None, Vec::new());
        }

        if had_parens {
            // Check for `new Type(...)`
            let prior_slice = trimmed[..idx].trim_end();
            if prior_slice.ends_with("new")
                && prior_slice.strip_suffix("new").is_some_and(|p| {
                    p.is_empty() || !is_ident_char(p.chars().next_back().unwrap_or(' '))
                })
            {
                segments_rev.push(format!("new {seg}()"));
                break;
            }
            segments_rev.push(format!("{seg}()"));
        } else {
            segments_rev.push(seg.to_owned());
        }

        // Continue walking backwards if chained by `.` or `::`.
        while idx > 0 && bytes[idx - 1].is_ascii_whitespace() {
            idx -= 1;
        }
        if idx >= 2 && &bytes[idx - 2..idx] == b"::" {
            idx -= 2;
        } else if idx >= 1 && bytes[idx - 1] == b'.' && !(idx >= 2 && bytes[idx - 2] == b'.') {
            idx -= 1;
        } else {
            break;
        }
    }

    if segments_rev.is_empty() {
        return (None, Vec::new());
    }
    segments_rev.reverse();
    let chain = segments_rev;
    let raw = chain.join(".");
    (Some(raw), chain)
}

fn find_matching_open(bytes: &[u8], close_idx: usize, open_ch: u8, close_ch: u8) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = close_idx + 1;
    while i > 0 {
        i -= 1;
        if bytes[i] == close_ch {
            depth += 1;
        } else if bytes[i] == open_ch {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

/// Scores how well `candidate` matches `query` (returns `None` if it does not match).
///
/// Ranking tiers (higher score is better):
/// - Exact match (case-sensitive): `500_000`
/// - Exact match (case-insensitive): `400_000`
/// - Case-sensitive prefix: `300_000+`
/// - Case-insensitive prefix: `200_000+`
/// - `CamelHumps` initials match (e.g. `rNN` → `requireNonNull`): `100_000+`
/// - Subsequence fuzzy match: `10_000+`
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    reason = "identifier lengths and char indices fit comfortably inside u32"
)]
pub fn score_completion_match(candidate: &str, query: &str) -> Option<(u32, Vec<u32>)> {
    if query.is_empty() {
        return Some((0, Vec::new()));
    }
    let cand_chars: Vec<char> = candidate.chars().collect();
    let query_chars: Vec<char> = query.chars().collect();
    if query_chars.len() > cand_chars.len() {
        return None;
    }

    let len_penalty = (cand_chars.len() - query_chars.len()).min(9_999) as u32;
    let all_indices = || (0..query_chars.len() as u32).collect::<Vec<u32>>();

    if candidate == query {
        return Some((500_000, all_indices()));
    }
    if candidate.eq_ignore_ascii_case(query) {
        return Some((400_000, all_indices()));
    }
    if candidate.starts_with(query) {
        return Some((350_000_u32.saturating_sub(len_penalty), all_indices()));
    }
    if cand_chars
        .iter()
        .zip(query_chars.iter())
        .all(|(a, b)| a.eq_ignore_ascii_case(b))
    {
        return Some((250_000_u32.saturating_sub(len_penalty), all_indices()));
    }

    if let Some(indices) = match_camel_humps(&cand_chars, &query_chars) {
        let spread = indices
            .last()
            .copied()
            .unwrap_or(0)
            .saturating_sub(indices.first().copied().unwrap_or(0));
        let score = 150_000_u32
            .saturating_sub(len_penalty)
            .saturating_sub(spread);
        return Some((score, indices));
    }

    match_fuzzy_subsequence(&cand_chars, &query_chars).map(|(bonus, indices)| {
        let score = 10_000_u32.saturating_add(bonus).saturating_sub(len_penalty);
        (score, indices)
    })
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "identifier char index fits in u32"
)]
fn match_camel_humps(cand: &[char], query: &[char]) -> Option<Vec<u32>> {
    let mut hump_positions = Vec::with_capacity(cand.len());
    for (idx, &ch) in cand.iter().enumerate() {
        let is_boundary = idx == 0
            || ch.is_ascii_uppercase()
            || cand
                .get(idx.wrapping_sub(1))
                .is_some_and(|prev| *prev == '_');
        if is_boundary {
            hump_positions.push(idx);
        }
    }

    let mut matched = Vec::with_capacity(query.len());
    let mut q_idx = 0usize;
    let mut pos = 0usize;

    while q_idx < query.len() && pos < cand.len() {
        let q = query[q_idx].to_ascii_lowercase();
        // Prefer matching the next CamelHump boundary first, or continuing consecutively from previous match.
        let prev_matched = matched.last().map(|&m: &u32| m as usize);
        if let Some(prev) = prev_matched
            && prev + 1 == pos
            && cand[pos].to_ascii_lowercase() == q
            && !hump_positions.contains(&pos)
        {
            // Look ahead: only take lowercase continuation if remaining query still fits remaining humps or chars.
            let next_hump = hump_positions
                .iter()
                .copied()
                .find(|&h| h >= pos && cand[h].to_ascii_lowercase() == q);
            if next_hump.is_none() || query[q_idx].is_ascii_lowercase() {
                matched.push(pos as u32);
                q_idx += 1;
                pos += 1;
                continue;
            }
        }

        if let Some(&hump) = hump_positions
            .iter()
            .find(|&&h| h >= pos && cand[h].to_ascii_lowercase() == q)
        {
            matched.push(hump as u32);
            q_idx += 1;
            pos = hump + 1;
        } else if let Some(prev) = prev_matched
            && pos == prev + 1
            && cand[pos].to_ascii_lowercase() == q
        {
            matched.push(pos as u32);
            q_idx += 1;
            pos += 1;
        } else {
            return None;
        }
    }

    (q_idx == query.len()).then_some(matched)
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "identifier char index fits in u32"
)]
fn match_fuzzy_subsequence(cand: &[char], query: &[char]) -> Option<(u32, Vec<u32>)> {
    let mut indices = Vec::with_capacity(query.len());
    let mut bonus = 0u32;
    let mut pos = 0usize;

    for &q in query {
        let ql = q.to_ascii_lowercase();
        let mut found = None;
        while pos < cand.len() {
            let c = cand[pos];
            if c.to_ascii_lowercase() == ql {
                if c == q {
                    bonus += 20;
                }
                if indices.last().is_some_and(|&prev| prev + 1 == pos as u32) {
                    bonus += 40;
                }
                if pos == 0 || cand[pos - 1] == '_' || c.is_ascii_uppercase() {
                    bonus += 30;
                }
                found = Some(pos as u32);
                pos += 1;
                break;
            }
            pos += 1;
        }
        indices.push(found?);
    }
    Some((bonus, indices))
}

/// Filters and ranks `proposals` against `query` according to KOVA-052 §FR-4.
///
/// - Empty `query`: ordered by `(is_deprecated, sort_priority, label, params_signature)`.
/// - Non-empty `query`: ordered by `(!is_deprecated, score desc, sort_priority, label.len(), label)`.
#[must_use]
pub fn filter_and_rank_proposals(
    proposals: &[CompletionProposal],
    query: &str,
) -> Vec<RankedProposal> {
    let mut ranked: Vec<RankedProposal> = proposals
        .iter()
        .filter_map(|proposal| {
            let (score, matched_char_indices) = score_completion_match(&proposal.label, query)?;
            Some(RankedProposal {
                proposal: proposal.clone(),
                score,
                matched_char_indices,
            })
        })
        .collect();

    if query.is_empty() {
        ranked.sort_by(|a, b| {
            a.proposal
                .is_deprecated
                .cmp(&b.proposal.is_deprecated)
                .then_with(|| a.proposal.sort_priority.cmp(&b.proposal.sort_priority))
                .then_with(|| {
                    a.proposal
                        .label
                        .to_ascii_lowercase()
                        .cmp(&b.proposal.label.to_ascii_lowercase())
                })
                .then_with(|| a.proposal.label.cmp(&b.proposal.label))
                .then_with(|| {
                    a.proposal
                        .params_signature
                        .cmp(&b.proposal.params_signature)
                })
        });
    } else {
        ranked.sort_by(|a, b| {
            a.proposal
                .is_deprecated
                .cmp(&b.proposal.is_deprecated)
                .then_with(|| b.score.cmp(&a.score))
                .then_with(|| a.proposal.sort_priority.cmp(&b.proposal.sort_priority))
                .then_with(|| a.proposal.label.len().cmp(&b.proposal.label.len()))
                .then_with(|| {
                    a.proposal
                        .label
                        .to_ascii_lowercase()
                        .cmp(&b.proposal.label.to_ascii_lowercase())
                })
                .then_with(|| a.proposal.label.cmp(&b.proposal.label))
                .then_with(|| {
                    a.proposal
                        .params_signature
                        .cmp(&b.proposal.params_signature)
                })
        });
    }

    ranked
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use kova_buffer::Buffer;

    use super::*;

    fn java_path() -> PathBuf {
        PathBuf::from("src/main/java/Demo.java")
    }

    fn rust_path() -> PathBuf {
        PathBuf::from("src/main.rs")
    }

    #[test]
    fn context_extractor_handles_dot_prefix_chains_method_ref_and_ctrl_space() {
        // 1. `Objects.`
        let s1 = "public class Demo { void f() { Objects. } }";
        let snap1 = Buffer::from_text(s1).snapshot();
        let pos1 = s1.find("Objects.").unwrap() + "Objects.".len();
        let ctx1 = extract_completion_context(
            &snap1,
            &java_path(),
            pos1,
            CompletionTriggerKind::AutoChar('.'),
        )
        .unwrap();
        assert_eq!(ctx1.receiver.as_deref(), Some("Objects"));
        assert_eq!(ctx1.receiver_chain, vec!["Objects"]);
        assert_eq!(ctx1.separator, Some(CompletionSeparator::Dot));
        assert_eq!(ctx1.prefix, "");
        assert_eq!(ctx1.anchor_byte, pos1);

        // 2. `Objects.req`
        let s2 = "public class Demo { void f() { Objects.req } }";
        let snap2 = Buffer::from_text(s2).snapshot();
        let pos2 = s2.find("Objects.req").unwrap() + "Objects.req".len();
        let ctx2 =
            extract_completion_context(&snap2, &java_path(), pos2, CompletionTriggerKind::Invoked)
                .unwrap();
        assert_eq!(ctx2.receiver.as_deref(), Some("Objects"));
        assert_eq!(ctx2.prefix, "req");
        assert_eq!(ctx2.anchor_byte, pos2 - 3);
        assert_eq!(ctx2.replace_range, (pos2 - 3)..pos2);

        // 3. `System.out.`
        let s3 = "public class Demo { void f() { System.out. } }";
        let snap3 = Buffer::from_text(s3).snapshot();
        let pos3 = s3.find("System.out.").unwrap() + "System.out.".len();
        let ctx3 = extract_completion_context(
            &snap3,
            &java_path(),
            pos3,
            CompletionTriggerKind::AutoChar('.'),
        )
        .unwrap();
        assert_eq!(ctx3.receiver_chain, vec!["System", "out"]);
        assert_eq!(ctx3.separator, Some(CompletionSeparator::Dot));

        // 4. `System.out::` Java method reference
        let s4 = "public class Demo { void f() { System.out:: } }";
        let snap4 = Buffer::from_text(s4).snapshot();
        let pos4 = s4.find("System.out::").unwrap() + "System.out::".len();
        let ctx4 = extract_completion_context(
            &snap4,
            &java_path(),
            pos4,
            CompletionTriggerKind::AutoChar(':'),
        )
        .unwrap();
        assert_eq!(ctx4.separator, Some(CompletionSeparator::DoubleColon));
        assert!(ctx4.is_method_reference);
        assert_eq!(ctx4.receiver_chain, vec!["System", "out"]);

        // 5. `OrderService::` in Rust
        let s5 = "fn main() { OrderService:: }";
        let snap5 = Buffer::from_text(s5).snapshot();
        let pos5 = s5.find("OrderService::").unwrap() + "OrderService::".len();
        let ctx5 = extract_completion_context(
            &snap5,
            &rust_path(),
            pos5,
            CompletionTriggerKind::AutoChar(':'),
        )
        .unwrap();
        assert_eq!(ctx5.separator, Some(CompletionSeparator::DoubleColon));
        assert!(!ctx5.is_method_reference);
        assert_eq!(ctx5.receiver_chain, vec!["OrderService"]);

        // 6. `Ctrl+Space` on identifier prefix and blank statement
        let s6 = "public class Demo { void f() { Str } }";
        let snap6 = Buffer::from_text(s6).snapshot();
        let pos6 = s6.find("Str").unwrap() + 3;
        let ctx6 =
            extract_completion_context(&snap6, &java_path(), pos6, CompletionTriggerKind::Invoked)
                .unwrap();
        assert_eq!(ctx6.receiver, None);
        assert_eq!(ctx6.separator, None);
        assert_eq!(ctx6.prefix, "Str");
        assert_eq!(ctx6.replace_range, (pos6 - 3)..pos6);

        let s7 = "public class Demo { void f() {  } }";
        let snap7 = Buffer::from_text(s7).snapshot();
        let pos7 = s7.find("{  }").unwrap() + 2;
        let ctx7 =
            extract_completion_context(&snap7, &java_path(), pos7, CompletionTriggerKind::Invoked)
                .unwrap();
        assert_eq!(ctx7.prefix, "");
        assert_eq!(ctx7.replace_range, pos7..pos7);
    }

    #[test]
    fn auto_trigger_blocks_comments_strings_numbers_ranges_and_single_colon() {
        let src = "\
// comment Objects.
/* multi
   Objects. */
class Demo {
    String s = \"Objects.\";
    double x = 3.;
    void f() {
        Objects.
        OrderService:
        OrderService::
    }
}
";
        let snap = Buffer::from_text(src).snapshot();
        let comment_dot = src.find("// comment Objects.").unwrap() + "// comment Objects.".len();
        assert!(!should_auto_trigger_completion(
            &snap,
            SourceLanguageKind::Java,
            comment_dot,
            '.'
        ));

        let block_dot = src.find("   Objects.").unwrap() + "   Objects.".len();
        assert!(!should_auto_trigger_completion(
            &snap,
            SourceLanguageKind::Java,
            block_dot,
            '.'
        ));

        let str_dot = src.find("\"Objects.\"").unwrap() + "\"Objects.".len();
        assert!(!should_auto_trigger_completion(
            &snap,
            SourceLanguageKind::Java,
            str_dot,
            '.'
        ));

        let num_dot = src.find("3.").unwrap() + 2;
        assert!(!should_auto_trigger_completion(
            &snap,
            SourceLanguageKind::Java,
            num_dot,
            '.'
        ));

        let valid_dot = src.rfind("Objects.").unwrap() + "Objects.".len();
        assert!(should_auto_trigger_completion(
            &snap,
            SourceLanguageKind::Java,
            valid_dot,
            '.'
        ));

        let single_colon = src.find("OrderService:\n").unwrap() + "OrderService:".len();
        assert!(!should_auto_trigger_completion(
            &snap,
            SourceLanguageKind::Rust,
            single_colon,
            ':'
        ));

        let double_colon = src.find("OrderService::").unwrap() + "OrderService::".len();
        assert!(should_auto_trigger_completion(
            &snap,
            SourceLanguageKind::Rust,
            double_colon,
            ':'
        ));

        let range_src = "fn f() { for i in 0.. {} }";
        let range_snap = Buffer::from_text(range_src).snapshot();
        let range_dot = range_src.find("0..").unwrap() + 3;
        assert!(!should_auto_trigger_completion(
            &range_snap,
            SourceLanguageKind::Rust,
            range_dot,
            '.'
        ));
    }

    #[test]
    fn scoring_ranks_exact_prefix_camel_humps_fuzzy_and_deprecated() {
        let (exact_s, _) = score_completion_match("requireNonNull", "requireNonNull").unwrap();
        let (prefix_exact_case, _) = score_completion_match("requireNonNull", "req").unwrap();
        let (prefix_folded_case, _) = score_completion_match("requireNonNull", "REQ").unwrap();
        let (camel_s, camel_idx) = score_completion_match("requireNonNull", "rNN").unwrap();
        let (fuzzy_s, _) = score_completion_match("requireNonNull", "rnul").unwrap();

        assert!(exact_s > prefix_exact_case);
        assert!(prefix_exact_case > prefix_folded_case);
        assert!(prefix_folded_case > camel_s);
        assert!(camel_s > fuzzy_s);
        assert_eq!(camel_idx, vec![0, 7, 10]);

        let make = |label: &str, sort_priority: u16, is_deprecated: bool| CompletionProposal {
            label: label.to_owned(),
            kind: CompletionProposalKind::Method,
            params_signature: Some("()".to_owned()),
            return_type: Some("void".to_owned()),
            detail_right: None,
            insert_text: label.to_owned(),
            is_callable: true,
            has_params: false,
            is_method_reference: false,
            additional_text_edits: Vec::new(),
            source: CompletionSource::JdkBuiltin,
            is_deprecated,
            sort_priority,
        };

        let items = vec![
            make("wait", 3, false),
            make("oldMethod", 0, true),
            make("run", 1, false),
            make("alpha", 0, false),
        ];
        let ranked = filter_and_rank_proposals(&items, "");
        let labels: Vec<&str> = ranked.iter().map(|r| r.proposal.label.as_str()).collect();
        assert_eq!(labels, vec!["alpha", "run", "wait", "oldMethod"]);
    }
}
