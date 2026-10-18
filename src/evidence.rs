//! Evidence for the audit record (docs/design/design-audit-record.md): the
//! provenance fields of `session_start`, the workspace's git state, and the
//! files a shell command changed.
//!
//! git runs through the tool registry's `CommandRunner`, so it has the same
//! sandbox, environment and timeout as the model's own shell commands: a
//! workspace's `.git/config` (an fsmonitor hook or a filter driver) gains
//! nothing it could not already do through `execute_bash`.

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use crate::audit::{hex, sha256};
use crate::context::AgentContext;
use crate::tools::CommandRunner;

/// Longest `git status` run before it is abandoned.
const GIT_TIMEOUT: Duration = Duration::from_secs(10);
/// Most changed paths recorded per shell command.
pub const MAX_CHANGED_PATHS: usize = 1000;
/// Files larger than this are recorded by size only, not hashed.
const MAX_HASHED_BYTES: u64 = 64 << 20;

/// Build metadata embedded by `build.rs`.
pub fn build_info() -> Value {
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "git_commit": env!("MIMA_GIT_COMMIT"),
        "git_dirty": env!("MIMA_GIT_DIRTY"),
        "profile": env!("MIMA_PROFILE"),
        "target": env!("MIMA_TARGET"),
        "rustc": env!("MIMA_RUSTC"),
    })
}

/// SHA-256 of the running executable, computed once per process.
pub fn binary_sha256() -> Option<String> {
    static HASH: OnceLock<Option<String>> = OnceLock::new();
    HASH.get_or_init(|| {
        let exe = std::env::current_exe().ok()?;
        let bytes = std::fs::read(exe).ok()?;
        Some(hex(&sha256(&[&bytes])))
    })
    .clone()
}

/// The repository the working directory belongs to: the nearest ancestor
/// (or itself) containing `.git`.
pub fn workspace_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .find(|d| d.join(".git").exists())
        .map(Path::to_path_buf)
}

/// A workspace's state as `git status --porcelain=v2` reports it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GitState {
    pub head: String,
    pub branch: String,
    /// Path (relative to the root) to its two-letter status (`?` for untracked).
    pub entries: BTreeMap<String, String>,
    /// SHA-256 of the status entries (not the branch headers).
    pub status_sha256: String,
}

impl GitState {
    pub fn summary(&self) -> Value {
        json!({ "head": self.head, "branch": self.branch,
                "status_sha256": self.status_sha256, "dirty": self.entries.len() })
    }
}

/// Runs `git status` in `root` without letting the repository's own
/// configuration start programs where it can be prevented: no fsmonitor, no
/// untracked cache, no optional locks, no system config, submodules ignored.
pub async fn git_state(runner: &CommandRunner, root: &Path) -> Result<GitState, String> {
    let argv: Vec<String> = [
        "git",
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.untrackedCache=false",
        "--no-optional-locks",
        "-C",
        &root.display().to_string(),
        "status",
        "--porcelain=v2",
        "--branch",
        "-z",
        "--untracked-files=all",
        "--ignore-submodules=all",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let env = [
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_OPTIONAL_LOCKS", "0"),
        ("LC_ALL", "C"),
    ];
    let done = runner.run(&argv, &env, GIT_TIMEOUT).await?;
    if done.code != Some(0) {
        return Err(format!(
            "git status failed (exit {:?}): {}",
            done.code,
            String::from_utf8_lossy(&done.stderr).trim()
        ));
    }
    Ok(parse_status(&done.stdout))
}

/// Parses `git status --porcelain=v2 --branch -z` output.
pub fn parse_status(out: &[u8]) -> GitState {
    let mut s = GitState::default();
    let mut body = Vec::new();
    let mut fields = out.split(|&b| b == 0).peekable();
    while let Some(raw) = fields.next() {
        if raw.is_empty() {
            continue;
        }
        let line = String::from_utf8_lossy(raw);
        if let Some(h) = line.strip_prefix("# branch.oid ") {
            s.head = h.to_string();
            continue;
        }
        if let Some(b) = line.strip_prefix("# branch.head ") {
            s.branch = b.to_string();
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        body.extend_from_slice(raw);
        body.push(0);
        // The path is the rest of the line after a fixed number of fields,
        // which depends on the entry type; paths may contain spaces.
        let split = |n: usize| -> Option<(String, String)> {
            let parts: Vec<&str> = line.splitn(n, ' ').collect();
            (parts.len() == n).then(|| (parts[1].to_string(), parts[n - 1].to_string()))
        };
        let entry = match line.as_bytes()[0] {
            // Ordinary change: 1 XY sub mH mI mW hH hI path
            b'1' => split(9),
            // Rename or copy: 2 XY sub mH mI mW hH hI Xscore path, then origPath
            b'2' => {
                if let Some(orig) = fields.next() {
                    body.extend_from_slice(orig);
                    body.push(0);
                }
                split(10)
            }
            // Unmerged: u XY sub m1 m2 m3 mW h1 h2 h3 path
            b'u' => split(11),
            b'?' => Some(("??".to_string(), line[2..].to_string())),
            _ => None,
        };
        if let Some((status, path)) = entry {
            s.entries.insert(path, status);
        }
    }
    s.status_sha256 = hex(&sha256(&[&body]));
    s
}

/// What a path held at one moment: its status and content hash.
#[derive(Debug, Clone, PartialEq)]
pub struct Mark {
    pub status: String,
    /// `None` when the path is gone; for a symlink, the hash of its target text.
    pub sha256: Option<String>,
    pub size: Option<u64>,
}

/// Hashes every path git reports as changed, so a later snapshot can tell
/// which ones a command touched (a modified file can be modified again).
pub fn snapshot(root: &Path, state: &GitState) -> BTreeMap<String, Mark> {
    state
        .entries
        .iter()
        .take(MAX_CHANGED_PATHS * 2)
        .map(|(path, status)| {
            let (sha256, size) = content_hash(&root.join(path));
            (
                path.clone(),
                Mark {
                    status: status.clone(),
                    sha256,
                    size,
                },
            )
        })
        .collect()
}

/// Content hash and size of a path, without following symlinks.
pub fn content_hash(path: &Path) -> (Option<String>, Option<u64>) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return (None, None);
    };
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(path).unwrap_or_default();
        let text = target.to_string_lossy();
        return (Some(hex(&sha256(&[text.as_bytes()]))), None);
    }
    if !meta.is_file() {
        return (None, Some(meta.len()));
    }
    if meta.len() > MAX_HASHED_BYTES {
        return (None, Some(meta.len()));
    }
    match std::fs::read(path) {
        Ok(bytes) => (Some(hex(&sha256(&[&bytes]))), Some(meta.len())),
        Err(_) => (None, Some(meta.len())),
    }
}

/// Paths whose status or content differs between two snapshots, for the
/// `workspace_changes` field. A path missing from `before` was unchanged
/// from the committed version then; missing from `after`, it is again.
pub fn changes(before: &BTreeMap<String, Mark>, after: &BTreeMap<String, Mark>) -> Value {
    let mut out = Vec::new();
    let paths: std::collections::BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    for path in paths {
        let (b, a) = (before.get(path), after.get(path));
        if b == a {
            continue;
        }
        out.push(json!({
            "path": path,
            "status": a.map_or("committed", |m| m.status.as_str()),
            "before_sha256": b.and_then(|m| m.sha256.clone()),
            "after_sha256": a.and_then(|m| m.sha256.clone()),
        }));
    }
    let truncated = out.len() > MAX_CHANGED_PATHS;
    out.truncate(MAX_CHANGED_PATHS);
    if truncated {
        json!({ "paths": out, "truncated": true })
    } else {
        json!({ "paths": out })
    }
}

/// The workspace's root and a snapshot of its changed paths, for comparing
/// before and after a shell command. `None` outside a git repository.
pub async fn marks(runner: &CommandRunner) -> Option<(PathBuf, BTreeMap<String, Mark>)> {
    let root = workspace_root(&std::env::current_dir().ok()?)?;
    let state = git_state(runner, &root).await.ok()?;
    let snap = snapshot(&root, &state);
    Some((root, snap))
}

/// The workspace's git summary (head, branch, status hash, dirty count), or
/// null outside a repository or when git fails.
pub async fn git_summary(runner: &CommandRunner) -> Value {
    let Some(root) = std::env::current_dir()
        .ok()
        .and_then(|c| workspace_root(&c))
    else {
        return Value::Null;
    };
    match git_state(runner, &root).await {
        Ok(s) => s.summary(),
        Err(_) => Value::Null,
    }
}

/// Who and where: user, uid, host, and whether a terminal is attached.
pub fn operator(record_host: bool) -> Value {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .ok();
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    let mut v = json!({ "user": user, "uid": uid, "tty": std::io::stdin().is_terminal() });
    if record_host {
        v["host"] = json!(crate::audit::hostname());
    }
    v
}

/// Kernel and platform.
fn os_info() -> Value {
    // SAFETY: utsname is plain data; uname fills it and returns 0 on success.
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    let release = if unsafe { libc::uname(&mut u) } == 0 {
        // SAFETY: uname NUL-terminates the field.
        let c = unsafe { std::ffi::CStr::from_ptr(u.release.as_ptr()) };
        Some(c.to_string_lossy().into_owned())
    } else {
        None
    };
    json!({ "platform": std::env::consts::OS, "arch": std::env::consts::ARCH, "kernel": release })
}

/// The `MIMA_*` variables set in the environment (names only: values may be
/// secrets, and the effective configuration shows what they changed).
fn env_overrides() -> Vec<String> {
    let mut names: Vec<String> = std::env::vars_os()
        .filter_map(|(k, _)| k.into_string().ok())
        .filter(|k| k.starts_with("MIMA_"))
        .collect();
    names.sort();
    names
}

/// The audit fields of `session_start` (see the design's header table).
pub fn header(ctx: &AgentContext, runner: &CommandRunner, workspace: Option<Value>) -> Value {
    let c = &ctx.config;
    let tools = crate::schema::to_openai_tools(&ctx.tool_specs);
    let names: Vec<&str> = ctx.tool_specs.iter().map(|t| t.name.as_str()).collect();
    let prompt = ctx.system_prompt_text();
    let support = crate::sandbox::probe();
    json!({
        "build": build_info(),
        "binary_sha256": binary_sha256(),
        "config": {
            "source": c.source.path.as_ref().map(|p| crate::audit::absolute(p)),
            "sha256": c.source.sha256,
            "workspace": c.source.workspace,
            "ignored": c.source.ignored,
            "env_overrides": env_overrides(),
            "effective": serde_json::to_value(c).unwrap_or(Value::Null),
        },
        "system_prompt": { "sha256": hex(&sha256(&[prompt.as_bytes()])), "text": prompt },
        "tools": { "names": names, "sha256": hex(&sha256(&[tools.to_string().as_bytes()])) },
        "sampling": {
            "temperature": c.agent.temperature, "top_p": c.agent.top_p, "top_k": c.agent.top_k,
            "min_p": c.agent.min_p, "max_tokens": c.agent.max_tokens,
            "extra_body": c.agent.extra_body, "stream": c.agent.stream,
        },
        "server": { "model": ctx.server_model, "version": ctx.server_version },
        "sandbox": {
            "mode": c.security.sandbox, "active": runner.sandboxed(), "summary": runner.summary(),
            "landlock_abi": support.landlock_abi, "seccomp": support.seccomp,
            "seatbelt": support.seatbelt, "network": c.security.sandbox_network,
        },
        "operator": operator(c.audit.record_host),
        "workspace": workspace,
        "os": os_info(),
    })
}

/// The workspace's root and git state, or `git: null` outside a repository
/// or when git fails (the error is kept).
pub async fn workspace(runner: &CommandRunner) -> (Value, Option<(PathBuf, GitState)>) {
    let cwd = std::env::current_dir().unwrap_or_default();
    let Some(root) = workspace_root(&cwd) else {
        return (
            json!({ "root": cwd.display().to_string(), "git": null }),
            None,
        );
    };
    match git_state(runner, &root).await {
        Ok(state) => (
            json!({ "root": root.display().to_string(), "git": state.summary() }),
            Some((root, state)),
        ),
        Err(e) => (
            json!({ "root": root.display().to_string(), "git": null, "git_error": e }),
            None,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain_v2() {
        let out = b"# branch.oid 0123abcd\0# branch.head main\0\
1 .M N... 100644 100644 100644 aaaa bbbb src/a.rs\0\
2 R. N... 100644 100644 100644 cccc dddd R100 new name.rs\0old.rs\0\
? notes/x y.txt\0";
        let s = parse_status(out);
        assert_eq!(s.head, "0123abcd");
        assert_eq!(s.branch, "main");
        assert_eq!(s.entries["src/a.rs"], ".M");
        assert_eq!(s.entries["new name.rs"], "R.");
        assert_eq!(s.entries["notes/x y.txt"], "??");
        assert_eq!(s.entries.len(), 3);
        // The headers do not change the status hash.
        let rest = &out[out.iter().position(|&b| b == 0).unwrap() + 1..];
        let other = parse_status(&[b"# branch.oid ffff\0" as &[u8], rest].concat());
        assert_eq!(other.head, "ffff");
        assert_eq!(other.status_sha256, s.status_sha256);
    }

    #[test]
    fn changes_compare_status_and_content() {
        let m = |status: &str, h: &str| Mark {
            status: status.into(),
            sha256: Some(h.into()),
            size: Some(1),
        };
        let before = BTreeMap::from([
            ("a".to_string(), m(".M", "1")),
            ("b".to_string(), m(".M", "2")),
            ("gone".to_string(), m(".M", "3")),
        ]);
        let after = BTreeMap::from([
            ("a".to_string(), m(".M", "1")),
            ("b".to_string(), m(".M", "9")),
            ("new".to_string(), m("??", "4")),
        ]);
        let c = changes(&before, &after);
        let paths: Vec<&str> = c["paths"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["path"].as_str().unwrap())
            .collect();
        assert_eq!(paths, ["b", "gone", "new"]);
        assert_eq!(c["paths"][1]["status"], "committed");
        assert!(c["paths"][2]["before_sha256"].is_null());
    }

    #[test]
    fn symlinks_are_not_followed() {
        let d = std::env::temp_dir().join(format!("mima-evidence-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("secret"), "s3cret").unwrap();
        std::os::unix::fs::symlink(d.join("secret"), d.join("link")).unwrap();
        let (link, _) = content_hash(&d.join("link"));
        let (file, _) = content_hash(&d.join("secret"));
        assert_ne!(link, file);
        assert_eq!(
            link,
            Some(hex(&sha256(&[d
                .join("secret")
                .display()
                .to_string()
                .as_bytes()])))
        );
    }
}
