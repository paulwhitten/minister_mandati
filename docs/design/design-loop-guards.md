# Design: Loop and Duplicate-Action Guards

Status: draft / proposal
Scope: `src/main.rs` execution loop, `src/tools/*`, `src/presenter.rs`, `src/context.rs`
Related: an early development run log (not published) that showed a repeated `write_file` loop

## Problem

In an early development run, the third task issued the same `write_file` call three times with
byte-identical arguments. Each call succeeded and returned a clear result, yet
the model kept re-issuing it before finally terminating with a minimal "Done."

The write path was not at fault. The gaps in the current loop are:

- No detection of an identical tool call that already succeeded this task.
- The only backstop against repetition is `MAX_STEPS = 50`.
- Identical state-changing calls re-trigger the approval prompt every time, so
  the operator approves the same action repeatedly.

## Goals

- Short-circuit an identical, already-successful, state-changing tool call
  (idempotency guard) without re-running it or re-prompting for approval.
- Detect broader no-progress repetition and intervene before `MAX_STEPS`.
- Preserve correctness: never suppress a legitimate repeat (different content,
  a re-read of possibly-changed state, or a command with real side effects).

## Non-goals

- Semantic equivalence of different arguments. Only exact-argument matches are
  considered duplicates.
- Cross-task memory. Guards are scoped to a single task/turn (see Scope).

## Feature 1: Duplicate successful-call skip

### Per-tool dedupe policy

Add a capability to `BaseTool` describing whether an identical successful call
is safe to skip:

```rust
pub enum DedupePolicy {
    /// Always execute; repeats may have side effects or time-varying output.
    Always,
    /// Skip if an identical call already succeeded this task (idempotent).
    SkipIfIdenticalSuccess,
}

// BaseTool gains:
fn dedupe_policy(&self) -> DedupePolicy { DedupePolicy::Always }
```

Per-tool assignment:

| Tool            | Policy                    | Rationale                                            |
|-----------------|---------------------------|------------------------------------------------------|
| `write_file`    | `SkipIfIdenticalSuccess`  | Overwriting with identical content is a true no-op.  |
| `read_file`     | `Always`                  | Cheap; file may have changed.                        |
| `list_dir`      | `Always`                  | Cheap; directory may have changed.                   |
| `execute_bash`  | `Always`                  | Commands have side effects and time-varying output.  |

This scoping fixes the observed `write_file` loop while leaving legitimate
re-reads and repeated shell commands untouched.

### Call fingerprint

Fingerprint a call as a stable hash of `(name, canonical_args)`:

- Serialize `args` (`serde_json::Value`) with sorted object keys so
  `{"a":1,"b":2}` and `{"b":2,"a":1}` hash equally.
- Hash with a `std::hash::Hasher` (for example `DefaultHasher`) into a `u64`.

### Loop integration

Maintain per-task state in `run_turn`:

```rust
struct TurnGuards {
    // fingerprint -> succeeded?  (only successful mutating calls are recorded)
    succeeded: HashMap<u64, String>, // value = original result payload
    recent: VecDeque<u64>,           // sliding window for Feature 2
}
```

Before executing a call whose tool policy is `SkipIfIdenticalSuccess`:

1. Compute the fingerprint.
2. If present in `succeeded`, skip execution. Do not prompt for approval.
   Append a synthetic tool result, for example:
   `already completed: identical write_file succeeded earlier (wrote N bytes to PATH); no changes made`.
3. Otherwise execute as normal; on success, record the fingerprint and result.

Only successes are recorded. A prior failure is not cached, so a retry after a
failed call is always allowed.

### Approval interaction

A skipped call performs no side effect, so it bypasses the approval gate. This
also removes the redundant re-approval the operator faced in the log.

## Feature 2: No-progress loop guard

A broader backstop that applies to all tools, independent of the skip in
Feature 1.

- Track the last `loop_guard_window` fingerprints in `recent`.
- If any single fingerprint appears at least `loop_guard_repeat_threshold`
  times within the window, treat it as a loop:
  1. First trip: inject a nudge as a tool/system message, for example
     `repeated identical action detected; if the task is complete, reply without further tool calls`.
  2. If repetition continues after the nudge, terminate the turn early via a
     new `Presenter::loop_detected` event (analogous to `step_cap_reached`),
     rather than waiting for `MAX_STEPS`.

This catches loops that the Feature 1 skip does not resolve, including a model
that ignores the synthetic "already completed" result, or alternation between
two distinct no-op actions.

## Presenter changes

Add one event so front ends can render the intervention:

```rust
fn loop_detected(&mut self, _fingerprint_repeats: usize) {}
```

`CliPresenter` prints a short notice; the core logs it via `tracing` for the
audit trail.

## Configuration (convention over configuration)

Defaults are chosen to be correct for the common case; all are overridable.

| Key (`[agent]`)                  | Default | Meaning                                        |
|----------------------------------|---------|------------------------------------------------|
| `dedupe_identical_writes`        | `true`  | Enable Feature 1 for `SkipIfIdenticalSuccess`. |
| `loop_guard_window`              | `6`     | Sliding window size for Feature 2.             |
| `loop_guard_repeat_threshold`    | `3`     | Repeats within the window that trip the guard. |
| `max_steps`                      | `50`    | Promote the current `MAX_STEPS` const to config. |

## Scope and lifetime

- Guards live for one task. In one-shot mode that is the process run; in the
  REPL it is one turn.
- `/reset` clears all guard state along with the conversation.
- A cancelled turn (Ctrl-C rollback) also discards guard state for that turn.

## Observability

- Log a skip at `info`: `duplicate call skipped` with tool and fingerprint.
- Log a loop intervention at `warn`: `loop guard tripped` with the repeat count.
- These entries keep the auditable posture: an operator can see exactly when a
  guard changed behavior.

## Edge cases and limitations

- Identical re-write after an external change is still skipped within the task.
  Acceptable given the per-task scope; documented as a known limitation.
- `execute_bash` is never skipped, so a genuinely repeated command still runs;
  the Feature 2 guard still protects against pathological repetition.

## Testing

- Fingerprint stability: object key order does not change the hash.
- Dedupe policy: `write_file` skips on identical success; `read_file` does not.
- Skip path returns a synthetic result and does not invoke approval.
- Loop guard: threshold and window logic trip and reset as specified.
- Integration: two identical `write_file` calls execute once; the second is
  skipped.

## Open questions

- Should `dedupe` scope be per-turn or per-session in the REPL? Per-turn is the
  safer default; per-session catches more but risks surprising a user who wants
  to rewrite the same file later.
- Should `execute_bash` gain an opt-in idempotency hint for known-safe commands?
  Deferred; `Always` is the safe default.
