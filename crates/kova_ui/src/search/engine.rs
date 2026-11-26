//! Headless fuzzy search engine for *Search Everywhere* backed by `nucleo-matcher`.

use std::cmp::Ordering;
use std::fmt;
use std::ops::Range;

use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use super::{SearchEntry, SearchEntryKind, SearchHit, SearchScope, SearchTarget};

/// Maximum number of ranked hits returned per query so rendering and scoring stay `< 5 ms`.
pub const MAX_SEARCH_HITS: usize = 200;

const EXACT_TITLE_BONUS: u32 = 500;
const PREFIX_TITLE_BONUS: u32 = 200;
const TITLE_MATCH_BONUS: u32 = 120;

/// Splits a raw query such as `"OrderApp:42"` into `("OrderApp", Some(42))`.
///
/// A `:0` or non-numeric suffix is kept as part of the pattern because editor lines are 1-based.
#[must_use]
pub fn parse_query_and_line(raw: &str) -> (&str, Option<usize>) {
    let trimmed = raw.trim();
    if let Some((prefix, line_str)) = trimmed.rsplit_once(':')
        && !prefix.is_empty()
        && let Ok(line) = line_str.parse::<usize>()
        && line >= 1
    {
        return (prefix, Some(line));
    }
    (trimmed, None)
}

/// Converts 0-based Unicode character (`char` / UTF-32) indices emitted by `nucleo-matcher` into
/// merged, sorted UTF-8 byte ranges safe for `gpui::StyledText::with_highlights`.
///
/// Out-of-bounds indices are ignored and multi-byte UTF-8 boundaries are always respected.
#[must_use]
pub fn char_indices_to_byte_ranges(text: &str, char_indices: &[u32]) -> Vec<Range<usize>> {
    if text.is_empty() || char_indices.is_empty() {
        return Vec::new();
    }
    let mut sorted = char_indices.to_vec();
    sorted.sort_unstable();
    sorted.dedup();

    let mut ranges: Vec<Range<usize>> = Vec::new();
    let mut cursor = 0usize;
    for (char_idx, (byte_start, ch)) in text.char_indices().enumerate() {
        while cursor < sorted.len() && (sorted[cursor] as usize) < char_idx {
            cursor += 1;
        }
        if cursor >= sorted.len() {
            break;
        }
        if sorted[cursor] as usize == char_idx {
            let byte_end = byte_start + ch.len_utf8();
            if let Some(last) = ranges.last_mut()
                && last.end == byte_start
            {
                last.end = byte_end;
            } else {
                ranges.push(byte_start..byte_end);
            }
            cursor += 1;
        }
    }
    ranges
}

/// Reusable fuzzy search engine (`nucleo-matcher`) with scratch buffers to avoid per-keystroke
/// allocations.
pub struct SearchEngine {
    matcher: Matcher,
    path_matcher: Matcher,
    utf32_buf: Vec<char>,
    indices_buf: Vec<u32>,
}

impl fmt::Debug for SearchEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SearchEngine")
            .field("utf32_capacity", &self.utf32_buf.capacity())
            .finish_non_exhaustive()
    }
}

impl Default for SearchEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchEngine {
    /// Creates a new [`SearchEngine`] configured for standard and path-aware fuzzy matching.
    #[must_use]
    pub fn new() -> Self {
        Self {
            matcher: Matcher::new(Config::DEFAULT),
            path_matcher: Matcher::new(Config::DEFAULT.match_paths()),
            utf32_buf: Vec::with_capacity(256),
            indices_buf: Vec::with_capacity(64),
        }
    }

    /// Filters and ranks `entries` for `scope` and `raw_query`.
    ///
    /// Empty queries return up to [`MAX_SEARCH_HITS`] starter items in index order; non-empty
    /// queries support `:line` suffixes and populate `matched_char_indices` on `entry.title`.
    pub fn search(
        &mut self,
        entries: &[SearchEntry],
        scope: SearchScope,
        raw_query: &str,
    ) -> Vec<SearchHit> {
        let (query_text, target_line) = parse_query_and_line(raw_query);

        if query_text.is_empty() {
            return entries
                .iter()
                .filter(|entry| scope.includes(entry.kind))
                .take(MAX_SEARCH_HITS)
                .map(|entry| SearchHit {
                    entry: entry_with_line(entry, target_line),
                    score: 0,
                    matched_char_indices: Vec::new(),
                    target_line,
                })
                .collect();
        }

        let pattern = Pattern::new(
            query_text,
            CaseMatching::Smart,
            Normalization::Smart,
            AtomKind::Fuzzy,
        );

        let mut scored: Vec<(u32, usize)> = Vec::new();
        for (idx, entry) in entries.iter().enumerate() {
            if !scope.includes(entry.kind) {
                continue;
            }
            if let Some(score) = self.score_entry(entry, &pattern, query_text) {
                scored.push((score, idx));
            }
        }

        scored.sort_unstable_by(|a, b| compare_scored(entries, *a, *b));
        scored.truncate(MAX_SEARCH_HITS);

        scored
            .into_iter()
            .filter_map(|(score, idx)| {
                let entry = entries.get(idx)?;
                let matched_char_indices = self.title_match_indices(entry, &pattern);
                Some(SearchHit {
                    entry: entry_with_line(entry, target_line),
                    score,
                    matched_char_indices,
                    target_line,
                })
            })
            .collect()
    }

    fn score_entry(&mut self, entry: &SearchEntry, pattern: &Pattern, query: &str) -> Option<u32> {
        let matcher = if entry.kind == SearchEntryKind::File {
            &mut self.path_matcher
        } else {
            &mut self.matcher
        };

        self.utf32_buf.clear();
        let title_score = pattern.score(Utf32Str::new(&entry.title, &mut self.utf32_buf), matcher);

        let match_score = if entry.match_text == entry.title {
            title_score?
        } else {
            self.utf32_buf.clear();
            let full_score = pattern.score(
                Utf32Str::new(&entry.match_text, &mut self.utf32_buf),
                matcher,
            );
            match (title_score, full_score) {
                (Some(t), Some(f)) => t.max(f).saturating_add(TITLE_MATCH_BONUS),
                (Some(t), None) => t.saturating_add(TITLE_MATCH_BONUS),
                (None, Some(f)) => f,
                (None, None) => return None,
            }
        };

        Some(match_score.saturating_add(title_bonus(&entry.title, query)))
    }

    fn title_match_indices(&mut self, entry: &SearchEntry, pattern: &Pattern) -> Vec<u32> {
        let matcher = if entry.kind == SearchEntryKind::File {
            &mut self.path_matcher
        } else {
            &mut self.matcher
        };
        self.utf32_buf.clear();
        self.indices_buf.clear();
        let haystack = Utf32Str::new(&entry.title, &mut self.utf32_buf);
        if pattern
            .indices(haystack, matcher, &mut self.indices_buf)
            .is_none()
        {
            return Vec::new();
        }
        self.indices_buf.sort_unstable();
        self.indices_buf.dedup();
        self.indices_buf.clone()
    }
}

fn entry_with_line(entry: &SearchEntry, target_line: Option<usize>) -> SearchEntry {
    let mut cloned = entry.clone();
    if let Some(line) = target_line {
        match &mut cloned.target {
            SearchTarget::File { line: slot, .. } => {
                *slot = Some(line);
            }
            SearchTarget::Symbol { .. } | SearchTarget::Action(_) => {}
        }
    }
    cloned
}

fn title_bonus(title: &str, query: &str) -> u32 {
    if query.is_empty() {
        return 0;
    }
    if title.eq_ignore_ascii_case(query) {
        EXACT_TITLE_BONUS
    } else if title
        .get(..query.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(query))
    {
        PREFIX_TITLE_BONUS
    } else {
        0
    }
}

fn compare_scored(entries: &[SearchEntry], a: (u32, usize), b: (u32, usize)) -> Ordering {
    let lhs = entries.get(a.1);
    let rhs = entries.get(b.1);
    b.0.cmp(&a.0)
        .then_with(|| lhs.map(|e| e.title.len()).cmp(&rhs.map(|e| e.title.len())))
        .then_with(|| lhs.map(|e| e.kind).cmp(&rhs.map(|e| e.kind)))
        .then_with(|| lhs.map(|e| &e.title).cmp(&rhs.map(|e| &e.title)))
        .then_with(|| a.1.cmp(&b.1))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::keymap::ShellAction;

    fn file_entry(path: &str) -> SearchEntry {
        let p = PathBuf::from(path);
        let title = p
            .file_name()
            .map_or_else(|| path.to_owned(), |n| n.to_string_lossy().into_owned());
        let subtitle = p
            .parent()
            .map(|parent| parent.to_string_lossy().into_owned())
            .unwrap_or_default();
        SearchEntry {
            kind: SearchEntryKind::File,
            title,
            subtitle,
            match_text: path.to_owned(),
            icon: "F",
            shortcut_hint: None,
            target: SearchTarget::File {
                path: p,
                line: None,
            },
        }
    }

    fn symbol_entry(name: &str, line: usize) -> SearchEntry {
        SearchEntry {
            kind: SearchEntryKind::Symbol,
            title: name.to_owned(),
            subtitle: format!("Method · OrderApp.java:{line}"),
            match_text: name.to_owned(),
            icon: "m",
            shortcut_hint: None,
            target: SearchTarget::Symbol {
                path: None,
                line,
                col: 1,
            },
        }
    }

    fn action_entry(title: &str, action: ShellAction, shortcut: Option<&str>) -> SearchEntry {
        SearchEntry {
            kind: SearchEntryKind::Action,
            title: title.to_owned(),
            subtitle: "Action".to_owned(),
            match_text: title.to_owned(),
            icon: "A",
            shortcut_hint: shortcut.map(str::to_owned),
            target: SearchTarget::Action(action),
        }
    }

    #[test]
    fn parses_line_suffix_only_for_positive_integers() {
        assert_eq!(parse_query_and_line("OrderApp:42"), ("OrderApp", Some(42)));
        assert_eq!(
            parse_query_and_line("  Main.java:10  "),
            ("Main.java", Some(10))
        );
        assert_eq!(parse_query_and_line("OrderApp:0"), ("OrderApp:0", None));
        assert_eq!(parse_query_and_line(":42"), (":42", None));
        assert_eq!(parse_query_and_line("OrderApp:abc"), ("OrderApp:abc", None));
    }

    #[test]
    fn converts_utf32_char_indices_to_valid_utf8_byte_ranges_without_panicking() {
        let text = "ZażółćGęśląJaźń";
        // Characters: Z(0) a(1) ż(2) ó(3) ł(4) ć(5) G(6) ...
        let ranges = char_indices_to_byte_ranges(text, &[0, 2, 3, 6, 999]);
        let slices: Vec<&str> = ranges.iter().map(|r| &text[r.clone()]).collect();
        assert_eq!(slices, vec!["Z", "żó", "G"]);
        assert_eq!(char_indices_to_byte_ranges("", &[0, 1]).len(), 0);
        assert_eq!(char_indices_to_byte_ranges(text, &[]).len(), 0);
    }

    #[test]
    fn fuzzy_search_ranks_files_symbols_and_actions_and_applies_line_suffix() {
        let entries = vec![
            file_entry("src/main/java/dev/kova/demo/OrderApp.java"),
            file_entry("src/main/java/dev/kova/demo/OrderService.java"),
            symbol_entry("processOrder", 48),
            action_entry("Toggle Vim Mode", ShellAction::ToggleVimMode, None),
            action_entry("Open Settings", ShellAction::OpenSettings, Some("cmd+,")),
        ];
        let mut engine = SearchEngine::new();

        // Empty query returns starter entries filtered by scope.
        assert_eq!(engine.search(&entries, SearchScope::All, "").len(), 5);
        assert_eq!(engine.search(&entries, SearchScope::Files, "").len(), 2);
        assert_eq!(engine.search(&entries, SearchScope::Actions, "").len(), 2);

        // Fuzzy query + :line suffix updates file target line and populates highlights.
        let hits = engine.search(&entries, SearchScope::All, "OrdApp:42");
        assert_ne!(hits.len(), 0);
        assert_eq!(hits[0].entry.title, "OrderApp.java");
        assert_eq!(hits[0].target_line, Some(42));
        assert_eq!(
            hits[0].entry.target,
            SearchTarget::File {
                path: PathBuf::from("src/main/java/dev/kova/demo/OrderApp.java"),
                line: Some(42),
            }
        );
        let highlighted =
            char_indices_to_byte_ranges(&hits[0].entry.title, &hits[0].matched_char_indices);
        assert_ne!(highlighted.len(), 0);

        // Path-aware query finds file even when parent folder is included.
        let path_hits = engine.search(&entries, SearchScope::Files, "demo/OrderSer");
        assert_eq!(path_hits[0].entry.title, "OrderService.java");

        // Scope filtering restricts to Actions.
        let action_hits = engine.search(&entries, SearchScope::Actions, "settings");
        assert_eq!(action_hits.len(), 1);
        assert_eq!(action_hits[0].entry.title, "Open Settings");
    }
}
