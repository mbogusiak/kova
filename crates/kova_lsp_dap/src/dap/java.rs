//! `java-debug` specifics (`KOVA-041`): `launch` / `attach` arguments and the `jdt.ls`
//! commands that start the adapter and resolve the classpath.
//!
//! `java-debug` is not a standalone executable: it is an `OSGi` bundle loaded into `jdt.ls`
//! (`initializationOptions.bundles`, see `kova_toolchain::build_jdtls_launch_spec`). The
//! `workspace/executeCommand` `vscode.java.startDebugSession` opens a DAP server socket and
//! returns its port, to which [`crate::dap::DapClient::connect_tcp`] connects.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::{Value, json};

use crate::dap::session::DebugTarget;
use crate::error::LspDapError;
use crate::lsp::JdtlsClient;

/// `jdt.ls` command starting a `java-debug` DAP server; returns its TCP port.
pub const START_DEBUG_SESSION_COMMAND: &str = "vscode.java.startDebugSession";
/// `jdt.ls` command resolving `[classPaths, modulePaths]` of a main class.
pub const RESOLVE_CLASSPATH_COMMAND: &str = "vscode.java.resolveClasspath";
/// `jdt.ls` command listing the main classes of the workspace.
pub const RESOLVE_MAIN_CLASS_COMMAND: &str = "vscode.java.resolveMainClass";

/// `launch` configuration of a JVM program or test run under `java-debug`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct JavaLaunchConfig {
    /// Fully qualified main class (`com.acme.App`, or a `JUnit` launcher).
    pub main_class: String,
    /// `jdt.ls` project name (disambiguates multi-module workspaces).
    pub project_name: Option<String>,
    /// Classpath entries.
    pub class_paths: Vec<String>,
    /// Module path entries.
    pub module_paths: Vec<String>,
    /// JVM options (`-Xmx512m -Dfoo=bar`).
    pub vm_args: Vec<String>,
    /// Program arguments.
    pub args: Vec<String>,
    /// Working directory.
    pub cwd: Option<PathBuf>,
    /// Extra environment variables.
    pub env: BTreeMap<String, String>,
    /// JDK used to run the debuggee (`JAVA_HOME`).
    pub java_home: Option<PathBuf>,
    /// Suspend on the first line of `main`.
    pub stop_on_entry: bool,
}

impl JavaLaunchConfig {
    /// `launch` arguments understood by `java-debug`.
    #[must_use]
    pub fn arguments(&self) -> Value {
        let mut args = json!({
            "type": "java",
            "request": "launch",
            "name": format!("Debug {}", self.main_class),
            "mainClass": self.main_class,
            "classPaths": self.class_paths,
            "modulePaths": self.module_paths,
            "vmArgs": self.vm_args.join(" "),
            "args": self.args.join(" "),
            "env": self.env,
            "console": "internalConsole",
            "stopOnEntry": self.stop_on_entry,
            "noDebug": false,
        });
        if let Some(project) = &self.project_name {
            args["projectName"] = json!(project);
        }
        if let Some(cwd) = &self.cwd {
            args["cwd"] = json!(cwd.display().to_string());
        }
        if let Some(java_home) = &self.java_home {
            args["javaExec"] = json!(java_home.join("bin").join("java").display().to_string());
        }
        args
    }

    /// As a [`DebugTarget::Launch`].
    #[must_use]
    pub fn target(&self) -> DebugTarget {
        DebugTarget::Launch(self.arguments())
    }
}

/// `attach` configuration for a JVM started with
/// `-agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=<port>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaAttachConfig {
    /// JDWP host.
    pub host_name: String,
    /// JDWP port.
    pub port: u16,
    /// `jdt.ls` project name used to resolve sources.
    pub project_name: Option<String>,
    /// Attach timeout in milliseconds.
    pub timeout_ms: u64,
}

impl JavaAttachConfig {
    /// Attach to `localhost:<port>` with a 30 s timeout.
    #[must_use]
    pub fn localhost(port: u16) -> Self {
        Self {
            host_name: "localhost".to_owned(),
            port,
            project_name: None,
            timeout_ms: 30_000,
        }
    }

    /// `attach` arguments understood by `java-debug`.
    #[must_use]
    pub fn arguments(&self) -> Value {
        let mut args = json!({
            "type": "java",
            "request": "attach",
            "name": format!("Attach {}:{}", self.host_name, self.port),
            "hostName": self.host_name,
            "port": self.port,
            "timeout": self.timeout_ms,
        });
        if let Some(project) = &self.project_name {
            args["projectName"] = json!(project);
        }
        args
    }

    /// As a [`DebugTarget::Attach`].
    #[must_use]
    pub fn target(&self) -> DebugTarget {
        DebugTarget::Attach(self.arguments())
    }
}

/// Parses the result of [`START_DEBUG_SESSION_COMMAND`] (a port number).
///
/// # Errors
/// [`LspDapError::InvalidArgument`] when the result is not a valid TCP port.
pub fn parse_debug_port(result: &Value) -> Result<u16, LspDapError> {
    result
        .as_u64()
        .or_else(|| result.as_str().and_then(|s| s.trim().parse().ok()))
        .and_then(|port| u16::try_from(port).ok())
        .filter(|port| *port != 0)
        .ok_or_else(|| LspDapError::InvalidArgument {
            reason: format!("`{START_DEBUG_SESSION_COMMAND}` returned no port: {result}"),
        })
}

/// Asks `jdt.ls` (with the `java-debug` bundle loaded) to open a DAP server and returns its port.
///
/// # Errors
/// Transport and protocol failures, or [`LspDapError::InvalidArgument`] when the bundle is
/// missing and the command returns no port.
pub async fn start_java_debug_adapter(client: &JdtlsClient) -> Result<u16, LspDapError> {
    let result: Value = client
        .request(
            "workspace/executeCommand",
            json!({ "command": START_DEBUG_SESSION_COMMAND, "arguments": [] }),
        )
        .await?;
    parse_debug_port(&result)
}

/// Resolves `(classPaths, modulePaths)` of `main_class` through `jdt.ls`.
///
/// # Errors
/// Transport and protocol failures.
pub async fn resolve_classpath(
    client: &JdtlsClient,
    main_class: &str,
    project_name: Option<&str>,
) -> Result<(Vec<String>, Vec<String>), LspDapError> {
    let result: Value = client
        .request(
            "workspace/executeCommand",
            json!({ "command": RESOLVE_CLASSPATH_COMMAND, "arguments": [main_class, project_name] }),
        )
        .await?;
    Ok(parse_classpath(&result))
}

/// Parses the `[[classPaths…], [modulePaths…]]` result of [`RESOLVE_CLASSPATH_COMMAND`].
#[must_use]
pub fn parse_classpath(result: &Value) -> (Vec<String>, Vec<String>) {
    let list = |index: usize| -> Vec<String> {
        result
            .get(index)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    (list(0), list(1))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "test assertions")]

    use super::*;

    #[test]
    fn launch_arguments_follow_java_debug_schema() {
        let config = JavaLaunchConfig {
            main_class: "com.acme.App".to_owned(),
            project_name: Some("app".to_owned()),
            class_paths: vec!["/p/build/classes".to_owned()],
            vm_args: vec!["-Xmx256m".to_owned(), "-Da=b".to_owned()],
            args: vec!["--port".to_owned(), "8080".to_owned()],
            cwd: Some(PathBuf::from("/p")),
            java_home: Some(PathBuf::from("/jdk")),
            ..JavaLaunchConfig::default()
        };
        let args = config.arguments();
        assert_eq!(args["mainClass"], "com.acme.App");
        assert_eq!(args["projectName"], "app");
        assert_eq!(args["vmArgs"], "-Xmx256m -Da=b");
        assert_eq!(args["args"], "--port 8080");
        assert_eq!(args["cwd"], "/p");
        assert_eq!(args["console"], "internalConsole");
        assert!(args["javaExec"].as_str().unwrap().ends_with("java"));
        assert_eq!(config.target().command(), "launch");
    }

    #[test]
    fn attach_arguments() {
        let args = JavaAttachConfig::localhost(5005).arguments();
        assert_eq!(args["hostName"], "localhost");
        assert_eq!(args["port"], 5005);
        assert_eq!(JavaAttachConfig::localhost(1).target().command(), "attach");
    }

    #[test]
    fn parses_ports_and_classpaths() {
        assert_eq!(parse_debug_port(&json!(41234)).unwrap(), 41234);
        assert_eq!(parse_debug_port(&json!("5005")).unwrap(), 5005);
        assert!(parse_debug_port(&json!(0)).is_err());
        assert!(parse_debug_port(&json!(70000)).is_err());
        assert!(parse_debug_port(&Value::Null).is_err());
        let (cp, mp) = parse_classpath(&json!([["/a", "/b"], ["/m"]]));
        assert_eq!(cp, ["/a", "/b"]);
        assert_eq!(mp, ["/m"]);
        assert_eq!(parse_classpath(&Value::Null), (vec![], vec![]));
    }
}
