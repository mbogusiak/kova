//! `kova_toolchain` — Omakase toolchain manager: JDK detection, `jdt.ls`, Lombok, Vineflower and `java-debug`.
//!
//! This crate belongs to the **Headless Core**: it must compile and pass its
//! tests without any windowing or GPU dependency (enforced by KOVA-003).

pub mod download;
pub mod error;
pub mod jdk;
pub mod jdtls;
pub mod omakase;
pub mod vineflower;

pub use download::{
    ArchiveFormat, ArtifactTransport, DEFAULT_MAX_JDK_UNPACKED_BYTES,
    DEFAULT_MAX_TOOL_UNPACKED_BYTES, ProgressCallback, ProgressEvent, ReqwestTransport,
    extract_archive_safe, verify_sha256_file,
};
pub use error::ToolchainError;
pub use jdk::{
    JDTLS_MIN_JAVA_MAJOR, JavaVersion, JdkDiscoveryRoots, JdkInstallation, JdkRequirement,
    JdkSource, JdkVendor, ReleaseMetadata, SystemJvmRoot, TemurinAsset, TemurinClient,
    TemurinPlatform, discover_jdks, inspect_jdk_dir, parse_jdk_requirement, parse_release_file,
    provision_temurin, select_project_jdk, select_tool_runtime_jdk,
};
pub use jdtls::{JdtlsLaunchOptions, JdtlsLaunchSpec, build_jdtls_launch_spec};
pub use omakase::{
    InstalledJdtlsPaths, InstalledOmakaseState, InstalledToolRecord, OmakaseManager,
    OmakaseManifest, OmakasePackageFormat, OmakaseTool, OmakaseToolkit, PinnedArtifactSpec,
};
pub use vineflower::{DecompileResult, DecompileTarget, VineflowerDecompiler};

/// Name of this crate as declared in its `Cargo.toml`.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    use super::CRATE_NAME;

    #[test]
    fn crate_name_matches_manifest() {
        assert_eq!(CRATE_NAME, "kova_toolchain");
    }

    #[test]
    fn inherits_workspace_license() {
        assert_eq!(env!("CARGO_PKG_LICENSE"), "MIT OR Apache-2.0");
    }
}
