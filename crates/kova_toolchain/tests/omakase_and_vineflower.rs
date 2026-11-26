//! Offline integration tests for the Omakase installer, the `jdt.ls` launch builder and
//! the Vineflower decompiler (KOVA-035).

#![allow(clippy::expect_used, reason = "integration test assertions and setup")]

use std::collections::BTreeMap;
use std::fs;
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use flate2::Compression;
use flate2::write::GzEncoder;
use kova_config::{EffectiveConfig, KovaPaths, KovaSettings};
use kova_toolchain::omakase::jdtls_config_dir_candidates;
use kova_toolchain::{
    ArtifactTransport, DecompileTarget, JavaVersion, JdkInstallation, JdkSource, JdkVendor,
    JdtlsLaunchOptions, OmakaseManager, OmakaseManifest, OmakasePackageFormat, OmakaseTool,
    OmakaseToolkit, PinnedArtifactSpec, ProgressCallback, ProgressEvent, ToolchainError,
    VineflowerDecompiler, build_jdtls_launch_spec,
};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use zip::write::SimpleFileOptions;

const LAUNCHER: &str = "org.eclipse.equinox.launcher_1.6.900.v20240613-2009.jar";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct MockTransport {
    artifacts: BTreeMap<String, Vec<u8>>,
    downloads: AtomicUsize,
}

impl MockTransport {
    fn new(artifacts: BTreeMap<String, Vec<u8>>) -> Self {
        Self {
            artifacts,
            downloads: AtomicUsize::new(0),
        }
    }

    fn downloads(&self) -> usize {
        self.downloads.load(Ordering::Relaxed)
    }
}

impl ArtifactTransport for MockTransport {
    fn fetch_json<'a>(
        &'a self,
        url: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, ToolchainError>> + Send + 'a>> {
        Box::pin(async move {
            Err(ToolchainError::DownloadFailed {
                url: url.to_owned(),
                reason: "no JSON in mock".to_owned(),
            })
        })
    }

    fn download_to_path<'a>(
        &'a self,
        url: &'a str,
        dest: &'a Path,
        progress: Option<&'a ProgressCallback>,
    ) -> Pin<Box<dyn Future<Output = Result<(), ToolchainError>> + Send + 'a>> {
        Box::pin(async move {
            self.downloads.fetch_add(1, Ordering::Relaxed);
            let bytes = self
                .artifacts
                .get(url)
                .ok_or_else(|| ToolchainError::DownloadFailed {
                    url: url.to_owned(),
                    reason: "unknown url".to_owned(),
                })?;
            if let Some(cb) = progress {
                cb(ProgressEvent::Started {
                    total_bytes: Some(bytes.len() as u64),
                });
            }
            fs::write(dest, bytes)
                .map_err(|err| ToolchainError::io(dest.display().to_string(), &err))
        })
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

fn append_file<W: Write>(builder: &mut tar::Builder<W>, path: &str, data: &[u8]) {
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder
        .append_data(&mut header, path, data)
        .expect("append tar entry");
}

fn jdtls_tarball(launchers: &[&str]) -> Vec<u8> {
    let encoder = GzEncoder::new(Vec::new(), Compression::fast());
    let mut builder = tar::Builder::new(encoder);
    for launcher in launchers {
        append_file(&mut builder, &format!("plugins/{launcher}"), b"launcher");
    }
    append_file(
        &mut builder,
        "plugins/org.eclipse.jdt.ls.core_1.40.0.jar",
        b"core",
    );
    for dir in [
        "config_mac",
        "config_mac_arm",
        "config_linux",
        "config_linux_arm",
        "config_win",
    ] {
        append_file(
            &mut builder,
            &format!("{dir}/config.ini"),
            format!("osgi.platform={dir}\n").as_bytes(),
        );
    }
    builder
        .into_inner()
        .expect("finish tar")
        .finish()
        .expect("finish gzip")
}

fn spec(
    tool: OmakaseTool,
    version: &str,
    bytes: &[u8],
    format: OmakasePackageFormat,
) -> PinnedArtifactSpec {
    PinnedArtifactSpec {
        tool,
        version: version.to_owned(),
        url: format!("https://mock.local/{}-{version}", tool.as_str()),
        sha256: sha256_hex(bytes),
        format,
        min_java_major: None,
    }
}

struct Fixture {
    tmp: TempDir,
    paths: KovaPaths,
    manifest: OmakaseManifest,
    transport: MockTransport,
}

fn fixture() -> Fixture {
    let tmp = TempDir::new().expect("tempdir");
    let paths = KovaPaths::from_roots(
        tmp.path().join("config"),
        tmp.path().join("data"),
        tmp.path().join("cache"),
    )
    .expect("paths");
    let jdtls = jdtls_tarball(&[LAUNCHER]);
    let artifacts = [
        (OmakaseTool::JdtLs, jdtls, OmakasePackageFormat::TarGz),
        (
            OmakaseTool::Lombok,
            b"lombok-jar".to_vec(),
            OmakasePackageFormat::RawJar,
        ),
        (
            OmakaseTool::Vineflower,
            b"vineflower-jar".to_vec(),
            OmakasePackageFormat::RawJar,
        ),
        (
            OmakaseTool::JavaDebug,
            b"java-debug-jar".to_vec(),
            OmakasePackageFormat::RawJar,
        ),
    ];
    let mut manifest = OmakaseManifest::default_pinned();
    let mut urls = BTreeMap::new();
    for (tool, bytes, format) in artifacts {
        let pinned = spec(tool, "1.0.0", &bytes, format);
        urls.insert(pinned.url.clone(), bytes);
        manifest = manifest.with_override(tool, pinned);
    }
    Fixture {
        tmp,
        paths,
        manifest,
        transport: MockTransport::new(urls),
    }
}

fn fake_jdk(home: &Path, major: u16, java_script: &str) -> JdkInstallation {
    let bin = home.join("bin");
    fs::create_dir_all(&bin).expect("create bin");
    let java_bin = bin.join("java");
    fs::write(&java_bin, java_script).expect("write java");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&java_bin, fs::Permissions::from_mode(0o755)).expect("chmod java");
    }
    JdkInstallation {
        home_dir: home.to_path_buf(),
        java_bin,
        jmods_dir: None,
        version: JavaVersion::new(major, 0, 5, Some(11)),
        vendor: JdkVendor::Temurin,
        arch: "aarch64".to_owned(),
        source: JdkSource::Explicit,
    }
}

fn dir_listing(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).expect("read dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                stack.push(path.clone());
            }
            out.push(path);
        }
    }
    out.sort();
    out
}

async fn install_toolkit(fx: &Fixture) -> OmakaseToolkit {
    let manager = OmakaseManager::new(fx.paths.clone(), fx.manifest.clone());
    manager
        .ensure_jvm_lsp_toolkit(true, &fx.transport, None)
        .await
        .expect("install toolkit")
}

// ---------------------------------------------------------------------------
// AC-1: idempotent Omakase installation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn installs_toolkit_and_second_call_is_offline() {
    let fx = fixture();
    let manager = OmakaseManager::new(fx.paths.clone(), fx.manifest.clone());
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    let callback = move |event: ProgressEvent| sink.lock().expect("lock").push(event);

    let toolkit = manager
        .ensure_jvm_lsp_toolkit(true, &fx.transport, Some(&callback))
        .await
        .expect("first install");
    assert_eq!(fx.transport.downloads(), 4);

    let omakase = fx.paths.omakase_dir();
    assert_eq!(toolkit.jdtls.root_dir, fx.paths.jdtls_dir());
    assert_eq!(
        toolkit.jdtls.launcher_jar,
        fx.paths.jdtls_dir().join("plugins").join(LAUNCHER)
    );
    assert!(toolkit.jdtls.base_config_dir.join("config.ini").is_file());
    let config_name = toolkit
        .jdtls
        .base_config_dir
        .file_name()
        .and_then(|n| n.to_str())
        .expect("config dir name");
    assert_eq!(config_name, jdtls_config_dir_candidates()[0]);
    assert_eq!(
        toolkit.jdtls.lombok_jar,
        Some(fx.paths.jdtls_dir().join("lombok.jar"))
    );
    assert_eq!(
        fs::read(&toolkit.vineflower_jar).expect("vf"),
        b"vineflower-jar"
    );
    assert!(
        toolkit
            .vineflower_jar
            .starts_with(fx.paths.vineflower_dir())
    );
    let java_debug = toolkit.java_debug_jar.clone().expect("java-debug");
    assert!(java_debug.starts_with(fx.paths.java_debug_dir()));

    let state = manager.inspect_installed().expect("state");
    assert!(omakase.join("installed.toml").is_file());
    for tool in OmakaseTool::ALL {
        let record = state.get(tool).expect("record");
        assert_eq!(record.version, "1.0.0");
        assert_eq!(record.sha256, fx.manifest.get(tool).expect("spec").sha256);
    }
    assert_eq!(
        state
            .get(OmakaseTool::JdtLs)
            .expect("jdtls")
            .entrypoint_rel_path,
        format!("jdtls/plugins/{LAUNCHER}")
    );
    {
        let events = events.lock().expect("lock");
        assert!(events.contains(&ProgressEvent::VerifyingChecksum));
        assert!(events.contains(&ProgressEvent::Extracting));
        assert!(events.contains(&ProgressEvent::Completed));
    }
    // No staging leftovers.
    let leftovers: Vec<_> = fs::read_dir(&omakase)
        .expect("read omakase")
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
        .collect();
    assert!(leftovers.is_empty(), "staging leftovers: {leftovers:?}");

    let again = manager
        .ensure_jvm_lsp_toolkit(true, &fx.transport, None)
        .await
        .expect("second install");
    assert_eq!(fx.transport.downloads(), 4, "second call must not download");
    assert_eq!(again, toolkit);

    // A fresh manager over the same directories is offline as well.
    let fresh = OmakaseManager::new(fx.paths.clone(), fx.manifest.clone());
    fresh
        .ensure_jvm_lsp_toolkit(true, &fx.transport, None)
        .await
        .expect("third install");
    assert_eq!(fx.transport.downloads(), 4);
}

#[tokio::test]
async fn deleted_entrypoint_and_version_bump_trigger_reinstall_preserving_lombok() {
    let mut fx = fixture();
    let toolkit = install_toolkit(&fx).await;
    assert_eq!(fx.transport.downloads(), 4);

    fs::remove_file(&toolkit.vineflower_jar).expect("delete vineflower");
    let manager = OmakaseManager::new(fx.paths.clone(), fx.manifest.clone());
    manager
        .ensure_tool(OmakaseTool::Vineflower, &fx.transport, None)
        .await
        .expect("reinstall vineflower");
    assert_eq!(fx.transport.downloads(), 5);

    let tarball = jdtls_tarball(&[LAUNCHER]);
    let bumped = spec(
        OmakaseTool::JdtLs,
        "2.0.0",
        &tarball,
        OmakasePackageFormat::TarGz,
    );
    let mut artifacts = fx.transport.artifacts.clone();
    artifacts.insert(bumped.url.clone(), tarball);
    fx.transport = MockTransport::new(artifacts);
    fx.manifest = fx
        .manifest
        .clone()
        .with_override(OmakaseTool::JdtLs, bumped);

    let upgraded = install_toolkit(&fx).await;
    assert_eq!(fx.transport.downloads(), 1, "only jdt.ls is re-downloaded");
    assert_eq!(upgraded.jdtls.version, "2.0.0");
    assert_eq!(upgraded.jdtls.lombok_jar, toolkit.jdtls.lombok_jar);
}

#[tokio::test]
async fn checksum_mismatch_installs_nothing() {
    let mut fx = fixture();
    let mut bad = fx
        .manifest
        .get(OmakaseTool::Vineflower)
        .expect("spec")
        .clone();
    bad.sha256 = "0".repeat(64);
    fx.manifest = fx
        .manifest
        .clone()
        .with_override(OmakaseTool::Vineflower, bad);
    let manager = OmakaseManager::new(fx.paths.clone(), fx.manifest.clone());

    let err = manager
        .ensure_tool(OmakaseTool::Vineflower, &fx.transport, None)
        .await
        .expect_err("checksum mismatch");
    assert!(
        matches!(err, ToolchainError::ChecksumMismatch { .. }),
        "{err:?}"
    );
    assert!(manager.inspect_installed().expect("state").tools.is_empty());
    assert!(!fx.paths.vineflower_dir().join("vineflower.jar").exists());
}

#[tokio::test]
async fn ambiguous_launcher_is_rejected() {
    let mut fx = fixture();
    let tarball = jdtls_tarball(&[LAUNCHER, "org.eclipse.equinox.launcher_1.7.0.jar"]);
    let pinned = spec(
        OmakaseTool::JdtLs,
        "9.9.9",
        &tarball,
        OmakasePackageFormat::TarGz,
    );
    let mut artifacts = fx.transport.artifacts.clone();
    artifacts.insert(pinned.url.clone(), tarball);
    fx.transport = MockTransport::new(artifacts);
    fx.manifest = fx
        .manifest
        .clone()
        .with_override(OmakaseTool::JdtLs, pinned);
    let manager = OmakaseManager::new(fx.paths.clone(), fx.manifest.clone());

    let err = manager
        .ensure_tool(OmakaseTool::JdtLs, &fx.transport, None)
        .await
        .expect_err("ambiguous");
    assert!(
        matches!(err, ToolchainError::InvalidToolInstallation { .. }),
        "{err:?}"
    );
    assert!(
        manager
            .inspect_installed()
            .expect("state")
            .get(OmakaseTool::JdtLs)
            .is_none()
    );
}

// ---------------------------------------------------------------------------
// AC-2: jdt.ls launch spec
// ---------------------------------------------------------------------------

fn launch_options(
    project_root: &Path,
    runtime: JdkInstallation,
    lombok: bool,
) -> JdtlsLaunchOptions {
    let mut settings = KovaSettings::default();
    settings.jvm.lombok_enabled = lombok;
    settings.jvm.jdtls_max_ram_mb = 1536;
    settings.jvm.gradle_offline = true;
    settings.editor.show_inlay_hints = false;
    JdtlsLaunchOptions {
        project_root: project_root.to_path_buf(),
        project_jdk: Some(runtime.clone()),
        runtime_jdk: runtime,
        effective_config: EffectiveConfig::merge(&settings, None),
        extra_jvm_args: vec!["-Dkova.extra=1".to_owned()],
    }
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "single end-to-end launch spec scenario"
)]
async fn jdtls_launch_spec_isolates_workspaces_and_wires_config() {
    let fx = fixture();
    let toolkit = install_toolkit(&fx).await;
    let jdk21 = fake_jdk(&fx.tmp.path().join("jdk21"), 21, "#!/bin/sh\nexit 0\n");
    let project_a = fx.tmp.path().join("project-a");
    let project_b = fx.tmp.path().join("project-b");
    fs::create_dir_all(&project_a).expect("project a");
    fs::create_dir_all(&project_b).expect("project b");

    let spec_a = build_jdtls_launch_spec(
        &fx.paths,
        &toolkit,
        &launch_options(&project_a, jdk21.clone(), true),
    )
    .expect("spec a");
    let spec_b = build_jdtls_launch_spec(
        &fx.paths,
        &toolkit,
        &launch_options(&project_b, jdk21.clone(), false),
    )
    .expect("spec b");

    let ws_a = fx.paths.workspace_cache_dir(&project_a).expect("ws a");
    assert_eq!(spec_a.workspace_data_dir, ws_a.join("jdtls-data"));
    assert_eq!(spec_a.osgi_config_dir, ws_a.join("jdtls-config"));
    assert_ne!(spec_a.workspace_data_dir, spec_b.workspace_data_dir);
    assert_ne!(spec_a.osgi_config_dir, spec_b.osgi_config_dir);
    assert!(spec_a.workspace_data_dir.is_dir());
    assert!(spec_a.osgi_config_dir.join("config.ini").is_file());
    assert!(spec_a.osgi_config_dir.join(".kova-jdtls-version").is_file());
    assert_eq!(spec_a.java_bin, jdk21.java_bin);
    assert_eq!(spec_a.working_dir, project_a);
    assert_eq!(
        spec_a.env.get("JAVA_HOME").map(String::as_str),
        Some(jdk21.home_dir.display().to_string().as_str())
    );

    let args = &spec_a.jvm_and_app_args;
    let pos = |needle: &str| args.iter().position(|a| a == needle).expect(needle);
    let lombok_arg = format!(
        "-javaagent:{}",
        toolkit.jdtls.lombok_jar.clone().expect("lombok").display()
    );
    assert!(args.contains(&"-Xmx1536m".to_owned()));
    assert!(args.contains(&"-Declipse.application=org.eclipse.jdt.ls.core.id1".to_owned()));
    let jar = pos("-jar");
    assert!(pos(&lombok_arg) < jar);
    assert!(pos("-Dkova.extra=1") < jar);
    assert!(pos("--add-modules=ALL-SYSTEM") < jar);
    assert_eq!(
        args[jar + 1],
        toolkit.jdtls.launcher_jar.display().to_string()
    );
    assert_eq!(args[jar + 2], "-configuration");
    assert_eq!(args[jar + 3], spec_a.osgi_config_dir.display().to_string());
    assert_eq!(args[jar + 4], "-data");
    assert_eq!(
        args[jar + 5],
        spec_a.workspace_data_dir.display().to_string()
    );
    assert!(
        !spec_b
            .jvm_and_app_args
            .iter()
            .any(|a| a.starts_with("-javaagent:"))
    );
    assert!(spec_a.command_line_preview().contains("-jar"));

    let init = &spec_a.initialization_options;
    let java_debug = toolkit.java_debug_jar.clone().expect("java-debug");
    assert_eq!(init["bundles"][0], java_debug.display().to_string());
    assert_eq!(
        init["settings"]["java"]["import"]["gradle"]["offline"]["enabled"],
        true
    );
    assert_eq!(
        init["settings"]["java"]["inlayHints"]["parameterNames"]["enabled"],
        "none"
    );
    let runtime = &init["settings"]["java"]["configuration"]["runtimes"][0];
    assert_eq!(runtime["name"], "JavaSE-21");
    assert_eq!(runtime["default"], true);

    // Omarchy purity: nothing was written into the project roots.
    assert_eq!(dir_listing(&project_a), Vec::<PathBuf>::new());
    assert_eq!(dir_listing(&project_b), Vec::<PathBuf>::new());

    // Rebuilding keeps the seeded configuration; a version change refreshes it.
    fs::write(spec_a.osgi_config_dir.join("org.eclipse.osgi.state"), b"x").expect("osgi state");
    build_jdtls_launch_spec(
        &fx.paths,
        &toolkit,
        &launch_options(&project_a, jdk21.clone(), true),
    )
    .expect("rebuild");
    assert!(
        spec_a
            .osgi_config_dir
            .join("org.eclipse.osgi.state")
            .exists()
    );
    let mut upgraded = toolkit.clone();
    upgraded.jdtls.version = "2.0.0".to_owned();
    build_jdtls_launch_spec(
        &fx.paths,
        &upgraded,
        &launch_options(&project_a, jdk21, true),
    )
    .expect("refresh");
    assert!(
        !spec_a
            .osgi_config_dir
            .join("org.eclipse.osgi.state")
            .exists()
    );
    assert!(spec_a.osgi_config_dir.join("config.ini").is_file());
}

#[tokio::test]
async fn jdtls_launch_spec_rejects_old_runtime() {
    let fx = fixture();
    let toolkit = install_toolkit(&fx).await;
    let jdk17 = fake_jdk(&fx.tmp.path().join("jdk17"), 17, "#!/bin/sh\nexit 0\n");
    let project = fx.tmp.path().join("project");
    fs::create_dir_all(&project).expect("project");

    let err = build_jdtls_launch_spec(&fx.paths, &toolkit, &launch_options(&project, jdk17, true))
        .expect_err("JDK 17 rejected");
    assert!(
        matches!(
            err,
            ToolchainError::UnsupportedJdkRuntime {
                found_major: 17,
                min_major: 21,
                ..
            }
        ),
        "{err:?}"
    );
}

// ---------------------------------------------------------------------------
// AC-3 / AC-4: Vineflower decompilation, cache and path safety
// ---------------------------------------------------------------------------

/// Mock `java`: treats the last two args as input/output dirs and writes one `.java` per
/// top-level class, recording the number of companion `Outer$*.class` files.
#[cfg(unix)]
const MOCK_VINEFLOWER: &str = r#"#!/bin/sh
echo run >> "$(dirname "$0")/invocations"
in_dir=""; out_dir=""
for a in "$@"; do in_dir="$out_dir"; out_dir="$a"; done
cd "$in_dir" || exit 3
find . -name '*.class' ! -name '*$*' | while read -r f; do
  rel="${f#./}"; dir=$(dirname "$rel"); base=$(basename "$rel" .class)
  inner=$(find "$dir" -name "${base}\$*.class" | wc -l | tr -d ' ')
  mkdir -p "$out_dir/$dir"
  {
    if [ "$dir" != "." ]; then echo "package $(echo "$dir" | tr / .);"; fi
    echo "// inner=$inner"
    echo "public class $base {}"
  } > "$out_dir/$dir/$base.java"
done
"#;

#[cfg(unix)]
fn invocations(jdk: &JdkInstallation) -> usize {
    fs::read_to_string(jdk.home_dir.join("bin").join("invocations"))
        .map_or(0, |s| s.lines().count())
}

fn write_zip(path: &Path, prefix: &[u8], entries: &[(&str, &[u8])]) {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, data) in entries {
        zip.start_file(*name, SimpleFileOptions::default())
            .expect("start file");
        zip.write_all(data).expect("write entry");
    }
    let bytes = zip.finish().expect("finish zip").into_inner();
    let mut out = prefix.to_vec();
    out.extend_from_slice(&bytes);
    fs::write(path, out).expect("write archive");
}

#[cfg(unix)]
#[tokio::test]
async fn decompiles_jar_entry_with_inner_classes_then_serves_cache() {
    let tmp = TempDir::new().expect("tempdir");
    let project = tmp.path().join("project");
    fs::create_dir_all(project.join("libs")).expect("project");
    let jar = project.join("libs").join("orders.jar");
    write_zip(
        &jar,
        &[],
        &[
            ("META-INF/MANIFEST.MF", b"Manifest-Version: 1.0\n"),
            ("com/example/Order.class", b"cafebabe"),
            ("com/example/Order$Item.class", b"cafebabe"),
            ("com/example/Order$1.class", b"cafebabe"),
            ("com/example/OrderService.class", b"cafebabe"),
        ],
    );
    let before = dir_listing(&project);
    let jdk = fake_jdk(&tmp.path().join("jdk"), 21, MOCK_VINEFLOWER);
    let vineflower_jar = tmp.path().join("vineflower.jar");
    fs::write(&vineflower_jar, b"vf").expect("vf jar");
    let cache_root = tmp.path().join("cache").join("decompiled");
    let decompiler = VineflowerDecompiler::new(vineflower_jar, jdk.clone(), cache_root.clone());

    let first = decompiler
        .decompile(&DecompileTarget::ArchiveEntry {
            archive_path: jar.clone(),
            entry_name: "com/example/Order$Item.class".to_owned(),
        })
        .await
        .expect("decompile");
    assert!(!first.from_cache);
    assert_eq!(first.fqcn, "com.example.Order");
    assert!(
        first.source_code.contains("// inner=2"),
        "{}",
        first.source_code
    );
    assert!(first.cache_file.starts_with(&cache_root));
    assert!(first.cache_file.ends_with("com/example/Order.java"));
    assert_eq!(
        fs::read_to_string(&first.cache_file).expect("cache"),
        first.source_code
    );
    assert_eq!(invocations(&jdk), 1);

    let second = decompiler
        .decompile(&DecompileTarget::ArchiveEntry {
            archive_path: jar.clone(),
            entry_name: "com/example/Order.class".to_owned(),
        })
        .await
        .expect("cached");
    assert!(second.from_cache);
    assert_eq!(second.source_code, first.source_code);
    assert_eq!(second.cache_file, first.cache_file);
    assert_eq!(invocations(&jdk), 1, "cache hit must not start the JVM");

    // No work directories left behind and nothing written into the project.
    let work_dirs: Vec<_> = fs::read_dir(&cache_root)
        .expect("cache root")
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with(".work"))
        .collect();
    assert!(work_dirs.is_empty());
    assert_eq!(dir_listing(&project), before);
}

#[cfg(unix)]
#[tokio::test]
async fn decompiles_jmod_entry_and_class_file() {
    let tmp = TempDir::new().expect("tempdir");
    let jmod = tmp.path().join("java.base.jmod");
    write_zip(
        &jmod,
        b"JM\x01\x00",
        &[
            ("classes/java/util/AbstractMap.class", b"cafebabe"),
            (
                "classes/java/util/AbstractMap$SimpleEntry.class",
                b"cafebabe",
            ),
            ("classes/module-info.class", b"cafebabe"),
        ],
    );
    let jdk = fake_jdk(&tmp.path().join("jdk"), 21, MOCK_VINEFLOWER);
    let vineflower_jar = tmp.path().join("vineflower.jar");
    fs::write(&vineflower_jar, b"vf").expect("vf jar");
    let paths = KovaPaths::from_roots(
        tmp.path().join("config"),
        tmp.path().join("data"),
        tmp.path().join("cache"),
    )
    .expect("paths");
    let decompiler = VineflowerDecompiler::for_paths(&paths, vineflower_jar, jdk);
    assert_eq!(
        decompiler.cache_root(),
        paths.cache_dir().join("decompiled")
    );

    let result = decompiler
        .decompile(&DecompileTarget::ArchiveEntry {
            archive_path: jmod,
            entry_name: "java/util/AbstractMap$SimpleEntry.class".to_owned(),
        })
        .await
        .expect("jmod decompile");
    assert_eq!(result.fqcn, "java.util.AbstractMap");
    assert!(result.source_code.contains("// inner=1"));
    assert!(result.cache_file.ends_with("java/util/AbstractMap.java"));

    let class_dir = tmp.path().join("classes");
    fs::create_dir_all(&class_dir).expect("class dir");
    fs::write(class_dir.join("Foo.class"), b"cafebabe").expect("class");
    fs::write(class_dir.join("Foo$Bar.class"), b"cafebabe").expect("inner");
    let single = decompiler
        .decompile(&DecompileTarget::ClassFile {
            class_path: class_dir.join("Foo.class"),
        })
        .await
        .expect("class file");
    assert_eq!(single.fqcn, "Foo");
    assert!(single.source_code.contains("// inner=1"));
    assert!(!single.from_cache);
}

#[tokio::test]
async fn malicious_entry_names_are_blocked() {
    let tmp = TempDir::new().expect("tempdir");
    let jar = tmp.path().join("evil.jar");
    write_zip(&jar, &[], &[("evil.class", b"cafebabe")]);
    let jdk = fake_jdk(&tmp.path().join("jdk"), 21, "#!/bin/sh\nexit 1\n");
    let vineflower_jar = tmp.path().join("vineflower.jar");
    fs::write(&vineflower_jar, b"vf").expect("vf jar");
    let cache_root = tmp.path().join("cache");
    let decompiler = VineflowerDecompiler::new(vineflower_jar, jdk, cache_root.clone());

    for entry in ["../../evil.class", "/etc/evil.class", "a/../../evil.class"] {
        let err = decompiler
            .decompile(&DecompileTarget::ArchiveEntry {
                archive_path: jar.clone(),
                entry_name: entry.to_owned(),
            })
            .await
            .expect_err("blocked");
        assert!(
            matches!(err, ToolchainError::PathTraversalBlocked { .. }),
            "{entry}: {err:?}"
        );
    }
    assert!(!tmp.path().join("evil.java").exists());
    assert!(!cache_root.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn hung_decompiler_times_out_without_caching() {
    let tmp = TempDir::new().expect("tempdir");
    let jar = tmp.path().join("lib.jar");
    write_zip(&jar, &[], &[("p/Slow.class", b"cafebabe")]);
    let jdk = fake_jdk(&tmp.path().join("jdk"), 21, "#!/bin/sh\nexec sleep 30\n");
    let vineflower_jar = tmp.path().join("vineflower.jar");
    fs::write(&vineflower_jar, b"vf").expect("vf jar");
    let cache_root = tmp.path().join("cache");
    let decompiler = VineflowerDecompiler::new(vineflower_jar, jdk, cache_root.clone())
        .with_timeout(std::time::Duration::from_millis(300));

    let started = std::time::Instant::now();
    let err = decompiler
        .decompile(&DecompileTarget::ArchiveEntry {
            archive_path: jar,
            entry_name: "p/Slow.class".to_owned(),
        })
        .await
        .expect_err("timeout");
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    match err {
        ToolchainError::ProcessFailed { status, .. } => {
            assert!(status.contains("timed out"), "{status}");
        }
        other => panic!("unexpected error: {other:?}"),
    }
    assert!(
        dir_listing(&cache_root).is_empty(),
        "nothing cached after timeout"
    );
}

/// Mock `java` mimicking Vineflower on a lone inner class: one `<Name>.java` per class file.
#[cfg(unix)]
const MOCK_EMIT_EVERY_CLASS: &str = r#"#!/bin/sh
in_dir=""; out_dir=""
for a in "$@"; do in_dir="$out_dir"; out_dir="$a"; done
cd "$in_dir" || exit 3
for f in *.class; do
  base=$(basename "$f" .class)
  echo "public class $base {}" > "$out_dir/$base.java"
done
"#;

#[cfg(unix)]
#[tokio::test]
async fn lone_inner_class_reads_inner_named_output() {
    let tmp = TempDir::new().expect("tempdir");
    let class_dir = tmp.path().join("classes");
    fs::create_dir_all(&class_dir).expect("class dir");
    let lone = class_dir.join("Outer$Inner.class");
    fs::write(&lone, b"cafebabe").expect("class");
    let jdk = fake_jdk(&tmp.path().join("jdk"), 21, MOCK_EMIT_EVERY_CLASS);
    let vineflower_jar = tmp.path().join("vineflower.jar");
    fs::write(&vineflower_jar, b"vf").expect("vf jar");
    let decompiler = VineflowerDecompiler::new(vineflower_jar, jdk, tmp.path().join("decompiled"));

    let result = decompiler
        .decompile(&DecompileTarget::ClassFile { class_path: lone })
        .await
        .expect("lone inner class");
    assert!(
        result.source_code.contains("class Outer$Inner"),
        "{}",
        result.source_code
    );
    assert_eq!(result.fqcn, "Outer");
    assert!(result.cache_file.ends_with("Outer.java"));
}

/// Real end-to-end run. Requires `KOVA_TEST_JAVA_HOME` (JDK >= 17) and
/// `KOVA_TEST_VINEFLOWER_JAR`; skipped silently when they are not set.
#[tokio::test]
#[ignore = "requires a real JDK and vineflower.jar"]
async fn real_vineflower_decompiles_jdk_class() {
    let (Some(java_home), Some(vf_jar)) = (
        std::env::var_os("KOVA_TEST_JAVA_HOME"),
        std::env::var_os("KOVA_TEST_VINEFLOWER_JAR"),
    ) else {
        return;
    };
    let jdk = kova_toolchain::inspect_jdk_dir(Path::new(&java_home), JdkSource::Explicit)
        .expect("valid JDK");
    let jmod = jdk.jmods_dir.clone().expect("jmods").join("java.base.jmod");
    let tmp = TempDir::new().expect("tempdir");
    let decompiler =
        VineflowerDecompiler::new(PathBuf::from(vf_jar), jdk, tmp.path().join("decompiled"));
    let result = decompiler
        .decompile(&DecompileTarget::ArchiveEntry {
            archive_path: jmod,
            entry_name: "java/util/AbstractMap$SimpleEntry.class".to_owned(),
        })
        .await
        .expect("real decompile");
    assert_eq!(result.fqcn, "java.util.AbstractMap");
    assert!(result.source_code.contains("class AbstractMap"));
    assert!(result.source_code.contains("SimpleEntry"));
}
