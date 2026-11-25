//! Optional project-level `kova.toml` model, lossless editor, and
//! [`EffectiveConfig`] resolution (`KOVA-006`).
//!
//! Per Omarchy philosophy, Kova never creates `kova.toml` automatically in a
//! repository; it is loaded optionally when present and saved only upon explicit
//! user action.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::Path;

use serde::{Deserialize, Serialize};
use toml_edit::{DocumentMut, Value};

use crate::document::{apply_value_to_doc, parse_dotted_path, save_atomic};
use crate::model::{ConfigError, KovaSettings};
use crate::paths::KovaPaths;

/// Build tool preference declared in `kova.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProjectBuildTool {
    /// Detect Gradle or Maven automatically.
    #[default]
    Auto,
    /// Force Gradle / BSP integration.
    Gradle,
    /// Force Maven / BSP integration.
    Maven,
}

/// Kind of shared team run configuration in `[[run_configurations]]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunConfigKind {
    /// Standard Java `main` class execution.
    JavaMain,
    /// Spring Boot application launch.
    SpringBoot,
    /// Gradle task invocation.
    GradleTask,
    /// Maven goal invocation.
    MavenGoal,
    /// `JUnit` test suite or class target.
    Junit,
}

/// Shared project run configuration entry inside `kova.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunConfiguration {
    /// Display name shown in the Run Configuration picker.
    pub name: String,
    /// Runner kind (`java_main`, `spring_boot`, `gradle_task`, `maven_goal`, `junit`).
    pub kind: RunConfigKind,
    /// Fully qualified JVM main class (for `java_main` / `spring_boot` / `junit`).
    pub main_class: Option<String>,
    /// Program arguments passed to the application.
    #[serde(default)]
    pub args: Vec<String>,
    /// JVM VM options (for example `-Xmx1G`).
    #[serde(default)]
    pub vm_options: Vec<String>,
    /// Additional environment variables injected into the launch process.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Optional working directory relative to project root.
    pub working_dir: Option<String>,
    /// Optional Gradle task or Maven goal string.
    pub task: Option<String>,
}

/// Optional project-scoped editor overrides in `kova.toml` under `[editor]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ProjectEditorOverrides {
    /// Override `editor.tab_size` (`1..=16`).
    pub tab_size: Option<u8>,
    /// Override `editor.insert_spaces`.
    pub insert_spaces: Option<bool>,
    /// Override `editor.format_on_save`.
    pub format_on_save: Option<bool>,
}

/// Optional project-scoped JVM overrides in `kova.toml` under `[jvm]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ProjectJvmOverrides {
    /// Override `jvm.jdtls_max_ram_mb` (`256..=65536`).
    pub jdtls_max_ram_mb: Option<u32>,
    /// Override `jvm.auto_hibernate_minutes` (`0..=1440`).
    pub auto_hibernate_minutes: Option<u32>,
    /// Override `jvm.lombok_enabled`.
    pub lombok_enabled: Option<bool>,
}

/// Optional per-repository configuration loaded from `<project_root>/kova.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProjectConfig {
    /// Schema version (`1`).
    pub schema_version: u32,
    /// Project JDK requirement or identifier (for example `"21"` or `"temurin-21"`).
    pub jdk: Option<String>,
    /// Build tool selection (`auto`, `gradle`, `maven`).
    pub build_tool: Option<ProjectBuildTool>,
    /// Project-scoped editor overrides.
    pub editor: ProjectEditorOverrides,
    /// Project-scoped JVM overrides.
    pub jvm: ProjectJvmOverrides,
    /// Shared team run configurations (`[[run_configurations]]`).
    pub run_configurations: Vec<RunConfiguration>,
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            schema_version: 1,
            jdk: None,
            build_tool: None,
            editor: ProjectEditorOverrides::default(),
            jvm: ProjectJvmOverrides::default(),
            run_configurations: Vec::new(),
        }
    }
}

impl ProjectConfig {
    /// Parses and validates a `kova.toml` string.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::ParseToml`], [`ConfigError::Deserialize`], or
    /// [`ConfigError::Validation`] if the content is invalid.
    pub fn from_toml_str(input: &str) -> Result<Self, ConfigError> {
        let doc = input
            .parse::<DocumentMut>()
            .map_err(|err| ConfigError::ParseToml {
                message: err.to_string(),
            })?;
        let config: Self =
            toml_edit::de::from_document(doc).map_err(|err| ConfigError::Deserialize {
                message: err.to_string(),
            })?;
        config.validate()?;
        Ok(config)
    }

    /// Loads `<project_root>/kova.toml` if present, returning `Ok(None)` when
    /// the file does not exist without creating any file or hidden directory.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if the file exists but cannot be read, parsed,
    /// or validated.
    pub fn load_optional(project_root: &Path) -> Result<Option<Self>, ConfigError> {
        let path = KovaPaths::project_config_file(project_root);
        match std::fs::read_to_string(&path) {
            Ok(contents) => Self::from_toml_str(&contents).map(Some),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
            Err(err) => Err(ConfigError::Io {
                path: path.to_string_lossy().into_owned(),
                message: err.to_string(),
            }),
        }
    }

    /// Validates all overrides and run configurations inside [`ProjectConfig`].
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] if `schema_version`, override bounds,
    /// or run configuration names are invalid.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.schema_version != 1 {
            return Err(ConfigError::Validation {
                field: "schema_version",
                reason: format!(
                    "unsupported schema_version {}, expected 1",
                    self.schema_version
                ),
            });
        }
        if let Some(jdk) = &self.jdk
            && jdk.trim().is_empty()
        {
            return Err(ConfigError::Validation {
                field: "jdk",
                reason: "must not be whitespace-only when specified".to_owned(),
            });
        }
        if let Some(tab_size) = self.editor.tab_size
            && !(1..=16).contains(&tab_size)
        {
            return Err(ConfigError::Validation {
                field: "editor.tab_size",
                reason: format!("expected 1..=16, got {tab_size}"),
            });
        }
        if let Some(ram) = self.jvm.jdtls_max_ram_mb
            && !(256..=65536).contains(&ram)
        {
            return Err(ConfigError::Validation {
                field: "jvm.jdtls_max_ram_mb",
                reason: format!("expected 256..=65536 MiB, got {ram}"),
            });
        }
        if let Some(mins) = self.jvm.auto_hibernate_minutes
            && mins > 1440
        {
            return Err(ConfigError::Validation {
                field: "jvm.auto_hibernate_minutes",
                reason: format!("expected 0..=1440 minutes, got {mins}"),
            });
        }
        for run_cfg in &self.run_configurations {
            if run_cfg.name.trim().is_empty() {
                return Err(ConfigError::Validation {
                    field: "run_configurations.name",
                    reason: "run configuration name must not be empty".to_owned(),
                });
            }
        }
        Ok(())
    }
}

/// Lossless `toml_edit::DocumentMut` wrapper for optional `<project_root>/kova.toml`.
#[derive(Debug, Clone)]
pub struct ProjectConfigDocument {
    doc: DocumentMut,
    config: ProjectConfig,
}

impl ProjectConfigDocument {
    /// Parses `kova.toml` text into a lossless [`ProjectConfigDocument`].
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if parsing or validation fails.
    pub fn from_toml_str(input: &str) -> Result<Self, ConfigError> {
        let doc = input
            .parse::<DocumentMut>()
            .map_err(|err| ConfigError::ParseToml {
                message: err.to_string(),
            })?;
        let config = deserialize_project(&doc)?;
        Ok(Self { doc, config })
    }

    /// Loads `<project_root>/kova.toml` losslessly if it exists.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if the file exists but cannot be read or parsed.
    pub fn load_optional(project_root: &Path) -> Result<Option<Self>, ConfigError> {
        let path = KovaPaths::project_config_file(project_root);
        match std::fs::read_to_string(&path) {
            Ok(contents) => Self::from_toml_str(&contents).map(Some),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
            Err(err) => Err(ConfigError::Io {
                path: path.to_string_lossy().into_owned(),
                message: err.to_string(),
            }),
        }
    }

    /// Returns the validated [`ProjectConfig`] model.
    #[must_use]
    pub fn config(&self) -> &ProjectConfig {
        &self.config
    }

    /// Serializes the CST document to TOML preserving team comments and layout.
    #[must_use]
    pub fn to_toml_string(&self) -> String {
        self.doc.to_string()
    }

    /// Updates a scalar key in `kova.toml` preserving comments and formatting.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if the path or resulting config is invalid.
    pub fn set_dotted_value(&mut self, dotted_path: &str, value: Value) -> Result<(), ConfigError> {
        let segments = parse_dotted_path(dotted_path)?;
        let prev_doc = self.doc.clone();
        let prev_config = self.config.clone();

        if let Err(err) = apply_value_to_doc(&mut self.doc, &segments, value, dotted_path) {
            self.doc = prev_doc;
            self.config = prev_config;
            return Err(err);
        }

        match deserialize_project(&self.doc) {
            Ok(next) => {
                self.config = next;
                Ok(())
            }
            Err(err) => {
                self.doc = prev_doc;
                self.config = prev_config;
                Err(err)
            }
        }
    }

    /// Explicitly saves `kova.toml` to `<project_root>/kova.toml` atomically.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Io`] on filesystem failure.
    pub fn save_to_project(&self, project_root: &Path) -> Result<(), ConfigError> {
        let path = KovaPaths::project_config_file(project_root);
        save_atomic(&path, &self.to_toml_string())
    }
}

fn deserialize_project(doc: &DocumentMut) -> Result<ProjectConfig, ConfigError> {
    let config: ProjectConfig =
        toml_edit::de::from_document(doc.clone()).map_err(|err| ConfigError::Deserialize {
            message: err.to_string(),
        })?;
    config.validate()?;
    Ok(config)
}

/// Effective workspace configuration formed by merging user `settings.toml`
/// with optional project `kova.toml` overrides without mutating user dotfiles.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveConfig {
    /// Merged user + project settings.
    pub settings: KovaSettings,
    /// Effective build tool preference from `kova.toml`, if any.
    pub build_tool: Option<ProjectBuildTool>,
    /// Shared team run configurations from `kova.toml`.
    pub run_configurations: Vec<RunConfiguration>,
}

impl EffectiveConfig {
    /// Resolves effective settings by overlaying optional `project` overrides on
    /// top of a cloned `user` [`KovaSettings`].
    #[must_use]
    pub fn merge(user: &KovaSettings, project: Option<&ProjectConfig>) -> Self {
        let mut settings = user.clone();
        let Some(proj) = project else {
            return Self {
                settings,
                build_tool: None,
                run_configurations: Vec::new(),
            };
        };

        if let Some(jdk) = &proj.jdk {
            settings.jvm.preferred_jdk = Some(jdk.clone());
        }
        if let Some(tab_size) = proj.editor.tab_size {
            settings.editor.tab_size = tab_size;
        }
        if let Some(insert_spaces) = proj.editor.insert_spaces {
            settings.editor.insert_spaces = insert_spaces;
        }
        if let Some(format_on_save) = proj.editor.format_on_save {
            settings.editor.format_on_save = format_on_save;
        }
        if let Some(ram) = proj.jvm.jdtls_max_ram_mb {
            settings.jvm.jdtls_max_ram_mb = ram;
        }
        if let Some(mins) = proj.jvm.auto_hibernate_minutes {
            settings.jvm.auto_hibernate_minutes = mins;
        }
        if let Some(lombok) = proj.jvm.lombok_enabled {
            settings.jvm.lombok_enabled = lombok;
        }

        Self {
            settings,
            build_tool: proj.build_tool,
            run_configurations: proj.run_configurations.clone(),
        }
    }
}
