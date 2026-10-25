# Design: verifier gate (verifier-gated self-refine)

Status: implemented, off by default, on a branch. The 2026-10 gate showed
no measurable effect on three models (see [Status and next
steps](#status-and-next-steps)); the [end check and
budget](#end-check-and-budget-2026-10-24) added on 2026-10-24 target what
the pro tasks showed, and are being measured. Plan item: Phase 1,
"verifier-gated self-refine" (`docs/plan.md`).

## Rule

The model revises its own work only in response to an **external verifier**:
a compiler, the project's tests, a parser. mima never asks the model to
"review" or "double-check" its own answer without new information.

## Evidence

- **Self-review without a verifier hurts.** Llama-2-70B on GSM8K fell from
  62.0 to 36.5 after two self-correction rounds (arXiv 2310.01798); GPT-4
  self-critique cut graph-colouring accuracy from 16% to 2% (2402.08115);
  Reflexion without its test step fell from 60% to 52% (2303.11366).
- **Repair driven by a real verifier helps.** Unit-test feedback added about
  8-12 points in Self-Debugging (2304.05128). SWE-agent's lint-on-edit guard,
  which rejects edits that introduce syntax errors, scored 18.0% against 15.0%
  without it on SWE-bench Lite (2405.15793).
- **Locating the error is the bottleneck, so feedback should be short and
  local.** Human-quality feedback raised GPT-4's repair rate from 33.3% to
  52.6% (2306.09896); models locate their mistakes poorly but fix well once
  told where (2311.08516). Longer feedback and failed-attempt history did not
  help (2402.08115, AlphaCodium 2401.08500).
- **Cap repair rounds.** Gains flatten after 2-3 rounds; after one failed
  edit SWE-agent's chance of eventual success falls from 90.5% to 57.2%.
  Agents that loop on verification cap it at about 3 (Aider
  `max_reflections = 3`, Roo Code and Cursor 3, Claude Code stop hooks 8).
- **Model-written tests are probes, not gates.** Only 44-59% of
  self-generated test suites were fully correct, and gating on them made
  open models worse (2501.12793); in Agentless only 94 of 213 generated
  reproduction tests accepted the correct fix.
- **Report only new problems, as external output.** Roo Code reports only
  errors that appeared with the edit; open models fix errors attributed to
  an external source more often than the same errors attributed to
  themselves (2507.02778).
- **A verifier loop invites test gaming; give an honest exit.** In
  ImpossibleBench (2510.20270) cheating rose with more submissions, and an
  explicit "flag for a human" option cut it sharply.
- **No surveyed agent detects test commands in code**; they take them from
  configuration or have the model read the project's docs. Detection here is
  our own choice and is conservative.

Full notes (literature review and a survey of agent implementations) are in
the private research repository.

## Design

Two tiers, both off the model's critical path unless they find something.

### Tier 1: syntax guard on every edit

After a successful `edit_file` or `write_file`, mima parses the file if it
has a cheap, side-effect-free parser:

| Files | Check |
|---|---|
| `.json` | in-process JSON parse |
| `.toml` | in-process TOML parse |
| `.py` | `python3 -c "import ast,sys; ast.parse(...)"` (parses, does not run) |
| `.sh`, `.bash` | `bash -n` |

If the file parsed before the edit and does not parse after it, the edit is
**reverted** (the previous bytes are restored) and the tool result says so,
with the parser's message and line. A file that was already broken is not
held to the rule. A **new** file that does not parse is kept, and the parser
output is added to the tool result (record kind `syntax_note`): it may be
meant that way, such as a fixture for a parser test. JSON files that their
tools read with comments (`tsconfig*.json`, `jsconfig*.json`, files under
`.vscode/` or `.devcontainer/`) are not checked. Languages that need a full
build (C, Rust) are left to tier 2.

### Tier 2: completion gate

When the model gives its final answer and the turn changed files, through
`edit_file`, `write_file`, or a shell command that changed git's view of the
workspace (generated files such as `__pycache__` aside), mima runs the
verifier before accepting it.

1. **Verifier commands**, in priority order:
   - `[verify].commands` from the user's configuration (not from a workspace
     `./agent.toml`, which is untrusted);
   - if `[verify].detect` is on and no commands are set, a detected project
     command: `Cargo.toml` -> `cargo test`; a Makefile with a `test:` target
     -> `make test`; `test_*.py` / `*_test.py` files -> `python3 -m pytest -q`
     (falling back to `python3 -m unittest` when pytest is missing);
   - otherwise no gate.
2. **Execution** goes through the same path as a model's `execute_bash`
   call: approval policy, shell sandbox, clean environment, timeout
   (`[verify].timeout_secs`, which replaces `[security].bash_timeout_secs`
   for these runs). A command the user would be asked to approve is asked
   about the same way. Each run has its own call id, carried by its approval
   and its `verification` record.
3. **Baseline.** Before the turn's first file change or shell command (a
   shell command may change files, which is known only afterwards), mima
   runs the verifier once and keeps its failure lines. The gate later
   reports only failures that are new against that baseline, so
   pre-existing failures are not the model's to fix. Failures are compared
   with durations, addresses and line numbers (`file:12:5`, `line 12`)
   removed, so an edit that moves an old failure does not make it new.
4. **Pass**: the answer is accepted.
5. **New failures**: the answer is not yet accepted. mima sends the model a
   message attributed to the tool, not to the model:
   `[automatic check] `cargo test` failed (exit 101). New failures:` followed
   by the extracted failure lines (errors, failing test names, panics,
   assertion values, `file:line` locations), capped at about 4 KB. It ends
   with: fix these, do not change existing tests to make them pass, or, if
   the tests or the task are wrong, say so in the answer and start it with
   `UNVERIFIED:`.
6. **Rounds**: at most `[verify].max_rounds` (default 3) per turn. After the
   last, the answer is accepted and the turn is recorded as unverified with
   the last failure. An answer given on the last step (or after the time
   limit with no repair steps) is treated the same way, since a round sent
   back could not be used. An answer starting with `UNVERIFIED:` is
   accepted at once.
7. **No critique prompts**: nothing in this feature asks the model to
   reconsider when the verifier passes or has nothing to say.

### Configuration

```toml
[verify]
enabled = false       # tier 2 completion gate (earlier name: gate)
syntax_guard = false  # tier 1
commands = []         # e.g. ["cargo build", "cargo test"]; empty: detect or none
detect = true         # with enabled, use a detected command when none is set
max_rounds = 3        # completion-gate rounds per turn
timeout_secs = 300    # per verifier command
end_check = false     # with enabled: workspace facts, checks at the limits
repair_steps = 0      # with the end check: last steps of the limit kept for repair

[agent]
max_seconds = 1500    # wall-clock limit per task (default: none)
wrap_up_percent = 80  # budget notice at this share of the step or time limit (0: off)
```

Both tiers are off by default until an A/B run shows they help; the
evaluation arm turns them on. `[verify]` in a workspace `./agent.toml` is
ignored apart from switching tiers off, like the security settings.

### Transcript

A `verification` record per run: `turn`, `kind` (`baseline`, `gate`,
`unverified`, `syntax`, `syntax_note`, `wrap_up`, `limit`, `end`), `round`,
`command`, `exit_code`, `passed`, `duration_ms`, the stored output,
`message` (the text sent to the model, if any), and `call_id` (the approval
this run answers; the audit check V5 counts it as that call's result). A notice with no command behind it (the
workspace facts alone, a budget notice before any change) is a record with
an empty `command`. Rebuild and replay re-add `message` as the same notice,
so resumed sessions see what the model saw. Records are attributed to rules
`verify.baseline`, `verify.gate`, `verify.syntax_guard`, `verify.wrap_up`
and `verify.limit`; the time limit's summary request is `loop_guard` action
`final_time` (rule `session.time_limit_summary`) and the turn ends with
outcome `time_limit`. `mima-eval` counts gate rounds, facts rounds, budget
and repair notices, and the last check after a repair notice.

## Not in this version

- **Verify before destructive actions.** Evidence favours deterministic rules
  plus read-only tool checks (for example listing what `git clean` would
  remove) over asking the model to verify itself (CoVe dropped correct
  answers; 2309.11495). Separate design.
- Detecting changes made through the shell for the gate itself (the end
  check's workspace facts do compare `git status` at turn start and end,
  but the gate still runs only after `edit_file` or `write_file`).
- Model-written tests as a gate (they stay probes the model may run).
- Language-server diagnostics.

## Evaluation

A/B on the same server and tasks, `main` against this feature, 3 trials per
task, with `detect = true` in the trial config. Report the paired
difference, gate rounds per trial, how often a gate failure was followed by
a pass, and any increase in test-file edits (gaming). Nemotron 3 Super was
RL-trained on test-execution reward (tech report 2604.12374), so it may use
gate feedback better than the older models in the literature; Qwen 3.6 on
the IGX gives a fast first measurement.

## Results

**Qwen 3.6 35B-A3B (NVFP4) on an RTX PRO 6000, full suite x 3 per arm**
(2026-10-11). Gate and syntax guard on vs off, same build:

| | Gate off | Gate on |
|---|---|---|
| Pass rate | 95.3% (303/318) | 95.0% (302/318) |
| Paired difference | | -0.3 points, 95% CI -2.7 to +2.1 |
| Verifier ran at an answer | | 178 trials |
| Feedback sent | | 1 trial (then passed) |
| Syntax reverts | | 0 |
| Trials whose diff touches test files | 25 | 21 |

**gpt-4o-mini (Azure OpenAI, model gpt-4o-mini-2024-07-18), full suite x 3
per arm** (2026-10-13), analysed with `tools/ab_compare.py` in the eval repo:

| | Gate off | Gate on |
|---|---|---|
| Pass rate | 43.4% (138/318) | 41.8% (133/318) |
| Paired difference | | -1.6 points, 95% CI -7.3 to +4.1 |
| Verifier ran at an answer | | 130 trials |
| Feedback sent | | 14 trials (21 rounds), 2 then passed |
| `UNVERIFIED:` answers | | 1 |
| Syntax reverts | | 13 trials (5 passed) |
| Trials whose diff touches test files | 16 | 18 |

Here the gate was active, unlike with Qwen, but feedback rarely turned into a
pass: 12 of the 14 trials that received feedback still failed, 9 of them at
the step cap or the loop guard (mostly the C repository tasks). Hypothesis,
not yet tested: feedback arrives when the step budget is nearly spent, so
the model has no room to act on it; reserving steps for repair rounds, or
raising the step cap by the rounds used, would test this. (Gate-active trials
are the ones already going wrong, so their pass rate is not a fair measure
of harm.)

**Nemotron 3 Nano 30B-A3B (NVFP4) on an RTX PRO 6000, full suite x 3 per
arm** (2026-10-13): 72.6% (231/318) both arms; paired +0.0 points, 95% CI
-4.3 to +4.3. The verifier ran at 101 answers and found new failures once
(that trial then passed); test-file edits 3 vs 3. This model's failures are
mostly step-cap and loop-guard exits (99 and 102 per arm), which end the
turn before any final answer, where the gate never acts; failed trials that
did answer passed the project's own tests and failed only the eval's checks,
which the gate cannot see.

Across the three models, the gate's reach is bounded by two things: what
the project's visible tests catch, and whether the model gets to a final
answer with steps to spare.

For Qwen 3.6:
no measurable effect either way, and no sign of extra test tampering. This
model nearly always checks its own work before answering (it ran a shell
command, mostly a build or a run of the program, after its last edit in 87%
of the trials that edited; a recognizable test-suite command in only 19% of
trials), and its answers already passed the project's tests, so the gate
almost never had anything to report. (Corrected 2026-10-13: an earlier
version said it "nearly always runs the project's tests itself", which the
transcripts do not support.) The
feature stays off by default. The next test is a model that runs the tests
less often before answering; whether Nemotron 3 Super does is to be checked
in its transcripts before choosing it.

## Status and next steps

On hold, 2026-10-13. The implementation works as designed, but in its
current form it rarely gets a chance to help:

- **Reach.** It acts only at a final answer. Nemotron 3 Nano 30B ends about
  100 of 318 trials per arm at the step cap or the loop guard, before any
  final answer.
- **Signal.** It reports new failures of existing tests, a regression
  signal. Published comparisons on SWE-bench rank regression tests as the
  weakest oracle signal; reproduction tests are much stronger (ORACLE-SWE,
  arXiv 2604.07789).
- **Budget.** Repair rounds re-enter the same step loop and use the model's
  step cap; with gpt-4o-mini, step-cap exits rose from 16 to 34.
- **Model dependence.** The models that check their own work (Qwen 3.6 ran a
  command after its last edit in 87% of the trials that edited) gain
  nothing; the weakest (gpt-4o-mini, 24%) rarely turn feedback into a fix.
  Published results point the same way: verification and repair help a
  middle band of models; strong reasoning models already verify, and weak
  non-reasoning models resample better than they repair (ICPC-Eval, arXiv
  2506.04894).

Experiments for when this resumes, cheapest first, each a paired comparison
on the full suite:

1. Show the remaining step budget in every step.
2. Give repair rounds their own steps, and run the gate also near the step
   cap and at a loop-guard stop when files were edited.
3. For non-reasoning models only: an explicit "run a real check that
   exercises the change before reporting done" cue with the project's test
   command.
4. Shorter, concrete feedback: failing test name with expected and actual
   values.
5. For reasoning models: an efficiency cue (stop once the relevant tests
   pass; do not re-run a passing suite).

The expected effects are a few points, near the smallest difference 106
tasks x 3 trials can detect; a task subset where baseline runs end with new
test failures would give more power. Full literature notes are in the private
research repository.

## End check and budget (2026-10-24)

A review of the pro-task runs (Qwen 3.6, 54 trials, 32 failures) found that
the gate could not have helped there either, for reasons a test signal at
the answer does not reach:

- 25 of the 32 failures never reached an answer: 14 ran into the wall-clock
  timeout (every timeout contained a reply cut at the 131,072-token reply
  limit) and 9 hit the step limit while still repairing.
- In all 7 genuine "done" failures the agent's last test run had passed;
  4-5 missed an edge case of the requirements and 2 edited a protected
  existing test file.
- 7 failures ended with a broken build, all at the step or time limit, 3 of
  them from the agent's own scratch or test files.

The changes below keep the rule (feedback only from external, deterministic
signals) and move the checks to where these trials ended. The reply limit
itself is a configuration matter (an evaluation arm lowers it to 49,152
tokens; the longest reply in a passing trial was 45,714).

**Workspace facts** (`[verify].end_check`, with the gate). When the turn
began, mima snapshots the files git reports as changed. At an answer it
compares again and reports, once per turn, existing test files the turn
modified and new untracked files it left, also when the tests pass:

```
[automatic check 1 of 3] The checks pass. What this task changed besides that:
Existing test files changed in this task: mux_test.go.
New files not tracked by git: scratch/main.go.

Undo any of these changes the task did not ask for, then give your answer
again; if they are all intended, give your answer again as it is. ...
```

Changes made through the shell are included, since the comparison is
against git's view, not mima's own edits. Files the verifier's own runs
wrote (mima compares git's view before and after each run) and generated
files (`__pycache__`, `*.pyc`, `.pytest_cache`, `node_modules`, a top-level
`target/`, coverage output) are left out: they are not the task's. A
test-file edit is reported, not reverted: adding tests to an existing file
is often right.

**Repair point** (`[verify].repair_steps`, with the end check). The last N
steps of the step limit are kept for repair: when N steps are left and the
verifier finds new failures, the model is told, with the failure lines; if
the checks pass, nothing is said. The total number of steps is unchanged,
so a paired comparison holds compute equal.

**Time limit** (`[agent].max_seconds`). Checked between steps (a reply in
progress is not interrupted). At the limit the turn ends like at the step
limit, with a summary request worded for time, and outcome `time_limit`.
With the end check, if the verifier finds new failures, up to `repair_steps`
more steps are allowed for repair first.

**Budget notice** (`[agent].wrap_up_percent`). At that share of the step or
time limit, one notice says where the turn stands, with the verifier's
result when files changed:

```
[budget] Step 48 of 60, 20 of 25 minutes used. `go test ./...` passes.
The task ends at the limit; finish and check the work within it.
```

After a repair notice at the step or time limit, the verifier runs once
more as the turn ends (kind `end`, recorded only), so the evaluation can
count repairs that finished.

**Detection** now also covers Go modules (`go test ./...`) and a
`package.json` test script (`npm test --silent`, not npm's placeholder).

**Measurement.** Pro tasks x 3 on Qwen 3.6 with the 49,152-token reply
limit, paired against the same profile without these settings. Treatment:
`enabled`, `end_check`, `repair_steps = 5`, `max_seconds = 1500` (the harness
stops trials at 1,800 s), `wrap_up_percent = 80`. Readouts: pass rate with
its interval and minimum detectable effect; firings by kind; repair notices
followed by a passing `end` check; passes that become failures; step-limit
and time-limit endings; edits to test files.
