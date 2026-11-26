//! Launch command builder for Eclipse `jdt.ls` ([`build_jdtls_launch_spec`]).
//!
//! All mutable `jdt.ls` state (`-configuration`, `-data`) lives in
//! `~/.cache/kova/workspaces/<hash>/`, never in the project or the shared installation.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use kova_config::{EffectiveConfig, KovaPaths};
use serde_json::{Value, json};

use crate::error::ToolchainError;
pub use crate::jdk::JDTLS_MIN_JAVA_MAJOR;
use crate::jdk::JdkInstallation;
use crate::omakase::OmakaseToolkit;

/// Name of the per-workspace isolated Equinox configuration directory.
pub const JDTLS_CONFIG_DIR_NAME: &str = "jdtls-config";
/// Name of the per-workspace `jdt.ls` data directory (`-data`).
pub const JDTLS_DATA_DIR_NAME: &str = "jdtls-data";
/// Marker file recording which `jdt.ls` build seeded the isolated configuration.
pub const JDTLS_VERSION_MARKER: &str = ".kova-jdtls-version";

/// Inputs for [`build_jdtls_launch_spec`].
#[derive(Debug, Clone, PartialEq)]
pub struct JdtlsLaunchOptions {
    /// Project root opened in Kova.
    pub project_root: PathBuf,
    /// Project target JDK, published as the default `java.configuration.runtimes` entry.
    pub project_jdk: Option<JdkInstallation>,
    /// JDK (>= [`JDTLS_MIN_JAVA_MAJOR`]) used to run the `jdt.ls` process itself.
    pub runtime_jdk: JdkInstallation,
    /// Effective user + project configuration.
    pub effective_config: EffectiveConfig,
    /// Additional JVM arguments, inserted before `-jar`.
    pub extra_jvm_args: Vec<String>,
}

/// Fully resolved `jdt.ls` process launch description.
#[derive(Debug, Clone, PartialEq)]
pub struct JdtlsLaunchSpec {
    /// `java` executable of the runtime JDK.
    pub java_bin: PathBuf,
    /// JVM options followed by `-jar <launcher> -configuration <dir> -data <dir>`.
    pub jvm_and_app_args: Vec<String>,
    /// Extra environment variables (`JAVA_HOME`).
    pub env: BTreeMap<String, String>,
    /// Process working directory (the project root).
    pub working_dir: PathBuf,
    /// `-data` directory inside the workspace cache.
    pub workspace_data_dir: PathBuf,
    /// `-configuration` directory inside the workspace cache.
    pub osgi_config_dir: PathBuf,
    /// LSP `initializationOptions` payload.
    pub initialization_options: Value,
}

impl JdtlsLaunchSpec {
    /// Shell-like single-line rendering of the command for diagnostic logs.
    #[must_use]
    pub fn command_line_preview(&self) -> String {
        std::iter::once(self.java_bin.display().to_string())
            .chain(self.jvm_and_app_args.iter().cloned())
            .map(|arg| shell_quote(&arg))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn shell_quote(arg: &str) -> String {
    let safe = !arg.is_empty()
        && arg.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '-' | '_' | '.' | '/' | '=' | ':' | '+' | ',' | '@')
        });
    if safe {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

/// Builds the `jdt.ls` launch command for `opts.project_root`.
///
/// Creates the isolated `-data` and `-configuration` directories in
/// `paths.workspace_cache_dir(project_root)`, seeding the configuration from the
/// installation's base `config_*` directory (refreshed when the `jdt.ls` version changes).
///
/// # Errors
/// Returns [`ToolchainError::UnsupportedJdkRuntime`] if the runtime JDK is older than
/// [`JDTLS_MIN_JAVA_MAJOR`], or [`ToolchainError::Io`] on path or filesystem failures.
pub fn build_jdtls_launch_spec(
    paths: &KovaPaths,
    toolkit: &OmakaseToolkit,
    opts: &JdtlsLaunchOptions,
) -> Result<JdtlsLaunchSpec, ToolchainError> {
    let runtime = &opts.runtime_jdk;
    if runtime.version.major < JDTLS_MIN_JAVA_MAJOR {
        return Err(ToolchainError::UnsupportedJdkRuntime {
            path: runtime.home_dir.display().to_string(),
            found_major: runtime.version.major,
            min_major: JDTLS_MIN_JAVA_MAJOR,
        });
    }

    let workspace_dir = paths
        .ensure_workspace_cache_dir(&opts.project_root)
        .map_err(|err| ToolchainError::Io {
            path: opts.project_root.display().to_string(),
            message: err.to_string(),
        })?;
    let workspace_data_dir = workspace_dir.join(JDTLS_DATA_DIR_NAME);
    let osgi_config_dir = workspace_dir.join(JDTLS_CONFIG_DIR_NAME);
    fs::create_dir_all(&workspace_data_dir)
        .map_err(|err| ToolchainError::io(workspace_data_dir.display().to_string(), &err))?;
    prepare_osgi_config(toolkit, &osgi_config_dir)?;

    let settings = &opts.effective_config.settings;
    let mut args = vec![
        "-Declipse.application=org.eclipse.jdt.ls.core.id1".to_owned(),
        "-Dosgi.bundles.defaultStartLevel=4".to_owned(),
        "-Declipse.product=org.eclipse.jdt.ls.core.product".to_owned(),
        "-Dlog.level=ALL".to_owned(),
        format!("-Xmx{}m", settings.jvm.jdtls_max_ram_mb),
        "-XX:+UseG1GC".to_owned(),
    ];
    if settings.jvm.lombok_enabled
        && let Some(lombok) = &toolkit.jdtls.lombok_jar
    {
        args.push(format!("-javaagent:{}", lombok.display()));
    }
    args.extend([
        "--add-modules=ALL-SYSTEM".to_owned(),
        "--add-opens=java.base/java.util=ALL-UNNAMED".to_owned(),
        "--add-opens=java.base/java.lang=ALL-UNNAMED".to_owned(),
    ]);
    args.extend(opts.extra_jvm_args.iter().cloned());
    args.extend([
        "-jar".to_owned(),
        toolkit.jdtls.launcher_jar.display().to_string(),
        "-configuration".to_owned(),
        osgi_config_dir.display().to_string(),
        "-data".to_owned(),
        workspace_data_dir.display().to_string(),
    ]);

    let mut env = BTreeMap::new();
    env.insert(
        "JAVA_HOME".to_owned(),
        runtime.home_dir.display().to_string(),
    );

    Ok(JdtlsLaunchSpec {
        java_bin: runtime.java_bin.clone(),
        jvm_and_app_args: args,
        env,
        working_dir: opts.project_root.clone(),
        workspace_data_dir,
        osgi_config_dir,
        initialization_options: initialization_options(toolkit, opts),
    })
}

fn initialization_options(toolkit: &OmakaseToolkit, opts: &JdtlsLaunchOptions) -> Value {
    let settings = &opts.effective_config.settings;
    let bundles: Vec<String> = toolkit
        .java_debug_jar
        .iter()
        .map(|jar| jar.display().to_string())
        .collect();
    let runtimes: Vec<Value> = opts
        .project_jdk
        .iter()
        .map(|jdk| {
            json!({
                "name": execution_environment(jdk.version.major),
                "path": jdk.home_dir.display().to_string(),
                "default": true,
            })
        })
        .collect();
    let inlay = if settings.editor.show_inlay_hints {
        "all"
    } else {
        "none"
    };
    json!({
        "bundles": bundles,
        "settings": {
            "java": {
                "import": { "gradle": { "offline": { "enabled": settings.jvm.gradle_offline } } },
                "inlayHints": { "parameterNames": { "enabled": inlay } },
                "configuration": { "runtimes": runtimes },
            }
        }
    })
}

/// Eclipse execution environment name for a Java major version (`JavaSE-1.8`, `JavaSE-17`).
fn execution_environment(major: u16) -> String {
    if major <= 8 {
        format!("JavaSE-1.{major}")
    } else {
        format!("JavaSE-{major}")
    }
}

/// Copies the base Equinox configuration into the isolated per-workspace directory when
/// it is missing or was seeded by a different `jdt.ls` build.
fn prepare_osgi_config(
    toolkit: &OmakaseToolkit,
    osgi_config_dir: &Path,
) -> Result<(), ToolchainError> {
    let launcher_name = toolkit
        .jdtls
        .launcher_jar
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let expected_marker = format!("{}\n{launcher_name}\n", toolkit.jdtls.version);
    let marker_path = osgi_config_dir.join(JDTLS_VERSION_MARKER);
    let up_to_date = osgi_config_dir.join("config.ini").is_file()
        && fs::read_to_string(&marker_path).is_ok_and(|m| m == expected_marker);
    if up_to_date {
        return Ok(());
    }
    let io =
        |path: &Path, err: &std::io::Error| ToolchainError::io(path.display().to_string(), err);
    if osgi_config_dir.exists() {
        fs::remove_dir_all(osgi_config_dir).map_err(|err| io(osgi_config_dir, &err))?;
    }
    copy_dir_files(&toolkit.jdtls.base_config_dir, osgi_config_dir)?;
    fs::write(&marker_path, expected_marker).map_err(|err| io(&marker_path, &err))
}

/// Recursively copies regular files and directories (symlinks are skipped).
fn copy_dir_files(from: &Path, to: &Path) -> Result<(), ToolchainError> {
    let io =
        |path: &Path, err: &std::io::Error| ToolchainError::io(path.display().to_string(), err);
    fs::create_dir_all(to).map_err(|err| io(to, &err))?;
    for entry in fs::read_dir(from).map_err(|err| io(from, &err))? {
        let entry = entry.map_err(|err| io(from, &err))?;
        let file_type = entry.file_type().map_err(|err| io(&entry.path(), &err))?;
        let dest = to.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_files(&entry.path(), &dest)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), &dest).map_err(|err| io(&dest, &err))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_environment_names() {
        assert_eq!(execution_environment(8), "JavaSE-1.8");
        assert_eq!(execution_environment(17), "JavaSE-17");
    }

    #[test]
    fn shell_quote_escapes_spaces_and_quotes() {
        assert_eq!(shell_quote("-Xmx2048m"), "-Xmx2048m");
        assert_eq!(shell_quote("/a b/c"), "'/a b/c'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }
}
