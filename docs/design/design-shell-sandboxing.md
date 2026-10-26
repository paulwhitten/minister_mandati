# Design: Sandboxing Shell Commands

Status: Tier 0 implemented (`src/tools/shell.rs`); Tier 1 implemented
(`src/sandbox/linux.rs`, tests in `itests/sandbox.rs`), tested on the dev host
(x86_64, Landlock ABI 6) and on the AGX and IGX Thor (Landlock ABI 4; see
[S3 results](#s3-results-on-the-thor)); CUDA programs need the opt-in
`[security].sandbox_gpu`; approval
integration (S2)
partly done (failure notes); Tiers 2-3 not started. macOS (Seatbelt):
M0-M2 implemented (`src/sandbox/macos.rs`, macOS cases in
`itests/sandbox.rs`) and tested on macOS 26.7.1; M3 network-on profile done;
`mima sandbox --status`, M4 (CI job added, not yet run) and M5 open. See
[macOS: Seatbelt](#macos-seatbelt).

Implementation notes for Tier 1 (differences from the proposal below):

- Settings are in `[security]` (all shown with their defaults in
  `agent.toml.example`): `sandbox` ("auto" | "required" | "off"),
  `sandbox_network`, `sandbox_gpu` (Linux, off by default; see
  [GPU access](#tier-1-landlock--seccomp--rlimits-default)),
  `sandbox_read_paths` (default `~/.cargo`, `~/.rustup`, `~/.local/bin`,
  `~/.local/lib`, `~/.gitconfig`, `~/.config/git`, `~/.local/go`,
  `~/go/pkg/mod`), `sandbox_writable_paths`. A workspace `./agent.toml`
  cannot widen any of them. Git needs `~/.config/git` readable: it treats an
  unreadable `ignore` file there as fatal.
- `/run` is readable (resolver and runtime files); `/dev` is writable (DAC
  still applies).
- With the network off, `sendto` is denied only when it names a destination
  address (plain `send` has none), so Unix socket pairs keep working.
  `sendmsg` and `sendmmsg` stay allowed, because the address is inside a
  struct that seccomp cannot read. So a command can still send datagrams to
  a local daemon's Unix datagram socket with `sendmsg` (for example
  `/dev/log` or journald, which may forward logs off the device). This is a
  known gap; closing it needs Landlock rules for Unix sockets (not in ABI
  6) or a network namespace (Tier 2).
- Landlock scopes: signals always, abstract Unix sockets when the network is
  off (ABI 6 and later).
- On x86_64 a second filter denies all x32-ABI system calls.
- In the eval harness the helper runs inside the trial's bubblewrap; the
  harness bind-mounts the mima binary so it is visible there.
- Approvals (S2, partly): `auto_approve_bash` applies only while commands are
  sandboxed (with a startup warning otherwise); `auto_approve_sandboxed`
  approves all sandboxed commands but defaults to **off**, not on as proposed
  below, because the sandbox does not protect the workspace itself. Not done
  yet: the escalation argument, trusted-source-only widening (a workspace
  `./agent.toml` can still set sandbox options), the `.git/hooks` integrity
  check, and `mima sandbox --status`.
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

Non-goals: protecting the workspace from the agent; Windows. macOS is covered
separately in [macOS: Seatbelt](#macos-seatbelt), with the same goals.

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

**GPU access (`[security].sandbox_gpu`, off by default).** CUDA programs do
not run in the default sandbox (see [S3 results](#s3-results-on-the-thor)).
Setting `sandbox_gpu = true` in the user configuration grants what the CUDA
driver needs beyond the already writable `/dev`:

- `/proc` becomes writable, because the driver names its threads through
  `/proc/self/task/<tid>/comm`. It is added to the policy's writable list,
  so the startup log and sandbox summary show it;
- with the network off, Unix sockets may `bind`, `listen` and `accept`,
  because the driver binds an abstract Unix socket. `socket` stays limited
  to `AF_UNIX`, and `connect` and `sendto` with an address stay denied, so
  stream connections to local daemons are still refused.

It is not granted when mima runs as root (a writable `/proc/sys` would let a
command set `kernel.core_pattern`, which runs a program outside the sandbox)
or when no NVIDIA device nodes exist (`/dev/nvidiactl`). A workspace
`./agent.toml` cannot turn it on.

The write cannot be narrowed to `/proc/self`: Landlock resolves that path
once, for the helper, not for its child processes. What the setting costs:

- Files of the user's other processes that the kernel does not guard with a
  ptrace check become writable from inside: `oom_score_adj` (a command can
  make mima or a model server the first target of the OOM killer),
  `coredump_filter` and `clear_refs`. `/proc/<pid>/mem` and similar files
  stay protected by Landlock's ptrace scoping.
- A command that creates a user namespace can write its `uid_map`, so user
  namespaces become usable inside the sandbox where the kernel allows them
  (not on the Thor, which restricts unprivileged user namespaces; allowed on
  the dev host). That exposes more kernel code.
- A command can run a Unix-socket server. With `setsid` it can outlive the
  command (the process-group kill does not reach it; it stays sandboxed),
  and it could take an abstract socket name that an outside client expects.
  Landlock ABI 6 does not prevent that either; Tier 3 (a systemd scope)
  would end such processes.

**Network and system calls (seccomp, deny with EPERM).**

- Network off: `socket` for any family other than `AF_UNIX`, and `connect`,
  `bind`, `listen`, `accept`, `accept4`, `sendto`, `sendmmsg`, `recvmmsg`.
  Denying `connect` and `sendto` outright also blocks DNS over UDP (which
  Landlock ABI 4 cannot restrict) and connections to local daemons over Unix
  sockets (resolver, D-Bus, `systemd --user`, ssh-agent, docker). As
  implemented (see the notes at the top), `sendto` is denied only with a
  destination address and `sendmsg`/`sendmmsg`/`recvmmsg` stay allowed.
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

## macOS: Seatbelt

Status: proposal. On macOS, `probe()` reports the sandbox as unavailable, so
with `sandbox = "auto"` every shell command runs unconfined (Tier 0 only) and
needs approval. With `sandbox = "required"` no shell command runs at all.

### What other agents do on macOS

The agents we checked all use Seatbelt through `/usr/bin/sandbox-exec`, with
a profile generated by the agent:

| Agent | macOS mechanism | Network | Notes |
|---|---|---|---|
| Codex CLI | `sandbox-exec` with a deny-by-default base policy derived from Chromium's renderer policy | off unless the policy allows it | Runs `/usr/bin/sandbox-exec` by absolute path so a binary planted on `PATH` cannot replace it. Under workspace-write it keeps `.git`, the resolved `gitdir:` target and `.codex` read-only. Also allows `user-preference-read` (cfprefs). |
| Claude Code (`sandbox-runtime`) | `sandbox-exec` with a profile generated for each command | only to a localhost port where its HTTP and SOCKS proxies enforce a domain allowlist | Reads allowed by default with a deny list. Writes allowed only where listed. Writes to `.git/hooks`, `.git/config`, shell rc files and IDE folders are always denied. Apple Events, pseudo-terminals and `trustd` are off by default. Reads violations from the system sandbox log. |
| Gemini CLI | `sandbox-exec` with six built-in profiles, or Docker/Podman | open or proxied, depending on the profile | The default profile (`permissive-open`) limits writes to the project and leaves the network open. |

No agent we checked uses Apple's private `sandbox_init*` API from its own
process. The `sandbox-exec(1)` man page has called the tool deprecated for
years, but macOS 26.7.1 still ships it (checked on the dev Mac), and all three
agents rely on it.

### Measured on the dev Mac

macOS 26.7.1, Apple silicon. Experiments used a scratch profile under `/tmp`:

- Rules are evaluated in order and the last match wins. A later
  `(deny file-read* (subpath HOME))` overrides an earlier
  `(allow file-read*)`, and a later allow for the workspace re-opens it.
  Unlike Landlock, Seatbelt can therefore deny a subpath inside an allowed
  directory: writes to `.git/hooks/*` and `.git/config` were refused while
  the rest of the workspace stayed writable.
- `~/.ssh` and `~/.zshrc` were unreadable, and writes outside the workspace
  and the temp directory failed with `Operation not permitted`.
- Without network rules, `curl` failed at name resolution (`Could not resolve
  host`). DNS on macOS goes through a Mach service, which deny-by-default also
  blocks. A direct UDP `bind` (as `host` does) also failed.
- `sandbox-exec` inside an already-sandboxed process fails with
  `sandbox_apply: Operation not permitted` (exit status 71). The sandbox does
  not nest.
- `/usr/bin/git` and `/usr/bin/cc` are `xcrun` shims. They failed until
  `TMPDIR` pointed to a writable directory (mima already sets a private one).
  Allowing the `com.apple.bsd.dirhelper` service made it worse, because
  clang then picked the per-user temp directory under `/var/folders`, which
  is not writable. git also needs `~/.gitconfig` to be readable; it is in the
  default `sandbox_read_paths`.
- With `(allow network*)`, name resolution worked, but HTTPS certificate
  verification failed until more Mach services were allowed. The sandbox
  violation log names each denied operation:
  `log show --last 1m --predicate 'sender == "Sandbox"'`. It reported
  `com.apple.TrustEvaluationAgent`, `com.apple.SystemConfiguration.configd`,
  cfprefs shared memory and `user-preference-read`, and the files
  `~/.CFUserTextEncoding` and `~/Library/Preferences/.GlobalPreferences*.plist`.

### Design

The filesystem and network semantics match Tier 1 on Linux. Seatbelt
enforces them, and the same `Policy` value (`read`, `write`, `network`)
drives both platforms.

- **Launch.** On macOS, `wrap()` returns
  `/usr/bin/sandbox-exec -p <profile> -D NAME=VALUE... -- sh -c <command>`,
  always by absolute path. Tier 0 (environment allowlist, `setsid`,
  process-group kill, private `TMPDIR`) is unchanged: `sandbox-exec` execs
  the shell, so the shell still leads the group. The `__sandbox-exec` helper
  is not used on macOS.
- **No path text in the profile.** Every path enters through a `-D`
  parameter and is referenced as `(param "WRITE_0")`, as Codex does. A
  workspace path that contains quotes or parentheses therefore cannot change
  the profile.
- **Base.** `(deny default)`, then only what a recorded denial justified.
  The rules were derived independently: a smoke set (`sh` basics, `uname`,
  `getconf`, `sysctl`, `git` commit, `cc`, `make`, `python3` including
  multiprocessing, `cargo build`) ran under a profile with nothing but
  process, file and signal rules, and each denial in the sandbox violation
  log was reviewed. Granted: `process-exec`, `process-fork`, and `signal`
  and `process-info*` within the same sandbox; 17 named `sysctl` values
  (five read by libSystem in every process, the rest by `uname`, `getconf`,
  clang, ld, Python and rustup) plus the `hw.optional.` and
  `sysctl.oidfmt.` prefixes; the `opendirectoryd.libinfo` lookup (user
  names); `ipc-posix-sem` (Python multiprocessing failed without it);
  `user-preference-read` and the global preference files (CoreFoundation
  tools such as `curl`). Seen but left denied, because the smoke set passed
  without them: `kern.bootargs`, `com.apple.logd`,
  `com.apple.system.notification_center`, `com.apple.analyticsd`,
  `SystemConfiguration.configd` (offline), `/dev/dtracehelper`, and two
  services that make things worse when allowed: `com.apple.bsd.dirhelper`
  and `opendirectoryd.membership` both let xcrun tools find the per-user
  temp directory, which is not writable, instead of using `TMPDIR`. Known
  gap: `/bin/ps` cannot run inside the sandbox. Further grants need a
  recorded denial.
- **Compared with the agents above.** Unlike `sandbox-runtime`'s default
  (reads allowed everywhere unless denied), the home directory, other
  volumes and per-user temp directories are unreadable without a deny
  list. Unlike Codex, which keeps all of `.git` read-only, the rest of
  `.git` stays writable so `git commit` works inside the sandbox; hooks and
  config stay protected, as in `sandbox-runtime`. Every base rule is backed
  by a recorded denial, and network on adds one Mach service.
- **Reads.** Allow `file-read*` everywhere, then deny the places that hold
  user data: `/Users`, `/Volumes` and `/private/var/folders` (per-user temp
  and cache directories of every app). Then re-allow the workspace roots,
  the private `TMPDIR`, `sandbox_read_paths`, the mima binary, and
  `file-read-metadata` on each ancestor of those, so that `cd` and
  `realpath` work. As on Linux, keys and credentials in the home directory
  are unreadable without a deny list. Because macOS spreads system files
  over `/System`, `/Library`, `/Applications/Xcode.app` and `/opt/homebrew`,
  we allow everything outside user data rather than list system directories
  (the Linux approach).
- **Writes.** Allow `file-write*` under the workspace roots, the private
  `TMPDIR` and `sandbox_writable_paths`, and data writes to `/dev/null`.
  Then deny `.git/hooks`, `.git/config` and the resolved `gitdir:` target's
  hooks and config in each workspace root. This closes, on macOS, the
  `.git/hooks` gap that Tier 1 has on Linux. We keep the rest of `.git`
  writable (unlike Codex) so that `git commit` works, which the
  `git-branch-commit` eval task needs.
- **Network off (default).** No network rules and no DNS service. Unix
  socket connects should be denied too, which keeps the ssh-agent and Docker
  sockets unreachable as on Linux Tier 1; this has not been tested yet, and
  M2 tests it.
- **Network on (`sandbox_network = true`).** `(allow network*)` plus one
  Mach service, `com.apple.TrustEvaluationAgent`. On the dev Mac, `curl`
  to an HTTPS site failed certificate verification without it and
  succeeded with it. Removing it alone broke the check; removing any of
  `trustd`, `trustd.agent`, `SecurityServer`, `configd` or the cfprefs
  services did not. Other TLS stacks (Go, Python's `ssl`) may need more
  services; add them only with a recorded violation, because trust
  services are a known exfiltration path (`sandbox-runtime` keeps them off
  by default).
- **Never allowed.** Apple Events and `lsopen` (`open` and `osascript`
  would start programs outside the sandbox), `/dev/dtracehelper`, and
  pseudo-terminals (commands have no terminal).
- **Probe.** At startup, run
  `/usr/bin/sandbox-exec -p '(version 1)(allow default)' /usr/bin/true`.
  If it fails (no binary, or mima already runs inside a sandbox), report
  the sandbox as unavailable. `auto` and `required` then behave as on a
  Linux kernel without Landlock.
- **Failure notes.** The existing denial detection (`Operation not
  permitted`, `Could not resolve`) already matches Seatbelt's errors. The
  note names the active restrictions, as on Linux.
- **Configuration.** No new settings. `sandbox`, `sandbox_network`,
  `sandbox_read_paths`, `sandbox_writable_paths` and
  `auto_approve_sandboxed` mean the same on both platforms, and so do the
  trusted-source rules in `restrict_to_untrusted`. `sandbox_gpu` is Linux
  only; on macOS it has no effect (mima logs that no NVIDIA device nodes
  exist).

### Code layout and keeping Linux unchanged

- One backend file per platform, each compiled only on its platform:
  `src/sandbox/linux.rs` (Landlock, seccomp, the helper's setup) and
  `src/sandbox/macos.rs` (Seatbelt). `src/sandbox.rs` holds what is shared
  (`Policy`, `Support`, the helper entry point) and dispatches to the
  backend. Each backend provides `SYSTEM_READ`, `SYSTEM_WRITE`, `probe()`,
  `describe()`, `wrap()` and `prepare()`. Other platforms are not
  supported and fail to build with a clear message.
- Moving the Linux code into `linux.rs` did not change its behavior.
- The profile generator is a pure function of `Policy` plus a few paths. It
  compiles under `cfg(any(target_os = "macos", test))`, so its unit tests
  also run on Linux CI.
- `Support` gains one field for Seatbelt; `describe()` is per backend.
- Every phase is merged only if the Linux unit tests and `itests/sandbox.rs`
  pass unchanged on the Linux dev host, and `cargo clippy -D warnings` is
  clean on both platforms.

### Eval harness on macOS

The harness requires `bwrap`, which macOS does not have. Seatbelt does not
nest, so the harness cannot also wrap `mima` in a Seatbelt profile while
`mima` sandboxes its own commands. The plan is for the harness to run `mima`
without a wrapper and rely on `mima`'s own Seatbelt sandbox for its
commands. Hidden checks would run under a harness-generated Seatbelt profile
that follows the bubblewrap rules: hide the home directory, the task's
solutions and checks, and other trials. This is M5. Until then, macOS
results are not comparable with Linux results.

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

### S3 results on the Thor

Run on 2026-10-25 from the working tree after a72d6f2. Binaries were built
natively on the IGX Thor (debug profile) and copied unchanged to the AGX
Thor, so no build ran on the AGX during an evaluation run there.

| | Dev host | IGX Thor | AGX Thor |
|---|---|---|---|
| System | Debian 13, kernel 6.12, x86_64 | L4T R38.5, kernel 6.8.0-1019-nvidia-tegra, aarch64 | L4T R39.2, kernel 6.8.12-1021-tegra, aarch64 |
| Landlock ABI | 6 | 4 | 4 |
| `kernel.yama.ptrace_scope` | | 1 | 1 |
| `dev.tty.legacy_tiocsti` | | 0 | 1 |
| Unit tests (226 mima, 61 mima-eval) | pass | pass | pass |
| `itests/sandbox.rs` (14, plus 1 ignored) | pass | pass | pass |
| Not available, so skipped | strace, gdb, pytest | none | cargo, strace |

Matrix items covered (numbers refer to the list above):

1. Filesystem: `workspace_is_writable_and_the_rest_is_not`, including a
   workspace symlink to a planted key.
2. Network: socket creation, TCP connect to a local listener, Unix connect to
   a listener standing in for D-Bus or ssh-agent, and connect to an abstract
   Unix socket, each refused with the network off and allowed with it on.
3. Terminal and processes: TIOCSTI on a fresh controlling pty is refused with
   EPERM. On the AGX the same call succeeds outside the sandbox
   (`legacy_tiocsti = 1`), so the seccomp rule is what stops it there.
   `PTRACE_ATTACH` to a process outside fails; gdb (both Thors) and strace
   (IGX) work on a program inside.
4. and 5. Environment allowlist, private `TMPDIR`, new session without a
   terminal, background processes ended with the command, and the timeout
   killing the whole group: unit tests in `src/tools/shell.rs`.
6. Real work: `git commit`, `make` with `cc`, Python multiprocessing,
   `pytest`, and `cargo build` (dev host and IGX) inside the sandbox.
7. Policy: only the nested-helper case (a nested helper cannot widen the
   outer policy). The cases that depend on S2 (`./agent.toml` widening,
   `.git/hooks`) wait for S2.
8. The seccomp program builds and applies on aarch64 (all tests above).

Findings:

- **Signals reach processes outside the sandbox on ABI 4.** Landlock signal
  scoping needs ABI 6, so `kill` from inside reaches any process of the same
  user on both Thors (test
  `linux_signals_reach_outside_processes_only_before_landlock_abi_6` pins
  this). Abstract Unix sockets, the other ABI 6 scope, are covered by the
  connect deny.
- **CUDA programs fail inside the sandbox on both Thors.** `cuInit` returns
  304 (`CUDA_ERROR_OPERATING_SYSTEM`); outside the sandbox it succeeds. Two
  rules cause this, found with strace:
  1. The driver names its worker thread by writing
     `/proc/self/task/<tid>/comm` and fails when that write is refused
     (`/proc` is read-only in the policy).
  2. With that fixed, the driver binds an abstract Unix socket
     (`@cuda-uvmfd-<netns>-<pid>`) and reports no device (error 100) when
     the network-off filter refuses `bind`.

  Addressed on 2026-10-26 by the opt-in `[security].sandbox_gpu` (see
  [GPU access](#tier-1-landlock--seccomp--rlimits-default) for what it
  costs). With it, `linux_cuda_driver_works` (opt-in, because it uses the
  GPU) runs `cuInit`, `cuCtxCreate` and a 1 MiB `cuMemAlloc` inside the
  sandbox on both Thors, and checks that the same calls fail without it. The
  driver's connect to the MPS control socket (`/tmp/nvidia-mps/control`) is
  still refused, without harm here because MPS was not running; programs
  that use MPS would need that socket.
  `linux_gpu_access_opens_proc_and_unix_servers_but_not_connect` checks the
  setting's effect on any Linux host.
- **Datagrams to local daemons.** Found in review after S3: with the network
  off, `sendto` with an address reached a Unix datagram socket outside (such
  as `/dev/log`). It is now denied
  (`linux_datagrams_to_local_daemons_are_refused_with_the_network_off`);
  `sendmsg` with an address cannot be filtered and remains a gap.

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

macOS ([macOS: Seatbelt](#macos-seatbelt)). Each phase must leave the Linux
build and tests unchanged:

| Phase | Content | Done when |
|---|---|---|
| M0 | Test suite runs green on macOS today: Linux-only tests (for example `/proc` in `shell.rs`) are gated or get a macOS equivalent; `itests/sandbox.rs` skips cleanly | `cargo test` passes on the dev Mac and Linux |
| M1 | Seatbelt profile generator, macOS `probe()` and `wrap()`, network off; unit tests for the generated profile (parameters, deny order, protected git paths) | `execute_bash` runs commands under `sandbox-exec`; the startup log says so |
| M2 | macOS cases in `itests/sandbox.rs`: workspace read and write, `~/.ssh` and rc files unreadable, writes outside refused, `.git/hooks` and `.git/config` refused, network and DNS refused, environment and temp directory, process-group kill; real work with `sh`, `git status` and `commit`, `cc`, `cargo build`, `python3`; nested-sandbox probe | All pass on the dev Mac; Linux itests unchanged |
| M3 | Network-on profile (Mach services from violation logs, reviewed); failure note checked against Seatbelt errors; `mima sandbox --status` for both platforms (shared with S2) | HTTPS works with `sandbox_network = true`; status output is correct on both platforms |
| M4 | CI: add a `macos-latest` job (fmt, clippy, tests, itests); README and `docs/plan.md` updated | CI is green on both operating systems |
| M5 | Eval harness on macOS: Seatbelt profile for hidden checks in place of `bwrap`; `mima` runs unwrapped and sandboxes its own commands; smoke suite validates | `mima-eval validate` and a smoke run pass on macOS against a cloud endpoint |

Eval work on macOS waits for M2. Results that need to be comparable with
Linux runs wait for M5.

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
  discover them without granting the whole home directory. This matters more
  on macOS, where conda and Homebrew-managed language versions often live in
  the home directory.
- macOS: whether Apple will remove `sandbox-exec`. If it does, the fallback
  is the same profile applied through `sandbox_init_with_parameters` in a
  helper, which is a private API.
- macOS: whether to keep the whole `.git` read-only (as Codex does) behind an
  option, at the cost of `git commit` inside the sandbox.
- Whether the integrity check should restore changed hook files or only
  report them.
- How to present escalation requests in non-interactive one-shot mode.
- Whether CUDA MPS clients need the MPS control socket allowed under
  `sandbox_gpu`.
- Whether to deny user namespaces (`unshare`, and `clone`/`clone3` with
  `CLONE_NEWUSER`) in the filter. `clone3` passes its flags in a struct, so
  this needs `clone3` to return ENOSYS (glibc then falls back to `clone`).

## References

- Claude Code sandboxing: <https://code.claude.com/docs/en/sandboxing>;
  <https://www.anthropic.com/engineering/claude-code-sandboxing>
- Anthropic sandbox-runtime: <https://github.com/anthropic-experimental/sandbox-runtime>
- Codex Linux sandbox: <https://github.com/openai/codex/tree/main/codex-rs/linux-sandbox>;
  bwrap failure on Ubuntu 24.04: <https://github.com/openai/codex/issues/17337>
- Gemini CLI sandbox: <https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/sandbox.md>
- Codex macOS Seatbelt: <https://github.com/openai/codex/blob/main/codex-rs/core/README.md>
  (macOS section); base policy
  <https://github.com/openai/codex/blob/main/codex-rs/sandboxing/src/seatbelt_base_policy.sbpl>;
  launcher <https://github.com/openai/codex/blob/main/codex-rs/sandboxing/src/seatbelt.rs>
- Anthropic sandbox-runtime (macOS profile, mandatory deny paths, violation
  log): <https://github.com/anthropics/sandbox-runtime>
- Apple Sandbox Guide (SBPL reference, unofficial):
  <https://reverse.put.as/wp-content/uploads/2011/09/Apple-Sandbox-Guide-v1.0.pdf>
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
