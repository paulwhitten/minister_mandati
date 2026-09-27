//! OpenAI-compatible chat client (works against vLLM, Ollama, or cloud).

use std::time::Duration;

use serde_json::{Value, json};
use snafu::prelude::*;

use crate::config::Config;
use crate::context::AgentContext;
use crate::schema;
use crate::tools::ToolCall;

/// Transient-failure retry budget for establishing a completion request.
const MAX_RETRIES: u32 = 3;
/// Base delay for exponential backoff between retries.
const BASE_BACKOFF_MS: u64 = 400;

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("request to model endpoint failed"))]
    Http { source: reqwest::Error },
    #[snafu(display("model endpoint returned HTTP {status}: {body}"))]
    Status { status: u16, body: String },
    #[snafu(display("failed to decode model response"))]
    Decode { source: reqwest::Error },
    #[snafu(display("unexpected response shape: {reason}"))]
    Shape { reason: String },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Token accounting from a single model response.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

impl TokenUsage {
    /// Reads the OpenAI-compatible `usage` object, if the server reported one.
    fn from_payload(payload: &Value) -> Option<Self> {
        let usage = payload.get("usage")?;
        let prompt = usage
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let completion = usage
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let total = usage
            .get("total_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(prompt + completion);
        Some(Self {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: total,
        })
    }
}

pub struct CompletionResponse {
    pub content: Option<String>,
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Raw OpenAI `tool_calls` JSON, echoed back into history on the next turn.
    pub raw_tool_calls: Option<Value>,
    /// Server-reported token usage for this request, when available.
    pub usage: Option<TokenUsage>,
}

#[tracing::instrument(skip(ctx, on_delta), fields(model = %ctx.config.provider.default_model))]
pub async fn generate_completion(
    ctx: &AgentContext,
    on_delta: &mut dyn FnMut(&str),
) -> Result<CompletionResponse> {
    let cfg = &ctx.config;
    let url = format!(
        "{}/chat/completions",
        cfg.provider.base_url.trim_end_matches('/')
    );

    let mut body = json!({
        "model": cfg.provider.default_model,
        "messages": ctx.messages(),
        "temperature": cfg.agent.temperature,
        "max_tokens": cfg.agent.max_tokens,
        "stream": cfg.agent.stream,
    });
    if cfg.agent.stream {
        // Ask the server to emit a final usage chunk so token tracking survives streaming.
        body["stream_options"] = json!({ "include_usage": true });
    }

    let use_native = matches!(cfg.agent.tool_calling.as_str(), "native" | "auto");
    if use_native {
        body["tools"] = schema::to_openai_tools(&ctx.tool_specs);
    }

    tracing::debug!(%url, stream = cfg.agent.stream, "sending completion request");
    let client = reqwest::Client::new();
    let response = send_with_retry(&client, &url, cfg, &body).await?;

    if cfg.agent.stream {
        let (message, usage) = read_stream(response, on_delta).await?;
        let result = build_response(&message, usage, cfg);
        tracing::debug!(
            tool_calls = result.tool_calls.as_ref().map(|c| c.len()).unwrap_or(0),
            "stream complete"
        );
        Ok(result)
    } else {
        parse_json_response(response, cfg).await
    }
}

/// Sends the request, retrying transient transport errors and retryable status
/// codes (429, 5xx) with exponential backoff up to `MAX_RETRIES`.
async fn send_with_retry(
    client: &reqwest::Client,
    url: &str,
    cfg: &Config,
    body: &Value,
) -> Result<reqwest::Response> {
    let mut attempt = 0u32;
    loop {
        let result = client
            .post(url)
            .bearer_auth(&cfg.provider.api_key)
            .json(body)
            .send()
            .await;
        match result {
            Ok(resp) if resp.status().is_success() => return Ok(resp),
            Ok(resp) => {
                let status = resp.status().as_u16();
                let retryable = status == 429 || status >= 500;
                if retryable && attempt < MAX_RETRIES {
                    tracing::warn!(status, attempt, "retryable status; backing off");
                    backoff(attempt).await;
                    attempt += 1;
                    continue;
                }
                let body = resp.text().await.unwrap_or_default();
                return StatusSnafu { status, body }.fail();
            }
            Err(e) => {
                if attempt < MAX_RETRIES {
                    tracing::warn!(error = %e, attempt, "transport error; backing off");
                    backoff(attempt).await;
                    attempt += 1;
                    continue;
                }
                return Err(e).context(HttpSnafu);
            }
        }
    }
}

async fn backoff(attempt: u32) {
    let ms = BASE_BACKOFF_MS.saturating_mul(2u64.saturating_pow(attempt));
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

/// Non-streaming path: decode the full JSON body into a response.
async fn parse_json_response(
    response: reqwest::Response,
    cfg: &Config,
) -> Result<CompletionResponse> {
    let payload: Value = response.json().await.context(DecodeSnafu)?;
    let usage = TokenUsage::from_payload(&payload);
    let message = payload
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .context(ShapeSnafu {
            reason: "missing choices[0].message".to_string(),
        })?;
    Ok(build_response(message, usage, cfg))
}

/// Partial tool call accumulated across streamed deltas, keyed by array index.
#[derive(Default, Clone)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// Streaming path: consume SSE `data:` events, emitting content deltas through
/// `on_delta` and reassembling a message value shaped like the non-streaming
/// `choices[0].message`, plus the final usage chunk (`include_usage`).
async fn read_stream(
    mut response: reqwest::Response,
    on_delta: &mut dyn FnMut(&str),
) -> Result<(Value, Option<TokenUsage>)> {
    let mut content = String::new();
    let mut partials: Vec<PartialToolCall> = Vec::new();
    let mut usage = None;
    let mut buf: Vec<u8> = Vec::new();

    while let Some(chunk) = response.chunk().await.context(HttpSnafu)? {
        buf.extend_from_slice(&chunk);
        while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&raw);
            let line = line.trim();
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data == "[DONE]" {
                continue;
            }
            let Ok(json) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            if let Some(u) = TokenUsage::from_payload(&json) {
                usage = Some(u);
            }
            let Some(delta) = json.pointer("/choices/0/delta") else {
                continue;
            };
            if let Some(c) = delta.get("content").and_then(Value::as_str)
                && !c.is_empty()
            {
                content.push_str(c);
                on_delta(c);
            }
            if let Some(tcs) = delta.get("tool_calls").and_then(Value::as_array) {
                accumulate_tool_calls(tcs, &mut partials);
            }
        }
    }

    let mut message = json!({});
    if !content.is_empty() {
        message["content"] = Value::String(content);
    }
    if !partials.is_empty() {
        let arr: Vec<Value> = partials
            .into_iter()
            .map(|p| {
                json!({
                    "id": p.id,
                    "type": "function",
                    "function": { "name": p.name, "arguments": p.arguments }
                })
            })
            .collect();
        message["tool_calls"] = Value::Array(arr);
    }
    Ok((message, usage))
}

/// Folds one delta's `tool_calls` array into the per-index accumulators.
fn accumulate_tool_calls(deltas: &[Value], partials: &mut Vec<PartialToolCall>) {
    for tc in deltas {
        let idx = tc.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
        if partials.len() <= idx {
            partials.resize(idx + 1, PartialToolCall::default());
        }
        let p = &mut partials[idx];
        if let Some(id) = tc.get("id").and_then(Value::as_str)
            && !id.is_empty()
        {
            p.id = id.to_string();
        }
        if let Some(f) = tc.get("function") {
            if let Some(n) = f.get("name").and_then(Value::as_str)
                && !n.is_empty()
            {
                p.name.push_str(n);
            }
            if let Some(a) = f.get("arguments").and_then(Value::as_str) {
                p.arguments.push_str(a);
            }
        }
    }
}

/// Turns a `choices[0].message`-shaped value into a `CompletionResponse`,
/// applying native tool-call parsing with a ReAct fallback in `auto` mode.
/// Shared by the streaming and non-streaming paths.
fn build_response(message: &Value, usage: Option<TokenUsage>, cfg: &Config) -> CompletionResponse {
    let content = message
        .get("content")
        .and_then(|c| c.as_str())
        .map(|s| s.to_string());
    let raw_tool_calls = message.get("tool_calls").cloned();

    // vLLM normalizes model-specific tool formats (e.g. Qwen3 XML via
    // --tool-call-parser qwen3_xml) into standard OpenAI tool_calls, so native
    // parsing works regardless of the underlying model. In "auto" mode we fall
    // back to a ReAct action block if the model returned no structured call.
    let use_native = matches!(cfg.agent.tool_calling.as_str(), "native" | "auto");
    let tool_calls = if use_native {
        schema::parse_native_tool_calls(message).or_else(|| {
            if cfg.agent.tool_calling == "auto" {
                content
                    .as_deref()
                    .and_then(schema::parse_react_action)
                    .map(|c| vec![c])
            } else {
                None
            }
        })
    } else {
        content
            .as_deref()
            .and_then(schema::parse_react_action)
            .map(|c| vec![c])
    };

    CompletionResponse {
        content,
        tool_calls,
        raw_tool_calls,
        usage,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_call_deltas_accumulate_across_chunks() {
        let mut partials = Vec::new();
        // Name and arguments arrive split across several streamed deltas.
        accumulate_tool_calls(
            &[json!({ "index": 0, "id": "call_1", "function": { "name": "list_" } })],
            &mut partials,
        );
        accumulate_tool_calls(
            &[json!({ "index": 0, "function": { "name": "dir", "arguments": "{\"path\":" } })],
            &mut partials,
        );
        accumulate_tool_calls(
            &[json!({ "index": 0, "function": { "arguments": "\"./\"}" } })],
            &mut partials,
        );

        assert_eq!(partials.len(), 1);
        assert_eq!(partials[0].id, "call_1");
        assert_eq!(partials[0].name, "list_dir");
        assert_eq!(partials[0].arguments, "{\"path\":\"./\"}");
    }

    #[test]
    fn parallel_tool_calls_track_by_index() {
        let mut partials = Vec::new();
        accumulate_tool_calls(
            &[
                json!({ "index": 0, "id": "a", "function": { "name": "read_file", "arguments": "{}" } }),
                json!({ "index": 1, "id": "b", "function": { "name": "list_dir", "arguments": "{}" } }),
            ],
            &mut partials,
        );
        assert_eq!(partials.len(), 2);
        assert_eq!(partials[0].name, "read_file");
        assert_eq!(partials[1].name, "list_dir");
    }
}
