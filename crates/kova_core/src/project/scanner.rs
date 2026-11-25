//! `.gitignore`-respecting project tree scanner producing a flat-arena [`ProjectTree`].

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Component, Path, PathBuf};

use ignore::WalkBuilder;

use super::{ProjectError, ensure_canonical_dir, project_display_name};
use crate::project::session::is_safe_relative_path;

/// Default hard cap on scanned non-root entries to keep memory well under 150 MiB.
pub const DEFAULT_MAX_ENTRIES: usize = 250_000;

/// Options controlling `.gitignore` handling, symlink traversal, and safety limits.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "scanner options expose independent boolean traversal toggles"
)]
pub struct ProjectScanOptions {
    /// Respect `.gitignore`, nested `.gitignore`, `.git/info/exclude`, and `.ignore`.
    pub respect_gitignore: bool,
    /// Respect the user's global gitignore configuration (defaults to `false` for deterministic scans).
    pub respect_global_ignore: bool,
    /// Include dotfiles such as `.gitignore`, `.editorconfig`, and `.github/` (while still honoring `skip_git_dir`).
    pub include_hidden: bool,
    /// Always skip `.git/` metadata directories and submodule `.git` files.
    pub skip_git_dir: bool,
    /// Follow symbolic links while traversing directories (defaults to `false` to prevent symlink loops).
    pub follow_symlinks: bool,
    /// Additional directory/file names, `*.ext` globs, or relative paths to exclude.
    pub extra_excludes: Vec<String>,
    /// Optional maximum traversal depth relative to `project_root`.
    pub max_depth: Option<usize>,
    /// Maximum number of non-root entries retained before setting [`ProjectTree::truncated`].
    pub max_entries: usize,
}

impl Default for ProjectScanOptions {
    fn default() -> Self {
        Self {
            respect_gitignore: true,
            respect_global_ignore: false,
            include_hidden: true,
            skip_git_dir: true,
            follow_symlinks: false,
            extra_excludes: Vec::new(),
            max_depth: None,
            max_entries: DEFAULT_MAX_ENTRIES,
        }
    }
}

/// Stable handle referencing a [`ProjectNode`] inside [`ProjectTree`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProjectNodeId(usize);

impl ProjectNodeId {
    /// Identifier of the root directory node (`ProjectNodeId(0)`).
    pub const ROOT: Self = Self(0);

    /// Returns the zero-based arena index of this node.
    #[must_use]
    pub const fn index(self) -> usize {
        self.0
    }
}

impl fmt::Display for ProjectNodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "node#{}", self.0)
    }
}

/// Kind-specific payload of a [`ProjectNode`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectNodeKind {
    /// Directory node containing sorted child node handles (directories first, then files).
    Directory {
        /// Ordered child node identifiers.
        children: Vec<ProjectNodeId>,
    },
    /// Regular file (or unfollowed non-directory leaf) entry.
    File {
        /// File size in bytes as reported by filesystem metadata.
        size_bytes: u64,
        /// Lowercased or verbatim file extension without the leading dot, if present.
        extension: Option<String>,
    },
}

/// Single node in a scanned [`ProjectTree`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectNode {
    /// Arena identifier of this node.
    pub id: ProjectNodeId,
    /// Parent directory identifier (`None` only for [`ProjectNodeId::ROOT`]).
    pub parent: Option<ProjectNodeId>,
    /// Basename of this file or directory.
    pub name: String,
    /// Path relative to `project_root` (empty for root; pure `Normal` components otherwise).
    pub rel_path: PathBuf,
    /// Node payload (`Directory` or `File`).
    pub kind: ProjectNodeKind,
}

/// Flat file index entry sorted deterministically by `rel_path` for fast *Go to File* lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectFileEntry {
    /// Arena identifier of the file node inside [`ProjectTree`].
    pub node_id: ProjectNodeId,
    /// Project-relative path to the file.
    pub rel_path: PathBuf,
    /// File extension without the leading dot, if present.
    pub extension: Option<String>,
    /// File size in bytes.
    pub size_bytes: u64,
}

/// Arena-backed representation of a scanned project workspace tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectTree {
    nodes: Vec<ProjectNode>,
    files: Vec<ProjectFileEntry>,
    path_index: HashMap<PathBuf, ProjectNodeId>,
    dir_count: usize,
    file_count: usize,
    /// `true` when scanning stopped early because `max_entries` was exceeded.
    pub truncated: bool,
}

impl ProjectTree {
    /// Returns [`ProjectNodeId::ROOT`].
    #[must_use]
    pub const fn root_id(&self) -> ProjectNodeId {
        ProjectNodeId::ROOT
    }

    /// Looks up a node by its [`ProjectNodeId`].
    #[must_use]
    pub fn get(&self, id: ProjectNodeId) -> Option<&ProjectNode> {
        self.nodes.get(id.0)
    }

    /// Looks up a node by its path relative to `project_root`.
    #[must_use]
    pub fn find_by_rel_path(&self, rel_path: &Path) -> Option<ProjectNodeId> {
        self.path_index.get(rel_path).copied()
    }

    /// Returns the sorted slice of child IDs for `dir_id`, or an empty slice for files/unknown IDs.
    #[must_use]
    pub fn children(&self, dir_id: ProjectNodeId) -> &[ProjectNodeId] {
        match self.nodes.get(dir_id.0) {
            Some(ProjectNode {
                kind: ProjectNodeKind::Directory { children },
                ..
            }) => children.as_slice(),
            _ => &[],
        }
    }

    /// Returns the flat, deterministically sorted slice of all indexed files.
    #[must_use]
    pub fn files(&self) -> &[ProjectFileEntry] {
        &self.files
    }

    /// Returns the number of indexed files in the tree.
    #[must_use]
    pub const fn file_count(&self) -> usize {
        self.file_count
    }

    /// Returns the number of directory nodes in the tree (excluding the project root itself).
    #[must_use]
    pub const fn dir_count(&self) -> usize {
        self.dir_count
    }

    /// Returns `true` if scanning was truncated at `max_entries`.
    #[must_use]
    pub const fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// Incrementally rescans `dir_rel_path` under `root` and updates the tree arena in place.
    ///
    /// Passing an empty path (`""`) or `"."` falls back to a full [`scan_project`]. If
    /// `dir_rel_path` was removed from disk, its nodes are pruned from the tree without
    /// rescanning sibling subtrees.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectError::InvalidProjectPath`] if `dir_rel_path` escapes the project root,
    /// [`ProjectError::NotFound`] / [`ProjectError::NotADirectory`] if `root` is invalid or
    /// `dir_rel_path` exists as a non-directory, or [`ProjectError::Io`] on filesystem errors.
    pub fn rescan_subtree(
        &mut self,
        root: &Path,
        dir_rel_path: &Path,
        options: &ProjectScanOptions,
    ) -> Result<(), ProjectError> {
        if dir_rel_path.as_os_str().is_empty() || dir_rel_path == Path::new(".") {
            *self = scan_project(root, options)?;
            return Ok(());
        }

        if !is_safe_relative_path(dir_rel_path) {
            return Err(ProjectError::InvalidProjectPath {
                path: dir_rel_path.to_path_buf(),
                reason: "subtree path must be a normalized relative path without `.` or `..`"
                    .to_owned(),
            });
        }

        let canonical_root = ensure_canonical_dir(root)?;
        let target_abs = canonical_root.join(dir_rel_path);

        let mut surviving: BTreeMap<PathBuf, RawEntry> = BTreeMap::new();
        for node in &self.nodes {
            if node.id == ProjectNodeId::ROOT {
                continue;
            }
            if node.rel_path == dir_rel_path || node.rel_path.starts_with(dir_rel_path) {
                continue;
            }
            surviving.insert(node.rel_path.clone(), RawEntry::from_node(node));
        }

        let mut truncated = self.truncated;
        match std::fs::metadata(&target_abs) {
            Ok(meta) => {
                if !meta.is_dir() {
                    return Err(ProjectError::NotADirectory { path: target_abs });
                }
                let canonical_target =
                    std::fs::canonicalize(&target_abs).map_err(|err| ProjectError::Io {
                        path: target_abs.clone(),
                        message: err.to_string(),
                    })?;
                if !options.follow_symlinks && !canonical_target.starts_with(&canonical_root) {
                    return Err(ProjectError::InvalidProjectPath {
                        path: dir_rel_path.to_path_buf(),
                        reason: "subtree symlink resolves outside project root".to_owned(),
                    });
                }

                let dir_depth = dir_rel_path.components().count();
                let within_depth = options.max_depth.is_none_or(|max_d| dir_depth <= max_d);

                if within_depth && !is_excluded_rel_path(dir_rel_path, options) {
                    let budget = options.max_entries.saturating_sub(surviving.len());
                    if budget == 0 {
                        truncated = true;
                    } else {
                        surviving.insert(dir_rel_path.to_path_buf(), RawEntry::directory());
                        let child_budget = options.max_entries.saturating_sub(surviving.len());
                        let sub_max_depth = options
                            .max_depth
                            .map(|max_d| max_d.saturating_sub(dir_depth));

                        if sub_max_depth != Some(0) {
                            let mut sub_options = options.clone();
                            sub_options.max_depth = sub_max_depth;
                            let (sub_entries, sub_truncated) = collect_walk_entries(
                                &canonical_root,
                                &target_abs,
                                &sub_options,
                                child_budget,
                            )?;
                            truncated = truncated || sub_truncated;
                            for (path, entry) in sub_entries {
                                surviving.insert(path, entry);
                            }
                        }
                    }
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                // Directory removed on disk; surviving map already omits the subtree.
            }
            Err(err) => {
                return Err(ProjectError::Io {
                    path: target_abs,
                    message: err.to_string(),
                });
            }
        }

        *self = build_tree_from_entries(&canonical_root, surviving, truncated);
        Ok(())
    }
}

/// Scans `root` synchronously using `ignore::WalkBuilder` and returns a deterministic [`ProjectTree`].
///
/// Honors `.gitignore`, nested `.gitignore`, `.git/info/exclude`, and `.ignore` when
/// `options.respect_gitignore` is `true`, even if `root` has no `.git/` repository directory.
/// Never writes any files inside `root`.
///
/// # Errors
///
/// Returns [`ProjectError::NotFound`] if `root` does not exist,
/// [`ProjectError::NotADirectory`] if `root` is not a directory, or
/// [`ProjectError::Io`] / [`ProjectError::Walk`] if root setup fails.
pub fn scan_project(
    root: &Path,
    options: &ProjectScanOptions,
) -> Result<ProjectTree, ProjectError> {
    let canonical_root = ensure_canonical_dir(root)?;
    let (entries, truncated) = collect_walk_entries(
        &canonical_root,
        &canonical_root,
        options,
        options.max_entries,
    )?;
    Ok(build_tree_from_entries(&canonical_root, entries, truncated))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RawEntry {
    Directory,
    File {
        size_bytes: u64,
        extension: Option<String>,
    },
}

impl RawEntry {
    const fn directory() -> Self {
        Self::Directory
    }

    fn from_node(node: &ProjectNode) -> Self {
        match &node.kind {
            ProjectNodeKind::Directory { .. } => Self::Directory,
            ProjectNodeKind::File {
                size_bytes,
                extension,
            } => Self::File {
                size_bytes: *size_bytes,
                extension: extension.clone(),
            },
        }
    }
}

fn collect_walk_entries(
    project_root: &Path,
    scan_root: &Path,
    options: &ProjectScanOptions,
    max_new_entries: usize,
) -> Result<(BTreeMap<PathBuf, RawEntry>, bool), ProjectError> {
    let mut builder = WalkBuilder::new(scan_root);
    builder
        .hidden(!options.include_hidden)
        .git_ignore(options.respect_gitignore)
        .git_exclude(options.respect_gitignore)
        .git_global(options.respect_global_ignore)
        .parents(options.respect_gitignore)
        .require_git(false)
        .follow_links(options.follow_symlinks)
        .max_depth(options.max_depth);

    let skip_git_dir = options.skip_git_dir;
    let extra_excludes = options.extra_excludes.clone();
    let filter_root = project_root.to_path_buf();

    builder.filter_entry(move |entry| {
        if entry.depth() == 0 {
            return true;
        }
        let file_name = entry.file_name();
        if skip_git_dir && file_name == ".git" {
            return false;
        }
        if extra_excludes.is_empty() {
            return true;
        }
        let rel_path = entry.path().strip_prefix(&filter_root).ok();
        !matches_extra_excludes(file_name, rel_path, &extra_excludes)
    });

    let mut collected = BTreeMap::new();
    let mut truncated = false;

    for result in builder.build() {
        let entry = match result {
            Ok(entry) => entry,
            Err(err) => {
                // Skip non-fatal traversal errors such as broken symlinks or symlink loops
                // while still surfacing root-level failure if nothing could be opened.
                if err.depth() == Some(0) {
                    return Err(ProjectError::Walk {
                        path: Some(scan_root.to_path_buf()),
                        message: err.to_string(),
                    });
                }
                continue;
            }
        };

        if entry.depth() == 0 {
            continue;
        }

        let Ok(rel_path_raw) = entry.path().strip_prefix(project_root) else {
            continue;
        };
        let rel_path = rel_path_raw.to_path_buf();
        if !is_safe_relative_path(&rel_path) {
            continue;
        }

        if collected.len() >= max_new_entries {
            truncated = true;
            break;
        }

        let is_dir = entry.file_type().is_some_and(|ft| ft.is_dir());
        if is_dir {
            collected.insert(rel_path, RawEntry::Directory);
        } else {
            let size_bytes = entry.metadata().map_or(0, |meta| meta.len());
            let extension = rel_path
                .extension()
                .and_then(|ext| ext.to_str())
                .filter(|ext| !ext.is_empty())
                .map(str::to_owned);
            collected.insert(
                rel_path,
                RawEntry::File {
                    size_bytes,
                    extension,
                },
            );
        }
    }

    Ok((collected, truncated))
}

fn is_excluded_rel_path(rel_path: &Path, options: &ProjectScanOptions) -> bool {
    let Some(file_name) = rel_path.file_name() else {
        return false;
    };
    if options.skip_git_dir && file_name == ".git" {
        return true;
    }
    matches_extra_excludes(file_name, Some(rel_path), &options.extra_excludes)
}

fn matches_extra_excludes(
    file_name: &std::ffi::OsStr,
    rel_path: Option<&Path>,
    extra_excludes: &[String],
) -> bool {
    let name_str = file_name.to_string_lossy();
    for raw_pattern in extra_excludes {
        let pattern = raw_pattern.trim();
        if pattern.is_empty() {
            continue;
        }
        if let Some(suffix) = pattern.strip_prefix('*') {
            if name_str.ends_with(suffix) {
                return true;
            }
        } else if pattern.contains('/') || pattern.contains('\\') {
            if let Some(rel) = rel_path {
                let normalized_rel = normalize_separators(rel);
                let normalized_pat = pattern.replace('\\', "/");
                let trimmed_pat = normalized_pat.trim_matches('/');
                if normalized_rel == trimmed_pat
                    || normalized_rel
                        .strip_prefix(trimmed_pat)
                        .is_some_and(|rest| rest.starts_with('/'))
                {
                    return true;
                }
            }
        } else if name_str == pattern {
            return true;
        }
    }
    false
}

fn normalize_separators(path: &Path) -> String {
    let mut out = String::new();
    for comp in path.components() {
        if let Component::Normal(seg) = comp {
            if !out.is_empty() {
                out.push('/');
            }
            out.push_str(&seg.to_string_lossy());
        }
    }
    out
}

fn ensure_ancestor_directories(raw_entries: &mut BTreeMap<PathBuf, RawEntry>) {
    let existing_paths: Vec<PathBuf> = raw_entries.keys().cloned().collect();
    for rel_path in existing_paths {
        let mut cursor = rel_path.parent();
        while let Some(parent) = cursor {
            if parent.as_os_str().is_empty() {
                break;
            }
            raw_entries
                .entry(parent.to_path_buf())
                .or_insert(RawEntry::Directory);
            cursor = parent.parent();
        }
    }
}

fn sort_tree_children(nodes: &mut [ProjectNode]) {
    let sort_keys: Vec<(bool, String, String)> = nodes
        .iter()
        .map(|node| {
            let is_file = matches!(node.kind, ProjectNodeKind::File { .. });
            (is_file, node.name.to_lowercase(), node.name.clone())
        })
        .collect();

    for node in nodes {
        if let ProjectNodeKind::Directory { children } = &mut node.kind {
            children.sort_by(|a, b| compare_sort_keys(&sort_keys[a.0], &sort_keys[b.0]));
        }
    }
}

fn build_tree_from_entries(
    canonical_root: &Path,
    mut raw_entries: BTreeMap<PathBuf, RawEntry>,
    truncated: bool,
) -> ProjectTree {
    ensure_ancestor_directories(&mut raw_entries);

    let root_name = project_display_name(canonical_root);
    let mut nodes = Vec::with_capacity(raw_entries.len() + 1);
    nodes.push(ProjectNode {
        id: ProjectNodeId::ROOT,
        parent: None,
        name: root_name,
        rel_path: PathBuf::new(),
        kind: ProjectNodeKind::Directory {
            children: Vec::new(),
        },
    });

    let mut path_index = HashMap::with_capacity(raw_entries.len() + 1);
    path_index.insert(PathBuf::new(), ProjectNodeId::ROOT);

    let mut ordered_entries: Vec<(PathBuf, RawEntry)> = raw_entries.into_iter().collect();
    ordered_entries.sort_by(|(a, _), (b, _)| {
        a.components()
            .count()
            .cmp(&b.components().count())
            .then_with(|| a.cmp(b))
    });

    let mut dir_count = 0usize;
    let mut file_count = 0usize;
    let mut files = Vec::new();

    for (rel_path, entry) in ordered_entries {
        let parent_rel = rel_path.parent().unwrap_or_else(|| Path::new(""));
        let Some(&parent_id) = path_index.get(parent_rel) else {
            continue;
        };

        let id = ProjectNodeId(nodes.len());
        let name = rel_path
            .file_name()
            .map(|seg| seg.to_string_lossy().into_owned())
            .unwrap_or_default();

        let kind = match entry {
            RawEntry::Directory => {
                dir_count += 1;
                ProjectNodeKind::Directory {
                    children: Vec::new(),
                }
            }
            RawEntry::File {
                size_bytes,
                extension,
            } => {
                file_count += 1;
                files.push(ProjectFileEntry {
                    node_id: id,
                    rel_path: rel_path.clone(),
                    extension: extension.clone(),
                    size_bytes,
                });
                ProjectNodeKind::File {
                    size_bytes,
                    extension,
                }
            }
        };

        nodes.push(ProjectNode {
            id,
            parent: Some(parent_id),
            name,
            rel_path: rel_path.clone(),
            kind,
        });
        path_index.insert(rel_path, id);

        if let Some(ProjectNode {
            kind: ProjectNodeKind::Directory { children },
            ..
        }) = nodes.get_mut(parent_id.0)
        {
            children.push(id);
        }
    }

    sort_tree_children(&mut nodes);
    files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));

    ProjectTree {
        nodes,
        files,
        path_index,
        dir_count,
        file_count,
        truncated,
    }
}

fn compare_sort_keys(a: &(bool, String, String), b: &(bool, String, String)) -> Ordering {
    a.0.cmp(&b.0)
        .then_with(|| a.1.cmp(&b.1))
        .then_with(|| a.2.cmp(&b.2))
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests assert scanner invariants"
)]
mod tests {
    use std::path::Path;

    use tempfile::TempDir;

    use super::{ProjectNodeKind, ProjectScanOptions, scan_project};

    #[test]
    fn sorts_directories_before_files_intellij_style() {
        let dir = TempDir::new().expect("temp dir");
        std::fs::create_dir_all(dir.path().join("zeta")).expect("zeta");
        std::fs::create_dir_all(dir.path().join("Alpha")).expect("Alpha");
        std::fs::write(dir.path().join("README.md"), "readme").expect("README");
        std::fs::write(dir.path().join("build.gradle.kts"), "").expect("gradle");
        std::fs::write(dir.path().join("a.txt"), "").expect("a.txt");

        let tree = scan_project(dir.path(), &ProjectScanOptions::default()).expect("scan");
        let child_names: Vec<&str> = tree
            .children(tree.root_id())
            .iter()
            .filter_map(|id| tree.get(*id).map(|n| n.name.as_str()))
            .collect();
        assert_eq!(
            child_names,
            vec!["Alpha", "zeta", "a.txt", "build.gradle.kts", "README.md"]
        );
    }

    #[test]
    fn honors_max_entries_and_sets_truncated_only_when_exceeded() {
        let dir = TempDir::new().expect("temp dir");
        for idx in 0..5 {
            std::fs::write(dir.path().join(format!("file_{idx}.java")), "class X {}")
                .expect("write file");
        }

        let exact_opts = ProjectScanOptions {
            max_entries: 5,
            ..ProjectScanOptions::default()
        };
        let exact_tree = scan_project(dir.path(), &exact_opts).expect("exact scan");
        assert_eq!(exact_tree.file_count(), 5);
        assert!(
            !exact_tree.truncated,
            "exact fit must not mark tree as truncated"
        );

        let capped_opts = ProjectScanOptions {
            max_entries: 3,
            ..ProjectScanOptions::default()
        };
        let capped_tree = scan_project(dir.path(), &capped_opts).expect("capped scan");
        assert_eq!(capped_tree.file_count(), 3);
        assert!(capped_tree.truncated);
    }

    #[test]
    fn skips_git_dir_while_keeping_dotfiles_and_rescanning_subtrees() {
        let dir = TempDir::new().expect("temp dir");
        std::fs::create_dir_all(dir.path().join(".git/objects")).expect(".git");
        std::fs::write(dir.path().join(".git/HEAD"), "ref: refs/heads/main\n").expect("HEAD");
        std::fs::write(dir.path().join(".gitignore"), "*.class\ntarget/\n").expect(".gitignore");
        std::fs::write(dir.path().join(".editorconfig"), "root = true\n").expect(".editorconfig");
        std::fs::create_dir_all(dir.path().join(".github/workflows")).expect(".github");
        std::fs::write(dir.path().join(".github/workflows/ci.yml"), "name: CI\n").expect("ci.yml");
        std::fs::create_dir_all(dir.path().join("src")).expect("src");
        std::fs::write(dir.path().join("src/Main.java"), "class Main {}").expect("Main.java");
        std::fs::write(dir.path().join("src/Main.class"), "bad").expect("Main.class");

        let opts = ProjectScanOptions::default();
        let mut tree = scan_project(dir.path(), &opts).expect("scan");

        assert!(tree.find_by_rel_path(Path::new(".git")).is_none());
        assert!(tree.find_by_rel_path(Path::new(".gitignore")).is_some());
        assert!(tree.find_by_rel_path(Path::new(".editorconfig")).is_some());
        assert!(
            tree.find_by_rel_path(Path::new(".github/workflows/ci.yml"))
                .is_some()
        );
        assert!(tree.find_by_rel_path(Path::new("src/Main.java")).is_some());
        assert!(tree.find_by_rel_path(Path::new("src/Main.class")).is_none());

        // Add a new file in src/ and remove Main.java, then rescan only src/.
        std::fs::write(dir.path().join("src/Helper.java"), "class Helper {}").expect("Helper.java");
        std::fs::remove_file(dir.path().join("src/Main.java")).expect("remove Main.java");
        tree.rescan_subtree(dir.path(), Path::new("src"), &opts)
            .expect("rescan src");

        assert!(tree.find_by_rel_path(Path::new("src/Main.java")).is_none());
        let helper_id = tree
            .find_by_rel_path(Path::new("src/Helper.java"))
            .expect("Helper.java indexed");
        assert!(matches!(
            tree.get(helper_id).map(|n| &n.kind),
            Some(ProjectNodeKind::File {
                extension: Some(ext),
                ..
            }) if ext == "java"
        ));
        assert!(
            tree.files()
                .iter()
                .all(|f| f.rel_path != Path::new("src/Main.java"))
        );
        assert!(
            tree.files()
                .iter()
                .any(|f| f.rel_path == Path::new("src/Helper.java"))
        );
    }
}
