# Technical Specification: Lightweight Open-Source Rust Coding Agent

> **Version:** v0.13.0 (design consolidated post-Phase 0)
> **Status:** Phase 0 research complete; scaffold runnable; consolidating design
> **Crate name:** `minister_mandati` (binary `mima`)
> **Config file:** `agent.toml` · **Binary:** `mima`

A terminal-native, **Linux-first**, privacy-first AI coding agent built in Rust. It uses a flat execution loop, enforces strict **Bring-Your-Own-Key (BYOK)** and **Bring-Your-Own-Endpoint** constraints, and requires human-in-the-loop approval for all state-changing local actions (shell commands and file writes).

## Project Goals & Positioning

This project has two deliberately-held motivations, and its scope is chosen to serve both:

1. **Learning goal (primary).** Deepen hands-on **Rust** (async, traits, error handling, single-binary distribution) and **agentic-coding** fundamentals (execution loops, tool calling, context management, human-in-the-loop safety). The build itself is a first-class outcome — the journey matters as much as the artifact. The maintainer has ~2 years of prior Rust experience but is treating this as a refresher, so the design favors idiomatic, well-understood patterns over advanced or exotic language features.
2. **Differentiation goal (secondary).** Rather than compete head-on with mature general-purpose CLI agents (Aider, gptme, OpenCode, OpenHands, Goose), this project targets a niche where the maintainer has an **unfair advantage**: the **NVIDIA Jetson AGX Thor (128 GB)**. The positioning is therefore not "another privacy CLI agent" but:

   > **An auditable, single static binary, edge-native coding agent for the Jetson Thor.**

### The single auditable binary

The defining product property is a **small, single, statically-linked Rust binary** whose entire behavior a security-minded operator can read end-to-end:

* **One artifact.** Compiles to a single self-contained executable (target the `aarch64-unknown-linux-gnu` Thor and the dev host) with no runtime interpreter, package tree, or plugin system to audit.
* **Auditable surface.** The codebase stays small enough to be reviewed in full. No hidden network calls: the agent talks **only** to the configured `base_url` — no telemetry, analytics, update pings, or third-party services.
* **Zero-exfiltration posture.** Suitable for air-gapped / on-prem edge use: with a local `base_url` (Ollama/vLLM on the Thor), code and context never leave the device.
* **Edge-native.** Optimized for on-device inference against the Thor's 128 GB unified memory rather than assuming a cloud endpoint.

These goals bound the scope: the agent stays intentionally minimal (see the flat loop below) rather than chasing feature parity with the incumbents.

## Domain Requirements: Linux-First Mastery

This agent is an open-source, Linux-first tool. Beyond general coding competence, it must be a domain expert in the Linux and systems space and aim to exceed other agents there.

The formal, testable requirements for this domain are specified with RFC 2119 language in [`docs/requirements.md`](docs/requirements.md) (`REQ-LNX`, `REQ-CMD`, `REQ-DST`, `REQ-KRN`, `REQ-LNG`, `REQ-EXC`), tracing to R1–R6 below. In summary, the agent must be Linux-first and demonstrate mastery of Linux and Unix commands, shells, service and system administration, and major distributions; deep knowledge of the Linux kernel, modules, the kbuild and Kconfig system, and the kernel contribution workflow (suitable for kernel, module, and Linux Foundation work); and exceptional, idiomatic development in C, Rust, and Python with their build, debug, and profiling tools. For Linux and kernel work specifically, the goal is best-in-class.

### Delivering the expertise

Two complementary mechanisms, in priority order:

1. **Baseline knowledge.** A strong, Linux-focused system prompt plus reusable skills and lessons that encode domain conventions and checklists (for example, a kernel patch submission checklist, or distro-specific playbooks).
2. **Grounded tools.** Specialized built-in tools and optional MCP plugins (below) that fetch authoritative, current facts (man pages, kernel docs, distro package databases) instead of relying on model memory.

### MCP plugins (optional, local, auditable)

To extend domain capability without bloating the core binary, the agent will add an **MCP client**. To preserve the auditable, zero-exfiltration posture (see Goals and Design Principles):

* MCP servers must be **local** processes (stdio); no remote or cloud MCP by default.
* Plugins are **opt-in** and declared in configuration.
* Core Linux, C, Rust, and Python competence must work with the **built-in tools alone**; MCP is additive, not required.

Candidate plugins:

* man and info pages, plus tldr lookups.
* Kernel documentation and source cross-reference, with checkpatch and get_maintainer wrappers.
* Distro package queries (apt, dnf, pacman, zypper) and file-to-package mapping.
* Build and debug helpers (cargo, cmake, gdb, lldb, perf, strace).
* Git patch workflow (git format-patch, git send-email).

**Tension to manage.** MCP expands the trust surface, which is in tension with the single auditable binary goal. It therefore stays optional and local, and the core stays minimal.

## Design Principles

### Convention over Configuration

The agent follows [convention over configuration](https://en.wikipedia.org/wiki/Convention_over_configuration): pick sensible defaults that are correct for 95%+ of cases, and require the user to configure only what genuinely cannot be inferred. Configuration stays fully available for the remaining cases, but it is never mandatory for the common path.

Rules of thumb:

* Add a sensible default before adding a configuration knob.
* The agent should run with little or no configuration. `agent.toml` is optional; when absent, built-in defaults apply.
* Only two things truly must be provided: the endpoint (`base_url`) and the model. Even these should fall back to a local convention (for example `http://localhost:11434/v1`) or an environment override where possible.
* Every default is documented next to its override (config key and/or environment variable).

Established conventions:

| Concern | Default (convention) | Override |
| --- | --- | --- |
| Config file | `./agent.toml`, then `~/.config/minister_mandati/agent.toml` | explicit path argument |
| Secrets | `${ENV_VAR}` expansion in config | any environment variable |
| Temperature | `0.2` | `[agent].temperature` |
| History window | `20` turns | `[agent].max_history_turns` |
| Response budget | `4096` tokens | `[agent].max_tokens` |
| Tool calling | `auto` (native, ReAct fallback) | `[agent].tool_calling` |
| Approvals | bash and writes require approval | `[security]` flags |
| Filesystem sandbox | `["./"]` | `[security].allowed_paths` |
| Log level | `info` | `MIMA_LOG` |
| Log format | human-readable | `MIMA_LOG_FORMAT=json` |
| Log target | stderr | n/a |

Current gap: today `base_url` and `default_model` are required and `agent.toml` must exist. Making the file optional with local-default fallbacks is the next step toward full convention-over-configuration (see Open Questions).

## Architecture Constraint: Presentation Decoupling

The agent core is decoupled from presentation. Core logic (the loop, client, tools, context) does not write to the terminal directly and does not depend on any rendering framework. It emits typed events and consumes typed inputs through a small presenter interface, so the same core can drive different front ends. See `REQ-UI-*` in [`docs/requirements.md`](docs/requirements.md).

Principles:

* **CLI-first.** A plain terminal interface is the default and works with only the core dependencies.
* **Pluggable front ends.** A rich TUI or a REST/HTTP interface can be added behind the same interface, selectable without touching core logic, ideally as optional Cargo features so the default binary stays small and auditable.
* **Event-driven.** The core emits events (task started, model output or token stream, tool-call requested, tool output, approval request, final answer) and receives inputs (instruction, approval decision). Presenters render events and collect inputs.
* **REST is local and optional.** An HTTP interface (streaming via SSE or WebSocket) is allowed as an optional presentation layer under the same zero-egress constraints.

Status: a `Presenter` trait now decouples the loop from the terminal (`src/presenter.rs`). The core emits typed events (task started, tool requested, tool completed, streamed content delta, step-cap reached, final answer) and collects the approval decision through the presenter; `CliPresenter` is the plain-terminal default. Approval *policy* (which tools are gated) stays core logic; only the prompt and rendering live in the presenter. Live token streaming is wired: with `[agent].stream = true` the client consumes SSE (via `Response::chunk`, no extra deps), emits content deltas through `Presenter::stream_delta`, reassembles tool calls by index, and preserves token usage via `stream_options.include_usage`. Remaining coupling: the top-level usage/error messages in `main()` still use `eprintln!`.

Candidate Rust crates to evaluate:

| Layer | Options |
| --- | --- |
| TUI framework | `ratatui` (modern, immediate-mode; backends `crossterm`, `termion`, `termwiz`); `cursive` (retained, ncurses-like, closest to classic curses) |
| Terminal backend | `crossterm` (cross-platform), `termion` (Unix), `termwiz` |
| Simple CLI UX | `console`, `dialoguer`, `indicatif`, `inquire` (prompts, progress, styling); `reedline` (line editor / REPL input) |
| Tables / color | `comfy-table`, `tabled`; `owo-colors`, `anstyle`, `nu-ansi-term` |
| REST / HTTP | `axum` (fits the tokio stack), with SSE or WebSocket for streaming; alternatives `actix-web`, `poem` |

Direction to revisit in Phase 0/1: keep the current plain CLI behind a `Presenter` trait, evaluate `ratatui` (mainstream) versus `cursive` (curses-like) for a rich TUI, and keep `axum` as the REST option.

## Phase 0 Findings → Design Decisions

Phase 0 research is complete. Full findings live in private research notes
(not published: `agents-core-concepts.md`, `agent-survey.md`,
`claude-code-leak.md`, `claude-code-southbridge-analysis.md`, `mooc-findings.md`,
`advanced-mooc-findings.md`). Consolidated decisions:

* **Flat ReAct loop stays the core** (validated by Claude Code, Goose, Anthropic).
  Add a bounded **max-one-branch** sub-agent ("squad") only for parallelizable,
  verifiable subtasks — no nested agents, and **no tree-search planner** (real
  FS/systemd/git actions are irreversible; this is an affirmative justification
  for the approval gate).
* **Verifier-gated self-refine (hard rule).** Never self-correct without an
  external verifier (compiler/test/sanitizer) — oracle-free self-refinement
  degrades accuracy. Use the self-debugging feedback pattern; Chain-of-Verification
  as one cheap pass before destructive actions.
* **Context Revision step** in the loop: compaction (summarize near the limit) +
  JIT/ripgrep retrieval over RAG, prioritizing **precision** (Context Rot:
  distractors actively hurt). Replace the chars/4 estimate with a real tokenizer.
* **Tooling:** read-only tools run in parallel, write tools serial; replace
  whole-file writes with **SEARCH/REPLACE `edit_file` + `multi_edit`**
  (read-before-edit, mtime check, `expected_replacements`, diff preview);
  dedicated ripgrep-backed `grep`/`glob`; a model-managed **TODO** tool;
  **sanitize all LLM-produced tool args/code** (block SQLi/RCE/SSRF).
* **Safety = least-privilege policy.** Scoped, dynamically-tightening approval
  rules (`Tool(glob)`), command-injection prefix detection, **taint-tracking** of
  untrusted content (files, repos, web, MCP output), human-in-the-loop for
  state-changing actions, the "agents rule of two". Consider a Privtrans-style
  small privileged monitor vs a larger unprivileged worker.
* **Dual-model:** a small model for cheap ops (summaries, status labels) —
  edge-friendly on the Thor.
* **Extensibility:** MCP client (local stdio); optional **ACP** server for editors.
* **Zero telemetry** stays a hard guarantee (the Claude Code leak's 837 `tengu_*`
  events are the anti-pattern; so is its 512k-line, zero-test, 486-complexity
  monolith — we stay small, tested, and auditable).
* **Positioning sharpened:** a defensive **security / vuln-detection** capability
  is our strongest differentiator (auditable-edge + cyber background) and is
  *cheaply verifiable* (a sanitizer/ASan crash is ground truth).

## Evaluation Subsystem (first-class)

Evaluation is a first-class subsystem, not a downstream afterthought — the central
lesson of both Berkeley MOOCs.

* **Verifier vector, not a boolean.** Composable typed graders: compile ·
  unit/integration/smoke test · sanitizer (ASan/valgrind) · lint · end-state diff
  · cost (tokens/turns/wall-time) · step count · format. Prefer verifiable graders;
  use LLM-as-judge only for non-verifiable quality, sanitized against injection
  and averaged over runs/judges.
* **Reliability:** report **pass@1 and pass@k** over N independent seeds.
* **Statistical rigor:** report the mean with **standard error**, compare versions
  with a **paired** test (variance of `A(x) − B(x)`) → z-score, and state the
  **minimum detectable effect size** for the set.
* **Task tracks (Linux-first):** verifiable bug-fix (SWE-bench-Verified style) ·
  kernel/systems build (compile + boot/smoke) · **vuln repro / hardening**
  (sanitizer-verified) · terminal/command (end-state diff) · rubric-graded quality.
* **Security tracks:** a **vuln-detection** track (CyberGym/ARVO-style, sanitizer
  oracle) and an **indirect prompt-injection red-team** track (AgentXploit-style,
  attack-success-rate → 0).
* **Anti-gaming:** hide reference tests during runs; network-isolate the sandbox;
  contamination control (dynamic + post-cutoff slices); monitor separability; run
  each task under ≥2 harness/tool-set variants to catch robustness regressions.
* **Harness-swappable** so production == test (expose over MCP where practical).

## Changelog

### v0.14.0

* **Named the project `minister_mandati`** (binary `mima`); `agent.toml`
  retained as the config filename.
* **Added** `clap`-based CLI with one-shot and interactive REPL modes
  (persistent context, slash-commands, per-turn Ctrl-C cancel with rollback).
* **Added** loop guards: per-tool dedupe of identical successful writes and a
  no-progress window guard (nudge then terminate).
* **Added** a composable system prompt (prefix/body/suffix, resolved
  env -> `agent.toml` -> default) with a full-replacement escape hatch.
* **Unified** the environment namespace under `MIMA_*`.

### v0.13.0

* **Completed Phase 0 research** (two Berkeley MOOCs, agent survey, Claude Code
  leak analyses) and consolidated the findings into design decisions.
* **Promoted evaluation to a first-class subsystem** (verifier vector, pass@k,
  paired error bars, security + injection red-team tracks).
* **Added** loop/tool rules: verifier-gated self-refine, Context Revision step,
  read-parallel/write-serial, SEARCH/REPLACE edits, least-privilege policy +
  tool-arg sanitization + taint-tracking, dual-model, ACP option.
* **Added** a defensive security/vuln-detection capability as a headline
  differentiator (sanitizer-verifiable).

### v0.12.0

* **Licensed** the project under **MIT** (added `LICENSE` and the Cargo `license` field).
* **Added** a development progress tracker (kept out of the published tree).
* **Added** a separate private research repository for papers and notes.

### v0.11.0

* **Added** a presentation-decoupling architecture constraint: CLI-first, event-driven core, pluggable front ends (TUI and optional local REST), with candidate Rust crates.
* **Added** `REQ-UI-1` through `REQ-UI-6` to `docs/requirements.md`.

### v0.10.0

* **Extracted** the Linux-first domain requirements into [`docs/requirements.md`](docs/requirements.md) using RFC 2119 language; summarized them here with a pointer.
* **Added** [`docs/plan.md`](docs/plan.md), a phased roadmap whose Phase 0 is deep research into agentic systems and a survey of existing agents.
* **Added** a gitignored local folder for development notes.

### v0.9.0

* **Added** Linux-first domain requirements (R1–R6): mastery of Linux/Unix commands, shells, the kernel, modules, distros, and administration, plus exceptional C, Rust, and Python development, targeting kernel, module, and Linux Foundation projects.
* **Designed** optional, local, auditable MCP plugins to ground domain knowledge, and recorded MCP client support as next work.
* **Strengthened** the default system prompt to reflect Linux/C/Rust/Python expertise.

### v0.8.0

* **Adopted** convention over configuration as an explicit design principle, with a documented default/override table.
* **Recorded** the zero-config default path (optional `agent.toml` with local fallbacks) as the next step.

### v0.7.0

* **Implemented** the runnable scaffold: `config`, `context`, `schema`, `client`, `tools/` (`mod`, `fs`), and `main` — `cargo build`, `clippy`, and `cargo test` are green.
* **Standardized error handling on [`snafu`](https://docs.rs/snafu)**: typed per-module `Error` enums with context selectors; `snafu::Whatever` at the `main` boundary.
* **Added observability via [`tracing`](https://docs.rs/tracing)**: instrumented loop/client/tool boundaries, `MIMA_LOG` env-filter, human logs by default and `MIMA_LOG_FORMAT=json` for structured/auditable output on stderr.
* **Added** a Build & Target section for the single auditable `aarch64` binary, plus a size-optimized release profile.
* **Clarified** that vLLM normalizes Qwen3 XML tool calls into standard OpenAI `tool_calls`, so the native path handles them directly.
* **Added** `agent.toml.example`, `README.md`, and unit tests for env-var expansion.

### v0.6.0

* **Clarified** the Thor's role: it is primarily a **LAN LLM server** (vLLM, OpenAI-compatible) that other machines connect to by `base_url`; running the agent itself on the Thor (aarch64) is a secondary, optional deployment.
* **Referenced** [jetson-ai-lab.com/models](https://www.jetson-ai-lab.com/models) as the canonical source for serving models on the Thor.
* **Added** a reference for example Thor models (Qwen3.6-35B, Nemotron-3-Super-120B) and their vLLM run commands.

### v0.5.0

* **Moved** references and prior work (BWIM papers, `autonomous_coding_agent`, AgentBeats results) out of this spec into separate, unpublished project notes. This spec now holds the technical design only.

### v0.4.0

* **Added** a References & Prior Work section citing the maintainer's related research and competition results.
* **Noted** the maintainer's ~2 years of Rust experience (refresher context) in the learning goal.

### v0.3.0

* **Added** a Project Goals & Positioning section: dual learning goal (Rust + agentic coding) and the differentiation goal.
* **Positioned** the product as an *auditable, single static binary, edge-native coding agent for the Jetson AGX Thor*, with a zero-exfiltration / no-telemetry posture.

### v0.2.0

* Neutralized naming to placeholders (`cli_agent` / `agent.toml` / `agent`) pending a final product name.
* **Fixed** `main.rs` argument bug (`&args[1]`), tool-result error handling, and the unflushed approval prompt.
* **Defined** the previously-missing `ToolRegistry` (registration, dispatch, and OpenAI schema export).
* **Added** tool-schema → OpenAI `tools` plumbing and a **ReAct text fallback** for local models without native tool calling.
* **Added** context-window / token-budget management for `max_history_turns`.
* **Corrected** the security model: file writes are now gated, and `allowed_paths` is scoped honestly to the filesystem tools (it cannot sandbox arbitrary shell).
* **Added** `${ENV_VAR}` expansion for secrets in config.
* **Added** local inference host guidance for the NVIDIA Jetson AGX Thor (128 GB).

### v0.1.0

* Initial draft: project topography, flat loop concept, `BaseTool` trait, and `BashExecutor`.

## Target Deployment

The agent is endpoint-agnostic, but the reference setup uses an **NVIDIA Jetson AGX Thor (128 GB unified memory)** in two distinct roles:

1. **Primary role — LAN LLM server.** The Thor serves large quantized models over an **OpenAI-compatible HTTP endpoint** (via **vLLM**, or Ollama) to any machine on the local network. Developers run the agent on their own dev boxes and point `base_url` at the Thor. Code and context stay on the LAN — nothing leaves for a cloud provider.
2. **Secondary/optional role — agent host.** Because the agent is designed to compile to a single static `aarch64-unknown-linux-gnu` binary (target; not yet verified on the Thor), it can also run *directly on the Thor* alongside the model server for a fully self-contained, air-gapped appliance.

Either way, the agent connects purely by `base_url`, so the same binary works against a laptop's Ollama, the Thor on the LAN, or a cloud API.

```text
        dev boxes on the LAN                      NVIDIA AGX Thor (128 GB)
┌────────────┐                          ┌───────────────────────────────────┐
│ mima       │  OpenAI-compatible HTTP  │  vLLM / Ollama  ·  quantized LLM   │
│ (laptop)   │ ───────────────────────► │  serves :8000 / :11434 to the LAN │
│ mima       │ ◄──────────────────────── │                                   │
│ (worksta.) │        JSON / SSE        │  (optional) mima runs here        │
└────────────┘                          └───────────────────────────────────┘
```

**Serving models on the Thor.** The canonical reference is [jetson-ai-lab.com/models](https://www.jetson-ai-lab.com/models). Example models include `nvidia/Qwen3.6-35B-A3B-NVFP4` and `nvidia/NVIDIA-Nemotron-3-Super-120B-A12B-NVFP4`. Notably, models like Qwen3.6 expose **native tool calling** (vLLM `--enable-auto-tool-choice --tool-call-parser qwen3_xml`), which the agent's `tool_calling = "auto"` mode can use directly, falling back to ReAct only for models that lack it.

## Project Topography

```text
minister_mandati/
├── Cargo.toml
├── agent.toml.example   # Sample configuration (copied to ./agent.toml)
├── src/
│   ├── main.rs          # CLI entrypoint, initialization, and the core orchestrator loop
│   ├── config.rs        # TOML config parser with ${ENV_VAR} expansion
│   ├── client.rs        # OpenAI-compatible API client (Ollama, vLLM, cloud) + streaming
│   ├── context.rs       # Message history, token budgeting, and system-prompt definitions
│   ├── schema.rs        # ToolSpec → OpenAI `tools` JSON, and tool-call parsing (native + ReAct)
│   └── tools/
│       ├── mod.rs       # ToolRegistry, BaseTool trait, ToolSpec, and dispatch
│       ├── fs.rs        # Path-sandboxed file discovery, reading, and editing tools
│       └── terminal.rs  # Command runner with interactive confirmation gates
```

## 1. Configuration (`agent.toml`)

The application avoids hardcoded paths. It reads a local (`./agent.toml`) or global (`~/.config/minister_mandati/agent.toml`) configuration that determines where requests are routed, allowing effortless hot-swapping between remote APIs and local offline infrastructure. Secrets should be provided via `${ENV_VAR}` references rather than plaintext.

```toml
[provider]
# "openai" | "anthropic" | "custom" (any OpenAI-compatible server)
type = "custom"
api_key = "${MIMA_API_KEY}"          # expanded from env at load time; "ollama" works as a stub
base_url = "http://thor.local:11434/v1"  # AGX Thor on the LAN (or http://localhost:11434/v1)
default_model = "deepseek-coder:32b"

[agent]
temperature = 0.2
max_history_turns = 20      # sliding-window cap; see context management
max_tokens = 4096           # response budget
tool_calling = "auto"       # "native" | "react" | "auto" (probe, then fall back)
stream = true               # stream assistant output to the terminal
system_prompt_override = ""

[security]
# Approval gates for state-changing actions. Read-only tools never prompt.
require_approval_for_bash = true
require_approval_for_writes = true
# Sandbox for the filesystem tools ONLY. This does NOT constrain shell commands.
allowed_paths = ["./"]
```

> **Security note.** `allowed_paths` is enforced by the filesystem tools (`fs.rs`), which canonicalize every path and reject anything outside the allowed roots. It is **not** a shell sandbox: a command run through `execute_bash` can read or write anywhere the user can. Shell safety therefore relies on the human approval gate (`require_approval_for_bash`), not on `allowed_paths`.

## 2. Core Engine (`src/main.rs`)

The system uses a flat execution design—no multi-agent state trees or recursive abstractions. The agent operates sequentially: **Observe State → Invoke Tool → Capture stdout/stderr → Append to Context → Repeat**. A step cap prevents runaway loops.

```rust
use std::io::{self, Write};

mod client;
mod config;
mod context;
mod schema;
mod tools;

use anyhow::Result;
use context::{AgentContext, Message};
use tools::ToolRegistry;

const MAX_STEPS: usize = 50;

#[tokio::main]
async fn main() -> Result<()> {
    // 1. Initialize configuration and the tool registry.
    let config = config::Config::load("agent.toml")?;
    let registry = ToolRegistry::init_default(&config);
    let mut ctx = AgentContext::new(config, registry.specs());

    // 2. Read the instruction from argv (everything after the binary name).
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("Usage: agent \"<instruction>\"");
        std::process::exit(2);
    }
    let user_prompt = args.join(" ");
    ctx.add_message(Message::user(&user_prompt));

    eprintln!("mima initialized | terminal mode");

    // 3. Flat execution loop with a hard step cap.
    for _ in 0..MAX_STEPS {
        let response = client::generate_completion(&ctx).await?;

        let calls = response.tool_calls.unwrap_or_default();
        if calls.is_empty() {
            println!("\n{}", response.content);
            return Ok(());
        }

        // Record the assistant turn (with its tool-call requests) before executing.
        ctx.add_message(Message::assistant_tool_calls(&response));

        for call in calls {
            eprintln!("tool: {} {}", call.name, call.args);

            if !approve_if_required(&ctx.config, &call)? {
                ctx.add_message(Message::tool_result(
                    &call.id,
                    "Error: user denied permission for this action.",
                ));
                continue;
            }

            // execute() always returns a String for the model — Ok output or an "Error: ..." string.
            let result = registry.execute(&call.name, &call.args).await;
            let payload = match result {
                Ok(out) => out,
                Err(err) => format!("Error: {err}"),
            };
            ctx.add_message(Message::tool_result(&call.id, &payload));
        }

        ctx.enforce_budget(); // trim history to max_history_turns / token budget
    }

    eprintln!("Reached MAX_STEPS ({MAX_STEPS}) without completion.");
    Ok(())
}

/// Prompts for approval on state-changing tools. Returns Ok(true) when allowed.
fn approve_if_required(cfg: &config::Config, call: &tools::ToolCall) -> Result<bool> {
    let needs_approval = match call.name.as_str() {
        "execute_bash" => cfg.security.require_approval_for_bash,
        "write_file" | "edit_file" => cfg.security.require_approval_for_writes,
        _ => false, // read-only tools never prompt
    };
    if !needs_approval {
        return Ok(true);
    }

    print!("Approve `{}`? (y/N): ", call.name);
    io::stdout().flush()?; // ensure the prompt is visible before blocking on stdin
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().eq_ignore_ascii_case("y"))
}
```

## 3. Modular Tool Framework (`src/tools/mod.rs`)

Every tool implements a standard async trait and advertises a machine-readable `ToolSpec` (name, description, JSON-Schema parameters). The registry owns the tools, dispatches calls by name, and exports specs both for the OpenAI `tools` array and for the ReAct system prompt.

```rust
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;

/// Machine-readable description used to build the model's tool schema.
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value, // JSON Schema for the arguments object
}

/// A tool-call request parsed from the model (native tool_calls or ReAct text).
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: Value,
}

#[async_trait]
pub trait BaseTool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn execute(&self, args: &Value) -> Result<String>;
}

pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn BaseTool>>,
}

impl ToolRegistry {
    pub fn init_default(cfg: &crate::config::Config) -> Self {
        let mut tools: HashMap<String, Box<dyn BaseTool>> = HashMap::new();
        for tool in [
            Box::new(BashExecutor) as Box<dyn BaseTool>,
            Box::new(crate::tools::fs::ReadFile),
            Box::new(crate::tools::fs::WriteFile::new(cfg.security.allowed_paths.clone())),
            Box::new(crate::tools::fs::ListDir::new(cfg.security.allowed_paths.clone())),
        ] {
            tools.insert(tool.spec().name.to_string(), tool);
        }
        Self { tools }
    }

    /// All specs, used to build the OpenAI `tools` array and the ReAct prompt.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|t| t.spec()).collect()
    }

    /// Dispatch by name. Unknown tools return an error string the model can recover from.
    pub async fn execute(&self, name: &str, args: &Value) -> Result<String> {
        match self.tools.get(name) {
            Some(tool) => tool.execute(args).await,
            None => Err(anyhow!("unknown tool `{name}`")),
        }
    }
}

/// Standard command runner. NOTE: not sandboxed by allowed_paths — gated by human approval.
pub struct BashExecutor;

#[async_trait]
impl BaseTool for BashExecutor {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "execute_bash",
            description: "Execute a single shell command and return stdout/stderr.",
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The shell command to run" }
                },
                "required": ["command"]
            }),
        }
    }

    async fn execute(&self, args: &Value) -> Result<String> {
        let cmd = args["command"]
            .as_str()
            .ok_or_else(|| anyhow!("missing `command` argument"))?;

        let output = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .output()
            .await?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        Ok(format!(
            "exit: {}\nSTDOUT:\n{stdout}\nSTDERR:\n{stderr}",
            output.status.code().unwrap_or(-1)
        ))
    }
}
```

## 4. Tool Schema & Model Compatibility (`src/schema.rs`)

Cloud models (and newer local models) support **native tool calling**: the tool specs are sent as an OpenAI `tools` array and the model replies with structured `tool_calls`. Many local Ollama models do not, so the client operates in one of three modes set by `agent.tool_calling`:

* `native` — send the `tools` array; parse `response.tool_calls`.
* `react` — omit `tools`; inject the tool catalog into the system prompt and parse a fenced action block from the text.
* `auto` — attempt `native`; if the endpoint rejects `tools` or never emits a structured call, fall back to `react` for the session.

**Native schema export** turns each `ToolSpec` into an OpenAI tool object:

```rust
pub fn to_openai_tools(specs: &[ToolSpec]) -> serde_json::Value {
    serde_json::Value::Array(
        specs.iter().map(|s| serde_json::json!({
            "type": "function",
            "function": {
                "name": s.name,
                "description": s.description,
                "parameters": s.parameters,
            }
        })).collect(),
    )
}
```

**ReAct fallback** advertises tools in the system prompt and expects the model to emit a single JSON action, which the client parses back into a `ToolCall`:

````text
You have these tools: {tool_catalog}. To use one, reply with ONLY:
```action
{"tool": "<name>", "args": { ... }}
```
When the task is complete, reply with a normal message and no action block.
````

The parser extracts the fenced `action` block, deserializes it, and synthesizes a `ToolCall` (generating a local `id`). If no block is present, the text is treated as the final answer — the same exit condition as an empty `tool_calls` array in native mode.

> **Qwen3 / vLLM note.** When vLLM serves a model with a tool-call parser (e.g. `--tool-call-parser qwen3_xml` for Qwen3.6), it parses the model's provider-specific format (Qwen emits XML-style tool calls) **server-side** and returns standard OpenAI `tool_calls` over the API. The agent therefore needs **no XML parsing** — the `native` path handles these models directly, and `react` is reserved only for endpoints that expose no tool-calling parser at all.

## 5. Context & Token Management (`src/context.rs`)

`AgentContext` owns the running message history plus the system prompt and tool specs. Because long agent sessions overflow the model's context window, it enforces a budget after every step:

* **Turn window** — keep at most `max_history_turns` user/assistant/tool exchanges, always preserving the system prompt and the original user instruction.
* **Token estimate** — approximate token count (chars/4 heuristic, or a real tokenizer later) and evict the oldest non-pinned turns until the request fits `context_window − max_tokens`.
* **Tool-output truncation** — cap large stdout/stderr payloads (head+tail with an elision marker) before they enter history.

```rust
impl AgentContext {
    /// Trim history to satisfy both the turn window and the token budget.
    pub fn enforce_budget(&mut self) {
        while self.turn_count() > self.config.agent.max_history_turns
            || self.estimated_tokens() > self.token_ceiling()
        {
            if !self.evict_oldest_unpinned() {
                break; // nothing left to evict but the pinned system + first user turn
            }
        }
    }
}
```

## Error Handling & Observability

Both are first-class concerns for an *auditable* agent — an operator should be able to see exactly what the agent did and why any step failed.

### Errors — `snafu`

* Each module defines a typed `Error` enum via `#[derive(Snafu)]` with **context selectors** (e.g. `ReadSnafu`, `SpawnSnafu`, `PathNotAllowedSnafu`), so every failure carries structured, greppable context rather than an opaque string.
* Modules expose a local `pub type Result<T, E = Error>` alias.
* The binary boundary (`main`/`run`) uses `snafu::Whatever` with `.whatever_context("…")` for ergonomic top-level propagation, keeping typed errors where structure matters and a catch-all only at the edge.
* **Tool** failures are *non-fatal*: `ToolRegistry::execute` returns a typed error that the loop converts into an `Error: …` string fed back to the model, so the agent can recover instead of aborting.

### Observability — `tracing`

* `tracing` spans/events instrument the loop (`step`), the client (`generate_completion`, with the model in the span), and every tool (`execute`).
* Logs go to **stderr** so **stdout** stays clean for the final answer.
* Verbosity via `MIMA_LOG` (an `EnvFilter`, e.g. `MIMA_LOG=debug`).
* Format via `MIMA_LOG_FORMAT`: **human-readable by default**, `json` for structured, machine-readable audit logs (well-suited to headless/edge operation on the Thor).

## Build & Target: The Single Auditable Binary

The defining product property is a small, statically-linked, single-file binary that runs on the dev host and cross-compiles to the Jetson Thor.

* **Dev host:** `cargo build --release` → `target/release/mima`.
* **Jetson Thor (aarch64):** target `aarch64-unknown-linux-gnu`. For a fully static, libc-independent artifact, `aarch64-unknown-linux-musl` is preferred where the dependency set allows it.

```bash
# Cross-compile for the Thor (example; a container or cross toolchain provides the linker)
rustup target add aarch64-unknown-linux-gnu
cargo build --release --target aarch64-unknown-linux-gnu
```

**Auditability measures baked into the build:**

* **rustls, not OpenSSL** — `reqwest` uses `rustls-tls` with `default-features = false`, avoiding a system OpenSSL dependency and shrinking the audited surface.
* **Size-optimized release profile** (`Cargo.toml`): `opt-level = "z"`, `lto = true`, `codegen-units = 1`, `strip = true`, `panic = "abort"`.
* **No hidden egress** — the only network call is to the configured `base_url`; there is no telemetry, analytics, or update check.
* **Small dependency tree** kept intentionally minimal so the whole thing can be reviewed.

## 6. Resolved Issues (v0.1.0 → v0.2.0)

| # | Issue in v0.1.0 | Resolution |
|---|-----------------|------------|
| 1 | `let user_prompt = &args;` bound the whole `Vec` | `args.skip(1).join(" ")` builds the instruction string |
| 2 | Tool `Result` passed as `&result`, `Err` dropped, type mismatch | `match` on the `Result` into an `Ok`/`Error: …` string for the model |
| 3 | `print!` prompt not flushed before `read_line` | `io::stdout().flush()?` before reading stdin |
| 4 | `ToolRegistry`/`init_default`/dispatch undefined | Defined with `HashMap` storage, `specs()`, and name dispatch |
| 5 | No tool-schema plumbing to the API | `schema.rs` exports OpenAI `tools`; response parsing defined |
| 6 | Local models without native tool calls unsupported | `react` mode + `auto` probe-and-fallback |
| 7 | `max_history_turns` unused; unbounded context | `enforce_budget()` sliding window + token estimate |
| 8 | `allowed_paths` implied shell sandboxing (false) | Scoped to `fs.rs` tools; shell relies on approval gate |
| 9 | File writes bypassed approval | `require_approval_for_writes` gates `write_file`/`edit_file` |
| 10 | `api_key` in plaintext TOML | `${ENV_VAR}` expansion at config load |

## 7. Open Questions / Next Steps

* Add an MCP client (local stdio servers only) to plug in domain tools (man and kernel docs, distro package queries, build and debug helpers), keeping plugins optional and auditable per the Domain Requirements.
* Author Linux-focused skills and lessons and a system-prompt library (kernel patch submission checklist, distro playbooks) to deliver R1–R6.
* Make `agent.toml` optional (convention over configuration): run with built-in defaults, discover a local endpoint (for example `http://localhost:11434/v1`), and allow `MIMA_BASE_URL` / `MIMA_MODEL` overrides so the common path needs no config file.
* Product name chosen: **`minister_mandati`** (binary **`mima`**); `agent.toml` retained as the config filename.
* Persistent shell state across `execute_bash` calls (`cd`, env, venv) — deferred; each call is currently independent.
* Diff preview for `edit_file` before applying, integrated with the approval gate.
* Real tokenizer instead of the chars/4 estimate.
