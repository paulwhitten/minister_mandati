//! Integration tests for the shell sandbox helper (`mima __sandbox-exec`,
//! src/sandbox/), run against the real binary: Landlock and seccomp on
//! Linux, Seatbelt on macOS. Skipped, with a message, where the sandbox
//! cannot be applied.

use std::path::{Path, PathBuf};
use std::process::Command;

fn mima() -> &'static str {
    env!("CARGO_BIN_EXE_mima")
}

fn supported() -> bool {
    // The helper exits 126 (Linux) or 71 (macOS) when it cannot apply it.
    let probe = sandboxed(&policy(&[], &[], false), "true");
    probe.0 == Some(0)
}

fn policy(read: &[&Path], write: &[&Path], network: bool) -> String {
    let sys = [
        "/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc", "/proc", "/sys", "/run",
    ];
    let mut r: Vec<String> = sys.iter().map(|s| s.to_string()).collect();
    r.extend(read.iter().map(|p| p.display().to_string()));
    let mut w: Vec<String> = vec!["/dev".into()];
    w.extend(write.iter().map(|p| p.display().to_string()));
    serde_json::json!({ "read": r, "write": w, "network": network }).to_string()
}

/// Runs `script` under `policy`; returns (exit code, stdout, stderr).
fn sandboxed(policy: &str, script: &str) -> (Option<i32>, String, String) {
    sandboxed_env(policy, script, &[])
}

/// Like `sandboxed`, with extra environment variables.
fn sandboxed_env(
    policy: &str,
    script: &str,
    env: &[(&str, &Path)],
) -> (Option<i32>, String, String) {
    let out = Command::new(mima())
        .args([
            "__sandbox-exec",
            "--policy",
            policy,
            "--",
            "sh",
            "-c",
            script,
        ])
        .envs(env.iter().map(|(k, v)| (k, v)))
        .output()
        .expect("run mima");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("mima-sbx-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

macro_rules! require_sandbox {
    () => {
        if !supported() {
            eprintln!("Landlock or seccomp unavailable; skipping");
            return;
        }
    };
}

#[test]
fn workspace_is_writable_and_the_rest_is_not() {
    require_sandbox!();
    let base = scratch("fs");
    let (ws, secret) = (base.join("ws"), base.join("secret"));
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&secret).unwrap();
    std::fs::write(secret.join("id_ed25519"), "key").unwrap();
    std::fs::write(ws.join("f.txt"), "hello").unwrap();
    // A symlink inside the workspace does not open the secret either.
    std::os::unix::fs::symlink(secret.join("id_ed25519"), ws.join("link")).unwrap();
    let p = policy(&[], &[&ws], false);
    let ok = |s: &str| sandboxed(&p, s).0 == Some(0);
    assert!(ok(&format!("cat {}/f.txt", ws.display())));
    assert!(ok(&format!("echo x > {}/new.txt", ws.display())));
    assert!(
        ok(&format!("echo y > {}/f.txt", ws.display())),
        "truncate an existing file"
    );
    assert!(!ok(&format!("cat {}/id_ed25519", secret.display())));
    assert!(!ok(&format!("cat {}/link", ws.display())));
    assert!(!ok(&format!("echo x > {}/outside.txt", base.display())));
    assert!(!base.join("outside.txt").exists());
}

#[test]
#[cfg(target_os = "linux")]
fn network_is_off_unless_allowed() {
    require_sandbox!();
    if Command::new("python3").arg("--version").output().is_err() {
        eprintln!("python3 missing; skipping");
        return;
    }
    let tcp = "python3 -c 'import socket; socket.socket(socket.AF_INET, socket.SOCK_STREAM)'";
    let udp = "python3 -c 'import socket; socket.socket(socket.AF_INET, socket.SOCK_DGRAM)'";
    let unix = "python3 -c 'import socket; a, b = socket.socketpair(); a.send(b\"x\"); assert b.recv(1) == b\"x\"'";
    let off = policy(&[], &[], false);
    assert_ne!(sandboxed(&off, tcp).0, Some(0), "TCP socket created");
    assert_ne!(sandboxed(&off, udp).0, Some(0), "UDP socket created");
    assert_eq!(sandboxed(&off, unix).0, Some(0), "socketpair must work");
    let on = policy(&[], &[], true);
    assert_eq!(
        sandboxed(&on, tcp).0,
        Some(0),
        "network = true allows sockets"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn unix_connect_to_local_daemons_is_denied() {
    require_sandbox!();
    let base = scratch("unix");
    let sock = base.join("s.sock");
    // A listener outside the sandbox stands in for D-Bus or ssh-agent.
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let p = policy(&[&base], &[], false);
    let script = format!(
        "python3 -c 'import socket; s = socket.socket(socket.AF_UNIX); s.connect(\"{}\")'",
        sock.display()
    );
    assert_ne!(sandboxed(&p, &script).0, Some(0));
    drop(listener);
}

#[test]
fn restrictions_cannot_be_lifted_by_children() {
    require_sandbox!();
    let base = scratch("nest");
    // A nested helper may only narrow, never widen: the outer policy still
    // applies to whatever the inner one grants.
    let inner = policy(&[&base], &[&base], true);
    let outer = policy(&[], &[], false);
    let script = format!(
        "{} __sandbox-exec --policy '{}' -- sh -c 'echo x > {}/f'",
        mima(),
        inner,
        base.display()
    );
    let (code, _, _) = sandboxed(&outer, &script);
    assert_ne!(code, Some(0));
    assert!(!base.join("f").exists());
}

#[test]
fn bad_usage_fails_closed() {
    let out = Command::new(mima())
        .args(["__sandbox-exec", "--policy", "not json", "--", "true"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(126));
}

/// macOS: a scratch workspace plus a private TMPDIR, which the xcrun shims
/// (`/usr/bin/git`, `/usr/bin/cc`) need, as mima provides in production.
#[cfg(target_os = "macos")]
fn mac_workspace(name: &str) -> (PathBuf, PathBuf, String) {
    let base = scratch(name);
    let (ws, tmp) = (base.join("ws"), base.join("tmp"));
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&tmp).unwrap();
    // git fails when ~/.gitconfig exists but is unreadable; mima grants it by default.
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    let (gitconfig, gitdir) = (home.join(".gitconfig"), home.join(".config/git"));
    let p = policy(&[&gitconfig, &gitdir], &[&ws, &tmp], false);
    (ws, tmp, p)
}

#[test]
#[cfg(target_os = "macos")]
fn mac_home_secrets_are_unreadable_but_cf_files_are() {
    require_sandbox!();
    let base = scratch("home");
    let home = base.join("home");
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    std::fs::write(home.join(".ssh/id_ed25519"), "key").unwrap();
    std::fs::write(home.join(".zshrc"), "rc").unwrap();
    std::fs::write(home.join(".CFUserTextEncoding"), "0x0:0:0").unwrap();
    let p = policy(&[], &[], false);
    let run = |s: &str| sandboxed_env(&p, s, &[("HOME", &home)]).0;
    assert_ne!(
        run(&format!("cat {}/.ssh/id_ed25519", home.display())),
        Some(0)
    );
    assert_ne!(run(&format!("cat {}/.zshrc", home.display())), Some(0));
    assert_ne!(run(&format!("ls {}", home.display())), Some(0));
    assert_eq!(
        run(&format!("cat {}/.CFUserTextEncoding", home.display())),
        Some(0)
    );
}

#[test]
#[cfg(target_os = "macos")]
fn mac_git_works_but_hooks_and_config_are_read_only() {
    require_sandbox!();
    let (ws, tmp, p) = mac_workspace("git");
    let git = |args: &[&str]| {
        let ok = Command::new("git").args(args).current_dir(&ws).output();
        assert!(ok.is_ok_and(|o| o.status.success()), "git {args:?}");
    };
    git(&["init", "-q"]);
    std::fs::write(ws.join("a.txt"), "a").unwrap();
    let run = |s: &str| {
        let script = format!("cd {} && {s}", ws.display());
        sandboxed_env(&p, &script, &[("TMPDIR", &tmp)])
    };
    let (code, out, err) = run(
        "git add a.txt && git -c user.name=t -c user.email=t@t commit -qm init && git log --oneline",
    );
    assert_eq!(code, Some(0), "commit inside the sandbox: {out} {err}");
    assert_ne!(run("echo x > .git/hooks/pre-commit").0, Some(0));
    assert_ne!(run("echo x >> .git/config").0, Some(0));
    assert_ne!(run("git config core.hooksPath /tmp/h").0, Some(0));
    assert!(!ws.join(".git/hooks/pre-commit").exists());
}

#[test]
#[cfg(target_os = "macos")]
fn mac_c_toolchain_works() {
    require_sandbox!();
    let (ws, tmp, p) = mac_workspace("cc");
    std::fs::write(ws.join("m.c"), "int main(void){return 3;}\n").unwrap();
    let script = format!("cd {} && cc m.c -o m && ./m", ws.display());
    let (code, out, err) = sandboxed_env(&p, &script, &[("TMPDIR", &tmp)]);
    assert_eq!(code, Some(3), "{out} {err}");
}

#[test]
#[cfg(target_os = "macos")]
fn mac_python_multiprocessing_works() {
    require_sandbox!();
    let (ws, tmp, p) = mac_workspace("py");
    let script = format!(
        "cd {} && /usr/bin/python3 -c 'from concurrent.futures import ProcessPoolExecutor as P; print(list(P(2).map(abs, [-1, -2])))'",
        ws.display()
    );
    let (code, out, err) = sandboxed_env(&p, &script, &[("TMPDIR", &tmp)]);
    assert_eq!(code, Some(0), "{out} {err}");
    assert!(out.contains("[1, 2]"), "{out}");
}

#[test]
#[cfg(target_os = "macos")]
fn mac_firmlink_spelling_does_not_bypass_denies() {
    require_sandbox!();
    // /private/var/folders (hidden) is also reachable as /System/Volumes/Data/private/var/folders.
    let base = scratch("firmlink");
    std::fs::write(base.join("secret.txt"), "s").unwrap();
    let alt = PathBuf::from(format!("/System/Volumes/Data{}", base.display())).join("secret.txt");
    assert!(alt.exists(), "firmlink path missing: {}", alt.display());
    let p = policy(&[], &[], false);
    assert_ne!(sandboxed(&p, &format!("cat {}", alt.display())).0, Some(0));
    assert_ne!(
        sandboxed(&p, &format!("cat {}/secret.txt", base.display())).0,
        Some(0)
    );
}

#[test]
#[cfg(target_os = "macos")]
fn mac_network_is_off_unless_allowed() {
    require_sandbox!();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let tcp = format!("/usr/bin/nc -z -w 2 127.0.0.1 {port}");
    assert_ne!(sandboxed(&policy(&[], &[], false), &tcp).0, Some(0));
    assert_eq!(
        sandboxed(&policy(&[], &[], true), &tcp).0,
        Some(0),
        "network = true allows connections"
    );
}

#[test]
#[cfg(target_os = "macos")]
fn mac_unix_connect_to_local_daemons_is_denied() {
    require_sandbox!();
    let base = scratch("unix");
    let sock = base.join("s.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let script = format!("echo x | /usr/bin/nc -U -w 2 {}", sock.display());
    assert_ne!(sandboxed(&policy(&[&base], &[], false), &script).0, Some(0));
}
