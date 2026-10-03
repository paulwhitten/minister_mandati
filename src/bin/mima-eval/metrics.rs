//! Per-trial metrics, read from mima's JSONL transcript (docs/sessions.md):
//! the transcript is the single record of what the agent did.

use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Default, Clone, Serialize)]
pub struct Metrics {
    /// How the turn ended: done, loop_guard, step_cap, error,
    /// context_exceeded; set by the runner to agent_timeout, crash or infra
    /// when the transcript has no ending.
    pub exit_reason: String,
    pub final_answer: String,
    pub steps: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Largest prompt sent (tokens), i.e. peak context fill.
    pub peak_context: u64,
    pub window: u64,
    pub tool_calls: BTreeMap<String, u64>,
    /// Failed tool results by cause (edit_not_found, bash_nonzero, ...).
    pub tool_errors: BTreeMap<String, u64>,
    /// edit_file successes that needed a tolerance (not an exact match).
    pub edit_tolerances: u64,
    pub compactions: u64,
    pub loop_guard_trips: u64,
    pub model_errors: u64,
    /// Replies cut off at `max_tokens` (finish_reason "length"): often a
    /// harness setting, not a model failure.
    pub truncated: u64,
    /// The last model error's text, if any (to tell an unreachable server
    /// from a model failure).
    pub last_model_error: String,
}

/// Parses a transcript file. Missing or unreadable files give default
/// metrics with an empty exit reason.
pub fn from_transcript(path: &Path) -> Metrics {
    let mut m = Metrics::default();
    let Ok(text) = std::fs::read_to_string(path) else {
        return m;
    };
    let mut call_names: BTreeMap<String, String> = BTreeMap::new();
    for line in text.lines() {
        let Ok(r) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let u = |k: &str| r.get(k).and_then(Value::as_u64).unwrap_or(0);
        match r["type"].as_str().unwrap_or_default() {
            "session_start" => m.window = u("window"),
            "model_response" => {
                m.steps += 1;
                if r["finish_reason"].as_str() == Some("length") {
                    m.truncated += 1;
                }
                m.prompt_tokens += r["usage"]["prompt"].as_u64().unwrap_or(0);
                m.completion_tokens += r["usage"]["completion"].as_u64().unwrap_or(0);
                let sent = r["usage"]["prompt"]
                    .as_u64()
                    .unwrap_or(u("counted_prompt_tokens"));
                m.peak_context = m.peak_context.max(sent);
                for c in r["tool_calls"].as_array().into_iter().flatten() {
                    let name = c["name"].as_str().unwrap_or("?").to_string();
                    *m.tool_calls.entry(name.clone()).or_default() += 1;
                    if let Some(id) = c["id"].as_str() {
                        call_names.insert(id.to_string(), name);
                    }
                }
            }
            "model_error" => {
                m.model_errors += 1;
                m.last_model_error = r["error"].as_str().unwrap_or_default().to_string();
            }
            "tool_result" => {
                let tool = r["tool"].as_str().unwrap_or("?");
                let out = r["output"].as_str().unwrap_or_default();
                if r["failed"].as_bool() == Some(true) {
                    *m.tool_errors.entry(error_cause(tool, out)).or_default() += 1;
                } else if tool == "edit_file" && out.contains(" Note: ") {
                    m.edit_tolerances += 1;
                }
            }
            "compaction" => {
                if r["stage"].as_str() != Some("normalize") {
                    m.compactions += 1;
                }
            }
            "loop_guard" => {
                if r["action"].as_str() == Some("terminate") {
                    m.loop_guard_trips += 1;
                }
            }
            "turn_end" => {
                m.final_answer = r["answer"].as_str().unwrap_or_default().to_string();
                let err = r["error"].as_str().unwrap_or_default();
                m.exit_reason = match r["outcome"].as_str().unwrap_or_default() {
                    "answered" => "done".into(),
                    "error" if err.contains("context window") => "context_exceeded".into(),
                    "error"
                        if err.contains("model endpoint") || err.contains("request to model") =>
                    {
                        "infra".into()
                    }
                    other => other.to_string(),
                };
            }
            _ => {}
        }
    }
    m
}

/// Classifies a failed tool result for error counts.
fn error_cause(tool: &str, out: &str) -> String {
    let cause = match tool {
        "edit_file" if out.contains("was not found") => "edit_not_found",
        "edit_file" if out.contains(" times in ") => "edit_not_unique",
        "edit_file" if out.contains("changed on disk") => "edit_stale",
        "edit_file" if out.contains("before changing") => "edit_unread",
        "edit_file" if out.contains("missing or null") || out.contains("must be a string") => {
            "edit_bad_args"
        }
        "execute_bash" if out.starts_with("exit: ") => "bash_nonzero",
        _ if out.contains("user denied") => "denied",
        _ if out.contains("unknown tool") => "unknown_tool",
        _ if out.contains("requires argument") => "bad_args",
        _ => return format!("{tool}_error"),
    };
    cause.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_transcript() {
        let d = std::env::temp_dir().join(format!("mima-eval-metrics-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("t.jsonl");
        let lines = [
            r#"{"type":"session_start","window":8192}"#,
            r#"{"type":"model_response","usage":{"prompt":800,"completion":40},"tool_calls":[{"id":"c1","name":"read_file","args":{}}]}"#,
            r#"{"type":"tool_result","tool":"read_file","failed":false,"output":"[f lines 1-3 of 3]"}"#,
            r#"{"type":"model_response","usage":{"prompt":1200,"completion":60},"tool_calls":[{"id":"c2","name":"edit_file","args":{}}]}"#,
            r#"{"type":"tool_result","tool":"edit_file","failed":true,"output":"Error: edit_file failed: old_string was not found in f."}"#,
            r#"{"type":"model_response","usage":{"prompt":1300,"completion":30},"tool_calls":[{"id":"c3","name":"edit_file","args":{}}]}"#,
            r#"{"type":"tool_result","tool":"edit_file","failed":false,"output":"Edited f: replaced lines 1-1 with 1 lines. Note: ignored trailing whitespace."}"#,
            r#"{"type":"compaction","stage":"mask"}"#,
            r#"{"type":"model_response","usage":{"prompt":900,"completion":10},"tool_calls":[]}"#,
            r#"{"type":"turn_end","outcome":"answered","answer":"Fixed.","error":null}"#,
        ];
        std::fs::write(&p, lines.join("\n")).unwrap();
        let m = from_transcript(&p);
        assert_eq!(m.exit_reason, "done");
        assert_eq!(m.final_answer, "Fixed.");
        assert_eq!(
            (m.steps, m.prompt_tokens, m.completion_tokens),
            (4, 4200, 140)
        );
        assert_eq!((m.peak_context, m.window), (1300, 8192));
        assert_eq!(m.tool_calls["edit_file"], 2);
        assert_eq!(m.tool_errors["edit_not_found"], 1);
        assert_eq!((m.edit_tolerances, m.compactions), (1, 1));
    }

    #[test]
    fn missing_transcript_has_no_exit_reason() {
        assert_eq!(
            from_transcript(Path::new("/nonexistent/t.jsonl")).exit_reason,
            ""
        );
    }
}
