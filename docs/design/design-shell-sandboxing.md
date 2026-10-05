# Design: Sandboxing Shell Commands

Status: proposal; Tier 0 implemented (`src/tools/shell.rs`)
Scope: `src/tools/mod.rs` (`execute_bash`), `src/approval.rs`, `src/config.rs`,
`src/main.rs`; shares code with `src/bin/mima-eval/exec.rs`
Related: `docs/design/design-eval-harness.md` (the harness already sandboxes
commands with bubblewrap)

## Problem

`execute_bash` runs `sh -c <command>` as the user, with the user's full
environment and file access. mima's own file tools are confined to
`allowed_paths`, but a shell command is not: it can read `~/.ssh`, write
anywhere the user can, open network connections, and keep running after a
timeout if it detached from `sh`. The only control today is human approval,
optionally skipped for commands that match `security.auto_approve_bash`.

That is weak for a coding agent, because the commands are chosen by a model
that reads untrusted text (repository files, tool output, web content). A
prompt injection in any of that text can steer the model into running a
command that the user approves without reading closely, or that matches an
allowlisted prefix. Concretely, the current code also:

- passes the whole environment to every command, including `MIMA_API_KEY`;
- leaves the command attached to the user's terminal, so on hosts that still
  allow the `TIOCSTI` ioctl (the Jetson AGX Thor measured below has
  `dev.tty.legacy_tiocsti = 1`) a command can push keystrokes into the user's
  shell;
- kills only the `sh` process on timeout, so background children survive.

## Threat model

In scope: a model steered by prompt injection or simply mistaken, issuing
commands that

- **exfiltrate** secrets or code (network connections, DNS lookups, writes to
  places another process will send);
- **steal credentials** (`~/.ssh`, `~/.aws`, `~/.config/gh`, `~/.netrc`,
  git credential helpers, environment variables, ssh-agent and D-Bus sockets);
- **destroy** data outside the workspace (`rm -rf ~`);
- **persist** (shell rc files, git hooks, `systemd --user` units, cron);
- **escape** through tools that run other programs (git `core.pager` and hooks,
  `make`, package-manager scripts).

Simon Willison's "lethal trifecta" (private data, untrusted content, and a way
to communicate out) describes why the combination matters; removing the
outbound channel and the access to secrets breaks it for shell commands.

Out of scope, and not solved by sandboxing commands:

- the model leaking what it has already read through its next request to the
  model endpoint (mima must reach that endpoint);
- damage inside the workspace, which the agent is meant to modify (version
  control and approvals are the controls there);
- a malicious model server, or a compromised mima binary.

## Goals

- A real boundary for every shell command on the target hardware, without
  root, extra packages, or a container runtime.
- Network off for commands by default, while mima itself keeps its connection
  to the model server.
- Secrets in the home directory unreadable by default, without maintaining a
  deny list.
- Fewer approval prompts: a command that runs inside the boundary can be
  approved automatically; anything that leaves it always asks.
- Fail closed: if the boundary cannot be set up, say so and require approval,
  never silently run unconfined.

Non-goals: protecting the workspace from the agent; Windows and macOS (Linux
first, as the rest of mima).

## Constraints measured on the target hosts

| | Dev host (Debian 13, kernel 6.12, x86_64) | Jetson AGX Thor (L4T R39.2, Ubuntu-based, kernel 6.8, aarch64) |
|---|---|---|
| Landlock | ABI 6 | ABI 4 (filesystem, plus TCP bind/connect by port; no UDP) |
| seccomp filters | yes | yes |
| Unprivileged user namespaces | allowed | blocked (`kernel.apparmor_restrict_unprivileged_userns = 1`) |
| bubblewrap, unprivileged | works | fails (`loopback: Failed RTM_NEWADDR: Operation not permitted`) |
| `dev.tty.legacy_tiocsti` | n/a | 1 (allowed) |
| `systemd-run --user --scope` | works in a login session | works in a login session; needs linger when headless |

The consequence: namespace-based sandboxes (bubblewrap, `unshare`, nsjail,
rootless containers) do not work for a normal user on the target device, the
same restriction Ubuntu 24.04 applies. Landlock and seccomp do, on both hosts.

## Prior art

| Agent | Linux mechanism | Network | Notes |
|---|---|---|---|
| Claude Code | bubblewrap + seccomp; Seatbelt on macOS | network namespace with a host proxy and domain allowlist | reads most of the machine by default; documents an AppArmor profile for bwrap on Ubuntu; Anthropic reports 84% fewer permission prompts |
| OpenAI Codex CLI | bubblewrap + seccomp (earlier Landlock + seccomp, kept as a fallback) | off by default in sandboxed modes | users on Ubuntu 24.04 hit the same bwrap failure and fall back to Landlock (codex issue #17337) |
| Gemini CLI | Docker/Podman; Seatbelt on macOS | container networking | requires a container runtime |
| Cursor | Landlock/namespaces, kernel 6.2+ | restricted | falls back to approvals when unavailable |
| OpenHands | Docker runtime | container networking | |
| Aider | none | | |

Incidents show why allowlists of commands are not enough on their own:
DNS exfiltration through allowlisted `ping`/`dig` in Claude Code
(CVE-2025-55284), a prefix-allowlist bypass in Gemini CLI (Tracebit), a NUL
byte in a hostname bypassing Claude Code's proxy, and Codex using a
model-supplied working directory as its writable root (CVE-2025-59532).
mima's `auto_approve_bash` is the same kind of control as those allowlists.

## Design

Layers, each independent of the next. Tier 0 and Tier 1 are on by default on
every Linux host; Tiers 2 and 3 are optional and used only where available.

### Tier 0: process hygiene (always on)

- **Environment allowlist.** Commands get `PATH HOME USER LOGNAME SHELL TERM
  TZ LANG LANGUAGE LC_* COLORTERM NO_COLOR`, non-secret toolchain locations
  (`RUSTUP_HOME CARGO_HOME GOPATH GOROOT JAVA_HOME VIRTUAL_ENV CONDA_PREFIX
  CUDA_HOME LD_LIBRARY_PATH PKG_CONFIG_PATH`) and a private `TMPDIR`, plus
  names the user lists in `security.env_passthrough`. Nothing else: no `MIMA_*` (the API key), no
  `*_TOKEN` or `*_KEY`, no `AWS_*`, no `SSH_AUTH_SOCK`, no
  `DBUS_SESSION_BUS_ADDRESS`. A passthrough entry that names one of these
  sensitive patterns is honored but logged at warn level.
- **New session and process group.** The child calls `setsid()` before exec:
  it has no controlling terminal (closing the `TIOCSTI` hole), and it leads its
  own process group. stdin is `/dev/null`.
- **Whole-group kill.** On timeout, mima sends SIGTERM to the process group,
  waits a short grace period, then SIGKILL. When a command finishes normally
  the group is also killed, so background processes it left do not outlive it.
  (A process that starts its own new session escapes the group; Tier 3's
  cgroup scope closes that gap.)
- **Private temp directory.** One directory per session, mode 0700, under
  `$XDG_RUNTIME_DIR` when set, else the system temp directory; exported as
  `TMPDIR` and removed when mima exits.

### Tier 1: Landlock + seccomp + rlimits (default)

mima re-executes itself as a small helper for each command,
`mima __sandbox-exec --policy <json> -- sh -c <command>`, as Codex does with
`codex-linux-sandbox`. The helper is single-threaded, which avoids the
async-signal-safety problems of configuring a child after `fork` inside a
multi-threaded tokio process. In order it:

1. calls `setsid()`;
2. sets rlimits: core 0, a large file-size cap, an open-files cap; no
   address-space limit by default (CUDA and JIT runtimes reserve large ranges);
3. sets `PR_SET_NO_NEW_PRIVS`;
4. applies a Landlock ruleset (best effort up to the kernel's ABI);
5. applies a seccomp filter (precompiled for x86_64 and aarch64);
6. execs the command.

It is also exposed as `mima sandbox -- <command>` for debugging, and
`mima sandbox --status` reports what the host supports.

**Filesystem (Landlock).** Landlock can only grant access; it cannot deny a
subpath of a granted directory. So the policy grants narrowly:

- read and execute: system directories (`/usr /bin /sbin /lib /lib64 /etc
  /opt /var /proc /sys`), selected devices, the workspace, and configured
  toolchain directories (by default `~/.cargo`, `~/.rustup`, `~/.local/bin`,
  `~/.gitconfig`);
- read and write: the workspace roots (the session's start directory and
  `allowed_paths`), the private temp directory, `/dev/null`, and
  `security.writable_paths`;
- not the home directory as a whole, so `~/.ssh`, `~/.aws`, `~/.netrc`,
  `~/.config/gh` and mima's own config are unreadable without a deny list.

On the Thor, GPU device nodes needed by CUDA programs are added to the device
list.

**Network and system calls (seccomp, deny with EPERM).**

- Network off: `socket` for any family other than `AF_UNIX`, and `connect`,
  `bind`, `listen`, `accept`, `accept4`, `sendto`, `sendmmsg`, `recvmmsg`.
  Denying `connect` and `sendto` outright also blocks DNS over UDP (which
  Landlock ABI 4 cannot restrict) and connections to local daemons over Unix
  sockets (resolver, D-Bus, `systemd --user`, ssh-agent, docker).
  `socketpair(AF_UNIX)` stays allowed for pipes between processes.
- Always: `io_uring_*` (a path around seccomp checks), `ioctl` with `TIOCSTI`
  or `TIOCLINUX`, the mount family, `keyctl`/`add_key`/`request_key`, `bpf`,
  module loading, and optionally `unshare`/`clone` with `CLONE_NEWUSER`.
- `ptrace` stays allowed by default: Landlock already stops a sandboxed
  process from tracing anything outside its domain, and `gdb`/`strace` on the
  program being built matter for this project's systems use cases. A
  `security.deny_ptrace` switch turns it off.

Dependencies: the `landlock` and `seccompiler` crates (pure Rust, no C
libraries), keeping the single static binary.

### Tier 2: bubblewrap (optional, where namespaces are allowed)

Where unprivileged user namespaces work (the dev host, or an Ubuntu/L4T host
whose administrator installed the AppArmor profile Claude Code documents for
`bwrap`), wrap the Tier 1 helper in bubblewrap, reusing the eval harness's
`sandbox_prefix`. It adds what Landlock cannot express:

- read-only binds over sensitive paths inside the writable workspace:
  `.git/hooks`, `.git/config`, `./agent.toml`, rc files;
- a private network namespace with an in-process HTTP CONNECT proxy, the only
  way to enforce a per-domain allowlist (Landlock and seccomp can only allow
  ports, not hosts). The proxy resolves each name once, refuses loopback,
  link-local and metadata addresses, rejects malformed hostnames, and starts
  with an empty allowlist.

bubblewrap is detected, not required, and not reimplemented: its namespace and
mount setup is security-critical and maintained elsewhere.

### Tier 3: resource limits (optional)

When `systemd-run --user` works, run each command in a transient scope with
`MemoryMax`, `TasksMax` and `CPUQuota`. This gives fork-bomb and memory limits
and a reliable kill of every process the command started, including ones that
left its process group. Without it, Tier 1's rlimits are the limit.

## Approvals and configuration

Illustrative configuration (names may change in implementation):

```toml
[security]
sandbox = "auto"            # "required" | "auto" | "off"
network = "off"             # "off" | "proxy" (Tier 2 only) | "host"
allowed_domains = []        # proxy mode only
writable_paths = []         # beyond the workspace and private temp dir
read_paths = ["~/.cargo", "~/.rustup", "~/.gitconfig"]
env_passthrough = []        # extra environment variable names for commands
auto_approve_sandboxed = true
```

Rules:

- A command that runs fully sandboxed with the network off is approved
  automatically when `auto_approve_sandboxed` is set. `auto_approve_bash`
  prefixes apply only to sandboxed runs.
- The model may ask for more (an optional tool argument naming `network` or
  `unsandboxed` with a reason). Such a run always prompts, shows exactly what
  is being widened, and is never covered by an allowlist. `--approve-all` does
  not approve escalations.
- Settings that widen the sandbox are honored only from trusted sources: the
  user config (`~/.config/minister_mandati/agent.toml`), environment, or
  command line. A workspace `./agent.toml` sits inside the writable sandbox,
  so it may only narrow, or its widening requires confirmation once per
  session.
- The writable root is the session's start directory (plus `allowed_paths`),
  never a working directory chosen in a tool call.
- When a sandboxed command fails, the tool result gets one line naming the
  active restrictions (for example `[sandbox: landlock abi=4, network off,
  writable: <workspace>, <tmp>]`), and, when the error looks like a denied
  path or connection, how to ask for more. This reduces blind retries.
- Tier 1 cannot protect `.git/hooks` inside a writable workspace. mima hashes
  `.git/config`, the hook directory and its own config files before and after
  each command, and asks for approval to keep any change.

## Degrading on older or restricted kernels

mima probes at startup (Landlock ABI, seccomp, usable namespaces,
`legacy_tiocsti`, `systemd-run`) and logs the result.

- No Landlock or no seccomp: with `sandbox = "required"`, `execute_bash`
  returns an error; with `auto`, mima warns once and every command needs
  approval (no automatic approval, no allowlist).
- Landlock ABI 1-3: filesystem rules with reduced rights; seccomp still turns
  the network off.
- Landlock ABI 4-5 (the Thor): full Tier 1, with known gaps (signals and
  abstract Unix sockets are not scoped; the connect deny covers the latter).
- A ruleset the kernel reports as not enforced counts as unavailable.
- `network = "proxy"` without a usable network namespace is refused with a
  clear message, never turned into host networking.
- mima running inside the eval harness's bubblewrap: Landlock layers stack,
  so this works.

## Testing

Integration tests run on both host types and skip, with a reason, when a tier
is unavailable:

1. Filesystem: writes outside the workspace and temp dir fail; a planted
   `~/.ssh/id_ed25519` (and a workspace symlink to it) is unreadable;
   workspace writes and truncation work.
2. Network: TCP connect, UDP `sendto` to port 53, name resolution, and Unix
   connects to the D-Bus and ssh-agent sockets all fail.
3. Terminal and processes: `TIOCSTI` fails and there is no controlling
   terminal; tracing a process outside the sandbox fails; `gdb`/`strace` on a
   child inside it works.
4. Environment: no `MIMA_API_KEY`, no `*_TOKEN`; `TMPDIR` is private.
5. Timeouts: `sleep 1000 &` is gone after the command returns or times out;
   limits hold.
6. Real work: `cargo build`/`cargo test`, `git status`/`commit`, `make`,
   `pytest`, and a CUDA sample on the Thor.
7. Policy: a tool-call working directory outside the workspace does not widen
   the writable roots; widening from `./agent.toml` is ignored or prompts; a
   change to `.git/hooks` is detected.
8. seccomp filters per architecture (golden tests; x32 syscalls denied on
   x86_64).

## Implementation phases

| Phase | Content | Effort |
|---|---|---|
| S0 | Tier 0: environment allowlist, new session and process-group kill, private temp directory; tests | 0.5-1 day |
| S1 | Tier 1 helper: Landlock, seccomp, rlimits, `[security]` sandbox settings, `mima sandbox --status` | 2-3 days |
| S2 | Approvals: automatic approval of sandboxed runs, escalation argument, trusted-source widening, failure notes, integrity check | 1-2 days |
| S3 | Test matrix on both host types; results documented | 1-2 days |
| S4 | Tier 2: bubblewrap shared with mima-eval, protected-path binds, network namespace and proxy | 3-5 days |
| S5 | Tier 3: `systemd-run --user --scope` limits | 0.5-1 day |

S0 and S1 give a real boundary on the target device. S4 matters only where
namespaces are allowed, and is the only route to a domain allowlist.

## Alternatives considered

- **bubblewrap as the default.** Strongest and already used by the eval
  harness, but it fails unprivileged on Ubuntu 24.04 and L4T unless an
  administrator installs an AppArmor profile; it would leave the target device
  without a sandbox. Kept as Tier 2.
- **Containers (Docker, Podman).** A heavy runtime dependency, contrary to the
  single-binary goal, and rootless Podman needs user namespaces too.
- **A deny list of secret paths under a readable home.** Easy to get wrong
  and never complete; granting only named directories is safer.
- **Command allowlists only.** The incidents above show prefix and allowlist
  bypasses; kept only as a convenience inside the sandbox.
- **Configuring the child with `pre_exec` instead of a re-exec helper.**
  Possible, but the setup code would run between `fork` and `exec` in a
  multi-threaded process, where only async-signal-safe operations are allowed;
  Landlock and seccomp setup is easier to keep correct in a separate
  single-threaded helper.
- **An AppArmor profile for mima itself.** Would need its exec transitions
  reviewed and root to install; the bwrap profile is narrower.

## Open questions

- Default read paths for toolchains beyond Rust (pyenv, nvm, conda) and how to
  discover them without granting the whole home directory.
- Whether the integrity check should restore changed hook files or only
  report them.
- How to present escalation requests in non-interactive one-shot mode.

## References

- Claude Code sandboxing: <https://code.claude.com/docs/en/sandboxing>;
  <https://www.anthropic.com/engineering/claude-code-sandboxing>
- Anthropic sandbox-runtime: <https://github.com/anthropic-experimental/sandbox-runtime>
- Codex Linux sandbox: <https://github.com/openai/codex/tree/main/codex-rs/linux-sandbox>;
  bwrap failure on Ubuntu 24.04: <https://github.com/openai/codex/issues/17337>
- Gemini CLI sandbox: <https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/sandbox.md>
- Cursor run modes: <https://cursor.com/docs/agent/security/run-modes>
- Landlock: <https://docs.kernel.org/userspace-api/landlock.html>;
  <https://man7.org/linux/man-pages/man7/landlock.7.html>; crate
  <https://docs.rs/landlock/>; seccompiler <https://docs.rs/seccompiler/>
- Ubuntu unprivileged user namespace restriction:
  <https://ubuntu.com/blog/ubuntu-23-10-restricted-unprivileged-user-namespaces>
- The lethal trifecta: <https://simonwillison.net/2025/Jun/16/the-lethal-trifecta/>
- CVE-2025-55284 (DNS exfiltration):
  <https://embracethered.com/blog/posts/2025/claude-code-exfiltration-via-dns-requests/>;
  Gemini CLI allowlist bypass: <https://tracebit.com/blog/code-exec-deception-gemini-ai-cli-hijack>;
  CVE-2025-59532: <https://osv.dev/vulnerability/CVE-2025-59532>
- Liu et al., "Your AI, My Shell" (arXiv:2509.22040); Greshake et al.,
  indirect prompt injection (arXiv:2302.12173)
