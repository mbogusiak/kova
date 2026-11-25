//! JDK discovery, version parsing, selection, and Eclipse Temurin provisioning.

pub mod discovery;
pub mod temurin;
pub mod version;

pub use discovery::{
    JDTLS_MIN_JAVA_MAJOR, JdkDiscoveryRoots, JdkInstallation, SystemJvmRoot, discover_jdks,
    inspect_jdk_dir, select_project_jdk, select_tool_runtime_jdk,
};
pub use temurin::{TemurinAsset, TemurinClient, TemurinPlatform, provision_temurin};
pub use version::{
    JavaVersion, JdkRequirement, JdkSource, JdkVendor, ReleaseMetadata, parse_jdk_requirement,
    parse_release_file,
};
