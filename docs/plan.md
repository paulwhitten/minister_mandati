# Implementation Plan (Phased Roadmap)

Phased roadmap with the current status of each item. The technical design
lives in [../rust_terminal_agent_plan.md](../rust_terminal_agent_plan.md);
feature documents and design proposals live in this folder and in
[design/](design/). The ordered work queue is [Next up](#next-up).

> Research notes (papers, concept notes) are kept in a separate private
> repository and are not published with this project.

Status values used below:

- Done: implemented, tested, and documented
- Partial: implemented in part; the remaining work is named in the row
- Planned: accepted, not started
- Proposal: design written, not yet accepted for implementation

Last reviewed: 2026-10-13.

## How progress is measured

Evaluation is the yardstick for every change to agent behavior (loop, prompts,
tools, context handling):

- Every behavior change ships with a paired comparison against the latest
  baseline on the affected suite groups, reporting the difference, its
  confidence interval, and the minimum detectable effect (see
  [eval.md](eval.md#statistics)).
- A regression larger than the minimum detectable effect on any group blocks
  the change until it is explained or fixed.
- Suites that a model saturates (pass rate above 90%) no longer discriminate
  for that model; harder tasks are added before claiming improvements there.
- Each phase below closes only when its exit criteria hold.

## Phase 0: Research and direction (Done)

Full findings are in the private research notes: `agents-core-concepts.md`,
`agent-survey.md`, `claude-code-leak.md`,
`claude-code-southbridge-analysis.md`, `mooc-findings.md`,
`advanced-mooc-findings.md`. Consolidated design decisions live in
[../rust_terminal_agent_plan.md](../rust_terminal_agent_plan.md)
(section "Phase 0 Findings").

Headline conclusions:

- Keep the flat ReAct loop; a bounded **max-1-branch** squad only when needed.
- **Evaluation is a first-class subsystem** (verifier vector, pass@k, paired
  error bars), the central lesson of both Berkeley MOOCs.
- **Verifier-gated self-refine** (never oracle-free self-correction on-device).
- A defensive **security / vuln-detection** capability is our strongest, and
  cheapest-to-verify, differentiator (a sanitizer crash is ground truth).
- Least-privilege safety (scoped policy, taint-tracking, arg sanitization).

## Phase 1: Core loop and convention over configuration (Partial)

| Item | Status | Reference |
|---|---|---|
| Optional config: zero-config path, local endpoint discovery, `MIMA_*` overrides | Done | [../README.md](../README.md) |
| Live smoke test against vLLM on the Thor (native tool calling, streaming, token ledger) | Done | |
| Streaming, Ctrl-C cancellation with rollback, retry with backoff | Done | |
| `Presenter` abstraction with the plain CLI as default | Done | |
| One-shot and REPL modes, slash commands, in-memory line editing | Done | [../README.md](../README.md) |
| Loop guards: duplicate-write skip, no-progress window, continue on empty or truncated replies | Done | [design/design-loop-guards.md](design/design-loop-guards.md) |
| Composable system prompt (prefix, body, suffix; full override) | Done | [design/design-composable-system-prompt.md](design/design-composable-system-prompt.md) |
| Context budget from the model window; output masking; step eviction; overflow recovery | Done | [context.md](context.md) |
| Exact token counts via the server's `/tokenize`, calibrated estimate as fallback | Done | [context.md](context.md) |
| Sessions, JSONL transcripts, resume | Done | [sessions.md](sessions.md), [design/session-resume.md](design/session-resume.md) |
| Cloud endpoints (Azure OpenAI) | Done | [cloud-providers.md](cloud-providers.md) |
| Summarizing compaction: a short progress note when steps are evicted | Planned | |
| `recall_output` tool: fetch a masked tool output back by call ID from the transcript | Planned | |
| Project instructions file loaded into the system prompt (nearest file up to the repository root) | Planned | |
| Verifier-gated self-refine wired into the loop | Planned | |

Exit criteria:

- Self-refine improves the paired pass rate of the two weakest baseline models
  on the `hard` and `mutations` suites, with no task group regressing beyond
  the minimum detectable effect.
- The 8k-window compaction regression task passes without a loop-guard
  termination.

## Phase 2: Evaluation subsystem (Partial)

| Item | Status | Reference |
|---|---|---|
| `mima-eval` harness: per-trial isolation, bubblewrap sandbox, hidden checks, oracle and cheat validation | Done | [eval.md](eval.md) |
| Verifier vector: build, tests, sanitizer (ASan/UBSan checks), end state, cost, steps, format | Done | [eval.md](eval.md#tasks) |
| Statistics: pass@k, pass^k, clustered standard error, paired differences, minimum detectable effect | Done | [eval.md](eval.md#statistics) |
| Task quality tools: `validate`, `strength`, `probe`, `calibrate` | Done | [eval.md](eval.md#task-quality) |
| Task set: 6 seed tasks here; 106 tasks in a separate repository | Done | [../evals/README.md](../evals/README.md) |
| Baseline: full task set, 6 models, 1 trial | Done | |
| Rerun the baseline with the current `mima` (behavior changed after the baseline) | Planned | |
| Paired comparison of two runs (same models, different `mima` builds) | Planned | |
| Harder tasks for models that saturate the current set | Planned | |
| Linux and kernel task track (systemd, packaging, kbuild, out-of-tree modules) | Planned | [requirements.md](requirements.md) |
| Indirect prompt-injection red-team track (attack success rate toward 0) | Planned | |
| Vulnerability-detection track (sanitizer oracle) | Planned | Phase 4 |

Exit criteria:

- A current baseline with at least 3 trials per task exists for every model
  profile in use.
- The red-team track runs in the same harness and reports an attack success
  rate per model.
- The strongest model no longer saturates the `all` suite.

## Phase 3: Tooling and safety (Partial)

Tools:

| Item | Status | Reference |
|---|---|---|
| `edit_file`: exact match, tolerance ladder, stale-file protection, diff preview, atomic writes | Done | [editing.md](editing.md) |
| `read_file` paging with numbered lines; `write_file` overwrite requires a prior read | Done | [editing.md](editing.md) |
| `find_files` (glob) and `search_files` (regex), sandboxed and read-only | Done | |
| `multi_edit` | Planned | add only if edit failure rates show a need |
| Model-managed TODO tool | Planned | |
| Read-only tool calls in parallel, write tools serial | Planned | |
| `/undo`: snapshot before each approved write | Planned | |

Safety:

| Item | Status | Reference |
|---|---|---|
| Approval gate: explicit yes/no, re-ask, escaped command and diff preview | Done | |
| `auto_approve_bash` allowlist (whole-word prefix; shell metacharacters always prompt) | Done | |
| Shell sandbox Tier 0: environment allowlist, process-group kill, private temp directory | Done | [design/design-shell-sandboxing.md](design/design-shell-sandboxing.md) |
| Shell sandbox Tier 1: Landlock, seccomp, rlimits (Linux only) | Done | [design/design-shell-sandboxing.md](design/design-shell-sandboxing.md) |
| Sandbox S2 remainder: escalation argument, trusted-source-only widening, `.git/hooks` integrity check, `mima sandbox --status` | Partial | [design/design-shell-sandboxing.md](design/design-shell-sandboxing.md) |
| Sandbox S3: test matrix on the Thor (Landlock ABI 4) | Planned | [design/design-shell-sandboxing.md](design/design-shell-sandboxing.md) |
| Sandbox Tiers 2 and 3 (bubblewrap, systemd scope limits) | Planned | [design/design-shell-sandboxing.md](design/design-shell-sandboxing.md) |
| macOS shell sandbox via Seatbelt (`sandbox-exec`), phases M0-M5 | Proposal | [design/design-shell-sandboxing.md](design/design-shell-sandboxing.md#macos-seatbelt) |
| Scoped least-privilege approval rules (`Tool(glob)`, per-session "always") | Planned | |
| Taint tracking of untrusted content (files, repositories, web, MCP output) | Planned | |
| Sanitizing model-produced tool arguments | Planned | |
| Web search and fetch tools (off by default) | Proposal | [design/design-web-tools.md](design/design-web-tools.md) |

Ordering constraint: the red-team track (Phase 2) and taint tracking come
before web tools or MCP, because both add untrusted content and an outbound
channel.

Exit criteria:

- The red-team attack success rate is reported per model and does not rise
  when web tools are enabled.
- Tier 1 is verified on the Thor.

## Phase 4: Linux domain expertise and security track (Planned)

Requirements: [requirements.md](requirements.md) (R1-R6).

- Linux/kernel skills + system-prompt library (patch-submission checklist,
  distro playbooks); `checkpatch.pl` / `get_maintainer.pl` wrappers (gated).
  Skills ship as plain files loaded into the prompt, so they do not wait for
  MCP.
- **Security / vuln-detection track (defensive):** a "Big Sleep" **dual-use tool
  triad**: code-index browser (jump-to-def/xref), sandboxed input-generator,
  debugger (gdb/lldb + watchpoints) with ASan/valgrind. It also satisfies the
  required C/Rust/Python debugging tools. Verified by the security eval track.
- First slice: a gated `checkpatch.pl` wrapper and a gdb/ASan tool, each
  measured on existing C tasks before the full triad.

Exit criteria:

- The Linux and kernel track passes at or above the general task set's rate
  for the same model.

## Phase 5: Extensibility, MCP and presentation (Planned)

- MCP client (local stdio only); reference plugin: man-page / `tldr`; follow-ons:
  distro package queries, build/debug helpers.
- Optional **ACP** server (Zed/JetBrains) alongside MCP.
- Pluggable presentation behind `Presenter`: TUI (`ratatui`/`cursive`) and
  optional local REST (`axum`), gated as features.

## Phase 6: Packaging and audit (Partial)

| Item | Status | Reference |
|---|---|---|
| Static x86_64 build (`crt-static`, static-pie) | Done | [../README.md](../README.md) |
| macOS build (Landlock and seccomp compiled on Linux only) | Done | |
| CI: format, clippy with warnings denied, tests | Done | |
| Transcript JSON Schema with compatibility tests | Done | [design/session-resume.md](design/session-resume.md) |
| Static `aarch64` build verified on the Thor; size and dependency surface | Planned | |
| `cargo-deny` and SBOM | Planned | |
| Model-weight supply chain: prefer safetensors, verify checksums | Planned | |

## Phase 7: Naming and release (Partial)

| Item | Status | Reference |
|---|---|---|
| Name: `minister_mandati` (binary `mima`); `agent.toml` kept as the config filename | Done | |
| README status section matches this plan | Planned | [../README.md](../README.md#status) |
| Tag releases; refresh `schemas/released/` at each release | Planned | |

## Next up

Ordered by dependency, then impact:

1. macOS shell sandbox (Seatbelt), phases M0-M2, before any further eval
   work on macOS; M5 before macOS eval results are compared with Linux.
2. Rerun the baseline with the current `mima` (3 trials) and add paired
   run-to-run comparison to `mima-eval`. Every item below is measured
   against it.
3. Verifier-gated self-refine: detect the project's build and test command,
   run it after edits, feed failures back, and require a passing check (or a
   stated reason) before the final answer.
4. Project instructions file and the TODO tool.
5. Summarizing compaction and `recall_output`, measured on the 8k-window
   regression task.
6. Prompt-injection red-team track, then taint tracking and tool-argument
   sanitization.
7. Linux and kernel eval tasks, plus harder tasks for saturated models.
8. Sandbox S2 remainder and S3 on the Thor.
9. Parallel read-only tools and `/undo`.
