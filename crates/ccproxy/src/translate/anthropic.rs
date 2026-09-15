//! Anthropic Messages -> CC `/alpha/generate` body, ported from
//! src/translate/anthropic.ts. Request half only; the streaming encoder is M3.

use crate::models::Catalog;
use crate::translate::models::{resolve_effort_for_model, resolve_model, ModelTables};
use crate::translate::util::{build_cc_config, prune_dangling_tools};
use crate::validation::is_effort_off;
use serde_json::{json, Map, Value};
use std::collections::HashMap;

/// Budget bands. Order matters: the first band whose ceiling the budget does
/// not exceed wins, so 2000 is "low" and 2001 is "medium".
const LOW: f64 = 2000.0;
const MEDIUM: f64 = 8000.0;
const HIGH: f64 = 16000.0;
const XHIGH: f64 = 32000.0;

/// Level used when the client asks for thinking to be off. The upstream accepts
/// no "off" value, so this is the closest expressible intent: the lowest level
/// the model supports (clipping raises it to that floor). Dropping the field
/// instead hands the choice back to the upstream's default, which is exactly
/// what someone turning thinking off is trying to avoid.
const DISABLED_THINKING_EFFORT: &str = "low";

/// Claude-branded names are not CC models; they map to the default model (or
/// `ANTHROPIC_DEFAULT_MODEL` when set).
pub fn resolve_anthropic_model(
    requested: &str,
    catalog: &Catalog,
    tables: &ModelTables,
    env_default: Option<&str>,
) -> String {
    if !requested.starts_with("claude-") {
        return resolve_model(requested, catalog, tables);
    }
    if let Some(default) = env_default {
        return resolve_model(default, catalog, tables);
    }
    let first = catalog.ids.first().map(String::as_str).unwrap_or("");
    resolve_model(first, catalog, tables)
}

pub fn to_cc_request(
    req: &Value,
    catalog: &Catalog,
    tables: &ModelTables,
    env_default_model: Option<&str>,
) -> Value {
    let messages: Vec<Value> = req
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let (mut cc_messages, system_prompt) = to_cc_messages(&messages);
    prune_dangling_tools(&mut cc_messages);

    let requested_model = req.get("model").and_then(Value::as_str).unwrap_or("");
    let resolved_model =
        resolve_anthropic_model(requested_model, catalog, tables, env_default_model);

    // Top-level `system`, string or block array. Blocks join on "\n\n" after
    // dropping non-text blocks and empty text.
    let system_text = match req.get("system") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Array(blocks)) => {
            let joined = blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .map(|b| {
                    b.get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string()
                })
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n");
            Some(joined)
        }
        _ => None,
    };

    // The history-derived prompt is prepended to the top-level one, not merged.
    let final_system = match system_text {
        Some(text) if !text.is_empty() => match system_prompt {
            Some(sp) => Some(format!("{sp}\n\n{text}")),
            None => Some(text),
        },
        _ => system_prompt,
    };

    let mut params = Map::new();
    params.insert("model".into(), Value::String(resolved_model.clone()));
    params.insert("messages".into(), Value::Array(cc_messages));
    params.insert(
        "stream".into(),
        match req.get("stream") {
            None | Some(Value::Null) => Value::Bool(false),
            Some(v) => v.clone(),
        },
    );
    for key in ["max_tokens", "temperature", "top_p"] {
        if let Some(v) = req.get(key) {
            params.insert(key.into(), v.clone());
        }
    }
    if let Some(stop) = req.get("stop_sequences") {
        params.insert("stop".into(), stop.clone());
    }
    if let Some(tools) = req.get("tools").and_then(Value::as_array) {
        let mapped: Vec<Value> = tools
            .iter()
            .map(|t| {
                let mut m = Map::new();
                for (from, to) in [
                    ("name", "name"),
                    ("description", "description"),
                    ("input_schema", "input_schema"),
                ] {
                    if let Some(v) = t.get(from) {
                        m.insert(to.into(), v.clone());
                    }
                }
                Value::Object(m)
            })
            .collect();
        params.insert("tools".into(), Value::Array(mapped));
    }
    if let Some(tc) = resolve_tool_choice(req.get("tool_choice")) {
        params.insert("tool_choice".into(), tc);
    }
    let effort = resolve_effort_for_model(
        &resolved_model,
        resolve_reasoning_effort(req).as_deref(),
        tables,
    );
    if let Some(effort) = effort {
        params.insert("reasoning_effort".into(), Value::String(effort));
    }
    if let Some(fs) = final_system.filter(|s| !s.is_empty()) {
        params.insert("system".into(), Value::String(fs));
    }

    json!({
        "config": build_cc_config(),
        "memory": "",
        "taste": "",
        "skills": "",
        "permissionMode": "standard",
        "params": Value::Object(params),
        "threadId": uuid::Uuid::new_v4().to_string(),
    })
}

fn to_cc_messages(messages: &[Value]) -> (Vec<Value>, Option<String>) {
    let mut tool_use_id_to_name: HashMap<String, String> = HashMap::new();
    for msg in messages {
        if msg.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(blocks) = msg.get("content").and_then(Value::as_array) else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                if let (Some(id), Some(name)) = (
                    block.get("id").and_then(Value::as_str),
                    block.get("name").and_then(Value::as_str),
                ) {
                    tool_use_id_to_name.insert(id.to_string(), name.to_string());
                }
            }
        }
    }

    let mut cc: Vec<Value> = Vec::new();
    let mut system_parts: Vec<String> = Vec::new();

    for msg in messages {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
        let content = msg.get("content");

        match role {
            "system" => {
                if let Some(Value::String(s)) = content {
                    system_parts.push(s.clone());
                }
            }

            "user" => match content {
                Some(Value::String(s)) => cc.push(json!({"role": "user", "content": s})),
                Some(Value::Array(blocks)) => {
                    let has_tool_result = blocks
                        .iter()
                        .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"));
                    if has_tool_result {
                        // Each tool_result becomes its own `tool` message, and
                        // any sibling text blocks are emitted after them.
                        let mut text_parts: Vec<Value> = Vec::new();
                        for block in blocks {
                            if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                                cc.push(tool_result_message(block, &tool_use_id_to_name));
                            } else if let Some(part) = to_cc_part(block) {
                                text_parts.push(part);
                            }
                        }
                        if !text_parts.is_empty() {
                            cc.push(json!({"role": "user", "content": text_parts}));
                        }
                    } else {
                        let parts: Vec<Value> = blocks.iter().filter_map(to_cc_part).collect();
                        cc.push(json!({"role": "user", "content": parts}));
                    }
                }
                _ => {}
            },

            "assistant" => match content {
                Some(Value::String(s)) => cc.push(json!({"role": "assistant", "content": s})),
                Some(Value::Array(blocks)) => {
                    // thinking blocks are dropped: CC neither accepts them as
                    // input nor returns a signature to echo back.
                    let mut parts: Vec<Value> = Vec::new();
                    for block in blocks {
                        match block.get("type").and_then(Value::as_str) {
                            Some("text") => parts.push(json!({
                                "type": "text",
                                "text": block.get("text").and_then(Value::as_str).unwrap_or(""),
                            })),
                            Some("tool_use") => {
                                let mut m = Map::new();
                                m.insert("type".into(), Value::String("tool-call".into()));
                                if let Some(id) = block.get("id") {
                                    m.insert("toolCallId".into(), id.clone());
                                }
                                if let Some(name) = block.get("name") {
                                    m.insert("toolName".into(), name.clone());
                                }
                                if let Some(input) = block.get("input") {
                                    m.insert("input".into(), input.clone());
                                }
                                parts.push(Value::Object(m));
                            }
                            _ => {}
                        }
                    }
                    if !parts.is_empty() {
                        cc.push(json!({"role": "assistant", "content": parts}));
                    }
                }
                _ => {}
            },

            _ => {}
        }
    }

    let system_prompt = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n\n"))
    };
    (cc, system_prompt)
}

fn tool_result_message(block: &Value, names: &HashMap<String, String>) -> Value {
    let tool_use_id = block
        .get("tool_use_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let name = names.get(tool_use_id).cloned().unwrap_or_default();
    let result_text = match block.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|p| {
                p.get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    };

    let mut m = Map::new();
    m.insert("type".into(), Value::String("tool-result".into()));
    m.insert("toolCallId".into(), Value::String(tool_use_id.into()));
    m.insert("toolName".into(), Value::String(name));
    m.insert(
        "output".into(),
        json!({"type": "text", "value": result_text}),
    );
    // `isError: undefined` is dropped by JSON.stringify; only a present value
    // is emitted (false included).
    if let Some(is_error) = block.get("is_error") {
        if !is_error.is_null() {
            m.insert("isError".into(), is_error.clone());
        }
    }

    json!({"role": "tool", "content": [Value::Object(m)]})
}

/// Non-text, non-image blocks (thinking, tool_use in a user turn, unknown
/// types) translate to nothing.
fn to_cc_part(block: &Value) -> Option<Value> {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => Some(json!({
            "type": "text",
            "text": block.get("text").and_then(Value::as_str).unwrap_or(""),
        })),
        Some("image") => {
            let source = block.get("source")?;
            if source.get("type").and_then(Value::as_str) == Some("base64") {
                let media_type = source
                    .get("media_type")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let data = source.get("data").and_then(Value::as_str).unwrap_or("");
                Some(json!({
                    "type": "image",
                    "image": format!("data:{media_type};base64,{data}"),
                }))
            } else {
                Some(json!({"type": "image", "image": source.get("url")?.clone()}))
            }
        }
        _ => None,
    }
}

/// Effort from whichever field the client used, priority highest first:
///   1. `thinking.type == "disabled"`, or an "off" marker in
///      `output_config.effort`
///   2. `output_config.effort`
///   3. `thinking.budget_tokens` (also accepted as `budgetTokens`)
///
/// A missing budget yields no level at all. The earlier version returned "max"
/// in that case — `undefined <= 2000` is false, so every comparison fell
/// through to the last branch.
fn resolve_reasoning_effort(req: &Value) -> Option<String> {
    let thinking = req.get("thinking");
    let explicit = req
        .get("output_config")
        .and_then(|c| c.get("effort"))
        .filter(|v| crate::translate::util::truthy(v));

    if thinking.and_then(|t| t.get("type")).and_then(Value::as_str) == Some("disabled")
        || explicit.is_some_and(is_effort_off)
    {
        return Some(DISABLED_THINKING_EFFORT.to_string());
    }
    if let Some(v) = explicit {
        if let Some(s) = v.as_str() {
            return Some(s.to_string());
        }
    }
    if thinking.and_then(|t| t.get("type")).and_then(Value::as_str) != Some("enabled") {
        return None;
    }

    let raw = thinking
        .and_then(|t| t.get("budget_tokens"))
        .filter(|v| !v.is_null())
        .or_else(|| thinking.and_then(|t| t.get("budgetTokens")));
    let budget = raw.and_then(Value::as_f64).filter(|b| b.is_finite())?;

    Some(
        if budget <= LOW {
            "low"
        } else if budget <= MEDIUM {
            "medium"
        } else if budget <= HIGH {
            "high"
        } else if budget <= XHIGH {
            "xhigh"
        } else {
            "max"
        }
        .to_string(),
    )
}

/// CC accepts `{type:"auto"|"any"|"tool", name?}` only. Anthropic "any" →
/// "any"; "none" has no CC equivalent, so it is omitted (default auto).
fn resolve_tool_choice(tc: Option<&Value>) -> Option<Value> {
    let tc = tc?;
    match tc.get("type").and_then(Value::as_str) {
        Some("any") => Some(json!({"type": "any"})),
        Some("tool") => {
            let mut m = Map::new();
            m.insert("type".into(), Value::String("tool".into()));
            if let Some(name) = tc.get("name") {
                m.insert("name".into(), name.clone());
            }
            Some(Value::Object(m))
        }
        _ => None,
    }
}
