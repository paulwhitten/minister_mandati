# Design: Evaluation Harness (skeleton)

Status: draft / skeleton implemented in `src/eval.rs`
Scope: `src/eval.rs` now; task running and CLI wiring later
Related: `docs/plan.md` (Phase 2), `rust_terminal_agent_plan.md`
(Evaluation Subsystem)

## Problem

Evaluation is a first-class Phase 2 subsystem, but nothing measures agent
quality yet. Regressions such as the repeated-write loop observed during development should be caught
by an automated, verifiable check rather than by reading logs.

## Goals

- Grade a task outcome with a composable verifier vector, not a single boolean.
- Report pass rate with a standard error so versions can be compared.
- Keep graders cheap, deterministic, and verifiable (a nonzero exit is ground
  truth), matching the Phase 2 spec.

## Non-goals (this skeleton)

- Running the agent in a sandbox to produce outcomes.
- LLM-as-judge grading, injection red-team tracks, and paired significance
  testing. These come later; the types here are the foundation.

## Implemented types (`src/eval.rs`)

- `Outcome` — what graders inspect: `workdir`, `final_answer`, `steps`,
  `total_tokens`.
- `Grade` — one typed result: `grader`, `passed`, `score` in `[0,1]`, `detail`.
- `Grader` trait — `name()` and `grade(&Outcome) -> Grade`.
- Concrete graders:
  - `FileExists` — a path exists under the working directory.
  - `FileContains` — a file contains a needle.
  - `CommandSucceeds` — a shell command exits zero (models compile/test/lint).
- `Verifier` — an ordered vector of graders; `verify(&Outcome) -> Report`.
- `Report` — `passed()` (all graders pass) and `score()` (mean).
- `Task` — `id`, `instruction`, `verifier`.
- `summarize(&[bool]) -> PassStats` — pass rate and Bernoulli SEM
  `sqrt(p(1-p)/n)`, the basis for pass@1 / pass@k.

## Next steps (not in this skeleton)

- A task runner that executes an instruction against the agent in an isolated,
  network-controlled `workdir`, populates an `Outcome`, and runs the verifier.
- pass@k over N seeds and a paired comparison (variance of `A(x) - B(x)`) with a
  z-score and a stated minimum detectable effect size.
- A seed task set (Linux/kernel build, bug-fix, terminal end-state) and a
  loop-guard regression task derived from that observed loop.
- CLI wiring (for example a `mima eval <taskset>` subcommand) behind a feature
  flag so the default binary stays small.

## Testing

`src/eval.rs` unit tests cover each grader, verifier aggregation, and the
`summarize` statistics. They avoid temp files by grading against known
repository paths (`Cargo.toml`) and trivial shell commands (`true` / `false`).
