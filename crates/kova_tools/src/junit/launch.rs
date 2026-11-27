//! Build-tool command lines for running `JUnit` targets without a BSP server (`KOVA-040`).
//!
//! The command is only *described* here (headless, no process is spawned); the host runs it and
//! then reads the `TEST-*.xml` reports listed in [`TestCommand::report_dirs`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::junit::xml_report::report_dirs;

/// Build tool driving a build or test run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuildTool {
    /// Gradle (`build.gradle`, `build.gradle.kts`, `settings.gradle*`).
    Gradle,
    /// Maven (`pom.xml`).
    Maven,
    /// Cargo (`Cargo.toml`).
    Cargo,
}

impl BuildTool {
    /// Detects the build tool of `project_root` (`Gradle` > `Maven` > `Cargo`).
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
        } else if project_root.join("Cargo.toml").is_file() {
            Some(Self::Cargo)
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
            Self::Cargo => "Cargo",
        }
    }
}

/// A fully described project compile/build process (`Build Project`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildCommand {
    /// Build tool.
    pub tool: BuildTool,
    /// Executable (`./gradlew`, `gradle`, `./mvnw`, `mvn`, `cargo`).
    pub program: PathBuf,
    /// Arguments.
    pub args: Vec<String>,
    /// Working directory.
    pub working_dir: PathBuf,
    /// Extra environment variables.
    pub env: BTreeMap<String, String>,
}

impl BuildCommand {
    /// Shell-like rendering for the run console.
    #[must_use]
    pub fn display(&self) -> String {
        format_command_display(&self.program, &self.args)
    }
}

/// A fully described test process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestCommand {
    /// Build tool.
    pub tool: BuildTool,
    /// Executable (`./gradlew`, `gradle`, `./mvnw`, `mvn`, `cargo`).
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
        format_command_display(&self.program, &self.args)
    }
}

fn format_command_display(program: &Path, args: &[String]) -> String {
    let mut out = program.display().to_string();
    for arg in args {
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

fn resolve_tool_wrapper(project_root: &Path, unix: &str, windows: &str, global: &str) -> PathBuf {
    let name = if cfg!(windows) { windows } else { unix };
    let local = project_root.join(name);
    if local.is_file() {
        local
    } else {
        PathBuf::from(global)
    }
}

/// Builds the compile command (`Build Project`, `Cmd+F9`) for `tool` in `project_root`.
#[must_use]
pub fn build_compile_command(
    tool: BuildTool,
    project_root: &Path,
    env: &BTreeMap<String, String>,
) -> BuildCommand {
    let (program, args) = match tool {
        BuildTool::Gradle => (
            resolve_tool_wrapper(project_root, "gradlew", "gradlew.bat", "gradle"),
            vec![
                "classes".to_owned(),
                "testClasses".to_owned(),
                "--console=plain".to_owned(),
            ],
        ),
        BuildTool::Maven => (
            resolve_tool_wrapper(project_root, "mvnw", "mvnw.cmd", "mvn"),
            vec!["test-compile".to_owned(), "-B".to_owned()],
        ),
        BuildTool::Cargo => (
            PathBuf::from("cargo"),
            vec![
                "build".to_owned(),
                "--all-targets".to_owned(),
                "--message-format=json".to_owned(),
            ],
        ),
    };
    BuildCommand {
        tool,
        program,
        args,
        working_dir: project_root.to_path_buf(),
        env: env.clone(),
    }
}

/// Formats a `JUnit` or `Spock` test target (`Class`, `Class#method`, `Outer$Inner#method`, or
/// `pkg.*`) into a Gradle `--tests` filter without corrupting `$` or `#` inside Spock method names.
#[must_use]
pub fn format_gradle_test_filter(target: &str) -> String {
    if let Some((class_part, method_part)) = target.split_once('#')
        && !method_part.is_empty()
    {
        format!("{}.{method_part}", class_part.replace('$', "."))
    } else {
        target.replace('$', ".")
    }
}

/// Formats a test target (`Class`, `Class#method`, or package pattern `pkg.*`) for Maven Surefire
/// `-Dtest=...`.
#[must_use]
pub fn format_maven_test_filter(target: &str) -> String {
    if !target.contains('#')
        && let Some(pkg) = target.strip_suffix(".*")
    {
        return format!("{}/**", pkg.replace('.', "/"));
    }
    target.to_owned()
}

/// Builds the command running `targets` (`Class`, `Class#method`, `pkg.*`; empty = all tests).
///
/// Gradle uses `test --tests 'Class.method' --console=plain`; Maven uses
/// `test -Dtest=Class#method,Other -Dsurefire.failIfNoSpecifiedTests=false -B` so a filter
/// that matches nothing in one module does not fail the reactor; Cargo uses `cargo test
/// --no-fail-fast [<filter> ...]`. The project wrapper (`gradlew` / `mvnw`) is preferred over
/// a global installation.
#[must_use]
pub fn build_test_command(
    tool: BuildTool,
    project_root: &Path,
    targets: &[String],
    vm_options: &[String],
    env: &BTreeMap<String, String>,
) -> TestCommand {
    let mut args = Vec::new();
    let (program, reports) = match tool {
        BuildTool::Gradle => {
            args.push("test".to_owned());
            for target in targets {
                args.push("--tests".to_owned());
                args.push(format_gradle_test_filter(target));
            }
            if !vm_options.is_empty() {
                args.push(format!("-Dorg.gradle.jvmargs={}", vm_options.join(" ")));
            }
            args.push("--continue".to_owned());
            args.push("--console=plain".to_owned());
            (
                resolve_tool_wrapper(project_root, "gradlew", "gradlew.bat", "gradle"),
                report_dirs(project_root),
            )
        }
        BuildTool::Maven => {
            args.push("test".to_owned());
            if !targets.is_empty() {
                let formatted: Vec<String> = targets
                    .iter()
                    .map(|t| format_maven_test_filter(t))
                    .collect();
                args.push(format!("-Dtest={}", formatted.join(",")));
                args.push("-Dsurefire.failIfNoSpecifiedTests=false".to_owned());
            }
            if !vm_options.is_empty() {
                args.push(format!("-DargLine={}", vm_options.join(" ")));
            }
            args.push("-B".to_owned());
            (
                resolve_tool_wrapper(project_root, "mvnw", "mvnw.cmd", "mvn"),
                report_dirs(project_root),
            )
        }
        BuildTool::Cargo => {
            args.push("test".to_owned());
            args.push("--no-fail-fast".to_owned());
            for target in targets {
                let filter = target.replace(['#', '$'], "::");
                if !filter.is_empty() {
                    args.push(filter);
                }
            }
            (PathBuf::from("cargo"), Vec::new())
        }
    };
    TestCommand {
        tool,
        program,
        args,
        working_dir: project_root.to_path_buf(),
        env: env.clone(),
        report_dirs: reports,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_gradle_maven_and_cargo_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(BuildTool::detect(tmp.path()), None);
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        assert_eq!(BuildTool::detect(tmp.path()), Some(BuildTool::Cargo));
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
                "--continue",
                "--console=plain",
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
                "-DargLine=-Xmx1G",
                "-B",
            ]
        );
        assert!(cmd.display().starts_with("mvn test -Dtest="));
    }

    #[test]
    fn build_compile_and_cargo_test_commands_match_expected_flags() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("gradlew"), "").unwrap();
        let gradle = build_compile_command(BuildTool::Gradle, tmp.path(), &BTreeMap::new());
        assert_eq!(gradle.program, tmp.path().join("gradlew"));
        assert_eq!(
            gradle.args,
            vec!["classes", "testClasses", "--console=plain"]
        );

        let maven = build_compile_command(BuildTool::Maven, tmp.path(), &BTreeMap::new());
        assert_eq!(maven.program, PathBuf::from("mvn"));
        assert_eq!(maven.args, vec!["test-compile", "-B"]);

        let cargo_build = build_compile_command(BuildTool::Cargo, tmp.path(), &BTreeMap::new());
        assert_eq!(cargo_build.program, PathBuf::from("cargo"));
        assert_eq!(
            cargo_build.args,
            vec!["build", "--all-targets", "--message-format=json"]
        );

        let cargo_test = build_test_command(
            BuildTool::Cargo,
            tmp.path(),
            &["suite::tests#it_works".to_owned()],
            &[],
            &BTreeMap::new(),
        );
        assert_eq!(cargo_test.program, PathBuf::from("cargo"));
        assert_eq!(
            cargo_test.args,
            vec!["test", "--no-fail-fast", "suite::tests::it_works"]
        );
        assert_eq!(cargo_test.report_dirs, Vec::<PathBuf>::new());
    }

    #[test]
    fn gradle_and_maven_preserve_spock_method_names_and_package_patterns() {
        let tmp = tempfile::tempdir().unwrap();
        let gradle = build_test_command(
            BuildTool::Gradle,
            tmp.path(),
            &[
                "com.example.OrderSpec#should cost $10 (#1)".to_owned(),
                "com.example.Outer$InnerSpec#handles spaces".to_owned(),
                "com.example.orders.*".to_owned(),
            ],
            &[],
            &BTreeMap::new(),
        );
        assert_eq!(
            gradle.args,
            vec![
                "test",
                "--tests",
                "com.example.OrderSpec.should cost $10 (#1)",
                "--tests",
                "com.example.Outer.InnerSpec.handles spaces",
                "--tests",
                "com.example.orders.*",
                "--continue",
                "--console=plain",
            ]
        );
        assert!(
            gradle
                .display()
                .contains("--tests 'com.example.OrderSpec.should cost $10 (#1)'")
        );

        let maven = build_test_command(
            BuildTool::Maven,
            tmp.path(),
            &[
                "com.example.OrderSpec#should cost $10 (#1)".to_owned(),
                "com.example.orders.*".to_owned(),
            ],
            &[],
            &BTreeMap::new(),
        );
        assert_eq!(
            maven.args,
            vec![
                "test",
                "-Dtest=com.example.OrderSpec#should cost $10 (#1),com/example/orders/**",
                "-Dsurefire.failIfNoSpecifiedTests=false",
                "-B",
            ]
        );
    }
}
