//! Offline integration tests for `kova_toolchain` JDK discovery, selection, archive safety, and Temurin provisioning.

#![allow(clippy::expect_used, reason = "integration test assertions and setup")]

use std::fs::{self, File};
use std::future::Future;
use std::io::Write;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};

use flate2::Compression;
use flate2::write::GzEncoder;
use kova_config::{EffectiveConfig, KovaPaths, KovaSettings};
use kova_toolchain::{
    ArchiveFormat, ArtifactTransport, DEFAULT_MAX_JDK_UNPACKED_BYTES, JDTLS_MIN_JAVA_MAJOR,
    JavaVersion, JdkDiscoveryRoots, JdkSource, JdkVendor, ProgressCallback, SystemJvmRoot,
    TemurinPlatform, ToolchainError, discover_jdks, extract_archive_safe, provision_temurin,
    select_project_jdk, select_tool_runtime_jdk, verify_sha256_file,
};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use zip::write::SimpleFileOptions;

fn create_fake_jdk(root: &Path, java_version: &str, implementor: &str, macos_bundle: bool) {
    let home = if macos_bundle {
        root.join("Contents").join("Home")
    } else {
        root.to_path_buf()
    };
    let bin_dir = home.join("bin");
    fs::create_dir_all(&bin_dir).expect("create bin dir");
    let java_bin = bin_dir.join(if cfg!(windows) { "java.exe" } else { "java" });
    fs::write(&java_bin, "#!/bin/sh\nexit 0\n").expect("write java bin");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&java_bin, fs::Permissions::from_mode(0o755)).expect("chmod java");
    }
    fs::create_dir_all(home.join("jmods")).expect("create jmods");
    let release = format!(
        "JAVA_VERSION=\"{java_version}\"\nIMPLEMENTOR=\"{implementor}\"\nOS_ARCH=\"aarch64\"\n"
    );
    fs::write(home.join("release"), release).expect("write release");
}

#[test]
fn discovers_jdks_across_sources_and_deduplicates_symlinks() {
    let tmp = TempDir::new().expect("tempdir");
    let omakase_dir = tmp.path().join("omakase_jdks");
    let sdkman_dir = tmp.path().join("sdkman");
    let macos_dir = tmp.path().join("macos_jvms");
    let linux_dir = tmp.path().join("linux_jvm");
    let env_home = tmp.path().join("env_jdk");

    create_fake_jdk(
        &omakase_dir.join("temurin-21"),
        "21.0.5+11",
        "Eclipse Adoptium",
        false,
    );
    create_fake_jdk(&env_home, "17.0.13", "Amazon.com Inc.", false);
    create_fake_jdk(
        &sdkman_dir.join("21.0.5-tem"),
        "21.0.5",
        "Eclipse Adoptium",
        false,
    );
    #[cfg(unix)]
    std::os::unix::fs::symlink(sdkman_dir.join("21.0.5-tem"), sdkman_dir.join("current"))
        .expect("symlink current");

    create_fake_jdk(
        &macos_dir.join("temurin-11.jdk"),
        "11.0.24",
        "Eclipse Adoptium",
        true,
    );
    create_fake_jdk(
        &linux_dir.join("java-8-openjdk"),
        "1.8.0_432",
        "OpenJDK",
        false,
    );

    let mut roots = JdkDiscoveryRoots::isolated();
    roots.omakase_jdks_dir = Some(omakase_dir);
    roots.env_java_home = Some(env_home);
    roots.sdkman_candidates_dir = Some(sdkman_dir);
    roots.system_dirs = vec![
        SystemJvmRoot {
            dir: macos_dir,
            source: JdkSource::SystemMacOs,
        },
        SystemJvmRoot {
            dir: linux_dir,
            source: JdkSource::SystemLinux,
        },
    ];

    let found = discover_jdks(&roots);
    assert_eq!(found.len(), 5);
    assert_eq!(found[0].source, JdkSource::KovaOmakase);
    assert_eq!(found[0].version, JavaVersion::new(21, 0, 5, Some(11)));
    assert_eq!(found[0].vendor, JdkVendor::Temurin);
    assert!(found.iter().any(|j| j.version.major == 8));
    assert!(found.iter().any(|j| j.source == JdkSource::SystemMacOs));
    assert!(found.iter().any(|j| j.source == JdkSource::SystemLinux));
}

#[test]
fn selects_project_and_tool_runtime_jdks() {
    let tmp = TempDir::new().expect("tempdir");
    let omakase_dir = tmp.path().join("omakase");
    let linux_dir = tmp.path().join("linux");

    create_fake_jdk(
        &omakase_dir.join("temurin-21"),
        "21.0.5",
        "Eclipse Adoptium",
        false,
    );
    create_fake_jdk(
        &linux_dir.join("java-17"),
        "17.0.13",
        "Amazon.com Inc.",
        false,
    );

    let mut roots = JdkDiscoveryRoots::isolated();
    roots.omakase_jdks_dir = Some(omakase_dir);
    roots.system_dirs = vec![SystemJvmRoot {
        dir: linux_dir,
        source: JdkSource::SystemLinux,
    }];
    let jdks = discover_jdks(&roots);

    let mut config = EffectiveConfig::merge(&KovaSettings::default(), None);
    config.settings.jvm.preferred_jdk = Some("17".to_owned());

    let selected = select_project_jdk(&jdks, &config).expect("select project jdk 17");
    assert_eq!(selected.version.major, 17);

    config.settings.jvm.preferred_jdk = Some("temurin-21".to_owned());
    let selected_pref = select_project_jdk(&jdks, &config).expect("select preferred temurin-21");
    assert_eq!(selected_pref.version.major, 21);

    let tool_jdk = select_tool_runtime_jdk(&jdks, JDTLS_MIN_JAVA_MAJOR).expect("tool runtime");
    assert_eq!(tool_jdk.version.major, 21);

    let only_17: Vec<_> = jdks.into_iter().filter(|j| j.version.major == 17).collect();
    let err = select_tool_runtime_jdk(&only_17, JDTLS_MIN_JAVA_MAJOR)
        .expect_err("17 must be rejected for jdtls");
    assert!(matches!(
        err,
        ToolchainError::UnsupportedJdkRuntime {
            found_major: 17,
            min_major: 21,
            ..
        }
    ));
}

#[test]
fn verifies_sha256_and_blocks_zip_tar_slip() {
    let tmp = TempDir::new().expect("tempdir");
    let sample = tmp.path().join("sample.bin");
    fs::write(&sample, b"kova-toolchain").expect("write sample");

    let digest = Sha256::digest(b"kova-toolchain");
    let hex_digest = digest.iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write;
        let _ = write!(acc, "{b:02x}");
        acc
    });
    verify_sha256_file(&sample, &hex_digest).expect("valid sha256");

    let mismatch = verify_sha256_file(&sample, "0000").expect_err("bad sha256");
    assert!(matches!(mismatch, ToolchainError::ChecksumMismatch { .. }));

    // Tar-Slip test
    let bad_tar = tmp.path().join("bad.tar.gz");
    {
        let file = File::create(&bad_tar).expect("create bad tar");
        let enc = GzEncoder::new(file, Compression::default());
        let mut builder = tar::Builder::new(enc);
        let mut header = tar::Header::new_gnu();
        let data = b"evil";
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        let _ = builder
            .append_data(&mut header, "../../etc/passwd", &data[..])
            .is_err();
    }
    // Even if tar::Builder rejects `..` on creation, test Zip-Slip with raw zip writer:
    let bad_zip = tmp.path().join("bad.zip");
    {
        let file = File::create(&bad_zip).expect("create bad zip");
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file("../../etc/passwd", SimpleFileOptions::default())
            .expect("start zip entry");
        zip.write_all(b"evil").expect("write zip entry");
        zip.finish().expect("finish zip");
    }
    let extract_dest = tmp.path().join("out");
    let zip_err = extract_archive_safe(
        &bad_zip,
        &extract_dest,
        ArchiveFormat::Zip,
        DEFAULT_MAX_JDK_UNPACKED_BYTES,
    )
    .expect_err("zip slip must be blocked");
    assert!(matches!(
        zip_err,
        ToolchainError::PathTraversalBlocked { .. }
    ));
}

struct MockTemurinTransport {
    archive_bytes: Vec<u8>,
    archive_sha256: String,
    calls: AtomicUsize,
}

impl ArtifactTransport for MockTemurinTransport {
    fn fetch_json<'a>(
        &'a self,
        _url: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, ToolchainError>> + Send + 'a>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let json = format!(
                r#"[{{
                    "binary": {{
                        "package": {{
                            "name": "OpenJDK21U-jdk_aarch64_mac_hotspot_21.0.5_11.tar.gz",
                            "link": "https://mock.local/temurin-21.tar.gz",
                            "checksum": "{}",
                            "size": {}
                        }}
                    }},
                    "version": {{
                        "semver": "21.0.5+11"
                    }}
                }}]"#,
                self.archive_sha256,
                self.archive_bytes.len()
            );
            Ok(json.into_bytes())
        })
    }

    fn download_to_path<'a>(
        &'a self,
        _url: &'a str,
        dest: &'a Path,
        _progress: Option<&'a ProgressCallback>,
    ) -> Pin<Box<dyn Future<Output = Result<(), ToolchainError>> + Send + 'a>> {
        Box::pin(async move {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).expect("create parent");
            }
            fs::write(dest, &self.archive_bytes).expect("write mock archive");
            Ok(())
        })
    }
}

#[tokio::test]
async fn provisions_temurin_offline_and_is_idempotent() {
    let tmp = TempDir::new().expect("tempdir");
    let paths = KovaPaths::from_roots(
        tmp.path().join("config"),
        tmp.path().join("data"),
        tmp.path().join("cache"),
    )
    .expect("kova paths");

    // Build synthetic .tar.gz containing jdk-21.0.5+11/bin/java and release
    let mut tar_gz_bytes = Vec::new();
    {
        let enc = GzEncoder::new(&mut tar_gz_bytes, Compression::default());
        let mut builder = tar::Builder::new(enc);

        let java_script = b"#!/bin/sh\nexit 0\n";
        let mut java_hdr = tar::Header::new_gnu();
        java_hdr.set_size(java_script.len() as u64);
        java_hdr.set_mode(0o755);
        java_hdr.set_cksum();
        builder
            .append_data(
                &mut java_hdr,
                "jdk-21.0.5+11/Contents/Home/bin/java",
                &java_script[..],
            )
            .expect("append java");

        let release =
            b"JAVA_VERSION=\"21.0.5\"\nIMPLEMENTOR=\"Eclipse Adoptium\"\nOS_ARCH=\"aarch64\"\n";
        let mut rel_hdr = tar::Header::new_gnu();
        rel_hdr.set_size(release.len() as u64);
        rel_hdr.set_mode(0o644);
        rel_hdr.set_cksum();
        builder
            .append_data(
                &mut rel_hdr,
                "jdk-21.0.5+11/Contents/Home/release",
                &release[..],
            )
            .expect("append release");

        builder.finish().expect("finish tar");
    }

    let digest = Sha256::digest(&tar_gz_bytes);
    let sha_hex = digest.iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write;
        let _ = write!(acc, "{b:02x}");
        acc
    });

    let transport = MockTemurinTransport {
        archive_bytes: tar_gz_bytes,
        archive_sha256: sha_hex,
        calls: AtomicUsize::new(0),
    };

    let installed = provision_temurin(21, TemurinPlatform::MacAarch64, &paths, &transport, None)
        .await
        .expect("provision temurin 21");
    assert_eq!(installed.version, JavaVersion::new(21, 0, 5, None));
    assert_eq!(installed.vendor, JdkVendor::Temurin);
    assert_eq!(transport.calls.load(Ordering::Relaxed), 1);

    // Second call must be idempotent and not hit transport
    let cached = provision_temurin(21, TemurinPlatform::MacAarch64, &paths, &transport, None)
        .await
        .expect("cached temurin 21");
    assert_eq!(cached.home_dir, installed.home_dir);
    assert_eq!(transport.calls.load(Ordering::Relaxed), 1);
}
