//! Tool framework: the `BaseTool` trait, tool specs, and the registry.

pub mod fs;

use async_trait::async_trait;
use serde_json::Value;
use snafu::prelude::*;
use std::collections::HashMap;
use std::time::Duration;

use crate::config::Config;

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("unknown tool `{name}`"))]
    UnknownTool { name: String },
    #[snafu(display("tool `{tool}` requires argument `{arg}`"))]
    MissingArg { tool: String, arg: String },
    #[snafu(display("failed to run command"))]
    Spawn { source: std::io::Error },
    #[snafu(display("command timed out after {secs}s and was killed"))]
    Timeout { secs: u64 },
    #[snafu(display("path `{path}` is outside the allowed sandbox roots"))]
    PathNotAllowed { path: String },
    #[snafu(display("filesystem operation failed for `{path}`"))]
    Fs {
        source: std::io::Error,
        path: String,
    },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Machine-readable description used to build the model's tool schema.
#[derive(Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the arguments object.
    pub parameters: Value,
}

/// A tool-call request parsed from the model (native `tool_calls` or ReAct text).
#[derive(Clone, Debug)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: Value,
}

/// Whether an identical, already-successful call may be skipped by the loop guard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DedupePolicy {
    /// Always execute; repeats may have side effects or time-varying output.
    Always,
    /// Skip if an identical call already succeeded this task (idempotent).
    SkipIfIdenticalSuccess,
}

#[async_trait]
pub trait BaseTool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn execute(&self, args: &Value) -> Result<String>;
    /// Default: always execute. Idempotent mutating tools override this.
    fn dedupe_policy(&self) -> DedupePolicy {
        DedupePolicy::Always
    }
}

pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn BaseTool>>,
}

impl ToolRegistry {
    pub fn init_default(config: &Config) -> Self {
        let allowed = config.security.allowed_paths.clone();
        let tools: Vec<Box<dyn BaseTool>> = vec![
            Box::new(BashExecutor {
                timeout: Duration::from_secs(config.security.bash_timeout_secs),
            }),
            Box::new(fs::ReadFile::new(allowed.clone())),
            Box::new(fs::WriteFile::new(allowed.clone())),
            Box::new(fs::ListDir::new(allowed)),
        ];
        let mut map: HashMap<String, Box<dyn BaseTool>> = HashMap::new();
        for tool in tools {
            map.insert(tool.spec().name, tool);
        }
        Self { tools: map }
    }

    /// All specs, used to build the OpenAI `tools` array and the ReAct prompt.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|t| t.spec()).collect()
    }

    /// The dedupe policy for a tool by name (unknown tools default to `Always`).
    pub fn dedupe_policy(&self, name: &str) -> DedupePolicy {
        self.tools
            .get(name)
            .map(|t| t.dedupe_policy())
            .unwrap_or(DedupePolicy::Always)
    }

    /// Dispatch by name. Unknown tools return an error the loop surfaces to the model.
    /// Output is capped at `max_output_bytes` (head and tail kept) to protect
    /// the context window; the caller derives the cap from the context budget.
    #[tracing::instrument(skip_all, fields(tool = %name))]
    pub async fn execute(
        &self,
        name: &str,
        args: &Value,
        max_output_bytes: usize,
    ) -> Result<String> {
        let out = match self.tools.get(name) {
            Some(tool) => tool.execute(args).await?,
            None => {
                return UnknownToolSnafu {
                    name: name.to_string(),
                }
                .fail();
            }
        };
        Ok(truncate_middle(out, max_output_bytes))
    }
}

/// Keeps the head and tail of `s` within roughly `max` bytes, since both the
/// start (context) and the end (errors, summaries) of tool output matter.
/// Cuts only on UTF-8 character boundaries.
pub fn truncate_middle(s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    let mut head = max / 2;
    while !s.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = s.len() - (max - head);
    while !s.is_char_boundary(tail) {
        tail += 1;
    }
    format!(
        "{}\n[... output truncated: {} of {} bytes omitted ...]\n{}",
        &s[..head],
        tail - head,
        s.len(),
        &s[tail..]
    )
}

/// Standard command runner. NOT sandboxed by `allowed_paths` — gated by human approval.
pub struct BashExecutor {
    /// Commands still running after this are killed (the `sh` process; children
    /// that detach from it may survive).
    timeout: Duration,
}

#[async_trait]
impl BaseTool for BashExecutor {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "execute_bash".into(),
            description: "Execute a single shell command and return stdout/stderr.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The shell command to run" }
                },
                "required": ["command"]
            }),
        }
    }

    async fn execute(&self, args: &Value) -> Result<String> {
        let cmd = args["command"].as_str().context(MissingArgSnafu {
            tool: "execute_bash".to_string(),
            arg: "command".to_string(),
        })?;

        let run = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .kill_on_drop(true)
            .output();
        // On timeout the future is dropped, and `kill_on_drop` kills the child.
        let output = tokio::time::timeout(self.timeout, run)
            .await
            .map_err(|_| Error::Timeout {
                secs: self.timeout.as_secs(),
            })?
            .context(SpawnSnafu)?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        Ok(format!(
            "exit: {}\nSTDOUT:\n{stdout}\nSTDERR:\n{stderr}",
            output.status.code().unwrap_or(-1)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn short_output_is_untouched() {
        assert_eq!(truncate_middle("abc".into(), 10), "abc");
    }

    #[test]
    fn long_output_keeps_head_and_tail() {
        let s = format!("{}{}", "a".repeat(100), "z".repeat(100));
        let t = truncate_middle(s, 20);
        assert!(t.starts_with("aaaaaaaaaa\n[... output truncated: 180 of 200 bytes"));
        assert!(t.ends_with("\nzzzzzzzzzz"));
    }

    #[test]
    fn truncation_respects_utf8_boundaries() {
        // Each 'é' is 2 bytes; an odd cap would split one without boundary checks.
        let t = truncate_middle("é".repeat(50), 11);
        assert!(t.contains("output truncated"));
    }

    #[tokio::test]
    async fn bash_runs_and_reports_exit_code() {
        let bash = BashExecutor {
            timeout: Duration::from_secs(10),
        };
        let out = bash
            .execute(&json!({ "command": "echo hi; exit 3" }))
            .await
            .unwrap();
        assert!(out.starts_with("exit: 3\nSTDOUT:\nhi\n"));
    }

    #[tokio::test]
    async fn bash_times_out() {
        let bash = BashExecutor {
            timeout: Duration::from_millis(200),
        };
        let start = std::time::Instant::now();
        let err = bash
            .execute(&json!({ "command": "sleep 5" }))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Timeout { .. }));
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
