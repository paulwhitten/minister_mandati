# minister_mandati

**minister_mandati** (binary: `mima`) is Latin for *"minister of the mandate"* —
the servant that executes the command. It's an **auditable, single-binary,
edge-native coding agent** written in Rust: a flat Observe → Tool → Append loop
against any **OpenAI-compatible** endpoint (vLLM, Ollama, or cloud), with
human-in-the-loop approval for state-changing actions by default.

> Full design: [rust_terminal_agent_plan.md](rust_terminal_agent_plan.md).

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

Requires Rust 1.88 or newer on Linux.

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
   works. See [Usage](#usage) for the modes of operation.

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

In the REPL, `/help` lists commands (`/tokens`, `/context`, `/session`, `/new`,
`/enable_transcript`, `/disable_transcript`, `/exit`). Ctrl-C
cancels the current turn and returns to the prompt; Ctrl-D exits. In one-shot
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

## Sessions and transcripts

Each run of `mima` is a session; in the REPL, `/new` starts a fresh one. A
session can be recorded to a **transcript**: a JSON Lines file of timestamped
events (UTC) in `~/.mima/transcripts/`, covering instructions, model
responses, approvals, full tool outputs, context compactions and token usage.

Transcripts are **off by default**, and `mima` says so when it starts. Enable
them with `/enable_transcript` (REPL), `--transcript` (one run), or
`[session].transcripts = true`. Files are private to your user (0600) and
never leave the machine; they can contain source code and command output, so
prune them as needed:
`find ~/.mima/transcripts -name '*.jsonl' -mtime +30 -delete`. Format and
record types: [docs/sessions.md](docs/sessions.md).

## Context management

`mima` sizes its working budget from the model's context window, which it
reads from the server (vLLM reports `max_model_len`) or from `[context].window`.
As the conversation grows it first hides old tool outputs behind one-line
placeholders, keeping its own reasoning and tool calls, and only then drops
the oldest steps. Tool calls and their results are never split. `/context`
shows the current budget. Details and the research behind the approach:
[docs/context.md](docs/context.md).

## Observability

Logs use [`tracing`](https://docs.rs/tracing) and go to stderr (stdout stays
clean for the final answer):

```bash
MIMA_LOG=debug cargo run -- "..."      # verbosity via EnvFilter
MIMA_LOG_FORMAT=json cargo run -- "..." # structured JSON logs for auditing
```

## Safety

- Shell commands (`execute_bash`) and file writes (`write_file`) prompt for
  approval by default (`[security]` in `agent.toml`).
- `auto_approve_bash` lists shell command prefixes that skip the prompt, e.g.
  `["cargo test", "git status"]`. Matching is whole-word, and any command with
  shell operators (`;`, `&`, `|`, `$`, backticks, parentheses, redirects,
  backslashes, newlines) still prompts. Auto-approvals are logged.
- Shell commands are killed after `bash_timeout_secs` (default 300). Tool
  output is capped relative to the model's context window, keeping the head
  and tail.
- `allowed_paths` sandboxes the **filesystem tools only**; it is not a shell
  sandbox — shell safety relies on the approval gate.

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
