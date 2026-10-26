# minister_mandati

**minister_mandati** (binary: `mima`) is Latin for *"minister of the mandate"* —
the servant that executes the command. It's an **auditable, single-binary,
edge-native coding agent** written in Rust: a flat Observe → Tool → Append loop
against any **OpenAI-compatible** endpoint (vLLM, Ollama, or cloud), with
human-in-the-loop approval for state-changing actions by default.

> Architecture and design goals: [docs/architecture.md](docs/architecture.md).
> Original technical specification: [rust_terminal_agent_plan.md](rust_terminal_agent_plan.md).

## Why

- **Auditable.** Small codebase, a single binary (optionally statically
  linked, see [Building](#building)), minimal dependencies, no OpenSSL. No
  telemetry, analytics, or update pings — the agent talks only to the
  `base_url` you configure.
- **Edge-native.** Targets the NVIDIA Jetson AGX Thor (128 GB) as a LAN model
  server. Running the agent on the Thor itself (`aarch64`) is a goal; it has
  not yet been verified.
- **BYOK / BYO-endpoint.** Bring your own key and endpoint; nothing is hardcoded.

## Quick start

Requires Rust 1.89 or newer on Linux.

1. Serve a model locally. See [vLLM](https://docs.vllm.ai/en/latest/getting_started/quickstart/) or 
   [Ollama](https://docs.ollama.com/quickstart) quickstart guides for online serving.
2. Configure (optional — convention over configuration):

   ```bash
   # Zero-config: with a local Ollama on :11434, just run. Otherwise override:
   export MIMA_BASE_URL="http://thor.local:8000/v1"
   export MIMA_MODEL="nvidia/NVIDIA-Nemotron-3-Super-120B-A12B-NVFP4"
   export MIMA_API_KEY="ollama"   # any stub for local vLLM/Ollama
   # Or copy the example for full control:
   cp agent.toml.example agent.toml
   ```

   Config is discovered at `./agent.toml` then
   `~/.config/minister_mandati/agent.toml`; if neither exists, built-in defaults
   apply.

3. Build and run:

   ```bash
   cargo build --release
   ./target/release/mima "list the files in the current directory"
   ```

   The binary is `target/release/mima`; `cargo run --release -- <args>` also
   works. The build also produces `target/release/mima-eval`, the evaluation
   tool (see [Evaluation](docs/eval.md)); run it with
   `cargo run --release --bin mima-eval -- <args>`. Plain `cargo run` always
   runs `mima`. See [Usage](#usage) for the modes of operation.

## Usage

```text
$ mima --help
minister_mandati (`mima`) — an auditable, edge-native coding agent.

Run a one-shot instruction, or start an interactive REPL when invoked with no instruction on a terminal.

Usage: mima [OPTIONS] [INSTRUCTION]...

Arguments:
  [INSTRUCTION]...
          Instruction to run. If omitted on a terminal, an interactive REPL starts; if omitted with piped stdin, the piped text is used as the instruction

Options:
  -i, --interactive
          Stay in the interactive REPL after running INSTRUCTION (implied when no instruction is given on a terminal)

      --transcript
          Write a transcript of this session to the transcript directory (default ~/.mima/transcripts). Off by default

      --config <FILE>
          Use this config file instead of discovering agent.toml

      --transcript-path <FILE>
          Write the transcript to FILE (implies --transcript)

      --max-steps <N>
          Override [agent].max_steps for this run

      --sessions
          List recorded sessions (newest first) and exit

      --resume <ID>
          Continue a recorded session: an id (a unique prefix is enough) or "last". Opens the interactive prompt; an INSTRUCTION runs first

      --approve-all
          Approve every tool call without asking. For the evaluation harness only: refused unless MIMA_EVAL_SANDBOX=1 is set (see docs/eval.md)

      --audit
          Required audit mode: recording always on, each record on disk before its action, and a recording failure stops mima (also MIMA_AUDIT=1 or [audit].required)

      --accept-chain-break
          With --resume in required mode: continue a transcript whose audit chain is broken, recording that you accepted it (chain_reset)

      --no-project-instructions
          Do not load AGENTS.md project instructions for this run

      --audit-verify <ID|PATH>
          Check a transcript's audit chain and its heads against the session ledger, then exit: a session id (a unique prefix is enough), "last", or a file path. Exit status 1 if any check fails

      --head <HEAD>
          With --audit-verify: also compare the transcript's head with HEAD, an anchor kept elsewhere (as printed at session end)

      --audit-export <ID|PATH>
          Write a copy of a transcript to share, with content withheld (see --withhold); it verifies with the same hashes. Then exit

      --withhold <SET>
          With --audit-export: what to withhold, comma-separated: "code" (all content, the default), "outputs" (tool outputs and diffs, with the compaction, context and verifier records that quote them), or field names such as turn_start.instruction
          
          [default: code]

      --output <FILE>
          With --audit-export: write the copy to FILE instead of stdout

  -h, --help
          Print help (see a summary with '-h')

  -V, --version
          Print version

Modes:
  mima                     Interactive REPL (no instruction on a terminal)
  mima "<instruction>"     Run once, then exit
  mima -i "<instruction>"  Run the instruction, then stay in the REPL
  echo "..." | mima        Run piped text once, then exit
```

### Modes of operation

| Invocation | Mode | Behavior |
|---|---|---|
| `mima` | Interactive | Opens the REPL with no starting task. |
| `mima "<instruction>"` | One-shot | Runs the instruction, then exits. |
| `mima -i "<instruction>"` | Seeded interactive | Runs the instruction, then keeps the REPL open with the same context for follow-ups. |
| `echo "..." \| mima` | Piped one-shot | Uses stdin as the instruction, then exits. |

`-i` matters only when you also pass an instruction: plain `mima` on a
terminal already starts the REPL, so `mima -i` on its own behaves the same.

In the REPL, `/help` lists commands (`/tokens`, `/context-status`, `/session`, `/sessions`, `/resume`, `/new`,
`/enable_transcript`, `/disable_transcript`, `/exit`). Ctrl-C
cancels the current turn and returns to the prompt; Ctrl-D exits. The prompt
supports line editing (arrow keys, Home/End, Delete) and Up/Down history for
the current session; history is not saved to disk. In one-shot
mode, Ctrl-C aborts with exit code 130.

### Examples

```bash
# One-shot: run a task and exit
mima "list the files in the current directory"

# Words after the flags are joined, so quoting is optional
mima explain what src/main.rs does

# Interactive: start a session and type tasks at the prompt
mima

# Seeded interactive: start with a task, then ask follow-ups in the same context
mima -i "read src/config.rs and summarize the config precedence"
#   > now add a unit test for the env-var override
#   > /tokens
#   > /exit

# Piped: feed an instruction (or a file of instructions) on stdin
echo "summarize Cargo.toml" | mima
mima < task.txt

# Via cargo during development
cargo run --release -- -i "run the tests and fix any failures"
```

## Building

```bash
cargo build --release                  # target/release/mima (dynamically linked)
```

Fully static x86_64 binary using static glibc (no extra toolchain needed):

```bash
RUSTFLAGS="-C target-feature=+crt-static" \
  cargo build --release --target x86_64-unknown-linux-gnu
# -> target/x86_64-unknown-linux-gnu/release/mima ("static-pie linked")
```

Static glibc still resolves hostnames through NSS at runtime, so behavior can
differ from the dynamic build on hosts with unusual NSS configuration. A musl
build (`--target x86_64-unknown-linux-musl`) avoids this but needs a musl C
compiler (e.g. the `musl-tools` package) for the `ring` crate.

## Project instructions (AGENTS.md)

`mima` reads `AGENTS.md` files, the common convention for telling coding
agents how to work in a repository: every `AGENTS.md` from the repository
root (the nearest `.git`) down to the working directory, root first, plus
your own `~/.config/minister_mandati/AGENTS.md` if it exists. They are read
once at startup, cleaned of invisible characters, capped at 8 KiB in total,
and added to the end of the system prompt in a labeled section that says
they cannot change tools, approvals or the sandbox. `mima` prints what it
loaded (`Instructions: AGENTS.md (1.2 KiB)`); `/instructions` shows the
text; `--no-project-instructions` turns it off for a run. A workspace
`./agent.toml` can only turn loading off or lower the cap, and can no longer
set the system prompt. Writes to files named like an instruction file or
`agent.toml` always ask for approval. Keep the file short and specific (the
commands to build and test, what not to touch): in published measurements,
long or generated files raised cost without improving results. Design:
[docs/design/design-project-instructions.md](docs/design/design-project-instructions.md).

## Sessions and transcripts

Each run of `mima` is a session; in the REPL, `/new` starts a fresh one. A
session can be recorded to a **transcript**: a JSON Lines file of timestamped
events (UTC) in `~/.mima/transcripts/`, covering instructions, model
responses, approvals, full tool outputs, context compactions and token usage.

A recorded session can be continued later: `mima --sessions` lists them and
`mima --resume <id|last>` (or `/resume` in the REPL) rebuilds the context and
appends to the same transcript
([docs/design/session-resume.md](docs/design/session-resume.md)).

Transcripts are **off by default**, and `mima` says so when it starts. Enable
them with `/enable_transcript` (REPL), `--transcript` (one run), or
`[session].transcripts = true`. Files are private to your user (0600) and
never leave the machine; they can contain source code and command output, so
prune them as needed:
`find ~/.mima/transcripts -name '*.jsonl' -mtime +30 -delete`. Format and
record types: [docs/sessions.md](docs/sessions.md).

### Audit chain and session ledger

Every transcript record carries a SHA-256 `hash` of itself and the record
before it, so an edited, deleted, inserted or reordered record is detected.
When a session ends, `mima` prints the transcript's head (its last hash).

The **session ledger** (`~/.mima/ledger.jsonl`, chained the same way) gets a
line for every session start, recording on/off change and session end:
time, user, host, working directory, whether recording was on, and the
transcript's head. It is written even when transcripts are off and holds no
instructions, outputs or code. Because it keeps each head, a transcript that
was cut short or rewritten no longer matches it.

Every record also names its **actor** (operator, model, rule, tool, system)
and, when one of mima's deterministic rules decided (an auto-approval, a loop
guard, a compaction), the **rule id**. The session header records the build,
the binary's hash, the configuration and where it came from, the system
prompt, the tool specifications, sampling settings, the server's model, the
sandbox, the operator and the workspace's git state; tool results record file
hashes before and after, and the files each shell command changed.

Check a transcript with `mima --audit-verify <id|last|path>` (offline; exit
status 1 if a check fails; `--head` compares with a head kept elsewhere).
Share one without its code with `mima --audit-export <id> --withhold code
--output copy.jsonl`: the copy verifies with the same hashes.

**Required mode** (`--audit`, `MIMA_AUDIT=1` or `[audit].required = true`),
for regulated work: recording is always on, each record is on disk before
the action it describes, and a recording failure stops `mima` before its
next action. Settings are in `[audit]`
([agent.toml.example](agent.toml.example)); design:
[docs/design/design-audit-record.md](docs/design/design-audit-record.md).

## Evaluation

`mima-eval` (built alongside `mima`) runs task suites against one or more
models with the real `mima` binary, scores trials with deterministic checks
(hidden tests, build results, file state), and reports pass rates with error
bars and paired model comparisons. Shell commands and checks run in a
network-less `bwrap` sandbox per trial.

```bash
./target/release/mima-eval validate evals/suites/smoke.toml
./target/release/mima-eval run evals/suites/smoke.toml --profiles profiles.toml
```

`evals/` holds a six-task smoke test of the harness. The full task set is
kept in a separate repository so that its solutions and hidden checks are not
published (they would leak into model training data); `mima-eval` runs it by
path. See [docs/eval.md](docs/eval.md).

## Context management

`mima` sizes its working budget from the model's context window, which it
reads from the server (vLLM reports `max_model_len`) or from `[context].window`.
As the conversation grows it first hides old tool outputs behind one-line
placeholders, keeping its own reasoning and tool calls, and only then drops
the oldest steps. Tool calls and their results are never split.
`/context-status` shows the budget, what fills the context, and the
compaction thresholds. Details and the research behind the approach:
[docs/context.md](docs/context.md).

## Observability

Logs use [`tracing`](https://docs.rs/tracing) and go to stderr (stdout stays
clean for the final answer):

```bash
MIMA_LOG=debug cargo run -- "..."      # verbosity via EnvFilter
MIMA_LOG_FORMAT=json cargo run -- "..." # structured JSON logs for auditing
```

## Safety

- Shell commands (`execute_bash`) and file changes (`edit_file`,
  `write_file`) prompt for approval by default (`[security]` in
  `agent.toml`). The prompt shows the full command, or a diff of the file
  change. The model must have read a file's current content before changing
  it; see [docs/editing.md](docs/editing.md). Answer `y`/`yes` or
  `n`/`no` (any case); Enter means no, and anything else is asked again.
- `auto_approve_bash` lists shell command prefixes that skip the prompt, e.g.
  `["cargo test", "git status"]`. Matching is whole-word, and any command with
  shell operators (`;`, `&`, `|`, `$`, backticks, parentheses, redirects,
  backslashes, newlines) still prompts. It applies only while shell commands
  are sandboxed (below); without the sandbox every command prompts.
  `auto_approve_sandboxed = true` approves every sandboxed command (off by
  default: the sandbox does not protect the workspace itself). Auto-approvals
  are logged.
- Shell commands are killed after `bash_timeout_secs` (default 300). Tool
  output is capped relative to the model's context window, keeping the head
  and tail.
- Each shell command gets only an allowlisted environment (PATH, HOME,
  locale, toolchain locations such as `CARGO_HOME`), never mima's API key or
  other credentials; `env_passthrough` adds names. It runs in its own session
  with no controlling terminal, its `TMPDIR` is a private directory removed
  when mima exits, and background processes it starts are stopped when it
  returns or times out.
- Shell commands are sandboxed with Landlock and seccomp when the kernel
  supports them (`[security].sandbox = "auto"`, the default): they can read
  system directories, toolchains (`sandbox_read_paths`) and the workspace,
  write only the workspace and their temp directory, and have no network.
  Keys and credentials in your home directory are unreadable to them.
  `sandbox = "required"` refuses to run commands without the sandbox;
  `sandbox_network = true`, `sandbox_gpu = true` (below) and
  `sandbox_writable_paths` widen it. On macOS commands run under Seatbelt
  (`sandbox-exec`). Where no sandbox can be applied, `auto` runs commands
  unsandboxed (still behind approval) and `required` refuses them. See
  [docs/design/design-shell-sandboxing.md](docs/design/design-shell-sandboxing.md).
- CUDA programs do not run inside the default sandbox on Linux. To allow them
  (for example on the Jetson Thor), set this in your user config:

  ```toml
  [security]
  sandbox_gpu = true
  ```

  It makes `/proc` writable and lets Unix sockets accept connections, which
  the CUDA driver needs; opening connections and the network stay blocked.
  It also lets sandboxed commands change some settings of your other
  processes (such as their out-of-memory priority). It is off by default,
  cannot be set from a workspace `./agent.toml`, and is not granted when
  mima runs as root or no NVIDIA device nodes exist. The startup log and
  the sandbox summary show when it is in effect. Details:
  [GPU access](docs/design/design-shell-sandboxing.md#tier-1-landlock--seccomp--rlimits-default).
- `read_file`, `list_dir`, `find_files` and `search_files` are read-only and
  never prompt; the search tools skip `.git`, build and dependency
  directories and never follow symlinks.
- `allowed_paths` confines mima's file tools, and is also the writable
  workspace for sandboxed shell commands.

## Status

Pre-1.0, early development. Working today: one-shot and interactive modes,
streaming, native tool calling (ReAct fallback implemented, less tested),
approval gating, loop guards, and token accounting. Validated live against one
model (Nemotron 3 Super 120B on vLLM). Not yet done: the evaluation harness is a
skeleton and not wired into the CLI, and `aarch64` / on-Thor builds are
unverified. See [docs/plan.md](docs/plan.md) for the roadmap and
[rust_terminal_agent_plan.md](rust_terminal_agent_plan.md) for the design.

## License

MIT. See [LICENSE](LICENSE).
