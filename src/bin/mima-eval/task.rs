//! Task and suite definitions (see docs/eval.md).
//!
//! ```text
//! evals/tasks/<id>/
//!   task.toml         family, limits, checks
//!   instruction.md    exactly what the agent is told
//!   fixture/          copied into a fresh work directory for every trial
//!   checks/           hidden: copied in only after the agent finishes ($CHECKS)
//!   solution/solve.sh reference solution, run by `mima-eval validate`
//! ```

use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskFile {
    /// Tasks sharing a family are related (same fixture or bug pattern); the
    /// suite standard error is clustered by family. Defaults to the task id.
    family: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    /// "capability" (expected to be hard) or "regression" (expected to pass).
    #[serde(default = "default_kind")]
    kind: String,
    #[serde(default)]
    limits: Limits,
    /// Run in the work directory before the agent starts (network allowed).
    setup: Option<String>,
    /// The untouched fixture is already correct (a "should do nothing" task),
    /// so validation expects it to pass rather than fail.
    #[serde(default)]
    expect_fixture_passes: bool,
    #[serde(rename = "check", default)]
    checks: Vec<Check>,
}

fn default_kind() -> String {
    "capability".into()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub agent_timeout_sec: u64,
    pub check_timeout_sec: u64,
    pub max_steps: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            agent_timeout_sec: 900,
            check_timeout_sec: 120,
            max_steps: 30,
        }
    }
}

/// One check. A trial passes only if every `required` check passes.
/// (serde cannot reject unknown keys through `flatten`; the task file's own
/// top-level keys are still checked.)
#[derive(Debug, Clone, Deserialize)]
pub struct Check {
    pub name: String,
    #[serde(default = "yes")]
    pub required: bool,
    #[serde(flatten)]
    pub kind: CheckKind,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CheckKind {
    /// Shell command must exit 0 (sandboxed, network off, with timeout).
    Command {
        run: String,
    },
    /// Command stdout, whitespace-normalized, must equal `expected`.
    OutputEquals {
        run: String,
        expected: String,
    },
    FileExists {
        path: String,
    },
    FileAbsent {
        path: String,
    },
    /// File must contain `text` (plain substring, not a pattern).
    FileContains {
        path: String,
        text: String,
    },
    /// File must not contain `text`.
    FileLacks {
        path: String,
        text: String,
    },
    /// These fixture paths (files or directories) must be byte-identical.
    Unchanged {
        paths: Vec<String>,
    },
    /// Every changed file must match one of these patterns (`*` wildcard).
    OnlyChanged {
        paths: Vec<String>,
    },
    /// The agent's final answer must contain `contains` (case-insensitive,
    /// whitespace-normalized).
    FinalAnswer {
        contains: String,
    },
    /// Limits on the agent's behavior, from its transcript.
    Agent {
        max_steps: Option<usize>,
        #[serde(default)]
        no_loop_guard: bool,
    },
}

#[derive(Debug, Clone)]
pub struct Task {
    pub id: String,
    pub dir: PathBuf,
    pub family: String,
    pub tags: Vec<String>,
    pub kind: String,
    pub limits: Limits,
    pub setup: Option<String>,
    pub expect_fixture_passes: bool,
    pub instruction: String,
    pub checks: Vec<Check>,
}

impl Task {
    pub fn fixture(&self) -> PathBuf {
        self.dir.join("fixture")
    }
    pub fn hidden_checks(&self) -> PathBuf {
        self.dir.join("checks")
    }
    pub fn solution(&self) -> PathBuf {
        self.dir.join("solution").join("solve.sh")
    }

    pub fn load(dir: &Path) -> Result<Self, String> {
        let id = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| format!("{}: not a task directory", dir.display()))?;
        let read = |name: &str| {
            std::fs::read_to_string(dir.join(name))
                .map_err(|e| format!("{id}: cannot read {name}: {e}"))
        };
        let file: TaskFile =
            toml::from_str(&read("task.toml")?).map_err(|e| format!("{id}: task.toml: {e}"))?;
        let instruction = read("instruction.md")?.trim().to_string();
        if instruction.is_empty() {
            return Err(format!("{id}: instruction.md is empty"));
        }
        if !file.checks.iter().any(|c| c.required) {
            return Err(format!("{id}: needs at least one required check"));
        }
        Ok(Self {
            family: file.family.unwrap_or_else(|| id.clone()),
            id,
            dir: dir.to_path_buf(),
            tags: file.tags,
            kind: file.kind,
            limits: file.limits,
            setup: file.setup,
            expect_fixture_passes: file.expect_fixture_passes,
            instruction,
            checks: file.checks,
        })
    }
}

/// A suite file lists tasks from the tasks directory.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SuiteFile {
    #[serde(default)]
    description: String,
    /// Task ids; `"*"` means every task in the directory.
    tasks: Vec<String>,
    /// Default trials per task.
    trials: Option<usize>,
}

pub struct Suite {
    pub name: String,
    pub description: String,
    pub tasks: Vec<Task>,
    pub trials: Option<usize>,
}

/// Loads a suite file (`evals/suites/<name>.toml`, tasks resolved from the
/// sibling `tasks/` directory), a directory of tasks, or a single task.
pub fn load_suite(path: &Path) -> Result<Suite, String> {
    if path.is_file() {
        let raw = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let file: SuiteFile =
            toml::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))?;
        let tasks_dir = path
            .parent()
            .and_then(Path::parent)
            .map(|p| p.join("tasks"))
            .ok_or("suite file must be in <evals>/suites/")?;
        let all = file.tasks.iter().any(|t| t == "*");
        let tasks = if all {
            load_dir(&tasks_dir)?
        } else {
            file.tasks
                .iter()
                .map(|id| Task::load(&tasks_dir.join(id)))
                .collect::<Result<_, _>>()?
        };
        let name = path
            .file_stem()
            .map_or("suite".into(), |s| s.to_string_lossy().into_owned());
        return Ok(Suite {
            name,
            description: file.description,
            tasks,
            trials: file.trials,
        });
    }
    if path.join("task.toml").is_file() {
        let task = Task::load(path)?;
        return Ok(Suite {
            name: task.id.clone(),
            description: String::new(),
            tasks: vec![task],
            trials: None,
        });
    }
    let name = path
        .file_name()
        .map_or("tasks".into(), |s| s.to_string_lossy().into_owned());
    Ok(Suite {
        name,
        description: String::new(),
        tasks: load_dir(path)?,
        trials: None,
    })
}

fn load_dir(dir: &Path) -> Result<Vec<Task>, String> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("task.toml").is_file())
        .collect();
    dirs.sort();
    if dirs.is_empty() {
        return Err(format!("{}: no tasks found", dir.display()));
    }
    dirs.iter().map(|d| Task::load(d)).collect()
}

/// `*`-only glob match against a relative path.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    fn go(p: &[u8], t: &[u8]) -> bool {
        match (p.first(), t.first()) {
            (None, None) => true,
            (Some(b'*'), _) => go(&p[1..], t) || (!t.is_empty() && go(p, &t[1..])),
            (Some(a), Some(b)) if a == b => go(&p[1..], &t[1..]),
            _ => false,
        }
    }
    go(pattern.as_bytes(), text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_checks() {
        let file: TaskFile = toml::from_str(
            r#"
            family = "c-ops"
            [limits]
            max_steps = 10
            [[check]]
            name = "builds"
            type = "command"
            run = "make"
            [[check]]
            name = "scope"
            type = "only_changed"
            paths = ["src/*.c"]
            required = false
            [[check]]
            name = "calm"
            type = "agent"
            no_loop_guard = true
            "#,
        )
        .unwrap();
        assert_eq!(file.limits.max_steps, 10);
        assert_eq!(file.limits.agent_timeout_sec, 900);
        assert_eq!(file.checks.len(), 3);
        assert!(!file.checks[1].required);
        assert!(matches!(
            file.checks[2].kind,
            CheckKind::Agent {
                no_loop_guard: true,
                ..
            }
        ));
    }

    #[test]
    fn rejects_unknown_fields() {
        assert!(toml::from_str::<TaskFile>("famly = \"x\"").is_err());
    }

    #[test]
    fn globs() {
        assert!(glob_match("src/*.c", "src/ring.c"));
        assert!(glob_match("*", "anything/at/all"));
        assert!(glob_match("report.txt", "report.txt"));
        assert!(!glob_match("src/*.c", "src/ring.h"));
        assert!(!glob_match("a.txt", "b.txt"));
    }
}
