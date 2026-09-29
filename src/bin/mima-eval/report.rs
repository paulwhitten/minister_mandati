//! `summary.md` and `compare.md` from a run's `trials.jsonl`.

use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::Path;

use crate::run::TrialRecord;
use crate::stats::{self, TaskResult};

fn pct(x: f64) -> String {
    if x.is_nan() {
        "n/a".into()
    } else {
        format!("{:.0}%", x * 100.0)
    }
}

fn pts(x: f64) -> String {
    if x.is_nan() {
        "n/a".into()
    } else {
        format!("{:+.0}", x * 100.0)
    }
}

/// Writes both reports into `run_dir`; returns the summary text.
pub fn write(run_dir: &Path) -> Result<String, String> {
    let meta: Value = serde_json::from_str(
        &std::fs::read_to_string(run_dir.join("run.json")).map_err(|e| format!("run.json: {e}"))?,
    )
    .map_err(|e| format!("run.json: {e}"))?;
    let records: Vec<TrialRecord> = std::fs::read_to_string(run_dir.join("trials.jsonl"))
        .map_err(|e| format!("trials.jsonl: {e}"))?
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    if records.is_empty() {
        return Err("no trials recorded yet".into());
    }
    // Infra failures are excluded (and counted) so they never score as fails.
    let infra = records.iter().filter(|r| r.exit_reason == "infra").count();
    let scored: Vec<&TrialRecord> = records
        .iter()
        .filter(|r| r.exit_reason != "infra")
        .collect();

    let mut by_profile: BTreeMap<&str, Vec<&TrialRecord>> = BTreeMap::new();
    for r in &scored {
        by_profile.entry(r.profile.as_str()).or_default().push(r);
    }
    let profiles: Vec<&str> = meta["profiles"]
        .as_array()
        .map(|a| a.iter().filter_map(|p| p["name"].as_str()).collect())
        .unwrap_or_else(|| by_profile.keys().copied().collect());
    let results: BTreeMap<&str, Vec<TaskResult>> = profiles
        .iter()
        .map(|p| {
            (
                *p,
                task_results(by_profile.get(p).map_or(&[][..], Vec::as_slice)),
            )
        })
        .collect();

    let summary = summary(&meta, &profiles, &by_profile, &results, infra);
    std::fs::write(run_dir.join("summary.md"), &summary).map_err(|e| e.to_string())?;
    if profiles.len() > 1 {
        std::fs::write(run_dir.join("compare.md"), compare(&profiles, &results))
            .map_err(|e| e.to_string())?;
    }
    Ok(summary)
}

fn task_results(records: &[&TrialRecord]) -> Vec<TaskResult> {
    let mut map: BTreeMap<&str, TaskResult> = BTreeMap::new();
    for r in records {
        let t = map.entry(r.task.as_str()).or_insert_with(|| TaskResult {
            task: r.task.clone(),
            family: r.family.clone(),
            c: 0,
            k: 0,
        });
        t.k += 1;
        t.c += r.passed as usize;
    }
    map.into_values().collect()
}

fn summary(
    meta: &Value,
    profiles: &[&str],
    by_profile: &BTreeMap<&str, Vec<&TrialRecord>>,
    results: &BTreeMap<&str, Vec<TaskResult>>,
    infra: usize,
) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "# mima eval: {}\n",
        meta["suite"].as_str().unwrap_or("?")
    );
    let _ = writeln!(
        s,
        "Run `{}`, started {}, {} trials per task. mima {} (commit {}{}). Sandbox: {}.\n",
        meta["run"].as_str().unwrap_or("?"),
        meta["started"].as_str().unwrap_or("?"),
        meta["trials"],
        meta["mima"]["version"].as_str().unwrap_or("?"),
        meta["mima"]["git"]["commit"]
            .as_str()
            .map_or("?", |c| &c[..c.len().min(8)]),
        if meta["mima"]["git"]["dirty"].as_bool() == Some(true) {
            ", uncommitted changes"
        } else {
            ""
        },
        meta["sandbox"].as_str().unwrap_or("?"),
    );
    if infra > 0 {
        let _ = writeln!(
            s,
            "{infra} trial(s) failed for infrastructure reasons and are excluded.\n"
        );
    }

    let _ = writeln!(s, "## Results\n");
    let _ = writeln!(
        s,
        "Pass rate is the mean of per-task pass rates, with a standard error clustered by task family and a 95% t interval. pass@k is the chance at least one of k tries passes; pass^k the chance all k pass.\n"
    );
    let _ = writeln!(
        s,
        "| Model | Pass rate | ± SE | 95% CI | Tasks (families) | pass@k | pass^k | Always / mixed / never |"
    );
    let _ = writeln!(s, "|---|---|---|---|---|---|---|---|");
    for p in profiles {
        let tr = &results[p];
        if tr.is_empty() {
            continue;
        }
        let e = stats::suite(tr);
        let k = tr.iter().map(|t| t.k).min().unwrap_or(0);
        let hat = tr
            .iter()
            .map(|t| stats::pass_hat_k(t.c, t.k, k))
            .sum::<f64>()
            / tr.len() as f64;
        let at = tr
            .iter()
            .map(|t| stats::pass_at_k(t.c, t.k, k))
            .sum::<f64>()
            / tr.len() as f64;
        let always = tr.iter().filter(|t| t.c == t.k).count();
        let never = tr.iter().filter(|t| t.c == 0).count();
        let _ = writeln!(
            s,
            "| {p} | {} | {} | {} – {} | {} ({}) | {} | {} (k={k}) | {always} / {} / {never} |",
            pct(e.mean),
            pct(e.se),
            pct(e.lo),
            pct(e.hi),
            e.n,
            e.clusters,
            pct(at),
            pct(hat),
            tr.len() - always - never
        );
    }

    let _ = writeln!(s, "\n## Behavior (all trials; medians)\n");
    let _ = writeln!(
        s,
        "| Model | Steps | Prompt tok | Completion tok | Peak context | Agent time | Tool errors | Edit tolerances | Compactions | Exit reasons |"
    );
    let _ = writeln!(s, "|---|---|---|---|---|---|---|---|---|---|");
    for p in profiles {
        let Some(rs) = by_profile.get(p) else {
            continue;
        };
        let med = |f: &dyn Fn(&TrialRecord) -> f64| {
            let v: Vec<f64> = rs.iter().map(|r| f(r)).collect();
            stats::median(&v)
        };
        let num = |r: &TrialRecord, k: &str| r.metrics[k].as_f64().unwrap_or(0.0);
        let sum = |k: &str| {
            rs.iter()
                .map(|r| r.metrics[k].as_u64().unwrap_or(0))
                .sum::<u64>()
        };
        let mut errors: BTreeMap<String, u64> = BTreeMap::new();
        let mut exits: BTreeMap<&str, u64> = BTreeMap::new();
        for r in rs {
            if let Some(m) = r.metrics["tool_errors"].as_object() {
                for (k, v) in m {
                    *errors.entry(k.clone()).or_default() += v.as_u64().unwrap_or(0);
                }
            }
            *exits.entry(r.exit_reason.as_str()).or_default() += 1;
        }
        let join = |m: Vec<String>| {
            if m.is_empty() {
                "-".into()
            } else {
                m.join(", ")
            }
        };
        let _ = writeln!(
            s,
            "| {p} | {:.0} | {:.0} | {:.0} | {:.0} | {:.0}s | {} | {} | {} | {} |",
            med(&|r| num(r, "steps")),
            med(&|r| num(r, "prompt_tokens")),
            med(&|r| num(r, "completion_tokens")),
            med(&|r| num(r, "peak_context")),
            med(&|r| r.agent_ms as f64 / 1000.0),
            join(errors.iter().map(|(k, v)| format!("{k} {v}")).collect()),
            sum("edit_tolerances"),
            sum("compactions"),
            join(exits.iter().map(|(k, v)| format!("{k} {v}")).collect()),
        );
    }

    let _ = writeln!(s, "\n## Per task\n");
    let _ = writeln!(
        s,
        "Passes out of trials, with a 95% Wilson interval. With few trials these intervals are wide; read single tasks with care.\n"
    );
    let _ = write!(s, "| Task | Family |");
    for p in profiles {
        let _ = write!(s, " {p} |");
    }
    let _ = writeln!(s);
    let _ = writeln!(s, "|---|---|{}", "---|".repeat(profiles.len()));
    let mut tasks: Vec<(&str, &str)> = results
        .values()
        .flatten()
        .map(|t| (t.task.as_str(), t.family.as_str()))
        .collect();
    tasks.sort();
    tasks.dedup();
    for (task, family) in tasks {
        let _ = write!(s, "| {task} | {family} |");
        for p in profiles {
            match results[p].iter().find(|t| t.task == task) {
                Some(t) => {
                    let (lo, hi) = stats::wilson(t.c, t.k);
                    let _ = write!(s, " {}/{} ({}–{}) |", t.c, t.k, pct(lo), pct(hi));
                }
                None => {
                    let _ = write!(s, " - |");
                }
            }
        }
        let _ = writeln!(s);
    }
    let _ = writeln!(
        s,
        "\nFailing trials keep their work directory, transcript, diff and check output under `trials/<model>/<task>/t<n>/`. Read transcripts before trusting a score."
    );
    s
}

fn compare(profiles: &[&str], results: &BTreeMap<&str, Vec<TaskResult>>) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "# Paired comparisons\n");
    let _ = writeln!(
        s,
        "Each row compares two models on the tasks both ran: the mean of per-task differences (A − B, in percentage points), its clustered standard error and 95% interval, the correlation of their per-task results, and the smallest difference this comparison could detect (5% significance, 80% power). A difference whose interval includes 0 is not distinguishable from noise.\n"
    );
    let _ = writeln!(
        s,
        "| A | B | A − B | ± SE | 95% CI | Detectable | Corr | A better / B better / same | Verdict |"
    );
    let _ = writeln!(s, "|---|---|---|---|---|---|---|---|---|");
    for (i, a) in profiles.iter().enumerate() {
        for b in &profiles[i + 1..] {
            let p = stats::paired(&results[a], &results[b]);
            let d = &p.diff;
            let verdict = if d.lo.is_nan() {
                "too few tasks"
            } else if d.lo > 0.0 {
                "A better"
            } else if d.hi < 0.0 {
                "B better"
            } else {
                "not distinguishable"
            };
            let _ = writeln!(
                s,
                "| {a} | {b} | {} | {} | {} – {} | ±{} | {} | {} / {} / {} | {verdict} |",
                pts(d.mean),
                pts(d.se).trim_start_matches('+'),
                pts(d.lo),
                pts(d.hi),
                pts(p.mde).trim_start_matches('+'),
                if p.corr.is_nan() {
                    "n/a".into()
                } else {
                    format!("{:.2}", p.corr)
                },
                p.a_better,
                p.b_better,
                p.ties
            );
        }
    }
    s
}
