//! Bottom panel model (`Problems`, `Run`, `Tests`, `Debug`, `Terminal`).
//!
//! The active tab is not stored here: it is derived from
//! [`DockLayoutState::bottom_open`](crate::dock::DockLayoutState::bottom_open) so the stripe icons,
//! shortcuts and the panel tab bar share one source of truth.

use crate::dock::layout::DockToolId;

/// Height of the bottom panel tab bar in logical pixels.
pub const BOTTOM_TAB_BAR_HEIGHT_PX: f32 = 30.0;

/// Tab inside the bottom panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BottomTab {
    /// Diagnostics / problems list (`Alt+6`).
    Problems,
    /// Run output log stream (`Alt+4`).
    Run,
    /// Integrated PTY terminal (`Alt+F12`, `KOVA-039`).
    Terminal,
    /// `JUnit` Test Runner (`Alt+8`, `KOVA-040`).
    Tests,
    /// JVM Debugger (`Alt+5`, `KOVA-041`), sharing the dock layout with the Test Runner.
    Debug,
}

impl BottomTab {
    /// All bottom panel tabs in display order.
    pub const ALL: [Self; 5] = [
        Self::Problems,
        Self::Run,
        Self::Tests,
        Self::Debug,
        Self::Terminal,
    ];

    /// Returns the display title of this bottom panel tab.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Problems => "Problems",
            Self::Run => "Run",
            Self::Terminal => "Terminal",
            Self::Tests => "Tests",
            Self::Debug => "Debug",
        }
    }

    /// Returns the tool window id that opens this tab.
    #[must_use]
    pub const fn tool(self) -> DockToolId {
        match self {
            Self::Problems => DockToolId::Problems,
            Self::Run => DockToolId::Run,
            Self::Terminal => DockToolId::Terminal,
            Self::Tests => DockToolId::Tests,
            Self::Debug => DockToolId::Debug,
        }
    }

    /// Maps a bottom-anchored tool window to its tab.
    #[must_use]
    pub const fn from_tool(tool: DockToolId) -> Option<Self> {
        match tool {
            DockToolId::Problems => Some(Self::Problems),
            DockToolId::Run => Some(Self::Run),
            DockToolId::Terminal => Some(Self::Terminal),
            DockToolId::Tests => Some(Self::Tests),
            DockToolId::Debug => Some(Self::Debug),
            DockToolId::Project | DockToolId::Commit | DockToolId::Structure => None,
        }
    }
}

/// Severity of a diagnostic entry in the `Problems` tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProblemSeverity {
    /// Compile/syntax error.
    Error,
    /// Warning or lint.
    Warning,
}

/// A single diagnostic entry in the `Problems` tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProblemEntry {
    /// File path or name.
    pub file: String,
    /// 1-based line number.
    pub line: usize,
    /// 1-based column number.
    pub col: usize,
    /// Severity level.
    pub severity: ProblemSeverity,
    /// Diagnostic message.
    pub message: String,
}

/// One row of the grouped `Problems` list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProblemRow<'a> {
    /// File group header with its problem count.
    FileHeader {
        /// File path or name.
        file: &'a str,
        /// Number of problems in this file.
        count: usize,
    },
    /// A diagnostic entry (index into [`BottomPanelModel::problems`]).
    Entry(usize, &'a ProblemEntry),
}

/// Headless state for the bottom panel (`Problems`, `Run`, `Terminal`).
#[derive(Debug, Clone, Default)]
pub struct BottomPanelModel {
    /// Diagnostic entries for `Problems`, kept grouped by file.
    problems: Vec<ProblemEntry>,
    /// Editor analysis diagnostics (replaced on every analysis pass).
    analysis_problems: Vec<ProblemEntry>,
    /// Compile diagnostics reported by the build server, keyed by file (`KOVA-040`).
    build_problems: std::collections::BTreeMap<String, Vec<ProblemEntry>>,
    /// Log lines for `Run`.
    pub run_logs: Vec<String>,
    selected_problem: usize,
}

impl BottomPanelModel {
    /// Creates a model from diagnostics, sorting them by file, line and column for grouping.
    #[must_use]
    pub fn with_problems(problems: Vec<ProblemEntry>) -> Self {
        let mut model = Self {
            analysis_problems: problems,
            ..Self::default()
        };
        model.rebuild_problems();
        model
    }

    /// Replaces the editor analysis diagnostics (re-grouped by file, build diagnostics kept).
    pub fn set_problems(&mut self, problems: Vec<ProblemEntry>) {
        self.analysis_problems = problems;
        self.selected_problem = 0;
        self.rebuild_problems();
    }

    /// Replaces the build-server compile diagnostics of `file` (`KOVA-040`); an empty list
    /// clears them.
    pub fn set_build_problems(&mut self, file: &str, problems: Vec<ProblemEntry>) {
        if problems.is_empty() {
            self.build_problems.remove(file);
        } else {
            self.build_problems.insert(file.to_owned(), problems);
        }
        self.rebuild_problems();
        self.select_problem(self.selected_problem);
    }

    fn rebuild_problems(&mut self) {
        let mut problems = self.analysis_problems.clone();
        problems.extend(self.build_problems.values().flatten().cloned());
        problems.sort_by(|a, b| (&a.file, a.line, a.col).cmp(&(&b.file, b.line, b.col)));
        problems.dedup();
        self.problems = problems;
    }

    /// Returns all diagnostics, grouped by file.
    #[must_use]
    pub fn problems(&self) -> &[ProblemEntry] {
        &self.problems
    }

    /// Returns the number of error diagnostics.
    #[must_use]
    pub fn error_count(&self) -> usize {
        self.problems
            .iter()
            .filter(|p| p.severity == ProblemSeverity::Error)
            .count()
    }

    /// Returns the number of warning diagnostics.
    #[must_use]
    pub fn warning_count(&self) -> usize {
        self.problems
            .iter()
            .filter(|p| p.severity == ProblemSeverity::Warning)
            .count()
    }

    /// Returns the grouped rows (file header followed by its entries) for rendering.
    #[must_use]
    pub fn grouped_rows(&self) -> Vec<ProblemRow<'_>> {
        let mut rows = Vec::with_capacity(self.problems.len() + 4);
        let mut idx = 0;
        while let Some(first) = self.problems.get(idx) {
            let file = first.file.as_str();
            let count = self.problems[idx..]
                .iter()
                .take_while(|p| p.file == file)
                .count();
            rows.push(ProblemRow::FileHeader { file, count });
            for (offset, entry) in self.problems[idx..idx + count].iter().enumerate() {
                rows.push(ProblemRow::Entry(idx + offset, entry));
            }
            idx += count;
        }
        rows
    }

    /// Returns the index of the selected problem.
    #[must_use]
    pub const fn selected_problem_index(&self) -> usize {
        self.selected_problem
    }

    /// Selects the problem at `index` (clamped).
    pub fn select_problem(&mut self, index: usize) {
        self.selected_problem = index.min(self.problems.len().saturating_sub(1));
    }

    /// Moves the problem selection by one row down (`true`) or up (`false`).
    pub fn move_selection(&mut self, down: bool) {
        if down {
            self.select_problem(self.selected_problem.saturating_add(1));
        } else {
            self.selected_problem = self.selected_problem.saturating_sub(1);
        }
    }

    /// Returns the selected problem (target of `Enter` → jump to line/column).
    #[must_use]
    pub fn selected_problem(&self) -> Option<&ProblemEntry> {
        self.problems.get(self.selected_problem)
    }

    /// Returns the index of the next diagnostic after `(current_file, line, col)` with wrap-around.
    #[must_use]
    pub fn next_problem_index(
        &self,
        current_file: Option<&str>,
        line: usize,
        col: usize,
    ) -> Option<usize> {
        if self.problems.is_empty() {
            return None;
        }
        let Some(file) = current_file else {
            return Some((self.selected_problem + 1) % self.problems.len());
        };
        Some(
            self.problems
                .iter()
                .position(|p| (p.file.as_str(), p.line, p.col) > (file, line, col))
                .unwrap_or(0),
        )
    }

    /// Returns the index of the previous diagnostic before `(current_file, line, col)` with wrap-around.
    #[must_use]
    pub fn prev_problem_index(
        &self,
        current_file: Option<&str>,
        line: usize,
        col: usize,
    ) -> Option<usize> {
        if self.problems.is_empty() {
            return None;
        }
        let len = self.problems.len();
        let Some(file) = current_file else {
            return Some((self.selected_problem + len - 1) % len);
        };
        Some(
            self.problems
                .iter()
                .rposition(|p| (p.file.as_str(), p.line, p.col) < (file, line, col))
                .unwrap_or(len - 1),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(file: &str, line: usize, severity: ProblemSeverity) -> ProblemEntry {
        ProblemEntry {
            file: file.to_owned(),
            line,
            col: 5,
            severity,
            message: "msg".to_owned(),
        }
    }

    #[test]
    fn counts_errors_and_warnings_and_groups_by_file() {
        let model = BottomPanelModel::with_problems(vec![
            entry("OrderApp.java", 14, ProblemSeverity::Warning),
            entry("Order.java", 3, ProblemSeverity::Error),
            entry("OrderApp.java", 10, ProblemSeverity::Error),
        ]);

        assert_eq!(model.error_count(), 2);
        assert_eq!(model.warning_count(), 1);

        let rows = model.grouped_rows();
        assert_eq!(rows.len(), 5);
        assert_eq!(
            rows[0],
            ProblemRow::FileHeader {
                file: "Order.java",
                count: 1
            }
        );
        assert!(matches!(rows[3], ProblemRow::Entry(1, e) if e.line == 10));
    }

    #[test]
    fn selection_navigates_problems_for_jump_to_line() {
        let mut model = BottomPanelModel::default();
        assert!(model.selected_problem().is_none());
        model.set_problems(vec![
            entry("A.java", 1, ProblemSeverity::Error),
            entry("A.java", 9, ProblemSeverity::Warning),
        ]);
        model.move_selection(true);
        model.move_selection(true);
        assert_eq!(model.selected_problem().map(|p| p.line), Some(9));
        model.move_selection(false);
        assert_eq!(model.selected_problem_index(), 0);
    }

    #[test]
    fn build_problems_survive_analysis_refresh() {
        let mut model = BottomPanelModel::default();
        model.set_build_problems("B.java", vec![entry("B.java", 2, ProblemSeverity::Error)]);
        model.set_problems(vec![entry("A.java", 1, ProblemSeverity::Warning)]);
        assert_eq!(model.problems().len(), 2);
        assert_eq!(model.error_count(), 1);
        model.set_build_problems("B.java", Vec::new());
        assert_eq!(model.problems().len(), 1);
    }

    #[test]
    fn tabs_map_to_bottom_tools() {
        for tab in BottomTab::ALL {
            assert_eq!(BottomTab::from_tool(tab.tool()), Some(tab));
        }
        assert_eq!(BottomTab::from_tool(DockToolId::Project), None);
    }

    #[test]
    fn next_and_prev_problem_index_advance_and_wrap_around() {
        let empty = BottomPanelModel::default();
        assert_eq!(empty.next_problem_index(Some("A.java"), 1, 1), None);
        assert_eq!(empty.prev_problem_index(Some("A.java"), 1, 1), None);

        let model = BottomPanelModel::with_problems(vec![
            entry("A.java", 4, ProblemSeverity::Error),
            entry("A.java", 12, ProblemSeverity::Warning),
            entry("B.java", 3, ProblemSeverity::Error),
        ]);

        assert_eq!(model.next_problem_index(Some("A.java"), 4, 5), Some(1));
        assert_eq!(model.next_problem_index(Some("A.java"), 12, 5), Some(2));
        assert_eq!(model.next_problem_index(Some("B.java"), 3, 5), Some(0));

        assert_eq!(model.prev_problem_index(Some("A.java"), 4, 5), Some(2));
        assert_eq!(model.prev_problem_index(Some("B.java"), 3, 5), Some(1));
        assert_eq!(model.prev_problem_index(Some("A.java"), 12, 5), Some(0));
    }
}
