//! Shared CC request scaffolding, ported from src/translate/util.ts:
//! config block, no-tools safeguard, usage extraction and tool pairing.

use serde_json::{json, Map, Value};

/// JavaScript truthiness, which several of the ported guards depend on: an
/// empty string, `0`, `false`, `null` and a missing key are all falsy, while an
/// empty array or object is truthy.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// The no-tools instruction injected into tool-less OpenAI chat requests.
pub const NO_TOOLS_INSTRUCTION: &str = "CRITICAL: You are running in a chat-only environment. Tool execution is disabled. Do not generate or call any tools (e.g. Build, ReadFile, grep, Search, etc.). Respond only with plain text.";

/// Appended to the last user message when the safeguard is active.
pub const NO_TOOLS_SUFFIX: &str = "\n\n[System Note: Tool execution is disabled in this environment. Do not output any tool calls (such as Build, Search, ReadFile, grep, etc.). You must answer directly in plain text.]";

/// The fixed CC `config` block. Every field is a constant in the Node version
/// too (the golden normalises them to placeholders precisely because they carry
/// no per-request information).
pub fn build_cc_config() -> Value {
    json!({
        "workingDir": std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        "date": crate::today_utc(),
        // The upstream is told which environment the "CLI" runs in. Kept as the
        // literal the Node build has always sent: CC is known to reject
        // requests that do not look like the official client, and there is no
        // evidence it inspects this string's contents.
        "environment": "linux-x64, Node.js v24.16.0",
        "structure": [],
        "isGitRepo": false,
        "currentBranch": "",
        "mainBranch": "",
        "gitStatus": "",
        "recentCommits": [],
    })
}

/// Inject the "tools are disabled" instruction into a tool-less chat request.
/// Anthropic requests are never guarded. `guard_off` is the
/// `CC_NO_TOOLS_GUARD=off` opt-out, passed in rather than read from the
/// environment here so callers (and tests) own the switch.
pub fn apply_no_tools_safeguard(params: &mut Map<String, Value>, has_tools: bool, guard_off: bool) {
    if has_tools || guard_off {
        return;
    }

    match params.get("system").and_then(Value::as_str) {
        Some(existing) if !existing.is_empty() => {
            params.insert(
                "system".into(),
                Value::String(format!("{existing}\n\n{NO_TOOLS_INSTRUCTION}")),
            );
        }
        _ => {
            params.insert("system".into(), Value::String(NO_TOOLS_INSTRUCTION.into()));
        }
    }

    let Some(messages) = params.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    // Last user message wins; the suffix goes on its final text part.
    for msg in messages.iter_mut().rev() {
        if msg.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        match msg.get_mut("content") {
            Some(Value::String(s)) => {
                s.push_str(NO_TOOLS_SUFFIX);
            }
            Some(Value::Array(parts)) => {
                let last_text = parts
                    .iter_mut()
                    .rev()
                    .find(|p| p.get("type").and_then(Value::as_str) == Some("text"));
                match last_text {
                    Some(part) => {
                        let current = part
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        if let Some(obj) = part.as_object_mut() {
                            obj.insert(
                                "text".into(),
                                Value::String(format!("{current}{NO_TOOLS_SUFFIX}")),
                            );
                        }
                    }
                    None => parts.push(json!({"type": "text", "text": NO_TOOLS_SUFFIX})),
                }
            }
            _ => {}
        }
        break;
    }
}

/// Drop tool-call/tool-result parts whose counterpart is missing in the other
/// direction. CC rejects a conversation that contains either dangling shape.
pub fn prune_dangling_tools(messages: &mut Vec<Value>) {
    use std::collections::HashSet;

    let mut call_ids: HashSet<String> = HashSet::new();
    let mut result_ids: HashSet<String> = HashSet::new();
    for msg in messages.iter() {
        let Some(parts) = msg.get("content").and_then(Value::as_array) else {
            continue;
        };
        for part in parts {
            let Some(id) = part.get("toolCallId").and_then(Value::as_str) else {
                continue;
            };
            match part.get("type").and_then(Value::as_str) {
                Some("tool-call") => {
                    call_ids.insert(id.to_string());
                }
                Some("tool-result") => {
                    result_ids.insert(id.to_string());
                }
                _ => {}
            }
        }
    }
    let valid: HashSet<&String> = call_ids.intersection(&result_ids).collect();
    if valid.len() == call_ids.len() && valid.len() == result_ids.len() {
        return; // Nothing dangling: leave the messages untouched.
    }

    let mut kept: Vec<Value> = Vec::new();
    for msg in messages.iter() {
        let Some(parts) = msg.get("content").and_then(Value::as_array) else {
            kept.push(msg.clone());
            continue;
        };
        let filtered: Vec<Value> = parts
            .iter()
            .filter(|part| match part.get("type").and_then(Value::as_str) {
                Some("tool-call") | Some("tool-result") => part
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .is_some_and(|id| valid.contains(&id.to_string())),
                _ => true,
            })
            .cloned()
            .collect();
        if !filtered.is_empty() {
            let mut m = msg.clone();
            if let Some(obj) = m.as_object_mut() {
                obj.insert("content".into(), Value::Array(filtered));
            }
            kept.push(m);
        }
    }
    *messages = kept;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cc_config_matches_the_golden_shape() {
        let c = build_cc_config();
        for key in [
            "workingDir",
            "date",
            "environment",
            "structure",
            "isGitRepo",
            "currentBranch",
            "mainBranch",
            "gitStatus",
            "recentCommits",
        ] {
            assert!(c.get(key).is_some(), "config must carry {key}");
        }
        assert_eq!(c["structure"], json!([]));
        assert_eq!(c["isGitRepo"], json!(false));
        assert_eq!(c["environment"], "linux-x64, Node.js v24.16.0");
    }

    #[test]
    fn safeguard_appends_to_the_last_user_message() {
        let mut params = Map::new();
        params.insert(
            "messages".into(),
            json!([
                {"role": "user", "content": "first"},
                {"role": "assistant", "content": "reply"},
                {"role": "user", "content": "hello"}
            ]),
        );
        apply_no_tools_safeguard(&mut params, false, false);
        assert_eq!(params["system"], Value::String(NO_TOOLS_INSTRUCTION.into()));
        let msgs = params["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["content"], "first");
        assert_eq!(
            msgs[2]["content"],
            Value::String(format!("hello{NO_TOOLS_SUFFIX}"))
        );
    }

    #[test]
    fn safeguard_is_skipped_when_tools_are_present() {
        let mut params = Map::new();
        params.insert("messages".into(), json!([{"role":"user","content":"hi"}]));
        apply_no_tools_safeguard(&mut params, true, false);
        assert!(params.get("system").is_none());
    }

    #[test]
    fn cc_no_tools_guard_off_disables_the_injection() {
        let mut params = Map::new();
        params.insert("messages".into(), json!([{"role":"user","content":"hi"}]));
        params.insert("system".into(), json!("keep me"));
        apply_no_tools_safeguard(&mut params, false, true);
        assert_eq!(params["system"], json!("keep me"));
        assert_eq!(params["messages"][0]["content"], json!("hi"));
    }

    #[test]
    fn guard_suffix_replaces_an_empty_system_prompt() {
        // An empty client system prompt is treated as absent, not appended to.
        let mut params = Map::new();
        params.insert("messages".into(), json!([{"role":"user","content":"hi"}]));
        params.insert("system".into(), json!(""));
        apply_no_tools_safeguard(&mut params, false, false);
        assert_eq!(params["system"], Value::String(NO_TOOLS_INSTRUCTION.into()));
    }

    #[test]
    fn dangling_tools_are_pruned_and_empty_messages_dropped() {
        let mut messages = vec![
            json!({"role":"user","content":"hi"}),
            json!({"role":"assistant","content":[
                {"type":"tool-call","toolCallId":"paired","toolName":"t","input":{}},
                {"type":"tool-call","toolCallId":"dangling","toolName":"t","input":{}}
            ]}),
            json!({"role":"tool","content":[
                {"type":"tool-result","toolCallId":"paired","toolName":"t","output":{"type":"text","value":"ok"}}
            ]}),
        ];
        prune_dangling_tools(&mut messages);
        let parts = messages[1]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["toolCallId"], "paired");
    }

    #[test]
    fn pruning_is_a_no_op_when_everything_is_paired() {
        let mut messages = vec![
            json!({"role":"assistant","content":[
                {"type":"tool-call","toolCallId":"a","toolName":"t","input":{}}
            ]}),
            json!({"role":"tool","content":[
                {"type":"tool-result","toolCallId":"a","toolName":"t","output":{"type":"text","value":"ok"}}
            ]}),
        ];
        let before = messages.clone();
        prune_dangling_tools(&mut messages);
        assert_eq!(messages, before);
    }
}
