//! Evaluating a task's checks against a finished trial.

use serde::Serialize;
use std::collections::BTreeMap;
use std::time::Duration;

use crate::exec;
use crate::metrics::Metrics;
use crate::task::{Check, CheckKind, Task, glob_match};

#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    pub name: String,
    pub required: bool,
    pub passed: bool,
    pub ms: u64,
    /// Short explanation on failure (command output tail, first difference).
    pub detail: String,
}

/// What checks can look at.
pub struct Evidence<'a> {
    pub task: &'a Task,
    pub dirs: &'a exec::TrialDirs,
    pub changed: &'a [String],
    pub metrics: &'a Metrics,
    pub sandbox: &'a [String],
}

/// Runs every check, recording the full command output in `log`.
pub fn run_all(ev: &Evidence, log: &mut String) -> Vec<CheckResult> {
    ev.task.checks.iter().map(|c| run_one(c, ev, log)).collect()
}

fn run_one(check: &Check, ev: &Evidence, log: &mut String) -> CheckResult {
    let started = std::time::Instant::now();
    let (passed, detail) = evaluate(&check.kind, ev, log, &check.name);
    CheckResult {
        name: check.name.clone(),
        required: check.required,
        passed,
        ms: started.elapsed().as_millis() as u64,
        detail,
    }
}

fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn tail(s: &str, lines: usize) -> String {
    let v: Vec<&str> = s.trim_end().lines().collect();
    v[v.len().saturating_sub(lines)..].join("\n")
}

fn command(run: &str, ev: &Evidence, log: &mut String, name: &str) -> exec::Output {
    let env: BTreeMap<String, String> =
        exec::trial_env(ev.dirs, &[("CHECKS", ev.dirs.checks.display().to_string())]);
    let o = exec::run(
        &exec::sh(run),
        ev.sandbox,
        &ev.dirs.work,
        &env,
        Duration::from_secs(ev.task.limits.check_timeout_sec),
    );
    log.push_str(&format!(
        "=== check {name}: {run}\nexit: {:?}{}\n--- stdout\n{}\n--- stderr\n{}\n",
        o.code,
        if o.timed_out { " (timed out)" } else { "" },
        o.stdout,
        o.stderr
    ));
    o
}

fn evaluate(kind: &CheckKind, ev: &Evidence, log: &mut String, name: &str) -> (bool, String) {
    let work = &ev.dirs.work;
    let read = |p: &str| std::fs::read_to_string(work.join(p));
    match kind {
        CheckKind::Command { run } => {
            let o = command(run, ev, log, name);
            if o.success() {
                (true, String::new())
            } else if o.timed_out {
                (false, "timed out".into())
            } else {
                let out = if o.stderr.trim().is_empty() {
                    &o.stdout
                } else {
                    &o.stderr
                };
                (false, format!("exit {:?}: {}", o.code, tail(out, 3)))
            }
        }
        CheckKind::OutputEquals { run, expected } => {
            let o = command(run, ev, log, name);
            let (got, want) = (norm(&o.stdout), norm(expected));
            if o.success() && got == want {
                (true, String::new())
            } else {
                (
                    false,
                    format!(
                        "expected {want:?}, got {:?} (exit {:?})",
                        tail(&got, 1),
                        o.code
                    ),
                )
            }
        }
        CheckKind::FileExists { path } => {
            (work.join(path).exists(), format!("{path} does not exist"))
        }
        CheckKind::FileAbsent { path } => (!work.join(path).exists(), format!("{path} exists")),
        CheckKind::FileContains { path, text } => match read(path) {
            Ok(c) => (c.contains(text.as_str()), format!("{path} lacks {text:?}")),
            Err(e) => (false, format!("{path}: {e}")),
        },
        CheckKind::FileLacks { path, text } => match read(path) {
            Ok(c) => (
                !c.contains(text.as_str()),
                format!("{path} still contains {text:?}"),
            ),
            Err(e) => (false, format!("{path}: {e}")),
        },
        CheckKind::Unchanged { paths } => {
            for p in paths {
                if let Err(e) = exec::unchanged(&ev.task.fixture(), work, p) {
                    return (false, e);
                }
            }
            (true, String::new())
        }
        CheckKind::OnlyChanged { paths } => {
            let outside: Vec<&String> = ev
                .changed
                .iter()
                .filter(|f| !paths.iter().any(|p| glob_match(p, f)))
                .collect();
            if outside.is_empty() {
                (true, String::new())
            } else {
                (
                    false,
                    format!(
                        "also changed: {}",
                        outside
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                )
            }
        }
        CheckKind::FinalAnswer { contains } => {
            let answer = norm(&ev.metrics.final_answer).to_lowercase();
            let want = norm(contains).to_lowercase();
            (
                answer.contains(&want),
                format!("final answer lacks {contains:?}"),
            )
        }
        CheckKind::Agent {
            max_steps,
            no_loop_guard,
        } => {
            let m = ev.metrics;
            if let Some(max) = max_steps
                && m.steps > *max as u64
            {
                return (false, format!("{} steps > {max}", m.steps));
            }
            if *no_loop_guard && m.loop_guard_trips > 0 {
                return (false, "loop guard tripped".into());
            }
            (true, String::new())
        }
    }
}

/// A trial passes when every required check passed.
pub fn passed(results: &[CheckResult]) -> bool {
    results.iter().filter(|r| r.required).all(|r| r.passed)
}

/// Fraction of all checks passed (secondary, partial-credit score).
pub fn score(results: &[CheckResult]) -> f64 {
    if results.is_empty() {
        return 0.0;
    }
    results.iter().filter(|r| r.passed).count() as f64 / results.len() as f64
}

/// Copies hidden checks into the trial (after the agent has finished).
pub fn install_hidden(task: &Task, dirs: &exec::TrialDirs) -> std::io::Result<()> {
    let src = task.hidden_checks();
    if src.is_dir() {
        exec::copy_tree(&src, &dirs.checks)?;
    } else {
        std::fs::create_dir_all(&dirs.checks)?;
    }
    Ok(())
}
