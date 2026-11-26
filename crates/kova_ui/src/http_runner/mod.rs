//! `.http` runner presentation (`KOVA-042`): the response panel. Gutter `▶` markers come
//! from the shared `kova_syntax` entry-point pipeline. Parsing, variables and networking live headless in `kova_tools::http_runner`.

pub mod response_view;

use std::sync::OnceLock;

use kova_syntax::{LanguageRegistry, SyntaxEngine, SyntaxEngineConfig};

pub use response_view::{HttpResponseTab, HttpResponseView, HttpRunState, status_badge_color};

/// Process-wide single-threaded syntax engine used to highlight JSON response bodies.
///
/// Created lazily on the first response so it never costs cold-start time.
pub(crate) fn highlight_engine() -> Option<&'static SyntaxEngine> {
    static ENGINE: OnceLock<Option<SyntaxEngine>> = OnceLock::new();
    ENGINE
        .get_or_init(|| {
            let registry = LanguageRegistry::builtin().ok()?.clone();
            let config = SyntaxEngineConfig {
                worker_threads: 1,
                ..SyntaxEngineConfig::default()
            };
            SyntaxEngine::new(registry, config).ok()
        })
        .as_ref()
}
