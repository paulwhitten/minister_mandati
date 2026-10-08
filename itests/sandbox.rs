//! Integration tests for the shell sandbox helper (`mima __sandbox-exec`,
//! src/sandbox.rs), run against the real binary. Skipped, with a message,
//! where the kernel has no Landlock or seccomp.

use std::path::{Path, PathBuf};
use std::process::Command;

fn mima() -> &'static str {
    env!("CARGO_BIN_EXE_mima")
}

fn supported() -> bool {
    // The helper exits 126 when it cannot apply the sandbox.
    let probe = sandboxed(&policy(&[], &[], false), "true");
    probe.0 != Some(126)
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
