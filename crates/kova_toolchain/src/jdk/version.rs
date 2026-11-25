//! Java version model, vendor/source classification, requirement parsing, and `release` file parser.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::ToolchainError;

/// Parsed semantic Java version supporting both legacy (`1.8.0_432`) and JEP 223/322 (`21.0.5+11`) formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct JavaVersion {
    /// Major feature version (e.g., `8` for `1.8.0_432`, `21` for `21.0.5+11`).
    pub major: u16,
    /// Minor/interim version component.
    pub minor: u16,
    /// Patch/update version component.
    pub patch: u16,
    /// Optional build number (e.g., `11` in `21.0.5+11`).
    pub build: Option<u32>,
}

impl JavaVersion {
    /// Creates a new [`JavaVersion`].
    #[must_use]
    pub const fn new(major: u16, minor: u16, patch: u16, build: Option<u32>) -> Self {
        Self {
            major,
            minor,
            patch,
            build,
        }
    }

    /// Parses a Java version string such as `"21"`, `"21.0.5"`, `"21.0.5+11-LTS"`, or `"1.8.0_432"`.
    ///
    /// # Errors
    /// Returns [`ToolchainError::InvalidJavaVersion`] if the string is empty or contains non-numeric components.
    pub fn parse(raw: &str) -> Result<Self, ToolchainError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(ToolchainError::InvalidJavaVersion {
                raw: raw.to_owned(),
                reason: "version string is empty".to_owned(),
            });
        }

        // Split off build metadata (`+11` or `+11-LTS`) first.
        let (version_part, build) = match trimmed.split_once('+') {
            Some((ver, build_rest)) => {
                let digits: String = build_rest
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect();
                let parsed_build = if digits.is_empty() {
                    None
                } else {
                    Some(digits.parse::<u32>().map_err(|err| {
                        ToolchainError::InvalidJavaVersion {
                            raw: raw.to_owned(),
                            reason: format!("invalid build number: {err}"),
                        }
                    })?)
                };
                (ver, parsed_build)
            }
            None => (trimmed, None),
        };

        // Strip trailing pre-release/vendor suffix like `-ea` or `-LTS`.
        let core = version_part
            .split_once('-')
            .map_or(version_part, |(head, _)| head);

        // Handle legacy `1.8.0_432` or `1.8.0` format.
        if let Some(rest) = core.strip_prefix("1.") {
            let normalized = rest.replace('_', ".");
            let mut parts = normalized.split('.');
            let major = parse_u16_part(parts.next(), raw, "major")?;
            let minor = parse_u16_opt(parts.next(), raw, "minor")?;
            let patch = parse_u16_opt(parts.next(), raw, "patch")?;
            return Ok(Self {
                major,
                minor,
                patch,
                build,
            });
        }

        let normalized = core.replace('_', ".");
        let mut parts = normalized.split('.');
        let major = parse_u16_part(parts.next(), raw, "major")?;
        let minor = parse_u16_opt(parts.next(), raw, "minor")?;
        let patch = parse_u16_opt(parts.next(), raw, "patch")?;

        Ok(Self {
            major,
            minor,
            patch,
            build,
        })
    }
}

impl fmt::Display for JavaVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(build) = self.build {
            write!(f, "+{build}")?;
        }
        Ok(())
    }
}

fn parse_u16_part(part: Option<&str>, raw: &str, label: &str) -> Result<u16, ToolchainError> {
    let Some(value) = part else {
        return Err(ToolchainError::InvalidJavaVersion {
            raw: raw.to_owned(),
            reason: format!("missing {label} version component"),
        });
    };
    if value.is_empty() {
        return Err(ToolchainError::InvalidJavaVersion {
            raw: raw.to_owned(),
            reason: format!("empty {label} version component"),
        });
    }
    value
        .parse::<u16>()
        .map_err(|err| ToolchainError::InvalidJavaVersion {
            raw: raw.to_owned(),
            reason: format!("invalid {label} version component `{value}`: {err}"),
        })
}

fn parse_u16_opt(part: Option<&str>, raw: &str, label: &str) -> Result<u16, ToolchainError> {
    match part {
        None => Ok(0),
        Some(value) => parse_u16_part(Some(value), raw, label),
    }
}

/// Known JDK distribution vendors.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum JdkVendor {
    /// Eclipse Adoptium / Temurin.
    Temurin,
    /// Oracle `GraalVM` or `GraalVM` Community.
    GraalVm,
    /// Amazon Corretto.
    Corretto,
    /// Azul Zulu.
    Zulu,
    /// Microsoft Build of `OpenJDK`.
    Microsoft,
    /// `JetBrains` Runtime (JBR).
    JetBrainsRuntime,
    /// Generic `OpenJDK` build.
    OpenJdk,
    /// Oracle JDK.
    Oracle,
    /// Unrecognized vendor string.
    Unknown(String),
}

impl JdkVendor {
    /// Infers [`JdkVendor`] from `release` file fields (`IMPLEMENTOR`, `IMPLEMENTOR_VERSION`, `BUILD_SOURCE`)
    /// and/or the installation directory name.
    #[must_use]
    pub fn from_release_fields(
        implementor: Option<&str>,
        implementor_version: Option<&str>,
        build_source: Option<&str>,
        dir_name: Option<&str>,
    ) -> Self {
        let combined = [
            implementor.unwrap_or_default(),
            implementor_version.unwrap_or_default(),
            build_source.unwrap_or_default(),
            dir_name.unwrap_or_default(),
        ]
        .join(" ")
        .to_ascii_lowercase();

        if combined.contains("temurin")
            || combined.contains("adoptium")
            || combined.contains("-tem")
        {
            Self::Temurin
        } else if combined.contains("graalvm") || combined.contains("graal") {
            Self::GraalVm
        } else if combined.contains("corretto") || combined.contains("amazon") {
            Self::Corretto
        } else if combined.contains("zulu") || combined.contains("azul") {
            Self::Zulu
        } else if combined.contains("microsoft") {
            Self::Microsoft
        } else if combined.contains("jetbrains") || combined.contains("jbr") {
            Self::JetBrainsRuntime
        } else if combined.contains("oracle") {
            Self::Oracle
        } else if combined.contains("openjdk") {
            Self::OpenJdk
        } else if let Some(raw) = implementor.filter(|s| !s.trim().is_empty()) {
            Self::Unknown(raw.trim().to_owned())
        } else {
            Self::Unknown("unknown".to_owned())
        }
    }

    /// Parses a vendor prefix from a requirement string such as `"temurin"` or `"tem"`.
    #[must_use]
    pub fn from_requirement_name(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "temurin" | "tem" | "adoptium" => Self::Temurin,
            "graalvm" | "graal" => Self::GraalVm,
            "corretto" | "amzn" | "amazon" => Self::Corretto,
            "zulu" => Self::Zulu,
            "microsoft" | "ms" => Self::Microsoft,
            "jbr" | "jetbrains" => Self::JetBrainsRuntime,
            "openjdk" | "open" => Self::OpenJdk,
            "oracle" => Self::Oracle,
            other => Self::Unknown(other.to_owned()),
        }
    }
}

impl fmt::Display for JdkVendor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Temurin => f.write_str("Temurin"),
            Self::GraalVm => f.write_str("GraalVM"),
            Self::Corretto => f.write_str("Corretto"),
            Self::Zulu => f.write_str("Zulu"),
            Self::Microsoft => f.write_str("Microsoft"),
            Self::JetBrainsRuntime => f.write_str("JetBrains Runtime"),
            Self::OpenJdk => f.write_str("OpenJDK"),
            Self::Oracle => f.write_str("Oracle"),
            Self::Unknown(name) => f.write_str(name),
        }
    }
}

/// Discovery source of a JDK installation, ordered from highest to lowest priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum JdkSource {
    /// Explicitly configured path in `kova.toml` or `settings.toml`.
    Explicit,
    /// Managed by Kova Omakase (`~/.local/share/kova/omakase/jdks/`).
    KovaOmakase,
    /// Configured via `JAVA_HOME` environment variable.
    EnvJavaHome,
    /// Installed via SDKMAN (`~/.sdkman/candidates/java/`).
    Sdkman,
    /// System macOS installation (`/Library/Java/JavaVirtualMachines/*/Contents/Home`).
    SystemMacOs,
    /// System Linux installation (`/usr/lib/jvm/*` or `/usr/lib64/jvm/*`).
    SystemLinux,
    /// Discovered via `PATH` (`java` executable).
    SystemPath,
}

/// Parsed JDK requirement from `kova.toml` (`jdk`) or `settings.toml` (`jvm.preferred_jdk`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum JdkRequirement {
    /// Any valid JDK is acceptable.
    Any,
    /// Exact major version (e.g., `"21"` or `"1.8"` -> `8`).
    ExactMajor(u16),
    /// Minimum major version (e.g., `">=17"`).
    MinMajor(u16),
    /// Specific vendor and major version (e.g., `"temurin-21"`).
    VendorAndMajor {
        /// Required JDK vendor.
        vendor: JdkVendor,
        /// Required major Java version.
        major: u16,
    },
    /// Explicit filesystem path to a JDK home directory.
    ExplicitPath(PathBuf),
}

impl JdkRequirement {
    /// Returns `true` if a JDK with the given `version`, `vendor`, and `home_dir` satisfies this requirement.
    #[must_use]
    pub fn matches(&self, version: JavaVersion, vendor: &JdkVendor, home_dir: &Path) -> bool {
        match self {
            Self::Any => true,
            Self::ExactMajor(major) => version.major == *major,
            Self::MinMajor(min_major) => version.major >= *min_major,
            Self::VendorAndMajor {
                vendor: req_vendor,
                major,
            } => version.major == *major && vendor == req_vendor,
            Self::ExplicitPath(path) => home_dir == path,
        }
    }
}

impl fmt::Display for JdkRequirement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Any => f.write_str("any"),
            Self::ExactMajor(major) => write!(f, "{major}"),
            Self::MinMajor(min_major) => write!(f, ">={min_major}"),
            Self::VendorAndMajor { vendor, major } => write!(f, "{vendor}-{major}"),
            Self::ExplicitPath(path) => write!(f, "{}", path.display()),
        }
    }
}

/// Parses a JDK requirement specification string.
///
/// Supported formats:
/// - `""` or `"any"` -> [`JdkRequirement::Any`]
/// - `">=17"` -> [`JdkRequirement::MinMajor`]
/// - `"21"` or `"1.8"` -> [`JdkRequirement::ExactMajor`]
/// - `"temurin-21"` -> [`JdkRequirement::VendorAndMajor`]
/// - `"/path/to/jdk"` (or Windows drive path) -> [`JdkRequirement::ExplicitPath`]
///
/// # Errors
/// Returns [`ToolchainError::InvalidJavaVersion`] if the specification cannot be parsed.
pub fn parse_jdk_requirement(spec: &str) -> Result<JdkRequirement, ToolchainError> {
    let trimmed = spec.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("any") {
        return Ok(JdkRequirement::Any);
    }

    if trimmed.starts_with('/')
        || trimmed.starts_with("~/")
        || trimmed.starts_with("\\\\")
        || (trimmed.len() >= 3
            && trimmed.as_bytes()[0].is_ascii_alphabetic()
            && trimmed.as_bytes()[1] == b':'
            && (trimmed.as_bytes()[2] == b'\\' || trimmed.as_bytes()[2] == b'/'))
    {
        return Ok(JdkRequirement::ExplicitPath(PathBuf::from(trimmed)));
    }

    if let Some(rest) = trimmed.strip_prefix(">=") {
        let ver = JavaVersion::parse(rest)?;
        return Ok(JdkRequirement::MinMajor(ver.major));
    }

    if let Some((vendor_part, version_part)) = trimmed.split_once('-')
        && !vendor_part.is_empty()
        && !version_part.is_empty()
        && vendor_part.chars().all(|c| c.is_ascii_alphabetic())
    {
        let ver = JavaVersion::parse(version_part)?;
        let vendor = JdkVendor::from_requirement_name(vendor_part);
        return Ok(JdkRequirement::VendorAndMajor {
            vendor,
            major: ver.major,
        });
    }

    let ver = JavaVersion::parse(trimmed)?;
    Ok(JdkRequirement::ExactMajor(ver.major))
}

/// Metadata extracted from a `<jdk_home>/release` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseMetadata {
    /// Parsed Java version.
    pub version: JavaVersion,
    /// Inferred JDK vendor.
    pub vendor: JdkVendor,
    /// Target architecture (e.g., `"aarch64"`, `"x86_64"`).
    pub arch: String,
    /// Raw key-value pairs from the `release` file.
    pub fields: BTreeMap<String, String>,
}

/// Parses the contents of a `<jdk_home>/release` file without spawning any external process.
///
/// # Errors
/// Returns [`ToolchainError::InvalidJavaVersion`] if `JAVA_VERSION` is missing or invalid.
pub fn parse_release_file(
    contents: &str,
    dir_name: Option<&str>,
) -> Result<ReleaseMetadata, ToolchainError> {
    let mut fields = BTreeMap::new();
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, raw_val)) = trimmed.split_once('=') else {
            continue;
        };
        let key = key.trim().to_owned();
        let val = unquote_release_value(raw_val.trim());
        fields.insert(key, val);
    }

    let raw_version =
        fields
            .get("JAVA_VERSION")
            .ok_or_else(|| ToolchainError::InvalidJavaVersion {
                raw: String::new(),
                reason: "missing JAVA_VERSION in release file".to_owned(),
            })?;

    let version = JavaVersion::parse(raw_version)?;
    let vendor = JdkVendor::from_release_fields(
        fields.get("IMPLEMENTOR").map(String::as_str),
        fields.get("IMPLEMENTOR_VERSION").map(String::as_str),
        fields.get("BUILD_SOURCE").map(String::as_str),
        dir_name,
    );
    let arch = fields
        .get("OS_ARCH")
        .cloned()
        .unwrap_or_else(|| std::env::consts::ARCH.to_owned());

    Ok(ReleaseMetadata {
        version,
        vendor,
        arch,
        fields,
    })
}

fn unquote_release_value(raw: &str) -> String {
    let stripped = if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
        &raw[1..raw.len() - 1]
    } else {
        raw
    };
    stripped.replace("\\\"", "\"").replace("\\\\", "\\")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modern_and_legacy_java_versions() {
        let v8 = JavaVersion::parse("1.8.0_432");
        assert_eq!(v8, Ok(JavaVersion::new(8, 0, 432, None)));

        let v17 = JavaVersion::parse("17");
        assert_eq!(v17, Ok(JavaVersion::new(17, 0, 0, None)));

        let v21 = JavaVersion::parse("21.0.5+11-LTS");
        assert_eq!(v21, Ok(JavaVersion::new(21, 0, 5, Some(11))));

        if let (Ok(v8), Ok(v17), Ok(v21)) = (v8, v17, v21) {
            assert!(v21 > v17);
            assert!(v17 > v8);
        }
    }

    #[test]
    fn parses_release_file_fields() {
        let content = r#"
# Generated by Adoptium
JAVA_VERSION="21.0.5"
IMPLEMENTOR="Eclipse Adoptium"
IMPLEMENTOR_VERSION="Temurin-21.0.5+11"
OS_ARCH="aarch64"
OS_NAME="Darwin"
"#;
        let meta = parse_release_file(content, Some("temurin-21"));
        assert!(meta.is_ok());
        if let Ok(meta) = meta {
            assert_eq!(meta.version, JavaVersion::new(21, 0, 5, None));
            assert_eq!(meta.vendor, JdkVendor::Temurin);
            assert_eq!(meta.arch, "aarch64");
        }
    }

    #[test]
    fn parses_jdk_requirements() {
        assert_eq!(parse_jdk_requirement(""), Ok(JdkRequirement::Any));
        assert_eq!(
            parse_jdk_requirement("21"),
            Ok(JdkRequirement::ExactMajor(21))
        );
        assert_eq!(
            parse_jdk_requirement("1.8"),
            Ok(JdkRequirement::ExactMajor(8))
        );
        assert_eq!(
            parse_jdk_requirement(">=17"),
            Ok(JdkRequirement::MinMajor(17))
        );
        assert_eq!(
            parse_jdk_requirement("temurin-21"),
            Ok(JdkRequirement::VendorAndMajor {
                vendor: JdkVendor::Temurin,
                major: 21,
            })
        );
        assert_eq!(
            parse_jdk_requirement("/opt/jdks/21"),
            Ok(JdkRequirement::ExplicitPath(PathBuf::from("/opt/jdks/21")))
        );
    }
}
