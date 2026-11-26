//! Virtualized project file tree model (`ProjectTreeModel`).
//!
//! The model stores a pre-flattened, depth-first list of nodes (directories before files)
//! plus a cached index of the currently visible rows, so GPUI only materializes the rows
//! inside the viewport (`O(viewport)`) and filtering never touches the filesystem.

use std::ops::Range;
use std::path::{Path, PathBuf};

use kova_core::project::{
    ProjectNodeId as CoreNodeId, ProjectNodeKind as CoreNodeKind, ProjectTree as CoreProjectTree,
};

/// Fixed row height of the Project tree in logical pixels.
pub const PROJECT_ROW_HEIGHT_PX: f32 = 22.0;
/// Horizontal indentation per tree level in logical pixels.
pub const PROJECT_INDENT_PX: f32 = 14.0;

/// Directory names collapsed by default to keep the tree focused on sources.
const COLLAPSED_BY_DEFAULT: [&str; 6] =
    [".git", "target", "build", ".gradle", "out", "node_modules"];

/// Kind of node in the project tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectNodeKind {
    /// A directory that can be expanded or collapsed.
    Directory {
        /// Whether children are currently visible.
        expanded: bool,
    },
    /// A file with a short language/type badge hint (e.g. `"J"`, `"K"`, `"T"`, `"H"`, `"⚙"`).
    File {
        /// Short icon/language hint.
        lang_hint: String,
    },
}

/// Semantic file icon variant rendered by `project_icons` (`KOVA-055`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FileIconKind {
    /// Markdown document (`.md`, `.mdx`).
    Markdown,
    /// Java source file (`.java`).
    Java,
    /// Java test source file (`*Test.java`, `*Tests.java`).
    JavaTest,
    /// Kotlin source or script (`.kt`, `.kts`).
    Kotlin,
    /// YAML config (`.yml`, `.yaml`).
    Yaml,
    /// XML / Maven POM / XSD (`.xml`, `.pom`, `.xsd`).
    Xml,
    /// Groovy / Gradle script (`.groovy`, `.gradle`).
    Groovy,
    /// Rust source (`.rs`).
    Rust,
    /// JSON / JSONC (`.json`, `.jsonc`).
    Json,
    /// TOML file (`.toml`).
    Toml,
    /// Project `kova.toml` configuration.
    KovaConfig,
    /// HTTP request file (`.http`, `.rest`).
    Http,
    /// Shell script (`.sh`, `.bash`, `.zsh`).
    Shell,
    /// Dotfile / git configuration (`.gitignore`, `.editorconfig`, `.env`).
    DotConfig,
    /// Generic fallback file icon.
    #[default]
    Generic,
}

/// Resolves the [`FileIconKind`] for a file basename.
#[must_use]
pub fn file_icon_kind_for(name: &str) -> FileIconKind {
    let lower = name.to_ascii_lowercase();
    if lower == "kova.toml" {
        return FileIconKind::KovaConfig;
    }
    if matches!(
        lower.as_str(),
        ".gitignore" | ".gitattributes" | ".gitmodules" | ".ignore" | ".editorconfig" | ".env"
    ) || lower.starts_with(".env.")
    {
        return FileIconKind::DotConfig;
    }
    let path = Path::new(name);
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    if ext == "java" && (stem.ends_with("test") || stem.ends_with("tests")) {
        return FileIconKind::JavaTest;
    }

    match ext.as_str() {
        "md" | "mdx" | "markdown" => FileIconKind::Markdown,
        "java" => FileIconKind::Java,
        "kt" | "kts" => FileIconKind::Kotlin,
        "yaml" | "yml" => FileIconKind::Yaml,
        "xml" | "pom" | "xsd" => FileIconKind::Xml,
        "groovy" | "gradle" => FileIconKind::Groovy,
        "rs" => FileIconKind::Rust,
        "json" | "jsonc" => FileIconKind::Json,
        "toml" => FileIconKind::Toml,
        "http" | "rest" => FileIconKind::Http,
        "sh" | "bash" | "zsh" => FileIconKind::Shell,
        _ => FileIconKind::Generic,
    }
}

/// Formats `path` replacing the user's `$HOME` prefix with `~`.
#[must_use]
pub fn format_tilde_path(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    format_tilde_path_with_home(path, home.as_deref())
}

/// Formats `path` replacing `home` prefix with `~` when present.
#[must_use]
pub fn format_tilde_path_with_home(path: &Path, home: Option<&Path>) -> String {
    if let Some(home) = home
        && !home.as_os_str().is_empty()
        && let Ok(stripped) = path.strip_prefix(home)
    {
        if stripped.as_os_str().is_empty() {
            return "~".to_owned();
        }
        return format!("~/{}", stripped.display());
    }
    path.display().to_string()
}

/// A flattened node in the project tree snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectNode {
    /// Stable node identifier.
    pub node_id: u64,
    /// Path relative to the project root (empty for the project root row).
    pub path: PathBuf,
    /// Display name.
    pub name: String,
    /// Indentation depth (`0` for root row when present, or root children when omitted).
    pub depth: u16,
    /// Node kind (directory or file).
    pub kind: ProjectNodeKind,
    /// Whether this entry is `kova.toml` (highlighted per Omarchy clean-project rules).
    pub is_kova_config: bool,
    /// Whether this row represents the workspace project root (`depth = 0`).
    pub is_root: bool,
    /// Optional dimmed `~/...` path hint displayed next to the project root name.
    pub root_path_hint: Option<String>,
    /// Whether this node matched `.gitignore` / `.ignore` / `.git/info/exclude`.
    pub is_git_ignored: bool,
    /// Semantic vector icon kind for file nodes.
    pub icon_kind: FileIconKind,
}

impl ProjectNode {
    /// Returns `true` for directory nodes.
    #[must_use]
    pub const fn is_dir(&self) -> bool {
        matches!(self.kind, ProjectNodeKind::Directory { .. })
    }

    /// Returns the short icon glyph rendered before the node name.
    #[must_use]
    pub fn icon(&self) -> &str {
        match &self.kind {
            ProjectNodeKind::Directory { expanded: true } => "▾",
            ProjectNodeKind::Directory { expanded: false } => "▸",
            ProjectNodeKind::File { lang_hint } => lang_hint.as_str(),
        }
    }
}

/// Result of activating (`Enter`) the selected tree row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeActivation {
    /// A directory was expanded or collapsed.
    Toggled,
    /// A file should be opened in the active editor tab group.
    OpenFile(PathBuf),
}

/// Returns the short type badge for a file name (`J`, `T`, `K`, `G`, `Y`, `<>`, `H`, `⚙`, `≡`, `{}`, `·`).
#[must_use]
pub fn lang_hint_for(name: &str) -> &'static str {
    let path = Path::new(name);
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if name.eq_ignore_ascii_case("kova.toml") {
        "⚙"
    } else if matches!(ext.as_str(), "java" | "kt" | "groovy")
        && (stem.ends_with("test") || stem.ends_with("tests"))
    {
        "T"
    } else {
        match ext.as_str() {
            "java" => "J",
            "kt" | "kts" => "K",
            "groovy" | "gradle" => "G",
            "yaml" | "yml" => "Y",
            "xml" | "pom" | "xsd" => "<>",
            "http" => "H",
            "toml" => "≡",
            "json" => "{}",
            _ => "·",
        }
    }
}

/// Headless model for the virtualized Project dock tree (`O(viewport)`).
#[derive(Debug, Clone)]
pub struct ProjectTreeModel {
    nodes: Vec<ProjectNode>,
    /// Lower-cased names, precomputed so filtering 10k+ nodes stays within a few milliseconds.
    lower_names: Vec<String>,
    /// Indices into `nodes` of the currently visible rows.
    visible: Vec<usize>,
    filter_query: String,
    selected_index: usize,
    show_git_ignored: bool,
}

impl Default for ProjectTreeModel {
    fn default() -> Self {
        Self {
            nodes: Vec::new(),
            lower_names: Vec::new(),
            visible: Vec::new(),
            filter_query: String::new(),
            selected_index: 0,
            show_git_ignored: true,
        }
    }
}

impl ProjectTreeModel {
    /// Creates a [`ProjectTreeModel`] from an ordered pre-flattened snapshot.
    ///
    /// Automatically filters out `.idea` entries to enforce Omarchy cleanliness.
    #[must_use]
    pub fn from_nodes(nodes: Vec<ProjectNode>) -> Self {
        let nodes: Vec<ProjectNode> = nodes
            .into_iter()
            .filter(|node| !is_hidden_idea_path(&node.path, &node.name))
            .collect();
        let lower_names = nodes.iter().map(|n| n.name.to_lowercase()).collect();
        let mut model = Self {
            nodes,
            lower_names,
            visible: Vec::new(),
            filter_query: String::new(),
            selected_index: 0,
            show_git_ignored: true,
        };
        model.rebuild_visible();
        model
    }

    /// Flattens a `.gitignore`-aware [`kova_core::project::ProjectTree`] snapshot (`KOVA-020`)
    /// without a synthetic project root row.
    ///
    /// Directories up to `expand_depth` are expanded, except build/VCS output directories
    /// (`.git`, `target`, `build`, `.gradle`, …) and `git-ignored` directories which always start collapsed.
    #[must_use]
    pub fn from_project_tree(tree: &CoreProjectTree, expand_depth: u16) -> Self {
        Self::flatten_core_tree(tree, None, expand_depth, false)
    }

    /// Flattens a [`kova_core::project::ProjectTree`] snapshot starting with a project root row (`depth = 0`, `KOVA-055`).
    ///
    /// The root row starts expanded with a dimmed `~/...` hint derived from `project_root`.
    /// Top-level non-ignored directories up to `expand_depth` start expanded.
    #[must_use]
    pub fn from_project_tree_with_root(
        tree: &CoreProjectTree,
        project_root: Option<&Path>,
        expand_depth: u16,
    ) -> Self {
        Self::flatten_core_tree(tree, project_root, expand_depth, true)
    }

    fn flatten_core_tree(
        tree: &CoreProjectTree,
        project_root: Option<&Path>,
        expand_depth: u16,
        include_root: bool,
    ) -> Self {
        let mut nodes =
            Vec::with_capacity(tree.file_count() + tree.dir_count() + usize::from(include_root));
        let child_base_depth = if include_root {
            if let Some(root) = tree.get(tree.root_id()) {
                nodes.push(ProjectNode {
                    node_id: root.id.index() as u64,
                    path: PathBuf::new(),
                    name: root.name.clone(),
                    depth: 0,
                    kind: ProjectNodeKind::Directory { expanded: true },
                    is_kova_config: false,
                    is_root: true,
                    root_path_hint: project_root.map(format_tilde_path),
                    is_git_ignored: false,
                    icon_kind: FileIconKind::Generic,
                });
            }
            1_u16
        } else {
            0_u16
        };

        // Depth-first traversal with an explicit stack (children pushed in reverse to keep order).
        let mut stack: Vec<(CoreNodeId, u16)> = tree
            .children(tree.root_id())
            .iter()
            .rev()
            .map(|id| (*id, child_base_depth))
            .collect();

        while let Some((id, depth)) = stack.pop() {
            let Some(node) = tree.get(id) else {
                continue;
            };
            let rel_depth = depth.saturating_sub(child_base_depth);
            let kind = match &node.kind {
                CoreNodeKind::Directory { children } => {
                    for child in children.iter().rev() {
                        stack.push((*child, depth.saturating_add(1)));
                    }
                    let collapsed =
                        node.is_git_ignored || COLLAPSED_BY_DEFAULT.contains(&node.name.as_str());
                    ProjectNodeKind::Directory {
                        expanded: rel_depth < expand_depth && !collapsed,
                    }
                }
                CoreNodeKind::File { .. } => ProjectNodeKind::File {
                    lang_hint: lang_hint_for(&node.name).to_owned(),
                },
            };
            nodes.push(ProjectNode {
                node_id: id.index() as u64,
                path: node.rel_path.clone(),
                is_kova_config: node.name == "kova.toml" && rel_depth == 0,
                icon_kind: file_icon_kind_for(&node.name),
                name: node.name.clone(),
                depth,
                kind,
                is_root: false,
                root_path_hint: None,
                is_git_ignored: node.is_git_ignored,
            });
        }
        Self::from_nodes(nodes)
    }

    /// Returns the total number of nodes in the snapshot (visible or not).
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Returns whether `git-ignored` entries are currently shown in the tree.
    #[must_use]
    pub const fn show_git_ignored(&self) -> bool {
        self.show_git_ignored
    }

    /// Shows or hides `git-ignored` nodes in the visible tree projection.
    pub fn set_show_git_ignored(&mut self, show: bool) {
        if self.show_git_ignored == show {
            return;
        }
        let selected_id = self.selected_node().map(|n| n.node_id);
        self.show_git_ignored = show;
        self.rebuild_visible();
        if let Some(id) = selected_id
            && let Some(row) = self.visible_row_by_id(id)
        {
            self.selected_index = row;
        }
    }

    /// Project-relative paths of non-ignored file nodes (used by *Search Everywhere*,
    /// *Find in Files*, *Go to Definition*, completion, and *Test Runner*).
    pub fn file_paths(&self) -> impl Iterator<Item = &Path> + '_ {
        self.nodes
            .iter()
            .filter(|n| !n.is_dir() && !n.is_git_ignored)
            .map(|n| n.path.as_path())
    }

    /// Returns the current keyboard filter query.
    #[must_use]
    pub fn filter_query(&self) -> &str {
        &self.filter_query
    }

    /// Updates the keyboard filter query and resets selection to the first match.
    pub fn set_filter(&mut self, query: impl Into<String>) {
        self.filter_query = query.into();
        self.selected_index = 0;
        self.rebuild_visible();
    }

    /// Appends `ch` to the *type-to-filter* query.
    pub fn push_filter_char(&mut self, ch: char) {
        let mut query = std::mem::take(&mut self.filter_query);
        query.push(ch);
        self.set_filter(query);
    }

    /// Removes the last character of the filter query. Returns `false` if it was already empty.
    pub fn pop_filter_char(&mut self) -> bool {
        let mut query = std::mem::take(&mut self.filter_query);
        let popped = query.pop().is_some();
        self.set_filter(query);
        popped
    }

    /// Toggles expand/collapse state for the directory with `node_id`.
    pub fn toggle_expand(&mut self, node_id: u64) -> bool {
        let Some(node) = self.nodes.iter_mut().find(|n| n.node_id == node_id) else {
            return false;
        };
        let ProjectNodeKind::Directory { expanded } = &mut node.kind else {
            return false;
        };
        *expanded = !*expanded;
        self.rebuild_visible();
        true
    }

    fn set_expanded_at(&mut self, node_index: usize, value: bool) -> bool {
        let Some(node) = self.nodes.get_mut(node_index) else {
            return false;
        };
        match &mut node.kind {
            ProjectNodeKind::Directory { expanded } if *expanded != value => {
                *expanded = value;
                self.rebuild_visible();
                true
            }
            _ => false,
        }
    }

    fn rebuild_visible(&mut self) {
        self.visible.clear();
        let query = self.filter_query.trim().to_lowercase();
        if query.is_empty() {
            let mut collapsed_depth: Option<u16> = None;
            for (idx, node) in self.nodes.iter().enumerate() {
                if let Some(min_depth) = collapsed_depth {
                    if node.depth > min_depth {
                        continue;
                    }
                    collapsed_depth = None;
                }
                if !self.show_git_ignored && !node.is_root && node.is_git_ignored {
                    if node.is_dir() {
                        collapsed_depth = Some(node.depth);
                    }
                    continue;
                }
                self.visible.push(idx);
                if let ProjectNodeKind::Directory { expanded: false } = node.kind {
                    collapsed_depth = Some(node.depth);
                }
            }
        } else {
            self.visible.extend(
                self.lower_names
                    .iter()
                    .enumerate()
                    .filter(|(idx, name)| {
                        let node = &self.nodes[*idx];
                        !node.is_root
                            && (self.show_git_ignored || !node.is_git_ignored)
                            && name.contains(query.as_str())
                    })
                    .map(|(idx, _)| idx),
            );
        }
        let max = self.visible.len().saturating_sub(1);
        self.selected_index = self.selected_index.min(max);
    }

    fn visible_row_by_id(&self, node_id: u64) -> Option<usize> {
        self.visible
            .iter()
            .position(|&idx| self.nodes.get(idx).is_some_and(|n| n.node_id == node_id))
    }

    /// Expands every directory in the tree while preserving selection when visible.
    pub fn expand_all(&mut self) {
        let selected_id = self.selected_node().map(|n| n.node_id);
        for node in &mut self.nodes {
            if let ProjectNodeKind::Directory { expanded } = &mut node.kind {
                *expanded = true;
            }
        }
        self.rebuild_visible();
        if let Some(id) = selected_id
            && let Some(row) = self.visible_row_by_id(id)
        {
            self.selected_index = row;
        }
    }

    /// Collapses all non-root directories (`!is_root`), leaving the project root expanded.
    ///
    /// If the selected node was inside a collapsed subtree, selection moves to its nearest
    /// visible ancestor directory.
    pub fn collapse_all(&mut self) {
        let selected_path = self.selected_node().map(|n| n.path.clone());
        let has_root = self.nodes.first().is_some_and(|n| n.is_root);
        for node in &mut self.nodes {
            if let ProjectNodeKind::Directory { expanded } = &mut node.kind {
                *expanded = node.is_root;
            }
        }
        self.rebuild_visible();
        if let Some(path) = selected_path {
            let mut cursor = Some(path.as_path());
            while let Some(candidate) = cursor {
                if let Some(row) = (0..self.visible.len()).find(|row| {
                    self.visible_row(*row)
                        .is_some_and(|n| n.path.as_path() == candidate)
                }) {
                    self.selected_index = row;
                    return;
                }
                if candidate.as_os_str().is_empty() {
                    break;
                }
                cursor = candidate.parent();
            }
            if has_root && !self.visible.is_empty() {
                self.selected_index = 0;
            }
        }
    }

    /// Returns the number of currently visible rows.
    #[must_use]
    pub fn visible_len(&self) -> usize {
        self.visible.len()
    }

    /// Returns the visible row at `index`, if any.
    #[must_use]
    pub fn visible_row(&self, index: usize) -> Option<&ProjectNode> {
        self.visible.get(index).and_then(|idx| self.nodes.get(*idx))
    }

    /// Returns the visible rows inside `range` (the GPUI viewport) — `O(viewport)`.
    pub fn visible_range(
        &self,
        range: Range<usize>,
    ) -> impl Iterator<Item = (usize, &ProjectNode)> {
        let start = range.start.min(self.visible.len());
        let end = range.end.min(self.visible.len());
        self.visible
            .get(start..end)
            .unwrap_or_default()
            .iter()
            .enumerate()
            .filter_map(move |(offset, idx)| self.nodes.get(*idx).map(|n| (start + offset, n)))
    }

    /// Returns all currently visible rows (allocates; prefer [`Self::visible_range`] when rendering).
    #[must_use]
    pub fn visible_rows(&self) -> Vec<&ProjectNode> {
        self.visible_range(0..self.visible.len())
            .map(|(_, n)| n)
            .collect()
    }

    /// Returns the currently selected row index within the visible rows.
    #[must_use]
    pub const fn selected_index(&self) -> usize {
        self.selected_index
    }

    /// Selects the visible row at `index` (clamped).
    pub fn select_index(&mut self, index: usize) {
        self.selected_index = index.min(self.visible.len().saturating_sub(1));
    }

    /// Moves selection up by one row.
    pub fn move_selection_up(&mut self) {
        self.selected_index = self.selected_index.saturating_sub(1);
    }

    /// Moves selection down by one row within the visible rows.
    pub fn move_selection_down(&mut self) {
        if self.selected_index + 1 < self.visible.len() {
            self.selected_index += 1;
        }
    }

    /// Returns the currently selected [`ProjectNode`], if any.
    #[must_use]
    pub fn selected_node(&self) -> Option<&ProjectNode> {
        self.visible_row(self.selected_index)
    }

    /// `→`: expands the selected directory, or moves to its first child when already expanded.
    pub fn expand_selected(&mut self) {
        let Some(&node_idx) = self.visible.get(self.selected_index) else {
            return;
        };
        if !self.set_expanded_at(node_idx, true)
            && self.nodes.get(node_idx).is_some_and(ProjectNode::is_dir)
        {
            self.move_selection_down();
        }
    }

    /// `←`: collapses the selected directory, or jumps to the parent directory row.
    pub fn collapse_selected(&mut self) {
        let Some(&node_idx) = self.visible.get(self.selected_index) else {
            return;
        };
        if self.set_expanded_at(node_idx, false) {
            return;
        }
        let Some(depth) = self.nodes.get(node_idx).map(|n| n.depth) else {
            return;
        };
        if depth == 0 {
            return;
        }
        // Walk back through visible rows to the nearest shallower directory (the parent).
        let parent_row = (0..self.selected_index).rev().find(|row| {
            self.visible_row(*row)
                .is_some_and(|n| n.depth < depth && n.is_dir())
        });
        if let Some(row) = parent_row {
            self.selected_index = row;
        }
    }

    /// `Enter`: toggles the selected directory or requests opening the selected file.
    pub fn activate_selected(&mut self) -> Option<TreeActivation> {
        let node = self.selected_node()?;
        match node.kind {
            ProjectNodeKind::Directory { .. } => {
                let id = node.node_id;
                self.toggle_expand(id);
                Some(TreeActivation::Toggled)
            }
            ProjectNodeKind::File { .. } => Some(TreeActivation::OpenFile(node.path.clone())),
        }
    }

    /// Expands all collapsed ancestors of `path`, clears an incompatible filter if needed,
    /// and selects the row showing `path`. Returns `true` when found and visible.
    pub fn reveal_path(&mut self, path: &Path) -> bool {
        let Some(target_idx) = self.nodes.iter().position(|n| n.path.as_path() == path) else {
            return false;
        };
        if !self.show_git_ignored && self.is_or_has_ignored_ancestor(path, target_idx) {
            return false;
        }
        let query = self.filter_query.trim().to_lowercase();
        if !query.is_empty()
            && (self.nodes[target_idx].is_root || !self.lower_names[target_idx].contains(&query))
        {
            self.filter_query.clear();
        }
        for node in &mut self.nodes {
            let is_ancestor = if node.is_root {
                !path.as_os_str().is_empty()
            } else {
                node.is_dir() && path.starts_with(&node.path) && node.path.as_path() != path
            };
            if is_ancestor && let ProjectNodeKind::Directory { expanded } = &mut node.kind {
                *expanded = true;
            }
        }
        self.rebuild_visible();
        let row = (0..self.visible.len()).find(|row| {
            self.visible_row(*row)
                .is_some_and(|node| node.path.as_path() == path)
        });
        if let Some(row) = row {
            self.selected_index = row;
            true
        } else {
            false
        }
    }

    fn is_or_has_ignored_ancestor(&self, path: &Path, target_idx: usize) -> bool {
        if self.nodes[target_idx].is_git_ignored {
            return true;
        }
        self.nodes.iter().any(|n| {
            !n.is_root
                && n.is_dir()
                && n.is_git_ignored
                && path.starts_with(&n.path)
                && n.path.as_path() != path
        })
    }

    /// Computes the project-relative base directory for creating a new file or folder
    /// from the currently selected row (directory -> itself, file -> parent, root/none -> `""`).
    #[must_use]
    pub fn target_dir_for_creation(&self) -> PathBuf {
        match self.selected_node() {
            None => PathBuf::new(),
            Some(node) if node.is_root => PathBuf::new(),
            Some(node) if node.is_dir() => node.path.clone(),
            Some(node) => node
                .path
                .parent()
                .map_or_else(PathBuf::new, Path::to_path_buf),
        }
    }

    /// Inserts a newly created file or directory (plus any missing intermediate directories)
    /// into the flattened tree in sorted order (`directories` before `files`, case-insensitive),
    /// expands its ancestors, and reveals/selects the created entry.
    pub fn insert_created_entry(
        &mut self,
        rel_path: &Path,
        is_dir: bool,
        is_git_ignored: bool,
    ) -> bool {
        if rel_path.as_os_str().is_empty() || is_hidden_idea_path(rel_path, "") {
            return false;
        }
        let mut chain = Vec::new();
        let mut acc = PathBuf::new();
        for comp in rel_path.components() {
            let std::path::Component::Normal(seg) = comp else {
                return false;
            };
            acc.push(seg);
            chain.push(acc.clone());
        }
        if chain.is_empty() {
            return false;
        }

        let last_idx = chain.len() - 1;
        for (idx, path) in chain.into_iter().enumerate() {
            let entry_is_dir = if idx == last_idx { is_dir } else { true };
            self.insert_single_node(path, entry_is_dir, is_git_ignored);
        }
        self.reveal_path(rel_path)
    }

    fn insert_single_node(&mut self, rel_path: PathBuf, is_dir: bool, is_git_ignored: bool) {
        if self.nodes.iter().any(|n| n.path == rel_path) {
            return;
        }
        let name = rel_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if is_hidden_idea_path(&rel_path, &name) {
            return;
        }
        let has_root = self.nodes.first().is_some_and(|n| n.is_root);
        let parent_rel = rel_path.parent().unwrap_or_else(|| Path::new(""));
        let (scan_start, child_depth, subtree_stop_depth) = if parent_rel.as_os_str().is_empty() {
            if has_root {
                (1_usize, 1_u16, Some(0_u16))
            } else {
                (0_usize, 0_u16, None)
            }
        } else {
            let Some(parent_idx) = self.nodes.iter().position(|n| n.path == parent_rel) else {
                return;
            };
            let parent_depth = self.nodes[parent_idx].depth;
            (
                parent_idx + 1,
                parent_depth.saturating_add(1),
                Some(parent_depth),
            )
        };

        let new_is_file = !is_dir;
        let new_lower = name.to_lowercase();
        let mut insert_at = self.nodes.len();
        let mut i = scan_start;
        while i < self.nodes.len() {
            let node = &self.nodes[i];
            if let Some(stop_depth) = subtree_stop_depth
                && node.depth <= stop_depth
            {
                insert_at = i;
                break;
            }
            if node.depth == child_depth {
                let node_key = (
                    !node.is_dir(),
                    self.lower_names[i].as_str(),
                    node.name.as_str(),
                );
                let new_key = (new_is_file, new_lower.as_str(), name.as_str());
                if node_key > new_key {
                    insert_at = i;
                    break;
                }
            }
            i += 1;
        }

        let next_id = self
            .nodes
            .iter()
            .map(|n| n.node_id)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let top_level_depth = u16::from(has_root);
        let kind = if is_dir {
            ProjectNodeKind::Directory { expanded: true }
        } else {
            ProjectNodeKind::File {
                lang_hint: lang_hint_for(&name).to_owned(),
            }
        };
        let node = ProjectNode {
            node_id: next_id,
            is_kova_config: name == "kova.toml" && child_depth == top_level_depth,
            icon_kind: file_icon_kind_for(&name),
            path: rel_path,
            name,
            depth: child_depth,
            kind,
            is_root: false,
            root_path_hint: None,
            is_git_ignored,
        };
        self.nodes.insert(insert_at, node);
        self.lower_names.insert(insert_at, new_lower);
    }
}

fn is_hidden_idea_path(path: &Path, name: &str) -> bool {
    name == ".idea"
        || path
            .components()
            .any(|c| c.as_os_str().to_str() == Some(".idea"))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    fn file(id: u64, path: &str, depth: u16) -> ProjectNode {
        let name = Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(path)
            .to_owned();
        ProjectNode {
            node_id: id,
            path: PathBuf::from(path),
            kind: ProjectNodeKind::File {
                lang_hint: lang_hint_for(&name).to_owned(),
            },
            is_kova_config: name == "kova.toml",
            icon_kind: file_icon_kind_for(&name),
            name,
            depth,
            is_root: false,
            root_path_hint: None,
            is_git_ignored: false,
        }
    }

    fn dir(id: u64, path: &str, depth: u16, expanded: bool) -> ProjectNode {
        ProjectNode {
            node_id: id,
            path: PathBuf::from(path),
            name: Path::new(path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(path)
                .to_owned(),
            depth,
            kind: ProjectNodeKind::Directory { expanded },
            is_kova_config: false,
            is_root: false,
            root_path_hint: None,
            is_git_ignored: false,
            icon_kind: FileIconKind::Generic,
        }
    }

    fn root_node(name: &str, hint: &str) -> ProjectNode {
        ProjectNode {
            node_id: 0,
            path: PathBuf::new(),
            name: name.to_owned(),
            depth: 0,
            kind: ProjectNodeKind::Directory { expanded: true },
            is_kova_config: false,
            is_root: true,
            root_path_hint: Some(hint.to_owned()),
            is_git_ignored: false,
            icon_kind: FileIconKind::Generic,
        }
    }

    fn sample_nodes() -> Vec<ProjectNode> {
        vec![
            dir(1, "src", 0, true),
            file(2, "src/OrderApp.java", 1),
            dir(3, ".idea", 0, true),
            file(5, ".idea/workspace.xml", 1),
            file(4, "kova.toml", 0),
        ]
    }

    #[test]
    fn hides_idea_and_supports_expand_collapse_and_filter() {
        let mut tree = ProjectTreeModel::from_nodes(sample_nodes());
        // .idea (and its children) are stripped
        assert_eq!(tree.visible_len(), 3);
        assert!(
            tree.visible_rows()
                .iter()
                .all(|n| !n.path.starts_with(".idea"))
        );

        // Collapse `src`
        assert!(tree.toggle_expand(1));
        assert_eq!(tree.visible_len(), 2);

        // Filter finds OrderApp.java even when `src` is collapsed
        tree.set_filter("order");
        let filtered = tree.visible_rows();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "OrderApp.java");

        tree.set_filter("");
        assert!(tree.toggle_expand(1));
        tree.move_selection_down();
        assert_eq!(
            tree.selected_node().map(|n| n.name.as_str()),
            Some("OrderApp.java")
        );
        assert_eq!(
            tree.activate_selected(),
            Some(TreeActivation::OpenFile(PathBuf::from("src/OrderApp.java")))
        );
        assert!(
            tree.visible_row(2)
                .is_some_and(|n| n.is_kova_config && n.icon() == "⚙")
        );
    }

    #[test]
    fn keyboard_navigation_expands_collapses_and_jumps_to_parent() {
        let mut tree = ProjectTreeModel::from_nodes(vec![
            dir(1, "src", 0, false),
            dir(2, "src/main", 1, false),
            file(3, "src/main/App.java", 2),
            file(4, "build.gradle.kts", 0),
        ]);
        assert_eq!(tree.visible_len(), 2);
        tree.expand_selected(); // expand src
        assert_eq!(tree.visible_len(), 3);
        tree.expand_selected(); // already expanded -> move to first child
        assert_eq!(tree.selected_node().map(|n| n.node_id), Some(2));
        tree.expand_selected(); // expand src/main
        tree.move_selection_down();
        assert_eq!(
            tree.selected_node().map(|n| n.name.as_str()),
            Some("App.java")
        );
        tree.collapse_selected(); // file -> jump to parent dir
        assert_eq!(tree.selected_node().map(|n| n.node_id), Some(2));
        tree.collapse_selected(); // collapse src/main
        assert_eq!(tree.visible_len(), 3);
        tree.move_selection_up();
        assert_eq!(tree.activate_selected(), Some(TreeActivation::Toggled));
        assert_eq!(tree.visible_len(), 2);
        assert!(tree.reveal_path(Path::new("build.gradle.kts")));
        assert_eq!(tree.selected_index(), 1);
    }

    #[test]
    fn type_to_filter_pushes_and_pops_characters() {
        let mut tree = ProjectTreeModel::from_nodes(sample_nodes());
        tree.push_filter_char('K');
        tree.push_filter_char('o');
        assert_eq!(tree.filter_query(), "Ko");
        assert_eq!(tree.visible_len(), 1);
        assert!(tree.pop_filter_char());
        assert!(tree.pop_filter_char());
        assert!(!tree.pop_filter_char());
        assert_eq!(tree.visible_len(), 3);
    }

    #[test]
    fn file_badges_follow_jvm_conventions() {
        assert_eq!(lang_hint_for("OrderApp.java"), "J");
        assert_eq!(lang_hint_for("OrderAppTest.java"), "T");
        assert_eq!(lang_hint_for("build.gradle.kts"), "K");
        assert_eq!(lang_hint_for("build.gradle"), "G");
        assert_eq!(lang_hint_for("Script.groovy"), "G");
        assert_eq!(lang_hint_for("OrderTest.groovy"), "T");
        assert_eq!(lang_hint_for("OrderTests.groovy"), "T");
        assert_eq!(lang_hint_for("application.yml"), "Y");
        assert_eq!(lang_hint_for("values.YAML"), "Y");
        assert_eq!(lang_hint_for("pom.xml"), "<>");
        assert_eq!(lang_hint_for("artifact.pom"), "<>");
        assert_eq!(lang_hint_for("schema.xsd"), "<>");
        assert_eq!(lang_hint_for("orders.http"), "H");
        assert_eq!(lang_hint_for("kova.toml"), "⚙");
    }

    #[test]
    fn filters_ten_thousand_nodes_quickly_and_renders_viewport_only() {
        let mut nodes = Vec::with_capacity(10_100);
        let mut id = 0_u64;
        for d in 0..100 {
            id += 1;
            nodes.push(dir(id, &format!("pkg{d}"), 0, true));
            for f in 0..100 {
                id += 1;
                nodes.push(file(id, &format!("pkg{d}/Service{d}x{f}.java"), 1));
            }
        }
        let mut tree = ProjectTreeModel::from_nodes(nodes);
        assert_eq!(tree.visible_len(), 10_100);

        let mut best = Duration::MAX;
        for _ in 0..5 {
            let start = Instant::now();
            tree.set_filter("service42x");
            best = best.min(start.elapsed());
        }
        assert_eq!(tree.visible_len(), 100);
        assert!(best < Duration::from_millis(5), "filter took {best:?}");

        tree.set_filter("");
        // A 30-row viewport materializes exactly 30 rows regardless of tree size.
        assert_eq!(tree.visible_range(500..530).count(), 30);
        assert_eq!(tree.visible_range(10_090..20_000).count(), 10);
    }

    #[test]
    fn adapts_kova_core_project_tree_snapshot() {
        let root = std::env::temp_dir().join(format!("kova-ui-tree-{}", std::process::id()));
        drop(std::fs::remove_dir_all(&root));
        std::fs::create_dir_all(root.join("src/main/java")).unwrap();
        std::fs::create_dir_all(root.join("build/classes")).unwrap();
        std::fs::write(
            root.join("src/main/java/OrderApp.java"),
            "class OrderApp {}",
        )
        .unwrap();
        std::fs::write(root.join("build/classes/OrderApp.class"), "").unwrap();
        std::fs::write(root.join("kova.toml"), "[project]\n").unwrap();

        let core = kova_core::project::scan_project(
            &root,
            &kova_core::project::ProjectScanOptions::default(),
        )
        .unwrap();
        let tree = ProjectTreeModel::from_project_tree(&core, 8);
        let names: Vec<_> = tree.visible_rows().iter().map(|n| n.name.clone()).collect();
        // Directories first, `build/` collapsed by default, kova.toml highlighted.
        assert_eq!(names.first().map(String::as_str), Some("build"));
        assert!(!names.iter().any(|n| n == "classes"));
        assert!(names.iter().any(|n| n == "OrderApp.java"));
        assert!(tree.visible_rows().iter().any(|n| n.is_kova_config));
        drop(std::fs::remove_dir_all(&root));
    }

    #[test]
    fn from_project_tree_with_root_formats_tilde_and_collapses_git_ignored() {
        assert_eq!(
            format_tilde_path_with_home(
                Path::new("/Users/dev/kitopi/api"),
                Some(Path::new("/Users/dev")),
            ),
            "~/kitopi/api"
        );
        assert_eq!(
            format_tilde_path_with_home(Path::new("/Users/dev"), Some(Path::new("/Users/dev"))),
            "~"
        );
        assert_eq!(
            format_tilde_path_with_home(Path::new("/opt/project"), Some(Path::new("/Users/dev"))),
            "/opt/project"
        );

        let root = std::env::temp_dir().join(format!("kova-ui-root-tree-{}", std::process::id()));
        drop(std::fs::remove_dir_all(&root));
        std::fs::create_dir_all(root.join("src/main")).unwrap();
        std::fs::create_dir_all(root.join(".claude/.cc-writes")).unwrap();
        std::fs::write(root.join(".gitignore"), ".claude/.cc-writes/\n").unwrap();
        std::fs::write(root.join("src/main/App.java"), "class App {}").unwrap();
        std::fs::write(root.join(".claude/.cc-writes/draft.md"), "# draft").unwrap();
        std::fs::write(root.join("README.md"), "# hi").unwrap();

        let opts = kova_core::project::ProjectScanOptions {
            include_git_ignored: true,
            ..Default::default()
        };
        let core = kova_core::project::scan_project(&root, &opts).unwrap();
        let mut tree = ProjectTreeModel::from_project_tree_with_root(&core, Some(&root), 1);

        let first = tree.visible_row(0).unwrap();
        assert!(first.is_root);
        assert_eq!(first.depth, 0);
        assert!(first.root_path_hint.is_some());

        // `.cc-writes` is visible as a collapsed git-ignored dir, but `draft.md` inside it is hidden
        // initially and excluded from `file_paths()`.
        let cc_writes = tree
            .visible_rows()
            .into_iter()
            .find(|n| n.name == ".cc-writes")
            .unwrap();
        assert!(cc_writes.is_git_ignored);
        assert_eq!(
            cc_writes.kind,
            ProjectNodeKind::Directory { expanded: false }
        );
        assert!(!tree.visible_rows().iter().any(|n| n.name == "draft.md"));
        assert!(
            tree.file_paths()
                .all(|p| p != Path::new(".claude/.cc-writes/draft.md"))
        );
        assert!(tree.file_paths().any(|p| p == Path::new("README.md")));

        // Toggling `set_show_git_ignored(false)` hides `.cc-writes`.
        tree.set_show_git_ignored(false);
        assert!(!tree.visible_rows().iter().any(|n| n.name == ".cc-writes"));
        tree.set_show_git_ignored(true);
        assert!(tree.visible_rows().iter().any(|n| n.name == ".cc-writes"));

        drop(std::fs::remove_dir_all(&root));
    }

    #[test]
    fn reveal_path_expands_collapsed_ancestors_and_clears_incompatible_filter() {
        let mut tree = ProjectTreeModel::from_nodes(vec![
            root_node("api", "~/api"),
            dir(1, "src", 1, false),
            dir(2, "src/main", 2, false),
            file(3, "src/main/App.java", 3),
            file(4, "README.md", 1),
        ]);
        assert_eq!(tree.visible_len(), 3); // root, src, README.md

        tree.set_filter("readme");
        assert_eq!(tree.visible_len(), 1);

        assert!(tree.reveal_path(Path::new("src/main/App.java")));
        assert_eq!(tree.filter_query(), "");
        assert_eq!(
            tree.selected_node().map(|n| n.path.as_path()),
            Some(Path::new("src/main/App.java"))
        );
        assert_eq!(tree.visible_len(), 5);
        assert!(!tree.reveal_path(Path::new("does/not/exist.java")));
    }

    #[test]
    fn expand_all_collapse_all_and_creation_helpers_work_consistently() {
        let mut tree = ProjectTreeModel::from_nodes(vec![
            root_node("api", "~/api"),
            dir(1, "src", 1, false),
            dir(2, "src/main", 2, false),
            file(3, "src/main/App.java", 3),
            file(4, "README.md", 1),
        ]);

        tree.expand_all();
        assert_eq!(tree.visible_len(), 5);
        assert!(tree.reveal_path(Path::new("src/main/App.java")));
        assert_eq!(tree.target_dir_for_creation(), Path::new("src/main"));

        tree.collapse_all();
        // Root remains expanded; `src` is collapsed and becomes selected ancestor.
        assert_eq!(tree.visible_len(), 3);
        assert_eq!(
            tree.selected_node().map(|n| n.path.as_path()),
            Some(Path::new("src"))
        );
        assert_eq!(tree.target_dir_for_creation(), Path::new("src"));

        // Selecting root targets project root `""`.
        tree.select_index(0);
        assert_eq!(tree.target_dir_for_creation(), Path::new(""));

        // Creating a nested file inserts intermediate directory sorted before files and selects it.
        assert!(tree.insert_created_entry(Path::new("domain/Order.java"), false, false));
        assert_eq!(
            tree.selected_node().map(|n| n.path.as_path()),
            Some(Path::new("domain/Order.java"))
        );
        let names: Vec<_> = tree
            .visible_rows()
            .iter()
            .map(|n| n.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["api", "domain", "Order.java", "src", "README.md"]
        );
    }

    #[test]
    fn file_icon_kinds_cover_required_languages() {
        assert_eq!(file_icon_kind_for("README.md"), FileIconKind::Markdown);
        assert_eq!(file_icon_kind_for("Order.java"), FileIconKind::Java);
        assert_eq!(file_icon_kind_for("OrderTest.java"), FileIconKind::JavaTest);
        assert_eq!(file_icon_kind_for("build.gradle.kts"), FileIconKind::Kotlin);
        assert_eq!(file_icon_kind_for("application.yml"), FileIconKind::Yaml);
        assert_eq!(file_icon_kind_for("pom.xml"), FileIconKind::Xml);
        assert_eq!(file_icon_kind_for("build.gradle"), FileIconKind::Groovy);
        assert_eq!(file_icon_kind_for("main.rs"), FileIconKind::Rust);
        assert_eq!(file_icon_kind_for("package.json"), FileIconKind::Json);
        assert_eq!(file_icon_kind_for("Cargo.toml"), FileIconKind::Toml);
        assert_eq!(file_icon_kind_for("kova.toml"), FileIconKind::KovaConfig);
        assert_eq!(file_icon_kind_for("orders.http"), FileIconKind::Http);
        assert_eq!(file_icon_kind_for("verify.sh"), FileIconKind::Shell);
        assert_eq!(file_icon_kind_for(".gitignore"), FileIconKind::DotConfig);
        assert_eq!(file_icon_kind_for("notes.txt"), FileIconKind::Generic);
    }
}
