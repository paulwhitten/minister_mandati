//! `mima-eval`: runs task suites against one or more model profiles with the
//! real `mima` binary, scores them with deterministic checks, and reports
//! honest statistics. See docs/eval.md.

mod calibrate;
mod checks;
mod exec;
mod exploit;
mod metrics;
mod power;
mod probe;
mod report;
mod run;
#[allow(dead_code)]
#[path = "../../session.rs"]
mod session;
mod source;
mod stats;
mod task;

use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "mima-eval", version, about = "Evaluate mima on task suites")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a suite (a suite file, a directory of tasks, or one task).
    Run {
        suite: PathBuf,
        /// Model profiles; without it, one profile from the base config.
        #[arg(long)]
        profiles: Option<PathBuf>,
        /// Only these profiles (repeatable).
        #[arg(long = "profile")]
        only: Vec<String>,
        /// Trials per task (default: the suite's, else 3).
        #[arg(long)]
        trials: Option<usize>,
        /// Base mima config (default: ./agent.toml if present).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Where runs are written.
        /// Where runs are written (default: `runs/` in the suite's
        /// evaluation tree, next to `tasks/`).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Continue an existing run directory, skipping recorded trials.
        #[arg(long)]
        resume: Option<PathBuf>,
        /// The mima binary to evaluate (default: next to this binary).
        #[arg(long)]
        mima: Option<PathBuf>,
        /// Keep work directories of passing trials too.
        #[arg(long)]
        keep: bool,
        /// Do not sandbox shell commands and checks (bwrap).
        #[arg(long)]
        no_sandbox: bool,
    },
    /// Check that each task's solution passes, its fixture fails, and the
    /// cheating baselines (delete tests, exit 0 early, list everything) fail.
    Validate {
        suite: PathBuf,
        #[arg(long)]
        no_sandbox: bool,
        /// Run each reference solution this many times (flakiness check).
        #[arg(long, default_value_t = 1)]
        repeat: usize,
        /// Skip the cheating baselines.
        #[arg(long)]
        no_cheats: bool,
    },
    /// Clone the pinned commits that source tasks use into the local cache,
    /// checking their licenses (runs also fetch on demand).
    Fetch { suite: PathBuf },
    /// Rewrite summary.md and compare.md for a run directory.
    Report { run_dir: PathBuf },
    /// Measure how strong each task's checks are: apply the reference
    /// solution plus one small code mutation at a time and count how many
    /// mutants the checks reject.
    Strength {
        suite: PathBuf,
        /// Mutants per task.
        #[arg(long, default_value_t = 12)]
        max_mutants: usize,
        #[arg(long)]
        no_sandbox: bool,
    },
    /// Ask a model, without code or tools, which file holds each repo
    /// task's bug and what the original code is (memorization check).
    Probe {
        suite: PathBuf,
        #[arg(long)]
        profiles: PathBuf,
        /// The profile to probe (its server must already be serving it).
        #[arg(long)]
        profile: String,
    },
    /// Task difficulty across runs: never solved, always solved, and the
    /// discriminating 30-70% band (optionally written as a suite).
    Calibrate {
        #[arg(required = true)]
        run_dirs: Vec<PathBuf>,
        /// Write the discriminating tasks to this suite file.
        #[arg(long)]
        suite_out: Option<PathBuf>,
    },
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

/// Sandboxing is required unless explicitly turned off: without it the
/// agent's commands could read solutions and hidden checks.
fn sandbox_setting(no_sandbox: bool) -> Result<bool, String> {
    if no_sandbox {
        eprintln!("warning: --no-sandbox: shell commands and checks run unconfined");
        return Ok(false);
    }
    if exec::bwrap_available() {
        Ok(true)
    } else {
        Err("bwrap is not usable; install bubblewrap or pass --no-sandbox".into())
    }
}

fn default_mima() -> PathBuf {
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("mima")));
    sibling
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("mima"))
}

fn load_base_config(path: Option<&Path>) -> Result<toml::Table, String> {
    let path = match path {
        Some(p) => p.to_path_buf(),
        None if Path::new("agent.toml").is_file() => PathBuf::from("agent.toml"),
        None => return Ok(toml::Table::new()),
    };
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    toml::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))
}

fn real_main() -> Result<(), String> {
    match Cli::parse().command {
        Command::Run {
            suite,
            profiles,
            only,
            trials,
            config,
            out,
            resume,
            mima,
            keep,
            no_sandbox,
        } => {
            let suite = task::load_suite(&suite)?;
            let base = load_base_config(config.as_deref())?;
            let profiles = match profiles {
                Some(p) => run::load_profiles(&p, &only)?,
                None => vec![run::profile_from_config(&base)],
            };
            let mima = mima.unwrap_or_else(default_mima);
            let opts = run::RunOptions {
                mima: std::fs::canonicalize(&mima).unwrap_or(mima),
                base_config: base,
                trials: trials.or(suite.trials).unwrap_or(3),
                sandbox: sandbox_setting(no_sandbox)?,
                keep,
            };
            let run_dir = resume.unwrap_or_else(|| {
                run::default_run_dir(&out.unwrap_or_else(|| suite.root.join("runs")), &suite.name)
            });
            eprintln!(
                "Running {} task(s) x {} trial(s) x {} model(s) into {}",
                suite.tasks.len(),
                opts.trials,
                profiles.len(),
                run_dir.display()
            );
            eprintln!(
                "Note: the agent runs model-chosen shell commands without asking, confined to \
                 each trial's directory{}.",
                if opts.sandbox {
                    " and a network-less sandbox"
                } else {
                    " (no sandbox)"
                }
            );
            run::run_suite(&suite, &profiles, &run_dir, &opts)?;
            let summary = report::write(&run_dir)?;
            println!("{summary}");
            eprintln!("Reports: {}", run_dir.display());
            Ok(())
        }
        Command::Validate {
            suite,
            no_sandbox,
            repeat,
            no_cheats,
        } => {
            let suite = task::load_suite(&suite)?;
            let root =
                std::env::temp_dir().join(format!("mima-eval-validate-{}", std::process::id()));
            let opts = run::ValidateOpts {
                sandbox: sandbox_setting(no_sandbox)?,
                repeat,
                exploits: !no_cheats,
            };
            let bad = run::validate(&suite, &root, &opts);
            let _ = std::fs::remove_dir_all(&root);
            if bad > 0 {
                return Err(format!("{bad} of {} task(s) invalid", suite.tasks.len()));
            }
            println!("all {} task(s) valid", suite.tasks.len());
            Ok(())
        }
        Command::Strength {
            suite,
            max_mutants,
            no_sandbox,
        } => {
            let suite = task::load_suite(&suite)?;
            let root =
                std::env::temp_dir().join(format!("mima-eval-strength-{}", std::process::id()));
            let table = run::strength(&suite, &root, sandbox_setting(no_sandbox)?, max_mutants);
            let _ = std::fs::remove_dir_all(&root);
            println!("{table}");
            Ok(())
        }
        Command::Probe {
            suite,
            profiles,
            profile,
        } => {
            let suite = task::load_suite(&suite)?;
            let p = run::load_profiles(&profiles, std::slice::from_ref(&profile))?
                .into_iter()
                .next()
                .ok_or("no such profile")?;
            println!("{}", probe::run(&suite, &p)?);
            Ok(())
        }
        Command::Calibrate {
            run_dirs,
            suite_out,
        } => {
            println!("{}", calibrate::run(&run_dirs, suite_out.as_deref())?);
            Ok(())
        }
        Command::Fetch { suite } => {
            let suite = task::load_suite(&suite)?;
            let mut n = 0;
            for t in &suite.tasks {
                if let Some(src) = &t.source {
                    source::fetch(src, &source::cache_root(&t.dir))
                        .map_err(|e| format!("{}: {e}", t.id))?;
                    n += 1;
                }
            }
            println!("{n} source task(s) ready");
            Ok(())
        }
        Command::Report { run_dir } => {
            println!("{}", report::write(&run_dir)?);
            Ok(())
        }
    }
}
