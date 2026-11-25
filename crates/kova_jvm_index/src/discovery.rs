//! `.gitignore`-aware project source file discovery for JVM languages.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use ignore::WalkBuilder;

use crate::error::JvmIndexError;

const DEFAULT_EXCLUDED_DIRS: &[&str] =
    &[".git", "target", "build", ".gradle", "out", "node_modules"];

/// Options controlling project source file discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceDiscoveryOptions {
    /// Whether to respect `.gitignore` and `.ignore` files.
    pub respect_gitignore: bool,
    /// Directory names excluded unconditionally even if not listed in `.gitignore`.
    pub excluded_dirs: Vec<String>,
}

impl Default for SourceDiscoveryOptions {
    fn default() -> Self {
        Self {
            respect_gitignore: true,
            excluded_dirs: DEFAULT_EXCLUDED_DIRS
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
        }
    }
}

/// Metadata for a discovered `.java`, `.kt`, or `.kts` source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredSourceFile {
    /// Absolute path on disk.
    pub abs_path: PathBuf,
    /// Path relative to `project_root`.
    pub rel_path: PathBuf,
    /// File size in bytes.
    pub file_size: u64,
    /// Last modification timestamp in nanoseconds since `UNIX_EPOCH`.
    pub modified_unix_ns: u64,
}

/// Collects all `.java`, `.kt`, and `.kts` files under `project_root` in deterministic order.
///
/// Respects `.gitignore` / `.ignore`, skips `.git/`, `target/`, `build/`, `.gradle/`, `out/`,
/// `node_modules/`, and never follows symlinks.
///
/// # Errors
///
/// Returns [`JvmIndexError::Io`] if `project_root` cannot be walked or read.
pub fn collect_project_sources(
    project_root: &Path,
    options: &SourceDiscoveryOptions,
) -> Result<Vec<DiscoveredSourceFile>, JvmIndexError> {
    if !project_root.exists() {
        return Err(JvmIndexError::Io {
            path: project_root.display().to_string(),
            message: String::from("project root does not exist"),
        });
    }

    let excluded = options.excluded_dirs.clone();
    let mut builder = WalkBuilder::new(project_root);
    builder
        .hidden(true)
        .require_git(false)
        .git_ignore(options.respect_gitignore)
        .git_global(options.respect_gitignore)
        .git_exclude(options.respect_gitignore)
        .ignore(options.respect_gitignore)
        .follow_links(false)
        .filter_entry(move |entry| {
            if entry.depth() == 0 {
                return true;
            }
            if entry.file_type().is_some_and(|ft| ft.is_dir()) {
                if let Some(name) = entry.file_name().to_str() {
                    return !excluded.iter().any(|ex| ex == name);
                }
            }
            true
        });

    let mut files = Vec::new();
    for result in builder.build() {
        let Ok(entry) = result else { continue };
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let abs_path = entry.into_path();
        if !is_jvm_source_file(&abs_path) {
            continue;
        }
        let rel_path = abs_path
            .strip_prefix(project_root)
            .unwrap_or(&abs_path)
            .to_path_buf();
        let Ok(meta) = std::fs::metadata(&abs_path) else {
            continue;
        };
        let modified_unix_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .and_then(|d| u64::try_from(d.as_nanos()).ok())
            .unwrap_or(0);

        files.push(DiscoveredSourceFile {
            abs_path,
            rel_path,
            file_size: meta.len(),
            modified_unix_ns,
        });
    }

    files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(files)
}

fn is_jvm_source_file(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    ext.eq_ignore_ascii_case("java")
        || ext.eq_ignore_ascii_case("kt")
        || ext.eq_ignore_ascii_case("kts")
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn respects_gitignore_and_excludes_build_dirs() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        std::fs::write(root.join(".gitignore"), "*.generated.java\nignored_dir/\n")
            .expect("write gitignore");

        std::fs::create_dir_all(root.join("src/main/java")).expect("mkdir src");
        std::fs::create_dir_all(root.join("target/classes")).expect("mkdir target");
        std::fs::create_dir_all(root.join("build/generated")).expect("mkdir build");
        std::fs::create_dir_all(root.join("ignored_dir")).expect("mkdir ignored");

        std::fs::write(root.join("src/main/java/App.java"), "class App {}").expect("write App");
        std::fs::write(root.join("src/main/java/Main.kt"), "fun main() {}").expect("write Main");
        std::fs::write(
            root.join("src/main/java/Foo.generated.java"),
            "class Foo {}",
        )
        .expect("write generated");
        std::fs::write(root.join("target/classes/Out.java"), "class Out {}").expect("write target");
        std::fs::write(root.join("build/generated/Gen.kt"), "class Gen {}").expect("write build");
        std::fs::write(root.join("ignored_dir/Skip.java"), "class Skip {}").expect("write skip");

        let found = collect_project_sources(root, &SourceDiscoveryOptions::default())
            .expect("collect sources");
        let rels: Vec<&Path> = found.iter().map(|f| f.rel_path.as_path()).collect();
        assert_eq!(
            rels,
            vec![
                Path::new("src/main/java/App.java"),
                Path::new("src/main/java/Main.kt"),
            ]
        );
    }
}
