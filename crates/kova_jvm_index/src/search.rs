//! Fuzzy search engine for *Go to Class* and *Go to Symbol* backed by `nucleo-matcher`.

use std::cmp::{Ordering, Reverse};
use std::fmt;
use std::ops::Range;

use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use rayon::prelude::*;

use crate::model::{JvmSourceOrigin, JvmSymbolKind, JvmTypeEntry, JvmVisibility};
use crate::snapshot::JvmIndexSnapshot;

/// Default maximum number of hits returned by [`JvmQueryEngine`].
pub const DEFAULT_SEARCH_LIMIT: usize = 100;

/// Options controlling *Go to Class* and *Go to Symbol* queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SearchQueryOptions {
    /// Maximum number of results to return (default: `100`).
    pub limit: usize,
    /// Whether to include non-project items (`.jar`, `.jmod`, `.class`) in results.
    pub include_jdk_and_libs: bool,
    /// Whether to include `@Deprecated` symbols (with a ranking penalty).
    pub include_deprecated: bool,
}

impl Default for SearchQueryOptions {
    fn default() -> Self {
        Self {
            limit: DEFAULT_SEARCH_LIMIT,
            include_jdk_and_libs: true,
            include_deprecated: true,
        }
    }
}

/// Parsed representation of a *Go to Class* / *Go to Symbol* query string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedClassQuery {
    /// Optional package prefix filter (e.g. `"j.u"` from `"j.u.ArrLi"`).
    pub package_filter: Option<String>,
    /// Pattern matched against the type's simple name (e.g. `"ArrLi"`).
    pub type_pattern: String,
    /// Optional member pattern after `#` (e.g. `"placeOrder"` from `"OrderService#placeOrder"`).
    pub member_pattern: Option<String>,
    /// Optional **1-based** target line number from `:line` suffix (e.g. `Some(42)` from `"OrderService:42"`).
    ///
    /// This is the number the user typed. It differs from
    /// [`JvmSourceOrigin::ProjectSource`]`::line`, which the source scanner emits as a
    /// **0-based** row: subtract 1 before navigating a 0-based editor buffer.
    pub target_line: Option<u32>,
}

/// Parses a raw user query into package, type, member, and `:line` components.
#[must_use]
pub fn parse_class_query(raw: &str) -> ParsedClassQuery {
    let trimmed = raw.trim();

    let (without_line, target_line) = if let Some((prefix, line_str)) = trimmed.rsplit_once(':') {
        if !prefix.is_empty()
            && let Ok(line) = line_str.parse::<u32>()
            && line >= 1
        {
            (prefix, Some(line))
        } else {
            (trimmed, None)
        }
    } else {
        (trimmed, None)
    };

    let (class_part, member_pattern) = if let Some((cls, mem)) = without_line.split_once('#') {
        (cls, Some(mem.to_owned()))
    } else {
        (without_line, None)
    };

    let (package_filter, type_pattern) = if let Some((pkg, ty)) = class_part.rsplit_once('.') {
        if pkg.is_empty() {
            (None, ty.to_owned())
        } else {
            (Some(pkg.to_owned()), ty.to_owned())
        }
    } else {
        (None, class_part.to_owned())
    };

    ParsedClassQuery {
        package_filter,
        type_pattern,
        member_pattern,
        target_line,
    }
}

/// A single *Go to Class* search result ready for UI rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassSearchHit {
    /// Fully qualified class name (e.g. `com.example.OrderService`).
    pub fqcn: String,
    /// JVM binary name (e.g. `com.example.OrderService$Builder`).
    pub binary_name: String,
    /// Simple class name (e.g. `OrderService`).
    pub simple_name: String,
    /// Package name (e.g. `com.example`).
    pub package_name: String,
    /// Symbol kind (class, interface, enum, record, etc.).
    pub kind: JvmSymbolKind,
    /// Source origin (project source, jar entry, etc.).
    pub origin: JvmSourceOrigin,
    /// Whether the type is `@Deprecated`.
    pub is_deprecated: bool,
    /// Combined ranking score (higher is better).
    pub score: u32,
    /// **0-based** Unicode character (`char` / UTF-32) indices in `simple_name`
    /// that matched the query, for UI highlight rendering.
    pub matched_char_indices: Vec<u32>,
    /// Optional **1-based** target line number from `:line` suffix.
    pub target_line: Option<u32>,
}

/// A single *Go to Symbol* search result ready for UI rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolSearchHit {
    /// FQCN of the owning type.
    pub owner_fqcn: String,
    /// Simple name of the owning type.
    pub owner_simple_name: String,
    /// Member name (method, constructor, or field).
    pub member_name: String,
    /// Symbol kind (`Method`, `Constructor`, or `Field`).
    pub kind: JvmSymbolKind,
    /// Human-readable signature.
    pub display_signature: String,
    /// Source origin of the member.
    pub origin: JvmSourceOrigin,
    /// Combined ranking score (higher is better).
    pub score: u32,
    /// **0-based** Unicode character (`char` / UTF-32) indices in `member_name`.
    pub matched_char_indices: Vec<u32>,
}

/// Ranking bonus for an exact (ASCII case-insensitive) simple-name match.
const EXACT_NAME_BONUS: u32 = 500;
/// Ranking bonus for a simple name that starts with the typed pattern.
const PREFIX_NAME_BONUS: u32 = 200;
/// Ranking penalty for `@Deprecated` types and members.
const DEPRECATION_PENALTY: u32 = 150;
/// Snapshots with at least this many types are scored on several threads.
const PARALLEL_QUERY_MIN_TYPES: usize = 8_192;
/// Upper bound on threads used by one keystroke query.
const MAX_QUERY_THREADS: usize = 8;

/// Reusable scratch buffers for [`JvmQueryEngine`] to avoid per-keystroke allocations.
pub struct QueryScratch {
    matcher: Matcher,
    parallel_matchers: Vec<Matcher>,
    indices: Vec<u32>,
    utf32_buf: Vec<char>,
    candidates: Vec<(u32, usize)>,
}

impl fmt::Debug for QueryScratch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueryScratch")
            .field("candidates_capacity", &self.candidates.capacity())
            .finish_non_exhaustive()
    }
}

impl Default for QueryScratch {
    fn default() -> Self {
        Self::new()
    }
}

impl QueryScratch {
    /// Creates a new [`QueryScratch`] with default `nucleo-matcher` configuration.
    #[must_use]
    pub fn new() -> Self {
        Self {
            matcher: Matcher::new(Config::DEFAULT),
            parallel_matchers: Vec::new(),
            indices: Vec::with_capacity(64),
            utf32_buf: Vec::with_capacity(128),
            candidates: Vec::new(),
        }
    }

    /// Grows the per-chunk matcher pool to `count` (allocated once, then reused).
    fn ensure_parallel_matchers(&mut self, count: usize) {
        while self.parallel_matchers.len() < count {
            self.parallel_matchers.push(Matcher::new(Config::DEFAULT));
        }
    }

    /// Runs `pattern` against `haystack` and returns the sorted,
    /// deduplicated 0-based character indices.
    fn match_indices(&mut self, pattern: &Pattern, haystack: Utf32Str<'_>) -> Vec<u32> {
        self.indices.clear();
        if pattern
            .indices(haystack, &mut self.matcher, &mut self.indices)
            .is_none()
        {
            return Vec::new();
        }
        self.indices.sort_unstable();
        self.indices.dedup();
        self.indices.clone()
    }
}

/// Stateless fuzzy query engine operating on [`JvmIndexSnapshot`].
#[derive(Debug, Clone, Copy, Default)]
pub struct JvmQueryEngine;

impl JvmQueryEngine {
    /// Creates a new [`JvmQueryEngine`].
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Searches indexed classes using `nucleo-matcher` with `CamelHumps`, package prefixes,
    /// and `:line` suffix support.
    ///
    /// An empty query returns up to `options.limit` types ranked by origin
    /// (project sources first) and then alphabetically.
    #[must_use]
    pub fn search_classes(
        &self,
        snapshot: &JvmIndexSnapshot,
        raw_query: &str,
        options: &SearchQueryOptions,
        scratch: &mut QueryScratch,
    ) -> Vec<ClassSearchHit> {
        if options.limit == 0 {
            return Vec::new();
        }

        let parsed = parse_class_query(raw_query);
        let type_pattern = (!parsed.type_pattern.is_empty()).then(|| fuzzy(&parsed.type_pattern));
        let pkg_pattern = parsed.package_filter.as_deref().map(fuzzy);

        let query = ClassQuery {
            type_pattern: type_pattern.as_ref(),
            type_text: &parsed.type_pattern,
            pkg_pattern: pkg_pattern.as_ref(),
            options,
        };

        let mut candidates = std::mem::take(&mut scratch.candidates);
        candidates.clear();
        let type_count = snapshot.type_count();
        let threads = rayon::current_num_threads().clamp(1, MAX_QUERY_THREADS);
        if type_count >= PARALLEL_QUERY_MIN_TYPES && threads > 1 {
            // Score disjoint chunks in parallel, each with its own matcher, and
            // keep only every chunk's local top `limit` before the final selection.
            scratch.ensure_parallel_matchers(threads);
            let chunk_len = type_count.div_ceil(threads);
            let limit = options.limit;
            let parts: Vec<Vec<(u32, usize)>> = scratch
                .parallel_matchers
                .par_iter_mut()
                .take(threads)
                .enumerate()
                .map(|(chunk, matcher)| {
                    let start = chunk * chunk_len;
                    let range = start..type_count.min(start + chunk_len);
                    let mut local = Vec::new();
                    query.score_range(snapshot, range, matcher, &mut local);
                    keep_best(&mut local, limit, |c| class_select_key(snapshot, c));
                    local
                })
                .collect();
            candidates.extend(parts.into_iter().flatten());
        } else {
            query.score_range(
                snapshot,
                0..type_count,
                &mut scratch.matcher,
                &mut candidates,
            );
        }

        let types = snapshot.types();
        // Integer-only key for the O(n) selection: ties in score are cut by
        // visibility and index order (FQCN order for workspace snapshots).
        let select_key = |c: &(u32, usize)| class_select_key(snapshot, c);
        let compare = |a: &(u32, usize), b: &(u32, usize)| compare_types(types, *a, *b);
        select_top(&mut candidates, options.limit, select_key, compare);

        let hits = candidates
            .iter()
            .filter_map(|&(score, idx)| {
                let ty = types.get(idx)?;
                let matched_char_indices = match (&type_pattern, snapshot.simple_name_utf32(idx)) {
                    (Some(pattern), Some(name)) => scratch.match_indices(pattern, name.slice(..)),
                    _ => Vec::new(),
                };
                Some(ClassSearchHit {
                    fqcn: ty.fqcn.clone(),
                    binary_name: ty.binary_name.clone(),
                    simple_name: ty.simple_name.clone(),
                    package_name: ty.package_name.clone(),
                    kind: ty.kind,
                    origin: ty.origin.clone(),
                    is_deprecated: ty.modifiers.is_deprecated,
                    score,
                    matched_char_indices,
                    target_line: parsed.target_line,
                })
            })
            .collect();

        scratch.candidates = candidates;
        hits
    }

    /// Searches indexed members (*Go to Symbol*), supporting both `"memberName"`
    /// and `"OwnerClass#memberName"` (optionally package-qualified: `"c.e.Owner#member"`).
    #[must_use]
    pub fn search_symbols(
        &self,
        snapshot: &JvmIndexSnapshot,
        raw_query: &str,
        options: &SearchQueryOptions,
        scratch: &mut QueryScratch,
    ) -> Vec<SymbolSearchHit> {
        if options.limit == 0 {
            return Vec::new();
        }

        let parsed = parse_class_query(raw_query);
        let (owner_query, pkg_query, member_query) = match parsed.member_pattern {
            Some(member) => (
                Some(parsed.type_pattern).filter(|owner| !owner.is_empty()),
                parsed.package_filter,
                member,
            ),
            None => (None, None, parsed.type_pattern),
        };
        if member_query.is_empty() {
            return Vec::new();
        }

        let member_pattern = fuzzy(&member_query);
        let owner_pattern = owner_query.as_deref().map(fuzzy);
        let pkg_pattern = pkg_query.as_deref().map(fuzzy);

        // (score, global member index, owner type index)
        let mut candidates: Vec<(u32, usize, usize)> = Vec::new();
        let mut member_base = 0usize;

        for (ty_idx, ty) in snapshot.types().iter().enumerate() {
            let members = snapshot.members_at(ty_idx);
            let base = member_base;
            member_base += members.len();
            if members.is_empty() || !include_type(ty, options) {
                continue;
            }

            let mut owner_score = 0u32;
            if let Some(pattern) = &owner_pattern {
                let Some(name) = snapshot.simple_name_utf32(ty_idx) else {
                    continue;
                };
                let Some(score) = pattern.score(name.slice(..), &mut scratch.matcher) else {
                    continue;
                };
                owner_score = score / 2;
            }
            if let Some(pattern) = &pkg_pattern {
                let Some(pkg) = snapshot.package_name_utf32(ty_idx) else {
                    continue;
                };
                let Some(score) = pattern.score(pkg.slice(..), &mut scratch.matcher) else {
                    continue;
                };
                owner_score = owner_score.saturating_add(score / 4);
            }

            for (offset, member) in members.iter().enumerate() {
                if !options.include_deprecated && member.modifiers.is_deprecated {
                    continue;
                }
                scratch.utf32_buf.clear();
                let haystack = Utf32Str::new(&member.name, &mut scratch.utf32_buf);
                let Some(member_score) = member_pattern.score(haystack, &mut scratch.matcher)
                else {
                    continue;
                };
                let score = rank(
                    member_score
                        .saturating_add(name_bonus(&member.name, &member_query))
                        .saturating_add(owner_score),
                    &ty.origin,
                    member.modifiers.is_deprecated || ty.modifiers.is_deprecated,
                );
                candidates.push((score, base + offset, ty_idx));
            }
        }

        let all_members = snapshot.members();
        let types = snapshot.types();
        let compare = |a: &(u32, usize, usize), b: &(u32, usize, usize)| {
            b.0.cmp(&a.0)
                .then_with(|| {
                    let lhs = all_members.get(a.1).map(|m| m.name.as_str());
                    let rhs = all_members.get(b.1).map(|m| m.name.as_str());
                    lhs.cmp(&rhs)
                })
                .then_with(|| {
                    let lhs = types.get(a.2).map(|t| t.fqcn.as_str());
                    let rhs = types.get(b.2).map(|t| t.fqcn.as_str());
                    lhs.cmp(&rhs)
                })
                .then_with(|| a.1.cmp(&b.1))
        };
        let select_key = |c: &(u32, usize, usize)| (Reverse(c.0), c.1);
        select_top(&mut candidates, options.limit, select_key, compare);
        build_symbol_hits(snapshot, &candidates, &member_pattern, scratch)
    }
}

fn build_symbol_hits(
    snapshot: &JvmIndexSnapshot,
    candidates: &[(u32, usize, usize)],
    member_pattern: &Pattern,
    scratch: &mut QueryScratch,
) -> Vec<SymbolSearchHit> {
    let all_members = snapshot.members();
    let types = snapshot.types();
    candidates
        .iter()
        .filter_map(|&(score, member_idx, ty_idx)| {
            let member = all_members.get(member_idx)?;
            let ty = types.get(ty_idx)?;
            scratch.utf32_buf.clear();
            let haystack = Utf32Str::new(&member.name, &mut scratch.utf32_buf);
            scratch.indices.clear();
            member_pattern.indices(haystack, &mut scratch.matcher, &mut scratch.indices);
            scratch.indices.sort_unstable();
            scratch.indices.dedup();
            Some(SymbolSearchHit {
                owner_fqcn: ty.fqcn.clone(),
                owner_simple_name: ty.simple_name.clone(),
                member_name: member.name.clone(),
                kind: member.kind,
                display_signature: member.display_signature.clone(),
                origin: ty.origin.clone(),
                score,
                matched_char_indices: scratch.indices.clone(),
            })
        })
        .collect()
}

/// Compiled *Go to Class* query shared (read-only) by all scoring chunks.
struct ClassQuery<'a> {
    type_pattern: Option<&'a Pattern>,
    type_text: &'a str,
    pkg_pattern: Option<&'a Pattern>,
    options: &'a SearchQueryOptions,
}

impl ClassQuery<'_> {
    /// Scores the types in `range`, appending `(score, type index)` for every match.
    fn score_range(
        &self,
        snapshot: &JvmIndexSnapshot,
        range: Range<usize>,
        matcher: &mut Matcher,
        out: &mut Vec<(u32, usize)>,
    ) {
        let types = snapshot.types();
        for idx in range {
            let Some(ty) = types.get(idx) else {
                break;
            };
            if !include_type(ty, self.options) {
                continue;
            }

            let pkg_score = match (self.pkg_pattern, snapshot.package_name_utf32(idx)) {
                (Some(pattern), Some(pkg)) => match pattern.score(pkg.slice(..), matcher) {
                    Some(score) => score / 2,
                    None => continue,
                },
                (Some(_), None) => continue,
                (None, _) => 0,
            };

            let type_score = match (self.type_pattern, snapshot.simple_name_utf32(idx)) {
                (Some(pattern), Some(name)) => match pattern.score(name.slice(..), matcher) {
                    Some(score) => {
                        score.saturating_add(name_bonus(&ty.simple_name, self.type_text))
                    }
                    None => continue,
                },
                (Some(_), None) => continue,
                (None, _) => 0,
            };

            let score = rank(
                type_score.saturating_add(pkg_score),
                &ty.origin,
                ty.modifiers.is_deprecated,
            );
            out.push((score, idx));
        }
    }
}

/// Integer-only selection key: score (desc), then visibility, then index order
/// (FQCN order for workspace snapshots). Avoids string compares over 50k candidates.
fn class_select_key(
    snapshot: &JvmIndexSnapshot,
    candidate: &(u32, usize),
) -> (Reverse<u32>, u8, usize) {
    (
        Reverse(candidate.0),
        visibility_rank(snapshot.types().get(candidate.1)),
        candidate.1,
    )
}

fn fuzzy(pattern: &str) -> Pattern {
    Pattern::new(
        pattern,
        CaseMatching::Smart,
        Normalization::Smart,
        AtomKind::Fuzzy,
    )
}

/// Keeps the best `limit` candidates by `key` (O(n) selection), in no particular order.
fn keep_best<T, K, F>(candidates: &mut Vec<T>, limit: usize, key: F)
where
    K: Ord,
    F: Fn(&T) -> K,
{
    if limit == 0 {
        candidates.clear();
    } else if candidates.len() > limit {
        candidates.select_nth_unstable_by(limit - 1, |a, b| key(a).cmp(&key(b)));
        candidates.truncate(limit);
    }
}

/// Keeps the best `limit` candidates and sorts them by `compare`.
///
/// Selection runs in O(n) on the cheap integer `key`; the (string-comparing)
/// `compare` only orders the kept `limit` candidates.
fn select_top<T, K, F, C>(candidates: &mut Vec<T>, limit: usize, key: F, compare: C)
where
    K: Ord,
    F: Fn(&T) -> K,
    C: Fn(&T, &T) -> Ordering,
{
    keep_best(candidates, limit, key);
    candidates.sort_unstable_by(compare);
}

fn compare_types(types: &[JvmTypeEntry], a: (u32, usize), b: (u32, usize)) -> Ordering {
    let lhs = types.get(a.1);
    let rhs = types.get(b.1);
    b.0.cmp(&a.0)
        .then_with(|| visibility_rank(lhs).cmp(&visibility_rank(rhs)))
        .then_with(|| {
            lhs.map(|t| &t.simple_name)
                .cmp(&rhs.map(|t| &t.simple_name))
        })
        .then_with(|| lhs.map(|t| &t.fqcn).cmp(&rhs.map(|t| &t.fqcn)))
        .then_with(|| a.1.cmp(&b.1))
}

/// Tie-break rank: public/protected/package types before private and Kotlin-internal ones.
fn visibility_rank(ty: Option<&JvmTypeEntry>) -> u8 {
    match ty.map(|t| t.modifiers.visibility) {
        Some(JvmVisibility::Public | JvmVisibility::Protected | JvmVisibility::PackagePrivate) => 0,
        Some(JvmVisibility::Private | JvmVisibility::Internal) => 1,
        None => 2,
    }
}

/// Exact or prefix match bonus, compared ASCII case-insensitively without allocating.
fn name_bonus(name: &str, pattern: &str) -> u32 {
    if pattern.is_empty() {
        0
    } else if name.eq_ignore_ascii_case(pattern) {
        EXACT_NAME_BONUS
    } else if name
        .get(..pattern.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(pattern))
    {
        PREFIX_NAME_BONUS
    } else {
        0
    }
}

fn rank(match_score: u32, origin: &JvmSourceOrigin, is_deprecated: bool) -> u32 {
    let score = match_score.saturating_add(origin_bonus(origin));
    if is_deprecated {
        score.saturating_sub(DEPRECATION_PENALTY)
    } else {
        score
    }
}

fn include_type(ty: &JvmTypeEntry, options: &SearchQueryOptions) -> bool {
    if !options.include_deprecated && ty.modifiers.is_deprecated {
        return false;
    }
    options.include_jdk_and_libs || matches!(ty.origin, JvmSourceOrigin::ProjectSource { .. })
}

/// Origin ranking: `ProjectSource > ClassFile > JarEntry > JmodEntry`.
fn origin_bonus(origin: &JvmSourceOrigin) -> u32 {
    match origin {
        JvmSourceOrigin::ProjectSource { .. } => 300,
        JvmSourceOrigin::ClassFile { .. } => 120,
        JvmSourceOrigin::JarEntry { .. } => 60,
        JvmSourceOrigin::JmodEntry { .. } => 20,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_line_member_and_package_parts() {
        let parsed = parse_class_query("com.ex.OrderService#placeOrder:42");
        assert_eq!(parsed.package_filter.as_deref(), Some("com.ex"));
        assert_eq!(parsed.type_pattern, "OrderService");
        assert_eq!(parsed.member_pattern.as_deref(), Some("placeOrder"));
        assert_eq!(parsed.target_line, Some(42));

        let no_line = parse_class_query("Foo:0");
        assert_eq!(no_line.target_line, None);
        assert_eq!(no_line.type_pattern, "Foo:0");

        let leading_dot = parse_class_query(".Foo");
        assert_eq!(leading_dot.package_filter, None);
        assert_eq!(leading_dot.type_pattern, "Foo");
    }

    #[test]
    fn name_bonus_handles_exact_prefix_and_multibyte() {
        assert_eq!(name_bonus("OrderService", "orderservice"), EXACT_NAME_BONUS);
        assert_eq!(name_bonus("OrderService", "Order"), PREFIX_NAME_BONUS);
        assert_eq!(name_bonus("ŻółćService", "Ż"), PREFIX_NAME_BONUS);
        // `get(..1)` falls inside the 2-byte `Ż`: no panic, no bonus.
        assert_eq!(name_bonus("ŻółćService", "x"), 0);
        assert_eq!(name_bonus("Order", ""), 0);
    }

    #[test]
    fn select_top_keeps_best_in_order() {
        let mut values = vec![5, 1, 9, 3, 7];
        select_top(
            &mut values,
            3,
            |v: &i32| Reverse(*v),
            |a: &i32, b: &i32| b.cmp(a),
        );
        assert_eq!(values, vec![9, 7, 5]);
    }
}
