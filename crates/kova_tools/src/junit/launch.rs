//! Build-tool command lines for running `JUnit` targets without a BSP server (`KOVA-040`).
//!
//! The command is only *described* here (headless, no process is spawned); the host runs it and
//! then reads the `TEST-*.xml` reports listed in [`TestCommand::report_dirs`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::junit::xml_report::report_dirs;

/// JVM build tool driving a test run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuildTool {
    /// Gradle (`build.gradle`, `build.gradle.kts`, `settings.gradle*`).
    Gradle,
    /// Maven (`pom.xml`).
    Maven,
}

impl BuildTool {
    /// Detects the build tool of `project_root` (Gradle wins when both are present).
    #[must_use]
    pub fn detect(project_root: &Path) -> Option<Self> {
        let gradle = [
            "build.gradle",
            "build.gradle.kts",
            "settings.gradle",
            "settings.gradle.kts",
        ];
        if gradle.iter().any(|f| project_root.join(f).is_file()) {
            Some(Self::Gradle)
        } else if project_root.join("pom.xml").is_file() {
            Some(Self::Maven)
        } else {
            None
        }
    }

    /// Display name.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Gradle => "Gradle",
            Self::Maven => "Maven",
        }
    }
}

/// A fully described test process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestCommand {
    /// Build tool.
    pub tool: BuildTool,
    /// Executable (`./gradlew`, `gradle`, `./mvnw`, `mvn`).
    pub program: PathBuf,
    /// Arguments.
    pub args: Vec<String>,
    /// Working directory.
    pub working_dir: PathBuf,
    /// Extra environment variables.
    pub env: BTreeMap<String, String>,
    /// Directories holding the `TEST-*.xml` reports after the run.
    pub report_dirs: Vec<PathBuf>,
}

impl TestCommand {
    /// Shell-like rendering for the run console.
    #[must_use]
    pub fn display(&self) -> String {
        let mut out = self.program.display().to_string();
        for arg in &self.args {
            out.push(' ');
            if arg.contains(' ') {
                out.push('\'');
                out.push_str(arg);
                out.push('\'');
            } else {
                out.push_str(arg);
            }
        }
        out
    }
}

/// Builds the command running `targets` (`Class`, `Class#method`; empty = all tests).
///
/// Gradle uses `test --tests 'Class.method'` (one `--tests` per target); Maven uses
/// `test -Dtest=Class#method,Other` with `-Dsurefire.failIfNoSpecifiedTests=false` so a filter
/// that matches nothing in one module does not fail the reactor. The project wrapper
/// (`gradlew` / `mvnw`) is preferred over a global installation.
#[must_use]
pub fn build_test_command(
    tool: BuildTool,
    project_root: &Path,
    targets: &[String],
    vm_options: &[String],
    env: &BTreeMap<String, String>,
) -> TestCommand {
    let wrapper = |unix: &str, windows: &str, global: &str| -> PathBuf {
        let name = if cfg!(windows) { windows } else { unix };
        let local = project_root.join(name);
        if local.is_file() {
            local
        } else {
            PathBuf::from(global)
        }
    };
    let mut args = Vec::new();
    let program = match tool {
        BuildTool::Gradle => {
            args.push("test".to_owned());
            for target in targets {
                args.push("--tests".to_owned());
                args.push(target.replace(['#', '$'], "."));
            }
            if !vm_options.is_empty() {
                args.push(format!("-Dorg.gradle.jvmargs={}", vm_options.join(" ")));
            }
            args.push("--continue".to_owned());
            wrapper("gradlew", "gradlew.bat", "gradle")
        }
        BuildTool::Maven => {
            args.push("test".to_owned());
            if !targets.is_empty() {
                args.push(format!("-Dtest={}", targets.join(",")));
                args.push("-Dsurefire.failIfNoSpecifiedTests=false".to_owned());
            }
            if !vm_options.is_empty() {
                args.push(format!("-DargLine={}", vm_options.join(" ")));
            }
            wrapper("mvnw", "mvnw.cmd", "mvn")
        }
    };
    TestCommand {
        tool,
        program,
        args,
        working_dir: project_root.to_path_buf(),
        env: env.clone(),
        report_dirs: report_dirs(project_root),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_gradle_before_maven() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(BuildTool::detect(tmp.path()), None);
        std::fs::write(tmp.path().join("pom.xml"), "<project/>").unwrap();
        assert_eq!(BuildTool::detect(tmp.path()), Some(BuildTool::Maven));
        std::fs::write(tmp.path().join("build.gradle.kts"), "").unwrap();
        assert_eq!(BuildTool::detect(tmp.path()), Some(BuildTool::Gradle));
    }

    #[test]
    fn gradle_single_method_uses_tests_filter_and_wrapper() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("gradlew"), "#!/bin/sh").unwrap();
        let cmd = build_test_command(
            BuildTool::Gradle,
            tmp.path(),
            &[
                "com.kova.OrderTest#creates".to_owned(),
                "com.kova.Outer$Inner".to_owned(),
            ],
            &[],
            &BTreeMap::new(),
        );
        assert_eq!(cmd.program, tmp.path().join("gradlew"));
        assert_eq!(
            cmd.args,
            vec![
                "test",
                "--tests",
                "com.kova.OrderTest.creates",
                "--tests",
                "com.kova.Outer.Inner",
                "--continue"
            ]
        );
        assert!(
            cmd.report_dirs
                .contains(&tmp.path().join("build/test-results/test"))
        );
    }

    #[test]
    fn maven_single_method_uses_dtest_and_global_mvn() {
        let tmp = tempfile::tempdir().unwrap();
        let cmd = build_test_command(
            BuildTool::Maven,
            tmp.path(),
            &["com.kova.OrderTest#creates".to_owned()],
            &["-Xmx1G".to_owned()],
            &BTreeMap::new(),
        );
        assert_eq!(cmd.program, PathBuf::from("mvn"));
        assert_eq!(
            cmd.args,
            vec![
                "test",
                "-Dtest=com.kova.OrderTest#creates",
                "-Dsurefire.failIfNoSpecifiedTests=false",
                "-DargLine=-Xmx1G"
            ]
        );
        assert!(cmd.display().starts_with("mvn test -Dtest="));
    }
}
