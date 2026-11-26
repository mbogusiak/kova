//! Explorer/query state and declarative Tool Window rendering for `kova-database`.

use std::collections::BTreeSet;

use kova_plugin_rs::ui::{self, UiBuilder};
use kova_plugin_rs::{SplitDirection, StyleVariant, TreeItem, UiEvent, UiEventKind, WidgetTree};

use crate::catalog::{Catalog, QueryResult};

/// Tool Window id of the database explorer.
pub const EXPLORER_PANEL_ID: &str = "kova-database.explorer";
/// Widget id of the schema `Tree`.
pub const SCHEMA_TREE_ID: &str = "kova-database.schema-tree";
/// Widget id of the query `TextInput`.
pub const QUERY_INPUT_ID: &str = "kova-database.query";
/// Widget id of the query preview `CodeBlock`.
pub const QUERY_PREVIEW_ID: &str = "kova-database.query-preview";
/// Widget id of the run `Button`.
pub const RUN_BUTTON_ID: &str = "kova-database.run";
/// Widget id of the query `Form`.
pub const QUERY_FORM_ID: &str = "kova-database.query-form";
/// Widget id of the status `Badge`.
pub const STATUS_ID: &str = "kova-database.status";
/// Widget id of the result `Table`.
pub const RESULTS_TABLE_ID: &str = "kova-database.results";

/// Row cap applied to queries generated from the explorer.
pub const DEFAULT_ROW_LIMIT: u32 = 1_000;

const TABLE_PREFIX: &str = "table:";

fn connection_node(conn: &str) -> String {
    format!("conn:{conn}")
}

fn schema_node(conn: &str, schema: &str) -> String {
    format!("schema:{conn}/{schema}")
}

fn table_node(conn: &str, schema: &str, table: &str) -> String {
    format!("{TABLE_PREFIX}{conn}/{schema}/{table}")
}

/// Outcome of the last executed query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryOutcome {
    /// Nothing executed yet.
    Idle,
    /// Rows returned successfully.
    Rows(QueryResult),
    /// Query rejected with a message.
    Failed(String),
}

/// Whole explorer Tool Window state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplorerState {
    catalog: Catalog,
    expanded: BTreeSet<String>,
    selected_node: Option<String>,
    query: String,
    outcome: QueryOutcome,
    selected_row: Option<u32>,
}

impl ExplorerState {
    /// Fresh state with every connection and schema expanded.
    #[must_use]
    pub fn new(catalog: Catalog) -> Self {
        let mut expanded = BTreeSet::new();
        for conn in catalog.connections {
            expanded.insert(connection_node(conn.id));
            for schema in conn.schemas {
                expanded.insert(schema_node(conn.id, schema.name));
            }
        }
        Self {
            catalog,
            expanded,
            selected_node: None,
            query: String::new(),
            outcome: QueryOutcome::Idle,
            selected_row: None,
        }
    }

    /// Current query text.
    #[must_use]
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Outcome of the last query.
    #[must_use]
    pub const fn outcome(&self) -> &QueryOutcome {
        &self.outcome
    }

    /// Replaces the query text.
    pub fn set_query(&mut self, query: impl Into<String>) {
        self.query = query.into();
    }

    /// Executes the current query against the catalog.
    pub fn run_query(&mut self) {
        self.selected_row = None;
        self.outcome = match self.catalog.execute(&self.query) {
            Ok(result) => QueryOutcome::Rows(result),
            Err(message) => QueryOutcome::Failed(message),
        };
    }

    /// Applies a UI event; returns whether the panel must be re-rendered.
    pub fn handle_ui_event(&mut self, event: &UiEvent) -> bool {
        match (event.widget_id.as_str(), &event.kind) {
            (SCHEMA_TREE_ID, UiEventKind::TreeToggled(node)) => {
                if !self.expanded.remove(node) {
                    self.expanded.insert(node.clone());
                }
                true
            }
            (SCHEMA_TREE_ID, UiEventKind::TreeSelected(node)) => {
                self.selected_node = Some(node.clone());
                if let Some(path) = node.strip_prefix(TABLE_PREFIX)
                    && let Some((_, qualified)) = path.split_once('/')
                {
                    let qualified = qualified.replace('/', ".");
                    self.query = format!("SELECT * FROM {qualified} LIMIT {DEFAULT_ROW_LIMIT}");
                    self.run_query();
                }
                true
            }
            (QUERY_INPUT_ID, UiEventKind::TextChanged(text)) => {
                self.query.clone_from(text);
                true
            }
            (RUN_BUTTON_ID, UiEventKind::Clicked)
            | (QUERY_FORM_ID | QUERY_INPUT_ID, UiEventKind::Submitted) => {
                self.run_query();
                true
            }
            (RESULTS_TABLE_ID, UiEventKind::RowSelected(row)) => {
                self.selected_row = Some(*row);
                true
            }
            _ => false,
        }
    }

    fn tree_items(&self) -> Vec<TreeItem> {
        let mut items = Vec::new();
        let mut push = |id: String, label: String, depth: u32, has_children: bool, style| {
            let expanded = has_children && self.expanded.contains(&id);
            let selected = self.selected_node.as_deref() == Some(id.as_str());
            items.push(TreeItem {
                id,
                label,
                depth,
                expanded,
                selected,
                has_children,
                style,
            });
            expanded
        };
        for conn in self.catalog.connections {
            let label = format!("{} ({})", conn.id, conn.endpoint);
            if !push(
                connection_node(conn.id),
                label,
                0,
                true,
                StyleVariant::Primary,
            ) {
                continue;
            }
            for schema in conn.schemas {
                let id = schema_node(conn.id, schema.name);
                if !push(id, schema.name.to_owned(), 1, true, StyleVariant::Muted) {
                    continue;
                }
                for table in schema.tables {
                    let id = table_node(conn.id, schema.name, table.name);
                    if !push(
                        id.clone(),
                        table.name.to_owned(),
                        2,
                        true,
                        StyleVariant::Primary,
                    ) {
                        continue;
                    }
                    for column in table.columns {
                        push(
                            format!("column:{}/{}", &id[TABLE_PREFIX.len()..], column.name),
                            format!("{}: {}", column.name, column.sql_type),
                            3,
                            false,
                            StyleVariant::Muted,
                        );
                    }
                }
            }
        }
        items
    }

    /// Renders the declarative explorer Tool Window.
    #[must_use]
    pub fn render(&self) -> WidgetTree {
        let mut ui = UiBuilder::new();

        let tree = ui.leaf(SCHEMA_TREE_ID, ui::tree(self.tree_items()));

        let preview_code = if self.query.trim().is_empty() {
            "-- select a table in the explorer or type a query"
        } else {
            self.query.as_str()
        };
        let preview = ui.leaf(QUERY_PREVIEW_ID, ui::code_block("sql", preview_code));
        let input = ui.leaf(
            QUERY_INPUT_ID,
            ui::text_input("SQL", &self.query, "SELECT * FROM public.orders LIMIT 100"),
        );
        let run = ui.leaf(
            RUN_BUTTON_ID,
            ui::button("Run", self.query.trim().is_empty()),
        );
        let (status_label, status_style) = match &self.outcome {
            QueryOutcome::Idle => ("No query executed".to_owned(), StyleVariant::Muted),
            QueryOutcome::Rows(result) => {
                (format!("{} rows", result.rows.len()), StyleVariant::Accent)
            }
            QueryOutcome::Failed(message) => (message.clone(), StyleVariant::Error),
        };
        let status = ui.styled(STATUS_ID, ui::badge(status_label), status_style);
        let editor = ui.container(
            QUERY_FORM_ID,
            ui::form(Some("Query")),
            &[preview, input, run, status],
        );

        let (columns, rows) = match &self.outcome {
            QueryOutcome::Rows(result) => (
                result
                    .columns
                    .iter()
                    .map(|name| ui::column(name, name, 1))
                    .collect(),
                result.rows.clone(),
            ),
            QueryOutcome::Idle | QueryOutcome::Failed(_) => (Vec::new(), Vec::new()),
        };
        let results = ui.leaf(
            RESULTS_TABLE_ID,
            ui::table(columns, rows, self.selected_row),
        );

        let workbench = ui.container(
            "kova-database.workbench",
            ui::split(SplitDirection::Vertical, 35),
            &[editor, results],
        );
        let root = ui.container(
            "kova-database.root",
            ui::split(SplitDirection::Horizontal, 30),
            &[tree, workbench],
        );
        ui.finish(root)
    }
}

#[cfg(test)]
mod tests {
    use kova_plugin_rs::ui::find_node;
    use kova_plugin_rs::{UiEvent, UiEventKind, WidgetKind};

    use super::{
        ExplorerState, QUERY_INPUT_ID, QueryOutcome, RESULTS_TABLE_ID, RUN_BUTTON_ID,
        SCHEMA_TREE_ID, STATUS_ID,
    };
    use crate::catalog::SAMPLE_CATALOG;

    fn event(widget_id: &str, kind: UiEventKind) -> UiEvent {
        UiEvent {
            widget_id: widget_id.to_owned(),
            kind,
        }
    }

    fn tree_ids(state: &ExplorerState) -> Vec<String> {
        match &find_node(&state.render(), SCHEMA_TREE_ID).map(|n| n.kind.clone()) {
            Some(WidgetKind::Tree(tree)) => tree.items.iter().map(|i| i.id.clone()).collect(),
            other => panic!("unexpected tree widget {other:?}"),
        }
    }

    #[test]
    fn explorer_starts_with_schemas_expanded_and_tables_collapsed() {
        let state = ExplorerState::new(SAMPLE_CATALOG);
        assert_eq!(
            tree_ids(&state),
            vec![
                "conn:local-postgres",
                "schema:local-postgres/public",
                "table:local-postgres/public/customers",
                "table:local-postgres/public/orders",
            ]
        );
    }

    #[test]
    fn toggling_a_table_reveals_and_hides_columns() {
        let mut state = ExplorerState::new(SAMPLE_CATALOG);
        let toggle = event(
            SCHEMA_TREE_ID,
            UiEventKind::TreeToggled("table:local-postgres/public/orders".to_owned()),
        );
        assert!(state.handle_ui_event(&toggle));
        assert!(tree_ids(&state).contains(&"column:local-postgres/public/orders/total".to_owned()));
        assert!(state.handle_ui_event(&toggle));
        assert_eq!(tree_ids(&state).len(), 4);
    }

    #[test]
    fn selecting_a_table_runs_a_limited_query() {
        let mut state = ExplorerState::new(SAMPLE_CATALOG);
        assert!(state.handle_ui_event(&event(
            SCHEMA_TREE_ID,
            UiEventKind::TreeSelected("table:local-postgres/public/orders".to_owned()),
        )));
        assert_eq!(state.query(), "SELECT * FROM public.orders LIMIT 1000");
        assert!(matches!(state.outcome(), QueryOutcome::Rows(r) if r.rows.len() == 1_000));

        assert!(state.handle_ui_event(&event(RESULTS_TABLE_ID, UiEventKind::RowSelected(7))));
        let tree = state.render();
        assert!(matches!(
            find_node(&tree, RESULTS_TABLE_ID).map(|n| &n.kind),
            Some(WidgetKind::Table(t)) if t.columns.len() == 4 && t.selected_row == Some(7)
        ));
    }

    #[test]
    fn typed_query_errors_are_shown_in_status_badge() {
        let mut state = ExplorerState::new(SAMPLE_CATALOG);
        state.handle_ui_event(&event(
            QUERY_INPUT_ID,
            UiEventKind::TextChanged("DROP TABLE orders".to_owned()),
        ));
        state.handle_ui_event(&event(RUN_BUTTON_ID, UiEventKind::Clicked));
        let tree = state.render();
        assert!(matches!(
            find_node(&tree, STATUS_ID).map(|n| &n.kind),
            Some(WidgetKind::Badge(b)) if b.label.contains("only `SELECT")
        ));
        assert!(!state.handle_ui_event(&event("unknown", UiEventKind::Clicked)));
    }
}
