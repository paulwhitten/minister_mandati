//! `mima-eval calibrate`: task difficulty from past runs.
//!
//! Pools every trial in the given run directories, per task and per model,
//! and sorts tasks into bands: never solved (check the task before calling it
//! hard), always solved by every model (saturated: it no longer separates
//! models), and discriminating (solved 30-70% of the time on average, which
//! carries the most information per trial; arXiv 2603.23749). Writes a
//! suite file of the discriminating tasks for fast comparisons.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::Path;

use crate::run::TrialRecord;

pub fn run(run_dirs: &[std::path::PathBuf], out_suite: Option<&Path>) -> Result<String, String> {
    // task -> model -> (passes, trials)
    let mut table: BTreeMap<String, BTreeMap<String, (usize, usize)>> = BTreeMap::new();
    for dir in run_dirs {
        let text = std::fs::read_to_string(dir.join("trials.jsonl"))
            .map_err(|e| format!("{}: {e}", dir.display()))?;
        for line in text.lines() {
            let Ok(r) = serde_json::from_str::<TrialRecord>(line) else {
                continue;
            };
            if r.exit_reason == "infra" {
                continue;
            }
            let e = table
                .entry(r.task)
                .or_default()
                .entry(r.profile)
                .or_default();
            e.0 += r.passed as usize;
            e.1 += 1;
        }
    }
    if table.is_empty() {
        return Err("no trials found".into());
    }
    let mut models: Vec<&String> = table.values().flat_map(|m| m.keys()).collect();
    models.sort();
    models.dedup();

    let mut s = String::new();
    let _ = writeln!(
        s,
        "# Task calibration\n\n{} run(s), {} task(s), {} model(s). Mean is the average over models of each model's pass rate.\n",
        run_dirs.len(),
        table.len(),
        models.len()
    );
    let mut never = Vec::new();
    let mut saturated = Vec::new();
    let mut discriminating = Vec::new();
    let _ = write!(s, "| Task | Mean |");
    for m in &models {
        let _ = write!(s, " {m} |");
    }
    let _ = writeln!(s, " Band |\n|---|---|{}---|", "---|".repeat(models.len()));
    for (task, per) in &table {
        let rates: Vec<f64> = per.values().map(|(c, k)| *c as f64 / *k as f64).collect();
        let mean = rates.iter().sum::<f64>() / rates.len() as f64;
        let band = if per.values().all(|(c, _)| *c == 0) {
            never.push(task.clone());
            "never solved"
        } else if per.values().all(|(c, k)| c == k) && per.len() == models.len() {
            saturated.push(task.clone());
            "always solved"
        } else if (0.3..=0.7).contains(&mean) {
            discriminating.push(task.clone());
            "discriminating"
        } else {
            ""
        };
        let _ = write!(s, "| {task} | {:.0}% |", mean * 100.0);
        for m in &models {
            match per.get(*m) {
                Some((c, k)) => {
                    let _ = write!(s, " {c}/{k} |");
                }
                None => {
                    let _ = write!(s, " - |");
                }
            }
        }
        let _ = writeln!(s, " {band} |");
    }
    let _ = writeln!(
        s,
        "\nNever solved: {}. Always solved by every model: {}. Discriminating (30-70%): {}.",
        never.len(),
        saturated.len(),
        discriminating.len()
    );
    let _ = writeln!(
        s,
        "\nPolicy (docs/eval.md): review never-solved tasks for broken checks or unclear instructions before keeping them; retire always-solved tasks from comparison suites once they have been saturated across two or more runs (keep them as regressions); with fewer than about 5 runs these bands are noisy."
    );
    if let Some(path) = out_suite {
        let mut f = String::from(
            "description = \"Discriminating tasks (30-70% mean pass rate in calibration runs); for fast comparisons, not for absolute scores.\"\ntasks = [\n",
        );
        for t in &discriminating {
            let _ = writeln!(f, "  \"{t}\",");
        }
        f.push_str("]\n");
        std::fs::write(path, f).map_err(|e| format!("{}: {e}", path.display()))?;
        let _ = writeln!(
            s,
            "\nWrote {} ({} tasks).",
            path.display(),
            discriminating.len()
        );
    }
    Ok(s)
}
