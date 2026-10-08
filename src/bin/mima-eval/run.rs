//! Running suites against model profiles, and validating tasks.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::checks::{self, CheckResult, Evidence};
use crate::exec::{self, TrialDirs};
use crate::metrics::{self, Metrics};
use crate::session::rfc3339_utc;
use crate::task::{Suite, Task};

/// Infra failures (model server unreachable) are retried this many times.
const INFRA_RETRIES: u32 = 2;

// ---------------------------------------------------------------------------
// Profiles

/// One model configuration to evaluate (`profiles.toml`, see docs/eval.md).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    #[serde(skip_deserializing)]
    pub name: String,
    pub base_url: String,
    pub model: String,
    /// Sent as the bearer token; `${VAR}` is expanded. Never recorded.
    #[serde(default, skip_serializing)]
    pub api_key: Option<String>,
    /// Command that makes this model available (e.g. restart the server with
    /// it), run before the profile's trials.
    #[serde(default)]
    pub setup: Option<String>,
    #[serde(default = "default_ready_timeout")]
    pub ready_timeout_sec: u64,
    /// Command that restarts the server when it dies mid-run (for example
    /// `ssh host MIMA_FORCE=1 ./serve.sh model`). Defaults to `setup`. Run
    /// when the server stays unreachable after an infrastructure failure;
    /// each restart is recorded in run.json.
    #[serde(default)]
    pub restart: Option<String>,
    /// Command whose output describes what is being served (image, flags),
    /// run once the server is ready and stored in run.json.
    #[serde(default)]
    pub fingerprint: Option<String>,
    /// Command that prints the serving device's power draw in watts, one
    /// reading per line, for as long as it runs; gives energy per trial.
    #[serde(default)]
    pub power: Option<String>,
    /// Extra mima configuration merged into the base config, e.g.
    /// `mima = { agent = { temperature = 0.6 } }`.
    #[serde(default)]
    pub mima: toml::Table,
}

fn default_ready_timeout() -> u64 {
    1800
}

#[derive(Deserialize)]
struct ProfilesFile {
    profile: BTreeMap<String, Profile>,
}

pub fn load_profiles(path: &Path, only: &[String]) -> Result<Vec<Profile>, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let file: ProfilesFile =
        toml::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut out = Vec::new();
    for (name, mut p) in file.profile {
        if only.is_empty() || only.contains(&name) {
            p.name = name;
            out.push(p);
        }
    }
    for want in only {
        if !out.iter().any(|p| &p.name == want) {
            return Err(format!("no profile named {want} in {}", path.display()));
        }
    }
    if out.is_empty() {
        return Err(format!("{}: no profiles", path.display()));
    }
    Ok(out)
}

/// A single profile from a mima config file's `[provider]` section.
pub fn profile_from_config(base: &toml::Table) -> Profile {
    let provider = base.get("provider").and_then(|v| v.as_table());
    let get = |k: &str, d: &str| {
        provider
            .and_then(|p| p.get(k))
            .and_then(|v| v.as_str())
            .unwrap_or(d)
            .to_string()
    };
    let model = get("default_model", "qwen2.5-coder:7b");
    Profile {
        name: model.rsplit('/').next().unwrap_or(&model).to_string(),
        base_url: get("base_url", "http://localhost:11434/v1"),
        model,
        api_key: provider
            .and_then(|p| p.get("api_key"))
            .and_then(|v| v.as_str())
            .map(String::from),
        setup: None,
        ready_timeout_sec: default_ready_timeout(),
        restart: None,
        fingerprint: None,
        power: None,
        mima: toml::Table::new(),
    }
}

pub fn expand_env(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        match rest[i + 2..].find('}') {
            Some(j) => {
                out.push_str(&std::env::var(&rest[i + 2..i + 2 + j]).unwrap_or_default());
                rest = &rest[i + 3 + j..];
            }
            None => {
                out.push_str(&rest[i..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Deep-merges `over` into `base`.
fn merge(base: &mut toml::Table, over: &toml::Table) {
    for (k, v) in over {
        match (base.get_mut(k), v) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => merge(b, o),
            _ => {
                base.insert(k.clone(), v.clone());
            }
        }
    }
}

/// Request settings the harness's own model requests (readiness, tool
/// preflight, memorization probe) take from the profile's mima config, so
/// they are accepted wherever mima's are: the reply-limit field name and
/// whether temperature may be sent (see docs/cloud-providers.md).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RequestStyle {
    /// "max_tokens" or "max_completion_tokens".
    pub token_field: &'static str,
    /// `[agent].temperature = "server"`: never send a temperature.
    pub server_temperature: bool,
}

impl RequestStyle {
    /// From the base config with the profile's `mima` table merged in.
    pub fn of(base: &toml::Table, p: &Profile) -> Result<Self, String> {
        let mut c = base.clone();
        merge(&mut c, &p.mima);
        let agent = |k: &str| c.get("agent").and_then(|a| a.get(k)).cloned();
        let token_field = match agent("max_tokens_param").as_ref().map(|v| v.as_str()) {
            None | Some(Some("max_tokens")) => "max_tokens",
            Some(Some("max_completion_tokens")) => "max_completion_tokens",
            Some(v) => {
                return Err(format!(
                    "profile {}: agent.max_tokens_param must be \"max_tokens\" or \"max_completion_tokens\", not {v:?}",
                    p.name
                ));
            }
        };
        let server_temperature = agent("temperature").is_some_and(|v| v.as_str() == Some("server"));
        Ok(Self {
            token_field,
            server_temperature,
        })
    }

    /// A chat completion body with the reply limit under the right name.
    pub fn body(&self, model: &str, max_tokens: u64, rest: Value) -> Value {
        let mut b = json!({ "model": model });
        b[self.token_field] = json!(max_tokens);
        if let Value::Object(m) = rest {
            for (k, v) in m {
                if k == "temperature" && self.server_temperature {
                    continue;
                }
                b[k] = v;
            }
        }
        b
    }
}

fn set(table: &mut toml::Table, path: &[&str], value: toml::Value) {
    let mut t = table;
    for key in &path[..path.len() - 1] {
        t = t
            .entry(key.to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .expect("config section is a table");
    }
    t.insert(path[path.len() - 1].to_string(), value);
}

/// The mima config for one trial: base config, then the profile's extras,
/// then what the harness controls (endpoint, confinement, sandbox, steps).
fn trial_config(
    base: &toml::Table,
    p: &Profile,
    task: &Task,
    dirs: &TrialDirs,
    sandbox: &[String],
    steps: usize,
) -> String {
    let mut c = base.clone();
    merge(&mut c, &p.mima);
    merge(&mut c, &task.mima);
    let s = |v: &str| toml::Value::String(v.to_string());
    set(&mut c, &["provider", "base_url"], s(&p.base_url));
    set(&mut c, &["provider", "default_model"], s(&p.model));
    set(&mut c, &["provider", "api_key"], s("${MIMA_API_KEY}"));
    set(
        &mut c,
        &["agent", "max_steps"],
        toml::Value::Integer(steps as i64),
    );
    set(&mut c, &["agent", "stream"], toml::Value::Boolean(false));
    let work = s(&dirs.work.display().to_string());
    set(
        &mut c,
        &["security", "allowed_paths"],
        toml::Value::Array(vec![work]),
    );
    let wrapper = sandbox.iter().map(|a| s(a)).collect();
    set(
        &mut c,
        &["security", "bash_wrapper"],
        toml::Value::Array(wrapper),
    );
    set(
        &mut c,
        &["session", "transcripts"],
        toml::Value::Boolean(false),
    );
    // mima gives shell commands only an allowlisted environment; pass on the
    // trial settings that commands must see.
    set(
        &mut c,
        &["security", "env_passthrough"],
        toml::Value::Array(
            ["PYTHONDONTWRITEBYTECODE", "GIT_EDITOR"]
                .iter()
                .map(|v| s(v))
                .collect(),
        ),
    );
    toml::to_string(&c).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Readiness

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .expect("http client")
}

fn server_root(base_url: &str) -> String {
    let b = base_url.trim_end_matches('/');
    b.strip_suffix("/v1").unwrap_or(b).to_string()
}

/// Waits until the profile's server is healthy, lists the model, and
/// answers a 1-token completion; then checks tool calling once. Returns a
/// readiness record for run.json.
async fn wait_ready(p: &Profile, style: RequestStyle) -> Result<Value, String> {
    let key = p.api_key.as_deref().map(expand_env).unwrap_or_default();
    let client = http();
    let deadline = Instant::now() + Duration::from_secs(p.ready_timeout_sec);
    let base = p.base_url.trim_end_matches('/').to_string();
    let mut last_err;
    loop {
        match probe(&client, &base, &key, p, style).await {
            Ok(window) => {
                let tools = tool_smoke(&client, &base, &key, p, style).await;
                return Ok(
                    json!({ "ready_at": rfc3339_utc(std::time::SystemTime::now()),
                                  "max_model_len": window, "tool_calls": tools }),
                );
            }
            Err(e) => last_err = e,
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "{} not ready after {}s: {last_err}",
                p.name, p.ready_timeout_sec
            ));
        }
        eprintln!("  waiting for {} ({last_err})", p.name);
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

async fn probe(
    client: &reqwest::Client,
    base: &str,
    key: &str,
    p: &Profile,
    style: RequestStyle,
) -> Result<Option<u64>, String> {
    let health = client
        .get(format!("{}/health", server_root(base)))
        .send()
        .await;
    match health {
        Ok(r) if r.status().as_u16() == 503 => return Err("server unhealthy (503)".into()),
        Err(e) => return Err(format!("unreachable: {e}")),
        _ => {}
    }
    let models: Value = client
        .get(format!("{base}/models"))
        .bearer_auth(key)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let entry = models["data"]
        .as_array()
        .and_then(|d| d.iter().find(|m| m["id"].as_str() == Some(&p.model)))
        .ok_or_else(|| format!("model {} not listed", p.model))?;
    let window = entry["max_model_len"].as_u64();
    // 16 tokens, not 1: Azure OpenAI rejects a reply it cannot finish.
    let r = client
        .post(format!("{base}/chat/completions"))
        .bearer_auth(key)
        .json(&style.body(
            &p.model,
            16,
            json!({ "messages": [{ "role": "user", "content": "Say OK." }] }),
        ))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !r.status().is_success() {
        return Err(format!("completion returned HTTP {}", r.status()));
    }
    Ok(window)
}

/// Tool-calling preflight: five single calls with arguments (how often the
/// server returns a well-formed call with valid JSON arguments), then one
/// round trip (call, tool result, final answer that uses the result).
/// Serving layers can drop or mangle tool calls silently; this records it.
async fn tool_smoke(
    client: &reqwest::Client,
    base: &str,
    key: &str,
    p: &Profile,
    style: RequestStyle,
) -> Value {
    let tools = json!([{ "type": "function", "function": {
        "name": "read_file", "description": "Read a text file.",
        "parameters": { "type": "object", "properties": { "path": { "type": "string" } },
                        "required": ["path"] } } }]);
    let ask = json!({ "role": "user", "content": "Use the read_file tool to read notes.txt, then tell me the code word in it." });
    let post = |body: Value| async move {
        let r = client
            .post(format!("{base}/chat/completions"))
            .bearer_auth(key)
            .json(&body)
            .send()
            .await
            .ok()?;
        r.json::<Value>().await.ok()
    };
    let valid_call = |v: &Value| -> Option<Value> {
        let call = v["choices"][0]["message"]["tool_calls"].get(0)?.clone();
        let args: Value = serde_json::from_str(call["function"]["arguments"].as_str()?).ok()?;
        (call["function"]["name"] == "read_file" && args["path"].is_string()).then_some(call)
    };
    let mut ok = 0;
    let mut first: Option<(Value, Value)> = None;
    for _ in 0..5 {
        let body = style.body(&p.model, 2048, json!({ "tools": tools, "messages": [ask] }));
        if let Some(v) = post(body).await
            && let Some(call) = valid_call(&v)
        {
            ok += 1;
            if first.is_none() {
                first = Some((v["choices"][0]["message"].clone(), call));
            }
        }
    }
    let mut roundtrip = false;
    if let Some((msg, call)) = first {
        let mut assistant = json!({ "role": "assistant", "content": msg["content"].clone(),
                                    "tool_calls": [call.clone()] });
        if assistant["content"].is_null() {
            assistant["content"] = json!("");
        }
        let body = style.body(
            &p.model,
            2048,
            json!({ "tools": tools, "messages": [
                ask, assistant,
                { "role": "tool", "tool_call_id": call["id"], "content": "The code word is PERIWINKLE." }
            ]}),
        );
        roundtrip = post(body).await.is_some_and(|v| {
            v["choices"][0]["message"]["content"]
                .as_str()
                .is_some_and(|c| c.to_uppercase().contains("PERIWINKLE"))
        });
    }
    json!({ "valid_calls": format!("{ok}/5"), "roundtrip": roundtrip })
}

// ---------------------------------------------------------------------------
// Trials

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrialRecord {
    pub run: String,
    pub profile: String,
    pub task: String,
    pub family: String,
    #[serde(default)]
    pub group: String,
    pub kind: String,
    pub trial: usize,
    pub passed: bool,
    pub score: f64,
    pub exit_reason: String,
    pub checks: Vec<CheckResultRecord>,
    pub metrics: Value,
    pub files_changed: usize,
    pub lines_added: u64,
    pub lines_removed: u64,
    pub agent_ms: u64,
    pub check_ms: u64,
    pub infra_retries: u32,
    pub started: String,
    /// Device energy during the trial (agent and checks), when the profile
    /// has a power command.
    #[serde(default)]
    pub energy_j: Option<f64>,
    #[serde(default)]
    pub mean_power_w: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResultRecord {
    pub name: String,
    pub required: bool,
    pub passed: bool,
    pub ms: u64,
    pub detail: String,
}

impl From<&CheckResult> for CheckResultRecord {
    fn from(c: &CheckResult) -> Self {
        Self {
            name: c.name.clone(),
            required: c.required,
            passed: c.passed,
            ms: c.ms,
            detail: c.detail.clone(),
        }
    }
}

pub struct RunOptions {
    pub mima: PathBuf,
    pub base_config: toml::Table,
    pub trials: usize,
    pub sandbox: bool,
    pub keep: bool,
}

/// Prepares a trial directory: fixture copy, git baseline, optional setup.
fn prepare(task: &Task, root: &Path) -> Result<(TrialDirs, String), String> {
    let dirs = TrialDirs::create(root).map_err(|e| format!("trial dir: {e}"))?;
    if let Some(src) = &task.source {
        crate::source::export(src, &crate::source::cache_root(&task.dir), &dirs.work)?;
    }
    if task.fixture().is_dir() {
        exec::copy_tree(&task.fixture(), &dirs.work).map_err(|e| format!("fixture: {e}"))?;
    }
    let baseline = exec::git_baseline(&dirs.work)?;
    if let Some(setup) = &task.setup {
        let env = exec::trial_env(&dirs, &[]);
        let o = exec::run(
            &exec::sh(setup),
            &[],
            &dirs.work,
            &env,
            Duration::from_secs(task.limits.agent_timeout_sec),
        );
        if !o.success() {
            return Err(format!("setup failed: {}", o.stderr.trim()));
        }
    }
    Ok((dirs, baseline))
}

fn run_trial(
    run_id: &str,
    task: &Task,
    p: &Profile,
    trial: usize,
    root: &Path,
    opts: &RunOptions,
) -> TrialRecord {
    let started = rfc3339_utc(std::time::SystemTime::now());
    let mut record = TrialRecord {
        run: run_id.into(),
        profile: p.name.clone(),
        task: task.id.clone(),
        family: task.family.clone(),
        group: task.group.clone(),
        kind: task.kind.clone(),
        trial,
        passed: false,
        score: 0.0,
        exit_reason: String::new(),
        checks: Vec::new(),
        metrics: Value::Null,
        files_changed: 0,
        lines_added: 0,
        lines_removed: 0,
        agent_ms: 0,
        check_ms: 0,
        infra_retries: 0,
        started,
        energy_j: None,
        mean_power_w: None,
    };
    let (dirs, baseline) = match prepare(task, root) {
        Ok(v) => v,
        Err(e) => {
            record.exit_reason = "infra".into();
            record.checks.push(CheckResultRecord {
                name: "setup".into(),
                required: true,
                passed: false,
                ms: 0,
                detail: e,
            });
            return record;
        }
    };
    let mut sandbox = exec::sandbox_prefix(&dirs, opts.sandbox);
    // mima runs its shell commands through its own binary (`mima
    // __sandbox-exec`, Landlock + seccomp inside this bubblewrap), so the
    // binary must be visible in the sandbox, which hides the home directory.
    if let Ok(exe) = opts.mima.canonicalize() {
        exec::add_ro_bind(&mut sandbox, &exe);
    }
    let config_path = dirs.root.join("mima.toml");
    let _ = std::fs::write(
        &config_path,
        trial_config(
            &opts.base_config,
            p,
            task,
            &dirs,
            &sandbox,
            task.limits.max_steps,
        ),
    );
    let transcript = dirs.root.join("transcript.jsonl");
    let mut extra = vec![
        ("MIMA_EVAL_SANDBOX", "1".to_string()),
        ("MIMA_LOG", "info".to_string()),
    ];
    let key = p
        .api_key
        .as_deref()
        .map(expand_env)
        .or_else(|| std::env::var("MIMA_API_KEY").ok());
    extra.push(("MIMA_API_KEY", key.unwrap_or_else(|| "none".into())));
    // Debugging model servers: with MIMA_DUMP_REQUESTS set for the harness,
    // each trial keeps every request mima sent in `requests/`, so a request
    // that crashes the server can be replayed exactly.
    if std::env::var_os("MIMA_DUMP_REQUESTS").is_some() {
        extra.push((
            "MIMA_DUMP_REQUESTS",
            dirs.root.join("requests").display().to_string(),
        ));
    }
    let env = exec::trial_env(&dirs, &extra);
    let argv: Vec<String> = vec![
        opts.mima.display().to_string(),
        "--config".into(),
        config_path.display().to_string(),
        "--approve-all".into(),
        "--transcript-path".into(),
        transcript.display().to_string(),
        "--max-steps".into(),
        task.limits.max_steps.to_string(),
        task.instruction.clone(),
    ];
    let out = exec::run(
        &argv,
        &[],
        &dirs.work,
        &env,
        Duration::from_secs(task.limits.agent_timeout_sec),
    );
    record.agent_ms = out.ms;
    let _ = std::fs::write(dirs.root.join("agent.stdout"), &out.stdout);
    let _ = std::fs::write(dirs.root.join("agent.stderr"), &out.stderr);

    let mut m: Metrics = metrics::from_transcript(&transcript);
    if out.timed_out {
        m.exit_reason = "agent_timeout".into();
    } else if m.exit_reason == "error" && is_transport_error(&m.last_model_error) {
        // mima ended the turn because the model server could not be
        // reached: the server's failure, not the model's. Retried after the
        // server is ready again, and never scored.
        m.exit_reason = "infra".into();
    } else if m.exit_reason.is_empty() {
        let e = &out.stderr;
        m.exit_reason = if e.contains("request to model endpoint failed")
            || e.contains("error sending request")
        {
            "infra".into()
        } else {
            "crash".into()
        };
    }

    let (changed, added, removed, patch) = exec::git_changes(&dirs.work, &baseline, &dirs.root);
    let _ = std::fs::write(dirs.root.join("diff.patch"), patch);
    if let Err(e) = checks::install_hidden(task, &dirs) {
        m.exit_reason = "infra".into();
        record.checks.push(CheckResultRecord {
            name: "install_checks".into(),
            required: true,
            passed: false,
            ms: 0,
            detail: e.to_string(),
        });
    }
    let mut log = String::new();
    let ev = Evidence {
        task,
        dirs: &dirs,
        changed: &changed,
        metrics: &m,
        sandbox: &sandbox,
    };
    let results = checks::run_all(&ev, &mut log);
    let _ = std::fs::write(dirs.root.join("checks.log"), log);

    record.passed = checks::passed(&results) && record.checks.is_empty();
    record.score = checks::score(&results);
    record.check_ms = results.iter().map(|r| r.ms).sum();
    record
        .checks
        .extend(results.iter().map(CheckResultRecord::from));
    record.exit_reason = m.exit_reason.clone();
    record.files_changed = changed.len();
    record.lines_added = added;
    record.lines_removed = removed;
    record.metrics = serde_json::to_value(&m).unwrap_or(Value::Null);
    if record.passed && !opts.keep {
        let _ = std::fs::remove_dir_all(&dirs.work);
    }
    let _ = std::fs::remove_dir_all(&dirs.home);
    let _ = std::fs::remove_dir_all(&dirs.tmp);
    record
}

/// Deterministic shuffle (xorshift) so trial order differs by round but is
/// reproducible.
fn shuffled<T: Clone>(items: &[T], seed: u64) -> Vec<T> {
    let mut v = items.to_vec();
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    for i in (1..v.len()).rev() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.swap(i, (x % (i as u64 + 1)) as usize);
    }
    v
}

/// A model error that means the server was unreachable or dropped the
/// connection (as opposed to rejecting the request).
fn is_transport_error(e: &str) -> bool {
    e.starts_with("request to model endpoint failed")
        || e.contains("error sending request")
        || e.contains("connection refused")
        || e.contains("HTTP 502")
        || e.contains("HTTP 503")
}

fn completed(trials_path: &Path) -> BTreeSet<(String, String, usize)> {
    std::fs::read_to_string(trials_path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<TrialRecord>(l).ok())
        .filter(|r| r.exit_reason != "infra")
        .map(|r| (r.profile, r.task, r.trial))
        .collect()
}

/// Commit and dirty flag of the git repository containing `dir`.
fn git_state(dir: &Path) -> Value {
    let out = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    json!({ "commit": out(&["rev-parse", "HEAD"]),
            "dirty": out(&["status", "--porcelain"]).map(|s| !s.is_empty()) })
}

fn mima_version(mima: &Path) -> String {
    std::process::Command::new(mima)
        .arg("--version")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|e| format!("unavailable: {e}"))
}

/// Runs `suite` against `profiles`, model by model, appending one record per
/// trial to `<run_dir>/trials.jsonl`. Resumes when records already exist.
pub fn run_suite(
    suite: &Suite,
    profiles: &[Profile],
    run_dir: &Path,
    opts: &RunOptions,
) -> Result<(), String> {
    std::fs::create_dir_all(run_dir).map_err(|e| e.to_string())?;
    let run_id = run_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let trials_path = run_dir.join("trials.jsonl");
    let done = completed(&trials_path);
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;

    let mut meta = json!({
        "run": run_id,
        "suite": suite.name,
        "description": suite.description,
        "tasks": suite.tasks.iter().map(|t| json!({ "id": t.id, "family": t.family, "kind": t.kind, "tags": t.tags })).collect::<Vec<_>>(),
        "trials": opts.trials,
        "profiles": profiles,
        "mima": { "path": opts.mima, "version": mima_version(&opts.mima), "git": git_state(opts.mima.parent().unwrap_or(Path::new("."))) },
        "tasks": { "root": suite.root, "git": git_state(&suite.root) },
        "sandbox": if opts.sandbox { "bwrap (network off for commands and checks)" } else { "none" },
        "started": rfc3339_utc(std::time::SystemTime::now()),
        "readiness": {},
    });
    let write_meta = |meta: &Value| {
        let _ = std::fs::write(
            run_dir.join("run.json"),
            serde_json::to_string_pretty(meta).unwrap_or_default(),
        );
    };
    write_meta(&meta);

    let mut out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&trials_path)
        .map_err(|e| e.to_string())?;
    let total = profiles.len() * suite.tasks.len() * opts.trials;
    let mut n = done.len();

    for p in profiles {
        eprintln!("== profile {} ({})", p.name, p.model);
        let style = RequestStyle::of(&opts.base_config, p)?;
        if let Some(setup) = &p.setup {
            eprintln!("  setup: {setup}");
            let st = std::process::Command::new("sh")
                .arg("-c")
                .arg(setup)
                .status();
            if !st.is_ok_and(|s| s.success()) {
                return Err(format!("setup for profile {} failed", p.name));
            }
        }
        let mut ready = rt.block_on(wait_ready(p, style))?;
        if let Some(cmd) = &p.fingerprint {
            let out = std::process::Command::new("sh").arg("-c").arg(cmd).output();
            let text = out
                .map(|o| {
                    String::from_utf8_lossy(&o.stdout)
                        .trim()
                        .chars()
                        .take(4000)
                        .collect::<String>()
                })
                .unwrap_or_else(|e| format!("fingerprint failed: {e}"));
            ready["fingerprint"] = json!(text);
        }
        meta["readiness"][&p.name] = ready;
        meta["settings"][&p.name] = effective_settings(&opts.base_config, p);
        write_meta(&meta);
        let meter = match &p.power {
            Some(cmd) => match crate::power::PowerMeter::start(cmd) {
                Ok(m) => Some(m),
                Err(e) => {
                    eprintln!("  warning: {e}; no energy figures for {}", p.name);
                    None
                }
            },
            None => None,
        };

        for trial in 1..=opts.trials {
            let seed = trial as u64 * 7919 + p.name.len() as u64;
            for task in shuffled(&suite.tasks, seed) {
                if done.contains(&(p.name.clone(), task.id.clone(), trial)) {
                    continue;
                }
                let root = run_dir
                    .join("trials")
                    .join(&p.name)
                    .join(&task.id)
                    .join(format!("t{trial}"));
                let mut retries = 0;
                let rec = loop {
                    let t0 = Instant::now();
                    let mut rec = run_trial(&run_id, &task, p, trial, &root, opts);
                    if let Some(e) = meter.as_ref().and_then(|m| m.energy(t0, Instant::now())) {
                        rec.energy_j = Some(e.joules);
                        rec.mean_power_w = Some(e.mean_watts);
                    }
                    rec.infra_retries = retries;
                    if rec.exit_reason == "infra" && retries < INFRA_RETRIES {
                        retries += 1;
                        // Keep the interrupted attempt (transcript, request
                        // dumps) for diagnosis; the retry gets a fresh dir.
                        let kept = root.with_file_name(format!("t{trial}-infra{retries}"));
                        let _ = std::fs::remove_dir_all(&kept);
                        if let Err(e) = std::fs::rename(&root, &kept) {
                            eprintln!("  could not keep the interrupted attempt: {e}");
                        }
                        eprintln!(
                            "  infra failure on {}#{trial}; waiting for the server",
                            task.id
                        );
                        // A server that does not come back by itself within
                        // a few minutes has crashed: restart it.
                        let quick = Profile {
                            ready_timeout_sec: 180,
                            ..p.clone()
                        };
                        if rt.block_on(wait_ready(&quick, style)).is_err()
                            && let Some(cmd) = p.restart.as_ref().or(p.setup.as_ref())
                        {
                            eprintln!("  server still down; restarting: {cmd}");
                            let ok = std::process::Command::new("sh")
                                .arg("-c")
                                .arg(cmd)
                                .status()
                                .is_ok_and(|s| s.success());
                            let entry = json!({
                                "at": rfc3339_utc(std::time::SystemTime::now()),
                                "task": task.id, "trial": trial, "command_ok": ok,
                            });
                            match meta["server_restarts"][&p.name].as_array_mut() {
                                Some(a) => a.push(entry),
                                None => meta["server_restarts"][&p.name] = json!([entry]),
                            }
                            write_meta(&meta);
                        }
                        rt.block_on(wait_ready(p, style))?;
                        continue;
                    }
                    break rec;
                };
                n += 1;
                let m = &rec.metrics;
                eprintln!(
                    "  [{n}/{total}] {} {:<28} t{trial} {:<4} {:>3} steps {:>6}s  {}",
                    p.name,
                    task.id,
                    if rec.passed { "PASS" } else { "fail" },
                    m["steps"],
                    rec.agent_ms / 1000,
                    rec.exit_reason
                );
                let line = serde_json::to_string(&rec).unwrap_or_default();
                writeln!(out, "{line}").map_err(|e| e.to_string())?;
                out.flush().map_err(|e| e.to_string())?;
            }
        }
    }
    meta["ended"] = json!(rfc3339_utc(std::time::SystemTime::now()));
    write_meta(&meta);
    Ok(())
}

// ---------------------------------------------------------------------------
// Validation

/// How a validation trial treats the starting files.
#[derive(Debug, Clone, PartialEq)]
enum Mode {
    /// Apply the reference solution: every required check must pass.
    Oracle,
    /// Apply the reference solution, then one small code mutation: the
    /// checks should notice (see `strength`).
    Mutant(Mutation),
    /// Change nothing: some required check must fail.
    Null,
    /// A cheating "solution" that leaves the bug in place: some required
    /// check must fail, or the task can be passed without solving it.
    Exploit(Exploit),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Exploit {
    /// Delete the visible tests.
    DeleteTests,
    /// Make every program exit 0 before doing anything (Python, C, shell)
    /// and switch off Rust integration tests.
    EarlyExit,
    /// Answer with every file path and function-like name in the project.
    ListEverything,
    /// Answer with the instruction itself (catches question tasks whose
    /// expected answer appears in the question).
    EchoInstruction,
}

impl Exploit {
    const ALL: [Exploit; 4] = [
        Exploit::DeleteTests,
        Exploit::EarlyExit,
        Exploit::ListEverything,
        Exploit::EchoInstruction,
    ];
    fn name(self) -> &'static str {
        match self {
            Exploit::DeleteTests => "delete-tests",
            Exploit::EarlyExit => "early-exit",
            Exploit::ListEverything => "list-everything",
            Exploit::EchoInstruction => "echo-instruction",
        }
    }
}

/// Validation settings.
pub struct ValidateOpts {
    pub sandbox: bool,
    /// Oracle runs per task; any failure among them marks the task flaky.
    pub repeat: usize,
    /// Also run the cheating baselines.
    pub exploits: bool,
}

/// Checks each task: the reference solution must pass every required check
/// (on every repeat), the untouched fixture must fail at least one, and so
/// must each cheating baseline. Also warns about checked paths that the
/// instruction never mentions. Returns the number of invalid tasks.
pub fn validate(suite: &Suite, work_root: &Path, opts: &ValidateOpts) -> usize {
    let mut bad = 0;
    for task in &suite.tasks {
        let mut problems = Vec::new();
        let reverse_edits = task.source.as_ref().is_some_and(|s| !s.edits.is_empty());
        if !task.solution().is_file() && !reverse_edits {
            problems.push("no solution/solve.sh".to_string());
        } else {
            for i in 0..opts.repeat.max(1) {
                let dir = work_root.join(&task.id).join(format!("oracle{i}"));
                match validate_run(task, &dir, opts.sandbox, Mode::Oracle) {
                    Ok(results) if checks::passed(&results) => {}
                    Ok(results) => {
                        let what = if i == 0 {
                            "oracle fails"
                        } else {
                            "oracle flaky"
                        };
                        problems.push(format!("{what} (run {}): {}", i + 1, failures(&results)));
                        break;
                    }
                    Err(e) => {
                        problems.push(format!("oracle: {e}"));
                        break;
                    }
                }
            }
        }
        match validate_run(
            task,
            &work_root.join(&task.id).join("null"),
            opts.sandbox,
            Mode::Null,
        ) {
            Ok(results) if checks::passed(&results) && !task.expect_fixture_passes => {
                problems.push("untouched fixture already passes every required check".into())
            }
            Ok(results) if !checks::passed(&results) && task.expect_fixture_passes => problems
                .push(format!(
                    "fixture should pass but fails: {}",
                    failures(&results)
                )),
            Ok(_) => {}
            Err(e) => problems.push(format!("null run: {e}")),
        }
        if opts.exploits && !task.expect_fixture_passes {
            for x in Exploit::ALL {
                let dir = work_root.join(&task.id).join(x.name());
                match validate_run(task, &dir, opts.sandbox, Mode::Exploit(x)) {
                    Ok(results) if checks::passed(&results) => problems.push(format!(
                        "cheat \"{}\" passes every required check",
                        x.name()
                    )),
                    Ok(_) => {}
                    Err(e) => problems.push(format!("cheat {}: {e}", x.name())),
                }
            }
        }
        let warnings = lint(task);
        if problems.is_empty() {
            println!("ok    {}", task.id);
        } else {
            bad += 1;
            println!("FAIL  {}: {}", task.id, problems.join("; "));
        }
        for w in warnings {
            println!("warn  {}: {w}", task.id);
        }
    }
    bad
}

/// Terminal-Bench's rule: what is checked must be stated. Flags checked
/// file paths that the instruction does not mention (by name).
fn lint(task: &Task) -> Vec<String> {
    let text = task.instruction.to_lowercase();
    let mut out = Vec::new();
    for c in &task.checks {
        let path = match &c.kind {
            crate::task::CheckKind::FileExists { path }
            | crate::task::CheckKind::FileAbsent { path }
            | crate::task::CheckKind::FileContains { path, .. }
            | crate::task::CheckKind::FileLacks { path, .. } => path,
            _ => continue,
        };
        let name = path.rsplit('/').next().unwrap_or(path).to_lowercase();
        if !text.contains(&name) {
            out.push(format!(
                "check {:?} looks at {path}, which the instruction never mentions",
                c.name
            ));
        }
    }
    out
}

fn failures(results: &[CheckResult]) -> String {
    results
        .iter()
        .filter(|r| r.required && !r.passed)
        .map(|r| format!("{} ({})", r.name, r.detail))
        .collect::<Vec<_>>()
        .join(", ")
}

fn validate_run(
    task: &Task,
    root: &Path,
    sandbox_on: bool,
    mode: Mode,
) -> Result<Vec<CheckResult>, String> {
    validate_run_changed(task, root, sandbox_on, mode).map(|(r, _)| r)
}

/// `validate_run`, also returning the paths changed from the fixture.
fn validate_run_changed(
    task: &Task,
    root: &Path,
    sandbox_on: bool,
    mode: Mode,
) -> Result<(Vec<CheckResult>, Vec<String>), String> {
    let (dirs, baseline) = prepare(task, root)?;
    let sandbox = exec::sandbox_prefix(&dirs, sandbox_on);
    let mut m = Metrics::default();
    let oracle = matches!(mode, Mode::Oracle | Mode::Mutant(_));
    match mode {
        _ if oracle && !task.solution().is_file() => {
            // Source tasks: the reference solution is the bug edits reversed.
            let edits = task
                .source
                .as_ref()
                .map(|s| s.edits.as_slice())
                .unwrap_or_default();
            crate::source::apply(edits, &dirs.work, true)?;
        }
        _ if oracle => {
            let env = exec::trial_env(&dirs, &[]);
            // Copy the solution into the trial directory: the sandbox shows
            // only that directory.
            let script = dirs.root.join("solve.sh");
            std::fs::copy(task.solution(), &script).map_err(|e| format!("solve.sh: {e}"))?;
            let o = exec::run(
                &["sh".to_string(), script.display().to_string()],
                &sandbox,
                &dirs.work,
                &env,
                Duration::from_secs(task.limits.agent_timeout_sec),
            );
            if !o.success() {
                return Err(format!("solve.sh exit {:?}: {}", o.code, o.stderr.trim()));
            }
            // Question-answering tasks keep their expected answer here.
            m.final_answer = std::fs::read_to_string(task.dir.join("solution").join("answer.txt"))
                .unwrap_or_default();
        }
        Mode::Null | Mode::Oracle | Mode::Mutant(_) => {}
        Mode::Exploit(Exploit::EchoInstruction) => {
            m.final_answer = task.instruction.clone();
        }
        Mode::Exploit(x) => {
            m.final_answer = crate::exploit::apply(x.name(), &dirs.work)
                .map_err(|e| format!("{}: {e}", x.name()))?;
        }
    }
    if let Mode::Mutant(mu) = &mode {
        mu.apply(&dirs.work)?;
    }
    let (changed, ..) = exec::git_changes(&dirs.work, &baseline, &dirs.root);
    checks::install_hidden(task, &dirs).map_err(|e| e.to_string())?;
    let mut log = String::new();
    let ev = Evidence {
        task,
        dirs: &dirs,
        changed: &changed,
        metrics: &m,
        sandbox: &sandbox,
    };
    let results = checks::run_all(&ev, &mut log);
    let _ = std::fs::write(dirs.root.join("checks.log"), log);
    Ok((results, changed))
}

pub fn default_run_dir(out: &Path, suite: &str) -> PathBuf {
    let ts: String = rfc3339_utc(std::time::SystemTime::now())
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(13)
        .collect(); // e.g. 20260928T1530
    out.join(format!("{ts}Z-{suite}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_style_follows_the_profile() {
        let mut p = profile_from_config(&toml::Table::new());
        let base = toml::Table::new();
        let local = RequestStyle::of(&base, &p).unwrap();
        let b = local.body("m", 16, json!({ "temperature": 0.0, "messages": [] }));
        assert_eq!(b["max_tokens"], 16);
        assert_eq!(b["temperature"], 0.0);

        p.mima = toml::from_str(
            "[agent]\nmax_tokens_param = \"max_completion_tokens\"\ntemperature = \"server\"",
        )
        .unwrap();
        let cloud = RequestStyle::of(&base, &p).unwrap();
        let b = cloud.body("m", 16, json!({ "temperature": 0.0, "messages": [] }));
        assert_eq!(b["max_completion_tokens"], 16);
        assert!(b.get("max_tokens").is_none() && b.get("temperature").is_none());

        p.mima = toml::from_str("[agent]\nmax_tokens_param = \"max_token\"").unwrap();
        assert!(RequestStyle::of(&base, &p).is_err());
    }

    #[test]
    fn merge_and_set_build_the_trial_config() {
        let mut base: toml::Table =
            toml::from_str("[agent]\ntemperature = 0.2\nmax_tokens = 4096").unwrap();
        let over: toml::Table = toml::from_str("[agent]\ntemperature = 0.6").unwrap();
        merge(&mut base, &over);
        set(
            &mut base,
            &["security", "allowed_paths"],
            toml::Value::Array(vec![]),
        );
        assert_eq!(base["agent"]["temperature"].as_float(), Some(0.6));
        assert_eq!(base["agent"]["max_tokens"].as_integer(), Some(4096));
        assert!(base["security"]["allowed_paths"].is_array());
    }

    #[test]
    fn shuffle_is_a_deterministic_permutation() {
        let v: Vec<u32> = (0..10).collect();
        let a = shuffled(&v, 3);
        assert_eq!(a, shuffled(&v, 3));
        let mut s = a.clone();
        s.sort();
        assert_eq!(s, v);
        assert_ne!(a, shuffled(&v, 4));
    }

    #[test]
    fn expands_env_references() {
        // SAFETY: test-only env mutation.
        unsafe { std::env::set_var("MIMA_EVAL_T", "k") };
        assert_eq!(expand_env("a${MIMA_EVAL_T}b"), "akb");
        assert_eq!(expand_env("${UNSET_MIMA_EVAL_X}"), "");
    }
}

/// The sampling and window settings a profile's trials actually use (mima's
/// defaults where nothing overrides them), recorded in run.json so that
/// comparisons can show when models ran under different settings.
fn effective_settings(base: &toml::Table, p: &Profile) -> Value {
    let mut c = base.clone();
    merge(&mut c, &p.mima);
    let get = |sec: &str, key: &str| c.get(sec).and_then(|t| t.get(key)).cloned();
    let num = |v: Option<toml::Value>| match v {
        Some(toml::Value::Integer(i)) => json!(i),
        Some(toml::Value::Float(f)) => json!(f),
        _ => Value::Null,
    };
    let or = |v: Value, d: Value| if v.is_null() { d } else { v };
    json!({
        "temperature": match get("agent", "temperature") {
            Some(toml::Value::String(w)) => json!(w),
            v => or(num(v), json!(0.2)),
        },
        "top_p": num(get("agent", "top_p")),
        "top_k": num(get("agent", "top_k")),
        "min_p": num(get("agent", "min_p")),
        "extra_body": get("agent", "extra_body").and_then(|v| serde_json::to_value(v).ok()),
        "max_tokens": or(num(get("agent", "max_tokens")), json!(4096)),
        "max_tokens_param": get("agent", "max_tokens_param").and_then(|v| v.as_str().map(String::from)).unwrap_or_else(|| "max_tokens".into()),
        "window": num(get("context", "window")),
        "tool_calling": get("agent", "tool_calling").and_then(|v| v.as_str().map(String::from)).unwrap_or_else(|| "auto".into()),
    })
}

// ---------------------------------------------------------------------------
// Check strength (mutation testing of the checks)

/// One small code change applied on top of the reference solution.
#[derive(Debug, Clone, PartialEq)]
pub struct Mutation {
    file: String,
    /// Byte offset of `from` in the file.
    at: usize,
    from: &'static str,
    to: &'static str,
    line: usize,
}

impl Mutation {
    fn apply(&self, work: &Path) -> Result<(), String> {
        let path = work.join(&self.file);
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", self.file))?;
        if text.get(self.at..self.at + self.from.len()) != Some(self.from) {
            return Err(format!("{}: mutation site moved", self.file));
        }
        let new = format!(
            "{}{}{}",
            &text[..self.at],
            self.to,
            &text[self.at + self.from.len()..]
        );
        std::fs::write(&path, new).map_err(|e| format!("{}: {e}", self.file))
    }
}

/// Classic mutation operators (relational, logical, arithmetic, boolean,
/// off-by-one). Longer patterns first so "<=" is not also seen as "<".
const OPERATORS: &[(&str, &str)] = &[
    (" <= ", " < "),
    (" >= ", " > "),
    (" < ", " <= "),
    (" > ", " >= "),
    (" == ", " != "),
    (" != ", " == "),
    (" && ", " || "),
    (" || ", " && "),
    (" and ", " or "),
    (" or ", " and "),
    (" + 1", " + 2"),
    (" - 1", " - 2"),
    (" + ", " - "),
    (" - ", " + "),
    ("True", "False"),
    ("False", "True"),
    ("true", "false"),
    ("false", "true"),
];

fn is_code(file: &str) -> bool {
    [".c", ".h", ".py", ".rs", ".sh", ".js", ".go"]
        .iter()
        .any(|e| file.ends_with(e))
}

/// Mutation sites in `text`, skipping comment lines; at most `max`, spread
/// evenly through the file (deterministic).
fn mutations(file: &str, text: &str, max: usize) -> Vec<Mutation> {
    let mut all = Vec::new();
    let mut offset = 0;
    let mut in_docstring = false;
    for (n, line) in text.split_inclusive('\n').enumerate() {
        let t = line.trim_start();
        // Python docstrings: lines inside (or opening/closing) a """ block.
        let triple = line.matches("\"\"\"").count() + line.matches("'''").count();
        let docstring_line = in_docstring || triple > 0;
        if triple % 2 == 1 {
            in_docstring = !in_docstring;
        }
        let comment = ["//", "#", "*", "/*"].iter().any(|c| t.starts_with(c));
        if !comment && !docstring_line {
            let mut taken: Vec<(usize, usize)> = Vec::new();
            for (from, to) in OPERATORS {
                for (i, _) in line.match_indices(from) {
                    if taken.iter().any(|(a, b)| i < *b && i + from.len() > *a)
                        || in_string_or_comment(&line[..i])
                    {
                        continue;
                    }
                    taken.push((i, i + from.len()));
                    all.push(Mutation {
                        file: file.to_string(),
                        at: offset + i,
                        from,
                        to,
                        line: n + 1,
                    });
                }
            }
        }
        offset += line.len();
    }
    if all.len() <= max {
        return all;
    }
    (0..max).map(|i| all[i * all.len() / max].clone()).collect()
}

/// Whether a position, given the line text before it, is inside a string
/// literal or after a trailing comment (approximate: no escapes across
/// lines, no raw strings).
fn in_string_or_comment(before: &str) -> bool {
    let (mut quote, mut escaped) = (None::<char>, false);
    let chars: Vec<char> = before.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        match quote {
            Some(q) if c == '\\' => {
                let _ = q;
                escaped = true;
            }
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '#' => return true,
            None if c == '/' && chars.get(i + 1) == Some(&'/') => return true,
            None => {}
        }
    }
    quote.is_some()
}

/// For each task: apply the reference solution, then one mutation at a time
/// in the files the solution changed, and count how many mutants the
/// required checks reject. Low kill rates mean the checks would accept
/// wrong solutions (UTBoost, ABC O.d.2). Mutants that fail to compile count
/// as killed and equivalent mutants cannot be told apart, so read the
/// survivors rather than only the rate.
pub fn strength(suite: &Suite, work_root: &Path, sandbox_on: bool, max: usize) -> String {
    let mut out = String::from(
        "| Task | Mutants | Killed | Kill rate | Survivors (file:line from -> to) |\n|---|---|---|---|---|\n",
    );
    let (mut total, mut killed_total) = (0, 0);
    for task in &suite.tasks {
        if task.expect_fixture_passes {
            continue;
        }
        let root = work_root.join(&task.id);
        let changed =
            match validate_run_changed(task, &root.join("oracle"), sandbox_on, Mode::Oracle) {
                Ok((r, changed)) if checks::passed(&r) => changed,
                Ok(_) | Err(_) => {
                    out.push_str(&format!("| {} | - | - | oracle fails | |\n", task.id));
                    continue;
                }
            };
        let work = root.join("oracle").join("work");
        let mut ms = Vec::new();
        let code: Vec<&String> = changed.iter().filter(|f| is_code(f)).collect();
        for f in &code {
            if let Ok(text) = std::fs::read_to_string(work.join(f)) {
                ms.extend(mutations(f, &text, max.div_ceil(code.len().max(1))));
            }
        }
        ms.truncate(max);
        if ms.is_empty() {
            out.push_str(&format!(
                "| {} | 0 | - | - | no mutable code changed |\n",
                task.id
            ));
            continue;
        }
        let mut killed = 0;
        let mut survivors = Vec::new();
        for (i, mu) in ms.iter().enumerate() {
            let dir = root.join(format!("m{i}"));
            let dead = match validate_run(task, &dir, sandbox_on, Mode::Mutant(mu.clone())) {
                Ok(r) => !checks::passed(&r),
                Err(_) => true,
            };
            let _ = std::fs::remove_dir_all(&dir);
            if dead {
                killed += 1;
            } else {
                survivors.push(format!(
                    "{}:{} `{}`->`{}`",
                    mu.file,
                    mu.line,
                    mu.from.trim(),
                    mu.to.trim()
                ));
            }
        }
        total += ms.len();
        killed_total += killed;
        out.push_str(&format!(
            "| {} | {} | {killed} | {:.0}% | {} |\n",
            task.id,
            ms.len(),
            100.0 * killed as f64 / ms.len() as f64,
            survivors.join("; ")
        ));
        eprintln!("  {}: {killed}/{} mutants killed", task.id, ms.len());
    }
    if total > 0 {
        out.push_str(&format!(
            "\nOverall: {killed_total}/{total} mutants killed ({:.0}%).\n",
            100.0 * killed_total as f64 / total as f64
        ));
    }
    out
}

#[cfg(test)]
mod strength_tests {
    use super::*;

    #[test]
    fn mutation_sites_skip_comments_and_overlaps() {
        let text = "# a <= b\nif a <= b and c:  # x == y\n    x = y + 1\n    s = \"a and b\"\n    \"\"\"doc a or b\n    c and d\n    \"\"\"\n";
        let ms = mutations("m.py", text, 50);
        let kinds: Vec<(&str, usize)> = ms.iter().map(|m| (m.from, m.line)).collect();
        assert_eq!(kinds, vec![(" <= ", 2), (" and ", 2), (" + 1", 3)]);
        let d = std::env::temp_dir().join(format!("mima-mut-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("m.py"), text).unwrap();
        ms[0].apply(&d).unwrap();
        assert!(
            std::fs::read_to_string(d.join("m.py"))
                .unwrap()
                .contains("if a < b and c")
        );
        assert_eq!(mutations("m.py", text, 2).len(), 2);
    }
}
