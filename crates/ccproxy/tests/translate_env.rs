//! Behaviours the golden samples cannot pin because they are env-driven. The
//! golden records one environment; these cover the other branch.

use ccproxy::models::static_catalog;
use ccproxy::translate::{anthropic_to_cc, Env, ModelTables};
use serde_json::json;

fn tables() -> ModelTables {
    ModelTables::load()
}

#[test]
fn claude_names_map_to_the_default_model() {
    let (c, t) = (static_catalog(), tables());
    let req = json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "hi"}],
    });
    let body = anthropic_to_cc(&req, &c, &t, None);
    assert_eq!(body["params"]["model"], "deepseek/deepseek-v4-pro");
}

#[test]
fn anthropic_default_model_env_overrides_claude_names() {
    let (c, t) = (static_catalog(), tables());
    let req = json!({
        "model": "claude-opus-4-6",
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "hi"}],
    });
    let body = anthropic_to_cc(&req, &c, &t, Some("kimi-k3"));
    // The override is itself run through resolveModel, so an alias works.
    assert_eq!(body["params"]["model"], "moonshotai/Kimi-K3");
}

#[test]
fn non_claude_names_ignore_the_default_model_env() {
    let (c, t) = (static_catalog(), tables());
    let req = json!({
        "model": "grok-4.6",
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "hi"}],
    });
    let body = anthropic_to_cc(&req, &c, &t, Some("kimi-k3"));
    assert_eq!(body["params"]["model"], "xai/grok-4.6");
}

#[test]
fn env_from_process_reads_both_knobs() {
    let env = Env::from_process();
    // Whatever the ambient values are, both fields must be populated without
    // panicking; the guard flag is exactly the "off" string.
    assert!(
        env.no_tools_guard_off
            == (std::env::var("CC_NO_TOOLS_GUARD").ok().as_deref() == Some("off"))
    );
}
