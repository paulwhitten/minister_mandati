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
    // As in mima's Linux SYSTEM_READ. Not on macOS: the temp directory, where the
    // tests plant their secrets, is under /var there.
    if cfg!(target_os = "linux") {
        r.extend(["/opt".to_string(), "/var".to_string()]);
    }
    r.extend(read.iter().map(|p| p.display().to_string()));
    let mut w: Vec<String> = vec!["/dev".into()];
    w.extend(write.iter().map(|p| p.display().to_string()));
    serde_json::json!({ "read": r, "write": w, "network": network }).to_string()
}

/// `policy` with GPU access, as mima builds it for `[security].sandbox_gpu`:
/// the flag plus `/proc` in the writable list.
#[cfg(target_os = "linux")]
fn with_gpu(policy: &str) -> String {
    let mut v: serde_json::Value = serde_json::from_str(policy).unwrap();
    v["gpu"] = true.into();
    v["write"].as_array_mut().unwrap().push("/proc".into());
    v.to_string()
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

#[cfg(target_os = "linux")]
macro_rules! require_python {
    () => {
        if Command::new("python3").arg("--version").output().is_err() {
            eprintln!("python3 missing; skipping");
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
    require_python!();
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
    require_python!();
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
    // applies to whatever the inner one grants. On Linux the outer policy can
    // run the helper (mima adds its own binary to the read list), so the write
    // fails because of the outer policy, not because the helper could not
    // start. Seatbelt does not nest, so on macOS the inner helper refuses (71).
    let inner = policy(&[&base], &[&base], true);
    let outer = policy(&[Path::new(mima())], &[], false);
    let nested = |script: &str| {
        format!(
            "{} __sandbox-exec --policy '{}' -- sh -c '{script}'",
            mima(),
            inner
        )
    };
    if cfg!(target_os = "linux") {
        assert_eq!(
            sandboxed(&outer, &nested("true")).0,
            Some(0),
            "nested helper runs"
        );
    }
    let (code, _, _) = sandboxed(&outer, &nested(&format!("echo x > {}/f", base.display())));
    assert_ne!(code, Some(0));
    assert!(!base.join("f").exists());
}

/// The kernel's Landlock ABI version (0 when unavailable).
#[cfg(target_os = "linux")]
fn landlock_abi() -> i64 {
    const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
    // SAFETY: the version query takes no ruleset attributes.
    let v = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<u8>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    v.max(0)
}

#[cfg(target_os = "linux")]
fn have(tool: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {tool}")])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// A process outside the sandbox, killed on drop.
#[cfg(target_os = "linux")]
struct Outside(std::process::Child);

#[cfg(target_os = "linux")]
impl Outside {
    fn sleep() -> Self {
        Outside(Command::new("sleep").arg("60").spawn().unwrap())
    }
    fn pid(&self) -> u32 {
        self.0.id()
    }
}

#[cfg(target_os = "linux")]
impl Drop for Outside {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Linux: a scratch workspace plus a private TMPDIR, with mima's default
/// read paths for toolchains and git configuration.
#[cfg(target_os = "linux")]
fn linux_workspace(name: &str) -> (PathBuf, PathBuf, String) {
    let base = scratch(name);
    let (ws, tmp) = (base.join("ws"), base.join("tmp"));
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&tmp).unwrap();
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    let read: Vec<PathBuf> = [
        ".cargo",
        ".rustup",
        ".local/bin",
        ".local/lib",
        ".gitconfig",
        ".config/git",
    ]
    .iter()
    .map(|p| home.join(p))
    .filter(|p| p.exists())
    .collect();
    let read: Vec<&Path> = read.iter().map(PathBuf::as_path).collect();
    let p = policy(&read, &[&ws, &tmp], false);
    (ws, tmp, p)
}

#[cfg(target_os = "linux")]
fn in_workspace(ws: &Path, tmp: &Path, p: &str, script: &str) -> (Option<i32>, String, String) {
    let script = format!("cd {} && {script}", ws.display());
    sandboxed_env(p, &script, &[("TMPDIR", tmp)])
}

#[test]
#[cfg(target_os = "linux")]
fn linux_tcp_connect_is_off_unless_allowed() {
    require_sandbox!();
    require_python!();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let tcp =
        format!("python3 -c 'import socket; socket.create_connection((\"127.0.0.1\", {port}), 2)'");
    assert_ne!(sandboxed(&policy(&[], &[], false), &tcp).0, Some(0));
    assert_eq!(
        sandboxed(&policy(&[], &[], true), &tcp).0,
        Some(0),
        "network = true allows connections"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn linux_abstract_unix_sockets_are_unreachable_with_the_network_off() {
    require_sandbox!();
    require_python!();
    use std::os::linux::net::SocketAddrExt;
    let name = format!("mima-sbx-{}", std::process::id());
    let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
    let _listener = std::os::unix::net::UnixListener::bind_addr(&addr).unwrap();
    let script = format!(
        "python3 -c 'import socket; s = socket.socket(socket.AF_UNIX); s.connect(\"\\0{name}\")'"
    );
    // Landlock scopes abstract sockets only from ABI 6; the connect deny
    // covers older kernels.
    assert_ne!(sandboxed(&policy(&[], &[], false), &script).0, Some(0));
    assert_eq!(
        sandboxed(&policy(&[], &[], true), &script).0,
        Some(0),
        "network = true allows the connect"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn linux_signals_reach_outside_processes_only_before_landlock_abi_6() {
    require_sandbox!();
    let outside = Outside::sleep();
    let (code, _, err) = sandboxed(
        &policy(&[], &[], false),
        &format!("kill -0 {}", outside.pid()),
    );
    let abi = landlock_abi();
    eprintln!("landlock abi {abi}: kill -0 outside exit {code:?} {err}");
    // Signal scoping needs ABI 6. Below it this is a known gap (see
    // design-shell-sandboxing.md, "Degrading on older or restricted kernels").
    assert_eq!(code != Some(0), abi >= 6);
}

#[test]
#[cfg(target_os = "linux")]
fn linux_tiocsti_is_denied() {
    require_sandbox!();
    require_python!();
    // The child gets a fresh pty as its controlling terminal, where TIOCSTI
    // would work on hosts with dev.tty.legacy_tiocsti = 1 (the Thor).
    // seccomp returns EPERM (1); without the filter the kernel returns 0 or,
    // with legacy TIOCSTI off, EIO (5).
    let script = "python3 -c '
import fcntl, os, pty, sys, termios
pid, fd = pty.fork()
if pid == 0:
    try:
        fcntl.ioctl(0, termios.TIOCSTI, b\"x\")
        os._exit(0)
    except OSError as e:
        os._exit(e.errno)
sys.exit(os.waitstatus_to_exitcode(os.waitpid(pid, 0)[1]))
'";
    let (code, out, err) = sandboxed(&policy(&[], &[], false), script);
    assert_eq!(code, Some(libc::EPERM), "{out} {err}");
}

#[test]
#[cfg(target_os = "linux")]
fn linux_tracing_outside_fails_but_debuggers_work_inside() {
    require_sandbox!();
    require_python!();
    let outside = Outside::sleep();
    let attach = format!(
        "python3 -c 'import ctypes, sys; c = ctypes.CDLL(None, use_errno=True); sys.exit(0 if c.ptrace(16, {}, 0, 0) == 0 else 3)'",
        outside.pid()
    );
    assert_eq!(
        sandboxed(&policy(&[], &[], false), &attach).0,
        Some(3),
        "PTRACE_ATTACH to a process outside the sandbox"
    );
    let (ws, tmp, p) = linux_workspace("trace");
    if have("strace") {
        let (code, out, err) = in_workspace(&ws, &tmp, &p, "strace -f -o trace.txt true");
        assert_eq!(code, Some(0), "strace inside: {out} {err}");
    } else {
        eprintln!("strace missing; skipping that part");
    }
    if have("gdb") {
        let (code, out, err) =
            in_workspace(&ws, &tmp, &p, "gdb -batch -nx -ex run --args /bin/true");
        assert_eq!(code, Some(0), "gdb inside: {out} {err}");
        assert!(out.contains("exited normally"), "{out} {err}");
    } else {
        eprintln!("gdb missing; skipping that part");
    }
}

#[test]
#[cfg(target_os = "linux")]
fn linux_git_commit_works() {
    require_sandbox!();
    let (ws, tmp, p) = linux_workspace("git");
    let ok = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&ws)
        .output();
    assert!(ok.is_ok_and(|o| o.status.success()));
    std::fs::write(ws.join("a.txt"), "a").unwrap();
    let (code, out, err) = in_workspace(
        &ws,
        &tmp,
        &p,
        "git add a.txt && git -c user.name=t -c user.email=t@t commit -qm init && git status --short && git log --oneline",
    );
    assert_eq!(code, Some(0), "commit inside the sandbox: {out} {err}");
}

#[test]
#[cfg(target_os = "linux")]
fn linux_make_and_cc_work() {
    require_sandbox!();
    let (ws, tmp, p) = linux_workspace("make");
    std::fs::write(ws.join("m.c"), "int main(void){return 3;}\n").unwrap();
    std::fs::write(ws.join("Makefile"), "m: m.c\n\tcc m.c -o m\n").unwrap();
    let (code, out, err) = in_workspace(&ws, &tmp, &p, "make -s && ./m");
    assert_eq!(code, Some(3), "{out} {err}");
}

#[test]
#[cfg(target_os = "linux")]
fn linux_python_multiprocessing_and_pytest_work() {
    require_sandbox!();
    require_python!();
    let (ws, tmp, p) = linux_workspace("py");
    let (code, out, err) = in_workspace(
        &ws,
        &tmp,
        &p,
        "python3 -c 'from concurrent.futures import ProcessPoolExecutor as P; print(list(P(2).map(abs, [-1, -2])))'",
    );
    assert_eq!(code, Some(0), "{out} {err}");
    assert!(out.contains("[1, 2]"), "{out}");
    if !have("pytest") {
        eprintln!("pytest missing; skipping that part");
        return;
    }
    std::fs::write(
        ws.join("test_a.py"),
        "def test_a():\n    assert 1 + 1 == 2\n",
    )
    .unwrap();
    let (code, out, err) = in_workspace(&ws, &tmp, &p, "pytest -q -p no:cacheprovider");
    assert_eq!(code, Some(0), "pytest: {out} {err}");
}

#[test]
#[cfg(target_os = "linux")]
fn linux_cargo_build_works() {
    require_sandbox!();
    if !have("cargo") {
        eprintln!("cargo missing; skipping");
        return;
    }
    let (ws, tmp, p) = linux_workspace("cargo");
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::write(
        ws.join("Cargo.toml"),
        "[package]\nname = \"t\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        ws.join("src/main.rs"),
        "fn main() { std::process::exit(3) }\n",
    )
    .unwrap();
    // A CARGO_TARGET_DIR inherited from the test run points outside the workspace.
    let script = format!(
        "cd {} && cargo build -q --offline && ./target/debug/t",
        ws.display()
    );
    let target = ws.join("target");
    let env: [(&str, &Path); 2] = [("TMPDIR", &tmp), ("CARGO_TARGET_DIR", &target)];
    let (code, out, err) = sandboxed_env(&p, &script, &env);
    assert_eq!(code, Some(3), "{out} {err}");
}

#[test]
#[cfg(target_os = "linux")]
fn linux_datagrams_to_local_daemons_are_refused_with_the_network_off() {
    require_sandbox!();
    require_python!();
    // A datagram socket outside stands in for /dev/log or journald.
    let base = scratch("dgram");
    let sock = base.join("log.sock");
    let daemon = std::os::unix::net::UnixDatagram::bind(&sock).unwrap();
    daemon.set_nonblocking(true).unwrap();
    let send = format!(
        "python3 -c 'import socket; socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM).sendto(b\"x\", \"{}\")'",
        sock.display()
    );
    let off = policy(&[&base], &[], false);
    assert_ne!(sandboxed(&off, &send).0, Some(0), "sendto refused");
    assert_ne!(
        sandboxed(&with_gpu(&off), &send).0,
        Some(0),
        "also with GPU access"
    );
    let mut buf = [0u8; 8];
    assert!(daemon.recv(&mut buf).is_err(), "nothing arrived");
    let on = policy(&[&base], &[], true);
    assert_eq!(sandboxed(&on, &send).0, Some(0), "network = true allows it");
    assert_eq!(daemon.recv(&mut buf).ok(), Some(1));
}

#[test]
#[cfg(target_os = "linux")]
fn linux_gpu_access_opens_proc_and_unix_servers_but_not_connect() {
    require_sandbox!();
    require_python!();
    let comm = "echo t > /proc/self/task/$$/comm";
    let off = policy(&[], &[], false);
    assert_ne!(sandboxed(&off, comm).0, Some(0), "/proc is read-only");
    assert_eq!(sandboxed(&with_gpu(&off), comm).0, Some(0), "thread name");
    let server = "python3 -c 'import socket; s = socket.socket(socket.AF_UNIX); s.bind(\"\\0mima-sbx-gpu-%d\" % __import__(\"os\").getpid()); s.listen(1)'";
    assert_ne!(sandboxed(&off, server).0, Some(0), "bind refused");
    assert_eq!(
        sandboxed(&with_gpu(&off), server).0,
        Some(0),
        "bind allowed"
    );
    // Clients stay refused, and only Unix sockets can be created.
    let base = scratch("gpu");
    let sock = base.join("s.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let connect = format!(
        "python3 -c 'import socket; s = socket.socket(socket.AF_UNIX); s.connect(\"{}\")'",
        sock.display()
    );
    let gpu = with_gpu(&policy(&[&base], &[], false));
    assert_ne!(sandboxed(&gpu, &connect).0, Some(0), "connect refused");
    let tcp = "python3 -c 'import socket; socket.socket(socket.AF_INET, socket.SOCK_STREAM)'";
    assert_ne!(sandboxed(&gpu, tcp).0, Some(0), "TCP socket refused");
}

/// CUDA driver calls (initialize, count devices, create a context, allocate
/// 1 MiB) through ctypes, so no compiler is needed. Skipped where the driver
/// is missing.
#[test]
#[cfg(target_os = "linux")]
#[ignore = "uses the GPU; reserve it, then run with --ignored"]
fn linux_cuda_driver_works() {
    require_sandbox!();
    let script = "python3 -c 'import ctypes; c = ctypes.CDLL(\"libcuda.so.1\"); assert c.cuInit(0) == 0; n = ctypes.c_int(); assert c.cuDeviceGetCount(ctypes.byref(n)) == 0; x = ctypes.c_void_p(); assert c.cuCtxCreate_v2(ctypes.byref(x), 0, 0) == 0; p = ctypes.c_uint64(); assert c.cuMemAlloc_v2(ctypes.byref(p), 1 << 20) == 0; print(n.value)'";
    let control = Command::new("sh").args(["-c", script]).output().unwrap();
    if !control.status.success() {
        eprintln!("no usable CUDA driver outside the sandbox; skipping");
        return;
    }
    let off = policy(&[], &[], false);
    assert_ne!(
        sandboxed(&off, script).0,
        Some(0),
        "fails without GPU access"
    );
    let (code, out, err) = sandboxed(&with_gpu(&off), script);
    assert_eq!(code, Some(0), "{out} {err}");
    assert_eq!(
        out,
        String::from_utf8_lossy(&control.stdout),
        "device count"
    );
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
