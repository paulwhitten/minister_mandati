# Requirements

Formal domain-competency requirements for the agent.

## Conformance language

The key words **MUST**, **MUST NOT**, **REQUIRED**, **SHALL**, **SHALL NOT**,
**SHOULD**, **SHOULD NOT**, **RECOMMENDED**, **MAY**, and **OPTIONAL** in this
document are to be interpreted as described in RFC 2119.

## Scope

This document specifies the Linux, command, and programming-language competency
requirements. Cross-cutting product requirements (auditability, BYOK, approval
gating, convention over configuration, observability) are specified in
[../rust_terminal_agent_plan.md](../rust_terminal_agent_plan.md). Requirement IDs
below trace to R1–R6 in that design spec.

## Linux and Unix (LNX)

- **REQ-LNX-1** The agent SHALL treat Linux and Unix as the primary target
  environment.
- **REQ-LNX-2** The agent SHALL demonstrate expert knowledge of common shells
  (bash, zsh, sh; fish where applicable), including scripting, quoting,
  redirection, job control, and exit-status handling.
- **REQ-LNX-3** The agent SHALL be proficient with coreutils and everyday
  command-line tooling.
- **REQ-LNX-4** The agent SHALL be proficient in service and system management,
  including systemd units, journald, cron and timers, processes, and signals.
- **REQ-LNX-5** The agent SHALL be proficient in file, permission, user,
  networking, and storage management.

## Commands and pipelines (CMD)

- **REQ-CMD-1** The agent SHALL construct correct, efficient command pipelines for
  text processing (for example grep, sed, awk, cut, sort, and jq).
- **REQ-CMD-2** The agent SHALL prefer idiomatic, portable, POSIX-aware
  constructs and SHOULD note distribution- or shell-specific behavior when
  relevant.
- **REQ-CMD-3** The agent MUST request human approval before executing
  state-changing or destructive shell commands.
- **REQ-CMD-4** The agent SHOULD explain non-obvious commands and their impact
  before or alongside execution.

## Distributions and administration (DST)

- **REQ-DST-1** The agent SHALL demonstrate mastery of major distributions
  (Debian and Ubuntu, Fedora and RHEL, Arch, SUSE).
- **REQ-DST-2** The agent SHALL be proficient with their package managers (apt,
  dnf, pacman, zypper) and with file-to-package mapping.
- **REQ-DST-3** The agent SHALL be proficient in common administration tasks,
  including init and systemd, users and permissions, networking, storage, and
  containers.

## Kernel and modules (KRN)

- **REQ-KRN-1** The agent SHALL demonstrate deep knowledge of the Linux kernel
  and loadable kernel modules.
- **REQ-KRN-2** The agent SHALL be proficient with the kbuild and Kconfig build
  system and with out-of-tree module builds.
- **REQ-KRN-3** The agent SHALL support the kernel contribution workflow,
  including checkpatch.pl, get_maintainer.pl, and git format-patch and
  git send-email.
- **REQ-KRN-4** The agent SHOULD be suitable for kernel, module, and Linux
  Foundation project work.

## Programming languages (LNG)

- **REQ-LNG-1** The agent SHALL produce exceptional, idiomatic C, including
  correct memory, pointer, and undefined-behavior awareness.
- **REQ-LNG-2** The agent SHALL produce exceptional, idiomatic Rust, including
  ownership, error handling, and async where appropriate.
- **REQ-LNG-3** The agent SHALL produce exceptional, idiomatic Python, including
  packaging and virtual environments.
- **REQ-LNG-4** The agent SHALL be proficient with the relevant build systems
  (make, CMake, kbuild, cargo, pip, venv, poetry).
- **REQ-LNG-5** The agent SHALL be proficient with debugging and profiling tools
  (gdb, lldb, valgrind, perf, strace, ltrace).

## Excellence and verification (EXC)

- **REQ-EXC-1** For Linux systems and kernel work, the agent SHOULD aim to be
  best-in-class relative to comparable agents.
- **REQ-EXC-2** Domain competency SHOULD be delivered first via a Linux-focused
  system prompt and skills and lessons, and MAY be augmented by optional, local,
  auditable MCP plugins.
- **REQ-EXC-3** These requirements SHOULD be validated by a domain evaluation
  suite, added in a later phase.

## Observability and cost (OBS)

- **REQ-OBS-1** The agent SHALL account for token usage on every model request,
  recording prompt, completion, and total tokens.
- **REQ-OBS-2** The agent SHALL treat server-reported usage (the OpenAI-compatible
  `usage` object) as authoritative, and MUST record when a response omits usage so
  cumulative counts remain trustworthy rather than silently undercounting.
- **REQ-OBS-3** The agent SHALL maintain cumulative per-task token totals across
  all steps and tool turns, and SHALL emit them through the observability layer at
  task completion.
- **REQ-OBS-4** When streaming responses, the agent SHALL request usage accounting
  (for example `stream_options.include_usage`) so totals stay reliable.
- **REQ-OBS-5** Token accounting SHOULD be exposed as typed events to presentation
  layers (per REQ-UI-4) and MAY be reported alongside step count and wall-clock
  time for cost and efficiency analysis.
- **REQ-OBS-6** When server usage is unavailable, the agent MAY estimate token
  counts with a real tokenizer, but SHALL mark such counts as estimated.

## Presentation and interfaces (UI)

- **REQ-UI-1** The agent core SHALL be decoupled from presentation; core logic
  (loop, client, tools, context) MUST NOT depend on any specific rendering or I/O
  framework and MUST NOT write to the terminal directly.
- **REQ-UI-2** The agent SHALL be CLI-first: a plain terminal interface is the
  default and MUST work with only the core dependencies.
- **REQ-UI-3** The architecture SHALL support pluggable presentation layers (for
  example a plain CLI, a rich TUI, or a REST/HTTP interface) behind a stable
  interface, selectable without changing core logic.
- **REQ-UI-4** The core SHALL communicate with presentation layers via typed
  events and inputs (for example: task started, model output, tool-call
  requested, tool output, approval request and response, final answer) rather
  than writing directly to stdout or stderr.
- **REQ-UI-5** The agent MAY expose a local REST/HTTP interface (streaming via
  SSE or WebSocket) as an optional presentation layer, subject to the same
  auditability constraints (no remote egress by default).
- **REQ-UI-6** Presentation layers SHOULD be optional build features so the
  default binary stays small and auditable.
