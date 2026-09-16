//! OpenAI chat completions -> CC `/alpha/generate` body, ported from
//! src/translate/openai.ts. Only the request half lives here; the streaming
//! encoder is M3.

use crate::models::Catalog;
use crate::translate::models::{resolve_effort_for_model, resolve_model, ModelTables};
use crate::translate::util::{
    apply_no_tools_safeguard, build_cc_config, prune_dangling_tools, truthy,
};
use serde_json::{json, Map, Value};
use std::collections::HashMap;

/// Build the CC body for an OpenAI chat request. Optional fields that the
/// client did not send are omitted rather than nulled, exactly as
/// `JSON.stringify` drops `undefined`.
///
/// `guard_off` carries the `CC_NO_TOOLS_GUARD=off` opt-out so the environment
/// is read by the caller (see `Env::guard_off`), keeping this a pure function.
pub fn to_cc_request(
    req: &Value,
    catalog: &Catalog,
    tables: &ModelTables,
    guard_off: bool,
) -> Value {
    let messages: Vec<Value> = req
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let (mut cc_messages, system_prompt) = to_cc_messages(&messages);
    prune_dangling_tools(&mut cc_messages);

    let requested_model = req.get("model").and_then(Value::as_str).unwrap_or("");
    let resolved_model = resolve_model(requested_model, catalog, tables);

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
    for key in ["max_tokens", "temperature", "top_p", "stop"] {
        if let Some(v) = req.get(key) {
            params.insert(key.into(), v.clone());
        }
    }
    if let Some(effort) = resolve_effort_for_model(
        &resolved_model,
        req.get("reasoning_effort").and_then(Value::as_str),
        tables,
    ) {
        params.insert("reasoning_effort".into(), Value::String(effort));
    }
    if let Some(tools) = to_cc_tools(req.get("tools")) {
        params.insert("tools".into(), tools);
    }
    if let Some(tc) = resolve_tool_choice(req.get("tool_choice")) {
        params.insert("tool_choice".into(), tc);
    }
    // Set before the safeguard so the instruction appends to the client's own.
    if let Some(sp) = system_prompt {
        if !sp.is_empty() {
            params.insert("system".into(), Value::String(sp));
        }
    }

    let has_tools = req
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|t| !t.is_empty());
    apply_no_tools_safeguard(&mut params, has_tools, guard_off);

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

/// Returns the CC messages plus the joined system prompt (`None` when the
/// client sent no system/developer message at all — an empty join counts as
/// absent, which is why the caller re-checks emptiness).
fn to_cc_messages(messages: &[Value]) -> (Vec<Value>, Option<String>) {
    // tool_call_id -> toolName: CC's tool-result parts must carry a toolName.
    let mut tool_name_by_id: HashMap<String, String> = HashMap::new();
    for msg in messages {
        if let Some(calls) = msg.get("tool_calls").and_then(Value::as_array) {
            for tc in calls {
                if let Some(id) = tc.get("id").and_then(Value::as_str) {
                    if let Some(name) = tc.pointer("/function/name").and_then(Value::as_str) {
                        tool_name_by_id.insert(id.to_string(), name.to_string());
                    }
                }
            }
        }
    }

    let mut system_parts: Vec<String> = Vec::new();
    let mut cc: Vec<Value> = Vec::new();

    for msg in messages {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
        let content = msg.get("content");

        match role {
            "system" | "developer" => {
                let text = match content {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Array(parts)) => parts
                        .iter()
                        .map(|p| {
                            if p.get("type").and_then(Value::as_str) == Some("text") {
                                p.get("text")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string()
                            } else {
                                String::new()
                            }
                        })
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => String::new(),
                };
                system_parts.push(text);
            }

            "tool" => {
                let tool_call_id = match msg.get("tool_call_id") {
                    None | Some(Value::Null) => Value::String(String::new()),
                    Some(v) => v.clone(),
                };
                let tool_name = match tool_call_id.as_str() {
                    Some(id) if !id.is_empty() => tool_name_by_id
                        .get(id)
                        .map(String::as_str)
                        .unwrap_or("")
                        .to_string(),
                    _ => String::new(),
                };

                let mut output = Map::new();
                output.insert("type".into(), Value::String("text".into()));
                match content {
                    Some(Value::String(s)) => {
                        output.insert("value".into(), Value::String(s.clone()));
                    }
                    // Non-string content is JSON-stringified, not flattened.
                    Some(v) => {
                        output.insert(
                            "value".into(),
                            Value::String(serde_json::to_string(v).unwrap_or_default()),
                        );
                    }
                    // `JSON.stringify(undefined)` drops the key entirely.
                    None => {}
                }

                cc.push(json!({
                    "role": "tool",
                    "content": [{
                        "type": "tool-result",
                        "toolCallId": tool_call_id,
                        "toolName": tool_name,
                        "output": Value::Object(output),
                    }],
                }));
            }

            // Note the array-content case: an assistant message with tool_calls
            // becomes parts, while one without falls through to the plain-text
            // branch below and is dropped when its content is not a string.
            "assistant" if msg.get("tool_calls").is_some_and(Value::is_array) => {
                let mut parts: Vec<Value> = Vec::new();
                if let Some(text) = content.and_then(Value::as_str) {
                    if !text.is_empty() {
                        parts.push(json!({"type": "text", "text": text}));
                    }
                }
                if let Some(calls) = msg.get("tool_calls").and_then(Value::as_array) {
                    for tc in calls {
                        let mut part = Map::new();
                        part.insert("type".into(), Value::String("tool-call".into()));
                        if let Some(id) = tc.get("id") {
                            part.insert("toolCallId".into(), id.clone());
                        }
                        if let Some(name) = tc.pointer("/function/name") {
                            part.insert("toolName".into(), name.clone());
                        }
                        let args = tc
                            .pointer("/function/arguments")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let input = if args.is_empty() {
                            json!({})
                        } else {
                            serde_json::from_str::<Value>(args)
                                .unwrap_or_else(|_| Value::String(args.to_string()))
                        };
                        part.insert("input".into(), input);
                        parts.push(Value::Object(part));
                    }
                }
                cc.push(json!({"role": "assistant", "content": parts}));
            }

            "user" => match content {
                Some(Value::String(s)) => cc.push(json!({"role": "user", "content": s})),
                Some(Value::Array(parts)) => {
                    let mapped: Vec<Value> = parts
                        .iter()
                        .map(|p| {
                            let is_image = p.get("type").and_then(Value::as_str)
                                == Some("image_url")
                                && p.get("image_url").is_some_and(truthy);
                            if is_image {
                                let mut m = Map::new();
                                m.insert("type".into(), Value::String("image".into()));
                                if let Some(url) = p.pointer("/image_url/url").filter(|u| truthy(u))
                                {
                                    m.insert("image".into(), url.clone());
                                }
                                Value::Object(m)
                            } else {
                                json!({
                                    "type": "text",
                                    "text": p.get("text").and_then(Value::as_str).unwrap_or(""),
                                })
                            }
                        })
                        .collect();
                    cc.push(json!({"role": "user", "content": mapped}));
                }
                _ => {}
            },

            "assistant" => {
                if let Some(Value::String(s)) = content {
                    cc.push(json!({"role": "assistant", "content": s}));
                }
            }

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

/// OpenAI tools -> CC tools. An empty list is omitted (unlike the Anthropic
/// path, which forwards it).
fn to_cc_tools(tools: Option<&Value>) -> Option<Value> {
    let arr = tools?.as_array()?;
    if arr.is_empty() {
        return None;
    }
    let out: Vec<Value> = arr
        .iter()
        .map(|t| {
            let mut m = Map::new();
            for (from, to) in [
                ("/function/name", "name"),
                ("/function/description", "description"),
                ("/function/parameters", "input_schema"),
            ] {
                if let Some(v) = t.pointer(from) {
                    m.insert(to.into(), v.clone());
                }
            }
            Value::Object(m)
        })
        .collect();
    Some(Value::Array(out))
}

/// CC validates `tool_choice` as an object and rejects bare strings. There is
/// no "required" (use "any") and no "none" — "none" has no CC equivalent, so it
/// is omitted and the model decides.
fn resolve_tool_choice(tc: Option<&Value>) -> Option<Value> {
    let tc = tc?;
    if let Some(s) = tc.as_str() {
        return match s {
            "required" => Some(json!({"type": "any"})),
            _ => None,
        };
    }
    if tc.get("type").and_then(Value::as_str) == Some("function") {
        let mut m = Map::new();
        m.insert("type".into(), Value::String("tool".into()));
        if let Some(name) = tc.pointer("/function/name") {
            m.insert("name".into(), name.clone());
        }
        return Some(Value::Object(m));
    }
    None
}
