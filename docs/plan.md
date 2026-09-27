# Implementation Plan (Phased Roadmap)

Brief, phased roadmap. The detailed technical design lives in
[../rust_terminal_agent_plan.md](../rust_terminal_agent_plan.md). This roadmap
reflects the completed Phase 0 research.

> Research notes (papers, concept notes) are kept in a separate private
> repository and are not published with this project.

## Phase 0 — Research & Direction — COMPLETE

Deep research done. Full findings are in the private research notes:
`agents-core-concepts.md`, `agent-survey.md`, `claude-code-leak.md`,
`claude-code-southbridge-analysis.md`, `mooc-findings.md`,
`advanced-mooc-findings.md`. Consolidated design decisions live in
[../rust_terminal_agent_plan.md](../rust_terminal_agent_plan.md)
(§ Phase 0 Findings → Design Decisions).

Headline conclusions:

- Keep the flat ReAct loop; a bounded **max-1-branch** squad only when needed.
- **Evaluation is a first-class subsystem** (verifier vector, pass@k, paired
  error bars) — the central lesson of both Berkeley MOOCs.
- **Verifier-gated self-refine** (never oracle-free self-correction on-device).
- A defensive **security / vuln-detection** capability is our strongest, and
  cheapest-to-verify, differentiator (a sanitizer crash is ground truth).
- Least-privilege safety (scoped policy, taint-tracking, arg sanitization).

## Phase 1 — Core loop & Convention over Configuration

- Config optional (CoC): zero-config path, local endpoint discovery
  (`http://localhost:11434/v1`), `MIMA_BASE_URL` / `MIMA_MODEL` overrides.
- Live smoke-test against the Thor's vLLM end-to-end on a real Linux task.
- Streaming (`agent.stream`), Ctrl-C cancellation, API retry/backoff.
- `Presenter` abstraction (event-driven) — plain CLI default.
- **Context Revision** loop step (compaction + JIT retrieval); real tokenizer.
- **Verifier-gated self-refine** rule wired into the loop.

## Phase 2 — Evaluation subsystem (first-class, built early)

- Eval harness: run tasks in a sandbox; **verifier vector** (compile · test ·
  sanitizer · lint · end-state · cost · steps · format).
- **pass@1 / pass@k** over N seeds; **paired error bars** (SEM + z-score) and a
  stated minimum detectable effect size.
- Seed a small Linux/kernel task set; an **indirect prompt-injection red-team**
  track (AgentXploit-style, attack-success-rate → 0). Harness-swappable
  (production == test).

## Phase 3 — Tooling & safety upgrades

- **Edit tools:** SEARCH/REPLACE `edit_file` + `multi_edit` (read-before-edit,
  mtime check, `expected_replacements`, diff preview); dedicated ripgrep-backed
  `grep`/`glob`; model-managed **TODO** tool.
- **Execution:** read-only tools parallel, write tools serial.
- **Safety:** scoped least-privilege policy (`Tool(glob)`), command-injection
  prefix detection, **taint-tracking** of untrusted content, and **sanitize all
  LLM-produced tool args/code** (block SQLi/RCE/SSRF).

## Phase 4 — Linux domain expertise (R1–R6) + security track

- Linux/kernel skills + system-prompt library (patch-submission checklist,
  distro playbooks); `checkpatch.pl` / `get_maintainer.pl` wrappers (gated).
- **Security / vuln-detection track (defensive):** a "Big Sleep" **dual-use tool
  triad** — code-index browser (jump-to-def/xref), sandboxed input-generator,
  debugger (gdb/lldb + watchpoints) with ASan/valgrind — which also satisfies the
  required C/Rust/Python debugging tools. Verified by the security eval track.

## Phase 5 — Extensibility (MCP + presentation)

- MCP client (local stdio only); reference plugin: man-page / `tldr`; follow-ons:
  distro package queries, build/debug helpers.
- Optional **ACP** server (Zed/JetBrains) alongside MCP.
- Pluggable presentation behind `Presenter`: TUI (`ratatui`/`cursive`) and
  optional local REST (`axum`), gated as features.

## Phase 6 — Packaging & audit

- `aarch64` static build for the Thor; verify size/dependency surface.
- **cargo-deny** + SBOM; expand tests; run the eval suite.
- Model-weight supply chain: prefer safetensors, checksum/verify downloads.

## Phase 7 — Naming & release

- Product name chosen: `minister_mandati` (binary `mima`); `agent.toml` retained as the config filename. Remaining: tag releases.

## Near-term backlog (start of Phase 1)

1. Config optional (CoC) — zero-config path + local endpoint discovery (High).
2. Live smoke-test against the Thor's vLLM (High).
3. Stand up the **evaluation harness** skeleton (verifier vector + pass@k) (High).
4. `Presenter` abstraction + Context Revision step + real tokenizer (High).
5. SEARCH/REPLACE edit tools + ripgrep grep/glob + TODO tool (Medium).
6. Least-privilege approval policy + tool-arg sanitization (Medium).
