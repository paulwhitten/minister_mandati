# Implementation Plan (Phased Roadmap)

Phased roadmap with the current status of each item. The high-level design
is [architecture.md](architecture.md); the technical specification lives in
[../rust_terminal_agent_plan.md](../rust_terminal_agent_plan.md);
feature documents and design proposals live in this folder and in
[design/](design/). The ordered work queue is [Next up](#next-up).

> Research notes (papers, concept notes) are kept in a separate private
> repository and are not published with this project.

Status values used below:

- Done: implemented, tested, and documented
- Partial: implemented in part; the remaining work is named in the row
- Planned: accepted, not started
- Proposal: design written, not yet accepted for implementation

Last reviewed: 2026-10-25.

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
| Progress guard: stall counter, redundant-read notes, status line in tool results (replaces the identical-call guard) | Partial | branch `dev/progress_guard`; paired comparison on Qwen 3.6 showed no significant difference |
| Composable system prompt (prefix, body, suffix; full override) | Done | [design/design-composable-system-prompt.md](design/design-composable-system-prompt.md) |
| Context budget from the model window; output masking; step eviction; overflow recovery | Done | [context.md](context.md) |
| Exact token counts via the server's `/tokenize`, calibrated estimate as fallback | Done | [context.md](context.md) |
| Sessions, JSONL transcripts, resume | Done | [sessions.md](sessions.md), [design/session-resume.md](design/session-resume.md) |
| Cloud endpoints (Azure OpenAI) | Done | [cloud-providers.md](cloud-providers.md) |
| Summarizing compaction: a compaction record (code-built digest plus optional model summary) in place of evicted steps; `/compact [focus]` | Partial (digest record done; summary and `/compact` proposed) | [design/design-context-management.md](design/design-context-management.md) |
| `/context-status`: context use by category, limits, thresholds, compaction history | Done | [design/design-context-reporting.md](design/design-context-reporting.md) |
| `recall_output` tool: fetch a masked tool output back by call ID from the transcript | Planned | |
| Project instructions: `AGENTS.md` from the repository root to the working directory plus a global file, cleaned, capped, labeled, recorded; workspace `agent.toml` can no longer set the system prompt | Done (paired A/B pending) | [design/design-project-instructions.md](design/design-project-instructions.md) |
| Verifier-gated self-refine wired into the loop: completion gate on the project's tests (new failures only, max 3 rounds, `UNVERIFIED:` exit), syntax guard, end check (workspace facts at an answer), reserved repair steps, time limit and budget notice; `[verify].enabled`, off by default | Done | on main since 8fc224d; [design/design-verifier-gate.md](design/design-verifier-gate.md) |
| Self-refine on by default, once a paired comparison shows a gain (no measurable effect so far for the completion gate on the base tasks: Qwen 3.6, gpt-4o-mini, Nano 30B, Nano 4B; end check and budget not yet measured) | Planned | [design/design-verifier-gate.md](design/design-verifier-gate.md#results) |

Exit criteria:

- Self-refine improves the paired pass rate of the two weakest baseline models
  on the `hard` and `mutations` suites, with no task group regressing beyond
  the minimum detectable effect.
- The 8k-window compaction regression task passes without a loop-guard
  termination. Met so far in the 2026-10-24 baseline (mima 2eb1364, digest
  record): 7 of 7 trials passed (lightning30b 3, Nano 4B 3, Nano 30B 1), none
  ended by the loop guard; the 4a61467 baseline had two loop-guard endings
  there. Confirm when the remaining Nano 30B and Qwen trials finish.

## Phase 2: Evaluation subsystem (Partial)

| Item | Status | Reference |
|---|---|---|
| `mima-eval` harness: per-trial isolation, bubblewrap sandbox, hidden checks, oracle and cheat validation | Done | [eval.md](eval.md) |
| Verifier vector: build, tests, sanitizer (ASan/UBSan checks), end state, cost, steps, format | Done | [eval.md](eval.md#tasks) |
| Statistics: pass@k, pass^k, clustered standard error, paired differences, minimum detectable effect | Done | [eval.md](eval.md#statistics) |
| Task quality tools: `validate`, `strength`, `probe`, `calibrate` | Done | [eval.md](eval.md#task-quality) |
| Task set: 6 seed tasks here; 126 tasks in a separate repository (106 base tasks plus 20 pro tasks) | Done | [../evals/README.md](../evals/README.md) |
| Baseline: full task set, 6 models, 1 trial | Done | |
| Rerun the baseline with the current `mima` (behavior changed after the baseline) | Partial | mima 2eb1364, base tasks x3 on the AGX: lightning30b 87%, Nano 4B 48% done, Nano 30B running, Qwen 3.6 and Qwen 3.8-27B x1 queued; Qwen 3.6 pro tasks x3 on the IGX done |
| Paired comparison of two runs (same models, different `mima` builds) | Done | `mima-eval compare`; [eval.md](eval.md#results) |
| Harder tasks for models that saturate the current set | Partial | 20 pro tasks (Go, Rust, Python, JavaScript; MIT-licensed upstream changes); Qwen 3.6 passes about half; more for Qwen 3.8-27B if it saturates them too |
| Linux and kernel task track (systemd, packaging, kbuild, out-of-tree modules) | Planned | [requirements.md](requirements.md) |
| Indirect prompt-injection red-team track (attack success rate toward 0) | Planned | |
| Vulnerability-detection track (sanitizer oracle) | Planned | Phase 4 |

Exit criteria:

- A current baseline with at least 3 trials per task exists for every model
  profile in use. A suite the model saturates (above 90%) needs only 1
  trial, as a regression check; the 3 trials go to a suite where it does not
  saturate. Profiles in use: lightning30b, Nano 4B and Nano 30B (base tasks
  x3), Qwen 3.6 (base tasks x1, pro tasks x3: done), Qwen 3.8-27B (base
  tasks x1; pro tasks x3 still needed). Cloud profiles are used for manual
  testing only and are not counted.
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
| Sandbox S3: test matrix on the Thor (Landlock ABI 4) | Done (passes on the AGX and IGX Thor; CUDA programs need the opt-in `[security].sandbox_gpu`; known gaps: signals not scoped below ABI 6, datagrams to local daemons through `sendmsg`) | [design/design-shell-sandboxing.md](design/design-shell-sandboxing.md#s3-results-on-the-thor) |
| Sandbox Tiers 2 and 3 (bubblewrap, systemd scope limits) | Planned | [design/design-shell-sandboxing.md](design/design-shell-sandboxing.md) |
| macOS shell sandbox via Seatbelt (`sandbox-exec`), phases M0-M5: profile generator with base rules from measured denials, `probe()`/`wrap()`, macOS itests, CI job | Partial | [design/design-shell-sandboxing.md](design/design-shell-sandboxing.md#macos-seatbelt) |
| Scoped least-privilege approval rules (`Tool(glob)`, per-session "always") | Planned | |
| Tool vocabularies: advertise each model's trained tool names and shapes (OpenHands, Codex, OpenCode, Claude Code, mima) on shared core operations; `auto` selection by model id; measured per vocabulary | Proposal | [design/design-tool-vocabularies.md](design/design-tool-vocabularies.md) |
| Taint tracking of untrusted content (files, repositories, web, MCP output) | Planned | |
| Sanitizing model-produced tool arguments | Planned | |
| Web search and fetch tools (off by default) | Proposal | [design/design-web-tools.md](design/design-web-tools.md) |

Ordering constraint: the red-team track (Phase 2) and taint tracking come
before web tools or MCP, because both add untrusted content and an outbound
channel.

Exit criteria:

- The red-team attack success rate is reported per model and does not rise
  when web tools are enabled.
- Tier 1 is verified on the Thor. Met on 2026-10-26: the matrix passed on the
  AGX and IGX Thor (CUDA with `sandbox_gpu`), with the gaps of Landlock ABI 4
  documented in the sandbox design.

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
| CI: format, clippy with warnings denied, tests, release build; Linux and macOS | Done | |
| Transcript JSON Schema with compatibility tests | Done | [design/session-resume.md](design/session-resume.md) |
| Audit record: hash chain, session ledger, attribution (actor and rule ids), complete header, evidence hashes, required mode, export, `--audit-verify`, eval integration | Done | [design/design-audit-record.md](design/design-audit-record.md) |
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

1. Finish the baseline rerun (mima 2eb1364, AGX): Nano 30B x3, then Qwen 3.6
   and Qwen 3.8-27B x1 on the base tasks, each compared with the 4a61467
   baseline; then Qwen 3.8-27B on the pro tasks x3. Every item below is
   measured against it.
2. Self-refine end check and budget: a paired A/B on Nano 4B and Nano 30B
   over `hard` and `mutations` (the Phase 1 exit criterion), and the Qwen 3.6
   pro-task run against its 48K-reply control. If neither shows a gain,
   revise the criterion rather than chase it. See
   [design/design-verifier-gate.md](design/design-verifier-gate.md).
3. Sandbox S2 remainder: escalation argument, trusted-source-only widening,
   `.git/hooks` integrity check, `mima sandbox --status`.
4. Prompt-injection red-team track, then taint tracking and tool-argument
   sanitization.
5. The TODO tool; a paired A/B for project instructions using hand-written
   AGENTS.md files on a task subset.
6. Summarizing compaction: the model summary and `/compact`, then
   `recall_output` (the digest record and `/context-status` are done).
   See [design/design-context-management.md](design/design-context-management.md).
7. Linux and kernel eval tasks; more hard tasks if Qwen 3.8-27B saturates
   the pro tasks.
8. macOS: M4 (CI green on macOS) and M5 before macOS eval results are
   compared with Linux.
9. Parallel read-only tools and `/undo`.
