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
        mima: toml::Table::new(),
    }
}

fn expand_env(s: &str) -> String {
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
    dirs: &TrialDirs,
    sandbox: &[String],
    steps: usize,
) -> String {
    let mut c = base.clone();
    merge(&mut c, &p.mima);
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
async fn wait_ready(p: &Profile) -> Result<Value, String> {
    let key = p.api_key.as_deref().map(expand_env).unwrap_or_default();
    let client = http();
    let deadline = Instant::now() + Duration::from_secs(p.ready_timeout_sec);
    let base = p.base_url.trim_end_matches('/').to_string();
    let mut last_err;
    loop {
        match probe(&client, &base, &key, p).await {
            Ok(window) => {
                let tools = tool_smoke(&client, &base, &key, p).await;
                return Ok(
                    json!({ "ready_at": rfc3339_utc(std::time::SystemTime::now()),
                                  "max_model_len": window, "tool_call_smoke": tools }),
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
    let r = client
        .post(format!("{base}/chat/completions"))
        .bearer_auth(key)
        .json(&json!({ "model": p.model, "max_tokens": 1,
                       "messages": [{ "role": "user", "content": "Say OK." }] }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !r.status().is_success() {
        return Err(format!("completion returned HTTP {}", r.status()));
    }
    Ok(window)
}

async fn tool_smoke(client: &reqwest::Client, base: &str, key: &str, p: &Profile) -> bool {
    let body = json!({
        "model": p.model, "max_tokens": 1024,
        "messages": [{ "role": "user", "content": "Call the ping tool now." }],
        "tools": [{ "type": "function", "function": { "name": "ping", "description": "Ping.",
                    "parameters": { "type": "object", "properties": {} } } }]
    });
    let Ok(r) = client
        .post(format!("{base}/chat/completions"))
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
    else {
        return false;
    };
    let v: Value = r.json().await.unwrap_or_default();
    v["choices"][0]["message"]["tool_calls"]
        .as_array()
        .is_some_and(|a| !a.is_empty())
}

// ---------------------------------------------------------------------------
// Trials

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrialRecord {
    pub run: String,
    pub profile: String,
    pub task: String,
    pub family: String,
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
    let sandbox = exec::sandbox_prefix(&dirs, opts.sandbox);
    let config_path = dirs.root.join("mima.toml");
    let _ = std::fs::write(
        &config_path,
        trial_config(&opts.base_config, p, &dirs, &sandbox, task.limits.max_steps),
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

fn completed(trials_path: &Path) -> BTreeSet<(String, String, usize)> {
    std::fs::read_to_string(trials_path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<TrialRecord>(l).ok())
        .filter(|r| r.exit_reason != "infra")
        .map(|r| (r.profile, r.task, r.trial))
        .collect()
}

fn git_state() -> Value {
    let out = |args: &[&str]| {
        std::process::Command::new("git")
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
        "mima": { "path": opts.mima, "version": mima_version(&opts.mima), "git": git_state() },
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
        let ready = rt.block_on(wait_ready(p))?;
        meta["readiness"][&p.name] = ready;
        write_meta(&meta);

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
                    let mut rec = run_trial(&run_id, &task, p, trial, &root, opts);
                    rec.infra_retries = retries;
                    if rec.exit_reason == "infra" && retries < INFRA_RETRIES {
                        retries += 1;
                        eprintln!(
                            "  infra failure on {}#{trial}; waiting for the server",
                            task.id
                        );
                        rt.block_on(wait_ready(p))?;
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

/// Checks each task: the reference solution must pass every required check,
/// and the untouched fixture must fail at least one. Returns the number of
/// invalid tasks.
pub fn validate(suite: &Suite, work_root: &Path, sandbox_on: bool) -> usize {
    let mut bad = 0;
    for task in &suite.tasks {
        let mut problems = Vec::new();
        if !task.solution().is_file() {
            problems.push("no solution/solve.sh".to_string());
        } else {
            match validate_run(
                task,
                &work_root.join(&task.id).join("oracle"),
                sandbox_on,
                true,
            ) {
                Ok(results) if checks::passed(&results) => {}
                Ok(results) => problems.push(format!("oracle fails: {}", failures(&results))),
                Err(e) => problems.push(format!("oracle: {e}")),
            }
        }
        match validate_run(
            task,
            &work_root.join(&task.id).join("null"),
            sandbox_on,
            false,
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
        if problems.is_empty() {
            println!("ok    {}", task.id);
        } else {
            bad += 1;
            println!("FAIL  {}: {}", task.id, problems.join("; "));
        }
    }
    bad
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
    oracle: bool,
) -> Result<Vec<CheckResult>, String> {
    let (dirs, baseline) = prepare(task, root)?;
    let sandbox = exec::sandbox_prefix(&dirs, sandbox_on);
    let mut m = Metrics::default();
    if oracle {
        let env = exec::trial_env(&dirs, &[]);
        // Copy the solution into the trial directory: the sandbox shows only
        // that directory (the task itself may be under a hidden /tmp).
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
    Ok(results)
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
