//! Replays every sample in conformance/golden/translate.json against the Rust
//! translation layer. This is the M2 acceptance gate: all 52 samples (14
//! OpenAI requests + 15 Anthropic requests + 23 model resolutions) must match
//! the recorded output.
//!
//! The fixture holds both the inputs (`*RequestInputs`) and the expected
//! outputs, so the test needs no network and no Node.

use ccproxy::models::static_catalog;
use ccproxy::translate::{anthropic_to_cc, openai_to_cc, resolve_model, ModelTables};
use serde_json::Value;

const GOLDEN: &str = include_str!("../../../conformance/golden/translate.json");

fn golden() -> Value {
    serde_json::from_str(GOLDEN).expect("translate.json parses")
}

/// Drop the fields whose values legitimately differ per run, mirroring
/// `normalisePhone` in conformance/record-translate.mjs.
fn normalise(mut body: Value) -> Value {
    if let Some(obj) = body.as_object_mut() {
        if obj.contains_key("threadId") {
            obj.insert("threadId".into(), Value::String("<uuid>".into()));
        }
        if let Some(config) = obj.get_mut("config").and_then(Value::as_object_mut) {
            for key in ["workingDir", "date", "environment", "isGitRepo"] {
                if config.contains_key(key) {
                    config.insert(key.into(), Value::String("<normalised>".into()));
                }
            }
            if config.contains_key("structure") {
                config.insert("structure".into(), Value::String("<tree>".into()));
            }
        }
    }
    body
}

fn check_requests(section: &str, inputs_section: &str, call: impl Fn(&Value) -> Value) -> usize {
    let g = golden();
    let inputs = g[inputs_section]
        .as_object()
        .unwrap_or_else(|| panic!("{inputs_section} present"));
    let expected = g[section]
        .as_object()
        .unwrap_or_else(|| panic!("{section} present"));

    assert_eq!(
        inputs.len(),
        expected.len(),
        "{section}: inputs and outputs must describe the same samples"
    );

    let mut checked = 0;
    for (name, req) in inputs {
        let want = expected
            .get(name)
            .unwrap_or_else(|| panic!("{section}.{name}: no recorded output"));
        assert_eq!(
            want.get("ok").and_then(Value::as_bool),
            Some(true),
            "{section}.{name}: fixture recorded a failing sample"
        );
        let want_value = normalise(want["value"].clone());
        let got = normalise(call(req));
        assert_eq!(
            serde_json::to_string_pretty(&got).unwrap(),
            serde_json::to_string_pretty(&want_value).unwrap(),
            "{section}/{name} differs"
        );
        checked += 1;
    }
    checked
}

#[test]
fn golden_openai_requests_match() {
    let catalog = static_catalog();
    let tables = ModelTables::load();
    let n = check_requests("openaiRequests", "openaiRequestInputs", |req| {
        // The date and cwd in `config` are normalised away, so the process
        // environment does not leak into the comparison.
        openai_to_cc(req, &catalog, &tables, false)
    });
    assert_eq!(n, 14, "expected 14 OpenAI request samples");
}

#[test]
fn golden_anthropic_requests_match() {
    let catalog = static_catalog();
    let tables = ModelTables::load();
    let n = check_requests("anthropicRequests", "anthropicRequestInputs", |req| {
        // No ANTHROPIC_DEFAULT_MODEL: the recorded samples use the built-in
        // default, and the env override is covered by its own unit test.
        anthropic_to_cc(req, &catalog, &tables, None)
    });
    assert_eq!(n, 15, "expected 15 Anthropic request samples");
}

#[test]
fn golden_model_resolution_matches() {
    let catalog = static_catalog();
    let tables = ModelTables::load();
    let g = golden();
    let expected = g["modelResolution"].as_object().expect("modelResolution");

    let mut checked = 0;
    for (key, want) in expected {
        let requested = if key == "<empty>" { "" } else { key.as_str() };
        assert_eq!(want["ok"].as_bool(), Some(true), "modelResolution.{key}");
        assert_eq!(
            Value::String(resolve_model(requested, &catalog, &tables)),
            want["value"],
            "modelResolution/{key} differs"
        );
        checked += 1;
    }
    assert_eq!(checked, 23, "expected 23 model-resolution samples");
}
