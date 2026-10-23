# Design: Tool vocabularies

Status: proposal.
Related: [design-audit-record.md](design-audit-record.md) (attribution, rule
ids), [design-shell-sandboxing.md](design-shell-sandboxing.md),
[../editing.md](../editing.md) (edit semantics), [../eval.md](../eval.md)
(measurement).

## Summary

mima advertises to each model the tool names and argument shapes it was
trained on, and implements all of them on one set of core operations with
the same approval, sandbox and audit rules. A **vocabulary** is a data-only
adapter: the tool specifications to advertise, how each call's arguments map
onto a core operation (renames, unit and offset conversion, type coercion),
the path policy, and how results and errors are rendered. One vocabulary is
advertised per session, chosen from the model id by a table compiled into
the binary (or set in the configuration), and recorded in the audit header.
Calls written in any other known vocabulary are still accepted.

First vocabularies: `openhands` (Nemotron 3 and the Anthropic-2024 editor
family), `codex` (`apply_patch`, shell, plan), `opencode` / `claude-code`
(separate read, write, edit, glob, grep, bash), and mima's own, which stays
the default for models with no evidence of a trained vocabulary.

## Why

- **Models fall back to the tools they trained on.** Nemotron 3 Nano 30B's
  training data uses OpenHands' `execute_bash`, `str_replace_editor`, `think`
  and `finish`. Under mima's own names it called `str_replace_editor` in 31%
  of its calls on the pro tasks (9.7% on the full suite), ran mima's tool
  names as shell commands in 7%, and ended 11 of 20 pro trials in the loop
  guard before the string-argument fix.
- **The server cannot help with tools it was not told about.** vLLM converts
  tool-call arguments to their declared types only for advertised tools; an
  unadvertised `str_replace_editor` arrives with every value as a string
  (`"offset": "735"`, `"view_range": "[1, 100]"`).
- **Mapping without advertising reinforces the wrong name.** The history
  shows the model its own call succeeding; after the first alias call, 61-85%
  of its later file views used it.
- **Other frameworks converged on this.** OpenHands' current SDK selects a
  tool preset per model (its default editor, `apply_patch` for GPT-5,
  Gemini-style file tools for Gemini); OpenAI's guidance for GPT-5 is to use
  `apply_patch` "to match the training distribution".
- **The same name means different things across vocabularies**, so renaming
  is not enough: `offset` is 0-based in Gemini CLI and Qwen Code and 1-based
  in mima and OpenCode; shell `timeout` is seconds in OpenHands and
  milliseconds in Codex, Claude Code and OpenCode; view output is
  `cat -n`-style in OpenHands, `N: ` in OpenCode, `L{n}: ` in Codex and
  unnumbered in Gemini CLI. Models also learn the result and error texts.

## Goals and non-goals

Goals:

- Each supported model sees one coherent tool set close to its training
  distribution: names, argument shapes, units, result and error formats.
- One implementation of each operation: every vocabulary inherits the
  sandbox, approval policy, read-before-edit protection, output caps and
  audit record unchanged.
- Deterministic selection, recorded in the audit header; a person can
  override it.
- Measured: each vocabulary ships behind a paired comparison against mima's
  own vocabulary on the same model.

Non-goals:

- Reproducing other agents' runtimes. Persistent interactive terminals
  (OpenHands `is_input`, Codex `write_stdin`), browsers and Python kernels
  are out of scope; calls that need them get a clear error.
- Copying other projects' description text. Names and argument schemas are
  interface facts; descriptions are written for mima, keeping the trained
  semantics (the same policy as the Seatbelt rules, commit 4a61467).
- Advertising several vocabularies at once. Overlapping tools dilute the
  "one way to do a thing" principle and cost tokens.

## Core operations

Every vocabulary maps onto these. Existing ones are refactored out of the
current tools; NEW ones are added.

| Op | Canonical inputs | Existing in mima |
|---|---|---|
| `read_lines` | path, start (1-based), end (inclusive or end of file), cap | `read_file` |
| `list_dir` | path, depth (1-2), hidden, ignore globs | `list_dir` (depth 1) |
| `glob` | pattern, root, sort, limit | `find_files` |
| `grep` | regex, root, include glob, case, mode (lines, files, count), limit | `search_files` (lines) |
| `write` | path, content, `create_only` or `create_or_overwrite` | `write_file` |
| `replace` | path, old, new, expect one, all or a count | `edit_file` |
| `insert` NEW | path, after line (0 = top), text | |
| `apply_patch` NEW | V4A patch: add, delete, update and move operations, verified in memory before any write | |
| `undo` NEW | path; per-file stack of 10 prior contents, refused if the file changed since | |
| `exec` | command, working directory, timeout (ms) | `execute_bash` |
| `plan` NEW | items {text, status}, rendered back as a checklist | |
| `control` NEW | `finish(message)` ends the turn with that answer; `think(thought)` is recorded and acknowledged | |

Each core op keeps the current safety rules: paths through `ensure_allowed`
(absolute paths inside the workspace are accepted), writes through the
approval policy with a diff preview, the read-freshness tracker on edits,
the stage-0 output cap, and the audit evidence (file hashes before and
after, workspace changes for shell commands). `undo` and `apply_patch` are
writes: approved, previewed and recorded like `edit_file`. `insert`,
`undo` and `apply_patch` record their rule-relevant facts in the same
`tool_result.file` fields.

**Shell working directory.** OpenHands- and Anthropic-trained models `cd`
once and expect it to persist; Claude Code keeps the working directory
between calls. `exec` gains an optional session working directory: after
each command mima reads the final directory (a marker the command wrapper
prints) and starts the next command there, inside the workspace only. It is
deterministic and visible in the transcript. Environment variables do not
persist.

## Vocabulary adapters

An adapter is a table in `src/tools/vocab/<name>.rs`, no new logic:

1. **Specs:** tool names, descriptions (ours), JSON Schemas with the trained
   argument names, `required`, enums, `minimum`, and
   `additionalProperties: false`.
2. **Argument mapping** into a core op: renames (`file_path` and `path`,
   `old_str` and `old_string`), unit conversion (seconds to ms), offset base
   (0- or 1-based), coercion to the declared type only (numbers or JSON
   arrays sent as strings), and defaults.
3. **Path policy:** absolute required (OpenHands, Claude Code), or relative
   allowed. The real workspace path goes into the path description, as
   OpenHands does. A path under a training workspace that does not exist
   here (`/workspace/...`, `/testbed/...`) is mapped to the workspace root
   with a note in the result.
4. **Result renderer:** view format and header, line-number style, shell
   result suffix (`[The command completed with exit code N.]` for
   OpenHands; `Exit code: N` and `Output:` for Codex), success texts.
5. **Error texts:** the messages the vocabulary's models were trained on,
   in our wording where they are prose.

Line-number prefixes that models copy into `old_str` are stripped for every
format a renderer produces (`%6d\t`, `N: `, `L{n}: `), extending what
`edit_file` does today.

### The first vocabularies

| Vocabulary | Advertised tools | For | Evidence |
|---|---|---|---|
| `mima` | `read_file`, `write_file`, `edit_file`, `list_dir`, `find_files`, `search_files`, `execute_bash` | Qwen 3.6, unknown models | Qwen 3.6: 2 invalid calls in 4,037 |
| `openhands` | `execute_bash {command, timeout s}`, `str_replace_editor {command: view, create, str_replace, insert, undo_edit; path, view_range, old_str, new_str, file_text, insert_line}`, `think`, `finish`; optional `task_tracker` | Nemotron 3 Nano and Super; DeepSeek V3.1; Kimi K2.5 harness; the Anthropic 2024 editor | released training data (Nemotron) |
| `codex` | `shell_command {command, workdir, timeout_ms}` (and `shell` with an argv array), `apply_patch {input}` (raw text accepted), `update_plan` | gpt-oss, GPT-5 family, Nemotron 3 Super's Codex agent | vendor guidance; Super's card |
| `opencode` | `read`, `write`, `edit {filePath, oldString, newString, replaceAll}`, `glob`, `grep`, `bash`, `todowrite` | Nemotron 3 Super (released OpenCode data), GLM, MiniMax | released data (Super) |
| `claude-code` | `Read`, `Write`, `Edit`, `Glob`, `Grep`, `Bash`, `TodoWrite` | models trained against Claude Code | compatibility claims |

`openhands` follows OpenHands V0 behavior where V0 and V1 differ, since
that is what Nemotron's data shows: `create` refuses an existing file, a
`str_replace` without `new_str` deletes the match, `view_range` is inclusive
with `-1` for the end of file and an end past the file is clamped with a
note, `view` on a directory lists two levels.

### Selection

```toml
[agent]
# tools = "auto"   # or "mima", "openhands", "codex", "opencode", "claude-code"
```

`auto` matches the served model id (from `/v1/models`, else the configured
name) against a table compiled into the binary, first match wins:

| Pattern | Vocabulary |
|---|---|
| `nemotron-3-nano`, `nemotron-3-super`, `nemotron3` | `openhands` |
| `gpt-oss`, `gpt-5`, `codex` | `codex` |
| anything else | `mima` |

The table changes only with measurements behind it (Evaluation below). A
workspace `./agent.toml` may not set `tools` (it would steer what the model
can do); the user configuration and profiles can. The chosen vocabulary,
how it was chosen (`auto:<pattern>` or `config`), and the hash of the
advertised specs are recorded in `session_start.tools`.

### Calls from other vocabularies

`normalize_call` stays: a call written for another vocabulary is mapped to
the core op, converting units and offsets for the vocabulary it came from.
Its result is rendered in the advertised vocabulary's format, and a
one-line note names the advertised tool to use. The call is recorded with
both names. `finish`, `submit` and `attempt_completion` end the turn with
their message in every vocabulary.

## Audit record

- `session_start.tools`: `vocabulary`, `selected_by`, `names`, `sha256` of
  the specs (the last two exist today).
- `model_response.tool_calls[]`: the name as the model wrote it.
- `tool_result`: `op` (the core operation) and `vocabulary` when the call
  came from a different one; approval rules key on the core op, so policy
  and rule ids (`approval.read_only`, `approval.auto_approve_bash`, ...) are
  the same in every vocabulary.
- New rule ids: `tools.alias` (a call mapped from another vocabulary),
  `tools.path_remap` (a training-workspace path mapped to the workspace).

## Evaluation

Each vocabulary is measured before it enters the `auto` table, with the
paired protocol in [../eval.md](../eval.md): the same model, the full suite
and the pro suite, 3 trials, the vocabulary as the only difference.

| Metric | Why |
|---|---|
| Pass rate (difference and minimum detectable effect) | the outcome |
| Invalid-call rate (unknown names, schema violations) | the mechanism |
| Alias calls, tool names run as shell commands | the mechanism |
| Loop-guard trips, no-answer and truncated outcomes | failure modes |
| Steps, tokens, time per solved task | cost |
| False acceptance (claimed done, hidden checks fail) | trust |

First measurements: Nemotron 3 Nano 30B with `openhands` against `mima`;
Qwen 3.6 with `openhands` against `mima` (to check the default); then
Nemotron 3 Super with `openhands`, `opencode` and `codex`.

## Implementation plan

1. **Core ops refactor.** Move the logic of the current tools behind the core
   ops with no behavior change; the `mima` vocabulary is the first adapter.
   Tests: the existing tool tests pass unchanged, and transcripts of a
   smoke run are equal apart from new fields.
2. **New core ops:** `insert`, `undo`, `control` (`finish`, `think`), `plan`,
   shell working directory. Each with approval, preview, audit and tests.
3. **`openhands` adapter**, `auto` selection, audit fields, `tools.alias`.
   Then the Nemotron 3 Nano A/B.
4. **`apply_patch`** (V4A parser: tolerant of whitespace, CRLF, a heredoc
   wrapper, a missing final newline, stacked `@@` anchors; every hunk
   verified before any write; one combined diff for approval) and the
   **`codex`** adapter.
5. **`opencode`** and **`claude-code`** adapters (mostly renames and
   renderers).
6. Per-vocabulary A/Bs; update the `auto` table from the results.

## Alternatives considered

- **Keep mima's names, add constrained decoding (`strict`).** Forces valid
  calls on vLLM, but on names and shapes the model was not trained on; it
  remains a separate option (to be measured on its own) and can be combined
  with any vocabulary.
- **Advertise the union of all vocabularies.** More tokens, overlapping
  tools, and conflicting semantics for the same names (`read_file`,
  `offset`).
- **Map aliases only, rewriting history to mima's names.** Smaller change,
  but the model keeps reaching for tools whose types the server cannot
  convert, and every call pays a round of correction.
- **Copy the other projects' tool descriptions verbatim.** Possibly closer
  to training, but it imports third-party text (MIT and Apache-2.0) into
  the binary; untested whether wording matters as much as names and
  shapes. Can be measured later with attribution if it does.

## Open questions

1. **Description wording:** own wording (proposed) or verbatim with
   attribution, if a measurement shows a difference.
2. **Shell persistence:** working directory only (proposed, as Claude Code)
   or also environment variables (as OpenHands and Anthropic `bash`).
3. **Qwen 3.6:** stays on `mima` unless `openhands` measures better; Qwen's
   training scaffold is not published.
