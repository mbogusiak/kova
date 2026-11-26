//! Builder and constructors for arena-encoded declarative widget trees.
//!
//! Plugins never draw pixels: they describe a tree of semantic widgets which
//! the host validates and renders natively with GPUI. A [`WidgetTree`] stores
//! nodes in a flat arena and refers to children by index.

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

#[cfg(test)]
mod tests {
    use super::{UiBuilder, badge, find_node, form, markdown, split};
    use crate::{SplitDirection, StyleVariant, WidgetKind};

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
}
