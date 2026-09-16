//! Request validation, ported from src/translate/validation.ts. Error strings
//! are part of the contract (the golden pins them verbatim), including the two
//! asymmetries: a missing `model` is NOT rejected locally (it is forwarded and
//! CC's 400 is passed through), and `max_tokens` is optional for OpenAI but
//! required for Anthropic.

use serde_json::{Map, Value};

#[derive(Debug)]
pub struct ValidationError(pub String);

type Result<T> = std::result::Result<T, ValidationError>;

fn err<T>(msg: impl Into<String>) -> Result<T> {
    Err(ValidationError(msg.into()))
}

fn as_object(body: &Value) -> Option<&Map<String, Value>> {
    body.as_object()
}

/// `typeof x === "number"` in JS terms: any JSON number.
fn is_number(v: &Value) -> bool {
    v.is_number()
}

// ── OpenAI ────────────────────────────────────────────────

const OPENAI_ROLES: [&str; 5] = ["system", "developer", "user", "assistant", "tool"];

pub fn validate_openai_chat_request(body: &Value) -> Result<()> {
    let Some(req) = as_object(body) else {
        return err("Request body must be a JSON object");
    };

    if let Some(model) = req.get("model") {
        if !model.is_string() {
            return err("Field 'model' must be a string");
        }
    }

    let Some(messages) = req.get("messages").and_then(Value::as_array) else {
        return err("Field 'messages' must be an array");
    };

    for (i, msg) in messages.iter().enumerate() {
        let Some(m) = as_object(msg) else {
            return err(format!("messages[{i}] must be an object"));
        };
        match m.get("role").and_then(Value::as_str) {
            Some(role) if OPENAI_ROLES.contains(&role) => {}
            _ => {
                return err(format!(
                    "messages[{i}].role must be one of: {}",
                    OPENAI_ROLES.join(", ")
                ))
            }
        }
        if !m.contains_key("content") && !m.contains_key("tool_calls") {
            return err(format!(
                "messages[{i}] must contain either 'content' or 'tool_calls'"
            ));
        }
        if m.get("role").and_then(Value::as_str) == Some("tool")
            && !m.get("tool_call_id").is_some_and(Value::is_string)
        {
            return err(format!(
                "messages[{i}].tool_call_id must be a string when role is \"tool\""
            ));
        }
    }

    if let Some(t) = req.get("temperature") {
        if !is_number(t) {
            return err("Field 'temperature' must be a number");
        }
        let n = t.as_f64().unwrap_or(0.0);
        if !(0.0..=2.0).contains(&n) {
            return err("Field 'temperature' must be between 0 and 2");
        }
    }
    if let Some(p) = req.get("top_p") {
        if !is_number(p) {
            return err("Field 'top_p' must be a number");
        }
        let n = p.as_f64().unwrap_or(0.0);
        if !(0.0..=1.0).contains(&n) {
            return err("Field 'top_p' must be between 0 and 1");
        }
    }
    if let Some(mt) = req.get("max_tokens") {
        if !is_number(mt) {
            return err("Field 'max_tokens' must be a number");
        }
        let n = mt.as_f64().unwrap_or(0.0);
        if !n.is_finite() || n <= 0.0 {
            return err("Field 'max_tokens' must be a positive number");
        }
    }
    if let Some(tc) = req.get("tool_choice") {
        if let Some(s) = tc.as_str() {
            if !["auto", "none", "required"].contains(&s) {
                return err("Field 'tool_choice' string must be one of: auto, none, required");
            }
        } else if let Some(obj) = tc.as_object() {
            if obj.get("type").and_then(Value::as_str) != Some("function") {
                return err(
                    "Field 'tool_choice.type' must be \"function\" when tool_choice is an object",
                );
            }
        }
    }

    Ok(())
}

// ── Anthropic ─────────────────────────────────────────────

const UNSUPPORTED_CONTENT_TYPES: [&str; 9] = [
    "document",
    "search_result",
    "web_search_tool_result",
    "web_fetch_tool_result",
    "code_execution_tool_result",
    "mcp_tool_result",
    "container_upload",
    "server_tool_use",
    "mid_conversation_system",
];

const THINKING_TYPES: [&str; 3] = ["enabled", "disabled", "adaptive"];
const EFFORT_LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
const EFFORT_OFF_VALUES: [&str; 4] = ["off", "none", "disabled", "minimal"];
const BUILT_IN_TOOL_TYPES: [&str; 4] = [
    "computer_20241022",
    "bash_20241022",
    "text_editor_20241022",
    "web_search_20250305",
];

/// Whether an effort value means "no extended thinking" rather than a level.
pub fn is_effort_off(value: &Value) -> bool {
    match value.as_str() {
        Some(s) => {
            let t = s.trim().to_ascii_lowercase();
            EFFORT_OFF_VALUES.contains(&t.as_str())
        }
        None => false,
    }
}

pub fn validate_anthropic_request(body: &Value) -> Result<()> {
    let Some(req) = as_object(body) else {
        return err("Request body must be a JSON object");
    };

    if !req.get("model").is_some_and(Value::is_string) {
        return err("Field 'model' must be a string");
    }
    if !req.get("max_tokens").is_some_and(is_number) {
        return err("Field 'max_tokens' must be a number");
    }

    let Some(messages) = req.get("messages").and_then(Value::as_array) else {
        return err("Field 'messages' must be an array");
    };

    for (i, msg) in messages.iter().enumerate() {
        let Some(m) = as_object(msg) else {
            return err(format!(
                "messages[{i}].role must be \"user\" or \"assistant\""
            ));
        };
        match m.get("role").and_then(Value::as_str) {
            Some("user") | Some("assistant") | Some("system") => {}
            _ => {
                return err(format!(
                    "messages[{i}].role must be \"user\" or \"assistant\""
                ))
            }
        }
        let Some(content) = m.get("content") else {
            return err(format!("messages[{i}] must contain 'content'"));
        };
        validate_content_blocks(content, i)?;
    }

    if let Some(tools) = req.get("tools").and_then(Value::as_array) {
        for (i, tool) in tools.iter().enumerate() {
            if let Some(t) = tool.get("type").and_then(Value::as_str) {
                if BUILT_IN_TOOL_TYPES.contains(&t) {
                    return err(format!(
                        "tools[{i}]: built-in tool type \"{t}\" is not supported. Only custom tools ({{name, description, input_schema}}) are allowed."
                    ));
                }
            }
        }
    }

    if let Some(thinking) = req.get("thinking").and_then(Value::as_object) {
        if let Some(t) = thinking.get("type") {
            match t.as_str() {
                Some(s) if THINKING_TYPES.contains(&s) => {}
                _ => {
                    return err(format!(
                        "Field 'thinking.type' must be one of: {}",
                        THINKING_TYPES.join(", ")
                    ))
                }
            }
        }
        if let Some(budget) = thinking.get("budget_tokens").and_then(Value::as_f64) {
            let max_tokens = req.get("max_tokens").and_then(Value::as_f64).unwrap_or(0.0);
            if budget >= max_tokens {
                return err("thinking.budget_tokens must be less than max_tokens");
            }
        }
    }

    if let Some(oc) = req.get("output_config") {
        let Some(oc) = oc.as_object() else {
            return err("Field 'output_config' must be an object");
        };
        if let Some(effort) = oc.get("effort") {
            let lower = effort
                .as_str()
                .map(str::to_ascii_lowercase)
                .unwrap_or_default();
            if !EFFORT_LEVELS.contains(&lower.as_str()) && !is_effort_off(effort) {
                return err(format!(
                    "Field 'output_config.effort' must be one of: {}",
                    EFFORT_LEVELS.join(", ")
                ));
            }
        }
    }

    if req.get("temperature").is_some_and(|v| !is_number(v)) {
        return err("Field 'temperature' must be a number");
    }
    if req.get("top_p").is_some_and(|v| !is_number(v)) {
        return err("Field 'top_p' must be a number");
    }
    if req.get("top_k").is_some_and(|v| !is_number(v)) {
        return err("Field 'top_k' must be a number");
    }

    Ok(())
}

fn validate_content_blocks(content: &Value, msg_idx: usize) -> Result<()> {
    if content.is_string() {
        return Ok(());
    }
    let Some(blocks) = content.as_array() else {
        return err(format!(
            "messages[{msg_idx}].content must be a string or array of blocks"
        ));
    };
    for (i, block) in blocks.iter().enumerate() {
        let Some(t) = block.get("type").and_then(Value::as_str) else {
            continue;
        };
        if UNSUPPORTED_CONTENT_TYPES.contains(&t) {
            return err(format!(
                "messages[{msg_idx}].content[{i}]: block type \"{t}\" is not supported"
            ));
        }
    }
    Ok(())
}

/// Validate only the fields consumed by the local estimator.
pub fn validate_count_tokens_request(body: &Value) -> Result<()> {
    let Some(req) = body.as_object() else {
        return err("Request body must be a JSON object");
    };
    if let Some(system) = req.get("system") {
        if !system.is_string() {
            let ok = system.as_array().is_some_and(|arr| {
                arr.iter().all(|b| {
                    b.as_object()
                        .is_some_and(|o| o.get("text").is_none_or(Value::is_string))
                })
            });
            if !ok {
                return err("Field 'system' must be a string or array of text blocks");
            }
        }
    }
    if let Some(messages) = req.get("messages") {
        let ok = messages.as_array().is_some_and(|arr| {
            arr.iter().all(|m| {
                m.as_object().is_some_and(|o| match o.get("content") {
                    Some(c) if c.is_string() => true,
                    Some(c) => c.as_array().is_some_and(|a| a.iter().all(Value::is_object)),
                    None => false,
                })
            })
        });
        if !ok {
            return err("Field 'messages' must contain objects with string or block-array content");
        }
    }
    if let Some(tools) = req.get("tools") {
        let ok = tools.as_array().is_some_and(|arr| {
            arr.iter().all(|t| {
                t.as_object().is_some_and(|o| {
                    o.get("name").is_none_or(Value::is_string)
                        && o.get("description").is_none_or(Value::is_string)
                        && o.get("input_schema").is_none_or(Value::is_object)
                })
            })
        });
        if !ok {
            return err("Field 'tools' must be an array of tool objects");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn openai_missing_model_is_not_rejected_locally() {
        // Contract: this is forwarded upstream and CC's 400 is passed through.
        let body = json!({"messages": [{"role": "user", "content": "x"}]});
        assert!(validate_openai_chat_request(&body).is_ok());
    }

    #[test]
    fn openai_messages_must_be_an_array() {
        let body = json!({"model": "m", "max_tokens": 8});
        let e = validate_openai_chat_request(&body).unwrap_err();
        assert_eq!(e.0, "Field 'messages' must be an array");
    }

    #[test]
    fn openai_role_message_matches_golden() {
        let body = json!({"model":"m","max_tokens":8,"messages":[{"role":"wizard","content":"x"}]});
        let e = validate_openai_chat_request(&body).unwrap_err();
        assert_eq!(
            e.0,
            "messages[0].role must be one of: system, developer, user, assistant, tool"
        );
    }

    #[test]
    fn openai_temperature_and_tool_choice_messages() {
        let mut base = json!({"model":"m","messages":[{"role":"user","content":"x"}]});
        base["temperature"] = json!("hot");
        assert_eq!(
            validate_openai_chat_request(&base).unwrap_err().0,
            "Field 'temperature' must be a number"
        );

        let mut base = json!({"model":"m","messages":[{"role":"user","content":"x"}]});
        base["temperature"] = json!(9);
        assert_eq!(
            validate_openai_chat_request(&base).unwrap_err().0,
            "Field 'temperature' must be between 0 and 2"
        );

        let mut base = json!({"model":"m","messages":[{"role":"user","content":"x"}]});
        base["tool_choice"] = json!("whatever");
        assert_eq!(
            validate_openai_chat_request(&base).unwrap_err().0,
            "Field 'tool_choice' string must be one of: auto, none, required"
        );
    }

    #[test]
    fn anthropic_requires_model_and_max_tokens() {
        let body = json!({"messages": []});
        assert_eq!(
            validate_anthropic_request(&body).unwrap_err().0,
            "Field 'model' must be a string"
        );
        let body = json!({"model": "m", "messages": []});
        assert_eq!(
            validate_anthropic_request(&body).unwrap_err().0,
            "Field 'max_tokens' must be a number"
        );
    }

    #[test]
    fn anthropic_budget_must_be_below_max_tokens() {
        let body = json!({
            "model": "m", "max_tokens": 100, "messages": [],
            "thinking": {"type": "enabled", "budget_tokens": 100}
        });
        assert_eq!(
            validate_anthropic_request(&body).unwrap_err().0,
            "thinking.budget_tokens must be less than max_tokens"
        );
    }

    #[test]
    fn effort_off_markers_are_accepted() {
        for v in ["off", "OFF", "none", "disabled", "minimal"] {
            assert!(is_effort_off(&json!(v)), "{v} should be an off marker");
        }
        assert!(!is_effort_off(&json!("high")));
    }

    #[test]
    fn unsupported_content_block_is_rejected() {
        let body = json!({
            "model": "m", "max_tokens": 8,
            "messages": [{"role": "user", "content": [{"type": "document"}]}]
        });
        assert_eq!(
            validate_anthropic_request(&body).unwrap_err().0,
            "messages[0].content[0]: block type \"document\" is not supported"
        );
    }
}
