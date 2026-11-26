//! In-plugin sample catalog and a minimal `SELECT` engine.
//!
//! WIT API `0.1` has no outgoing-socket call yet, so the plugin ships a
//! deterministic sample database (`local-postgres`) to exercise the explorer
//! and the virtualized result table. A real driver replaces [`Catalog`] once
//! the host exposes networking.

/// Column definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Column {
    /// Column name.
    pub name: &'static str,
    /// SQL type shown in the explorer.
    pub sql_type: &'static str,
}

/// Table definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Table {
    /// Table name.
    pub name: &'static str,
    /// Column definitions in order.
    pub columns: &'static [Column],
    /// Number of generated sample rows.
    pub row_count: u32,
}

/// Schema definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schema {
    /// Schema name.
    pub name: &'static str,
    /// Tables in the schema.
    pub tables: &'static [Table],
}

/// Connection definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Connection {
    /// Stable connection id.
    pub id: &'static str,
    /// Network destination (must be allowlisted in `plugin.toml`).
    pub endpoint: &'static str,
    /// Schemas available on the connection.
    pub schemas: &'static [Schema],
}

/// Sample catalog browsed by the explorer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Catalog {
    /// Configured connections.
    pub connections: &'static [Connection],
}

const fn col(name: &'static str, sql_type: &'static str) -> Column {
    Column { name, sql_type }
}

/// Built-in sample catalog.
pub const SAMPLE_CATALOG: Catalog = Catalog {
    connections: &[Connection {
        id: "local-postgres",
        endpoint: "localhost:5432",
        schemas: &[Schema {
            name: "public",
            tables: &[
                Table {
                    name: "customers",
                    columns: &[
                        col("id", "bigint"),
                        col("name", "text"),
                        col("country", "text"),
                    ],
                    row_count: 50,
                },
                Table {
                    name: "orders",
                    columns: &[
                        col("id", "bigint"),
                        col("customer_id", "bigint"),
                        col("total", "numeric(10,2)"),
                        col("status", "text"),
                    ],
                    row_count: 5_000,
                },
            ],
        }],
    }],
};

/// Rows returned by [`Catalog::execute`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryResult {
    /// Selected column names.
    pub columns: Vec<String>,
    /// Row cells as display strings.
    pub rows: Vec<Vec<String>>,
}

const COUNTRIES: [&str; 5] = ["PL", "AE", "DE", "SA", "GB"];
const STATUSES: [&str; 3] = ["NEW", "PAID", "SHIPPED"];

fn cell(table: &str, column: &str, row: u32) -> String {
    let id = u64::from(row) + 1;
    match (table, column) {
        (_, "id") => id.to_string(),
        ("customers", "name") => format!("Customer {id}"),
        ("customers", "country") => COUNTRIES[row as usize % COUNTRIES.len()].to_owned(),
        ("orders", "customer_id") => (u64::from(row) % 50 + 1).to_string(),
        ("orders", "total") => format!("{}.{:02}", 10 + row % 490, row % 100),
        ("orders", "status") => STATUSES[row as usize % STATUSES.len()].to_owned(),
        _ => String::new(),
    }
}

impl Catalog {
    /// Finds a table as `schema.table` or bare `table` (first match).
    #[must_use]
    pub fn find_table(&self, qualified: &str) -> Option<&'static Table> {
        let (schema_name, table_name) = match qualified.split_once('.') {
            Some((schema, table)) => (Some(schema), table),
            None => (None, qualified),
        };
        self.connections
            .iter()
            .flat_map(|conn| conn.schemas.iter())
            .filter(|schema| schema_name.is_none_or(|name| schema.name.eq_ignore_ascii_case(name)))
            .flat_map(|schema| schema.tables.iter())
            .find(|table| table.name.eq_ignore_ascii_case(table_name))
    }

    /// Executes `SELECT <*|col, ...> FROM <[schema.]table> [LIMIT n]`.
    ///
    /// # Errors
    ///
    /// Returns a human-readable message for unsupported syntax, unknown
    /// tables or unknown columns.
    pub fn execute(&self, sql: &str) -> Result<QueryResult, String> {
        let normalized = sql.trim().trim_end_matches(';').replace(',', " , ");
        let tokens: Vec<&str> = normalized.split_whitespace().collect();
        let keyword = |index: usize, word: &str| {
            tokens
                .get(index)
                .is_some_and(|t| t.eq_ignore_ascii_case(word))
        };
        if !keyword(0, "select") {
            return Err("only `SELECT … FROM …` queries are supported".to_owned());
        }
        let from = (1..tokens.len())
            .find(|&i| keyword(i, "from"))
            .ok_or_else(|| "missing `FROM` clause".to_owned())?;
        let table_name = tokens
            .get(from + 1)
            .ok_or_else(|| "missing table after `FROM`".to_owned())?;
        let table = self
            .find_table(table_name)
            .ok_or_else(|| format!("unknown table `{table_name}`"))?;

        let limit = match tokens.get(from + 2) {
            None => None,
            Some(t) if t.eq_ignore_ascii_case("limit") => Some(
                tokens
                    .get(from + 3)
                    .and_then(|n| n.parse::<u32>().ok())
                    .ok_or_else(|| "`LIMIT` expects a non-negative integer".to_owned())?,
            ),
            Some(other) => return Err(format!("unsupported clause `{other}`")),
        };

        let projection: Vec<&str> = tokens[1..from]
            .iter()
            .copied()
            .filter(|t| *t != ",")
            .collect();
        let columns: Vec<&'static str> = if projection == ["*"] {
            table.columns.iter().map(|c| c.name).collect()
        } else if projection.is_empty() {
            return Err("empty column list".to_owned());
        } else {
            projection
                .iter()
                .map(|name| {
                    table
                        .columns
                        .iter()
                        .find(|c| c.name.eq_ignore_ascii_case(name))
                        .map(|c| c.name)
                        .ok_or_else(|| format!("unknown column `{name}` in `{}`", table.name))
                })
                .collect::<Result<_, _>>()?
        };

        let row_count = limit.map_or(table.row_count, |n| n.min(table.row_count));
        let rows = (0..row_count)
            .map(|row| columns.iter().map(|c| cell(table.name, c, row)).collect())
            .collect();
        Ok(QueryResult {
            columns: columns.iter().map(|c| (*c).to_owned()).collect(),
            rows,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::SAMPLE_CATALOG;

    #[test]
    fn selects_all_columns_with_limit() {
        let result = SAMPLE_CATALOG
            .execute("select * from public.orders limit 3;")
            .unwrap();
        assert_eq!(result.columns, vec!["id", "customer_id", "total", "status"]);
        assert_eq!(result.rows.len(), 3);
        assert_eq!(result.rows[1], vec!["2", "2", "11.01", "PAID"]);
    }

    #[test]
    fn projects_columns_and_returns_full_table_without_limit() {
        let result = SAMPLE_CATALOG
            .execute("SELECT name,country FROM customers")
            .unwrap();
        assert_eq!(result.columns, vec!["name", "country"]);
        assert_eq!(result.rows.len(), 50);
        assert_eq!(result.rows[0], vec!["Customer 1", "PL"]);
    }

    #[test]
    fn rejects_unsupported_queries() {
        for (sql, needle) in [
            ("DELETE FROM orders", "only `SELECT"),
            ("SELECT *", "missing `FROM`"),
            ("SELECT * FROM nope", "unknown table"),
            ("SELECT bogus FROM orders", "unknown column"),
            ("SELECT * FROM orders WHERE id = 1", "unsupported clause"),
            ("SELECT * FROM orders LIMIT x", "LIMIT"),
            ("SELECT FROM orders", "empty column list"),
        ] {
            let err = SAMPLE_CATALOG.execute(sql).unwrap_err();
            assert!(err.contains(needle), "{sql}: {err}");
        }
    }
}
