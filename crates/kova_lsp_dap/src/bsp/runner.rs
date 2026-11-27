//! Build and test run orchestration (`KOVA-040`, `KOVA-058`): BSP when a JVM project has
//! a `.bsp/*.json` connection, otherwise the Gradle / Maven / Cargo command line with live
//! `stdout`/`stderr` streaming, compiler diagnostics parsing, `cargo test` output parsing,
//! and `TEST-*.xml` fallback reports.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use kova_toolchain::{
    ToolchainEnvOptions, build_augmented_path, resolve_executable, resolve_toolchain_env,
};
use kova_tools::junit::{
    BuildCommand, BuildTool, CargoTestOutputParser, CompilerOutputParser, DiagnosticLevel,
    OutputStream, ParsedCompilerDiagnostic, TestCommand, TestEvent, TestId, build_compile_command,
    build_test_command, collect_reports, parse_cargo_json_line, report_dirs,
};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::bsp::client::{
    BspClient, BspConnectionDetails, BspTestParams, BuildTargetIdentifier, select_test_targets,
};
use crate::bsp::events::BspEvent;
use crate::error::LspDapError;
use crate::lsp::sync::path_to_lsp_uri;

/// A request to compile/build the project (`Build Project`, `Cmd+F9`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildRequest {
    /// Project root.
    pub project_root: PathBuf,
    /// Extra environment variables.
    pub env: BTreeMap<String, String>,
    /// Build tool forced by `kova.toml` (`None` = auto-detect).
    pub build_tool: Option<BuildTool>,
}

/// How a build request is executed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildBackend {
    /// Build Server Protocol (`buildTarget/compile`).
    Bsp(BspConnectionDetails),
    /// Build tool command line (`./gradlew classes testClasses`, `./mvnw test-compile`, `cargo build`).
    Cli(BuildCommand),
}

impl BuildBackend {
    /// Short description for the run console.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Bsp(details) => format!("BSP server `{}`", details.name),
            Self::Cli(cmd) => format!("{}: {}", cmd.tool.label(), cmd.display()),
        }
    }
}

/// A request to run `JUnit` or `Cargo` test targets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TestRunRequest {
    /// Project root.
    pub project_root: PathBuf,
    /// `Class` / `Class#method` targets (empty = every test).
    pub targets: Vec<String>,
    /// Absolute path of the file the run was started from (selects the BSP target).
    pub file: Option<PathBuf>,
    /// Extra JVM options.
    pub vm_options: Vec<String>,
    /// Extra environment variables.
    pub env: BTreeMap<String, String>,
    /// Build tool forced by `kova.toml` (`None` = auto-detect).
    pub build_tool: Option<BuildTool>,
}

/// How a test run is executed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestBackend {
    /// Build Server Protocol.
    Bsp(BspConnectionDetails),
    /// Build tool command line + `TEST-*.xml` reports (or `libtest` stream for Cargo).
    Cli(TestCommand),
}

impl TestBackend {
    /// Short description for the run console.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Bsp(details) => format!("BSP server `{}`", details.name),
            Self::Cli(cmd) => format!("{}: {}", cmd.tool.label(), cmd.display()),
        }
    }
}

/// Chooses the build backend for `request` (BSP connection first for JVM projects, then the
/// detected or configured build tool).
#[must_use]
pub fn plan_build_backend(request: &BuildRequest) -> Option<BuildBackend> {
    if request.build_tool != Some(BuildTool::Cargo)
        && let Some(details) = BspConnectionDetails::discover_jvm(&request.project_root)
    {
        return Some(BuildBackend::Bsp(details));
    }
    let tool = request
        .build_tool
        .or_else(|| BuildTool::detect(&request.project_root))?;
    Some(BuildBackend::Cli(build_compile_command(
        tool,
        &request.project_root,
        &request.env,
    )))
}

/// Finds the nearest enclosing Gradle/Maven submodule relative to `project_root` for `file`
/// (excluding `project_root` itself).
#[must_use]
pub fn resolve_submodule_for_file(project_root: &Path, file: Option<&Path>) -> Option<PathBuf> {
    let target = file?;
    if !target.starts_with(project_root) {
        return None;
    }
    let build_files = match BuildTool::detect(project_root) {
        Some(BuildTool::Gradle) => ["build.gradle", "build.gradle.kts"].as_slice(),
        Some(BuildTool::Maven) => ["pom.xml"].as_slice(),
        _ => ["build.gradle", "build.gradle.kts", "pom.xml"].as_slice(),
    };
    let mut cur = Some(target);
    while let Some(dir) = cur {
        if dir == project_root {
            break;
        }
        if build_files.iter().any(|f| dir.join(f).is_file()) {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

/// Applies submodule scoping (`:sub:test` for Gradle, `-pl sub` for Maven) to a [`TestCommand`].
pub fn apply_submodule_scope(command: &mut TestCommand, project_root: &Path, submodule: &Path) {
    let Ok(rel) = submodule.strip_prefix(project_root) else {
        return;
    };
    let parts: Vec<String> = rel
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str().map(str::to_owned),
            _ => None,
        })
        .collect();
    if parts.is_empty() {
        return;
    }
    match command.tool {
        BuildTool::Gradle => {
            if command.args.first().is_some_and(|a| a == "test") {
                command.args[0] = format!(":{}:test", parts.join(":"));
            }
        }
        BuildTool::Maven => {
            let module_arg = parts.join("/");
            command.args.insert(1, module_arg);
            command.args.insert(1, "-pl".to_owned());
        }
        BuildTool::Cargo => {}
    }
    for dir in report_dirs(submodule) {
        if !command.report_dirs.contains(&dir) {
            command.report_dirs.push(dir);
        }
    }
}

/// Chooses the test backend for `request` (BSP connection first for JVM projects, then the
/// detected or configured build tool).
#[must_use]
pub fn plan_test_backend(request: &TestRunRequest) -> Option<TestBackend> {
    if request.build_tool != Some(BuildTool::Cargo)
        && let Some(details) = BspConnectionDetails::discover_jvm(&request.project_root)
    {
        return Some(TestBackend::Bsp(details));
    }
    let tool = request
        .build_tool
        .or_else(|| BuildTool::detect(&request.project_root))?;
    let mut command = build_test_command(
        tool,
        &request.project_root,
        &request.targets,
        &request.vm_options,
        &request.env,
    );
    command.report_dirs = if tool == BuildTool::Cargo {
        Vec::new()
    } else {
        module_report_dirs(&request.project_root)
    };
    if let Some(submodule) =
        resolve_submodule_for_file(&request.project_root, request.file.as_deref())
    {
        apply_submodule_scope(&mut command, &request.project_root, &submodule);
    }
    Some(TestBackend::Cli(command))
}

/// Report directories of the root project and its first-level modules.
#[must_use]
pub fn module_report_dirs(project_root: &Path) -> Vec<PathBuf> {
    let mut dirs = report_dirs(project_root);
    if let Ok(entries) = std::fs::read_dir(project_root) {
        let mut modules: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_dir()
                    && ["build.gradle", "build.gradle.kts", "pom.xml"]
                        .iter()
                        .any(|f| p.join(f).is_file())
            })
            .collect();
        modules.sort();
        for module in modules {
            dirs.extend(report_dirs(&module));
        }
    }
    dirs
}

/// Compiles `request`, streaming [`BspEvent`]s (logs, diagnostics, compile reports) to `sink`.
/// Always ends with `TestEvent::RunFinished`. Returns whether the build succeeded.
///
/// # Errors
/// [`LspDapError::InvalidArgument`] when no BSP connection or Gradle/Maven/Cargo build exists,
/// or spawn/protocol errors.
pub async fn run_build(
    request: BuildRequest,
    sink: mpsc::UnboundedSender<BspEvent>,
) -> Result<bool, LspDapError> {
    let Some(backend) = plan_build_backend(&request) else {
        let err = LspDapError::InvalidArgument {
            reason: format!(
                "no BSP connection (.bsp/*.json) or Gradle/Maven/Cargo build found in {}",
                request.project_root.display()
            ),
        };
        finish(&sink, false, Some(err.to_string()));
        return Err(err);
    };
    console(
        &sink,
        format!("Building project via {}", backend.describe()),
    );
    let result = match &backend {
        BuildBackend::Bsp(details) => {
            match BspClient::connect(details, &request.project_root).await {
                Ok(client) => run_compile_over_bsp(&client, &sink).await,
                Err(err) => Err(err),
            }
        }
        BuildBackend::Cli(command) => {
            run_cli_streaming(
                command.tool,
                &command.program,
                &command.args,
                &command.working_dir,
                &command.env,
                CliMode::Build,
                &[],
                &sink,
                SystemTime::now(),
            )
            .await
        }
    };
    match result {
        Ok(success) => {
            finish(&sink, success, None);
            Ok(success)
        }
        Err(err) => {
            finish(&sink, false, Some(err.to_string()));
            Err(err)
        }
    }
}

/// Runs `request`, streaming [`BspEvent`]s (test tree updates, diagnostics, messages) to
/// `sink`. Always ends with a `TestEvent::RunFinished`. Returns whether the run succeeded.
///
/// # Errors
/// [`LspDapError::InvalidArgument`] when the project has neither a BSP connection nor a
/// Gradle / Maven / Cargo build, or spawn / protocol errors (a `RunFinished { success: false }`
/// is still emitted).
pub async fn run_tests(
    request: TestRunRequest,
    sink: mpsc::UnboundedSender<BspEvent>,
) -> Result<bool, LspDapError> {
    let Some(backend) = plan_test_backend(&request) else {
        let err = LspDapError::InvalidArgument {
            reason: format!(
                "no BSP connection (.bsp/*.json) or Gradle/Maven/Cargo build found in {}",
                request.project_root.display()
            ),
        };
        finish(&sink, false, Some(err.to_string()));
        return Err(err);
    };
    run_with_backend(request, backend, sink).await
}

/// Runs `request` with an explicit `backend`.
///
/// # Errors
/// See [`run_tests`].
pub async fn run_with_backend(
    request: TestRunRequest,
    backend: TestBackend,
    sink: mpsc::UnboundedSender<BspEvent>,
) -> Result<bool, LspDapError> {
    let started = SystemTime::now();
    console(&sink, format!("Running tests via {}", backend.describe()));
    let result = match &backend {
        TestBackend::Bsp(details) => {
            match BspClient::connect(details, &request.project_root).await {
                Ok(client) => run_over_bsp(&client, &request, &sink, started).await,
                Err(err) => Err(err),
            }
        }
        TestBackend::Cli(command) => {
            run_cli_streaming(
                command.tool,
                &command.program,
                &command.args,
                &command.working_dir,
                &command.env,
                CliMode::Test,
                &command.report_dirs,
                &sink,
                started,
            )
            .await
        }
    };
    match result {
        Ok(success) => {
            finish(&sink, success, None);
            Ok(success)
        }
        Err(err) => {
            finish(&sink, false, Some(err.to_string()));
            Err(err)
        }
    }
}

async fn run_compile_over_bsp(
    client: &BspClient,
    sink: &mpsc::UnboundedSender<BspEvent>,
) -> Result<bool, LspDapError> {
    let mut events = client.take_events();
    let targets = client.build_targets().await?;
    let compile_targets: Vec<BuildTargetIdentifier> = targets
        .into_iter()
        .filter(|t| t.capabilities.can_compile)
        .map(|t| t.id)
        .collect();
    if compile_targets.is_empty() {
        return Err(LspDapError::InvalidArgument {
            reason: format!(
                "BSP server `{}` exposes no compilable build target",
                client.server_name()
            ),
        });
    }
    let compile = client.compile(&compile_targets);
    tokio::pin!(compile);
    let status = loop {
        let Some(rx) = events.as_mut() else {
            break compile.await?;
        };
        tokio::select! {
            result = &mut compile => break result?,
            event = rx.recv() => match event {
                Some(event) => drop(sink.send(event)),
                None => events = None,
            },
        }
    };
    if let Some(rx) = events.as_mut() {
        while let Ok(event) = rx.try_recv() {
            drop(sink.send(event));
        }
    }
    drop(client.shutdown().await);
    Ok(status.is_ok())
}

/// Runs the tests of `request` on an already connected `client` (without `RunFinished`).
///
/// # Errors
/// Protocol errors of `workspace/buildTargets` or `buildTarget/test`.
pub async fn run_over_bsp(
    client: &BspClient,
    request: &TestRunRequest,
    sink: &mpsc::UnboundedSender<BspEvent>,
    started: SystemTime,
) -> Result<bool, LspDapError> {
    let mut events = client.take_events();
    let targets = client.build_targets().await?;
    let file_uri = match &request.file {
        Some(file) => Some(path_to_lsp_uri(file)?.as_str().to_owned()),
        None => None,
    };
    let selected = select_test_targets(&targets, file_uri.as_deref());
    if selected.is_empty() {
        return Err(LspDapError::InvalidArgument {
            reason: format!(
                "BSP server `{}` exposes no testable build target",
                client.server_name()
            ),
        });
    }
    let params = BspTestParams {
        targets: selected,
        selection: request
            .targets
            .iter()
            .map(|t| TestId::from_target(t))
            .collect(),
        jvm_options: request.vm_options.clone(),
        env: request.env.clone(),
        working_dir: Some(request.project_root.clone()),
    };
    let mut saw_tests = false;
    let forward = |event: BspEvent, saw_tests: &mut bool| {
        if matches!(
            event,
            BspEvent::Test(TestEvent::TestStarted(_) | TestEvent::TestFinished { .. })
        ) {
            *saw_tests = true;
        }
        drop(sink.send(event));
    };
    let test = client.test(&params);
    tokio::pin!(test);
    let status = loop {
        let Some(rx) = events.as_mut() else {
            break test.await?;
        };
        tokio::select! {
            result = &mut test => break result?,
            event = rx.recv() => match event {
                Some(event) => forward(event, &mut saw_tests),
                None => events = None,
            },
        }
    };
    // Notifications delivered before the response are already queued.
    if let Some(rx) = events.as_mut() {
        while let Ok(event) = rx.try_recv() {
            forward(event, &mut saw_tests);
        }
    }
    if !saw_tests {
        console(
            sink,
            "BSP server sent no per-test notifications; reading TEST-*.xml reports".to_owned(),
        );
        emit_reports(&module_report_dirs(&request.project_root), sink, started);
    }
    drop(client.shutdown().await);
    Ok(status.is_ok())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliMode {
    Build,
    Test,
}

struct PreparedCommand {
    program: PathBuf,
    args: Vec<String>,
    env_vars: BTreeMap<String, String>,
    wrapper_via_sh: bool,
}

fn prepare_command_env(
    working_dir: &Path,
    extra_env: &BTreeMap<String, String>,
    program: &Path,
    args: &[String],
) -> PreparedCommand {
    let toolchain = resolve_toolchain_env(&ToolchainEnvOptions {
        project_root: Some(working_dir.to_path_buf()),
        ..ToolchainEnvOptions::default()
    });
    let mut env_vars = toolchain.vars;
    for (k, v) in extra_env {
        env_vars.insert(k.clone(), v.clone());
    }

    // If `extra_env` supplied a custom `JAVA_HOME` without overriding `PATH`, prepend its `bin/`.
    if let Some(custom_home) = extra_env.get("JAVA_HOME")
        && !extra_env.contains_key("PATH")
    {
        let base_path = env_vars.get("PATH").map(OsString::from);
        let augmented = build_augmented_path(
            Some(Path::new(custom_home)),
            base_path.as_deref(),
            None,
            &[],
        );
        env_vars.insert("PATH".to_owned(), augmented.to_string_lossy().into_owned());
    }

    let effective_path = env_vars
        .get("PATH")
        .map_or_else(OsString::new, OsString::from);
    let mut resolved_program = resolve_executable(program, &effective_path);
    let mut resolved_args = args.to_vec();
    let mut wrapper_via_sh = false;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&resolved_program)
            && meta.is_file()
            && meta.permissions().mode() & 0o111 == 0
        {
            let script = resolved_program.to_string_lossy().into_owned();
            resolved_program = resolve_executable(Path::new("sh"), &effective_path);
            resolved_args.insert(0, script);
            wrapper_via_sh = true;
        }
    }

    PreparedCommand {
        program: resolved_program,
        args: resolved_args,
        env_vars,
        wrapper_via_sh,
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "shared streaming CLI runner for both build and test invocations"
)]
async fn run_cli_streaming(
    tool: BuildTool,
    program: &Path,
    args: &[String],
    working_dir: &Path,
    extra_env: &BTreeMap<String, String>,
    mode: CliMode,
    report_dirs_list: &[PathBuf],
    sink: &mpsc::UnboundedSender<BspEvent>,
    started: SystemTime,
) -> Result<bool, LspDapError> {
    let prepared = prepare_command_env(working_dir, extra_env, program, args);
    if tool != BuildTool::Cargo
        && let Some(java_home) = prepared.env_vars.get("JAVA_HOME")
    {
        console(sink, format!("Using JAVA_HOME={java_home}"));
    }
    if prepared.wrapper_via_sh {
        console(
            sink,
            format!(
                "Wrapper `{}` lacks +x permission; invoking via `sh`",
                program.display()
            ),
        );
    }

    let program_display = prepared.program.display().to_string();
    let mut child = Command::new(&prepared.program)
        .args(&prepared.args)
        .envs(&prepared.env_vars)
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|err| LspDapError::SpawnFailed {
            program: program_display.clone(),
            reason: err.to_string(),
        })?;

    let accumulated_diags: Arc<Mutex<HashMap<PathBuf, Vec<lsp_types::Diagnostic>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let root = working_dir.to_path_buf();

    let stdout = child.stdout.take().map(|s| {
        tokio::spawn(pump_lines(
            s,
            OutputStream::Stdout,
            tool,
            mode,
            root.clone(),
            Arc::clone(&accumulated_diags),
            sink.clone(),
        ))
    });
    let stderr = child.stderr.take().map(|s| {
        tokio::spawn(pump_lines(
            s,
            OutputStream::Stderr,
            tool,
            mode,
            root,
            Arc::clone(&accumulated_diags),
            sink.clone(),
        ))
    });

    let status = child
        .wait()
        .await
        .map_err(|err| LspDapError::io(program_display, &err))?;
    for pump in [stdout, stderr].into_iter().flatten() {
        drop(pump.await);
    }

    if mode == CliMode::Test && !report_dirs_list.is_empty() {
        emit_reports(report_dirs_list, sink, started);
    }

    let (error_count, warning_count) = {
        let guard = accumulated_diags
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut errors = 0_u64;
        let mut warnings = 0_u64;
        for diag in guard.values().flatten() {
            match diag.severity {
                Some(lsp_types::DiagnosticSeverity::WARNING) => warnings += 1,
                _ => errors += 1,
            }
        }
        (errors, warnings)
    };
    if mode == CliMode::Build && (error_count > 0 || warning_count > 0 || !status.success()) {
        drop(sink.send(BspEvent::CompileReport {
            target: None,
            errors: error_count,
            warnings: warning_count,
        }));
    }

    Ok(status.success())
}

async fn pump_lines<R: AsyncRead + Unpin>(
    reader: R,
    stream: OutputStream,
    tool: BuildTool,
    mode: CliMode,
    project_root: PathBuf,
    accumulated_diags: Arc<Mutex<HashMap<PathBuf, Vec<lsp_types::Diagnostic>>>>,
    sink: mpsc::UnboundedSender<BspEvent>,
) {
    let mut lines = BufReader::new(reader).lines();
    let mut compiler_parser = CompilerOutputParser::new();
    let mut cargo_test_parser =
        (tool == BuildTool::Cargo && mode == CliMode::Test && stream == OutputStream::Stdout)
            .then(CargoTestOutputParser::new);

    while let Ok(Some(line)) = lines.next_line().await {
        if tool == BuildTool::Cargo && mode == CliMode::Build && stream == OutputStream::Stdout {
            let decoded = parse_cargo_json_line(&line);
            for (out_stream, text) in decoded.console_lines {
                drop(sink.send(BspEvent::Test(TestEvent::Output {
                    id: None,
                    stream: out_stream,
                    text,
                })));
            }
            if !decoded.diagnostics.is_empty() {
                emit_parsed_diagnostics(
                    decoded.diagnostics,
                    &project_root,
                    &accumulated_diags,
                    &sink,
                );
            }
            continue;
        }

        drop(sink.send(BspEvent::Test(TestEvent::Output {
            id: None,
            stream,
            text: line.clone(),
        })));

        let diags = compiler_parser.parse_line(&line);
        if !diags.is_empty() {
            emit_parsed_diagnostics(diags, &project_root, &accumulated_diags, &sink);
        }

        if let Some(parser) = cargo_test_parser.as_mut() {
            for event in parser.parse_line(&line) {
                drop(sink.send(BspEvent::Test(event)));
            }
        }
    }

    if let Some(parser) = cargo_test_parser.as_mut() {
        for event in parser.finish() {
            drop(sink.send(BspEvent::Test(event)));
        }
    }
}

fn emit_parsed_diagnostics(
    parsed: Vec<ParsedCompilerDiagnostic>,
    project_root: &Path,
    accumulated: &Mutex<HashMap<PathBuf, Vec<lsp_types::Diagnostic>>>,
    sink: &mpsc::UnboundedSender<BspEvent>,
) {
    let mut touched: Vec<PathBuf> = Vec::new();
    {
        let mut guard = accumulated.lock().unwrap_or_else(PoisonError::into_inner);
        for diag in parsed {
            let abs_path = if diag.file.is_absolute() {
                diag.file.clone()
            } else {
                project_root.join(&diag.file)
            };
            let line0 = u32::try_from(diag.line.saturating_sub(1)).unwrap_or(u32::MAX);
            let col0 = u32::try_from(diag.col.saturating_sub(1)).unwrap_or(u32::MAX);
            let pos = lsp_types::Position {
                line: line0,
                character: col0,
            };
            let lsp_diag = lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: pos,
                    end: pos,
                },
                severity: Some(match diag.severity {
                    DiagnosticLevel::Error => lsp_types::DiagnosticSeverity::ERROR,
                    DiagnosticLevel::Warning => lsp_types::DiagnosticSeverity::WARNING,
                }),
                source: Some("compiler".to_owned()),
                message: diag.message,
                ..lsp_types::Diagnostic::default()
            };
            let list = guard.entry(abs_path.clone()).or_default();
            if !list.contains(&lsp_diag) {
                list.push(lsp_diag);
            }
            if !touched.contains(&abs_path) {
                touched.push(abs_path);
            }
        }
    }

    let guard = accumulated.lock().unwrap_or_else(PoisonError::into_inner);
    for path in touched {
        let Some(diagnostics) = guard.get(&path) else {
            continue;
        };
        let uri = path_to_lsp_uri(&path)
            .map_or_else(|_| path.display().to_string(), |u| u.as_str().to_owned());
        drop(sink.send(BspEvent::Diagnostics {
            uri,
            build_target: None,
            diagnostics: diagnostics.clone(),
            reset: true,
        }));
    }
}

fn emit_reports(dirs: &[PathBuf], sink: &mpsc::UnboundedSender<BspEvent>, since: SystemTime) {
    // File systems with coarse timestamps may round the report mtime down.
    let since = since
        .checked_sub(std::time::Duration::from_secs(2))
        .unwrap_or(since);
    let (events, errors) = collect_reports(dirs, Some(since));
    for event in events {
        drop(sink.send(BspEvent::Test(event)));
    }
    for error in errors {
        console(sink, error.to_string());
    }
}

fn console(sink: &mpsc::UnboundedSender<BspEvent>, text: String) {
    drop(sink.send(BspEvent::Test(TestEvent::Output {
        id: None,
        stream: OutputStream::Stdout,
        text,
    })));
}

fn finish(sink: &mpsc::UnboundedSender<BspEvent>, success: bool, message: Option<String>) {
    drop(sink.send(BspEvent::Test(TestEvent::RunFinished { success, message })));
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "runner unit and integration tests assert backend planning and streaming"
)]
mod tests {
    use super::*;

    #[test]
    fn plans_bsp_before_cli_and_skips_bsp_when_cargo_forced() {
        let tmp = tempfile::tempdir().unwrap();
        let mut request = TestRunRequest {
            project_root: tmp.path().to_path_buf(),
            targets: vec!["a.T#m".to_owned()],
            ..TestRunRequest::default()
        };
        assert_eq!(plan_test_backend(&request), None);

        std::fs::write(tmp.path().join("pom.xml"), "<project/>").unwrap();
        std::fs::create_dir_all(tmp.path().join("core")).unwrap();
        std::fs::write(tmp.path().join("core/pom.xml"), "<project/>").unwrap();
        let Some(TestBackend::Cli(cmd)) = plan_test_backend(&request) else {
            panic!("expected Maven CLI backend");
        };
        assert_eq!(cmd.tool, BuildTool::Maven);
        assert!(cmd.args.contains(&"-Dtest=a.T#m".to_owned()));
        assert!(
            cmd.report_dirs
                .contains(&tmp.path().join("core/target/surefire-reports"))
        );

        request.build_tool = Some(BuildTool::Gradle);
        let Some(TestBackend::Cli(cmd)) = plan_test_backend(&request) else {
            panic!("expected Gradle CLI backend");
        };
        assert_eq!(cmd.tool, BuildTool::Gradle);

        std::fs::create_dir_all(tmp.path().join(".bsp")).unwrap();
        std::fs::write(
            tmp.path().join(".bsp/gradle.json"),
            r#"{"name":"gradle-bsp","argv":["gradle-bsp"],"languages":["java"]}"#,
        )
        .unwrap();
        assert!(matches!(
            plan_test_backend(&request),
            Some(TestBackend::Bsp(_))
        ));

        // Explicit Cargo override bypasses JVM BSP discovery.
        request.build_tool = Some(BuildTool::Cargo);
        let Some(TestBackend::Cli(cmd)) = plan_test_backend(&request) else {
            panic!("expected Cargo CLI backend");
        };
        assert_eq!(cmd.tool, BuildTool::Cargo);
        assert_eq!(cmd.report_dirs, Vec::<PathBuf>::new());
    }

    #[test]
    fn plans_build_backend_for_cargo_and_jvm() {
        let tmp = tempfile::tempdir().unwrap();
        let mut req = BuildRequest {
            project_root: tmp.path().to_path_buf(),
            ..BuildRequest::default()
        };
        assert_eq!(plan_build_backend(&req), None);

        std::fs::write(tmp.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        let Some(BuildBackend::Cli(cmd)) = plan_build_backend(&req) else {
            panic!("expected Cargo build command");
        };
        assert_eq!(cmd.tool, BuildTool::Cargo);
        assert_eq!(
            cmd.args,
            vec!["build", "--all-targets", "--message-format=json"]
        );

        std::fs::write(tmp.path().join("build.gradle.kts"), "").unwrap();
        req.build_tool = Some(BuildTool::Gradle);
        let Some(BuildBackend::Cli(cmd)) = plan_build_backend(&req) else {
            panic!("expected Gradle build command");
        };
        assert_eq!(cmd.tool, BuildTool::Gradle);
        assert_eq!(cmd.args, vec!["classes", "testClasses", "--console=plain"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_build_streams_javac_diagnostics_and_injects_env() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let src_file = tmp.path().join("src/main/java/App.java");
        std::fs::create_dir_all(src_file.parent().unwrap()).unwrap();
        std::fs::write(&src_file, "class App {}\n").unwrap();

        // Create a Gradle wrapper script WITHOUT +x permission to verify `sh` fallback
        // and custom env injection.
        let gradlew = tmp.path().join("gradlew");
        let script = format!(
            "#!/bin/sh\necho \"CUSTOM_FLAG=$CUSTOM_FLAG\"\necho \"{}:12:4: error: ';' expected\" >&2\nexit 1\n",
            src_file.display()
        );
        std::fs::write(&gradlew, script).unwrap();
        std::fs::set_permissions(&gradlew, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::write(tmp.path().join("build.gradle"), "").unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut env = BTreeMap::new();
        env.insert("CUSTOM_FLAG".to_owned(), "kova_ok".to_owned());
        let ok = run_build(
            BuildRequest {
                project_root: tmp.path().to_path_buf(),
                env,
                build_tool: Some(BuildTool::Gradle),
            },
            tx,
        )
        .await
        .expect("build process runs");
        assert!(!ok);

        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }

        assert!(events.iter().any(|ev| matches!(
            ev,
            BspEvent::Test(TestEvent::Output { text, .. }) if text.contains("CUSTOM_FLAG=kova_ok")
        )));
        assert!(events.iter().any(|ev| matches!(
            ev,
            BspEvent::Diagnostics { diagnostics, .. }
                if diagnostics.iter().any(|d| d.message == "';' expected" && d.range.start.line == 11 && d.range.start.character == 3)
        )));
        assert!(
            events
                .iter()
                .any(|ev| matches!(ev, BspEvent::CompileReport { errors: 1, .. }))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_build_and_test_support_cargo_json_and_libtest_streaming() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = tmp.path().join("fake_bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let fake_cargo = bin_dir.join("cargo");
        let script = r#"#!/bin/sh
if [ "$1" = "build" ]; then
  printf '%s\n' '{"reason":"compiler-artifact","package_id":"demo 0.1.0"}'
  printf '%s\n' '{"reason":"compiler-message","package_id":"demo 0.1.0","message":{"rendered":"error[E0425]: cannot find value `x`\n --> src/main.rs:3:5","message":"cannot find value `x`","level":"error","spans":[{"file_name":"src/main.rs","line_start":3,"column_start":5,"is_primary":true,"label":"not found"}]}}'
  exit 1
elif [ "$1" = "test" ]; then
  printf '%s\n' "Running unittests src/lib.rs (target/debug/deps/demo-1)"
  printf '%s\n' "test api::tests::passes ... ok"
  printf '%s\n' "test api::tests::fails ... FAILED"
  printf '%s\n' "---- api::tests::fails stdout ----"
  printf '%s\n' "thread 'api::tests::fails' panicked at src/lib.rs:9:5:"
  printf '%s\n' "boom"
  printf '%s\n' "test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out"
  exit 1
fi
"#;
        std::fs::write(&fake_cargo, script).unwrap();
        std::fs::set_permissions(&fake_cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\n",
        )
        .unwrap();

        // Build with fake cargo on PATH.
        let mut env = BTreeMap::new();
        env.insert("PATH".to_owned(), bin_dir.display().to_string());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let ok = run_build(
            BuildRequest {
                project_root: tmp.path().to_path_buf(),
                env: env.clone(),
                build_tool: Some(BuildTool::Cargo),
            },
            tx,
        )
        .await
        .expect("cargo build runs");
        assert!(!ok);
        let mut build_events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            build_events.push(ev);
        }
        assert!(build_events.iter().any(|ev| matches!(
            ev,
            BspEvent::Diagnostics { uri, diagnostics, .. }
                if uri.ends_with("src/main.rs")
                    && diagnostics.iter().any(|d| d.message.contains("cannot find value `x`"))
        )));
        assert!(!build_events.iter().any(|ev| matches!(
            ev,
            BspEvent::Test(TestEvent::Output { text, .. }) if text.contains("compiler-artifact")
        )));

        // Test with fake cargo on PATH.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let test_ok = run_tests(
            TestRunRequest {
                project_root: tmp.path().to_path_buf(),
                env,
                build_tool: Some(BuildTool::Cargo),
                ..TestRunRequest::default()
            },
            tx,
        )
        .await
        .expect("cargo test runs");
        assert!(!test_ok);
        let mut test_events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            test_events.push(ev);
        }
        assert!(test_events.iter().any(|ev| matches!(
            ev,
            BspEvent::Test(TestEvent::TestFinished { id, outcome })
                if id.method.as_deref() == Some("passes") && outcome.status == kova_tools::junit::TestStatus::Passed
        )));
        assert!(test_events.iter().any(|ev| matches!(
            ev,
            BspEvent::Test(TestEvent::TestFinished { id, outcome })
                if id.method.as_deref() == Some("fails") && outcome.status == kova_tools::junit::TestStatus::Failed
        )));
    }

    #[test]
    fn plans_submodule_scoped_gradle_and_maven_commands() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("settings.gradle.kts"), "").unwrap();
        let sub = tmp.path().join("services/orders");
        let test_file = sub.join("src/test/groovy/com/kova/OrderSpec.groovy");
        std::fs::create_dir_all(test_file.parent().unwrap()).unwrap();
        std::fs::write(sub.join("build.gradle.kts"), "").unwrap();
        std::fs::write(&test_file, "").unwrap();

        let req = TestRunRequest {
            project_root: tmp.path().to_path_buf(),
            targets: vec!["com.kova.OrderSpec#creates $10".to_owned()],
            file: Some(test_file.clone()),
            ..TestRunRequest::default()
        };
        let Some(TestBackend::Cli(gradle)) = plan_test_backend(&req) else {
            panic!("expected Gradle CLI backend");
        };
        assert_eq!(
            gradle.args,
            vec![
                ":services:orders:test",
                "--tests",
                "com.kova.OrderSpec.creates $10",
                "--continue",
                "--console=plain",
            ]
        );
        assert!(
            gradle
                .report_dirs
                .contains(&sub.join("build/test-results/test"))
        );

        std::fs::write(tmp.path().join("pom.xml"), "<project/>").unwrap();
        std::fs::write(sub.join("pom.xml"), "<project/>").unwrap();
        let maven_req = TestRunRequest {
            project_root: tmp.path().to_path_buf(),
            targets: Vec::new(),
            file: Some(sub.clone()),
            build_tool: Some(BuildTool::Maven),
            ..TestRunRequest::default()
        };
        let Some(TestBackend::Cli(maven)) = plan_test_backend(&maven_req) else {
            panic!("expected Maven CLI backend");
        };
        assert_eq!(maven.args, vec!["test", "-pl", "services/orders", "-B"]);
        assert!(
            maven
                .report_dirs
                .contains(&sub.join("target/surefire-reports"))
        );
    }
}
