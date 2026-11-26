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

/// A flattened node in the project tree snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectNode {
    /// Stable node identifier.
    pub node_id: u64,
    /// Path relative to the project root.
    pub path: PathBuf,
    /// Display name.
    pub name: String,
    /// Indentation depth (`0` for root children).
    pub depth: u16,
    /// Node kind (directory or file).
    pub kind: ProjectNodeKind,
    /// Whether this entry is `kova.toml` (highlighted per Omarchy clean-project rules).
    pub is_kova_config: bool,
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

/// Returns the short type badge for a file name (`J`, `T`, `K`, `H`, `⚙`, `{}`, `·`).
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
    } else if (ext == "java" || ext == "kt") && (stem.ends_with("test") || stem.ends_with("tests"))
    {
        "T"
    } else {
        match ext.as_str() {
            "java" => "J",
            "kt" | "kts" | "gradle" => "K",
            "http" => "H",
            "toml" => "≡",
            "json" => "{}",
            _ => "·",
        }
    }
}

/// Headless model for the virtualized Project dock tree (`O(viewport)`).
#[derive(Debug, Clone, Default)]
pub struct ProjectTreeModel {
    nodes: Vec<ProjectNode>,
    /// Lower-cased names, precomputed so filtering 10k+ nodes stays within a few milliseconds.
    lower_names: Vec<String>,
    /// Indices into `nodes` of the currently visible rows.
    visible: Vec<usize>,
    filter_query: String,
    selected_index: usize,
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
        };
        model.rebuild_visible();
        model
    }

    /// Flattens a `.gitignore`-aware [`kova_core::project::ProjectTree`] snapshot (`KOVA-020`).
    ///
    /// Directories up to `expand_depth` are expanded, except build/VCS output directories
    /// (`.git`, `target`, `build`, `.gradle`, …) which always start collapsed.
    #[must_use]
    pub fn from_project_tree(tree: &CoreProjectTree, expand_depth: u16) -> Self {
        let mut nodes = Vec::with_capacity(tree.file_count() + tree.dir_count());
        // Depth-first traversal with an explicit stack (children pushed in reverse to keep order).
        let mut stack: Vec<(CoreNodeId, u16)> = tree
            .children(tree.root_id())
            .iter()
            .rev()
            .map(|id| (*id, 0_u16))
            .collect();

        while let Some((id, depth)) = stack.pop() {
            let Some(node) = tree.get(id) else {
                continue;
            };
            let kind = match &node.kind {
                CoreNodeKind::Directory { children } => {
                    for child in children.iter().rev() {
                        stack.push((*child, depth.saturating_add(1)));
                    }
                    let collapsed = COLLAPSED_BY_DEFAULT.contains(&node.name.as_str());
                    ProjectNodeKind::Directory {
                        expanded: depth < expand_depth && !collapsed,
                    }
                }
                CoreNodeKind::File { .. } => ProjectNodeKind::File {
                    lang_hint: lang_hint_for(&node.name).to_owned(),
                },
            };
            nodes.push(ProjectNode {
                node_id: id.index() as u64,
                path: node.rel_path.clone(),
                is_kova_config: node.name == "kova.toml" && depth == 0,
                name: node.name.clone(),
                depth,
                kind,
            });
        }
        Self::from_nodes(nodes)
    }

    /// Returns the total number of nodes in the snapshot (visible or not).
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Project-relative paths of every file node (used to resolve stack frames, `KOVA-040`).
    pub fn file_paths(&self) -> impl Iterator<Item = &Path> + '_ {
        self.nodes
            .iter()
            .filter(|n| !n.is_dir())
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
                    .filter(|(_, name)| name.contains(query.as_str()))
                    .map(|(idx, _)| idx),
            );
        }
        let max = self.visible.len().saturating_sub(1);
        self.selected_index = self.selected_index.min(max);
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

    /// Selects the visible row that shows `path`, if any. Returns `true` when found.
    pub fn reveal_path(&mut self, path: &Path) -> bool {
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
            name,
            depth,
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
}
