//! Headless state of the project shown in a workspace window (`KOVA-049`).
//!
//! `File → Open Project…` replaces the active project while an earlier tree scan may still be
//! running. [`ActiveProject`] tags every scan with a generation so the host can drop results
//! that belong to a project the user already left.

use std::path::{Path, PathBuf};

use super::project_display_name;

/// Loading phase of the active project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectPhase {
    /// No real project yet (built-in demo tree).
    Unloaded,
    /// The project tree of the current generation is being scanned.
    Loading,
    /// The tree of the current generation is shown.
    Ready,
    /// The scan of the current generation failed with this message.
    Failed(String),
}

/// Identifier of one project load; results of older generations are stale.
pub type ProjectGeneration = u64;

/// The project currently bound to a workspace window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveProject {
    root: Option<PathBuf>,
    name: String,
    generation: ProjectGeneration,
    phase: ProjectPhase,
}

impl ActiveProject {
    /// No project bound; `placeholder_name` is shown in the titlebar until one opens.
    #[must_use]
    pub fn unloaded(placeholder_name: impl Into<String>) -> Self {
        Self {
            root: None,
            name: placeholder_name.into(),
            generation: 0,
            phase: ProjectPhase::Unloaded,
        }
    }

    /// Binds `root` without starting a load (the host calls [`Self::begin_load`] later).
    pub fn set_root(&mut self, root: impl Into<PathBuf>) {
        let root = root.into();
        self.name = project_display_name(&root);
        self.root = Some(root);
        self.generation += 1;
        self.phase = ProjectPhase::Unloaded;
    }

    /// Switches to `root` and starts loading it; returns the generation tagging that load.
    ///
    /// Any load still running for the previous root becomes stale.
    pub fn switch_to(&mut self, root: impl Into<PathBuf>) -> ProjectGeneration {
        self.set_root(root);
        self.phase = ProjectPhase::Loading;
        self.generation
    }

    /// Starts (re)loading the bound root; `None` when no root is bound.
    pub fn begin_load(&mut self) -> Option<ProjectGeneration> {
        self.root.as_ref()?;
        self.phase = ProjectPhase::Loading;
        Some(self.generation)
    }

    /// Applies the outcome of load `generation`.
    ///
    /// Returns `false` (and changes nothing) when the result is stale.
    pub fn finish_load(
        &mut self,
        generation: ProjectGeneration,
        result: Result<(), String>,
    ) -> bool {
        if !self.is_current(generation) {
            return false;
        }
        self.phase = match result {
            Ok(()) => ProjectPhase::Ready,
            Err(message) => ProjectPhase::Failed(message),
        };
        true
    }

    /// Whether `generation` is the latest load of this window.
    #[must_use]
    pub const fn is_current(&self, generation: ProjectGeneration) -> bool {
        generation == self.generation
    }

    /// Bound project root.
    #[must_use]
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// Owned copy of the bound project root.
    #[must_use]
    pub fn root_owned(&self) -> Option<PathBuf> {
        self.root.clone()
    }

    /// Titlebar display name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Current loading phase.
    #[must_use]
    pub const fn phase(&self) -> &ProjectPhase {
        &self.phase
    }

    /// Whether the tree of the current generation is still being scanned.
    #[must_use]
    pub const fn is_loading(&self) -> bool {
        matches!(self.phase, ProjectPhase::Loading)
    }

    /// Current generation.
    #[must_use]
    pub const fn generation(&self) -> ProjectGeneration {
        self.generation
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{ActiveProject, ProjectPhase};
    use crate::project::project_display_name;

    #[test]
    fn starts_unloaded_with_placeholder() {
        let mut project = ActiveProject::unloaded("demo");
        assert_eq!(project.name(), "demo");
        assert_eq!(project.root(), None);
        assert_eq!(project.phase(), &ProjectPhase::Unloaded);
        assert_eq!(project.begin_load(), None);
    }

    #[test]
    fn cli_root_then_first_load_reaches_ready() {
        let mut project = ActiveProject::unloaded("demo");
        project.set_root("/work/orders");
        assert_eq!(project.name(), "orders");
        let generation = project.begin_load().expect("root bound");
        assert!(project.is_loading());
        assert!(project.finish_load(generation, Ok(())));
        assert_eq!(project.phase(), &ProjectPhase::Ready);
    }

    #[test]
    fn switching_project_makes_previous_scan_stale() {
        let mut project = ActiveProject::unloaded("demo");
        project.set_root("/work/orders");
        let first = project.begin_load().expect("root bound");

        let second = project.switch_to("/work/billing");
        assert_ne!(first, second);
        assert_eq!(project.root(), Some(Path::new("/work/billing")));
        assert_eq!(project.name(), "billing");
        assert!(project.is_loading());

        // The old scan finishes late: ignored, the new project keeps loading.
        assert!(!project.finish_load(first, Ok(())));
        assert!(project.is_loading());

        assert!(project.finish_load(second, Err("permission denied".to_owned())));
        assert_eq!(
            project.phase(),
            &ProjectPhase::Failed("permission denied".to_owned())
        );
    }

    #[test]
    fn reopening_same_project_starts_a_new_generation() {
        let mut project = ActiveProject::unloaded("demo");
        let a = project.switch_to("/work/orders");
        let b = project.switch_to("/work/orders");
        assert!(b > a);
        assert!(!project.is_current(a));
        assert!(project.is_current(b));
    }

    #[test]
    fn display_name_is_the_final_component() {
        assert_eq!(project_display_name(Path::new("/work/orders")), "orders");
        assert_eq!(project_display_name(Path::new("/")), "kova-project");
    }
}
