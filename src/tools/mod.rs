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
    /// Replaces the shell tool's own time limit for this call (the
    /// verifier's runs use `[verify].timeout_secs`).
    pub timeout: Option<Duration>,
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

/// A non-negative integer argument, given as a JSON number or as a string
/// of digits: some models send `"offset": "735"` for an integer parameter.
/// Calls are checked against their schema first (`conform`); this also
/// reads other agents' arguments while `normalize_call` maps them.
pub fn arg_u64(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(lenient_u64)
}

/// A boolean argument, given as `true`/`false` or as the strings
/// `"true"`/`"false"`.
pub fn arg_bool(args: &Value, key: &str) -> Option<bool> {
    match args.get(key)? {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn lenient_u64(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str()?.trim().parse().ok())
}

fn lenient_i64(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_str()?.trim().parse().ok())
}

/// Brings a call's arguments to the JSON types its tool's schema declares,
/// before approval and execution. vLLM converts argument values only for
/// tools it was given, so a call under another agent's tool name arrives
/// as text: `"offset": "735"`, `"replace_all": "true"`, `"[1, 100]"`.
///
/// A value whose meaning is exact is converted and listed as `{arg, from}`
/// for the audit record (rule `tools.convert_args`): digits for an integer,
/// `true`/`false` for a boolean, JSON text for an array or object, a whole
/// number for an integer, and `null` as an absent argument. Anything else
/// (`"abc"`, `"7.5"`, `"yes"`, a value below the schema's `minimum`) is an
/// error naming the argument, never a silent default. Undeclared keys and
/// string arguments are left as sent.
pub fn conform(schema: &Value, args: &Value) -> std::result::Result<(Value, Vec<Value>), String> {
    let (Some(props), Some(given)) = (
        schema.get("properties").and_then(Value::as_object),
        args.as_object(),
    ) else {
        return Ok((args.clone(), Vec::new()));
    };
    let mut out = given.clone();
    let mut converted = Vec::new();
    for (key, value) in given {
        let Some(prop) = props.get(key) else {
            continue;
        };
        let want = prop.get("type").and_then(Value::as_str).unwrap_or("");
        let fits = match want {
            "integer" => value.is_i64() || value.is_u64(),
            "number" => value.is_number(),
            "boolean" => value.is_boolean(),
            "array" => value.is_array(),
            "object" => value.is_object(),
            _ => true,
        };
        if value.is_null() && !fits {
            out.remove(key);
            converted.push(serde_json::json!({ "arg": key, "from": null }));
            continue;
        }
        let value = if fits {
            value.clone()
        } else {
            let Some(v) = convert(want, value) else {
                let expected = match want {
                    "integer" => "a whole number",
                    "number" => "a number",
                    "boolean" => "true or false",
                    "array" => "an array",
                    _ => "an object",
                };
                return Err(format!("argument `{key}` must be {expected}, got {value}"));
            };
            converted.push(serde_json::json!({ "arg": key, "from": value }));
            out.insert(key.clone(), v.clone());
            v
        };
        if let (Some(min), Some(n)) = (prop.get("minimum").and_then(Value::as_f64), value.as_f64())
            && n < min
        {
            return Err(format!(
                "argument `{key}` must be at least {min}, got {value}"
            ));
        }
    }
    Ok((Value::Object(out), converted))
}

/// The exact conversions `conform` makes; `None` when the meaning is not exact.
fn convert(want: &str, v: &Value) -> Option<Value> {
    match (want, v) {
        ("integer", Value::Number(n)) => n
            .as_f64()
            .filter(|f| f.fract() == 0.0 && f.abs() < 9.0e15)
            .map(|f| Value::from(f as i64)),
        ("integer", Value::String(s)) => s.trim().parse::<i64>().ok().map(Value::from),
        ("number", Value::String(s)) => s
            .trim()
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number),
        ("boolean", Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "true" => Some(Value::Bool(true)),
            "false" => Some(Value::Bool(false)),
            _ => None,
        },
        ("array" | "object", Value::String(s)) => serde_json::from_str::<Value>(s.trim())
            .ok()
            .filter(|p| p.is_array() == (want == "array") && (p.is_array() || p.is_object())),
        _ => None,
    }
}

pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn BaseTool>>,
    /// Which files the model has read (shared by the file tools).
    files: Arc<edit::FileTracker>,
    /// Whether `execute_bash` runs commands inside the Tier 1 sandbox.
    shell_sandboxed: bool,
    /// Runs mima's own read-only commands (`git status` for the audit
    /// record) with the same wrapper, environment and sandbox as the shell.
    runner: CommandRunner,
}

/// Runs a command the way `execute_bash` would (wrapper, environment,
/// sandbox, process-group kill), for commands mima itself issues.
#[derive(Clone)]
pub struct CommandRunner {
    wrapper: Vec<String>,
    env: Vec<(String, String)>,
    sandbox: Option<std::result::Result<SandboxRun, String>>,
    summary: String,
}

impl CommandRunner {
    /// Runs `argv` with `extra_env` added; an error when it cannot start,
    /// times out, or the required sandbox is unavailable.
    pub async fn run(
        &self,
        argv: &[String],
        extra_env: &[(&str, &str)],
        timeout: Duration,
    ) -> std::result::Result<shell::Finished, String> {
        let inner = match &self.sandbox {
            None => argv.to_vec(),
            Some(Ok(run)) => crate::sandbox::wrap(&run.exe, &run.policy, argv),
            Some(Err(e)) => return Err(e.clone()),
        };
        let full: Vec<String> = self.wrapper.iter().cloned().chain(inner).collect();
        let mut env = self.env.clone();
        for (k, v) in extra_env {
            env.retain(|(n, _)| n != k);
            env.push((k.to_string(), v.to_string()));
        }
        match shell::run(&full, &env, timeout).await {
            Ok(Ok(done)) => Ok(done),
            Ok(Err(_)) => Err(format!("timed out after {} s", timeout.as_secs())),
            Err(e) => Err(e.to_string()),
        }
    }

    /// How commands are confined: `off`, an error, or the sandbox summary.
    pub fn summary(&self) -> &str {
        &self.summary
    }

    pub fn sandboxed(&self) -> bool {
        matches!(self.sandbox, Some(Ok(_)))
    }
}

impl ToolRegistry {
    pub fn init_default(config: &Config) -> Self {
        let bash = BashExecutor::from_config(&config.security);
        let shell_sandboxed = matches!(bash.sandbox, Some(Ok(_)));
        let runner = CommandRunner {
            wrapper: bash.wrapper.clone(),
            env: bash.env.clone(),
            sandbox: bash.sandbox.clone(),
            summary: bash.sandbox_summary(),
        };
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
            runner,
        }
    }

    /// Runs mima's own commands under the shell's confinement.
    pub fn runner(&self) -> &CommandRunner {
        &self.runner
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

    /// The call's arguments in the types its tool declares, and what was
    /// converted (see `conform`). Unknown tools pass through; `execute`
    /// reports them.
    pub fn conform_args(
        &self,
        name: &str,
        args: &Value,
    ) -> std::result::Result<(Value, Vec<Value>), String> {
        match self.tools.get(name) {
            Some(tool) => {
                conform(&tool.spec().parameters, args).map_err(|e| format!("{name}: {e}"))
            }
            None => Ok((args.clone(), Vec::new())),
        }
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
                    // Line ranges given the way read_file takes them.
                    for key in ["offset", "limit"] {
                        if let Some(n) = arg_u64(a, key) {
                            args[key] = serde_json::json!(n);
                        }
                    }
                    // view_range: an array, or (Nemotron 3 Nano) the same
                    // array as a JSON string, "[1, 100]".
                    let range = match a.get("view_range") {
                        Some(Value::String(t)) => serde_json::from_str::<Value>(t).ok(),
                        other => other.cloned(),
                    };
                    if let Some(r) = range.as_ref().and_then(Value::as_array)
                        && let (Some(start), Some(end)) = (
                            r.first().and_then(lenient_u64),
                            r.get(1).and_then(lenient_i64),
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
#[derive(Clone)]
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
                // Go writes a build cache (default ~/.cache/go-build, which
                // the sandbox does not let commands write): keep it in the
                // session's directory unless the user chose one.
                if !env.iter().any(|(k, _)| k == "GOCACHE") {
                    env.push((
                        "GOCACHE".into(),
                        t.path().join("go-build").display().to_string(),
                    ));
                }
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
    /// enabled (see `src/sandbox.rs`).
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
                "{}, network {}{}, writable: {}",
                crate::sandbox::probe().describe(),
                if run.policy.network { "on" } else { "off" },
                if run.policy.gpu {
                    ", GPU access (Unix sockets may accept connections)"
                } else {
                    ""
                },
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
/// `[security].sandbox_gpu`, unless it cannot help or is unsafe: without
/// NVIDIA device nodes there is nothing to grant, and as root a writable
/// `/proc` would let a command change kernel settings (for example
/// `kernel.core_pattern`, which runs a program outside the sandbox).
fn gpu_access(sec: &crate::config::Security) -> bool {
    if !sec.sandbox_gpu {
        return false;
    }
    if !crate::sandbox::gpu_present() {
        tracing::warn!(
            "[security].sandbox_gpu is set but no NVIDIA device nodes exist; not granted"
        );
        return false;
    }
    // SAFETY: geteuid has no preconditions and cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        tracing::warn!(
            "[security].sandbox_gpu is not granted when mima runs as root (a writable /proc \
             would let commands change kernel settings)"
        );
        return false;
    }
    true
}

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
    let gpu = gpu_access(sec);
    if gpu {
        write.push("/proc".into());
    }
    let policy = crate::sandbox::Policy {
        read,
        write,
        network: sec.sandbox_network,
        gpu,
    };
    tracing::info!(
        support = %support.describe(),
        network = sec.sandbox_network,
        gpu = policy.gpu,
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

    async fn execute(&self, args: &Value, env: &ToolEnv) -> Result<String> {
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
        let timeout = env.timeout.unwrap_or(self.timeout);
        let output = shell::run(&argv, &self.env, timeout)
            .await
            .context(SpawnSnafu)?
            .map_err(|_| Error::Timeout {
                secs: timeout.as_secs(),
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
            timeout: None,
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
    async fn a_call_can_replace_the_time_limit() {
        // The verifier's runs use their own limit, longer than the tool's.
        let bash = BashExecutor::new(Duration::from_millis(200), Vec::new(), &[]);
        let env = ToolEnv {
            timeout: Some(Duration::from_secs(10)),
            ..env()
        };
        let out = bash
            .execute(&json!({ "command": "sleep 1; echo ok" }), &env)
            .await
            .unwrap();
        assert!(out.contains("STDOUT:\nok\n"), "{out}");
    }

    #[tokio::test]
    async fn bash_gets_a_clean_environment_and_private_tmpdir() {
        let bash = BashExecutor::new(Duration::from_secs(10), Vec::new(), &[]);
        assert!(bash.env.iter().all(|(k, _)| !k.starts_with("MIMA_")));
        let out = bash
            .execute(
                &json!({ "command": "env | cut -d= -f1 | sort; test -d \"$TMPDIR\" && ls -ld \"$TMPDIR\" | cut -c1-10" }),
                &env(),
            )
            .await
            .unwrap();
        assert!(out.contains("TMPDIR"), "{out}");
        assert!(
            out.contains("\ndrwx------\n"),
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

    #[test]
    fn view_keeps_offset_and_limit_even_as_strings() {
        // Nemotron 3 Nano's calls in the pro calibration: offset and limit
        // given directly, as strings. They used to be dropped, so the model
        // got the start of the file again and again until the loop guard
        // stopped the turn.
        let c = normalize_call(ToolCall {
            id: "1".into(),
            name: "str_replace_editor".into(),
            args: json!({ "command": "view", "path": "lark/lark.py", "offset": "735", "limit": "50" }),
        });
        assert_eq!(c.name, "read_file");
        assert_eq!(
            (&c.args["offset"], &c.args["limit"]),
            (&json!(735), &json!(50))
        );
        let c = normalize_call(ToolCall {
            id: "2".into(),
            name: "str_replace_editor".into(),
            args: json!({ "command": "view", "path": "a.py", "view_range": ["5", "9"] }),
        });
        assert_eq!(
            (&c.args["offset"], &c.args["limit"]),
            (&json!(5), &json!(5))
        );
        // How Nano actually sends it: the whole array as a JSON string.
        let c = normalize_call(ToolCall {
            id: "3".into(),
            name: "str_replace_editor".into(),
            args: json!({ "command": "view", "path": "a.py", "view_range": "[1, 100]" }),
        });
        assert_eq!(
            (&c.args["offset"], &c.args["limit"]),
            (&json!(1), &json!(100))
        );
        let c = normalize_call(ToolCall {
            id: "4".into(),
            name: "str_replace_editor".into(),
            args: json!({ "command": "view", "path": "a.py", "view_range": "[40, -1]" }),
        });
        assert_eq!(c.args["offset"], 40);
        assert!(c.args.get("limit").is_none(), "-1 means to the end");
    }

    #[test]
    fn numbers_and_booleans_may_arrive_as_strings() {
        let a = json!({ "n": 7, "s": " 42 ", "neg": "-1", "x": "seven", "t": "TRUE", "f": false, "b": "maybe" });
        assert_eq!(arg_u64(&a, "n"), Some(7));
        assert_eq!(arg_u64(&a, "s"), Some(42));
        assert_eq!(arg_u64(&a, "neg"), None);
        assert_eq!(arg_u64(&a, "x"), None);
        assert_eq!(arg_u64(&a, "missing"), None);
        assert_eq!(arg_bool(&a, "t"), Some(true));
        assert_eq!(arg_bool(&a, "f"), Some(false));
        assert_eq!(arg_bool(&a, "b"), None);
    }

    #[test]
    fn arguments_take_their_declared_types_or_are_refused() {
        let schema = json!({ "type": "object", "properties": {
            "path": { "type": "string" },
            "offset": { "type": "integer", "minimum": 0 },
            "all": { "type": "boolean" },
            "range": { "type": "array" },
            "ratio": { "type": "number" }
        }});
        let ok = |args: Value| conform(&schema, &args).unwrap();
        // Already the right types: unchanged, nothing recorded.
        let (a, c) = ok(json!({ "path": "7", "offset": 3, "all": true, "extra": "x" }));
        assert_eq!(
            a,
            json!({ "path": "7", "offset": 3, "all": true, "extra": "x" })
        );
        assert!(c.is_empty());
        // Exact conversions, each recorded with what the model sent.
        let (a, c) = ok(json!({ "offset": " 42 ", "all": "TRUE", "range": "[1, 5]",
                                "ratio": "0.5", "path": null }));
        assert_eq!(
            a,
            json!({ "offset": 42, "all": true, "range": [1, 5], "ratio": 0.5,
                              "path": null })
        );
        assert_eq!(c.len(), 4);
        assert!(c.contains(&json!({ "arg": "offset", "from": " 42 " })));
        let (a, c) = ok(json!({ "offset": 5.0, "all": null }));
        assert_eq!(a, json!({ "offset": 5 }), "a whole float; null is absent");
        assert_eq!(c.len(), 2);
        // Anything inexact is an error naming the argument.
        for (args, msg) in [
            (
                json!({ "offset": "abc" }),
                "argument `offset` must be a whole number, got \"abc\"",
            ),
            (json!({ "offset": "7.5" }), "must be a whole number"),
            (json!({ "offset": 7.5 }), "must be a whole number"),
            (
                json!({ "offset": -1 }),
                "argument `offset` must be at least 0, got -1",
            ),
            (json!({ "offset": "-1" }), "must be at least 0"),
            (
                json!({ "all": "yes" }),
                "argument `all` must be true or false",
            ),
            (json!({ "all": 1 }), "must be true or false"),
            (
                json!({ "range": "1-5" }),
                "argument `range` must be an array",
            ),
            (json!({ "range": "{}" }), "must be an array"),
        ] {
            let e = conform(&schema, &args).unwrap_err();
            assert!(e.contains(msg), "{args}: {e}");
        }
    }

    #[tokio::test]
    async fn read_file_honors_string_line_numbers() {
        let d = std::env::temp_dir().join(format!("mima-strnum-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("f.txt");
        std::fs::write(
            &f,
            (1..=20).map(|i| format!("line {i}\n")).collect::<String>(),
        )
        .unwrap();
        let tool = fs::ReadFile::new(vec![d.display().to_string()], edit::FileTracker::shared());
        let out = tool
            .execute(
                &json!({ "path": f.display().to_string(), "offset": "15", "limit": "2" }),
                &env(),
            )
            .await
            .unwrap();
        assert!(
            out.contains("line 15") && out.contains("line 16") && !out.contains("line 17"),
            "{out}"
        );
        assert!(
            !out.contains("line 1\n") && !out.contains("    1\t"),
            "{out}"
        );
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
