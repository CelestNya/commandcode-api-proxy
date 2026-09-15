//! Translation layer: client request in, CC `/alpha/generate` body out
//! (request half; the streaming encoders land in M3).
//!
//! Ported from src/translate/. The decision order in `models` and the field
//! presence rules in `openai`/`anthropic` are the contract — the golden samples
//! pin them, and the tests in `golden` replay all 52 of them.

pub mod anthropic;
pub mod models;
pub mod openai;
pub mod util;

use crate::models::Catalog;
use serde_json::Value;

pub use anthropic::{resolve_anthropic_model, to_cc_request as anthropic_to_cc};
pub use models::{needs_catalog_discovery, resolve_effort_for_model, resolve_model, ModelTables};
pub use openai::to_cc_request as openai_to_cc;
pub use util::{build_cc_config, prune_dangling_tools, truthy};

/// The two environment knobs the translation layer honors. Read once at the
/// call site and threaded through, so the translators stay pure and the tests
/// do not depend on ambient environment variables.
#[derive(Debug, Clone, Default)]
pub struct Env {
    /// `ANTHROPIC_DEFAULT_MODEL` — what `claude-*` request names map to.
    pub anthropic_default_model: Option<String>,
    /// `CC_NO_TOOLS_GUARD=off` — skip the no-tools instruction injection.
    pub no_tools_guard_off: bool,
}

impl Env {
    pub fn from_process() -> Self {
        Self {
            anthropic_default_model: std::env::var("ANTHROPIC_DEFAULT_MODEL")
                .ok()
                .filter(|s| !s.is_empty()),
            no_tools_guard_off: std::env::var("CC_NO_TOOLS_GUARD").ok().as_deref() == Some("off"),
        }
    }
}

/// Anthropic request -> CC body, reading the two environment knobs.
pub fn anthropic_to_cc_with_env(
    req: &Value,
    catalog: &Catalog,
    tables: &ModelTables,
    env: &Env,
) -> Value {
    anthropic::to_cc_request(req, catalog, tables, env.anthropic_default_model.as_deref())
}
