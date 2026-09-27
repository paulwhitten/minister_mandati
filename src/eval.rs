//! Evaluation harness skeleton: a verifier vector over task outcomes, plus basic
//! pass-rate statistics. This is scaffolding for the Phase 2 evaluation subsystem
//! (see `docs/plan.md` and `docs/design/design-eval-harness.md`); it is not yet wired
//! into the CLI. Graders are intentionally small and composable.

use std::path::PathBuf;
use std::process::Command;

/// The observable result of one task attempt that graders inspect.
pub struct Outcome {
    /// Working directory the attempt ran in (graders read files relative to it).
    pub workdir: PathBuf,
    /// The agent's final answer text.
    pub final_answer: String,
    /// Steps taken (loop iterations).
    pub steps: usize,
    /// Total tokens consumed.
    pub total_tokens: u64,
}

impl Outcome {
    pub fn new(workdir: impl Into<PathBuf>) -> Self {
        Self {
            workdir: workdir.into(),
            final_answer: String::new(),
            steps: 0,
            total_tokens: 0,
        }
    }
}

/// A single typed grade in the verifier vector.
#[derive(Clone, Debug, PartialEq)]
pub struct Grade {
    pub grader: String,
    pub passed: bool,
    /// Continuous score in `[0.0, 1.0]`; binary graders use 0.0 or 1.0.
    pub score: f64,
    pub detail: String,
}

impl Grade {
    fn boolean(grader: &str, passed: bool, detail: impl Into<String>) -> Self {
        Self {
            grader: grader.to_string(),
            passed,
            score: if passed { 1.0 } else { 0.0 },
            detail: detail.into(),
        }
    }
}

/// One element of the verifier vector. Graders are cheap, deterministic checks.
pub trait Grader {
    fn name(&self) -> &str;
    fn grade(&self, outcome: &Outcome) -> Grade;
}

/// Passes when a path exists under the outcome's working directory.
pub struct FileExists {
    pub path: PathBuf,
}

impl Grader for FileExists {
    fn name(&self) -> &str {
        "file_exists"
    }

    fn grade(&self, outcome: &Outcome) -> Grade {
        let full = outcome.workdir.join(&self.path);
        let ok = full.exists();
        Grade::boolean(self.name(), ok, format!("{} exists: {ok}", full.display()))
    }
}

/// Passes when a file under the working directory contains the needle.
pub struct FileContains {
    pub path: PathBuf,
    pub needle: String,
}

impl Grader for FileContains {
    fn name(&self) -> &str {
        "file_contains"
    }

    fn grade(&self, outcome: &Outcome) -> Grade {
        let full = outcome.workdir.join(&self.path);
        let ok = std::fs::read_to_string(&full)
            .map(|c| c.contains(&self.needle))
            .unwrap_or(false);
        Grade::boolean(
            self.name(),
            ok,
            format!("{} contains {:?}: {ok}", full.display(), self.needle),
        )
    }
}

/// Passes when a shell command exits zero in the working directory. Models the
/// compile/test/sanitizer/lint graders (a nonzero exit is ground truth).
pub struct CommandSucceeds {
    pub label: String,
    pub command: String,
}

impl Grader for CommandSucceeds {
    fn name(&self) -> &str {
        &self.label
    }

    fn grade(&self, outcome: &Outcome) -> Grade {
        let status = Command::new("sh")
            .arg("-c")
            .arg(&self.command)
            .current_dir(&outcome.workdir)
            .status();
        match status {
            Ok(s) => Grade::boolean(
                self.name(),
                s.success(),
                format!("`{}` -> {s}", self.command),
            ),
            Err(e) => Grade::boolean(
                self.name(),
                false,
                format!("`{}` failed to spawn: {e}", self.command),
            ),
        }
    }
}

/// A verifier vector: the ordered set of graders applied to an outcome.
#[derive(Default)]
pub struct Verifier {
    graders: Vec<Box<dyn Grader>>,
}

impl Verifier {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, grader: Box<dyn Grader>) -> Self {
        self.graders.push(grader);
        self
    }

    pub fn verify(&self, outcome: &Outcome) -> Report {
        Report {
            grades: self.graders.iter().map(|g| g.grade(outcome)).collect(),
        }
    }
}

/// The verifier vector's result for one outcome.
pub struct Report {
    pub grades: Vec<Grade>,
}

impl Report {
    /// True only when every grader passed.
    pub fn passed(&self) -> bool {
        !self.grades.is_empty() && self.grades.iter().all(|g| g.passed)
    }

    /// Mean score across graders (0.0 when empty).
    pub fn score(&self) -> f64 {
        if self.grades.is_empty() {
            return 0.0;
        }
        self.grades.iter().map(|g| g.score).sum::<f64>() / self.grades.len() as f64
    }
}

/// A task definition: an instruction plus the verifier that grades it.
pub struct Task {
    pub id: String,
    pub instruction: String,
    pub verifier: Verifier,
}

/// Pass-rate statistics over N independent attempts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PassStats {
    pub runs: usize,
    pub passes: usize,
    pub rate: f64,
    /// Standard error of the mean for a Bernoulli sample: sqrt(p(1-p)/n).
    pub sem: f64,
}

/// Summarizes a set of pass/fail results (the basis for pass@1 and pass@k).
pub fn summarize(results: &[bool]) -> PassStats {
    let runs = results.len();
    let passes = results.iter().filter(|&&r| r).count();
    if runs == 0 {
        return PassStats {
            runs: 0,
            passes: 0,
            rate: 0.0,
            sem: 0.0,
        };
    }
    let rate = passes as f64 / runs as f64;
    let sem = (rate * (1.0 - rate) / runs as f64).sqrt();
    PassStats {
        runs,
        passes,
        rate,
        sem,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn outcome_here() -> Outcome {
        Outcome::new(Path::new("."))
    }

    #[test]
    fn file_exists_grader() {
        let pass = FileExists {
            path: PathBuf::from("Cargo.toml"),
        }
        .grade(&outcome_here());
        assert!(pass.passed);
        let fail = FileExists {
            path: PathBuf::from("does_not_exist_xyz"),
        }
        .grade(&outcome_here());
        assert!(!fail.passed);
    }

    #[test]
    fn command_succeeds_grader() {
        let pass = CommandSucceeds {
            label: "true".into(),
            command: "true".into(),
        }
        .grade(&outcome_here());
        assert!(pass.passed);
        let fail = CommandSucceeds {
            label: "false".into(),
            command: "false".into(),
        }
        .grade(&outcome_here());
        assert!(!fail.passed);
    }

    #[test]
    fn verifier_aggregates_all_graders() {
        let v = Verifier::new()
            .with(Box::new(FileExists {
                path: PathBuf::from("Cargo.toml"),
            }))
            .with(Box::new(CommandSucceeds {
                label: "true".into(),
                command: "true".into(),
            }));
        let report = v.verify(&outcome_here());
        assert!(report.passed());
        assert_eq!(report.score(), 1.0);

        let v2 = v.with(Box::new(FileExists {
            path: PathBuf::from("nope_xyz"),
        }));
        let report2 = v2.verify(&outcome_here());
        assert!(!report2.passed());
    }

    #[test]
    fn summarize_rate_and_sem() {
        let s = summarize(&[true, true, true, false]);
        assert_eq!(s.runs, 4);
        assert_eq!(s.passes, 3);
        assert!((s.rate - 0.75).abs() < 1e-9);
        assert!((s.sem - (0.75 * 0.25 / 4.0_f64).sqrt()).abs() < 1e-9);
        assert_eq!(summarize(&[]).rate, 0.0);
    }
}
