//! Shell sandbox (`docs/design/design-shell-sandboxing.md`): the
//! platform-neutral policy and the dispatch to one backend per platform.
//!
//! - `linux.rs`: Landlock, seccomp and resource limits, applied by mima's own
//!   helper (`mima __sandbox-exec`) before it executes the command.
//! - `macos.rs`: a Seatbelt profile run through `/usr/bin/sandbox-exec`.
//!
//! Each backend provides `SYSTEM_READ`, `SYSTEM_WRITE`, `probe()`,
//! `describe()`, `wrap()` and `prepare()`. Only Linux and macOS are supported.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as platform;

// Also compiled in tests elsewhere, so the profile generator is tested on Linux CI.
#[cfg(any(target_os = "macos", test))]
mod macos;
#[cfg(target_os = "macos")]
use macos as platform;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("mima's shell sandbox supports only Linux and macOS");

pub use platform::{SYSTEM_READ, SYSTEM_WRITE, gpu_present, probe};

/// What a sandboxed command may access. Serialized to the helper as JSON.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Policy {
    /// Read and execute (directories recursively).
    pub read: Vec<PathBuf>,
    /// Read, write, create and delete (directories recursively).
    pub write: Vec<PathBuf>,
    /// Allow network access (sockets of any family).
    pub network: bool,
    /// Linux: NVIDIA GPU access (`[security].sandbox_gpu`). The CUDA driver
    /// binds a Unix socket, so with the network off Unix sockets may bind,
    /// listen and accept (`connect` stays denied). The driver also names its
    /// threads through `/proc/self/task/*/comm`; the caller adds `/proc` to
    /// `write` for that, so `write` lists every writable location.
    #[serde(default)]
    pub gpu: bool,
}

/// What the running system supports.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Support {
    /// Linux: Landlock ABI version; 0 when Landlock is unavailable or disabled.
    pub landlock_abi: i32,
    /// Linux: seccomp filters can be installed.
    pub seccomp: bool,
    /// macOS: `sandbox-exec` can apply a profile.
    pub seatbelt: bool,
}

impl Support {
    /// The shell sandbox can be applied.
    pub fn usable(&self) -> bool {
        self.seatbelt || (self.landlock_abi >= 1 && self.seccomp)
    }

    pub fn describe(&self) -> String {
        platform::describe(self)
    }
}

/// The argv that runs `command` under `policy`.
pub fn wrap(exe: &Path, policy: &Policy, command: &[String]) -> Vec<String> {
    platform::wrap(exe, policy, command)
}

/// Entry point of `mima __sandbox-exec`: apply the policy, then exec the
/// command. Never returns; exit status 126 means the sandbox could not be
/// set up (the command did not run). On macOS it execs `sandbox-exec` with
/// the Seatbelt profile instead (exit status 71 when that cannot apply it).
pub fn exec_main(args: &[String]) -> ! {
    let fail = |msg: String| -> ! {
        eprintln!("mima sandbox: {msg}");
        std::process::exit(126);
    };
    let (policy, command) = match parse_args(args) {
        Ok(v) => v,
        Err(e) => fail(e),
    };
    let command = match platform::prepare(&policy, command) {
        Ok(c) => c,
        Err(e) => fail(e),
    };
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(&command[0])
        .args(&command[1..])
        .exec();
    fail(format!("cannot run {}: {err}", command[0]))
}

fn parse_args(args: &[String]) -> Result<(Policy, Vec<String>), String> {
    // args: [exe, "__sandbox-exec", "--policy", json, "--", command...]
    let rest = args.get(2..).unwrap_or_default();
    match rest {
        [flag, json, sep, command @ ..]
            if flag == "--policy" && sep == "--" && !command.is_empty() =>
        {
            let policy: Policy =
                serde_json::from_str(json).map_err(|e| format!("bad policy: {e}"))?;
            Ok((policy, command.to_vec()))
        }
        _ => Err("usage: mima __sandbox-exec --policy <json> -- <command...>".into()),
    }
}

/// Expands a leading `~/` to the home directory.
pub fn expand_home(p: &str) -> PathBuf {
    match (p.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(p),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "linux")]
    fn args_round_trip() {
        let policy = Policy {
            read: vec!["/usr".into()],
            write: vec!["/tmp/w".into()],
            network: false,
            gpu: true,
        };
        let argv = wrap(
            Path::new("/bin/mima"),
            &policy,
            &["sh".into(), "-c".into(), "echo hi".into()],
        );
        let (p, cmd) = parse_args(&argv).unwrap();
        assert_eq!(p, policy);
        assert_eq!(cmd, vec!["sh", "-c", "echo hi"]);
        assert!(parse_args(&argv[..4]).is_err());
    }

    #[test]
    fn probe_reports_something_sensible() {
        let s = probe();
        assert!(s.landlock_abi >= 0);
        assert!(!s.describe().is_empty());
    }
}
