//! Process hygiene for shell commands (Tier 0 of
//! `docs/design/design-shell-sandboxing.md`):
//!
//! - commands get an allowlisted environment, never mima's own secrets;
//! - each command runs in a new session (no controlling terminal, so it cannot
//!   inject keystrokes with `TIOCSTI`) and leads its own process group;
//! - when the command returns or times out, the whole group is killed, so
//!   background processes it started do not outlive it;
//! - `TMPDIR` is a private directory for the session, removed afterwards.
//!
//! This is not a sandbox: file and network access are unchanged (Tier 1).

use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};

/// Variables every command gets (when set in mima's environment): what shells
/// and common build tools need, nothing that carries credentials.
const BASE_VARS: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TERM",
    "TZ",
    "LANG",
    "LANGUAGE",
    "COLORTERM",
    "NO_COLOR",
    // Toolchains whose location differs per machine.
    "RUSTUP_HOME",
    "CARGO_HOME",
    "GOPATH",
    "GOROOT",
    "JAVA_HOME",
    "VIRTUAL_ENV",
    "CONDA_PREFIX",
    "CUDA_HOME",
    "LD_LIBRARY_PATH",
    "PKG_CONFIG_PATH",
];

/// Names that usually carry secrets or reach credential stores. Passing one
/// through `env_passthrough` is allowed but logged.
fn looks_sensitive(name: &str) -> bool {
    let n = name.to_ascii_uppercase();
    n.starts_with("MIMA_")
        || n.starts_with("AWS_")
        || n.ends_with("_TOKEN")
        || n.ends_with("_KEY")
        || n.contains("SECRET")
        || n.contains("PASSWORD")
        || matches!(
            n.as_str(),
            "SSH_AUTH_SOCK" | "GPG_AGENT_INFO" | "DBUS_SESSION_BUS_ADDRESS"
        )
}

/// The environment for commands: the base variables, `LC_*`, and the names in
/// `passthrough`, taken from `source` (mima's own environment in production).
/// `TMPDIR` is added separately (the session's private directory).
pub fn child_env(
    source: impl IntoIterator<Item = (String, String)>,
    passthrough: &[String],
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = source
        .into_iter()
        .filter(|(k, _)| {
            BASE_VARS.contains(&k.as_str()) || k.starts_with("LC_") || passthrough.contains(k)
        })
        .collect();
    out.sort();
    for name in passthrough {
        if looks_sensitive(name) {
            tracing::warn!(
                var = %name,
                "env_passthrough gives shell commands a variable that may hold a secret"
            );
        }
    }
    out
}

/// A private temporary directory (mode 0700) for one session's commands,
/// removed when dropped. Created under `$XDG_RUNTIME_DIR` when set (a per-user
/// tmpfs), else the system temp directory.
pub struct SessionTmp {
    path: PathBuf,
}

impl SessionTmp {
    pub fn create() -> std::io::Result<Self> {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .unwrap_or_else(std::env::temp_dir);
        Self::create_in(&base)
    }

    pub fn create_in(base: &Path) -> std::io::Result<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let path = base.join(format!("mima-{}-{nanos:08x}", std::process::id()));
        std::fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SessionTmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// What a finished command produced.
pub struct Finished {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// The command did not finish within the time limit; its group was killed.
#[derive(Debug)]
pub struct TimedOut;

/// Most output kept per stream; the rest is read and discarded so the
/// command never blocks on a full pipe.
const MAX_CAPTURE: u64 = 32 << 20;
/// Time between SIGTERM and SIGKILL when a command times out.
const GRACE: Duration = Duration::from_secs(2);
/// How long to wait for output after the group is gone (a process that left
/// the group with its own `setsid` could hold the pipes open).
const DRAIN: Duration = Duration::from_secs(2);

/// Runs `argv` with exactly `env`, stdin from /dev/null, in a new session.
/// Kills the command's whole process group when it exits or after `timeout`.
pub async fn run(
    argv: &[String],
    env: &[(String, String)],
    timeout: Duration,
) -> std::io::Result<Result<Finished, TimedOut>> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| std::io::Error::other("empty command"))?;
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // SAFETY: setsid() is async-signal-safe and touches no memory of the
    // parent, so it is sound between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    // After setsid() the child leads a group whose id is its pid.
    let pgid = child.id().map(|p| p as libc::pid_t);
    let out = drain(child.stdout.take());
    let err = drain(child.stderr.take());

    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(status) => {
            kill_group(pgid, libc::SIGKILL);
            Some(status?)
        }
        Err(_) => {
            kill_group(pgid, libc::SIGTERM);
            if tokio::time::timeout(GRACE, child.wait()).await.is_err() {
                kill_group(pgid, libc::SIGKILL);
                let _ = child.wait().await;
            }
            // Members that ignored SIGTERM.
            kill_group(pgid, libc::SIGKILL);
            None
        }
    };
    let stdout = collect(out).await;
    let stderr = collect(err).await;
    Ok(match status {
        Some(s) => Ok(Finished {
            code: s.code(),
            stdout,
            stderr,
        }),
        None => Err(TimedOut),
    })
}

fn kill_group(pgid: Option<libc::pid_t>, signal: libc::c_int) {
    if let Some(p) = pgid.filter(|p| *p > 1) {
        // SAFETY: plain syscall; ESRCH (group already gone) is expected.
        unsafe {
            libc::killpg(p, signal);
        }
    }
}

type Reader = tokio::task::JoinHandle<Vec<u8>>;

fn drain<R: AsyncRead + Unpin + Send + 'static>(r: Option<R>) -> Option<Reader> {
    r.map(|r| {
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut limited = r.take(MAX_CAPTURE);
            let _ = limited.read_to_end(&mut buf).await;
            let _ = tokio::io::copy(&mut limited.into_inner(), &mut tokio::io::sink()).await;
            buf
        })
    })
}

async fn collect(r: Option<Reader>) -> Vec<u8> {
    match r {
        Some(handle) => {
            let abort = handle.abort_handle();
            match tokio::time::timeout(DRAIN, handle).await {
                Ok(Ok(buf)) => buf,
                _ => {
                    abort.abort();
                    Vec::new()
                }
            }
        }
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> String {
        v.to_string()
    }

    fn sh(script: &str) -> Vec<String> {
        vec![s("sh"), s("-c"), s(script)]
    }

    fn base_env() -> Vec<(String, String)> {
        vec![(s("PATH"), std::env::var("PATH").unwrap_or_default())]
    }

    #[test]
    fn env_keeps_only_allowlisted_names() {
        let source = vec![
            (s("PATH"), s("/usr/bin")),
            (s("HOME"), s("/home/u")),
            (s("LC_ALL"), s("C")),
            (s("MIMA_API_KEY"), s("secret")),
            (s("GITHUB_TOKEN"), s("t")),
            (s("SSH_AUTH_SOCK"), s("/run/agent")),
            (s("EDITOR"), s("vi")),
        ];
        let env = child_env(source.clone(), &[]);
        let names: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, vec!["HOME", "LC_ALL", "PATH"]);
        let env = child_env(source, &[s("EDITOR")]);
        assert!(env.iter().any(|(k, v)| k == "EDITOR" && v == "vi"));
        assert!(env.iter().all(|(k, _)| k != "MIMA_API_KEY"));
    }

    #[test]
    fn sensitive_names_are_recognized() {
        for n in [
            "MIMA_API_KEY",
            "AWS_PROFILE",
            "GH_TOKEN",
            "OPENAI_API_KEY",
            "SSH_AUTH_SOCK",
        ] {
            assert!(looks_sensitive(n), "{n}");
        }
        for n in ["PATH", "EDITOR", "CARGO_HOME"] {
            assert!(!looks_sensitive(n), "{n}");
        }
    }

    #[test]
    fn session_tmp_is_private_and_removed() {
        let base = std::env::temp_dir();
        let t = SessionTmp::create_in(&base).unwrap();
        let p = t.path().to_path_buf();
        let mode =
            std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&p).unwrap().permissions());
        assert_eq!(mode & 0o777, 0o700);
        drop(t);
        assert!(!p.exists());
    }

    #[tokio::test]
    async fn runs_in_a_new_session_without_a_terminal() {
        // /proc/<pid>/stat: field 6 is the session id, field 7 the controlling tty.
        let f = run(
            &sh("set -- $(cut -d' ' -f6,7 /proc/$$/stat); echo \"$1 $2 $$\""),
            &base_env(),
            Duration::from_secs(10),
        )
        .await
        .unwrap()
        .expect("finished");
        let out = String::from_utf8_lossy(&f.stdout);
        let v: Vec<&str> = out.split_whitespace().collect();
        assert_eq!(v[0], v[2], "the shell leads its own session: {out}");
        assert_eq!(v[1], "0", "no controlling terminal: {out}");
    }

    #[tokio::test]
    async fn gets_exactly_the_given_environment() {
        let env = vec![
            (s("PATH"), std::env::var("PATH").unwrap()),
            (s("ONLY"), s("1")),
        ];
        let f = run(&sh("env | sort"), &env, Duration::from_secs(10))
            .await
            .unwrap()
            .expect("finished");
        let out = String::from_utf8_lossy(&f.stdout);
        assert!(out.contains("ONLY=1"));
        assert!(!out.contains("HOME="), "{out}");
    }

    #[tokio::test]
    async fn background_processes_end_with_the_command() {
        let dir = SessionTmp::create_in(&std::env::temp_dir()).unwrap();
        let pidfile = dir.path().join("pid");
        let start = std::time::Instant::now();
        // The background sleep keeps stdout open; the command must still
        // return promptly, and the sleep must be gone afterwards.
        let f = run(
            &sh(&format!(
                "sleep 30 & echo $! > {}; echo started",
                pidfile.display()
            )),
            &base_env(),
            Duration::from_secs(20),
        )
        .await
        .unwrap()
        .expect("finished");
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "{:?}",
            start.elapsed()
        );
        assert_eq!(String::from_utf8_lossy(&f.stdout).trim(), "started");
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        // SAFETY: signal 0 only checks whether the process exists.
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            -1,
            "background sleep survived"
        );
    }

    #[tokio::test]
    async fn timeout_kills_the_whole_group() {
        let dir = SessionTmp::create_in(&std::env::temp_dir()).unwrap();
        let pidfile = dir.path().join("pid");
        let start = std::time::Instant::now();
        let r = run(
            &sh(&format!(
                "sleep 30 & echo $! > {}; sleep 30",
                pidfile.display()
            )),
            &base_env(),
            Duration::from_millis(500),
        )
        .await
        .unwrap();
        assert!(r.is_err(), "should time out");
        assert!(
            start.elapsed() < Duration::from_secs(8),
            "{:?}",
            start.elapsed()
        );
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        // SAFETY: signal 0 only checks whether the process exists.
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            -1,
            "background sleep survived the timeout"
        );
    }
}
