//! Running processes for trials and checks, and preparing trial directories.
//!
//! Isolation (docs/eval.md): each trial gets a fresh copy of the fixture in
//! its own directory with a new git repository, its own HOME and TMPDIR, and
//! an allowlisted environment. Commands run in their own process group (a
//! timeout kills the whole group) and, when `bwrap` is available, in a
//! sandbox: read-only system, writable trial directory, private /tmp, no
//! network. `mima` itself runs outside the sandbox so it can reach the model
//! server; its shell commands go through the same sandbox via
//! `[security].bash_wrapper`.

use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Directories of one trial.
pub struct TrialDirs {
    pub root: PathBuf,
    pub work: PathBuf,
    pub home: PathBuf,
    pub tmp: PathBuf,
    pub checks: PathBuf,
}

impl TrialDirs {
    pub fn create(root: &Path) -> std::io::Result<Self> {
        if root.exists() {
            std::fs::remove_dir_all(root)?;
        }
        let d = Self {
            root: root.to_path_buf(),
            work: root.join("work"),
            home: root.join("home"),
            tmp: root.join("tmp"),
            checks: root.join("checks"),
        };
        for p in [&d.work, &d.home, &d.tmp] {
            std::fs::create_dir_all(p)?;
        }
        // Canonical paths: the sandbox binds them and tools compare them.
        Ok(Self {
            root: d.root.canonicalize()?,
            work: d.work.canonicalize()?,
            home: d.home.canonicalize()?,
            tmp: d.tmp.canonicalize()?,
            checks: d.root.canonicalize()?.join("checks"),
        })
    }
}

/// Recursively copies `from` into `to` (created if needed). Symlinks are
/// copied as links.
pub fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        let ft = entry.file_type()?;
        if ft.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if ft.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(entry.path())?, &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// The sandbox command prefix for a trial, or empty when `bwrap` is absent.
pub fn sandbox_prefix(dirs: &TrialDirs, enabled: bool) -> Vec<String> {
    if !enabled {
        return Vec::new();
    }
    let root = dirs.root.display().to_string();
    let work = dirs.work.display().to_string();
    [
        // Order matters: later mounts cover earlier ones, so the private /tmp
        // comes before the trial directory (which may itself be under /tmp).
        "bwrap",
        "--unshare-net",
        "--die-with-parent",
        "--ro-bind",
        "/",
        "/",
        "--dev",
        "/dev",
        "--proc",
        "/proc",
        "--tmpfs",
        "/tmp",
        "--bind",
        &root,
        &root,
        "--chdir",
        &work,
        "--",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

pub fn bwrap_available() -> bool {
    Command::new("bwrap")
        .args(["--ro-bind", "/", "/", "true"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Allowlisted environment for processes in a trial.
pub fn trial_env(dirs: &TrialDirs, extra: &[(&str, String)]) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for key in ["PATH", "LANG", "LC_ALL", "TERM", "USER", "LOGNAME"] {
        if let Ok(v) = std::env::var(key) {
            env.insert(key.to_string(), v);
        }
    }
    // Rust toolchains live under the real home; keep them reachable.
    let real_home = std::env::var("HOME").unwrap_or_default();
    let rustup = std::env::var("RUSTUP_HOME").unwrap_or_else(|_| format!("{real_home}/.rustup"));
    if Path::new(&rustup).exists() {
        env.insert("RUSTUP_HOME".into(), rustup);
    }
    env.insert("HOME".into(), dirs.home.display().to_string());
    env.insert("TMPDIR".into(), dirs.tmp.display().to_string());
    env.insert(
        "CARGO_HOME".into(),
        dirs.home.join(".cargo").display().to_string(),
    );
    env.insert("WORK".into(), dirs.work.display().to_string());
    for (k, v) in extra {
        env.insert(k.to_string(), v.clone());
    }
    env
}

/// Result of a finished (or killed) process.
pub struct Output {
    /// `None` when killed on timeout.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub ms: u64,
    pub timed_out: bool,
}

impl Output {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// Runs `argv` (after an optional sandbox prefix) in `cwd` with exactly
/// `env`, killing its whole process group after `timeout`. Captures up to
/// 1 MiB of each stream.
pub fn run(
    argv: &[String],
    prefix: &[String],
    cwd: &Path,
    env: &BTreeMap<String, String>,
    timeout: Duration,
) -> Output {
    let full: Vec<&String> = prefix.iter().chain(argv).collect();
    let start = Instant::now();
    let mut cmd = Command::new(full[0]);
    cmd.args(&full[1..])
        .current_dir(cwd)
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Output {
                code: None,
                stdout: String::new(),
                stderr: format!("failed to start {}: {e}", full[0]),
                ms: 0,
                timed_out: false,
            };
        }
    };
    let pgid = child.id();
    // Drain the pipes on threads so a chatty process cannot block.
    let drain = |r: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut r) = r {
                let _ = r.by_ref().take(1 << 20).read_to_end(&mut buf);
                let _ = std::io::copy(&mut r, &mut std::io::sink());
            }
            String::from_utf8_lossy(&buf).into_owned()
        })
    };
    let out = drain(
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );
    let err = drain(
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );
    let mut timed_out = false;
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) if start.elapsed() >= timeout => {
                timed_out = true;
                kill_group(pgid);
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => break None,
        }
    };
    // Anything the group left behind goes too.
    kill_group(pgid);
    Output {
        code,
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
        ms: start.elapsed().as_millis() as u64,
        timed_out,
    }
}

fn kill_group(pgid: u32) {
    let _ = Command::new("kill")
        .args(["-KILL", "--", &format!("-{pgid}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// `sh -c <script>` as an argv.
pub fn sh(script: &str) -> Vec<String> {
    vec!["sh".into(), "-c".into(), script.into()]
}

fn git(work: &Path, args: &[&str], index: Option<&Path>) -> Output {
    let mut env = BTreeMap::new();
    env.insert(
        "PATH".to_string(),
        std::env::var("PATH").unwrap_or_default(),
    );
    env.insert("GIT_AUTHOR_NAME".into(), "mima-eval".into());
    env.insert("GIT_AUTHOR_EMAIL".into(), "eval@localhost".into());
    env.insert("GIT_COMMITTER_NAME".into(), "mima-eval".into());
    env.insert("GIT_COMMITTER_EMAIL".into(), "eval@localhost".into());
    env.insert("GIT_CONFIG_NOSYSTEM".into(), "1".into());
    env.insert("HOME".into(), work.display().to_string());
    if let Some(i) = index {
        env.insert("GIT_INDEX_FILE".into(), i.display().to_string());
    }
    let argv: Vec<String> = std::iter::once("git".to_string())
        .chain(args.iter().map(|s| s.to_string()))
        .collect();
    run(&argv, &[], work, &env, Duration::from_secs(60))
}

/// Initializes a fresh repository with the fixture as its only commit, so
/// changes can be diffed and no history exists to learn from. Returns the
/// commit id.
pub fn git_baseline(work: &Path) -> Result<String, String> {
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["add", "-A"],
        &[
            "commit",
            "-q",
            "--allow-empty",
            "--no-gpg-sign",
            "-m",
            "fixture",
        ],
    ] {
        let o = git(work, args, None);
        if !o.success() {
            return Err(format!("git {}: {}", args.join(" "), o.stderr.trim()));
        }
    }
    Ok(git(work, &["rev-parse", "HEAD"], None)
        .stdout
        .trim()
        .to_string())
}

/// Changes since the baseline, using a separate index so the repository the
/// agent may itself use is untouched: (changed paths, lines added, lines
/// removed, patch).
pub fn git_changes(work: &Path, baseline: &str, scratch: &Path) -> (Vec<String>, u64, u64, String) {
    let index = scratch.join("eval-index");
    let _ = std::fs::remove_file(&index);
    git(work, &["read-tree", baseline], Some(&index));
    git(work, &["add", "-A"], Some(&index));
    let names = git(
        work,
        &["diff", "--cached", "--name-only", baseline],
        Some(&index),
    )
    .stdout;
    let numstat = git(
        work,
        &["diff", "--cached", "--numstat", baseline],
        Some(&index),
    )
    .stdout;
    let patch = git(work, &["diff", "--cached", baseline], Some(&index)).stdout;
    let _ = std::fs::remove_file(&index);
    let (mut added, mut removed) = (0, 0);
    for line in numstat.lines() {
        let mut parts = line.split('\t');
        added += parts
            .next()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        removed += parts
            .next()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
    }
    let names = names
        .lines()
        .map(str::to_string)
        .filter(|n| !n.is_empty())
        .collect();
    (names, added, removed, patch)
}

/// Whether `rel` is byte-identical in `fixture` and `work` (recursively for
/// directories; files present in only one side count as changes).
pub fn unchanged(fixture: &Path, work: &Path, rel: &str) -> Result<(), String> {
    let (a, b) = (fixture.join(rel), work.join(rel));
    if a.is_dir() {
        let list = |d: &Path| -> Vec<std::ffi::OsString> {
            let mut v: Vec<_> = std::fs::read_dir(d)
                .map(|r| r.filter_map(|e| e.ok().map(|e| e.file_name())).collect())
                .unwrap_or_default();
            v.sort();
            v
        };
        if !b.is_dir() || list(&a) != list(&b) {
            return Err(format!("{rel}: directory contents changed"));
        }
        for name in list(&a) {
            unchanged(fixture, work, &format!("{rel}/{}", name.to_string_lossy()))?;
        }
        return Ok(());
    }
    match (std::fs::read(&a), std::fs::read(&b)) {
        (Ok(x), Ok(y)) if x == y => Ok(()),
        (Ok(_), Ok(_)) => Err(format!("{rel}: modified")),
        (Ok(_), Err(_)) => Err(format!("{rel}: deleted")),
        (Err(e), _) => Err(format!("{rel}: not in fixture ({e})")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mima-eval-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn run_captures_output_and_kills_on_timeout() {
        let d = tmp("run");
        let env: BTreeMap<String, String> =
            [("PATH".to_string(), std::env::var("PATH").unwrap())].into();
        let o = run(
            &sh("echo hi; echo err >&2; exit 3"),
            &[],
            &d,
            &env,
            Duration::from_secs(5),
        );
        assert_eq!(
            (o.code, o.stdout.as_str(), o.stderr.as_str()),
            (Some(3), "hi\n", "err\n")
        );
        let start = Instant::now();
        let o = run(
            &sh("sleep 5 & sleep 5"),
            &[],
            &d,
            &env,
            Duration::from_millis(300),
        );
        assert!(o.timed_out && start.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn git_baseline_and_changes() {
        let d = tmp("git");
        let work = d.join("work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join("a.txt"), "one\n").unwrap();
        let base = git_baseline(&work).unwrap();
        std::fs::write(work.join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(work.join("b.txt"), "new\n").unwrap();
        let (names, added, removed, patch) = git_changes(&work, &base, &d);
        assert_eq!(names, vec!["a.txt", "b.txt"]);
        assert_eq!((added, removed), (2, 0));
        assert!(patch.contains("+two"));
    }

    #[test]
    fn unchanged_detects_edits_and_deletions() {
        let d = tmp("unch");
        let (fx, wk) = (d.join("fx"), d.join("wk"));
        std::fs::create_dir_all(fx.join("tests")).unwrap();
        std::fs::write(fx.join("tests/t.py"), "assert 1\n").unwrap();
        copy_tree(&fx, &wk).unwrap();
        assert!(unchanged(&fx, &wk, "tests").is_ok());
        std::fs::write(wk.join("tests/t.py"), "pass\n").unwrap();
        assert!(
            unchanged(&fx, &wk, "tests")
                .unwrap_err()
                .contains("modified")
        );
    }
}
