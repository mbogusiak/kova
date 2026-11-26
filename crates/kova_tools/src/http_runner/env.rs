//! `{{name}}` variable resolution for `.http` requests.
//!
//! Variable sources, lowest to highest precedence:
//! 1. `[http.environments.<env>]` in `kova.toml`;
//! 2. `http-client.env.json` next to the project root;
//! 3. `http-client.private.env.json` (secrets kept out of version control);
//! 4. file-level `@name = value` declarations in the `.http` file itself.
//!
//! The built-in dynamic variable `{{$timestamp}}` expands to the current Unix time in seconds.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use kova_config::ProjectConfig;

use crate::http_runner::error::HttpRunnerError;
use crate::http_runner::parser::{HttpHeader, HttpRequestSpec};

/// Public environment file name (`IntelliJ` HTTP Client convention).
pub const ENV_FILE_NAME: &str = "http-client.env.json";
/// Private environment file name (secrets, usually git-ignored).
pub const PRIVATE_ENV_FILE_NAME: &str = "http-client.private.env.json";

/// Walks up from `start` (a file or directory) to the first directory containing
/// `kova.toml`, `http-client.env.json` or `.git`; falls back to the starting directory.
#[must_use]
pub fn discover_project_root(start: &Path) -> std::path::PathBuf {
    let start_dir = if start.is_file() {
        start.parent().unwrap_or(start)
    } else {
        start
    };
    start_dir
        .ancestors()
        .find(|dir| {
            ["kova.toml", ENV_FILE_NAME, ".git"]
                .iter()
                .any(|marker| dir.join(marker).exists())
        })
        .unwrap_or(start_dir)
        .to_path_buf()
}

/// Named variable sets (`environment -> name -> value`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HttpEnvironments {
    /// Environment selected when no explicit name is requested.
    pub default_environment: Option<String>,
    /// Variable sets by environment name.
    pub environments: BTreeMap<String, BTreeMap<String, String>>,
}

impl HttpEnvironments {
    /// Parses an `http-client.env.json` document (`{"dev": {"baseUrl": "..."}}`).
    ///
    /// Non-string scalar values (numbers, booleans) are converted to their JSON text.
    ///
    /// # Errors
    ///
    /// Returns [`HttpRunnerError::Environment`] if the JSON is malformed or not an
    /// object of objects.
    pub fn from_env_json_str(json: &str, origin: &str) -> Result<Self, HttpRunnerError> {
        let err = |message: String| HttpRunnerError::Environment {
            path: origin.to_owned(),
            message,
        };
        let root: serde_json::Value = serde_json::from_str(json).map_err(|e| err(e.to_string()))?;
        let serde_json::Value::Object(envs) = root else {
            return Err(err("top-level value must be an object".to_owned()));
        };
        let mut out = Self::default();
        for (env_name, vars) in envs {
            let serde_json::Value::Object(vars) = vars else {
                return Err(err(format!("environment '{env_name}' must be an object")));
            };
            let entry = out.environments.entry(env_name).or_default();
            for (key, value) in vars {
                let text = match value {
                    serde_json::Value::String(s) => s,
                    serde_json::Value::Null => String::new(),
                    other @ (serde_json::Value::Bool(_)
                    | serde_json::Value::Number(_)
                    | serde_json::Value::Array(_)
                    | serde_json::Value::Object(_)) => other.to_string(),
                };
                entry.insert(key, text);
            }
        }
        Ok(out)
    }

    /// Builds environments from the `[http]` table of a parsed `kova.toml`.
    #[must_use]
    pub fn from_project_config(config: &ProjectConfig) -> Self {
        Self {
            default_environment: config.http.environment.clone(),
            environments: config.http.environments.clone(),
        }
    }

    /// Merges `other` into `self`; values from `other` win per variable.
    pub fn merge(&mut self, other: Self) {
        if other.default_environment.is_some() {
            self.default_environment = other.default_environment;
        }
        for (env, vars) in other.environments {
            self.environments.entry(env).or_default().extend(vars);
        }
    }

    /// Loads and merges `kova.toml`, `http-client.env.json` and
    /// `http-client.private.env.json` from `project_root` (all optional).
    ///
    /// # Errors
    ///
    /// Returns [`HttpRunnerError::Environment`] if a present file cannot be read or parsed.
    pub fn load_from_dir(project_root: &Path) -> Result<Self, HttpRunnerError> {
        let mut envs = match ProjectConfig::load_optional(project_root) {
            Ok(Some(config)) => Self::from_project_config(&config),
            Ok(None) => Self::default(),
            Err(e) => {
                return Err(HttpRunnerError::Environment {
                    path: project_root.join("kova.toml").display().to_string(),
                    message: e.to_string(),
                });
            }
        };
        for file_name in [ENV_FILE_NAME, PRIVATE_ENV_FILE_NAME] {
            let path = project_root.join(file_name);
            match std::fs::read_to_string(&path) {
                Ok(json) => {
                    envs.merge(Self::from_env_json_str(&json, &path.display().to_string())?);
                }
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(HttpRunnerError::Environment {
                        path: path.display().to_string(),
                        message: e.to_string(),
                    });
                }
            }
        }
        Ok(envs)
    }

    /// Returns the environment names in sorted order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.environments.keys().map(String::as_str)
    }

    /// Resolves the variables of `name`, falling back to the default environment, or to
    /// the only declared environment when exactly one exists.
    ///
    /// # Errors
    ///
    /// Returns [`HttpRunnerError::UnknownEnvironment`] when `name` (or the default) is
    /// not declared.
    pub fn variables_for(&self, name: Option<&str>) -> Result<HttpVariables, HttpRunnerError> {
        let selected = name
            .map(str::to_owned)
            .or_else(|| self.default_environment.clone())
            .or_else(|| {
                (self.environments.len() == 1)
                    .then(|| self.environments.keys().next().cloned())
                    .flatten()
            });
        let Some(selected) = selected else {
            return Ok(HttpVariables::default());
        };
        self.environments
            .get(&selected)
            .map(|vars| HttpVariables::from_map(vars.clone()))
            .ok_or(HttpRunnerError::UnknownEnvironment { name: selected })
    }
}

/// Flat variable map used for `{{name}}` substitution.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HttpVariables {
    values: BTreeMap<String, String>,
}

/// A request with every placeholder substituted, ready to execute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedHttpRequest {
    /// Uppercase HTTP method.
    pub method: String,
    /// Fully resolved URL.
    pub url: String,
    /// Resolved headers in declaration order.
    pub headers: Vec<HttpHeader>,
    /// Resolved body.
    pub body: Option<String>,
}

impl HttpVariables {
    /// Creates a variable set from `values`.
    #[must_use]
    pub const fn from_map(values: BTreeMap<String, String>) -> Self {
        Self { values }
    }

    /// Inserts or replaces `name`.
    pub fn insert(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.values.insert(name.into(), value.into());
    }

    /// Overlays `other` on top of `self` (values from `other` win).
    pub fn extend(&mut self, other: impl IntoIterator<Item = (String, String)>) {
        self.values.extend(other);
    }

    /// Returns the value of `name`, if declared.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    /// Substitutes every `{{ name }}` placeholder in `input`.
    ///
    /// Variable values may themselves reference other variables (up to 8 levels deep).
    /// An unterminated `{{` is kept literally.
    ///
    /// # Errors
    ///
    /// Returns [`HttpRunnerError::UnresolvedVariable`] for the first unknown name.
    pub fn substitute(&self, input: &str) -> Result<String, HttpRunnerError> {
        self.substitute_depth(input, 0)
    }

    fn substitute_depth(&self, input: &str, depth: u8) -> Result<String, HttpRunnerError> {
        const MAX_DEPTH: u8 = 8;
        let mut out = String::with_capacity(input.len());
        let mut rest = input;
        while let Some(start) = rest.find("{{") {
            let after = &rest[start + 2..];
            let Some(end) = after.find("}}") else {
                break;
            };
            out.push_str(&rest[..start]);
            let name = after[..end].trim();
            let value = self.lookup(name)?;
            if depth < MAX_DEPTH && value.contains("{{") {
                out.push_str(&self.substitute_depth(&value, depth + 1)?);
            } else {
                out.push_str(&value);
            }
            rest = &after[end + 2..];
        }
        out.push_str(rest);
        Ok(out)
    }

    fn lookup(&self, name: &str) -> Result<String, HttpRunnerError> {
        if name == "$timestamp" {
            let secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            return Ok(secs.to_string());
        }
        self.values
            .get(name)
            .cloned()
            .ok_or_else(|| HttpRunnerError::UnresolvedVariable {
                name: name.to_owned(),
            })
    }

    /// Substitutes placeholders in the URL, header values and body of `spec`.
    ///
    /// # Errors
    ///
    /// Returns [`HttpRunnerError::UnresolvedVariable`] for any unknown placeholder.
    pub fn resolve(&self, spec: &HttpRequestSpec) -> Result<ResolvedHttpRequest, HttpRunnerError> {
        let headers = spec
            .headers
            .iter()
            .map(|h| {
                Ok(HttpHeader::new(
                    self.substitute(&h.name)?,
                    self.substitute(&h.value)?,
                ))
            })
            .collect::<Result<Vec<_>, HttpRunnerError>>()?;
        Ok(ResolvedHttpRequest {
            method: spec.method.clone(),
            url: self.substitute(&spec.url)?,
            headers,
            body: spec
                .body
                .as_deref()
                .map(|b| self.substitute(b))
                .transpose()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutes_trimmed_and_nested_placeholders() {
        let mut vars = HttpVariables::default();
        vars.insert("host", "localhost");
        vars.insert("baseUrl", "http://{{host}}:8080");
        assert_eq!(
            vars.substitute("{{ baseUrl }}/a?x={{host}}")
                .ok()
                .as_deref(),
            Some("http://localhost:8080/a?x=localhost")
        );
    }

    #[test]
    fn unknown_placeholder_is_an_error_and_unterminated_is_literal() {
        let vars = HttpVariables::default();
        assert_eq!(
            vars.substitute("{{missing}}"),
            Err(HttpRunnerError::UnresolvedVariable {
                name: "missing".to_owned()
            })
        );
        assert_eq!(vars.substitute("a {{b").ok().as_deref(), Some("a {{b"));
    }

    #[test]
    fn self_reference_terminates() {
        let mut vars = HttpVariables::default();
        vars.insert("loop", "{{loop}}");
        assert!(vars.substitute("{{loop}}").is_ok());
    }

    #[test]
    fn timestamp_is_numeric() {
        let out = HttpVariables::default().substitute("{{$timestamp}}");
        assert!(out.is_ok_and(|s| s.parse::<u64>().is_ok()));
    }

    #[test]
    fn env_json_parses_scalars_and_rejects_bad_shapes() {
        let envs = HttpEnvironments::from_env_json_str(
            r#"{"dev": {"baseUrl": "http://d", "port": 8080, "debug": true}}"#,
            "mem",
        );
        let Ok(envs) = envs else {
            panic!("valid env json must parse");
        };
        let vars = envs.variables_for(None).unwrap_or_default();
        assert_eq!(vars.get("port"), Some("8080"));
        assert_eq!(vars.get("debug"), Some("true"));
        assert!(HttpEnvironments::from_env_json_str("[]", "mem").is_err());
        assert!(HttpEnvironments::from_env_json_str(r#"{"dev": 1}"#, "mem").is_err());
    }

    #[test]
    fn discovers_root_by_marker_files() {
        let Ok(dir) = tempfile::tempdir() else {
            panic!("tempdir");
        };
        let nested = dir.path().join("api/requests");
        assert!(std::fs::create_dir_all(&nested).is_ok());
        let file = nested.join("users.http");
        assert!(std::fs::write(&file, "GET http://x\n").is_ok());
        assert_eq!(discover_project_root(&file), nested);
        assert!(std::fs::write(dir.path().join(ENV_FILE_NAME), "{}").is_ok());
        assert_eq!(discover_project_root(&file), dir.path());
    }

    #[test]
    fn unknown_environment_is_reported() {
        let envs = HttpEnvironments::default();
        assert!(envs.variables_for(None).is_ok());
        assert_eq!(
            envs.variables_for(Some("prod")),
            Err(HttpRunnerError::UnknownEnvironment {
                name: "prod".to_owned()
            })
        );
    }
}
