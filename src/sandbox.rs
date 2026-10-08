//! Tier 1 shell sandbox (`docs/design/design-shell-sandboxing.md`):
//! Landlock filesystem rules, a seccomp filter, and resource limits, applied
//! by a small helper before it executes the shell command.
//!
//! mima re-runs its own binary as `mima __sandbox-exec --policy <json> --
//! <argv...>`. The helper is entered from `main` before the async runtime
//! starts, so it is single-threaded while it sets up: it lowers resource
//! limits, applies a Landlock ruleset (which also sets no-new-privileges),
//! installs the seccomp filter, and `exec`s the command. Everything the
//! command starts inherits the restrictions; nothing can lift them.
//!
//! - Filesystem: read and execute under system directories, configured
//!   toolchain directories and the workspace; write only in the workspace,
//!   the session's private temp directory and device nodes. The home
//!   directory as a whole is not granted, so `~/.ssh`, `~/.aws` and similar
//!   are unreadable without a deny list.
//! - Network (unless allowed): no sockets other than Unix ones, and no
//!   `connect`/`bind`/`listen`/`accept`, which also blocks DNS over UDP and
//!   local daemons (D-Bus, ssh-agent, the resolver).
//! - Always: no io_uring, no terminal keystroke injection (`TIOCSTI`), no
//!   mounts, kernel keyrings, BPF, module loading or kexec.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// What a sandboxed command may access. Serialized to the helper as JSON.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Policy {
    /// Read and execute (directories recursively).
    pub read: Vec<PathBuf>,
    /// Read, write, create and delete (directories recursively).
    pub write: Vec<PathBuf>,
    /// Allow network access (sockets of any family).
    pub network: bool,
}

/// System locations every command may read and execute.
pub const SYSTEM_READ: &[&str] = &[
    "/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/libx32", "/etc", "/opt", "/var",
    "/proc", "/sys", "/run",
];

/// Locations every command may write: device nodes (DAC still applies, so
/// this is /dev/null, /dev/zero, ttys and, on the Thor, GPU devices).
pub const SYSTEM_WRITE: &[&str] = &["/dev"];

/// What the running kernel supports.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Support {
    /// Landlock ABI version; 0 when Landlock is unavailable or disabled.
    pub landlock_abi: i32,
    /// seccomp filters can be installed.
    pub seccomp: bool,
}

impl Support {
    /// The Tier 1 sandbox can be applied.
    pub fn usable(&self) -> bool {
        self.landlock_abi >= 1 && self.seccomp
    }

    pub fn describe(&self) -> String {
        if self.usable() {
            format!("landlock abi={}, seccomp", self.landlock_abi)
        } else if self.landlock_abi < 1 {
            "unavailable: the kernel has no Landlock".into()
        } else {
            "unavailable: the kernel has no seccomp filters".into()
        }
    }
}

/// Probes the running kernel without changing anything.
pub fn probe() -> Support {
    // landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)
    // returns the ABI version.
    // SAFETY: with a null attribute pointer and size 0 this only queries.
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            1u32,
        )
    };
    // PR_GET_SECCOMP fails with EINVAL when seccomp is not built in.
    // SAFETY: plain query.
    let seccomp = unsafe { libc::prctl(libc::PR_GET_SECCOMP, 0, 0, 0, 0) } >= 0;
    Support {
        landlock_abi: if abi > 0 { abi as i32 } else { 0 },
        seccomp,
    }
}

/// The argv that runs `command` under `policy` through this binary.
pub fn wrap(exe: &Path, policy: &Policy, command: &[String]) -> Vec<String> {
    let mut argv = vec![
        exe.display().to_string(),
        "__sandbox-exec".to_string(),
        "--policy".to_string(),
        serde_json::to_string(policy).unwrap_or_default(),
        "--".to_string(),
    ];
    argv.extend(command.iter().cloned());
    argv
}

/// Entry point of `mima __sandbox-exec`: apply the policy, then exec the
/// command. Never returns; exit status 126 means the sandbox could not be
/// set up (the command did not run).
pub fn exec_main(args: &[String]) -> ! {
    let fail = |msg: String| -> ! {
        eprintln!("mima sandbox: {msg}");
        std::process::exit(126);
    };
    let (policy, command) = match parse_args(args) {
        Ok(v) => v,
        Err(e) => fail(e),
    };
    if let Err(e) = apply(&policy) {
        fail(e);
    }
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

/// Applies limits, Landlock and seccomp to the current (single-threaded)
/// process.
pub fn apply(policy: &Policy) -> Result<(), String> {
    set_limits();
    apply_landlock(policy)?;
    apply_seccomp(policy)?;
    Ok(())
}

fn set_limits() {
    // No core dumps (they could hold secrets read by the command); cap file
    // size and open files without raising anything already lower.
    let lower = |res, value: u64| {
        let mut cur = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: getrlimit/setrlimit on our own process with valid pointers.
        unsafe {
            if libc::getrlimit(res, &mut cur) == 0 && cur.rlim_cur > value {
                cur.rlim_cur = value;
                libc::setrlimit(res, &cur);
            }
        }
    };
    lower(libc::RLIMIT_CORE, 0);
    lower(libc::RLIMIT_FSIZE, 64 << 30);
    lower(libc::RLIMIT_NOFILE, 8192);
}

fn apply_landlock(policy: &Policy) -> Result<(), String> {
    use landlock::{
        ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, Ruleset, RulesetAttr,
        RulesetCreatedAttr, RulesetStatus, Scope, path_beneath_rules,
    };
    let abi = ABI::V6;
    let err = |e: landlock::RulesetError| format!("landlock: {e}");
    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_all(abi))
        .map_err(err)?
        // Signals cannot reach processes outside the sandbox (ABI >= 6).
        .scope(Scope::Signal)
        .map_err(err)?;
    if !policy.network {
        ruleset = ruleset
            .handle_access(AccessNet::BindTcp | AccessNet::ConnectTcp)
            .map_err(err)?
            .scope(Scope::AbstractUnixSocket)
            .map_err(err)?;
    }
    let status = ruleset
        .create()
        .map_err(err)?
        .add_rules(path_beneath_rules(&policy.read, AccessFs::from_read(abi)))
        .map_err(err)?
        .add_rules(path_beneath_rules(&policy.write, AccessFs::from_all(abi)))
        .map_err(err)?
        .restrict_self()
        .map_err(err)?;
    if status.ruleset == RulesetStatus::NotEnforced {
        return Err("landlock: the kernel did not enforce the ruleset".into());
    }
    Ok(())
}

/// System calls denied with EPERM regardless of the policy.
fn always_denied() -> Vec<i64> {
    let mut v = vec![
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_move_mount,
        libc::SYS_open_tree,
        libc::SYS_fsopen,
        libc::SYS_fsconfig,
        libc::SYS_fsmount,
        libc::SYS_fspick,
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_bpf,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_kexec_load,
        libc::SYS_kexec_file_load,
    ];
    v.sort_unstable();
    v.dedup();
    v
}

/// System calls denied when the network is off (besides non-Unix sockets).
fn network_denied() -> Vec<i64> {
    vec![
        libc::SYS_connect,
        libc::SYS_bind,
        libc::SYS_listen,
        libc::SYS_accept,
        libc::SYS_accept4,
    ]
}

fn target_arch() -> Result<seccompiler::TargetArch, String> {
    if cfg!(target_arch = "x86_64") {
        Ok(seccompiler::TargetArch::x86_64)
    } else if cfg!(target_arch = "aarch64") {
        Ok(seccompiler::TargetArch::aarch64)
    } else {
        Err("seccomp: unsupported architecture".into())
    }
}

/// The seccomp program for `policy` (default allow, EPERM on a match).
pub fn seccomp_program(policy: &Policy) -> Result<seccompiler::BpfProgram, String> {
    use seccompiler::{
        SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter, SeccompRule,
    };
    use std::collections::BTreeMap;
    let err = |e: seccompiler::BackendError| format!("seccomp: {e}");
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    let mut nrs = always_denied();
    if !policy.network {
        nrs.extend(network_denied());
    }
    for nr in nrs {
        rules.insert(nr, vec![]); // empty: match unconditionally
    }
    // ioctl(fd, TIOCSTI | TIOCLINUX, ...): keystroke injection into a tty.
    let ioctl_rule = |cmd: u64| -> Result<SeccompRule, String> {
        SeccompRule::new(vec![
            SeccompCondition::new(1, SeccompCmpArgLen::Dword, SeccompCmpOp::Eq, cmd)
                .map_err(err)?,
        ])
        .map_err(err)
    };
    // The ioctl request constants are `c_ulong` on glibc but `c_int` on musl
    // (the static build), so the cast is needed on some targets.
    #[allow(clippy::unnecessary_cast)]
    let (tiocsti, tioclinux) = (libc::TIOCSTI as u64, libc::TIOCLINUX as u64);
    rules.insert(
        libc::SYS_ioctl,
        vec![ioctl_rule(tiocsti)?, ioctl_rule(tioclinux)?],
    );
    if !policy.network {
        // socket(domain, ...) for any domain other than AF_UNIX.
        rules.insert(
            libc::SYS_socket,
            vec![
                SeccompRule::new(vec![
                    SeccompCondition::new(
                        0,
                        SeccompCmpArgLen::Dword,
                        SeccompCmpOp::Ne,
                        libc::AF_UNIX as u64,
                    )
                    .map_err(err)?,
                ])
                .map_err(err)?,
            ],
        );
    }
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        target_arch()?,
    )
    .map_err(err)?;
    filter.try_into().map_err(err)
}

fn apply_seccomp(policy: &Policy) -> Result<(), String> {
    let program = seccomp_program(policy)?;
    seccompiler::apply_filter(&program).map_err(|e| format!("seccomp: {e}"))?;
    deny_x32()
}

/// On x86_64, system calls can also be made through the x32 ABI (numbers
/// with bit 30 set), which the filter above does not match. Deny them all.
#[cfg(target_arch = "x86_64")]
fn deny_x32() -> Result<(), String> {
    use seccompiler::sock_filter;
    const BPF_LD_W_ABS: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
    const BPF_JEQ_K: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
    const BPF_JSET_K: u16 = 0x45; // BPF_JMP | BPF_JSET | BPF_K
    const BPF_RET_K: u16 = 0x06; // BPF_RET | BPF_K
    const AUDIT_ARCH_X86_64: u32 = 62 | 0x8000_0000 | 0x4000_0000;
    const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
    const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
    let ins = |code, jt, jf, k| sock_filter { code, jt, jf, k };
    let program = vec![
        ins(BPF_LD_W_ABS, 0, 0, 4), // seccomp_data.arch
        ins(BPF_JEQ_K, 0, 3, AUDIT_ARCH_X86_64),
        ins(BPF_LD_W_ABS, 0, 0, 0), // seccomp_data.nr
        ins(BPF_JSET_K, 0, 1, 0x4000_0000),
        ins(BPF_RET_K, 0, 0, SECCOMP_RET_ERRNO | libc::EPERM as u32),
        ins(BPF_RET_K, 0, 0, SECCOMP_RET_ALLOW),
    ];
    seccompiler::apply_filter(&program).map_err(|e| format!("seccomp (x32): {e}"))
}

#[cfg(not(target_arch = "x86_64"))]
fn deny_x32() -> Result<(), String> {
    Ok(())
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
    fn args_round_trip() {
        let policy = Policy {
            read: vec!["/usr".into()],
            write: vec!["/tmp/w".into()],
            network: false,
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
    fn seccomp_programs_build_for_both_network_settings() {
        for network in [false, true] {
            let p = Policy {
                read: vec![],
                write: vec![],
                network,
            };
            assert!(!seccomp_program(&p).unwrap().is_empty());
        }
    }

    #[test]
    fn probe_reports_something_sensible() {
        let s = probe();
        assert!(s.landlock_abi >= 0);
        assert!(!s.describe().is_empty());
    }
}
