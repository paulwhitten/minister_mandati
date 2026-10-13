//! macOS shell sandbox: a Seatbelt profile run through `/usr/bin/sandbox-exec`
//! (`docs/design/design-shell-sandboxing.md`, "macOS: Seatbelt").
//!
//! The profile enforces the same `Policy` as Landlock and seccomp on Linux:
//! system files readable, user data (`/Users`, `/Volumes`, per-user temp and
//! cache directories) unreadable except the granted paths, writes only to
//! the granted paths minus git hooks and config, and no network unless
//! allowed. Paths enter only as `-D` parameters, never as profile text.

use super::Policy;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Everything outside user data is readable (see `HIDDEN`).
#[cfg(target_os = "macos")]
pub const SYSTEM_READ: &[&str] = &[];

/// Only /dev/null: commands have no terminal.
#[cfg(target_os = "macos")]
pub const SYSTEM_WRITE: &[&str] = &["/dev/null"];

#[cfg(target_os = "macos")]
pub fn describe(s: &super::Support) -> String {
    if s.seatbelt {
        "seatbelt (sandbox-exec)".into()
    } else {
        "unavailable: /usr/bin/sandbox-exec cannot apply a profile (missing, or mima \
         already runs inside a sandbox)"
            .into()
    }
}

/// Probes the system without changing anything.
#[cfg(target_os = "macos")]
pub fn probe() -> super::Support {
    super::Support {
        landlock_abi: 0,
        seccomp: false,
        seatbelt: available(),
    }
}

/// The argv that runs `command` under `policy`.
#[cfg(target_os = "macos")]
pub fn wrap(_exe: &Path, policy: &Policy, command: &[String]) -> Vec<String> {
    argv(policy, command)
}

/// In the helper: the command to exec is `sandbox-exec` with the profile.
#[cfg(target_os = "macos")]
pub fn prepare(policy: &Policy, command: Vec<String>) -> Result<Vec<String>, String> {
    Ok(argv(policy, &command))
}

/// Always run by absolute path, so a `sandbox-exec` planted on `PATH` cannot
/// replace it.
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Where user data lives; unreadable unless a path below is granted.
const HIDDEN: &[&str] = &["/Users", "/Volumes", "/private/var/folders"];

/// Files in the home directory that CoreFoundation-based tools read at
/// startup (seen in the sandbox violation log).
const HOME_READ: &[&str] = &[
    ".CFUserTextEncoding",
    "Library/Preferences/.GlobalPreferences.plist",
    "Library/Preferences/.GlobalPreferences_m.plist",
];

/// The fixed part of the profile. Every rule after `(deny default)` was added
/// for a denial recorded in the sandbox violation log while running a smoke
/// set (shell, git, cc, make, python3, cargo) on macOS 26.7.1; see the
/// design doc. `com.apple.bsd.dirhelper` is left out on purpose: with it,
/// xcrun tools use the per-user temp directory instead of `TMPDIR`.
const BASE: &str = r#"(version 1)
(deny default)
(allow process-exec process-fork)
(allow signal (target same-sandbox))
(allow process-info* (target same-sandbox))
; read by libSystem at startup in every process
(allow sysctl-read
  (sysctl-name "kern.osproductversion") (sysctl-name "kern.osvariant_status")
  (sysctl-name "kern.iossupportversion") (sysctl-name "security.mac.lockdown_mode_state")
  (sysctl-name "hw.ephemeral_storage"))
; uname, getconf, sysctl, clang, ld, python3, rustup
(allow sysctl-read
  (sysctl-name "hw.ncpu") (sysctl-name "hw.activecpu") (sysctl-name "hw.machine")
  (sysctl-name "hw.pagesize_compat") (sysctl-name-prefix "hw.optional.")
  (sysctl-name "machdep.cpu.brand_string")
  (sysctl-name "kern.argmax") (sysctl-name "kern.hostname") (sysctl-name "kern.ngroups")
  (sysctl-name "kern.ostype") (sysctl-name "kern.osrelease") (sysctl-name "kern.version")
  (sysctl-name "kern.sysv.semmns") (sysctl-name-prefix "sysctl.oidfmt."))
; user and group lookups (id, whoami, git, make); the membership service is
; left out: with it, xcrun tools again try the per-user temp directory
(allow mach-lookup (global-name "com.apple.system.opendirectoryd.libinfo"))
; python3 multiprocessing
(allow ipc-posix-sem)
; CoreFoundation tools (curl)
(allow user-preference-read)
(allow file-read*)
"#;

/// The profile text and its `-D` parameters for `policy`. `home` is the
/// user's home directory; `protected` are paths inside writable roots that
/// stay read-only (git hooks and config). All paths should be canonical:
/// Seatbelt matches the resolved path (`/private/var/...`, not `/var/...`).
pub fn profile(
    policy: &Policy,
    home: Option<&Path>,
    protected: &[PathBuf],
) -> (String, Vec<(String, String)>) {
    let mut params: Vec<(String, String)> = Vec::new();
    let mut text = String::from(BASE);
    // Adds one rule with a `(param ...)` per path; nothing when empty.
    let mut rule = |action: &str, filter: &str, prefix: &str, paths: &[PathBuf]| {
        if paths.is_empty() {
            return;
        }
        text.push_str(&format!("({action}"));
        for (i, p) in paths.iter().enumerate() {
            let name = format!("{prefix}_{i}");
            text.push_str(&format!(" ({filter} (param \"{name}\"))"));
            params.push((name, p.display().to_string()));
        }
        text.push_str(")\n");
    };

    let hidden: Vec<PathBuf> = HIDDEN.iter().map(PathBuf::from).collect();
    rule("deny file-read*", "subpath", "HIDDEN", &hidden);

    let home_files: Vec<PathBuf> = home
        .map(|h| HOME_READ.iter().map(|f| h.join(f)).collect())
        .unwrap_or_default();
    rule("allow file-read*", "literal", "HOME_FILE", &home_files);

    let granted: BTreeSet<&PathBuf> = policy.read.iter().chain(&policy.write).collect();
    let ancestors: Vec<PathBuf> = granted
        .iter()
        .flat_map(|p| p.ancestors().skip(1))
        .map(Path::to_path_buf)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    rule(
        "allow file-read-metadata",
        "literal",
        "ANCESTOR",
        &ancestors,
    );

    let granted: Vec<PathBuf> = granted.into_iter().cloned().collect();
    rule("allow file-read*", "subpath", "READ", &granted);
    rule("allow file-write*", "subpath", "WRITE", &policy.write);
    rule("deny file-write*", "subpath", "PROTECTED", protected);

    if policy.network {
        // HTTPS certificate checks fail without it (measured on macOS 26.7.1).
        text.push_str(
            "(allow network*)\n(allow mach-lookup (global-name \"com.apple.TrustEvaluationAgent\"))\n",
        );
    }
    (text, params)
}

/// Paths in each writable root that stay read-only: `.git/hooks` and
/// `.git/config`, also when they do not exist yet. When `.git` is a file
/// (a worktree or submodule), the file itself and the hooks and config of
/// the directory it points to, and of that directory's common directory.
pub fn protected_paths(write: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = BTreeSet::new();
    for root in write.iter().filter(|r| r.is_dir()) {
        let dotgit = root.join(".git");
        let mut gitdirs = vec![];
        if dotgit.is_file() {
            out.insert(dotgit.clone());
            if let Some(dir) = std::fs::read_to_string(&dotgit)
                .ok()
                .and_then(|s| {
                    s.trim()
                        .strip_prefix("gitdir:")
                        .map(|d| root.join(d.trim()))
                })
                .and_then(|d| d.canonicalize().ok())
            {
                if let Some(common) = std::fs::read_to_string(dir.join("commondir"))
                    .ok()
                    .and_then(|c| dir.join(c.trim()).canonicalize().ok())
                {
                    gitdirs.push(common);
                }
                gitdirs.push(dir);
            }
        } else {
            gitdirs.push(dotgit);
        }
        for g in gitdirs {
            out.insert(g.join("hooks"));
            out.insert(g.join("config"));
        }
    }
    out.into_iter().collect()
}

/// The `sandbox-exec` argv that runs `command` under `policy`.
pub fn argv(policy: &Policy, command: &[String]) -> Vec<String> {
    let canon = |p: &PathBuf| p.canonicalize().unwrap_or_else(|_| p.clone());
    let policy = Policy {
        read: policy.read.iter().map(canon).collect(),
        write: policy.write.iter().map(canon).collect(),
        network: policy.network,
    };
    let home = std::env::var_os("HOME").map(|h| canon(&PathBuf::from(h)));
    let protected = protected_paths(&policy.write);
    let (text, params) = profile(&policy, home.as_deref(), &protected);
    let mut argv = vec![SANDBOX_EXEC.to_string(), "-p".to_string(), text];
    for (k, v) in params {
        argv.push("-D".to_string());
        argv.push(format!("{k}={v}"));
    }
    argv.push("--".to_string());
    argv.extend(command.iter().cloned());
    argv
}

/// Whether `sandbox-exec` can apply a profile here. It cannot when it is
/// missing or when this process already runs inside a sandbox (Seatbelt does
/// not nest).
#[cfg(target_os = "macos")]
pub fn available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        std::process::Command::new(SANDBOX_EXEC)
            .args(["-p", "(version 1)(allow default)", "/usr/bin/true"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(network: bool) -> Policy {
        Policy {
            read: vec!["/opt/tools".into()],
            write: vec!["/work/repo".into(), "/dev/null".into()],
            network,
        }
    }

    #[test]
    fn paths_are_parameters_not_profile_text() {
        let p = Policy {
            read: vec![],
            write: vec![r#"/work/a") (allow default) ("#.into()],
            network: false,
        };
        let (text, params) = profile(&p, None, &[]);
        assert!(!text.contains("/work"), "{text}");
        assert!(!text.contains("(allow default)"), "{text}");
        assert!(params.iter().any(|(_, v)| v.contains("(allow default)")));
    }

    #[test]
    fn denies_come_after_the_grants_they_override() {
        let protected = vec![PathBuf::from("/work/repo/.git/hooks")];
        let (text, params) = profile(&policy(false), Some(Path::new("/Users/u")), &protected);
        let pos = |s: &str| {
            text.find(s)
                .unwrap_or_else(|| panic!("{s} missing: {text}"))
        };
        assert!(pos("(allow file-read*)\n") < pos("(deny file-read*"));
        assert!(pos("(deny file-read*") < pos("(allow file-read* (subpath"));
        assert!(pos("(allow file-write*") < pos("(deny file-write*"));
        let has = |k: &str, v: &str| params.iter().any(|(a, b)| a.starts_with(k) && b == v);
        assert!(has("HIDDEN", "/Users"));
        assert!(has("HOME_FILE", "/Users/u/.CFUserTextEncoding"));
        assert!(has("READ", "/opt/tools"));
        assert!(has("READ", "/work/repo"), "writable paths are readable");
        assert!(has("WRITE", "/work/repo"));
        assert!(has("ANCESTOR", "/work"));
        assert!(has("ANCESTOR", "/"));
        assert!(!has("ANCESTOR", "/work/repo"));
        assert!(has("PROTECTED", "/work/repo/.git/hooks"));
    }

    #[test]
    fn network_only_when_allowed() {
        let off = profile(&policy(false), None, &[]).0;
        assert!(!off.contains("network") && !off.contains("TrustEvaluation"));
        let on = profile(&policy(true), None, &[]).0;
        assert!(on.contains("(allow network*)") && on.contains("TrustEvaluationAgent"));
    }

    #[test]
    fn git_hooks_and_config_are_protected() {
        let base = std::env::temp_dir().join(format!("mima-seatbelt-{}", std::process::id()));
        let (repo, wt, main_git) = (base.join("repo"), base.join("wt"), base.join("main.git"));
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(main_git.join("worktrees/wt")).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: ../main.git/worktrees/wt\n").unwrap();
        std::fs::write(main_git.join("worktrees/wt/commondir"), "../..\n").unwrap();
        let base = base.canonicalize().unwrap();
        let got = protected_paths(&[base.join("repo"), base.join("wt")]);
        for p in [
            "repo/.git/hooks",
            "repo/.git/config",
            "wt/.git",
            "main.git/hooks",
            "main.git/config",
            "main.git/worktrees/wt/hooks",
            "main.git/worktrees/wt/config",
        ] {
            assert!(got.contains(&base.join(p)), "{p} missing: {got:?}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn argv_runs_sandbox_exec_by_absolute_path() {
        let argv = argv(&policy(false), &["sh".into(), "-c".into(), "true".into()]);
        assert_eq!(argv[0], SANDBOX_EXEC);
        assert_eq!(argv[1], "-p");
        let sep = argv.iter().position(|a| a == "--").unwrap();
        assert_eq!(&argv[sep + 1..], ["sh", "-c", "true"]);
        assert!(
            argv[3..sep]
                .chunks(2)
                .all(|c| c[0] == "-D" && c[1].contains('='))
        );
    }
}
