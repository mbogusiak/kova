//! Builder and constructors for arena-encoded declarative widget trees.
//!
//! Plugins never draw pixels: they describe a tree of semantic widgets which
//! the host validates and renders natively with GPUI. A [`WidgetTree`] stores
//! nodes in a flat arena and refers to children by index.
//!
//! Two ways to build one:
//!
//! - **Fluent, nested** — every constructor below returns a [`WidgetKind`];
//!   [`WidgetKindExt::id`] turns it into an [`Element`] that takes a style and
//!   children, and [`Element::build`] flattens it into a [`WidgetTree`]:
//!
//!   ```
//!   use kova_plugin_rs::prelude::*;
//!
//!   let tree = ui::split(SplitDirection::Horizontal, 30)
//!       .id("demo.root")
//!       .child(ui::markdown("# Files").id("demo.title"))
//!       .child(
//!           ui::badge("3 errors")
//!               .id("demo.status")
//!               .style(StyleVariant::Error),
//!       )
//!       .build();
//!   assert_eq!(tree.nodes.len(), 3);
//!   assert!(ui::validate(&tree).is_ok());
//!   ```
//!
//! - **Arena, bottom-up** — [`UiBuilder`] adds children before parents and
//!   returns [`NodeRef`] handles, useful when nodes are shared or computed in
//!   loops. [`UiBuilder::add`] accepts an [`Element`] too.

use std::collections::HashSet;

use crate::{
    BadgeProps, ButtonProps, CodeBlockProps, FormProps, ListItem, ListProps, MarkdownProps,
    SelectProps, SplitDirection, SplitProps, StyleVariant, TabItem, TableColumn, TableProps,
    TabsProps, TextInputProps, ToggleProps, TreeItem, TreeProps, WidgetKind, WidgetNode,
    WidgetTree,
};

/// Index of a node inside the arena of a [`UiBuilder`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeRef(u32);

impl NodeRef {
    /// Raw arena index of this node.
    #[must_use]
    pub const fn index(self) -> u32 {
        self.0
    }
}

/// Incremental builder for a [`WidgetTree`].
///
/// Children must be created before their parent (bottom-up), which makes
/// cycles impossible by construction.
#[derive(Debug, Default)]
pub struct UiBuilder {
    nodes: Vec<WidgetNode>,
}

impl UiBuilder {
    /// Creates an empty builder.
    #[must_use]
    pub const fn new() -> Self {
        Self { nodes: Vec::new() }
    }

    /// Adds a leaf widget with the [`StyleVariant::Primary`] style.
    pub fn leaf(&mut self, id: impl Into<String>, kind: WidgetKind) -> NodeRef {
        self.node(id, kind, StyleVariant::Primary, &[])
    }

    /// Adds a styled leaf widget.
    pub fn styled(
        &mut self,
        id: impl Into<String>,
        kind: WidgetKind,
        style: StyleVariant,
    ) -> NodeRef {
        self.node(id, kind, style, &[])
    }

    /// Adds a container widget with previously created `children`.
    pub fn container(
        &mut self,
        id: impl Into<String>,
        kind: WidgetKind,
        children: &[NodeRef],
    ) -> NodeRef {
        self.node(id, kind, StyleVariant::Primary, children)
    }

    /// Adds a fully specified node and returns its reference.
    ///
    /// # Panics
    ///
    /// Panics if more than `u32::MAX` nodes are added (far beyond the host limit).
    pub fn node(
        &mut self,
        id: impl Into<String>,
        kind: WidgetKind,
        style: StyleVariant,
        children: &[NodeRef],
    ) -> NodeRef {
        let index = u32::try_from(self.nodes.len()).unwrap_or(u32::MAX);
        self.nodes.push(WidgetNode {
            id: id.into(),
            kind,
            style,
            children: children.iter().map(|child| child.0).collect(),
        });
        NodeRef(index)
    }

    /// Adds a nested [`Element`] (children first) and returns the reference to its root.
    pub fn add(&mut self, element: Element) -> NodeRef {
        let children: Vec<NodeRef> = element
            .children
            .into_iter()
            .map(|child| self.add(child))
            .collect();
        self.node(element.id, element.kind, element.style, &children)
    }

    /// Number of nodes added so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether no node was added yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Finishes the tree with `root` as its root node.
    #[must_use]
    pub fn finish(self, root: NodeRef) -> WidgetTree {
        WidgetTree {
            root: root.0,
            nodes: self.nodes,
        }
    }
}

/// Finds a node by its stable `id`.
#[must_use]
pub fn find_node<'a>(tree: &'a WidgetTree, id: &str) -> Option<&'a WidgetNode> {
    tree.nodes.iter().find(|node| node.id == id)
}

/// `Split` container dividing space between two children.
#[must_use]
pub const fn split(direction: SplitDirection, ratio_percent: u8) -> WidgetKind {
    WidgetKind::Split(SplitProps {
        direction,
        ratio_percent,
    })
}

/// `Tabs` container; children correspond to `titles` in order.
#[must_use]
pub fn tabs(titles: &[(&str, &str)], active_index: u32) -> WidgetKind {
    WidgetKind::Tabs(TabsProps {
        items: titles
            .iter()
            .map(|(id, title)| TabItem {
                id: (*id).to_owned(),
                title: (*title).to_owned(),
            })
            .collect(),
        active_index,
    })
}

/// Flattened `Tree` widget.
#[must_use]
pub const fn tree(items: Vec<TreeItem>) -> WidgetKind {
    WidgetKind::Tree(TreeProps { items })
}

/// Virtualized `Table` widget.
#[must_use]
pub const fn table(
    columns: Vec<TableColumn>,
    rows: Vec<Vec<String>>,
    selected_row: Option<u32>,
) -> WidgetKind {
    WidgetKind::Table(TableProps {
        columns,
        rows,
        selected_row,
    })
}

/// Column header for [`table`].
#[must_use]
pub fn column(id: &str, title: &str, width_weight: u16) -> TableColumn {
    TableColumn {
        id: id.to_owned(),
        title: title.to_owned(),
        width_weight,
    }
}

/// Virtualized `List` widget.
#[must_use]
pub const fn list(items: Vec<ListItem>, selected_index: Option<u32>) -> WidgetKind {
    WidgetKind::ListView(ListProps {
        items,
        selected_index,
    })
}

/// `Form` container grouping form controls.
#[must_use]
pub fn form(title: Option<&str>) -> WidgetKind {
    WidgetKind::Form(FormProps {
        title: title.map(str::to_owned),
    })
}

/// `TextInput` form control.
#[must_use]
pub fn text_input(label: &str, value: &str, placeholder: &str) -> WidgetKind {
    WidgetKind::TextInput(TextInputProps {
        label: label.to_owned(),
        value: value.to_owned(),
        placeholder: placeholder.to_owned(),
    })
}

/// `Select` form control.
#[must_use]
pub fn select(label: &str, options: &[&str], selected_index: Option<u32>) -> WidgetKind {
    WidgetKind::Select(SelectProps {
        label: label.to_owned(),
        options: options.iter().map(|option| (*option).to_owned()).collect(),
        selected_index,
    })
}

/// `Toggle` form control.
#[must_use]
pub fn toggle(label: &str, checked: bool) -> WidgetKind {
    WidgetKind::Toggle(ToggleProps {
        label: label.to_owned(),
        checked,
    })
}

/// `Button` form control.
#[must_use]
pub fn button(label: &str, disabled: bool) -> WidgetKind {
    WidgetKind::Button(ButtonProps {
        label: label.to_owned(),
        disabled,
    })
}

/// `CodeBlock` widget highlighted by the host with `kova_syntax`.
#[must_use]
pub fn code_block(language: &str, code: &str) -> WidgetKind {
    WidgetKind::CodeBlock(CodeBlockProps {
        language: language.to_owned(),
        code: code.to_owned(),
    })
}

/// `Markdown` widget.
#[must_use]
pub fn markdown(content: impl Into<String>) -> WidgetKind {
    WidgetKind::Markdown(MarkdownProps {
        content: content.into(),
    })
}

/// `Badge` status widget.
#[must_use]
pub fn badge(label: impl Into<String>) -> WidgetKind {
    WidgetKind::Badge(BadgeProps {
        label: label.into(),
    })
}

/// A widget with its id, style and nested children (fluent builder).
///
/// Create one with [`WidgetKindExt::id`] or [`Element::new`]; finish it with
/// [`Element::build`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    id: String,
    kind: WidgetKind,
    style: StyleVariant,
    children: Vec<Element>,
}

impl Element {
    /// Creates an element with the [`StyleVariant::Primary`] style and no children.
    #[must_use]
    pub fn new(id: impl Into<String>, kind: WidgetKind) -> Self {
        Self {
            id: id.into(),
            kind,
            style: StyleVariant::Primary,
            children: Vec::new(),
        }
    }

    /// Sets the semantic style token.
    #[must_use]
    pub const fn style(mut self, style: StyleVariant) -> Self {
        self.style = style;
        self
    }

    /// Appends one child.
    #[must_use]
    pub fn child(mut self, child: Self) -> Self {
        self.children.push(child);
        self
    }

    /// Appends several children in order.
    #[must_use]
    pub fn children(mut self, children: impl IntoIterator<Item = Self>) -> Self {
        self.children.extend(children);
        self
    }

    /// Stable id of this element.
    #[must_use]
    pub fn element_id(&self) -> &str {
        &self.id
    }

    /// Flattens the element into an arena [`WidgetTree`] (root is the last node).
    #[must_use]
    pub fn build(self) -> WidgetTree {
        let mut builder = UiBuilder::new();
        let root = builder.add(self);
        builder.finish(root)
    }
}

/// Turns a [`WidgetKind`] from the constructors in this module into an [`Element`].
pub trait WidgetKindExt {
    /// Gives the widget its stable `id` (prefix it with the plugin id).
    fn id(self, id: impl Into<String>) -> Element;
}

impl WidgetKindExt for WidgetKind {
    fn id(self, id: impl Into<String>) -> Element {
        Element::new(id, self)
    }
}

/// Checks the invariants the host enforces on a returned tree.
///
/// Useful in native unit tests of a plugin: a non-empty arena, a valid root,
/// non-empty unique ids, in-bounds children, no cycles and split ratios up to 100.
///
/// # Errors
///
/// Returns a description of the first violated invariant.
pub fn validate(tree: &WidgetTree) -> Result<(), String> {
    let len = tree.nodes.len();
    if len == 0 {
        return Err("widget tree must contain at least one node".to_owned());
    }
    let index = |raw: u32| usize::try_from(raw).ok().filter(|i| *i < len);
    let root = index(tree.root).ok_or_else(|| format!("root index {} out of bounds", tree.root))?;

    let mut ids = HashSet::with_capacity(len);
    for node in &tree.nodes {
        if node.id.is_empty() {
            return Err("widget node has an empty `id`".to_owned());
        }
        if !ids.insert(node.id.as_str()) {
            return Err(format!("duplicate widget id `{}`", node.id));
        }
        if let WidgetKind::Split(props) = &node.kind
            && props.ratio_percent > 100
        {
            return Err(format!("split `{}` ratio_percent exceeds 100", node.id));
        }
        if let Some(bad) = node.children.iter().find(|c| index(**c).is_none()) {
            return Err(format!(
                "widget `{}` has out-of-bounds child {bad}",
                node.id
            ));
        }
    }

    // Iterative DFS from the root: 1 = on the current path, 2 = done.
    let mut state = vec![0_u8; len];
    let mut stack = vec![(root, false)];
    while let Some((node, leaving)) = stack.pop() {
        if leaving {
            state[node] = 2;
            continue;
        }
        if state[node] == 2 {
            continue;
        }
        state[node] = 1;
        stack.push((node, true));
        for child in tree.nodes[node].children.iter().filter_map(|c| index(*c)) {
            match state[child] {
                1 => {
                    return Err(format!(
                        "cycle detected at widget `{}`",
                        tree.nodes[child].id
                    ));
                }
                0 => stack.push((child, false)),
                _ => {}
            }
        }
    }
    Ok(())
}

/// Tab header for [`tabs`] built from owned strings.
#[must_use]
pub fn tab(id: &str, title: &str) -> TabItem {
    TabItem {
        id: id.to_owned(),
        title: title.to_owned(),
    }
}

/// Collapsed, unselected [`TreeItem`] with the primary style.
#[must_use]
pub fn tree_item(id: &str, label: &str, depth: u32, has_children: bool) -> TreeItem {
    TreeItem {
        id: id.to_owned(),
        label: label.to_owned(),
        depth,
        expanded: false,
        selected: false,
        has_children,
        style: StyleVariant::Primary,
    }
}

/// [`ListItem`] with the primary style.
#[must_use]
pub fn list_item(id: &str, label: &str, detail: Option<&str>) -> ListItem {
    ListItem {
        id: id.to_owned(),
        label: label.to_owned(),
        detail: detail.map(str::to_owned),
        style: StyleVariant::Primary,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Element, UiBuilder, WidgetKindExt, badge, button, code_block, column, find_node, form,
        list, list_item, markdown, select, split, tab, table, tabs, text_input, toggle, tree,
        tree_item, validate,
    };
    use crate::{SplitDirection, StyleVariant, WidgetKind, WidgetNode, WidgetTree};

    #[test]
    fn builds_bottom_up_arena_with_root_last() {
        let mut ui = UiBuilder::new();
        let text = ui.leaf("text", markdown("# Hi"));
        let status = ui.styled("status", badge("OK"), StyleVariant::Accent);
        let panel = ui.container("panel", form(Some("Panel")), &[text, status]);
        let root = ui.container(
            "root",
            split(SplitDirection::Vertical, 50),
            &[panel, status],
        );
        assert_eq!(ui.len(), 4);
        let tree = ui.finish(root);

        assert_eq!(tree.root, 3);
        assert_eq!(tree.nodes[2].children, vec![0, 1]);
        assert_eq!(tree.nodes[1].style, StyleVariant::Accent);
        assert!(matches!(
            find_node(&tree, "text").map(|node| &node.kind),
            Some(WidgetKind::Markdown(props)) if props.content == "# Hi"
        ));
        assert!(find_node(&tree, "missing").is_none());
    }

    #[test]
    fn children_always_precede_parents() {
        let mut ui = UiBuilder::new();
        assert_eq!(ui.len(), 0);
        let a = ui.leaf("a", badge("a"));
        let b = ui.container("b", form(None), &[a]);
        let tree = ui.finish(b);
        for (index, node) in tree.nodes.iter().enumerate() {
            for child in &node.children {
                assert!(usize::try_from(*child).is_ok_and(|c| c < index));
            }
        }
    }

    #[test]
    fn fluent_elements_flatten_children_before_parents() {
        let tree = split(SplitDirection::Horizontal, 40)
            .id("root")
            .child(
                form(Some("Settings"))
                    .id("form")
                    .child(text_input("Name", "", "Your name").id("name"))
                    .child(toggle("Loud", true).id("loud")),
            )
            .child(badge("ok").id("status").style(StyleVariant::Accent))
            .build();

        let ids: Vec<&str> = tree.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, ["name", "loud", "form", "status", "root"]);
        assert_eq!(tree.root, 4);
        assert_eq!(tree.nodes[2].children, vec![0, 1]);
        assert_eq!(tree.nodes[4].children, vec![2, 3]);
        assert_eq!(tree.nodes[3].style, StyleVariant::Accent);
        assert!(validate(&tree).is_ok());
    }

    #[test]
    fn builder_mixes_elements_and_arena_nodes() {
        let mut ui = UiBuilder::new();
        let shared = ui.leaf("shared", badge("shared"));
        let nested = ui.add(Element::new("nested", form(None)).child(markdown("x").id("text")));
        assert_eq!(nested.index(), 2);
        let root = ui.container("root", form(None), &[shared, nested]);
        let tree = ui.finish(root);
        assert!(validate(&tree).is_ok());
        assert_eq!(
            find_node(&tree, "nested").map(|n| n.children.clone()),
            Some(vec![1])
        );
    }

    #[test]
    fn constructors_cover_every_widget_kind() {
        let kinds = [
            split(SplitDirection::Vertical, 50),
            tabs(&[("a", "A")], 0),
            tree(vec![tree_item("t", "T", 0, true)]),
            table(vec![column("c", "C", 1)], vec![vec!["1".to_owned()]], None),
            list(vec![list_item("l", "L", Some("detail"))], Some(0)),
            form(Some("F")),
            text_input("Label", "value", "placeholder"),
            select("Pick", &["one", "two"], Some(1)),
            toggle("On", false),
            button("Go", false),
            code_block("rust", "fn main() {}"),
            markdown("**md**"),
            badge("1"),
        ];
        let names: Vec<&str> = kinds
            .iter()
            .map(|kind| match kind {
                WidgetKind::Split(_) => "split",
                WidgetKind::Tabs(_) => "tabs",
                WidgetKind::Tree(_) => "tree",
                WidgetKind::Table(_) => "table",
                WidgetKind::ListView(_) => "list",
                WidgetKind::Form(_) => "form",
                WidgetKind::TextInput(_) => "text-input",
                WidgetKind::Select(_) => "select",
                WidgetKind::Toggle(_) => "toggle",
                WidgetKind::Button(_) => "button",
                WidgetKind::CodeBlock(_) => "code-block",
                WidgetKind::Markdown(_) => "markdown",
                WidgetKind::Badge(_) => "badge",
            })
            .collect();
        let unique: std::collections::HashSet<&str> = names.iter().copied().collect();
        assert_eq!(unique.len(), 13);
        assert_eq!(tab("x", "X").title, "X");
    }

    fn raw(root: u32, nodes: &[(&str, &[u32])]) -> WidgetTree {
        WidgetTree {
            root,
            nodes: nodes
                .iter()
                .map(|(id, children)| WidgetNode {
                    id: (*id).to_owned(),
                    kind: badge("b"),
                    style: StyleVariant::Primary,
                    children: children.to_vec(),
                })
                .collect(),
        }
    }

    #[test]
    fn validate_rejects_what_the_host_rejects() {
        assert!(validate(&raw(0, &[])).unwrap_err().contains("at least one"));
        assert!(
            validate(&raw(3, &[("a", &[])]))
                .unwrap_err()
                .contains("root")
        );
        assert!(
            validate(&raw(0, &[("", &[])]))
                .unwrap_err()
                .contains("empty")
        );
        assert!(
            validate(&raw(1, &[("a", &[]), ("a", &[0])]))
                .unwrap_err()
                .contains("duplicate")
        );
        assert!(
            validate(&raw(0, &[("a", &[9])]))
                .unwrap_err()
                .contains("out-of-bounds")
        );
        assert!(
            validate(&raw(0, &[("a", &[1]), ("b", &[0])]))
                .unwrap_err()
                .contains("cycle")
        );
        let mut bad_split = raw(0, &[("s", &[])]);
        bad_split.nodes[0].kind = split(SplitDirection::Horizontal, 101);
        assert!(validate(&bad_split).unwrap_err().contains("ratio"));
        // A shared child (DAG) is allowed, like in the host.
        assert!(validate(&raw(2, &[("a", &[]), ("b", &[0]), ("c", &[0, 1])])).is_ok());
    }
}
