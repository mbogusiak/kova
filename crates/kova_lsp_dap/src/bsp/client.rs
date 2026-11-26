//! Asynchronous Build Server Protocol client for Gradle / Maven BSP servers (`KOVA-040`).
//!
//! BSP uses the same `Content-Length` framed JSON-RPC 2.0 as LSP, so the client reuses
//! [`LspSession`]. Notifications are mapped synchronously in the session's notification hook by
//! a [`BspEventMapper`] and pushed into an **unbounded** channel, so a long test run never drops
//! `test-finish` events the way a lagging `broadcast` receiver could.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kova_toolchain::{ToolchainEnvOptions, resolve_executable, resolve_toolchain_env};
use kova_tools::junit::TestId;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use crate::bsp::events::{BspEvent, BspEventMapper, BspStatusCode};
use crate::error::LspDapError;
use crate::lsp::diagnostics::DiagnosticsStore;
use crate::lsp::session::{LspSession, NotificationHook, SessionOptions};
use crate::lsp::sync::path_to_lsp_uri;

/// BSP protocol version announced in `build/initialize`.
pub const BSP_VERSION: &str = "2.1.0";
/// Timeout of short requests (`build/initialize`, `workspace/buildTargets`, `build/shutdown`).
pub const BSP_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// Timeout of `buildTarget/compile` and `buildTarget/test` (whole test suites).
pub const BSP_LONG_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// Connection file `.bsp/<name>.json` describing how to start a BSP server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BspConnectionDetails {
    /// Server name (`gradle-bsp`, `bazelbsp`, …).
    pub name: String,
    /// Command line starting the server on stdio.
    pub argv: Vec<String>,
    /// Server version.
    #[serde(default)]
    pub version: String,
    /// Supported BSP version.
    #[serde(default)]
    pub bsp_version: String,
    /// Supported language ids.
    #[serde(default)]
    pub languages: Vec<String>,
}

impl BspConnectionDetails {
    /// Reads every valid `<project_root>/.bsp/*.json` connection file, sorted by file name.
    ///
    /// Files that fail to parse or have an empty `argv` are skipped.
    #[must_use]
    pub fn discover(project_root: &Path) -> Vec<Self> {
        let Ok(entries) = std::fs::read_dir(project_root.join(".bsp")) else {
            return Vec::new();
        };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
            })
            .collect();
        files.sort();
        files
            .into_iter()
            .filter_map(|path| std::fs::read_to_string(path).ok())
            .filter_map(|text| serde_json::from_str::<Self>(&text).ok())
            .filter(|details| !details.argv.is_empty())
            .collect()
    }

    /// Picks the connection for a JVM project: one declaring `java`/`kotlin`, else the first.
    #[must_use]
    pub fn discover_jvm(project_root: &Path) -> Option<Self> {
        let all = Self::discover(project_root);
        all.iter()
            .find(|d| {
                d.languages
                    .iter()
                    .any(|l| l == "java" || l == "kotlin" || l == "scala")
            })
            .cloned()
            .or_else(|| all.into_iter().next())
    }
}

/// `BuildTargetIdentifier`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BuildTargetIdentifier {
    /// Target URI.
    pub uri: String,
}

/// `BuildTargetCapabilities`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "mirrors the BSP BuildTargetCapabilities wire type"
)]
pub struct BuildTargetCapabilities {
    /// Supports `buildTarget/compile`.
    pub can_compile: bool,
    /// Supports `buildTarget/test`.
    pub can_test: bool,
    /// Supports `buildTarget/run`.
    pub can_run: bool,
    /// Supports `debugSession/start`.
    pub can_debug: bool,
}

/// `BuildTarget` returned by `workspace/buildTargets`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildTarget {
    /// Identifier.
    pub id: BuildTargetIdentifier,
    /// Human-readable name.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Directory URI of the target sources.
    #[serde(default)]
    pub base_directory: Option<String>,
    /// Tags (`library`, `application`, `test`, …).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Language ids.
    #[serde(default)]
    pub language_ids: Vec<String>,
    /// Capabilities.
    #[serde(default)]
    pub capabilities: BuildTargetCapabilities,
}

impl BuildTarget {
    /// Display name or the URI.
    #[must_use]
    pub fn name(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.id.uri)
    }
}

/// Picks the test-capable targets that should run tests for `file` (when given).
///
/// Targets whose `baseDirectory` is the longest prefix of the file URI win; targets tagged
/// `test` are preferred; with no file every test-capable target is returned.
#[must_use]
pub fn select_test_targets(
    targets: &[BuildTarget],
    file_uri: Option<&str>,
) -> Vec<BuildTargetIdentifier> {
    let testable: Vec<&BuildTarget> = targets.iter().filter(|t| t.capabilities.can_test).collect();
    let tagged: Vec<&BuildTarget> = testable
        .iter()
        .copied()
        .filter(|t| t.tags.iter().any(|tag| tag == "test"))
        .collect();
    let pool = if tagged.is_empty() { testable } else { tagged };
    if let Some(file) = file_uri {
        let owners: Vec<(usize, &BuildTarget)> = pool
            .iter()
            .filter_map(|t| {
                let base = t.base_directory.as_deref()?.trim_end_matches('/');
                file.starts_with(&format!("{base}/"))
                    .then_some((base.len(), *t))
            })
            .collect();
        if let Some(best) = owners.iter().map(|(len, _)| *len).max() {
            return owners
                .into_iter()
                .filter(|(len, _)| *len == best)
                .map(|(_, t)| t.id.clone())
                .collect();
        }
    }
    pool.into_iter().map(|t| t.id.clone()).collect()
}

/// Parameters of one `buildTarget/test` request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BspTestParams {
    /// Build targets to test.
    pub targets: Vec<BuildTargetIdentifier>,
    /// Classes / methods to select (empty = all tests of the targets).
    pub selection: Vec<TestId>,
    /// Extra JVM options.
    pub jvm_options: Vec<String>,
    /// Extra environment variables.
    pub env: BTreeMap<String, String>,
    /// Working directory.
    pub working_dir: Option<PathBuf>,
}

/// Builds the JSON params of `buildTarget/test`.
///
/// The class / method selection uses the `scala-test-suites-selection` data kind (BSP 2.1),
/// which JVM BSP servers use for `JUnit` filtering; servers that ignore it run whole targets and
/// the runner still shows every reported test.
#[must_use]
pub fn test_params_json(params: &BspTestParams, origin_id: &str) -> Value {
    let env: Vec<String> = params.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let mut value = json!({
        "targets": params.targets,
        "originId": origin_id,
    });
    if !params.env.is_empty() {
        value["environmentVariables"] = json!(params.env);
    }
    if let Some(dir) = &params.working_dir {
        value["workingDirectory"] = json!(dir.display().to_string());
    }
    if !params.selection.is_empty() {
        let mut suites: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for id in &params.selection {
            let tests = suites.entry(id.class_name.as_str()).or_default();
            if let Some(method) = &id.method {
                tests.push(method);
            }
        }
        let suites: Vec<Value> = suites
            .into_iter()
            .map(|(class, tests)| json!({ "className": class, "tests": tests }))
            .collect();
        value["dataKind"] = json!("scala-test-suites-selection");
        value["data"] = json!({
            "suites": suites,
            "jvmOptions": params.jvm_options,
            "environmentVariables": env,
        });
    }
    value
}

/// Connected BSP client.
pub struct BspClient {
    session: LspSession,
    child: tokio::sync::Mutex<Option<Child>>,
    mapper: Arc<Mutex<BspEventMapper>>,
    events: Mutex<Option<mpsc::UnboundedReceiver<BspEvent>>>,
    diagnostics: Arc<DiagnosticsStore>,
    capabilities: Value,
    server_name: String,
    project_root: PathBuf,
    next_origin: AtomicU64,
}

impl std::fmt::Debug for BspClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BspClient")
            .field("server_name", &self.server_name)
            .field("project_root", &self.project_root)
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

impl BspClient {
    /// Spawns the server described by `details` (`kill_on_drop`) and performs
    /// `build/initialize` + `build/initialized`.
    ///
    /// # Errors
    /// [`LspDapError::SpawnFailed`] or any handshake error (the process is killed then).
    pub async fn connect(
        details: &BspConnectionDetails,
        project_root: &Path,
    ) -> Result<Self, LspDapError> {
        let program = details.argv.first().cloned().unwrap_or_default();
        let toolchain_env = resolve_toolchain_env(&ToolchainEnvOptions {
            project_root: Some(project_root.to_path_buf()),
            ..ToolchainEnvOptions::default()
        });
        let resolved_program = resolve_executable(Path::new(&program), &toolchain_env.path);
        let spawn_failed = |reason: String| LspDapError::SpawnFailed {
            program: program.clone(),
            reason,
        };
        let mut child = Command::new(&resolved_program)
            .args(details.argv.iter().skip(1))
            .envs(&toolchain_env.vars)
            .current_dir(project_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| spawn_failed(err.to_string()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| spawn_failed("stdin pipe unavailable".to_owned()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| spawn_failed("stdout pipe unavailable".to_owned()))?;
        match Self::start_with_streams(stdout, stdin, project_root).await {
            Ok(client) => {
                *client.child.lock().await = Some(child);
                Ok(client)
            }
            Err(err) => {
                let _ignored = child.kill().await;
                Err(err)
            }
        }
    }

    /// Performs the handshake over arbitrary streams (tests use `tokio::io::duplex`).
    ///
    /// # Errors
    /// Any error of `build/initialize` or `build/initialized`.
    pub async fn start_with_streams<R, W>(
        reader: R,
        writer: W,
        project_root: &Path,
    ) -> Result<Self, LspDapError>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (tx, rx) = mpsc::unbounded_channel();
        let mapper = Arc::new(Mutex::new(BspEventMapper::new()));
        let diagnostics = Arc::new(DiagnosticsStore::new());
        let hook: NotificationHook = {
            let mapper = Arc::clone(&mapper);
            let diagnostics = Arc::clone(&diagnostics);
            let accumulated: Mutex<HashMap<String, Vec<lsp_types::Diagnostic>>> =
                Mutex::new(HashMap::new());
            Arc::new(move |notification| {
                let events = mapper
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .map(notification);
                for event in events {
                    if let BspEvent::Diagnostics {
                        uri,
                        diagnostics: items,
                        reset,
                        ..
                    } = &event
                    {
                        let mut acc = accumulated.lock().unwrap_or_else(PoisonError::into_inner);
                        apply_diagnostics(&diagnostics, &mut acc, uri, items, *reset);
                    }
                    drop(tx.send(event));
                }
            })
        };
        let session = LspSession::from_streams_with_options(
            reader,
            writer,
            SessionOptions {
                notification_hook: Some(hook),
                ..SessionOptions::default()
            },
        );
        let root_uri = path_to_lsp_uri(project_root)?;
        let init: Value = session
            .request_value(
                "build/initialize",
                json!({
                    "displayName": "Kova",
                    "version": env!("CARGO_PKG_VERSION"),
                    "bspVersion": BSP_VERSION,
                    "rootUri": root_uri.as_str(),
                    "capabilities": { "languageIds": ["java", "kotlin"] },
                }),
                BSP_REQUEST_TIMEOUT,
            )
            .await?;
        session.notify("build/initialized", Value::Null).await?;
        Ok(Self {
            session,
            child: tokio::sync::Mutex::new(None),
            mapper,
            events: Mutex::new(Some(rx)),
            diagnostics,
            server_name: init["displayName"].as_str().unwrap_or("BSP").to_owned(),
            capabilities: init["capabilities"].clone(),
            project_root: project_root.to_path_buf(),
            next_origin: AtomicU64::new(1),
        })
    }

    /// Server display name from `build/initialize`.
    #[must_use]
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// Raw `BuildServerCapabilities`.
    #[must_use]
    pub const fn capabilities(&self) -> &Value {
        &self.capabilities
    }

    /// Diagnostics published by the server (`build/publishDiagnostics`), with `reset: false`
    /// batches appended to the previous ones.
    #[must_use]
    pub const fn diagnostics(&self) -> &Arc<DiagnosticsStore> {
        &self.diagnostics
    }

    /// Takes the event stream (once).
    #[must_use]
    pub fn take_events(&self) -> Option<mpsc::UnboundedReceiver<BspEvent>> {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    /// `workspace/buildTargets`; also registers target names as test suite names.
    ///
    /// # Errors
    /// Request or deserialization errors.
    pub async fn build_targets(&self) -> Result<Vec<BuildTarget>, LspDapError> {
        let result = self
            .session
            .request_value("workspace/buildTargets", Value::Null, BSP_REQUEST_TIMEOUT)
            .await?;
        let targets: Vec<BuildTarget> = serde_json::from_value(result["targets"].clone())?;
        self.mapper
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .set_target_names(
                targets
                    .iter()
                    .map(|t| (t.id.uri.clone(), t.name().to_owned())),
            );
        Ok(targets)
    }

    /// `buildTarget/compile`.
    ///
    /// # Errors
    /// Request errors.
    pub async fn compile(
        &self,
        targets: &[BuildTargetIdentifier],
    ) -> Result<BspStatusCode, LspDapError> {
        let origin = self.origin_id();
        let result = self
            .session
            .request_value(
                "buildTarget/compile",
                json!({ "targets": targets, "originId": origin }),
                BSP_LONG_TIMEOUT,
            )
            .await?;
        Ok(BspStatusCode::from_code(
            result["statusCode"].as_i64().unwrap_or(2),
        ))
    }

    /// `buildTarget/test`; test events stream through [`Self::take_events`] meanwhile.
    ///
    /// # Errors
    /// Request errors.
    pub async fn test(&self, params: &BspTestParams) -> Result<BspStatusCode, LspDapError> {
        let origin = self.origin_id();
        let result = self
            .session
            .request_value(
                "buildTarget/test",
                test_params_json(params, &origin),
                BSP_LONG_TIMEOUT,
            )
            .await?;
        Ok(BspStatusCode::from_code(
            result["statusCode"].as_i64().unwrap_or(2),
        ))
    }

    /// `build/shutdown` + `build/exit`, then waits briefly for the process to exit.
    ///
    /// # Errors
    /// Request errors of `build/shutdown`.
    pub async fn shutdown(&self) -> Result<(), LspDapError> {
        self.session
            .request_value("build/shutdown", Value::Null, BSP_REQUEST_TIMEOUT)
            .await?;
        self.session.notify("build/exit", Value::Null).await?;
        if let Some(mut child) = self.child.lock().await.take() {
            if tokio::time::timeout(Duration::from_secs(5), child.wait())
                .await
                .is_err()
            {
                let _ignored = child.kill().await;
            }
        }
        Ok(())
    }

    fn origin_id(&self) -> String {
        format!("kova-{}", self.next_origin.fetch_add(1, Ordering::Relaxed))
    }
}

fn apply_diagnostics(
    store: &DiagnosticsStore,
    accumulated: &mut HashMap<String, Vec<lsp_types::Diagnostic>>,
    uri: &str,
    items: &[lsp_types::Diagnostic],
    reset: bool,
) {
    let Ok(parsed) = uri.parse::<lsp_types::Uri>() else {
        return;
    };
    let entry = accumulated.entry(uri.to_owned()).or_default();
    if reset {
        entry.clear();
    }
    entry.extend(items.iter().cloned());
    store.apply_publish_diagnostics(lsp_types::PublishDiagnosticsParams {
        uri: parsed,
        diagnostics: entry.clone(),
        version: None,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(uri: &str, base: &str, tags: &[&str], can_test: bool) -> BuildTarget {
        BuildTarget {
            id: BuildTargetIdentifier {
                uri: uri.to_owned(),
            },
            display_name: Some(uri.to_owned()),
            base_directory: Some(base.to_owned()),
            tags: tags.iter().map(|t| (*t).to_owned()).collect(),
            language_ids: vec!["java".to_owned()],
            capabilities: BuildTargetCapabilities {
                can_compile: true,
                can_test,
                can_run: false,
                can_debug: false,
            },
        }
    }

    #[test]
    fn selects_the_module_owning_the_file() {
        let targets = [
            target("app-main", "file:///p/app", &["library"], false),
            target("app-test", "file:///p/app", &["test"], true),
            target("core-test", "file:///p/core", &["test"], true),
            target("root-test", "file:///p", &["test"], true),
        ];
        let picked = select_test_targets(&targets, Some("file:///p/core/src/test/java/A.java"));
        assert_eq!(
            picked,
            vec![BuildTargetIdentifier {
                uri: "core-test".to_owned()
            }]
        );
        let all = select_test_targets(&targets, None);
        assert_eq!(all.len(), 3);
        let outside = select_test_targets(&targets, Some("file:///elsewhere/A.java"));
        assert_eq!(outside.len(), 3);
    }

    #[test]
    fn test_params_carry_the_method_selection() {
        let params = BspTestParams {
            targets: vec![BuildTargetIdentifier {
                uri: "t".to_owned(),
            }],
            selection: vec![
                TestId::method("a.OrderTest", "creates"),
                TestId::method("a.OrderTest", "rejects"),
                TestId::class("a.OtherTest"),
            ],
            jvm_options: vec!["-ea".to_owned()],
            env: BTreeMap::from([("K".to_owned(), "V".to_owned())]),
            working_dir: None,
        };
        let value = test_params_json(&params, "kova-1");
        assert_eq!(value["originId"], "kova-1");
        assert_eq!(value["dataKind"], "scala-test-suites-selection");
        assert_eq!(value["data"]["suites"][0]["className"], "a.OrderTest");
        assert_eq!(
            value["data"]["suites"][0]["tests"],
            json!(["creates", "rejects"])
        );
        assert_eq!(value["data"]["suites"][1]["tests"], json!([]));
        assert_eq!(value["data"]["environmentVariables"], json!(["K=V"]));
        assert_eq!(value["environmentVariables"]["K"], "V");

        let whole = test_params_json(
            &BspTestParams {
                targets: params.targets.clone(),
                ..BspTestParams::default()
            },
            "kova-2",
        );
        assert!(whole.get("dataKind").is_none());
    }

    #[test]
    fn discovers_connection_files() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(BspConnectionDetails::discover_jvm(tmp.path()).is_none());
        let bsp = tmp.path().join(".bsp");
        std::fs::create_dir_all(&bsp).unwrap();
        std::fs::write(bsp.join("a-broken.json"), "{").unwrap();
        std::fs::write(
            bsp.join("b-other.json"),
            r#"{"name":"other","argv":["x"],"version":"1","bspVersion":"2.1.0","languages":["rust"]}"#,
        )
        .unwrap();
        std::fs::write(
            bsp.join("c-gradle.json"),
            r#"{"name":"gradle-bsp","argv":["java","-jar","bsp.jar"],"version":"1","bspVersion":"2.1.0","languages":["java","kotlin"]}"#,
        )
        .unwrap();
        assert_eq!(BspConnectionDetails::discover(tmp.path()).len(), 2);
        let jvm = BspConnectionDetails::discover_jvm(tmp.path()).unwrap();
        assert_eq!(jvm.name, "gradle-bsp");
        assert_eq!(jvm.argv[0], "java");
    }
}
