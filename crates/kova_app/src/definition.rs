//! Host side of *Go to Definition* (`KOVA-051`, `KOVA-061`):
//!
//! - **Level 1 (`< 15 ms`):** instant workspace & open-buffer symbol resolution via
//!   `kova_core::navigation::resolve_local_definition` for Java, Kotlin, and Rust.
//! - **Level 2 (`jdt.ls` + Vineflower):** when a Java symbol is not in the workspace and a ready
//!   [`JdtlsClient`] is available, delegates to [`DefinitionResolver::go_to_definition`].
//! - **Level 1.5 (Zero-`jdt.ls` JDK & library decompiler):** resolves external JDK `.jmod` /
//!   `rt.jar` and dependency `.jar` (project `libs/`, Gradle, Maven) classes immediately using
//!   [`ExternalClassLocator`], decompiling with [`VineflowerDecompiler`] when installed or the
//!   instant pure-Rust `cafebabe` (+ optional `javap -p`) fallback otherwise.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use kova_buffer::{Buffer, Utf16Point};
use kova_config::{EffectiveConfig, JvmSettings, KovaPaths, KovaSettings};
use kova_core::navigation::{
    ExternalTargetCandidates, SourceLanguageKind, SymbolQuery, extract_symbol_query,
    resolve_external_fqcn_candidates, resolve_local_definition,
};
use kova_jvm_index::{
    BytecodeScanOptions, DependencyArchiveConfig, DependencyArchiveIndex, ExternalClassLocator,
    JdkOnDemandIndex, JvmSourceOrigin, JvmSymbolKind, LocatedClassBytes,
    cache_bytecode_decompilation, decompile_class_bytes_to_java, parse_class_bytes,
};
use kova_lsp_dap::lsp::{
    DefinitionResolver, DefinitionTarget, JdtlsClient, LspLifecycleState, find_declaration,
    path_to_lsp_uri,
};
use kova_lsp_dap::manager::{JdtlsSessionManager, OsProcessMemoryProbe, SessionManagerConfig};
use kova_toolchain::{
    DecompileTarget, JdkDiscoveryRoots, JdkInstallation, VineflowerDecompiler,
    decompile_with_javap_fallback, discover_jdks, discover_vineflower_decompiler,
    select_tool_runtime_jdk,
};
use kova_ui::{DefinitionOutcome, DefinitionRequest, DefinitionSink, DefinitionTargetKind};
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use crate::AppEvent;

/// Timeout applied to Level-2 `textDocument/definition` queries so UI navigation stays snappy.
const LSP_DEFINITION_TIMEOUT: Duration = Duration::from_secs(3);

/// Host configuration for Level-1.5 external JDK/library bytecode navigation (`KOVA-061`).
#[derive(Debug, Clone)]
pub struct ExternalDefinitionConfig {
    /// On-demand JDK `.jmod` / `rt.jar` index.
    pub jdk_index: Arc<JdkOnDemandIndex>,
    /// Optional runtime JDK (used for Vineflower or `javap -p` fallback).
    pub runtime_jdk: Option<JdkInstallation>,
    /// Optional Vineflower decompiler (when installed locally).
    pub decompiler: Option<VineflowerDecompiler>,
    /// Base configuration for project/Gradle/Maven `.jar` dependency indexing.
    pub base_dependency_config: DependencyArchiveConfig,
    /// Cache root for decompiled `.java` files (`~/.cache/kova/decompiled`).
    pub cache_root: PathBuf,
    /// Whether Vineflower decompilation is enabled (`JvmSettings::auto_decompile_class`).
    pub auto_decompile_class: bool,
}

#[derive(Debug)]
struct ExternalDefinitionRuntime {
    jdk_index: Arc<JdkOnDemandIndex>,
    runtime_jdk: Option<JdkInstallation>,
    decompiler: Option<VineflowerDecompiler>,
    base_dependency_config: DependencyArchiveConfig,
    cache_root: PathBuf,
    auto_decompile_class: bool,
    dep_indexes: Mutex<HashMap<Option<PathBuf>, Arc<DependencyArchiveIndex>>>,
}

impl ExternalDefinitionRuntime {
    fn from_config(cfg: ExternalDefinitionConfig) -> Self {
        Self {
            jdk_index: cfg.jdk_index,
            runtime_jdk: cfg.runtime_jdk,
            decompiler: cfg.decompiler,
            base_dependency_config: cfg.base_dependency_config,
            cache_root: cfg.cache_root,
            auto_decompile_class: cfg.auto_decompile_class,
            dep_indexes: Mutex::new(HashMap::new()),
        }
    }

    fn discover_from_host() -> Self {
        let paths = KovaPaths::resolve().ok();
        let jdks = paths
            .as_ref()
            .map(|p| discover_jdks(&JdkDiscoveryRoots::from_env_and_paths(p)))
            .unwrap_or_default();
        let jdk_index = jdks
            .iter()
            .find(|j| j.jmods_dir.is_some())
            .or_else(|| jdks.first())
            .map_or_else(
                || Arc::new(JdkOnDemandIndex::empty()),
                |j| Arc::new(JdkOnDemandIndex::from_jdk_home(&j.home_dir)),
            );
        let runtime_jdk = select_tool_runtime_jdk(&jdks, 17)
            .ok()
            .or_else(|| jdks.first())
            .cloned();
        let decompiler = paths
            .as_ref()
            .and_then(|p| discover_vineflower_decompiler(p, &jdks));
        let cache_root = paths.as_ref().map_or_else(
            || std::env::temp_dir().join("kova-decompiled"),
            |p| p.cache_dir().join("decompiled"),
        );
        let settings = default_effective_config().settings.jvm;
        Self::from_config(ExternalDefinitionConfig {
            jdk_index,
            runtime_jdk,
            decompiler,
            base_dependency_config: DependencyArchiveConfig::from_host_env(None),
            cache_root,
            auto_decompile_class: settings.auto_decompile_class,
        })
    }

    fn dependency_index_for(&self, project_root: Option<&Path>) -> Arc<DependencyArchiveIndex> {
        let key = project_root.map(Path::to_path_buf);
        let mut guard = self
            .dep_indexes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(existing) = guard.get(&key) {
            return Arc::clone(existing);
        }
        let mut cfg = self.base_dependency_config.clone();
        cfg.project_root.clone_from(&key);
        let created = Arc::new(DependencyArchiveIndex::new(cfg));
        guard.insert(key, Arc::clone(&created));
        created
    }
}

/// Optional Level-2 LSP + Level-1.5 bytecode/Vineflower backend maintained by the host.
#[derive(Clone, Default)]
pub struct LspDefinitionBackend {
    inner: Arc<Mutex<Option<ActiveLspBackend>>>,
    external: Arc<OnceLock<Arc<ExternalDefinitionRuntime>>>,
}

impl std::fmt::Debug for LspDefinitionBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspDefinitionBackend")
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
struct ActiveLspBackend {
    client: Arc<JdtlsClient>,
    resolver: DefinitionResolver,
}

impl LspDefinitionBackend {
    /// Creates an empty backend with no active `jdt.ls` client yet; Level-1.5 external lookup
    /// initializes lazily on first background request.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a backend pre-configured with explicit Level-1.5 external paths/indexes (useful
    /// in isolated tests that must not touch `$HOME`).
    #[must_use]
    pub fn with_external_config(config: ExternalDefinitionConfig) -> Self {
        let lock = OnceLock::new();
        drop(lock.set(Arc::new(ExternalDefinitionRuntime::from_config(config))));
        Self {
            inner: Arc::new(Mutex::new(None)),
            external: Arc::new(lock),
        }
    }

    /// Attaches an active [`JdtlsClient`] and [`DefinitionResolver`] for Level-2 queries.
    pub fn attach_client(&self, client: Arc<JdtlsClient>, resolver: DefinitionResolver) {
        *self.inner.lock().unwrap_or_else(PoisonError::into_inner) =
            Some(ActiveLspBackend { client, resolver });
    }

    fn current(&self) -> Option<ActiveLspBackend> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn get_or_init_external(&self) -> Arc<ExternalDefinitionRuntime> {
        Arc::clone(
            self.external
                .get_or_init(|| Arc::new(ExternalDefinitionRuntime::discover_from_host())),
        )
    }
}

/// Builds a [`DefinitionResolver`] from default `KovaPaths` and `[jvm]` settings when a
/// `VineflowerDecompiler` is available, or a cache-only resolver otherwise.
#[must_use]
pub fn default_definition_resolver(
    paths: &KovaPaths,
    settings: &JvmSettings,
    decompiler: Option<VineflowerDecompiler>,
) -> DefinitionResolver {
    match decompiler {
        Some(decompiler) => DefinitionResolver::from_settings(settings, decompiler),
        None => DefinitionResolver::new(
            paths.cache_dir().join("decompiled"),
            settings.auto_decompile_class,
        ),
    }
}

/// Creates a [`JdtlsSessionManager`] configured with [`KovaPaths`] for optional background
/// Java language-server management.
#[must_use]
pub fn build_session_manager(paths: KovaPaths) -> JdtlsSessionManager {
    JdtlsSessionManager::new(
        paths,
        SessionManagerConfig::default(),
        Arc::new(OsProcessMemoryProbe),
    )
}

/// Registers `project_root` in `manager` using default `EffectiveConfig` when `jdt.ls` launch
/// configuration is constructed.
#[must_use]
pub fn default_effective_config() -> EffectiveConfig {
    EffectiveConfig::merge(&KovaSettings::default(), None)
}

/// Creates the [`DefinitionSink`] installed on [`kova_ui::WorkspaceShell`].
#[must_use]
pub fn definition_sink(runtime: Handle, events: mpsc::UnboundedSender<AppEvent>) -> DefinitionSink {
    definition_sink_with_backend(runtime, events, LspDefinitionBackend::new())
}

/// Creates a [`DefinitionSink`] backed by an explicit [`LspDefinitionBackend`].
#[must_use]
pub fn definition_sink_with_backend(
    runtime: Handle,
    events: mpsc::UnboundedSender<AppEvent>,
    backend: LspDefinitionBackend,
) -> DefinitionSink {
    Arc::new(move |request: DefinitionRequest| {
        let events = events.clone();
        let backend = backend.clone();
        runtime.spawn(async move {
            let request_id = request.request_id;
            let outcome = resolve_definition_request(request, Some(&backend)).await;
            drop(events.send(AppEvent::DefinitionResolved {
                request_id,
                outcome,
            }));
        });
    })
}

/// Resolves one [`DefinitionRequest`] using Level 1 first, optional Level 2 (`jdt.ls`) when
/// connected, and Level-1.5 direct JDK/dependency bytecode decompilation as an immediate fallback.
pub async fn resolve_definition_request(
    request: DefinitionRequest,
    backend: Option<&LspDefinitionBackend>,
) -> DefinitionOutcome {
    let source_path = request.source_path.clone();
    let source_text = request.source_text.clone();
    let cursor_byte = request.cursor_byte;
    let project_root = request.project_root.clone();
    let open_buffers = request.open_buffers.clone();
    let workspace_files = request.workspace_files.clone();
    let language = SourceLanguageKind::from_path(&source_path);

    let level1 = tokio::task::spawn_blocking(move || {
        let buffer = Buffer::from_text(&source_text);
        let snapshot = buffer.snapshot();
        let query = extract_symbol_query(&snapshot, &source_path, cursor_byte)?;
        let symbol_name = query.ident.clone();
        let resolved = resolve_local_definition(
            &query,
            &source_text,
            &open_buffers,
            project_root.as_deref(),
            &workspace_files,
        );
        Some((symbol_name, snapshot, query, resolved))
    })
    .await;

    let Ok(Some((symbol_name, snapshot, query, resolved))) = level1 else {
        return DefinitionOutcome::NoSymbolAtCursor;
    };

    if let Some(hit) = resolved {
        return DefinitionOutcome::Found {
            target: DefinitionTargetKind::WorkspaceFile { rel_path: hit.path },
            line: hit.line,
            column: hit.column,
            symbol_name: hit.symbol_name,
        };
    }

    // Level 2: query jdt.ls + Vineflower for Java symbols outside workspace (e.g. JDK / .jar).
    if language == SourceLanguageKind::Java
        && let Some(active) = backend.and_then(LspDefinitionBackend::current)
        && matches!(
            active.client.state(),
            LspLifecycleState::Ready | LspLifecycleState::IndexingProject
        )
    {
        let abs_source = match (&request.project_root, &request.source_path) {
            (Some(root), rel) if rel.is_relative() => root.join(rel),
            (_, path) => path.clone(),
        };
        if let Ok(uri) = path_to_lsp_uri(&abs_source) {
            let pos = utf16_point_at_byte(&snapshot, cursor_byte);
            let lsp_result = tokio::time::timeout(
                LSP_DEFINITION_TIMEOUT,
                active
                    .resolver
                    .go_to_definition(&active.client, uri.as_str(), pos),
            )
            .await;
            if let Ok(Ok(mut targets)) = lsp_result
                && let Some(target) = targets.drain(..).next()
            {
                return map_definition_target(target, request.project_root.as_deref(), symbol_name);
            }
        }
    }

    // Level 1.5: zero-jdt.ls direct lookup + Vineflower/cafebabe decompilation for Java & Kotlin.
    if matches!(
        language,
        SourceLanguageKind::Java | SourceLanguageKind::Kotlin
    ) && let Some(backend) = backend
        && let Some(outcome) = resolve_level15_external(
            backend,
            &query,
            &request.source_text,
            request.project_root.clone(),
            &symbol_name,
        )
        .await
    {
        return outcome;
    }

    DefinitionOutcome::NotFound { symbol_name }
}

async fn resolve_level15_external(
    backend: &LspDefinitionBackend,
    query: &SymbolQuery,
    source_text: &str,
    project_root: Option<PathBuf>,
    symbol_name: &str,
) -> Option<DefinitionOutcome> {
    let candidates = resolve_external_fqcn_candidates(query, source_text);
    if candidates.fqcns.is_empty() {
        return None;
    }

    let backend_cloned = backend.clone();
    let candidates_cloned = candidates.clone();
    let located_res = tokio::task::spawn_blocking(move || {
        let runtime = backend_cloned.get_or_init_external();
        let dep_index = runtime.dependency_index_for(project_root.as_deref());
        let locator = ExternalClassLocator::new(Arc::clone(&runtime.jdk_index), dep_index);
        let located = locate_external_target(&locator, &candidates_cloned)?;
        Some((runtime, located))
    })
    .await
    .ok()??;

    let (runtime, located) = located_res;
    let target_simple = simple_binary_name(&located.fqcn).to_owned();

    // Primary path: Vineflower (if installed & enabled) with persistent cache.
    if runtime.auto_decompile_class
        && let Some(decompiler) = &runtime.decompiler
    {
        let target = DecompileTarget::ArchiveEntry {
            archive_path: located.archive_path.clone(),
            entry_name: located.outer_entry_name.clone(),
        };
        if let Ok(decompiled) = decompiler.decompile(&target).await {
            let (line, column) = find_member_or_type_declaration(
                &decompiled.source_code,
                candidates.member_name.as_deref(),
                &target_simple,
            );
            return Some(DefinitionOutcome::Found {
                target: DefinitionTargetKind::DecompiledClass {
                    fqcn: decompiled.fqcn,
                    cache_file: decompiled.cache_file,
                    source_text: decompiled.source_code,
                },
                line,
                column,
                symbol_name: symbol_name.to_owned(),
            });
        }
    }

    // Immediate fallback path: pure-Rust cafebabe decompiler (and optional javap -p).
    let (java_src, cache_file) = decompile_with_fallback(&runtime, &located).await?;

    let (line, column) = find_member_or_type_declaration(
        &java_src,
        candidates.member_name.as_deref(),
        &target_simple,
    );
    Some(DefinitionOutcome::Found {
        target: DefinitionTargetKind::DecompiledClass {
            fqcn: located.fqcn,
            cache_file,
            source_text: java_src,
        },
        line,
        column,
        symbol_name: symbol_name.to_owned(),
    })
}

async fn decompile_with_fallback(
    runtime: &ExternalDefinitionRuntime,
    located: &LocatedClassBytes,
) -> Option<(String, PathBuf)> {
    let located_for_fallback = located.clone();
    let cache_root = runtime.cache_root.clone();
    let fallback_java = tokio::task::spawn_blocking(move || {
        let source_label = format!(
            "{}!/{}",
            located_for_fallback.archive_path.display(),
            located_for_fallback.entry_name
        );
        let java_src = decompile_class_bytes_to_java(
            &located_for_fallback.bytes,
            &located_for_fallback.fqcn,
            &source_label,
        )
        .ok()?;
        let cache_file =
            cache_bytecode_decompilation(&cache_root, &located_for_fallback, &java_src).ok()?;
        Some((java_src, cache_file))
    })
    .await
    .ok()
    .flatten();

    if let Some(pair) = fallback_java {
        return Some(pair);
    }

    let jdk = runtime.runtime_jdk.as_ref()?;
    let java_src = decompile_with_javap_fallback(
        jdk,
        &located.archive_path,
        &located.entry_name,
        &located.bytes,
    )
    .await?;
    let cache_root = runtime.cache_root.clone();
    let located_clone = located.clone();
    let cache_file = tokio::task::spawn_blocking(move || {
        cache_bytecode_decompilation(&cache_root, &located_clone, &java_src).ok()
    })
    .await
    .ok()??;
    let source_text = std::fs::read_to_string(&cache_file).ok()?;
    Some((source_text, cache_file))
}

fn locate_external_target(
    locator: &ExternalClassLocator,
    candidates: &ExternalTargetCandidates,
) -> Option<LocatedClassBytes> {
    if candidates.field_chain.is_empty() {
        return locator.locate_candidates(&candidates.fqcns);
    }

    for start_fqcn in &candidates.fqcns {
        let mut cur_fqcn = start_fqcn.clone();
        let mut chain_ok = true;
        for field_name in &candidates.field_chain {
            if let Some(next_fqcn) = locator
                .jdk_index()
                .resolve_field_type(&cur_fqcn, field_name, None)
            {
                cur_fqcn = next_fqcn;
                continue;
            }
            let Some(owner_bytes) = locator.locate_candidates(std::slice::from_ref(&cur_fqcn))
            else {
                chain_ok = false;
                break;
            };
            let Some(next_fqcn) = resolve_field_type_from_bytes(&owner_bytes, field_name) else {
                chain_ok = false;
                break;
            };
            cur_fqcn = next_fqcn;
        }
        if chain_ok && let Some(target) = locator.locate_candidates(&[cur_fqcn]) {
            return Some(target);
        }
    }

    locator.locate_candidates(&candidates.fqcns)
}

fn resolve_field_type_from_bytes(located: &LocatedClassBytes, field_name: &str) -> Option<String> {
    let opts = BytecodeScanOptions {
        include_members: true,
        include_private_members: true,
        include_package_private_classes: true,
        skip_anonymous_and_synthetic: false,
        ..BytecodeScanOptions::default()
    };
    let origin = JvmSourceOrigin::JarEntry {
        jar_path: located.archive_path.clone(),
        entry_name: located.entry_name.clone(),
    };
    let unit = parse_class_bytes(&located.bytes, origin, &opts).ok()??;
    let field = unit
        .members
        .into_iter()
        .find(|m| m.kind == JvmSymbolKind::Field && m.name == field_name)?;
    let trimmed = field.descriptor.trim();
    trimmed
        .strip_prefix('L')
        .and_then(|s| s.strip_suffix(';'))
        .map(|internal| internal.replace('/', "."))
}

fn simple_binary_name(fqcn: &str) -> &str {
    let simple = fqcn.rsplit('.').next().unwrap_or(fqcn);
    let mut parts = simple.split('$');
    let outer = parts.next().unwrap_or(simple);
    parts
        .rev()
        .find(|p| !p.is_empty() && !p.chars().all(|c| c.is_ascii_digit()))
        .unwrap_or(outer)
}

/// Locates a 1-based `(line, column)` for `member_name` (if present) or `type_simple_name`
/// inside decompiled Java source code (`KOVA-061`).
#[must_use]
pub fn find_member_or_type_declaration(
    source: &str,
    member_name: Option<&str>,
    type_simple_name: &str,
) -> (usize, usize) {
    if let Some(member) = member_name
        && !member.is_empty()
    {
        if let Some(pos) = find_type_decl_1_based(source, member) {
            return pos;
        }
        if let Some(pos) = find_member_decl_1_based(source, member) {
            return pos;
        }
    }
    if let Some(pos) = find_type_decl_1_based(source, type_simple_name) {
        return pos;
    }
    (1, 1)
}

fn find_type_decl_1_based(source: &str, name: &str) -> Option<(usize, usize)> {
    if name.is_empty() {
        return None;
    }
    if let Some(range) = find_declaration(source, name) {
        return Some((range.start.row + 1, range.start.column + 1));
    }
    // Also support `@interface Name` for annotations.
    for (row, line) in source.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*') {
            continue;
        }
        let needle = format!("@interface {name}");
        if let Some(start) = line.find(&needle) {
            let after = &line[start + needle.len()..];
            if after.chars().next().is_none_or(|c| !is_java_ident_char(c)) {
                let col = line[..start + "@interface ".len()].chars().count() + 1;
                return Some((row + 1, col));
            }
        }
    }
    None
}

fn find_member_decl_1_based(source: &str, member: &str) -> Option<(usize, usize)> {
    // Pass 1: method or constructor declaration (`member(`).
    for (row, line) in source.lines().enumerate() {
        let trimmed = line.trim_start();
        if is_non_decl_line(trimmed) {
            continue;
        }
        if let Some(col_byte) = find_method_decl_col_in_line(line, member) {
            let col = line[..col_byte].chars().count() + 1;
            return Some((row + 1, col));
        }
    }

    // Pass 2: field or enum constant declaration (`Type member;` / `Type member = ...;` / `MEMBER,`).
    for (row, line) in source.lines().enumerate() {
        let trimmed = line.trim_start();
        if is_non_decl_line(trimmed) {
            continue;
        }
        if let Some(col_byte) = find_field_or_enum_col_in_line(line, trimmed, member) {
            let col = line[..col_byte].chars().count() + 1;
            return Some((row + 1, col));
        }
    }

    None
}

fn is_non_decl_line(trimmed: &str) -> bool {
    trimmed.is_empty()
        || trimmed.starts_with("//")
        || trimmed.starts_with("/*")
        || trimmed.starts_with('*')
        || trimmed.starts_with("package ")
        || trimmed.starts_with("import ")
        || trimmed.starts_with('@')
}

fn is_java_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

fn find_method_decl_col_in_line(line: &str, member: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("if ")
        || trimmed.starts_with("if(")
        || trimmed.starts_with("for ")
        || trimmed.starts_with("for(")
        || trimmed.starts_with("while ")
        || trimmed.starts_with("while(")
        || trimmed.starts_with("switch ")
        || trimmed.starts_with("catch ")
        || trimmed.starts_with("return ")
        || trimmed.starts_with("throw ")
        || trimmed.starts_with("new ")
    {
        return None;
    }

    let mut from = 0;
    while let Some(found) = line[from..].find(member) {
        let start = from + found;
        let end = start + member.len();
        let before_ok = line[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !is_java_ident_char(c) && c != '.');
        let after = line[end..].trim_start();
        if before_ok && after.starts_with('(') {
            let prefix = line[..start].trim();
            if !prefix.contains(" = ")
                && !prefix.ends_with("new")
                && !prefix.ends_with("return")
                && !prefix.ends_with("throw")
            {
                return Some(start);
            }
        }
        from = end;
    }
    None
}

fn find_field_or_enum_col_in_line(line: &str, trimmed: &str, member: &str) -> Option<usize> {
    if !trimmed.contains(member) {
        return None;
    }
    let mut from = 0;
    while let Some(found) = line[from..].find(member) {
        let start = from + found;
        let end = start + member.len();
        let before_ok = line[..start]
            .chars()
            .next_back()
            .is_none_or(|c| c.is_whitespace() || c == ',');
        let after = line[end..].trim_start();
        let after_ok = after.starts_with(';') || after.starts_with('=') || after.starts_with(',');
        if before_ok && after_ok {
            let prefix = line[..start].trim();
            if !prefix.starts_with("return") && !prefix.starts_with("throw") {
                return Some(start);
            }
        }
        from = end;
    }
    None
}

/// Converts a `kova_lsp_dap` [`DefinitionTarget`] into a UI-ready [`DefinitionOutcome`].
#[must_use]
pub fn map_definition_target(
    target: DefinitionTarget,
    project_root: Option<&Path>,
    symbol_name: String,
) -> DefinitionOutcome {
    match target {
        DefinitionTarget::SourceFile {
            path,
            range,
            read_only,
        } => {
            let (line, column) = if let Ok(text) = std::fs::read_to_string(&path) {
                utf16_point_to_line_col(&text, range.start)
            } else {
                (range.start.row + 1, range.start.column + 1)
            };
            let target_kind = if !read_only
                && let Some(root) = project_root
                && let Ok(rel) = path.strip_prefix(root)
            {
                DefinitionTargetKind::WorkspaceFile {
                    rel_path: rel.to_path_buf(),
                }
            } else {
                DefinitionTargetKind::ExternalFile { path, read_only }
            };
            DefinitionOutcome::Found {
                target: target_kind,
                line,
                column,
                symbol_name,
            }
        }
        DefinitionTarget::DecompiledClass {
            fqcn,
            cache_file,
            source_text,
            range,
            ..
        } => {
            let (line, column) = utf16_point_to_line_col(&source_text, range.start);
            DefinitionOutcome::Found {
                target: DefinitionTargetKind::DecompiledClass {
                    fqcn,
                    cache_file,
                    source_text,
                },
                line,
                column,
                symbol_name,
            }
        }
    }
}

pub(crate) fn utf16_point_at_byte(
    snapshot: &kova_buffer::BufferSnapshot,
    byte: usize,
) -> Utf16Point {
    let clamped = snapshot.clamp_byte_to_char_boundary(byte.min(snapshot.len_bytes()));
    let Ok(pt) = snapshot.byte_to_point(clamped) else {
        return Utf16Point::new(0, 0);
    };
    let line_start = snapshot.rope().line_to_byte(pt.row);
    let col_utf16 = snapshot
        .slice_bytes(line_start..clamped)
        .map_or(0, |s| s.chunks().map(|c| c.encode_utf16().count()).sum());
    Utf16Point::new(pt.row, col_utf16)
}

fn utf16_point_to_line_col(text: &str, point: Utf16Point) -> (usize, usize) {
    let line = text.lines().nth(point.row).unwrap_or("");
    let mut utf16_seen = 0usize;
    let mut chars_seen = 0usize;
    for ch in line.chars() {
        if utf16_seen >= point.column {
            break;
        }
        utf16_seen += ch.len_utf16();
        chars_seen += 1;
    }
    (point.row + 1, chars_seen + 1)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use kova_buffer::Utf16Range;
    use kova_lsp_dap::lsp::{
        ClientRuntimeConfig, IncomingMessage, JsonRpcResponse, LspEvent, read_lsp_frame,
        write_lsp_frame,
    };
    use serde_json::json;
    use tokio::io::{AsyncWriteExt, BufReader};
    use tokio::sync::mpsc;

    use super::*;
    use crate::build_io_runtime;

    #[test]
    fn definition_sink_resolves_cross_file_java_import_and_same_file_symbol() {
        let runtime = build_io_runtime().expect("runtime");
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        let app_rel = PathBuf::from("src/main/java/dev/kova/demo/OrderApp.java");
        let svc_rel = PathBuf::from("src/main/java/dev/kova/orders/OrderService.java");
        std::fs::create_dir_all(root.join(app_rel.parent().unwrap())).unwrap();
        std::fs::create_dir_all(root.join(svc_rel.parent().unwrap())).unwrap();

        let app_src = "\
package dev.kova.demo;

import dev.kova.orders.OrderService;

public class OrderApp {
    public void run() {
        OrderService svc = new OrderService();
        helper();
    }
    private void helper() {}
}
";
        let svc_src = "\
package dev.kova.orders;

public class OrderService {
    public OrderService() {}
}
";
        std::fs::write(root.join(&app_rel), app_src).unwrap();
        std::fs::write(root.join(&svc_rel), svc_src).unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = definition_sink(runtime.handle().clone(), tx);

        // 1. Cross-file import jump
        let byte_import_target = app_src.find("OrderService svc").unwrap();
        sink(DefinitionRequest {
            request_id: 1,
            project_root: Some(root.to_path_buf()),
            source_path: app_rel.clone(),
            source_text: app_src.to_owned(),
            cursor_byte: byte_import_target,
            line: 7,
            column: 9,
            open_buffers: vec![(app_rel.clone(), app_src.to_owned())],
            workspace_files: vec![app_rel.clone(), svc_rel.clone()],
        });

        let event1 = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("timeout")
                .expect("event")
        });
        match event1 {
            AppEvent::DefinitionResolved {
                request_id: 1,
                outcome:
                    DefinitionOutcome::Found {
                        target: DefinitionTargetKind::WorkspaceFile { rel_path },
                        line,
                        column: _,
                        symbol_name,
                    },
            } => {
                assert_eq!(rel_path, svc_rel);
                assert_eq!(line, 3);
                assert_eq!(symbol_name, "OrderService");
            }
            other => panic!("unexpected event1: {other:?}"),
        }

        // 2. Same-file method jump
        let byte_helper = app_src.find("helper();").unwrap();
        sink(DefinitionRequest {
            request_id: 2,
            project_root: Some(root.to_path_buf()),
            source_path: app_rel.clone(),
            source_text: app_src.to_owned(),
            cursor_byte: byte_helper,
            line: 8,
            column: 9,
            open_buffers: vec![(app_rel.clone(), app_src.to_owned())],
            workspace_files: vec![app_rel.clone(), svc_rel],
        });
        let event2 = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("timeout")
                .expect("event")
        });
        match event2 {
            AppEvent::DefinitionResolved {
                request_id: 2,
                outcome:
                    DefinitionOutcome::Found {
                        target: DefinitionTargetKind::WorkspaceFile { rel_path },
                        line,
                        column: _,
                        symbol_name,
                    },
            } => {
                assert_eq!(rel_path, app_rel);
                assert_eq!(line, 10);
                assert_eq!(symbol_name, "helper");
            }
            other => panic!("unexpected event2: {other:?}"),
        }
    }

    fn spawn_mock_jdtls_server(
        server_read: tokio::io::DuplexStream,
        mut writer: tokio::io::DuplexStream,
        external_uri: String,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut reader = BufReader::new(server_read);
            while let Ok(Some(frame)) = read_lsp_frame(&mut reader).await {
                let Ok(IncomingMessage::Request(req)) = IncomingMessage::from_slice(&frame) else {
                    continue;
                };
                let resp = match req.method.as_str() {
                    "initialize" => JsonRpcResponse::success(
                        req.id,
                        json!({ "capabilities": { "definitionProvider": true } }),
                    ),
                    "textDocument/definition" => JsonRpcResponse::success(
                        req.id,
                        json!([{
                            "uri": external_uri,
                            "range": {
                                "start": { "line": 1, "character": 19 },
                                "end": { "line": 1, "character": 25 }
                            }
                        }]),
                    ),
                    _ => JsonRpcResponse::success(req.id, json!(null)),
                };
                let payload = serde_json::to_vec(&resp).unwrap();
                write_lsp_frame(&mut writer, &payload).await.unwrap();
                if req.method == "initialize" {
                    let ready = serde_json::to_vec(&json!({
                        "jsonrpc": "2.0",
                        "method": "language/status",
                        "params": { "type": "ServiceReady", "message": "Ready" }
                    }))
                    .unwrap();
                    write_lsp_frame(&mut writer, &ready).await.unwrap();
                }
            }
            drop(writer.shutdown().await);
        })
    }

    #[test]
    fn decompiled_target_maps_to_read_only_outcome() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let cache_root = root.join(".cache");
        let decompiled_cache = cache_root.join("java/lang/String.java");
        let decompiled_src = "package java.lang;\npublic final class String {\n}\n";
        let mapped = map_definition_target(
            DefinitionTarget::DecompiledClass {
                fqcn: "java.lang.String".to_owned(),
                archive_path: None,
                cache_file: decompiled_cache.clone(),
                source_text: decompiled_src.to_owned(),
                range: Utf16Range::new(Utf16Point::new(1, 19), Utf16Point::new(1, 25)),
                from_cache: false,
                read_only: true,
            },
            Some(&root),
            "String".to_owned(),
        );
        assert_eq!(
            mapped,
            DefinitionOutcome::Found {
                target: DefinitionTargetKind::DecompiledClass {
                    fqcn: "java.lang.String".to_owned(),
                    cache_file: decompiled_cache,
                    source_text: decompiled_src.to_owned(),
                },
                line: 2,
                column: 20,
                symbol_name: "String".to_owned(),
            }
        );
    }

    #[test]
    fn level2_lsp_roundtrip_resolves_external_definition() {
        let runtime = build_io_runtime().expect("runtime");
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let cache_root = root.join(".cache");
        let app_rel = PathBuf::from("src/App.java");
        let app_abs = root.join(&app_rel);
        std::fs::create_dir_all(app_abs.parent().unwrap()).unwrap();
        let app_src = "public class App { String name; }\n";
        std::fs::write(&app_abs, app_src).unwrap();

        let decompiled_src = "package java.lang;\npublic final class String {\n}\n";
        let external_dir = tempfile::tempdir().expect("external tempdir");
        let external_jdk_src = external_dir.path().join("java/lang/String.java");
        std::fs::create_dir_all(external_jdk_src.parent().unwrap()).unwrap();
        std::fs::write(&external_jdk_src, decompiled_src).unwrap();
        let external_uri = path_to_lsp_uri(&external_jdk_src).unwrap().to_string();

        runtime.block_on(async {
            let (client_read, server_write) = tokio::io::duplex(64 * 1024);
            let (server_read, client_write) = tokio::io::duplex(64 * 1024);
            let server_task = spawn_mock_jdtls_server(server_read, server_write, external_uri);

            let client = Arc::new(
                JdtlsClient::start_with_streams(
                    client_read,
                    client_write,
                    ClientRuntimeConfig::new(&root),
                )
                .await
                .expect("client start"),
            );
            let mut events = client.subscribe_events();
            while let Ok(ev) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await {
                if matches!(
                    ev,
                    Ok(LspEvent::StateChanged {
                        current: LspLifecycleState::Ready,
                        ..
                    })
                ) {
                    break;
                }
            }

            let backend = LspDefinitionBackend::new();
            backend.attach_client(
                Arc::clone(&client),
                DefinitionResolver::new(&cache_root, true),
            );

            let (tx, mut rx) = mpsc::unbounded_channel();
            let sink = definition_sink_with_backend(Handle::current(), tx, backend);
            sink(DefinitionRequest {
                request_id: 7,
                project_root: Some(root.clone()),
                source_path: app_rel,
                source_text: app_src.to_owned(),
                cursor_byte: app_src.find("String").unwrap(),
                line: 1,
                column: 20,
                open_buffers: vec![],
                workspace_files: vec![],
            });

            let event = tokio::time::timeout(Duration::from_secs(3), rx.recv())
                .await
                .expect("sink timeout")
                .expect("event");
            match event {
                AppEvent::DefinitionResolved {
                    request_id: 7,
                    outcome:
                        DefinitionOutcome::Found {
                            target: DefinitionTargetKind::ExternalFile { path, .. },
                            line,
                            column,
                            symbol_name,
                        },
                } => {
                    assert_eq!(path, external_jdk_src);
                    assert_eq!(line, 2);
                    assert_eq!(column, 20);
                    assert_eq!(symbol_name, "String");
                }
                other => panic!("unexpected LSP outcome: {other:?}"),
            }

            drop(client.shutdown_graceful(Duration::from_secs(1)).await);
            server_task.abort();
        });
    }

    fn cp_add_utf8(cp: &mut Vec<Vec<u8>>, s: &str) -> u16 {
        let mut e = vec![1];
        e.extend_from_slice(&u16::try_from(s.len()).unwrap().to_be_bytes());
        e.extend_from_slice(s.as_bytes());
        cp.push(e);
        u16::try_from(cp.len()).unwrap()
    }

    fn cp_add_class(cp: &mut Vec<Vec<u8>>, utf8_idx: u16) -> u16 {
        let mut e = vec![7];
        e.extend_from_slice(&utf8_idx.to_be_bytes());
        cp.push(e);
        u16::try_from(cp.len()).unwrap()
    }

    fn build_test_class_bytes(
        internal_name: &str,
        super_internal: &str,
        access_flags: u16,
        fields: &[(&str, &str, u16)],
        methods: &[(&str, &str, u16)],
    ) -> Vec<u8> {
        let mut cp = Vec::new();
        let this_u = cp_add_utf8(&mut cp, internal_name);
        let this_cls = cp_add_class(&mut cp, this_u);
        let sup_u = cp_add_utf8(&mut cp, super_internal);
        let sup_cls = cp_add_class(&mut cp, sup_u);

        let mut field_entries = Vec::with_capacity(fields.len());
        for (name, desc, flags) in fields {
            let n = cp_add_utf8(&mut cp, name);
            let d = cp_add_utf8(&mut cp, desc);
            field_entries.push((n, d, *flags));
        }
        let mut method_entries = Vec::with_capacity(methods.len());
        for (name, desc, flags) in methods {
            let n = cp_add_utf8(&mut cp, name);
            let d = cp_add_utf8(&mut cp, desc);
            method_entries.push((n, d, *flags));
        }

        let mut out = Vec::new();
        out.extend_from_slice(&0xCAFE_BABEu32.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&61u16.to_be_bytes());
        out.extend_from_slice(&u16::try_from(cp.len() + 1).unwrap().to_be_bytes());
        for entry in cp {
            out.extend_from_slice(&entry);
        }
        out.extend_from_slice(&access_flags.to_be_bytes());
        out.extend_from_slice(&this_cls.to_be_bytes());
        out.extend_from_slice(&sup_cls.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes()); // interfaces
        out.extend_from_slice(&u16::try_from(field_entries.len()).unwrap().to_be_bytes());
        for (n, d, flags) in field_entries {
            out.extend_from_slice(&flags.to_be_bytes());
            out.extend_from_slice(&n.to_be_bytes());
            out.extend_from_slice(&d.to_be_bytes());
            out.extend_from_slice(&0u16.to_be_bytes());
        }
        out.extend_from_slice(&u16::try_from(method_entries.len()).unwrap().to_be_bytes());
        for (n, d, flags) in method_entries {
            out.extend_from_slice(&flags.to_be_bytes());
            out.extend_from_slice(&n.to_be_bytes());
            out.extend_from_slice(&d.to_be_bytes());
            out.extend_from_slice(&0u16.to_be_bytes());
        }
        out.extend_from_slice(&0u16.to_be_bytes()); // attributes
        out
    }

    fn write_test_zip(path: &Path, entries: &[(&str, Vec<u8>)]) {
        use std::io::Write as _;
        use zip::ZipWriter;
        use zip::write::SimpleFileOptions;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let file = std::fs::File::create(path).unwrap();
        let mut zip = ZipWriter::new(file);
        let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, bytes) in entries {
            zip.start_file(*name, opts).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
    }

    fn resolve_sync(
        runtime: &tokio::runtime::Runtime,
        backend: &LspDefinitionBackend,
        project_root: Option<PathBuf>,
        source_path: PathBuf,
        source_text: &str,
        needle: &str,
    ) -> DefinitionOutcome {
        let cursor_byte = source_text.find(needle).expect("needle in source_text");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = definition_sink_with_backend(runtime.handle().clone(), tx, backend.clone());
        sink(DefinitionRequest {
            request_id: 100,
            project_root,
            source_path,
            source_text: source_text.to_owned(),
            cursor_byte,
            line: 1,
            column: 1,
            open_buffers: vec![],
            workspace_files: vec![],
        });
        let ev = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("sink timeout")
                .expect("event")
        });
        match ev {
            AppEvent::DefinitionResolved { outcome, .. } => outcome,
            other => panic!("unexpected event: {other:?}"),
        }
    }

    fn build_sample_jdk_jmod(jmods_dir: &Path) {
        let java_object_bytes =
            build_test_class_bytes("java/lang/Object", "java/lang/Object", 0x0021, &[], &[]);
        let string_bytes = build_test_class_bytes(
            "java/lang/String",
            "java/lang/Object",
            0x0031,
            &[],
            &[("length", "()I", 0x0001)],
        );
        let objects_bytes = build_test_class_bytes(
            "java/util/Objects",
            "java/lang/Object",
            0x0031,
            &[],
            &[(
                "requireNonNull",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                0x0009,
            )],
        );
        let map_bytes =
            build_test_class_bytes("java/util/Map", "java/lang/Object", 0x0601, &[], &[]);
        let map_entry_bytes = build_test_class_bytes(
            "java/util/Map$Entry",
            "java/lang/Object",
            0x0601,
            &[],
            &[("getKey", "()Ljava/lang/Object;", 0x0401)],
        );
        let system_bytes = build_test_class_bytes(
            "java/lang/System",
            "java/lang/Object",
            0x0031,
            &[("out", "Ljava/io/PrintStream;", 0x0019)],
            &[],
        );
        let print_stream_bytes = build_test_class_bytes(
            "java/io/PrintStream",
            "java/lang/Object",
            0x0021,
            &[],
            &[("println", "(Ljava/lang/String;)V", 0x0001)],
        );

        write_test_zip(
            &jmods_dir.join("java.base.jmod"),
            &[
                ("classes/java/lang/Object.class", java_object_bytes),
                ("classes/java/lang/String.class", string_bytes),
                ("classes/java/util/Objects.class", objects_bytes),
                ("classes/java/util/Map.class", map_bytes),
                ("classes/java/util/Map$Entry.class", map_entry_bytes),
                ("classes/java/lang/System.class", system_bytes),
                ("classes/java/io/PrintStream.class", print_stream_bytes),
            ],
        );
    }

    fn assert_decompiled_outcome(
        outcome: DefinitionOutcome,
        expected_fqcn: &str,
        expected_line_substr: &str,
    ) -> (PathBuf, String) {
        match outcome {
            DefinitionOutcome::Found {
                target:
                    DefinitionTargetKind::DecompiledClass {
                        fqcn,
                        cache_file,
                        source_text,
                    },
                line,
                column,
                ..
            } => {
                assert_eq!(fqcn, expected_fqcn);
                assert_eq!(
                    cache_file.extension().and_then(|e| e.to_str()),
                    Some("java")
                );
                assert!(cache_file.is_file());
                assert!(column >= 1);
                let line_txt = source_text.lines().nth(line - 1).unwrap();
                assert!(line_txt.contains(expected_line_substr), "{line_txt}");
                (cache_file, source_text)
            }
            other => panic!("unexpected outcome for {expected_fqcn}: {other:?}"),
        }
    }

    #[test]
    fn level15_resolves_jdk_jmod_class_with_bytecode_fallback_without_jdtls() {
        let runtime = build_io_runtime().expect("runtime");
        let dir = tempfile::tempdir().expect("tempdir");
        let jmods_dir = dir.path().join("jdk/jmods");
        let cache_root = dir.path().join("cache/decompiled");
        build_sample_jdk_jmod(&jmods_dir);

        let backend = LspDefinitionBackend::with_external_config(ExternalDefinitionConfig {
            jdk_index: Arc::new(JdkOnDemandIndex::from_jmods_dir(jmods_dir)),
            runtime_jdk: None,
            decompiler: None,
            base_dependency_config: DependencyArchiveConfig::default(),
            cache_root: cache_root.clone(),
            auto_decompile_class: true,
        });

        let src = "\
package dev.kova.demo;

import java.util.Objects;
import java.util.Map;

public class Demo {
    public void run(String input) {
        Objects.requireNonNull(input);
        Map.Entry<String, String> entry = null;
        System.out.println(input);
    }
}
";
        let src_path = PathBuf::from("src/Demo.java");

        let (str_cache, _) = assert_decompiled_outcome(
            resolve_sync(
                &runtime,
                &backend,
                None,
                src_path.clone(),
                src,
                "String input",
            ),
            "java.lang.String",
            "class String",
        );
        assert!(str_cache.starts_with(cache_root.join("bytecode")));

        let (req_cache, req_text) = assert_decompiled_outcome(
            resolve_sync(
                &runtime,
                &backend,
                None,
                src_path.clone(),
                src,
                "requireNonNull(input)",
            ),
            "java.util.Objects",
            "requireNonNull(",
        );
        assert_decompiled_outcome(
            resolve_sync(
                &runtime,
                &backend,
                None,
                src_path.clone(),
                src,
                "Entry<String",
            ),
            "java.util.Map$Entry",
            "interface Entry",
        );
        assert_decompiled_outcome(
            resolve_sync(&runtime, &backend, None, src_path, src, "println(input)"),
            "java.io.PrintStream",
            "println(",
        );

        // Recursive Cmd+Click inside decompiled Objects.java on `Object`.
        assert_decompiled_outcome(
            resolve_sync(
                &runtime,
                &backend,
                None,
                req_cache,
                &req_text,
                "Object requireNonNull",
            ),
            "java.lang.Object",
            "class Object",
        );
    }

    #[test]
    fn level15_resolves_gradle_maven_and_project_libs_jars() {
        let runtime = build_io_runtime().expect("runtime");
        let dir = tempfile::tempdir().expect("tempdir");
        let project = dir.path().join("workspace");
        let fake_home = dir.path().join("home");
        let cache_root = dir.path().join("cache/decompiled");

        let libs_bytes = build_test_class_bytes(
            "com/libs/LocalDep",
            "java/lang/Object",
            0x0021,
            &[],
            &[("execute", "()V", 0x0001)],
        );
        let gradle_bytes = build_test_class_bytes(
            "com/gradle/GradleLib",
            "java/lang/Object",
            0x0021,
            &[],
            &[("compute", "()I", 0x0009)],
        );
        let maven_bytes = build_test_class_bytes(
            "org/maven/MavenApi",
            "java/lang/Object",
            0x0021,
            &[],
            &[("ping", "()V", 0x0009)],
        );

        // Sources jar must be ignored.
        write_test_zip(
            &project.join("libs/local-sources.jar"),
            &[("com/libs/LocalDep.class", b"NOT_A_CLASS".to_vec())],
        );
        write_test_zip(
            &project.join("libs/local.jar"),
            &[("com/libs/LocalDep.class", libs_bytes)],
        );
        write_test_zip(
            &fake_home
                .join(".gradle/caches/modules-2/files-2.1/com.gradle/lib/1.0/hash/lib-1.0.jar"),
            &[("com/gradle/GradleLib.class", gradle_bytes)],
        );
        write_test_zip(
            &fake_home.join(".m2/repository/org/maven/api/1.0/api-1.0.jar"),
            &[("org/maven/MavenApi.class", maven_bytes)],
        );

        let backend = LspDefinitionBackend::with_external_config(ExternalDefinitionConfig {
            jdk_index: Arc::new(JdkOnDemandIndex::empty()),
            runtime_jdk: None,
            decompiler: None,
            base_dependency_config: DependencyArchiveConfig::from_home_and_project(
                Some(&fake_home),
                None,
            ),
            cache_root,
            auto_decompile_class: true,
        });

        let src = "\
package app;
import com.libs.LocalDep;
import com.gradle.GradleLib;
import org.maven.MavenApi;

public class App {
    void go(LocalDep d) {
        d.execute();
        GradleLib.compute();
        MavenApi.ping();
    }
}
";
        let src_path = PathBuf::from("src/App.java");

        for (needle, expected_fqcn, expected_member) in [
            ("execute()", "com.libs.LocalDep", "execute("),
            ("compute()", "com.gradle.GradleLib", "compute("),
            ("ping()", "org.maven.MavenApi", "ping("),
        ] {
            let outcome = resolve_sync(
                &runtime,
                &backend,
                Some(project.clone()),
                src_path.clone(),
                src,
                needle,
            );
            match outcome {
                DefinitionOutcome::Found {
                    target:
                        DefinitionTargetKind::DecompiledClass {
                            fqcn,
                            cache_file,
                            source_text,
                        },
                    line,
                    ..
                } => {
                    assert_eq!(fqcn, expected_fqcn);
                    assert!(cache_file.is_file());
                    let line_txt = source_text.lines().nth(line - 1).unwrap();
                    assert!(line_txt.contains(expected_member), "{line_txt}");
                }
                other => panic!("unexpected outcome for {needle}: {other:?}"),
            }
        }
    }

    #[cfg(unix)]
    fn write_exec_script(path: &Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::write(path, contents).unwrap();
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    #[cfg(unix)]
    fn dummy_test_jdk(home: &Path, java_bin: PathBuf, jmods_dir: PathBuf) -> JdkInstallation {
        use kova_toolchain::{JavaVersion, JdkSource, JdkVendor};
        JdkInstallation {
            home_dir: home.to_path_buf(),
            java_bin,
            jmods_dir: Some(jmods_dir),
            version: JavaVersion::new(21, 0, 0, None),
            vendor: JdkVendor::Unknown("Test".to_owned()),
            arch: "aarch64".to_owned(),
            source: JdkSource::EnvJavaHome,
        }
    }

    #[cfg(unix)]
    #[test]
    fn level15_prefers_vineflower_when_available_and_falls_back_on_error() {
        let runtime = build_io_runtime().expect("runtime");
        let dir = tempfile::tempdir().expect("tempdir");
        let jmods_dir = dir.path().join("jdk/jmods");
        let cache_root = dir.path().join("cache/decompiled");
        let vf_jar = dir.path().join("vineflower.jar");
        std::fs::write(&vf_jar, b"PK\x03\x04").unwrap();
        build_sample_jdk_jmod(&jmods_dir);

        let ok_java = dir.path().join("mock-java-ok.sh");
        write_exec_script(
            &ok_java,
            "#!/bin/sh\nfor arg in \"$@\"; do out_dir=\"$arg\"; done\nmkdir -p \"$out_dir/java/util\"\ncat <<'EOF' > \"$out_dir/java/util/Objects.java\"\npackage java.util;\npublic final class Objects {\n    public static <T> T requireNonNull(T obj) { return obj; }\n}\nEOF\n",
        );
        let ok_jdk = dummy_test_jdk(&dir.path().join("jdk"), ok_java, jmods_dir.clone());
        let backend_ok = LspDefinitionBackend::with_external_config(ExternalDefinitionConfig {
            jdk_index: Arc::new(JdkOnDemandIndex::from_jmods_dir(jmods_dir.clone())),
            runtime_jdk: Some(ok_jdk.clone()),
            decompiler: Some(VineflowerDecompiler::new(
                vf_jar.clone(),
                ok_jdk,
                cache_root.clone(),
            )),
            base_dependency_config: DependencyArchiveConfig::default(),
            cache_root: cache_root.clone(),
            auto_decompile_class: true,
        });

        let src = "import java.util.Objects;\nclass Use { void f(Object o) { Objects.requireNonNull(o); } }\n";
        let (vf_cache, _) = assert_decompiled_outcome(
            resolve_sync(
                &runtime,
                &backend_ok,
                None,
                PathBuf::from("src/Use.java"),
                src,
                "requireNonNull(o)",
            ),
            "java.util.Objects",
            "return obj;",
        );
        assert!(!vf_cache.starts_with(cache_root.join("bytecode")));

        // 2. Subsequent call hits persistent Vineflower cache without invoking java again.
        let cached_jdk = dummy_test_jdk(
            &dir.path().join("jdk"),
            dir.path().join("nonexistent-java"),
            jmods_dir.clone(),
        );
        let backend_cached = LspDefinitionBackend::with_external_config(ExternalDefinitionConfig {
            jdk_index: Arc::new(JdkOnDemandIndex::from_jmods_dir(jmods_dir.clone())),
            runtime_jdk: None,
            decompiler: Some(VineflowerDecompiler::new(
                vf_jar.clone(),
                cached_jdk,
                cache_root.clone(),
            )),
            base_dependency_config: DependencyArchiveConfig::default(),
            cache_root: cache_root.clone(),
            auto_decompile_class: true,
        });
        let (cached_vf_path, _) = assert_decompiled_outcome(
            resolve_sync(
                &runtime,
                &backend_cached,
                None,
                PathBuf::from("src/Use.java"),
                src,
                "requireNonNull(o)",
            ),
            "java.util.Objects",
            "return obj;",
        );
        assert_eq!(cached_vf_path, vf_cache);

        // 3. Cold cache + failing Vineflower -> falls back to cafebabe bytecode decompiler.
        let fail_cache_root = dir.path().join("cache/decompiled_fail");
        let fail_java = dir.path().join("mock-java-fail.sh");
        write_exec_script(&fail_java, "#!/bin/sh\nexit 1\n");
        let fail_jdk = dummy_test_jdk(&dir.path().join("jdk"), fail_java, jmods_dir.clone());
        let backend_fallback =
            LspDefinitionBackend::with_external_config(ExternalDefinitionConfig {
                jdk_index: Arc::new(JdkOnDemandIndex::from_jmods_dir(jmods_dir)),
                runtime_jdk: None,
                decompiler: Some(VineflowerDecompiler::new(
                    vf_jar,
                    fail_jdk,
                    fail_cache_root.clone(),
                )),
                base_dependency_config: DependencyArchiveConfig::default(),
                cache_root: fail_cache_root.clone(),
                auto_decompile_class: true,
            });

        let (fb_cache, fb_src) = assert_decompiled_outcome(
            resolve_sync(
                &runtime,
                &backend_fallback,
                None,
                PathBuf::from("src/Use.java"),
                src,
                "requireNonNull(o)",
            ),
            "java.util.Objects",
            "requireNonNull(",
        );
        assert!(fb_cache.starts_with(fail_cache_root.join("bytecode")));
        assert!(fb_src.contains("/* compiled code */"));
    }
}
