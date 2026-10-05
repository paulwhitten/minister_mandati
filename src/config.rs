//! Configuration loading for the agent.
//!
//! Convention over configuration: `agent.toml` is optional. When absent, the
//! agent runs on built-in defaults (a local Ollama-style endpoint). A config
//! file is discovered at `./agent.toml` or `~/.config/minister_mandati/agent.toml`, and
//! `${ENV_VAR}` references are expanded so secrets stay out of the file. The
//! `MIMA_BASE_URL`, `MIMA_MODEL`, and `MIMA_API_KEY` env vars override after.

use serde::Deserialize;
use snafu::prelude::*;
use std::path::{Path, PathBuf};

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("failed to read config at {}", path.display()))]
    Read {
        source: std::io::Error,
        path: PathBuf,
    },
    #[snafu(display("failed to parse config at {}", path.display()))]
    Parse {
        source: toml::de::Error,
        path: PathBuf,
    },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub provider: Provider,
    #[serde(default)]
    pub agent: Agent,
    #[serde(default)]
    pub security: Security,
    #[serde(default)]
    pub context: Context,
    #[serde(default)]
    pub session: SessionConfig,
}

#[derive(Debug, Deserialize)]
pub struct Provider {
    // Reserved for provider-specific behavior (e.g. anthropic vs openai); parsed now, used later.
    #[allow(dead_code)]
    #[serde(rename = "type", default = "default_provider_type")]
    pub provider_type: String,
    #[serde(default = "default_api_key")]
    pub api_key: String,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default = "default_model")]
    pub default_model: String,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct Agent {
    pub temperature: f32,
    /// Reply limit per request; also reserved out of the context window.
    pub max_tokens: usize,
    /// "native" | "react" | "auto"
    pub tool_calling: String,
    /// Stream tokens live over SSE when true. Override via `[agent].stream`.
    pub stream: bool,
    /// Nucleus sampling; sent only when set (model vendors publish
    /// recommended values, e.g. for their reasoning modes).
    #[serde(default)]
    pub top_p: Option<f32>,
    /// Top-k sampling (a vLLM and llama.cpp extension); sent only when set.
    #[serde(default)]
    pub top_k: Option<u32>,
    /// Min-p sampling (vLLM and llama.cpp); sent only when set. llama.cpp's
    /// own default is 0.05, not off.
    #[serde(default)]
    pub min_p: Option<f32>,
    /// Extra request fields passed through as-is, e.g.
    /// `extra_body = { chat_template_kwargs = { force_nonempty_content = true } }`.
    #[serde(default)]
    pub extra_body: Option<serde_json::Map<String, serde_json::Value>>,
    /// Full-replacement system prompt; when non-empty it bypasses composition.
    #[serde(default)]
    pub system_prompt_override: String,
    /// Composed system-prompt segments. `None` uses the built-in default; an
    /// empty string suppresses that segment.
    #[serde(default)]
    pub system_prompt_prefix: Option<String>,
    #[serde(default)]
    pub system_prompt_body: Option<String>,
    #[serde(default)]
    pub system_prompt_suffix: Option<String>,
    /// Skip an identical, already-successful mutating tool call (loop guard).
    #[serde(default = "default_true")]
    pub dedupe_identical_writes: bool,
    /// Sliding-window size for the no-progress loop guard.
    #[serde(default = "default_loop_window")]
    pub loop_guard_window: usize,
    /// Identical repeats within the window that trip the loop guard.
    #[serde(default = "default_loop_threshold")]
    pub loop_guard_repeat_threshold: usize,
    /// Hard cap on tool/reasoning steps per task.
    #[serde(default = "default_max_steps")]
    pub max_steps: usize,
}

/// Context-window management (see `docs/context.md`). Fractions are of the
/// operating budget E = min(usable window, `budget`).
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct Context {
    /// Context window in tokens; `None` discovers it from the server.
    pub window: Option<usize>,
    /// Operate below the window (tokens); `None` uses the whole usable window.
    pub budget: Option<usize>,
    /// Mask old tool outputs above this fraction of E ...
    pub mask_at: f64,
    /// ... down to this fraction (evicting oldest steps if masking cannot).
    pub mask_to: f64,
    /// Newest tool output (fraction of E) protected from masking.
    pub keep_recent: f64,
    /// Count each request exactly with the server's `/tokenize` endpoint
    /// (vLLM) when it has one; otherwise estimate from reported usage.
    pub server_tokenize: bool,
}

/// Sessions and transcripts (see `docs/sessions.md`).
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct SessionConfig {
    /// Write a transcript for each session. Off by default; can also be
    /// enabled with `--transcript` or `/enable_transcript`.
    pub transcripts: bool,
    /// Where transcripts go; a leading `~/` means the home directory.
    pub transcript_dir: String,
    /// Largest single tool output stored in a transcript, in bytes.
    pub max_output_bytes: usize,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            transcripts: false,
            transcript_dir: "~/.mima/transcripts".to_string(),
            max_output_bytes: 1 << 20,
        }
    }
}

impl Default for Context {
    fn default() -> Self {
        Self {
            window: None,
            budget: None,
            mask_at: 0.6,
            mask_to: 0.4,
            keep_recent: 0.25,
            server_tokenize: true,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct Security {
    pub require_approval_for_bash: bool,
    pub require_approval_for_writes: bool,
    /// Shell command prefixes that skip the approval prompt (whole-word match;
    /// commands with shell metacharacters always prompt). See `approval.rs`.
    #[serde(default)]
    pub auto_approve_bash: Vec<String>,
    /// Wall-clock limit for one `execute_bash` command; it is killed on expiry.
    #[serde(default = "default_bash_timeout_secs")]
    pub bash_timeout_secs: u64,
    /// Command prefix for every shell command, e.g. a sandbox:
    /// `["bwrap", "--unshare-net", ..., "--"]`. The command runs as
    /// `<prefix...> sh -c <command>`. Empty (default) runs `sh -c` directly.
    #[serde(default)]
    pub bash_wrapper: Vec<String>,
    /// Extra environment variable names passed to shell commands. Commands
    /// otherwise get only a fixed allowlist (PATH, HOME, locale, toolchain
    /// locations; see `src/tools/shell.rs`), never mima's own secrets.
    #[serde(default)]
    pub env_passthrough: Vec<String>,
    /// Sandbox roots for the filesystem tools only (not a shell sandbox).
    pub allowed_paths: Vec<String>,
}

impl Default for Agent {
    fn default() -> Self {
        Self {
            temperature: 0.2,
            max_tokens: 4096,
            tool_calling: "auto".to_string(),
            stream: true,
            top_p: None,
            top_k: None,
            min_p: None,
            extra_body: None,
            system_prompt_override: String::new(),
            system_prompt_prefix: None,
            system_prompt_body: None,
            system_prompt_suffix: None,
            dedupe_identical_writes: true,
            loop_guard_window: 6,
            loop_guard_repeat_threshold: 3,
            max_steps: 50,
        }
    }
}

impl Default for Security {
    fn default() -> Self {
        Self {
            require_approval_for_bash: true,
            require_approval_for_writes: true,
            auto_approve_bash: Vec::new(),
            bash_timeout_secs: default_bash_timeout_secs(),
            bash_wrapper: Vec::new(),
            env_passthrough: Vec::new(),
            allowed_paths: vec!["./".to_string()],
        }
    }
}

impl Default for Provider {
    fn default() -> Self {
        Self {
            provider_type: default_provider_type(),
            api_key: default_api_key(),
            base_url: default_base_url(),
            default_model: default_model(),
        }
    }
}

fn default_provider_type() -> String {
    "custom".to_string()
}

fn default_api_key() -> String {
    // Local servers (Ollama/vLLM) ignore the key; this stub keeps zero-config working.
    "ollama".to_string()
}

fn default_base_url() -> String {
    "http://localhost:11434/v1".to_string()
}

fn default_model() -> String {
    "qwen2.5-coder:latest".to_string()
}

fn default_true() -> bool {
    true
}

fn default_loop_window() -> usize {
    6
}

fn default_loop_threshold() -> usize {
    3
}

fn default_max_steps() -> usize {
    50
}

fn default_bash_timeout_secs() -> u64 {
    300
}

impl Config {
    /// Load configuration following convention over configuration: use
    /// `./agent.toml`, then `~/.config/minister_mandati/agent.toml`, else built-in
    /// defaults. Zero configuration is a valid setup. `MIMA_BASE_URL`,
    /// `MIMA_MODEL`, and `MIMA_API_KEY` env vars override afterward.
    pub fn load_or_discover() -> Result<Self> {
        let mut config = match discover_config_path() {
            Some(path) => Self::load(&path)?,
            None => Self::default(),
        };
        config.apply_env_overrides();
        Ok(config)
    }

    /// Load from an explicit path (the file must exist).
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path).context(ReadSnafu {
            path: path.to_path_buf(),
        })?;
        let expanded = expand_env(&raw);
        let config: Config = toml::from_str(&expanded).context(ParseSnafu {
            path: path.to_path_buf(),
        })?;
        Ok(config)
    }

    fn apply_env_overrides(&mut self) {
        if let Some(v) = override_var("BASE_URL") {
            self.provider.base_url = v;
        }
        if let Some(v) = override_var("MODEL") {
            self.provider.default_model = v;
        }
        if let Some(v) = override_var("API_KEY") {
            self.provider.api_key = v;
        }
    }

    /// The system prompt for this configuration, composed from prefix, body, and
    /// suffix (each resolved env -> config -> default), unless a full override is
    /// set. See `docs/design/design-composable-system-prompt.md`.
    pub fn system_prompt(&self) -> String {
        compose_system_prompt(&self.agent)
    }
}

/// Reads a `MIMA_`-prefixed override env var, returning it when non-empty.
fn override_var(suffix: &str) -> Option<String> {
    match std::env::var(format!("MIMA_{suffix}")) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

const DEFAULT_PREFIX: &str = "";

const DEFAULT_BODY: &str = "You are a terminal-native, Linux-first coding agent. You are an expert in Linux \
     and Unix systems: shells (bash, zsh, sh), core commands and pipelines, systemd \
     and journald, and process, file, network, and storage management. You know the \
     major distributions (Debian/Ubuntu, Fedora/RHEL, Arch, SUSE) and their package \
     managers, and you have deep knowledge of the Linux kernel, loadable modules, the \
     kbuild/Kconfig build system, and kernel contribution workflows (checkpatch, \
     get_maintainer, git format-patch/send-email). You write excellent C, Rust, and \
     Python and use the right build and debug tools (make, cmake, cargo, pip/venv, \
     gdb, lldb, valgrind, perf, strace). You can call tools to read and write files \
     and run shell commands.";

const DEFAULT_SUFFIX: &str = "Prefer small, verifiable steps and idiomatic, standard practices. Use find_files and \
     search_files to locate code rather than shell commands. To change an \
     existing file, read it (or the relevant lines) and use edit_file: copy old_string exactly \
     from the read_file output without the line-number prefix, include a few unchanged lines \
     around the change, and prefer replacing a whole small block over many one-line edits. \
     Use write_file only for new files or complete rewrites, and never write placeholders \
     such as '... existing code ...'. Do not repeat a \
     tool call that has already succeeded. When the task is complete, stop and request \
     no further tools; a brief acknowledgment is sufficient even if asked to be silent.";

/// Resolves one prompt segment: env var (possibly empty) wins, then the config
/// value (possibly empty), then the built-in default. A set-but-empty value
/// suppresses the segment.
fn resolve_segment(env_key: &str, toml_val: &Option<String>, default: &str) -> String {
    if let Ok(v) = std::env::var(env_key) {
        return expand_env(&v);
    }
    if let Some(v) = toml_val {
        return v.clone();
    }
    default.to_string()
}

/// Composes the system prompt from segments unless a full override is set.
fn compose_system_prompt(agent: &Agent) -> String {
    if let Ok(v) = std::env::var("MIMA_SYSTEM_PROMPT")
        && !v.is_empty()
    {
        return expand_env(&v);
    }
    if !agent.system_prompt_override.is_empty() {
        return agent.system_prompt_override.clone();
    }
    let prefix = resolve_segment(
        "MIMA_SYSTEM_PROMPT_PREFIX",
        &agent.system_prompt_prefix,
        DEFAULT_PREFIX,
    );
    let body = resolve_segment(
        "MIMA_SYSTEM_PROMPT_BODY",
        &agent.system_prompt_body,
        DEFAULT_BODY,
    );
    let suffix = resolve_segment(
        "MIMA_SYSTEM_PROMPT_SUFFIX",
        &agent.system_prompt_suffix,
        DEFAULT_SUFFIX,
    );
    [prefix, body, suffix]
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// First existing config file: `./agent.toml`, then
/// `$XDG_CONFIG_HOME/minister_mandati/agent.toml`, then `$HOME/.config/minister_mandati/agent.toml`.
fn discover_config_path() -> Option<PathBuf> {
    let local = PathBuf::from("agent.toml");
    if local.is_file() {
        return Some(local);
    }
    let mut candidates = Vec::new();
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        candidates.push(PathBuf::from(xdg).join("minister_mandati/agent.toml"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join(".config/minister_mandati/agent.toml"));
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// Replaces `${VAR}` occurrences with the value of the environment variable
/// `VAR`. Unset variables expand to an empty string.
fn expand_env(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' && chars.peek() == Some(&'{') {
            chars.next(); // consume '{'
            let mut var = String::new();
            for c in chars.by_ref() {
                if c == '}' {
                    break;
                }
                var.push(c);
            }
            if let Ok(val) = std::env::var(&var) {
                out.push_str(&val);
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{Config, expand_env};

    #[test]
    fn partial_sections_keep_defaults() {
        let c: Config = toml::from_str(
            "[agent]\ntemperature = 0.6\ntop_p = 0.95\n[security]\nrequire_approval_for_bash = false\n",
        )
        .unwrap();
        assert_eq!(c.agent.temperature, 0.6);
        assert_eq!(c.agent.top_p, Some(0.95));
        assert_eq!(c.agent.tool_calling, "auto");
        assert_eq!(c.agent.max_tokens, 4096);
        assert!(!c.security.require_approval_for_bash);
        assert!(c.security.require_approval_for_writes);
    }

    #[test]
    fn expands_known_var() {
        // SAFETY: single-threaded test.
        unsafe { std::env::set_var("MIMA_TEST_KEY", "secret") };
        assert_eq!(expand_env("key=${MIMA_TEST_KEY}"), "key=secret");
    }

    #[test]
    fn unset_var_becomes_empty() {
        assert_eq!(expand_env("x=${DEFINITELY_UNSET_VAR_XYZ}"), "x=");
    }

    #[test]
    fn defaults_give_a_local_endpoint() {
        let c = Config::default();
        assert_eq!(c.provider.base_url, "http://localhost:11434/v1");
        assert!(c.security.require_approval_for_bash);
        assert_eq!(c.agent.tool_calling, "auto");
    }

    #[test]
    fn env_overrides_apply() {
        // SAFETY: single-threaded test.
        unsafe { std::env::set_var("MIMA_MODEL", "test-model:1b") };
        let mut c = Config::default();
        c.apply_env_overrides();
        assert_eq!(c.provider.default_model, "test-model:1b");
        unsafe { std::env::remove_var("MIMA_MODEL") };
    }

    #[test]
    fn default_prompt_composes_body_and_suffix() {
        let p = Config::default().system_prompt();
        assert!(p.contains("Linux-first"));
        assert!(p.contains("Do not repeat a tool call that has already succeeded"));
    }

    #[test]
    fn override_bypasses_composition() {
        let mut c = Config::default();
        c.agent.system_prompt_override = "CUSTOM PROMPT".to_string();
        assert_eq!(c.system_prompt(), "CUSTOM PROMPT");
    }

    #[test]
    fn empty_segment_suppresses_it() {
        let mut c = Config::default();
        c.agent.system_prompt_body = Some(String::new());
        let p = c.system_prompt();
        assert!(!p.contains("Linux-first"));
        assert!(p.contains("Do not repeat a tool call that has already succeeded"));
    }
}
