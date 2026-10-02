//! Test helpers: a scripted OpenAI-compatible model server on localhost.

use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serves `/v1/chat/completions` from a script: each request gets the next
/// assistant message (the last one repeats). Other paths get 404. Returns
/// the base URL (`http://127.0.0.1:<port>/v1`).
pub async fn mock_model(script: Vec<Value>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let script = Arc::new(Mutex::new((script, 0usize)));
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let script = script.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                // Read headers, then the body by Content-Length.
                let (head_end, len) = loop {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..i]).to_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        break (i + 4, len);
                    }
                };
                while buf.len() < head_end + len {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let first = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let (status, body) = if first.starts_with("POST /v1/chat/completions") {
                    let mut s = script.lock().unwrap();
                    let i = s.1.min(s.0.len() - 1);
                    s.1 += 1;
                    let message = s.0[i].clone();
                    // Report roughly what a real server would: ~4 bytes/token.
                    let prompt = (buf.len() - head_end) / 4;
                    (
                        "200 OK",
                        json!({ "choices": [{ "message": message }],
                                "usage": { "prompt_tokens": prompt, "completion_tokens": 10,
                                           "total_tokens": prompt + 10 } })
                        .to_string(),
                    )
                } else {
                    ("404 Not Found", "{}".to_string())
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}/v1")
}

/// An assistant message calling one tool.
pub fn call(id: &str, name: &str, args: Value) -> Value {
    json!({ "role": "assistant", "content": null, "tool_calls": [{
        "id": id, "type": "function",
        "function": { "name": name, "arguments": args.to_string() } }] })
}

/// A final assistant answer.
pub fn answer(text: &str) -> Value {
    json!({ "role": "assistant", "content": text })
}
