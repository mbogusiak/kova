//! Immutable, thread-safe index snapshot for lock-free 120 FPS queries.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use nucleo_matcher::Utf32String;

use crate::indexer::JvmWorkspaceIndex;
use crate::model::{JvmMemberEntry, JvmTypeEntry, ParsedClassUnit};

/// Pre-converted `nucleo-matcher` haystacks for one indexed type.
///
/// Shared through [`Arc`] between consecutive snapshots so that an incremental
/// update of one project file does not re-convert tens of thousands of library
/// type names.
#[derive(Debug)]
struct TypeSearchNames {
    simple_name: Utf32String,
    package_name: Utf32String,
}

/// Immutable, `Send + Sync` snapshot of indexed JVM types and members.
///
/// Type `simple_name` and `package_name` strings are converted to [`Utf32String`]
/// once, when the snapshot is built, so keystroke queries in `nucleo-matcher`
/// perform no UTF-8 -> UTF-32 conversion across 50,000+ classes. Member names are
/// matched lazily through the query scratch buffer to keep RSS low.
#[derive(Debug, Clone, Default)]
pub struct JvmIndexSnapshot {
    types: Vec<JvmTypeEntry>,
    members: Vec<JvmMemberEntry>,
    member_ranges: Vec<Range<usize>>,
    search_names: Vec<Arc<TypeSearchNames>>,
    fqcn_to_idx: HashMap<String, usize>,
    binary_to_idx: HashMap<String, usize>,
    package_to_types: HashMap<String, Vec<usize>>,
}

const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<JvmIndexSnapshot>();
};

impl JvmIndexSnapshot {
    /// Creates an empty [`JvmIndexSnapshot`].
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Builds a [`JvmIndexSnapshot`] from a [`JvmWorkspaceIndex`].
    #[must_use]
    pub fn from_workspace_index(index: &JvmWorkspaceIndex) -> Self {
        Self::from_units_reusing(index.types(), None)
    }

    /// Builds a [`JvmIndexSnapshot`] from a [`JvmWorkspaceIndex`], reusing the
    /// pre-converted search names of every type whose name is unchanged in `previous`.
    #[must_use]
    pub fn from_workspace_index_reusing(
        index: &JvmWorkspaceIndex,
        previous: &JvmIndexSnapshot,
    ) -> Self {
        Self::from_units_reusing(index.types(), Some(previous))
    }

    /// Builds a [`JvmIndexSnapshot`] from a slice of [`ParsedClassUnit`]s.
    #[must_use]
    pub fn from_units(units: &[ParsedClassUnit]) -> Self {
        Self::from_units_reusing(units, None)
    }

    /// Builds a [`JvmIndexSnapshot`] from `units`, sharing pre-converted search
    /// names with `previous` for types whose FQCN, simple name and package are unchanged.
    #[must_use]
    pub fn from_units_reusing(
        units: &[ParsedClassUnit],
        previous: Option<&JvmIndexSnapshot>,
    ) -> Self {
        let type_count = units.len();
        let member_count: usize = units.iter().map(|u| u.members.len()).sum();

        let mut types = Vec::with_capacity(type_count);
        let mut members = Vec::with_capacity(member_count);
        let mut member_ranges = Vec::with_capacity(type_count);
        let mut search_names = Vec::with_capacity(type_count);
        let mut fqcn_to_idx = HashMap::with_capacity(type_count);
        let mut binary_to_idx = HashMap::with_capacity(type_count);
        let mut package_to_types: HashMap<String, Vec<usize>> = HashMap::new();

        for (idx, unit) in units.iter().enumerate() {
            let ty = &unit.ty;
            fqcn_to_idx.insert(ty.fqcn.clone(), idx);
            binary_to_idx.insert(ty.binary_name.clone(), idx);
            package_to_types
                .entry(ty.package_name.clone())
                .or_default()
                .push(idx);

            let reused = previous.and_then(|prev| prev.reusable_names(ty));
            search_names.push(reused.unwrap_or_else(|| {
                Arc::new(TypeSearchNames {
                    simple_name: Utf32String::from(ty.simple_name.as_str()),
                    package_name: Utf32String::from(ty.package_name.as_str()),
                })
            }));

            let start = members.len();
            members.extend(unit.members.iter().cloned());
            member_ranges.push(start..members.len());

            types.push(ty.clone());
        }

        Self {
            types,
            members,
            member_ranges,
            search_names,
            fqcn_to_idx,
            binary_to_idx,
            package_to_types,
        }
    }

    fn reusable_names(&self, ty: &JvmTypeEntry) -> Option<Arc<TypeSearchNames>> {
        let idx = *self.fqcn_to_idx.get(&ty.fqcn)?;
        let old = self.types.get(idx)?;
        if old.simple_name == ty.simple_name && old.package_name == ty.package_name {
            self.search_names.get(idx).map(Arc::clone)
        } else {
            None
        }
    }

    /// Returns `true` if `self` and `other` share the same pre-converted search
    /// names allocation for `fqcn` (i.e. it was reused, not re-converted).
    #[must_use]
    pub fn shares_search_names_with(&self, other: &JvmIndexSnapshot, fqcn: &str) -> bool {
        let lhs = self
            .fqcn_to_idx
            .get(fqcn)
            .and_then(|&idx| self.search_names.get(idx));
        let rhs = other
            .fqcn_to_idx
            .get(fqcn)
            .and_then(|&idx| other.search_names.get(idx));
        matches!((lhs, rhs), (Some(a), Some(b)) if Arc::ptr_eq(a, b))
    }

    /// Resolves a type by its fully qualified class name (e.g. `java.util.ArrayList`) in O(1).
    #[must_use]
    pub fn resolve_fqcn(&self, fqcn: &str) -> Option<&JvmTypeEntry> {
        self.fqcn_to_idx
            .get(fqcn)
            .and_then(|&idx| self.types.get(idx))
    }

    /// Resolves a type by its JVM binary name (e.g. `java.util.Map$Entry`) in O(1).
    #[must_use]
    pub fn resolve_binary_name(&self, binary_name: &str) -> Option<&JvmTypeEntry> {
        self.binary_to_idx
            .get(binary_name)
            .and_then(|&idx| self.types.get(idx))
    }

    /// Returns all indexed members belonging to `fqcn` in O(1).
    #[must_use]
    pub fn members_of(&self, fqcn: &str) -> &[JvmMemberEntry] {
        self.fqcn_to_idx
            .get(fqcn)
            .map_or(&[], |&idx| self.members_at(idx))
    }

    /// Returns all indexed members of the type at position `type_idx` in [`Self::types`].
    #[must_use]
    pub fn members_at(&self, type_idx: usize) -> &[JvmMemberEntry] {
        self.member_ranges
            .get(type_idx)
            .and_then(|range| self.members.get(range.clone()))
            .unwrap_or(&[])
    }

    /// Returns all types declared directly in `package_name`.
    #[must_use]
    pub fn types_in_package(&self, package_name: &str) -> Vec<&JvmTypeEntry> {
        self.package_to_types
            .get(package_name)
            .map(|indices| {
                indices
                    .iter()
                    .filter_map(|&idx| self.types.get(idx))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Returns the total number of indexed types.
    #[must_use]
    pub fn type_count(&self) -> usize {
        self.types.len()
    }

    /// Returns the total number of indexed members.
    #[must_use]
    pub fn member_count(&self) -> usize {
        self.members.len()
    }

    /// Returns a slice of all indexed types.
    #[must_use]
    pub fn types(&self) -> &[JvmTypeEntry] {
        &self.types
    }

    /// Returns a slice of all indexed members.
    #[must_use]
    pub fn members(&self) -> &[JvmMemberEntry] {
        &self.members
    }

    /// Returns the pre-converted UTF-32 `simple_name` for the type at `idx`.
    #[must_use]
    pub fn simple_name_utf32(&self, idx: usize) -> Option<&Utf32String> {
        self.search_names.get(idx).map(|names| &names.simple_name)
    }

    /// Returns the pre-converted UTF-32 `package_name` for the type at `idx`.
    #[must_use]
    pub fn package_name_utf32(&self, idx: usize) -> Option<&Utf32String> {
        self.search_names.get(idx).map(|names| &names.package_name)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::model::{JvmModifiers, JvmSourceOrigin, JvmSymbolKind};

    fn unit(fqcn: &str, members: &[&str]) -> ParsedClassUnit {
        let (package_name, simple_name) = crate::model::split_package_and_simple(fqcn);
        ParsedClassUnit {
            ty: JvmTypeEntry {
                fqcn: fqcn.to_owned(),
                binary_name: fqcn.to_owned(),
                simple_name,
                package_name,
                kind: JvmSymbolKind::Class,
                modifiers: JvmModifiers::default(),
                super_fqcn: None,
                interfaces: Vec::new(),
                origin: JvmSourceOrigin::ClassFile {
                    path: PathBuf::from("A.class"),
                },
            },
            members: members
                .iter()
                .map(|name| JvmMemberEntry {
                    owner_fqcn: fqcn.to_owned(),
                    name: (*name).to_owned(),
                    kind: JvmSymbolKind::Method,
                    modifiers: JvmModifiers::default(),
                    descriptor: String::from("()V"),
                    display_signature: String::from("(): void"),
                    line: None,
                })
                .collect(),
        }
    }

    #[test]
    fn resolves_types_members_and_packages() {
        let snap = JvmIndexSnapshot::from_units(&[
            unit("a.b.First", &["run", "stop"]),
            unit("a.b.Second", &[]),
            unit("c.Third", &["go"]),
        ]);
        assert_eq!(snap.type_count(), 3);
        assert_eq!(snap.member_count(), 3);
        assert!(snap.resolve_fqcn("a.b.Second").is_some());
        assert!(snap.resolve_binary_name("c.Third").is_some());
        assert_eq!(snap.members_of("a.b.First").len(), 2);
        assert_eq!(snap.members_of("a.b.Second").len(), 0);
        assert_eq!(snap.members_of("missing.Type").len(), 0);
        assert_eq!(snap.types_in_package("a.b").len(), 2);
        assert_eq!(
            snap.simple_name_utf32(2).map(ToString::to_string),
            Some(String::from("Third"))
        );
    }

    #[test]
    fn reuses_search_names_for_unchanged_types() {
        let first = JvmIndexSnapshot::from_units(&[unit("a.Keep", &[]), unit("a.Gone", &[])]);
        let second = JvmIndexSnapshot::from_units_reusing(
            &[unit("a.Added", &[]), unit("a.Keep", &[])],
            Some(&first),
        );
        assert!(second.shares_search_names_with(&first, "a.Keep"));
        assert!(!second.shares_search_names_with(&first, "a.Added"));
        assert!(!second.shares_search_names_with(&first, "a.Gone"));
    }
}
