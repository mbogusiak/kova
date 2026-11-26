//! Document symbol structure outline model (`StructureModel`).

use kova_syntax::{StructureNode, StructureOutline, StructureSymbolKind};

/// Fixed row height of the Structure dock list in logical pixels.
pub const STRUCTURE_ROW_HEIGHT_PX: f32 = 22.0;

/// Returns the display label for a `kova_syntax` [`StructureSymbolKind`].
///
/// `Section` covers TOML tables, `.http` requests and Gradle DSL blocks (`KOVA-023`).
#[must_use]
pub const fn kind_label(kind: StructureSymbolKind) -> &'static str {
    match kind {
        StructureSymbolKind::Package => "Package",
        StructureSymbolKind::Class => "Class",
        StructureSymbolKind::Interface => "Interface",
        StructureSymbolKind::Enum => "Enum",
        StructureSymbolKind::Record => "Record",
        StructureSymbolKind::Annotation => "Annotation",
        StructureSymbolKind::Object => "Object",
        StructureSymbolKind::Constructor => "Constructor",
        StructureSymbolKind::Method => "Method",
        StructureSymbolKind::Function => "Function",
        StructureSymbolKind::Field => "Field",
        StructureSymbolKind::Property => "Property",
        StructureSymbolKind::EnumConstant => "Constant",
        StructureSymbolKind::Section => "Section",
    }
}

/// Returns the one-glyph icon for a structure symbol kind label.
#[must_use]
pub fn kind_icon(label: &str) -> &'static str {
    match label {
        "Class" | "Record" | "Object" => "C",
        "Interface" => "I",
        "Enum" => "E",
        "Annotation" => "@",
        "Method" | "Function" | "Constructor" => "m",
        "Field" | "Property" | "Constant" => "f",
        "Section" => "§",
        _ => "·",
    }
}

/// A single symbol item displayed in the right `Structure` dock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructureSymbolItem {
    /// Symbol name (e.g. `OrderApp`, `main`, `processOrder`).
    pub name: String,
    /// Short kind label (`Class`, `Interface`, `Enum`, `Method`, `Field`, `Section`).
    pub kind_label: &'static str,
    /// 1-based line number in the document.
    pub line: usize,
    /// 1-based column number in the document.
    pub col: usize,
    /// Nesting depth (`0` for top-level symbols).
    pub depth: u16,
    /// Whether this symbol is a runnable entry point (`▶`).
    pub is_entry_point: bool,
}

/// Headless model for the virtualized `Structure` dock.
#[derive(Debug, Clone, Default)]
pub struct StructureModel {
    items: Vec<StructureSymbolItem>,
    selected_index: usize,
}

impl StructureModel {
    /// Creates a [`StructureModel`] from a list of symbol items.
    #[must_use]
    pub fn from_items(items: Vec<StructureSymbolItem>) -> Self {
        Self {
            items,
            selected_index: 0,
        }
    }

    /// Flattens a hierarchical `kova_syntax` [`StructureOutline`] into depth-annotated rows.
    ///
    /// A `static` method named `main` is flagged as a runnable entry point (`▶`).
    #[must_use]
    pub fn from_outline(outline: &StructureOutline) -> Self {
        let mut items = Vec::with_capacity(outline.symbol_count());
        let mut stack: Vec<(&StructureNode, u16)> =
            outline.roots.iter().rev().map(|n| (n, 0_u16)).collect();
        while let Some((node, depth)) = stack.pop() {
            items.push(StructureSymbolItem {
                name: node.name.clone(),
                kind_label: kind_label(node.kind),
                line: node.selection_point.row + 1,
                col: node.selection_point.column + 1,
                depth,
                is_entry_point: matches!(
                    node.kind,
                    StructureSymbolKind::Method | StructureSymbolKind::Function
                ) && node.name == "main"
                    && (node.is_static || node.kind == StructureSymbolKind::Function),
            });
            for child in node.children.iter().rev() {
                stack.push((child, depth.saturating_add(1)));
            }
        }
        Self::from_items(items)
    }

    /// Selects the symbol at `index` (clamped to the list).
    pub fn select_index(&mut self, index: usize) {
        self.selected_index = index.min(self.items.len().saturating_sub(1));
    }

    /// Returns all symbol items.
    #[must_use]
    pub fn items(&self) -> &[StructureSymbolItem] {
        &self.items
    }

    /// Returns the currently selected index.
    #[must_use]
    pub const fn selected_index(&self) -> usize {
        self.selected_index
    }

    /// Moves selection to the next symbol.
    pub fn select_next(&mut self) {
        if !self.items.is_empty() && self.selected_index + 1 < self.items.len() {
            self.selected_index += 1;
        }
    }

    /// Moves selection to the previous symbol.
    pub fn select_prev(&mut self) {
        if self.selected_index > 0 {
            self.selected_index -= 1;
        }
    }

    /// Returns the currently selected symbol, if any.
    #[must_use]
    pub fn selected_item(&self) -> Option<&StructureSymbolItem> {
        self.items.get(self.selected_index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structure_model_navigates_symbols() {
        let mut model = StructureModel::from_items(vec![
            StructureSymbolItem {
                name: "OrderApp".to_owned(),
                kind_label: "Class",
                line: 3,
                col: 1,
                depth: 0,
                is_entry_point: false,
            },
            StructureSymbolItem {
                name: "main".to_owned(),
                kind_label: "Method",
                line: 5,
                col: 5,
                depth: 1,
                is_entry_point: true,
            },
        ]);

        assert_eq!(
            model.selected_item().map(|s| s.name.as_str()),
            Some("OrderApp")
        );
        model.select_next();
        assert_eq!(model.selected_item().map(|s| s.line), Some(5));
        model.select_prev();
        assert_eq!(model.selected_index(), 0);
        model.select_index(99);
        assert_eq!(model.selected_index(), 1);
    }

    #[test]
    fn flattens_kova_syntax_outline_with_depths_and_entry_points() {
        let src = "package demo;\n\npublic class OrderApp {\n    private int count;\n\n    public static void main(String[] args) {}\n}\n";
        let registry = kova_syntax::LanguageRegistry::with_defaults().unwrap();
        let engine =
            kova_syntax::SyntaxEngine::new(registry, kova_syntax::SyntaxEngineConfig::default())
                .unwrap();
        let snapshot = kova_buffer::Buffer::from_text(src).snapshot();
        let tree = engine
            .parse_sync(kova_theme::SyntaxLanguage::Java, &snapshot, None)
            .unwrap();
        let outline = kova_syntax::extract_structure(&tree, &snapshot);
        let model = StructureModel::from_outline(&outline);

        let class = model.items().iter().find(|s| s.name == "OrderApp").unwrap();
        assert_eq!(class.kind_label, "Class");
        assert_eq!(class.line, 3);
        let main = model.items().iter().find(|s| s.name == "main").unwrap();
        assert_eq!(main.kind_label, "Method");
        assert_eq!(main.line, 6);
        assert!(main.depth > class.depth);
        assert!(main.is_entry_point);
        let field = model.items().iter().find(|s| s.name == "count").unwrap();
        assert_eq!(kind_icon(field.kind_label), "f");
        assert_eq!(kind_label(StructureSymbolKind::Section), "Section");
    }
}
