//! Omakase tool manifest and idempotent installer for `jdt.ls`, Lombok, Vineflower and `java-debug`.

pub mod installer;
pub mod manifest;

pub use installer::{
    InstalledJdtlsPaths, OmakaseManager, OmakaseToolkit, jdtls_config_dir_candidates,
    resolve_installed_jdtls_paths,
};
pub use manifest::{
    INSTALLED_STATE_FILE_NAME, InstalledOmakaseState, InstalledToolRecord, OmakaseManifest,
    OmakasePackageFormat, OmakaseTool, PinnedArtifactSpec,
};
