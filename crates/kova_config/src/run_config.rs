//! Lossless editing of shared `[[run_configurations]]` in the project `kova.toml` (`KOVA-040`).
//!
//! Run configurations live in the single, team-shared `<project_root>/kova.toml` file; Kova never
//! creates an `.idea/` folder or any other hidden project directory. Every edit goes through the
//! `toml_edit` CST, so comments and the layout of untouched entries survive, and an edit that
//! would make the document invalid (empty or duplicate names) is rolled back.

use std::collections::BTreeMap;

use toml_edit::{Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table, value};

use crate::model::ConfigError;
use crate::project::{ProjectConfigDocument, RunConfigKind, RunConfiguration, deserialize_project};

/// Top-level key holding the array of run configuration tables.
pub const RUN_CONFIGURATIONS_KEY: &str = "run_configurations";

impl RunConfigKind {
    /// The `kind` string written to `kova.toml`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::JavaMain => "java_main",
            Self::SpringBoot => "spring_boot",
            Self::GradleTask => "gradle_task",
            Self::MavenGoal => "maven_goal",
            Self::Junit => "junit",
        }
    }

    /// Human-readable label shown in the Run Configuration list.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::JavaMain => "Java",
            Self::SpringBoot => "Spring Boot",
            Self::GradleTask => "Gradle",
            Self::MavenGoal => "Maven",
            Self::Junit => "JUnit",
        }
    }
}

impl RunConfiguration {
    /// Creates a `JUnit` run configuration for a test class (`com.acme.FooTest`) or a single
    /// method (`com.acme.FooTest#shouldWork`).
    #[must_use]
    pub fn junit(name: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            kind: RunConfigKind::Junit,
            main_class: Some(target.into()),
            args: Vec::new(),
            vm_options: Vec::new(),
            env: BTreeMap::new(),
            working_dir: None,
            task: None,
        }
    }

    /// One-line summary for list rows, e.g. `JUnit · com.acme.FooTest#bar`.
    #[must_use]
    pub fn summary(&self) -> String {
        let target = self
            .task
            .as_deref()
            .or(self.main_class.as_deref())
            .unwrap_or("—");
        format!("{} · {target}", self.kind.label())
    }
}

impl ProjectConfigDocument {
    /// Creates an empty in-memory `kova.toml` (`schema_version = 1`).
    ///
    /// Nothing is written to disk until [`ProjectConfigDocument::save_to_project`] is called
    /// explicitly by the user.
    #[must_use]
    pub fn empty() -> Self {
        let mut doc = DocumentMut::new();
        doc.insert("schema_version", value(1_i64));
        Self {
            doc,
            config: crate::project::ProjectConfig::default(),
        }
    }

    /// Returns the shared run configurations in file order.
    #[must_use]
    pub fn run_configurations(&self) -> &[RunConfiguration] {
        &self.config.run_configurations
    }

    /// Returns the run configuration named `name`, if any.
    #[must_use]
    pub fn run_configuration(&self, name: &str) -> Option<&RunConfiguration> {
        self.config
            .run_configurations
            .iter()
            .find(|cfg| cfg.name == name)
    }

    /// Inserts `config`, or replaces the entry named `config.name` in place.
    ///
    /// Replacing keeps the entry's position, the comments attached to its header and the
    /// decoration of keys whose value did not change.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] (and leaves the document unchanged) if the result is invalid or
    /// `run_configurations` is not an array of tables.
    pub fn upsert_run_configuration(
        &mut self,
        config: &RunConfiguration,
    ) -> Result<(), ConfigError> {
        let name = config.name.clone();
        self.edit_run_configurations(|tables| {
            if let Some(table) = tables
                .iter_mut()
                .find(|table| table_name(table) == Some(name.as_str()))
            {
                write_run_configuration(table, config);
                return Ok(());
            }
            let mut table = Table::new();
            write_run_configuration(&mut table, config);
            tables.push(table);
            Ok(())
        })
    }

    /// Renames the run configuration `old_name` to `config.name` and replaces its fields.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] when `old_name` does not exist or the new name is
    /// already taken; the document is left unchanged.
    pub fn replace_run_configuration(
        &mut self,
        old_name: &str,
        config: &RunConfiguration,
    ) -> Result<(), ConfigError> {
        self.edit_run_configurations(|tables| {
            let table = tables
                .iter_mut()
                .find(|table| table_name(table) == Some(old_name))
                .ok_or_else(|| missing(old_name))?;
            write_run_configuration(table, config);
            Ok(())
        })
    }

    /// Removes the run configuration named `name`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] when no entry has that name.
    pub fn remove_run_configuration(&mut self, name: &str) -> Result<(), ConfigError> {
        self.edit_run_configurations(|tables| {
            let index = (0..tables.len())
                .find(|&idx| tables.get(idx).and_then(table_name) == Some(name))
                .ok_or_else(|| missing(name))?;
            tables.remove(index);
            Ok(())
        })
    }

    fn edit_run_configurations(
        &mut self,
        edit: impl FnOnce(&mut ArrayOfTables) -> Result<(), ConfigError>,
    ) -> Result<(), ConfigError> {
        let mut next = self.doc.clone();
        let item = next
            .entry(RUN_CONFIGURATIONS_KEY)
            .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
        let tables = item
            .as_array_of_tables_mut()
            .ok_or_else(|| ConfigError::InvalidPath {
                path: RUN_CONFIGURATIONS_KEY.to_owned(),
                reason: "`run_configurations` must be an array of tables ([[run_configurations]])"
                    .to_owned(),
            })?;
        edit(tables)?;
        if tables.is_empty() {
            next.remove(RUN_CONFIGURATIONS_KEY);
        }
        let config = deserialize_project(&next)?;
        self.doc = next;
        self.config = config;
        Ok(())
    }
}

fn missing(name: &str) -> ConfigError {
    ConfigError::Validation {
        field: "run_configurations.name",
        reason: format!("no run configuration named `{name}`"),
    }
}

fn table_name(table: &Table) -> Option<&str> {
    table.get("name").and_then(Item::as_str)
}

/// Writes `config` into `table`, only touching keys whose value changed.
fn write_run_configuration(table: &mut Table, config: &RunConfiguration) {
    set_str(table, "name", Some(&config.name));
    set_str(table, "kind", Some(config.kind.as_str()));
    set_str(table, "main_class", config.main_class.as_deref());
    set_str(table, "task", config.task.as_deref());
    set_list(table, "args", &config.args);
    set_list(table, "vm_options", &config.vm_options);
    set_str(table, "working_dir", config.working_dir.as_deref());
    set_env(table, &config.env);
}

fn set_str(table: &mut Table, key: &str, new: Option<&str>) {
    match new {
        Some(new) if table.get(key).and_then(Item::as_str) == Some(new) => {}
        Some(new) => {
            table.insert(key, value(new));
        }
        None => {
            table.remove(key);
        }
    }
}

fn set_list(table: &mut Table, key: &str, items: &[String]) {
    let current: Option<Vec<&str>> = table
        .get(key)
        .and_then(Item::as_array)
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect());
    if items.is_empty() {
        table.remove(key);
        return;
    }
    if current.is_some_and(|cur| cur.iter().copied().eq(items.iter().map(String::as_str))) {
        return;
    }
    let array: Array = items.iter().map(String::as_str).collect();
    table.insert(key, value(array));
}

fn set_env(table: &mut Table, env: &BTreeMap<String, String>) {
    let current: Option<BTreeMap<&str, &str>> = table.get("env").and_then(|item| match item {
        Item::Table(t) => Some(
            t.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k, s)))
                .collect(),
        ),
        Item::Value(toml_edit::Value::InlineTable(t)) => Some(
            t.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k, s)))
                .collect(),
        ),
        _ => None,
    });
    if env.is_empty() {
        table.remove("env");
        return;
    }
    if current.is_some_and(|cur| {
        cur.len() == env.len()
            && env
                .iter()
                .all(|(k, v)| cur.get(k.as_str()) == Some(&v.as_str()))
    }) {
        return;
    }
    let mut inline = InlineTable::new();
    for (key, val) in env {
        inline.insert(key, val.as_str().into());
    }
    table.insert("env", value(inline));
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEAM_TOML: &str = r#"# Team-shared Kova project settings
schema_version = 1

# Main application entry point
[[run_configurations]]
name = "App"   # keep this name stable
kind = "java_main"
main_class = "com.acme.App"
vm_options = ["-Xmx1G"]

[[run_configurations]]
name = "Unit tests"
kind = "gradle_task"
task = "test"
"#;

    #[test]
    fn upsert_appends_new_entry_and_keeps_comments() {
        let mut doc = ProjectConfigDocument::from_toml_str(TEAM_TOML).unwrap();
        let cfg = RunConfiguration::junit("OrderTest", "com.acme.OrderTest#creates");
        doc.upsert_run_configuration(&cfg).unwrap();

        let out = doc.to_toml_string();
        assert!(out.starts_with("# Team-shared Kova project settings\n"));
        assert!(out.contains("# Main application entry point\n[[run_configurations]]"));
        assert!(out.contains("name = \"App\"   # keep this name stable"));
        assert_eq!(doc.run_configurations().len(), 3);
        assert_eq!(doc.run_configurations()[2], cfg);

        let reparsed = ProjectConfigDocument::from_toml_str(&out).unwrap();
        assert_eq!(reparsed.config(), doc.config());
    }

    #[test]
    fn upsert_updates_in_place_preserving_unchanged_key_decor() {
        let mut doc = ProjectConfigDocument::from_toml_str(TEAM_TOML).unwrap();
        let mut app = doc.run_configuration("App").unwrap().clone();
        app.vm_options = vec!["-Xmx2G".to_owned(), "-ea".to_owned()];
        app.env
            .insert("SPRING_PROFILES_ACTIVE".to_owned(), "dev".to_owned());
        doc.upsert_run_configuration(&app).unwrap();

        let out = doc.to_toml_string();
        assert!(out.contains("name = \"App\"   # keep this name stable"));
        assert!(out.contains("vm_options = [\"-Xmx2G\", \"-ea\"]"));
        assert!(out.contains("SPRING_PROFILES_ACTIVE = \"dev\""));
        assert_eq!(doc.run_configurations()[0], app);
        assert_eq!(doc.run_configurations().len(), 2);
    }

    #[test]
    fn replace_renames_and_rejects_duplicates_atomically() {
        let mut doc = ProjectConfigDocument::from_toml_str(TEAM_TOML).unwrap();
        let mut tests = doc.run_configuration("Unit tests").unwrap().clone();
        tests.name = "App".to_owned();
        let before = doc.to_toml_string();
        assert!(doc.replace_run_configuration("Unit tests", &tests).is_err());
        assert_eq!(doc.to_toml_string(), before);

        tests.name = "All tests".to_owned();
        doc.replace_run_configuration("Unit tests", &tests).unwrap();
        assert!(doc.run_configuration("All tests").is_some());
        assert!(doc.run_configuration("Unit tests").is_none());
        assert!(doc.replace_run_configuration("missing", &tests).is_err());
    }

    #[test]
    fn remove_drops_entry_and_empty_array() {
        let mut doc = ProjectConfigDocument::from_toml_str(TEAM_TOML).unwrap();
        doc.remove_run_configuration("App").unwrap();
        assert_eq!(doc.run_configurations().len(), 1);
        assert!(doc.remove_run_configuration("App").is_err());
        doc.remove_run_configuration("Unit tests").unwrap();
        assert_eq!(doc.run_configurations().len(), 0);
        assert!(!doc.to_toml_string().contains("run_configurations"));
    }

    #[test]
    fn empty_document_roundtrips_and_rejects_blank_names() {
        let mut doc = ProjectConfigDocument::empty();
        let blank = RunConfiguration::junit("  ", "com.acme.T");
        assert!(doc.upsert_run_configuration(&blank).is_err());
        assert_eq!(doc.run_configurations().len(), 0);
        doc.upsert_run_configuration(&RunConfiguration::junit("T", "com.acme.T"))
            .unwrap();
        let reparsed = ProjectConfigDocument::from_toml_str(&doc.to_toml_string()).unwrap();
        assert_eq!(reparsed.run_configurations().len(), 1);
        assert_eq!(reparsed.config().schema_version, 1);
    }

    #[test]
    fn non_table_run_configurations_is_rejected() {
        let mut doc =
            ProjectConfigDocument::from_toml_str("schema_version = 1\nrun_configurations = []\n")
                .unwrap();
        let err = doc
            .upsert_run_configuration(&RunConfiguration::junit("T", "com.acme.T"))
            .unwrap_err();
        assert!(matches!(err, ConfigError::InvalidPath { .. }));
    }

    #[test]
    fn summary_prefers_task_then_main_class() {
        let cfg = RunConfiguration::junit("T", "com.acme.T#a");
        assert_eq!(cfg.summary(), "JUnit · com.acme.T#a");
    }
}
