//! Tool-schema export and tool-call parsing (native OpenAI + ReAct fallback).

use serde_json::{Value, json};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::tools::{ToolCall, ToolSpec};

/// Convert tool specs into the OpenAI `tools` array.
pub fn to_openai_tools(specs: &[ToolSpec]) -> Value {
    Value::Array(
        specs
            .iter()
            .map(|s| {
                json!({
                    "type": "function",
                    "function": {
                        "name": s.name,
                        "description": s.description,
                        "parameters": s.parameters,
                    }
                })
            })
            .collect(),
    )
}

/// Parse native OpenAI `tool_calls` from a response `message` object.
pub fn parse_native_tool_calls(message: &Value) -> Option<Vec<ToolCall>> {
    let array = message.get("tool_calls")?.as_array()?;
    let mut calls = Vec::new();
    for tc in array {
        let function = tc.get("function")?;
        let name = function.get("name").and_then(|v| v.as_str())?.to_string();
        let id = tc
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(local_id);
        let args = match function.get("arguments") {
            // OpenAI sends arguments as a JSON-encoded string.
            Some(Value::String(s)) => serde_json::from_str(s).unwrap_or_else(|_| json!({})),
            Some(other) => other.clone(),
            None => json!({}),
        };
        calls.push(ToolCall { id, name, args });
    }
    (!calls.is_empty()).then_some(calls)
}

/// ReAct fallback: parse a single fenced ```action { "tool": ..., "args": ... } block.
pub fn parse_react_action(content: &str) -> Option<ToolCall> {
    let start = content.find("```action")?;
    let after = &content[start + "```action".len()..];
    let end = after.find("```")?;
    let json_str = after[..end].trim();
    let value: Value = serde_json::from_str(json_str).ok()?;
    let name = value.get("tool")?.as_str()?.to_string();
    let args = value.get("args").cloned().unwrap_or_else(|| json!({}));
    Some(ToolCall {
        id: local_id(),
        name,
        args,
    })
}

fn local_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("call-{nanos}")
}
