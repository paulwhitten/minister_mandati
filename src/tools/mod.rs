//! Tool framework: the `BaseTool` trait, tool specs, and the registry.

pub mod edit;
pub mod fs;
pub mod search;
pub mod shell;

use async_trait::async_trait;
use serde_json::Value;
use snafu::prelude::*;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::config::Config;

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("unknown tool `{name}`; the available tools are: {available}"))]
    UnknownTool { name: String, available: String },
    #[snafu(display("tool `{tool}` requires argument `{arg}`"))]
    MissingArg { tool: String, arg: String },
    #[snafu(display("failed to run command"))]
    Spawn { source: std::io::Error },
    #[snafu(display("command timed out after {secs}s and was killed"))]
    Timeout { secs: u64 },
    #[snafu(display("path `{path}` is outside the allowed sandbox roots"))]
    PathNotAllowed { path: String },
    #[snafu(display("cannot access `{path}`: {}", describe_io(source)))]
    Fs {
        source: std::io::Error,
        path: String,
    },
    /// A tool declined the call; `message` is written for the model and says
    /// what to do next.
    #[snafu(display("{message}"))]
    Refused { message: String },
    #[snafu(display("not run: {reason}"))]
    Sandbox { reason: String },
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

/// Per-call information from the agent loop.
pub struct ToolEnv {
    /// Largest output (bytes) that reaches the model uncut; tools that can
    /// stop at a clean boundary (read_file at a whole line) use it.
    pub output_budget: usize,
}

#[async_trait]
pub trait BaseTool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn execute(&self, args: &Value, env: &ToolEnv) -> Result<String>;
    /// What the operator approves, e.g. a diff; computed without side
    /// effects. An error means the call cannot succeed as given and goes
    /// straight back to the model, with nothing to approve.
    async fn preview(&self, _args: &Value) -> Result<Option<String>> {
        Ok(None)
    }
    /// Default: always execute. Idempotent mutating tools override this.
    fn dedupe_policy(&self) -> DedupePolicy {
        DedupePolicy::Always
    }
}

pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn BaseTool>>,
    /// Which files the model has read (shared by the file tools).
    files: Arc<edit::FileTracker>,
    /// Whether `execute_bash` runs commands inside the Tier 1 sandbox.
    shell_sandboxed: bool,
}

impl ToolRegistry {
    pub fn init_default(config: &Config) -> Self {
        let bash = BashExecutor::from_config(&config.security);
        let shell_sandboxed = matches!(bash.sandbox, Some(Ok(_)));
        let allowed = config.security.allowed_paths.clone();
        let files = edit::FileTracker::shared();
        let tools: Vec<Box<dyn BaseTool>> = vec![
            Box::new(bash),
            Box::new(fs::ReadFile::new(allowed.clone(), files.clone())),
            Box::new(edit::EditFile::new(allowed.clone(), files.clone())),
            Box::new(fs::WriteFile::new(allowed.clone(), files.clone())),
            Box::new(fs::ListDir::new(allowed.clone())),
            Box::new(search::FindFiles::new(allowed.clone())),
            Box::new(search::SearchFiles::new(allowed)),
        ];
        let mut map: HashMap<String, Box<dyn BaseTool>> = HashMap::new();
        for tool in tools {
            map.insert(tool.spec().name, tool);
        }
        if !shell_sandboxed && !config.security.auto_approve_bash.is_empty() {
            tracing::warn!(
                "auto_approve_bash is ignored: shell commands are not sandboxed, so every command asks for approval"
            );
        }
        Self {
            tools: map,
            files,
            shell_sandboxed,
        }
    }

    /// Whether shell commands run inside the Tier 1 sandbox.
    pub fn shell_sandboxed(&self) -> bool {
        self.shell_sandboxed
    }

    /// All specs, used to build the OpenAI `tools` array and the ReAct prompt.
    /// Sorted by name so every request is identical (deterministic, and the
    /// server's prefix cache stays valid).
    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut specs: Vec<ToolSpec> = self.tools.values().map(|t| t.spec()).collect();
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        specs
    }

    /// Forgets which files were read (a new session).
    pub fn reset_files(&self) {
        self.files.clear();
    }

    /// The tool's approval preview (see `BaseTool::preview`).
    pub async fn preview(&self, name: &str, args: &Value) -> Result<Option<String>> {
        match self.tools.get(name) {
            Some(tool) => tool.preview(args).await,
            None => Ok(None),
        }
    }

    /// The dedupe policy for a tool by name (unknown tools default to `Always`).
    pub fn dedupe_policy(&self, name: &str) -> DedupePolicy {
        self.tools
            .get(name)
            .map(|t| t.dedupe_policy())
            .unwrap_or(DedupePolicy::Always)
    }

    /// Dispatch by name. Unknown tools return an error the loop surfaces to the model.
    /// Returns the full output; the caller caps what enters the model's context
    /// (see `truncate_middle`) and may keep the full copy in the transcript.
    #[tracing::instrument(skip_all, fields(tool = %name))]
    pub async fn execute(&self, name: &str, args: &Value, env: &ToolEnv) -> Result<String> {
        match self.tools.get(name) {
            Some(tool) => tool.execute(args, env).await,
            None => UnknownToolSnafu {
                name: name.to_string(),
                available: {
                    let mut names: Vec<&str> = self.tools.keys().map(String::as_str).collect();
                    names.sort_unstable();
                    names.join(", ")
                },
            }
            .fail(),
        }
    }
}

/// An I/O error in words the model can act on.
fn describe_io(e: &std::io::Error) -> String {
    use std::io::ErrorKind::*;
    match e.kind() {
        NotFound => "no such file or directory (paths are relative to the working \
                     directory; use find_files or list_dir to locate it)"
            .into(),
        IsADirectory => "it is a directory (use list_dir or find_files)".into(),
        NotADirectory => "a component of the path is a file, not a directory".into(),
        PermissionDenied => "permission denied".into(),
        _ => e.to_string(),
    }
}

/// Maps tool calls written for other agents onto mima's tools, so a model
/// trained on them still makes progress. Eval transcripts showed many
/// `str_replace_editor` calls (Anthropic's text-editor tool) and shell-tool
/// names such as `bash`. Returns the call unchanged when nothing applies.
pub fn normalize_call(call: ToolCall) -> ToolCall {
    let s = |v: &Value| v.as_str().map(str::to_string);
    let mapped = match call.name.as_str() {
        "bash" | "shell" | "run_shell_command" | "run_command" | "execute_command" | "terminal"
        | "run_terminal_cmd" => call
            .args
            .get("command")
            .and_then(s)
            .map(|c| ("execute_bash", serde_json::json!({ "command": c }))),
        "str_replace_editor" | "str_replace_based_edit_tool" | "text_editor" => {
            let a = &call.args;
            let path = a.get("path").and_then(s);
            match (a.get("command").and_then(Value::as_str), path) {
                (Some("view"), Some(path)) => {
                    let mut args = serde_json::json!({ "path": path });
                    if let Some(r) = a.get("view_range").and_then(Value::as_array)
                        && let (Some(start), Some(end)) = (
                            r.first().and_then(Value::as_u64),
                            r.get(1).and_then(Value::as_i64),
                        )
                    {
                        args["offset"] = serde_json::json!(start.max(1));
                        if end > 0 {
                            args["limit"] =
                                serde_json::json!((end as u64).saturating_sub(start) + 1);
                        }
                    }
                    let is_dir = std::path::Path::new(&path).is_dir();
                    Some((if is_dir { "list_dir" } else { "read_file" }, args))
                }
                (Some("str_replace"), Some(path)) => Some((
                    "edit_file",
                    serde_json::json!({
                        "path": path,
                        "old_string": a.get("old_str").cloned().unwrap_or(Value::Null),
                        "new_string": a.get("new_str").cloned().unwrap_or(Value::String(String::new())),
                    }),
                )),
                (Some("create"), Some(path)) => Some((
                    "write_file",
                    serde_json::json!({
                        "path": path,
                        "content": a.get("file_text").cloned().unwrap_or(Value::String(String::new())),
                    }),
                )),
                _ => None,
            }
        }
        _ => None,
    };
    match mapped {
        Some((name, args)) => {
            tracing::info!(from = %call.name, to = name, "mapped a tool call to a mima tool");
            ToolCall {
                id: call.id,
                name: name.to_string(),
                args,
            }
        }
        None => call,
    }
}

/// mima's own tool names; a shell command starting with one is a mistake.
const TOOL_NAMES: &[&str] = &[
    "read_file",
    "write_file",
    "edit_file",
    "list_dir",
    "find_files",
    "search_files",
    "execute_bash",
];

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

/// Standard command runner. NOT sandboxed by `allowed_paths` — gated by human
/// approval. Each command gets an allowlisted environment and a private
/// `TMPDIR`, runs in its own session, and its whole process group is killed
/// when it returns or times out (see `shell.rs`).
pub struct BashExecutor {
    /// Commands still running after this are killed with their whole group.
    timeout: Duration,
    /// Optional command prefix (e.g. a `bwrap` sandbox); see `bash_wrapper`.
    wrapper: Vec<String>,
    /// The complete environment commands run with.
    env: Vec<(String, String)>,
    /// Kept for its lifetime: the directory is removed when this is dropped.
    _tmp: Option<shell::SessionTmp>,
    /// Tier 1 sandbox: `None` when off; an error when it is required but the
    /// kernel cannot provide it (every command then fails with that error).
    sandbox: Option<std::result::Result<SandboxRun, String>>,
}

/// How to run a command inside the sandbox.
struct SandboxRun {
    exe: std::path::PathBuf,
    policy: crate::sandbox::Policy,
}

impl BashExecutor {
    pub fn new(timeout: Duration, wrapper: Vec<String>, env_passthrough: &[String]) -> Self {
        let mut env = shell::child_env(std::env::vars(), env_passthrough);
        let tmp = match shell::SessionTmp::create() {
            Ok(t) => {
                env.retain(|(k, _)| k != "TMPDIR");
                env.push(("TMPDIR".into(), t.path().display().to_string()));
                Some(t)
            }
            Err(e) => {
                tracing::warn!(error = %e, "no private temp directory for shell commands");
                None
            }
        };
        Self {
            timeout,
            wrapper,
            env,
            _tmp: tmp,
            sandbox: None,
        }
    }

    /// The executor configured by `[security]`, with the shell sandbox when
    /// enabled (see `sandbox.rs`).
    pub fn from_config(sec: &crate::config::Security) -> Self {
        let mut bash = Self::new(
            Duration::from_secs(sec.bash_timeout_secs),
            sec.bash_wrapper.clone(),
            &sec.env_passthrough,
        );
        bash.sandbox = sandbox_setup(sec, bash._tmp.as_ref().map(|t| t.path()));
        bash
    }

    /// One line describing how commands are confined, for the startup log
    /// and failure notes.
    pub fn sandbox_summary(&self) -> String {
        match &self.sandbox {
            None => "off".into(),
            Some(Err(e)) => e.clone(),
            Some(Ok(run)) => format!(
                "{}, network {}, writable: {}",
                crate::sandbox::probe().describe(),
                if run.policy.network { "on" } else { "off" },
                run.policy
                    .write
                    .iter()
                    .filter(|p| !p.starts_with("/dev"))
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

/// Error text that a sandbox restriction typically produces.
fn looks_like_sandbox_denial(stderr: &str) -> bool {
    const SIGNS: &[&str] = &[
        "Permission denied",
        "Operation not permitted",
        "Read-only file system",
        "Network is unreachable",
        "Temporary failure in name resolution",
        "Could not resolve",
        "Name or service not known",
    ];
    SIGNS.iter().any(|s| stderr.contains(s))
}

/// Decides whether and how commands are sandboxed. Paths come only from the
/// session's configuration and starting directory, never from a tool call.
fn sandbox_setup(
    sec: &crate::config::Security,
    tmp: Option<&std::path::Path>,
) -> Option<std::result::Result<SandboxRun, String>> {
    use crate::sandbox::{SYSTEM_READ, SYSTEM_WRITE, expand_home, probe};
    let mode = sec.sandbox.as_str();
    // In unit tests the current executable is the test runner, which cannot
    // act as the helper; itests/sandbox.rs tests the real binary.
    if cfg!(test) {
        return None;
    }
    if mode == "off" {
        tracing::warn!("shell sandbox is off ([security].sandbox = \"off\")");
        return None;
    }
    let support = probe();
    if !support.usable() {
        let msg = format!("shell sandbox {}", support.describe());
        if mode == "required" {
            tracing::error!(
                "{msg}; [security].sandbox = \"required\", so shell commands will not run"
            );
            return Some(Err(format!("{msg} and [security].sandbox is \"required\"")));
        }
        tracing::warn!("{msg}; shell commands run without it");
        return None;
    }
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            let msg = format!("shell sandbox unavailable: cannot locate the mima binary: {e}");
            tracing::warn!("{msg}");
            return (mode == "required").then_some(Err(msg));
        }
    };
    let canon = |p: std::path::PathBuf| p.canonicalize().ok();
    let roots: Vec<std::path::PathBuf> = sec
        .allowed_paths
        .iter()
        .filter_map(|p| canon(expand_home(p)))
        .collect();
    let mut read: Vec<std::path::PathBuf> = SYSTEM_READ.iter().map(Into::into).collect();
    read.extend(
        sec.sandbox_read_paths
            .iter()
            .filter_map(|p| canon(expand_home(p))),
    );
    read.push(exe.clone());
    let mut write: Vec<std::path::PathBuf> = roots;
    write.extend(tmp.map(|t| t.to_path_buf()));
    write.extend(SYSTEM_WRITE.iter().map(Into::into));
    write.extend(
        sec.sandbox_writable_paths
            .iter()
            .filter_map(|p| canon(expand_home(p))),
    );
    let policy = crate::sandbox::Policy {
        read,
        write,
        network: sec.sandbox_network,
    };
    tracing::info!(
        support = %support.describe(),
        network = sec.sandbox_network,
        "shell commands are sandboxed"
    );
    Some(Ok(SandboxRun { exe, policy }))
}

#[async_trait]
impl BaseTool for BashExecutor {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "execute_bash".into(),
            description: "Execute a single shell command and return stdout/stderr. \
                          Background processes it starts are stopped when it returns."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The shell command to run" }
                },
                "required": ["command"]
            }),
        }
    }

    async fn execute(&self, args: &Value, _env: &ToolEnv) -> Result<String> {
        let cmd = args["command"].as_str().context(MissingArgSnafu {
            tool: "execute_bash".to_string(),
            arg: "command".to_string(),
        })?;

        if let Some(first) = cmd.split_whitespace().next()
            && TOOL_NAMES.contains(&first)
        {
            return RefusedSnafu {
                message: format!(
                    "`{first}` is a mima tool, not a shell command. Call the {first} tool \
                     directly instead of running it through execute_bash."
                ),
            }
            .fail();
        }
        let shell = vec!["sh".to_string(), "-c".to_string(), cmd.to_string()];
        let inner = match &self.sandbox {
            None => shell,
            Some(Ok(run)) => crate::sandbox::wrap(&run.exe, &run.policy, &shell),
            Some(Err(e)) => return Err(Error::Sandbox { reason: e.clone() }),
        };
        let argv: Vec<String> = self.wrapper.iter().cloned().chain(inner).collect();
        let output = shell::run(&argv, &self.env, self.timeout)
            .await
            .context(SpawnSnafu)?
            .map_err(|_| Error::Timeout {
                secs: self.timeout.as_secs(),
            })?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut text = format!(
            "exit: {}\nSTDOUT:\n{stdout}\nSTDERR:\n{stderr}",
            output.code.unwrap_or(-1)
        );
        if output.code != Some(0)
            && matches!(self.sandbox, Some(Ok(_)))
            && looks_like_sandbox_denial(&stderr)
        {
            text.push_str(&format!(
                "\n[sandbox: {}. Writes outside those paths, network access and \
                 reading other parts of the home directory are blocked; ask the \
                 user if the task needs more.]",
                self.sandbox_summary()
            ));
        }
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn env() -> ToolEnv {
        ToolEnv {
            output_budget: 1 << 20,
        }
    }

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
        let bash = BashExecutor::new(Duration::from_secs(10), Vec::new(), &[]);
        let out = bash
            .execute(&json!({ "command": "echo hi; exit 3" }), &env())
            .await
            .unwrap();
        assert!(out.starts_with("exit: 3\nSTDOUT:\nhi\n"));
    }

    #[tokio::test]
    async fn bash_runs_through_the_wrapper() {
        // `env` as a stand-in wrapper: it runs the rest of its arguments.
        let bash = BashExecutor::new(
            Duration::from_secs(10),
            vec!["env".into(), "MIMA_WRAPPED=1".into()],
            &[],
        );
        let out = bash
            .execute(&json!({ "command": "echo $MIMA_WRAPPED" }), &env())
            .await
            .unwrap();
        assert!(out.contains("STDOUT:\n1\n"), "{out}");
    }

    #[tokio::test]
    async fn bash_times_out() {
        let bash = BashExecutor::new(Duration::from_millis(200), Vec::new(), &[]);
        let start = std::time::Instant::now();
        let err = bash
            .execute(&json!({ "command": "sleep 5" }), &env())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Timeout { .. }));
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn bash_gets_a_clean_environment_and_private_tmpdir() {
        let bash = BashExecutor::new(Duration::from_secs(10), Vec::new(), &[]);
        assert!(bash.env.iter().all(|(k, _)| !k.starts_with("MIMA_")));
        let out = bash
            .execute(
                &json!({ "command": "env | cut -d= -f1 | sort; test -d \"$TMPDIR\" && stat -c %a \"$TMPDIR\"" }),
                &env(),
            )
            .await
            .unwrap();
        assert!(out.contains("TMPDIR"), "{out}");
        assert!(
            out.contains("\n700\n"),
            "TMPDIR exists with mode 700: {out}"
        );
        assert!(!out.contains("MIMA_"), "{out}");
        let tmp = bash._tmp.as_ref().map(|t| t.path().to_path_buf()).unwrap();
        drop(bash);
        assert!(!tmp.exists(), "private TMPDIR removed with the executor");
    }

    #[test]
    fn other_agents_tool_calls_are_mapped() {
        let call = |name: &str, args: serde_json::Value| ToolCall {
            id: "1".into(),
            name: name.into(),
            args,
        };
        let c = normalize_call(call(
            "str_replace_editor",
            json!({ "command": "view", "path": "src/lib.rs", "view_range": [10, 20] }),
        ));
        assert_eq!(c.name, "read_file");
        assert_eq!(c.args["offset"], 10);
        assert_eq!(c.args["limit"], 11);
        let c = normalize_call(call(
            "str_replace_editor",
            json!({ "command": "str_replace", "path": "a.c", "old_str": "x", "new_str": "y" }),
        ));
        assert_eq!(
            (
                c.name.as_str(),
                &c.args["old_string"],
                &c.args["new_string"]
            ),
            ("edit_file", &json!("x"), &json!("y"))
        );
        let c = normalize_call(call("bash", json!({ "command": "ls" })));
        assert_eq!(
            (c.name.as_str(), &c.args["command"]),
            ("execute_bash", &json!("ls"))
        );
        // Unknown shapes pass through unchanged.
        let c = normalize_call(call(
            "str_replace_editor",
            json!({ "command": "undo_edit" }),
        ));
        assert_eq!(c.name, "str_replace_editor");
    }

    #[tokio::test]
    async fn tool_names_in_the_shell_get_a_hint() {
        let bash = BashExecutor::new(Duration::from_secs(10), Vec::new(), &[]);
        let err = bash
            .execute(&json!({ "command": "read_file -p src/x.c" }), &env())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("is a mima tool, not a shell command"), "{err}");
    }

    #[test]
    fn io_errors_are_explained() {
        let e = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert!(describe_io(&e).contains("relative to the working directory"));
        let e = std::io::Error::from(std::io::ErrorKind::IsADirectory);
        assert!(describe_io(&e).contains("list_dir"));
    }
}
