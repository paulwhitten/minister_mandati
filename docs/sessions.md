# Sessions and transcripts

Status: implemented (sessions, transcripts, `/new`, `/session`,
`/enable_transcript`, `/disable_transcript`, `--transcript`, `--sessions`,
`--resume`, `/sessions`, `/resume`). Resume is described in
[design/session-resume.md](design/session-resume.md); recall is planned.

A **session** is one continuous conversation between an operator and `mima`,
with one context (history, token ledger, budget) and, when enabled, one
**transcript**: an append-only, timestamped record of everything that
happened, saved under `~/.mima/transcripts/`.

**Transcripts are off by default.** `mima` states the transcript state when it
starts:

```
Transcripts: off. Enable with /enable_transcript.     (interactive)
Transcripts: off. Enable with --transcript.            (one-shot, on stderr)
Transcripts: on (/home/user/.mima/transcripts/20260927T143205Z-minister_mandati-3f9a.jsonl)
```

Turn them on for one run with `--transcript`, during an interactive session
with `/enable_transcript`, or always with `[session].transcripts = true`.

Transcripts serve three purposes:

1. **Audit.** What the agent was asked, what it did, what it was allowed to
   do, and when. This is the core of an auditable agent.
2. **Recall.** Context management hides old tool outputs from the model
   ([context.md](context.md)); the transcript keeps them in full, so a
   placeholder can point to the original.
3. **Resume** (later). A transcript holds enough to rebuild a session's
   context.

## Session lifecycle

| Event | What happens |
|---|---|
| `mima "<instruction>"` or piped input | a new session for that one task; it ends when the task ends |
| `mima` (interactive) | a new session that lasts until `/exit`, Ctrl-D, or `/new` |
| `mima -i "<instruction>"` | as above, seeded with the instruction |
| `/new` (interactive) | ends the current session and starts a fresh one: new id, empty context, and a new transcript if recording was on |
| `/reset` | alias of `/new` (clearing the context is starting a new session) |
| `/session` | prints the session id, start time (UTC), turn count, and transcript path or `off` |
| `/enable_transcript` | starts recording the current session from this point |
| `/disable_transcript` | stops recording; the session continues. Enabling again appends to the same file |
| Ctrl-C during a turn | the turn is cancelled and recorded as such; the session continues |
| Ctrl-C in one-shot mode, or a fatal error | the session ends; the last lines record why when possible |
| `mima --resume <id\|last>` | later: rebuild the context from a transcript and continue in a new session linked to it |

A recorded session can continue in a later process with `--resume` (see
[design/session-resume.md](design/session-resume.md)): the context is rebuilt
from the transcript and the conversation is appended to the same file. Each
transcript is locked while a process has it open, so two `mima` processes
never write to one file; each running process has its own session.

## Session id and file name

```
~/.mima/transcripts/20260927T143205Z-minister_mandati-3f9a.jsonl
                    └─ start time ─┘ └─── label ────┘ └──┘ 4 hex chars of randomness
```

- The id and the file name are the same string, so `/session` output maps
  directly to a file.
- **Start time** in UTC (basic ISO 8601, second precision) comes first, so
  `ls` lists sessions in start order.
- **Label**: the name of the project the working directory belongs to, so a
  listing shows which repository each session was in. `mima` walks up from
  the working directory to the nearest folder containing `.git` (a directory,
  or a file for worktrees and submodules) and uses that folder's name, so it
  is the same from anywhere inside a repository. Outside a repository it is
  the last folder of the working directory.
  - Only `A-Z a-z 0-9 . _ -` are kept; anything else becomes `_`. At most 32
    characters. Omitted when nothing usable remains (for example in `/`),
    giving `20260927T143205Z-3f9a`.
  - It is a label, not an identifier: folder names repeat, and uniqueness
    comes from the timestamp and random suffix. The exact path is in the
    `cwd` field of `session_start`. Labels may contain `-`, so parse an id by
    its first and last `-`.
- The random suffix separates sessions started in the same second.
- The name uses no `:`, so it is valid on every file system.

## Date and time

Every record carries a timestamp:

- `ts`: wall-clock time in **UTC**, RFC 3339 with milliseconds, e.g.
  `2026-09-27T14:32:05.123Z`. All times are UTC: unambiguous across time
  zones and daylight-saving changes, and sortable as text. No local time or
  offset is recorded.
- `session_start` also carries `session_started`, the session's own start
  time, which differs from `ts` when recording was enabled mid-session.
- Durations (`duration_ms`) come from a monotonic clock, so they stay correct
  even if the wall clock changes mid-session.

## Format

JSON Lines: one JSON object per line, appended and flushed as each event
happens, so a crash loses at most the event in progress. Every line has
`ts`, `type`, and `seq` (a per-session counter, to detect gaps or reordering),
plus the audit chain fields `salt` and `hash` (see "Audit chain and session
ledger" below; the example lines omit them).

```json
{"seq":0,"ts":"2026-09-27T14:32:05.123Z","type":"session_start","schema":1,"session":"20260927T143205Z-minister_mandati-3f9a","session_started":"2026-09-27T14:32:05.120Z","turns_before":0,"mima_version":"0.1.0","mode":"interactive","cwd":"/home/user/minister_mandati/src","model":"nvidia/NVIDIA-Nemotron-3-Super-120B-A12B-NVFP4","base_url":"http://thor.local:8000/v1","window":32768,"budget":27689,"approvals":{"bash":true,"writes":true,"auto_approve_bash":["cargo test"]}}
{"seq":1,"ts":"2026-09-27T14:32:09.870Z","type":"turn_start","turn":1,"instruction":"add a unit test for truncate_middle"}
{"seq":2,"ts":"2026-09-27T14:32:14.402Z","type":"model_response","turn":1,"step":0,"duration_ms":4521,"content":null,"tool_calls":[{"id":"call_1","name":"read_file","args":{"path":"src/tools/mod.rs"}}],"usage":{"prompt":1843,"completion":61}}
{"seq":3,"ts":"2026-09-27T14:32:14.405Z","type":"approval","turn":1,"call_id":"call_1","decision":"not_required"}
{"seq":4,"ts":"2026-09-27T14:32:14.411Z","type":"tool_result","turn":1,"call_id":"call_1","tool":"read_file","duration_ms":6,"failed":false,"bytes":9120,"sent_bytes":9120,"output":"..."}
{"seq":5,"ts":"2026-09-27T14:33:40.002Z","type":"compaction","stage":"mask","reason":"threshold","call_ids":["call_1"],"est_tokens_before":17120,"est_tokens_after":10902}
{"seq":6,"ts":"2026-09-27T14:34:02.550Z","type":"turn_end","turn":1,"outcome":"answered","answer":"Added test ...","tokens":{"requests":6,"prompt":20411,"completion":812}}
{"seq":7,"ts":"2026-09-27T14:35:10.004Z","type":"session_end","reason":"exit","turns":1,"tokens":{"requests":6,"prompt":20411,"completion":812}}
```

### Record types (schema 1)

| `type` | Written when | Key fields |
|---|---|---|
| `session_start` | recording begins (at session start, or when enabled later) | `schema`, `session`, `session_started`, `turns_before`, `mima_version`, `mode`, `cwd`, `model`, `base_url`, `window`, `budget`, `approvals`, `allowed_paths`, `token_counting` (`tokenize` or `usage+estimate`), `chain` (audit chain rules, `v1`), `gap_after_seq` (when recording resumes after `/disable_transcript`: that record's `seq`) |
| `turn_start` | an instruction arrives | `turn`, `instruction` |
| `model_response` | a completion returns | `turn`, `step`, `duration_ms`, `content`, `tool_calls`, `usage` (server-reported), `counted_prompt_tokens` and `count_source` (the count before sending; see [context.md](context.md)) |
| `model_error` | a completion fails | `turn`, `step`, `error`, `overflow` (bool), `retry` |
| `approval` | before a tool runs | `call_id`, `decision`: `not_required`, `auto_approved`, `approved`, `denied`, `skipped_duplicate`, `not_requested_invalid` (the call could not succeed, so nothing was asked); `preview` (the diff shown, for edits and overwrites); `actor`, and `rule` (with `matched`) or the operator's `answer`, `shown_sha256`, `wait_ms` (see Attribution) |
| `tool_result` | a tool finishes | `call_id`, `tool`, `duration_ms` (the tool's run time, without the approval wait; 0 if it did not run), `failed`, `bytes`, `sent_bytes` (after the stage 0 cap), `output` (full), `cap_bytes` (the cap in effect), `file` (`{path, fnv1a64}` after a read, edit or write) |
| `compaction` | a context stage acts | `stage` (`normalize`, `mask`, `evict`), `reason`, `call_ids`, `messages_removed`, token estimates before and after; eviction also writes `method: "digest"`, `record` (its number), `requests_removed`, `seqs` (records of the removed messages), `digest` (cumulative, structured) and `text` (the compaction record the model sees) |
| `loop_guard` | a nudge or stop | `action`, `repeats`, `tool` |
| `turn_end` | a turn finishes | `turn`, `outcome` (`answered`, `no_answer` (only empty replies after the continue prompts), `truncated` (replies still cut off at `max_tokens`; `answer` holds any partial text), `loop_guard`, `step_cap`, `cancelled`, `error`), `answer`, `tokens` |
| `transcript_disabled` | `/disable_transcript` | `turns` |
| `session_end` | session ends | `reason` (`exit`, `eof`, `new`, `resume`, `task_done`, `interrupted`, `fatal`), `turns`, `tokens`, `error` |
| `context` | a turn ends, or compaction runs | what the model sees, by reference to earlier records (`entries`), with `context_schema` and a `check`; see [design/session-resume.md](design/session-resume.md) |
| `chain_reset` | `--resume --accept-chain-break` in required mode | `reason`, `findings` (the verifier's failures) |
| `session_resumed` | `--resume` or `/resume` | `method` (`context` or `replay`), `reason`, `from_seq`, `mima_version`, `model`, `cwd`, `changed_files` |

The format is defined by `schemas/transcript.schema.json` (JSON Schema
2020-12, generated from the Rust types in `src/transcript.rs`), and changes
follow the evolution rules in
[design/session-resume.md](design/session-resume.md): readers ignore unknown
fields and record types, additions are optional, and `schema` (or
`context_schema` for `context` records) increments only for breaking
changes. `cargo test` checks the committed schema, validates everything mima
writes against it, and compares it with the last released schema.

### What is recorded

- The **full** tool output, not the capped copy the model saw (`bytes`
  versus `sent_bytes` shows the difference). A single output is stored up to
  1 MiB; beyond that it is cut with a marker, to keep one runaway command
  from filling the disk.
- Final answers in full. Streamed chunks are not recorded individually.
- Never the API key. `base_url` is recorded; the key is not.

## Audit chain and session ledger

Design: [design/design-audit-record.md](design/design-audit-record.md).
Implemented in `src/audit.rs`.

**Chain.** Each record carries `salt` (16 random bytes, hex) and `hash`:

```
hash = SHA-256( previous record's hash || canonical(disclosure form) )
```

The disclosure form is the record without `hash`, `salt`, `salts` and
`withheld`, with each content field replaced by `"c:" + hex(SHA-256(field
salt || 0x00 || canonical(value)))`, where the field salt is `SHA-256(salt ||
path)`. Content fields: `turn_start.instruction`, `model_response.content`,
`.reasoning` and `.tool_calls/*/args`, `approval.preview`,
`tool_result.output`, `.output_full_sha256` and `.converted/*/from`, `turn_end.answer`,
`context.entries/*/text`, `compaction.text` and `.digest`,
`session_start.system_prompt/text` (it includes
the project instructions). Canonical encoding: keys sorted, no whitespace,
minimal escaping, integers as written, floats in shortest round-trip form.

**Sharing.** `mima --audit-export <id> --withhold <set> [--output FILE]`
writes a copy in which the chosen content fields hold their digests (listed
in `withheld`), disclosed content fields carry their own field salt (in
`salts`), and record salts are gone, so withheld values cannot be guessed.
Sets: `code` (all content, the default), `outputs` (tool outputs and
diffs, plus the compaction records and context snapshots that quote tool
output), or field names such as `turn_start.instruction`. The copy verifies
with the same hashes and finds its heads in the ledger by session id.
Disclosing one more value later is another export that withholds less.

When the line before a record has no hash (a transcript written before
chaining, or a partial line left by a crash), the chain continues from the
SHA-256 of all bytes before the record. The last record's `hash` is the
transcript's **head**; `mima` prints it when the session ends.

**Attribution.** Every record has `actor`: `operator` (an instruction,
`/disable_transcript`, an approval answer), `model` (a completion), `rule`
(a decision by mima's deterministic code), `tool` (a tool's result) or
`system` (session lifecycle, turn ends, context records, model errors).
Records decided by a rule also name it in `rule`:

| Rule | Decides |
|---|---|
| `approval.read_only` | a read-only tool runs without approval |
| `approval.bash_not_required` | `require_approval_for_bash = false` |
| `approval.writes_not_required` | `require_approval_for_writes = false` |
| `approval.auto_approve_sandboxed` | a sandboxed shell command, with `auto_approve_sandboxed` |
| `approval.auto_approve_bash` | an allowlist match; the entry is in `matched` |
| `approval.approve_all` | `--approve-all` (evaluation harness), not a person |
| `approval.invalid_call` | the call could not succeed, so nobody was asked |
| `tools.convert_args` | an argument sent as the wrong JSON type was converted (`tool_result.converted`) |
| `loop_guard.skip_duplicate` | an identical, already successful write is skipped |
| `loop_guard.nudge` | repeated identical actions: a nudge is added |
| `loop_guard.terminate` | repeated identical actions: the turn stops |
| `loop_guard.continue` | an empty or cut-off reply: the model is asked to continue |
| `loop_guard.steps_left` | the step limit is near: the model is told |
| `context.normalize` | tool calls and results re-paired |
| `context.mask` | old tool outputs replaced by placeholders |
| `context.evict` | old steps removed |
| `session.step_cap_summary` | at the step limit, a final summary is requested |

**Header and evidence.** `session_start` records `build` (version, commit,
whether the tree had uncommitted changes, profile, target, compiler),
`binary_sha256`, `config` (source file, its SHA-256, whether it was the
workspace's, ignored settings, the names of `MIMA_*` variables set, and the
effective configuration without the API key), `system_prompt` (`text` and
`sha256`), `tools` (`names`, `sha256` of the specifications sent),
`sampling`, `server` (the model's `/models` entry, the server version),
`sandbox`, `operator` (`user`, `uid`, `host`, `tty`), `workspace` (`root`,
`git`: `head`, `branch`, `status_sha256`, `dirty`), `os`, and `instructions`
(the project instruction files: `cap`, `loaded_bytes`, `override`, and per
file `source`, `path`, `scope`, `bytes`, `raw_sha256`, `loaded_sha256`,
`loaded_bytes`, `truncated`, `removed_chars`, `skipped`;
[design/design-project-instructions.md](design/design-project-instructions.md)).
`session_resumed` also carries `instructions`. `turn_start` and
`turn_end` carry `workspace` (the git summary). `model_response` carries
`request_sha256` (the exact request body), `response_id` and
`served_model`. File tools' `file` adds `before_sha256` (null: the file did
not exist) and `after_sha256`. A shell command in a git workspace adds
`workspace_changes.paths`: each path whose status or content changed, with
`status` (`committed` when it returned to the committed version),
`before_sha256` (null: as committed) and `after_sha256`; at most 1000 paths
(`truncated`). git runs with the shell's sandbox and environment, with
fsmonitor, the untracked cache, optional locks, system config and
submodules off. A stored output cut at `max_output_bytes` adds
`output_full_sha256`. An argument the model sent as the wrong JSON type and
mima converted to the type the tool declares adds `converted`: `[{arg,
from}]`, with `from` as sent (see [editing.md](editing.md#tool-arguments)).

An approval answered by a person has `actor: "operator"`, `answer` (`yes`,
`no`, `empty` for Enter, `eof` for no input; the last two deny),
`shown_sha256` (SHA-256 of the exact text displayed) and `wait_ms`.

**Session ledger.** `~/.mima/ledger.jsonl` (`[audit].ledger`, mode 0600,
appended under a lock, synced) gets one chained record per event, for every
session whether or not it is recorded:

| `type` | Fields |
|---|---|
| `session_start` | `session`, `mima_version`, `mode`, `resumed`, `cwd`, `recording` (`on`/`off`), `transcript` (absolute path or null), `user`, `uid`, `host` (unless `record_host = false`) |
| `recording_changed` | `session`, `recording`, `turn`, `transcript` |
| `session_end` | `session`, `reason`, `turns`, `records`, `head`, `transcript` |

A workspace `./agent.toml` cannot change `[audit]` (it can only turn
`required` on); it comes from the user's own config file or the defaults. A
ledger write failure is reported once and, outside required mode, does not
stop the session.

**Required mode** (`--audit`, `MIMA_AUDIT=1`, `[audit].required = true`):
the transcript is always on (mima refuses to start if it cannot be opened);
each record is synced to disk before returning, so an approval is on disk
before its tool runs; a transcript or ledger write failure stops mima
before its next action (one-shot: exit status 1; REPL: the session ends).
mima refuses to start when a sandbox path (`allowed_paths`,
`sandbox_read_paths`, `sandbox_writable_paths`) covers the transcript or
ledger directory, and warns when shell commands are not sandboxed at all.
`--resume` continues only an intact chain; `--accept-chain-break` continues
anyway and writes a `chain_reset` record (`actor: "operator"`, `reason`,
`findings`). Turning recording off is allowed in every mode and recorded.

**Verifying.** `mima --audit-verify <id|last|path> [--head HEAD]` checks,
offline, with exit status 1 if any check fails:

| Check | Fails or warns when |
|---|---|
| `lines` | a line is not JSON or the last line is incomplete (fail) |
| `chain` | a hash does not recompute, or a record after chaining began has none (fail); records written before chaining (warn) |
| `order` | `seq` does not increase by one (fail) |
| `header` | a `session_start` lacks the audit header (warn) |
| `files` | a file's state before a change differs from the state last recorded, during a turn or between turns (warn) |
| `approvals` | a tool result has no approval, or a denied call ran (fail); an approved call has no result in a completed turn (warn) |
| `attribute` | records without an actor, or unknown rule ids (warn) |
| `gaps` | recording was turned off: each unrecorded interval, with its turns (warn) |
| `complete` | no `session_end` at the end (warn) |
| `ledger` | the ledger's own chain is broken (fail) |
| `anchor` | a head the ledger recorded for this transcript is not in it (fail); records after the last recorded head (warn) |
| `head` | the head given with `--head` is not in the transcript (fail) or is not its last record (warn) |

`mima-eval` runs these checks on every trial transcript (against the
trial's own ledger), keeps the failures in `metrics.audit_failures`, and
reports the count per model.

## Privacy and security

Transcripts contain everything the agent read and ran: source code, command
output, and anything secret that passed through them. So:

- `~/.mima/` and `~/.mima/transcripts/` are created with mode `0700`, and
  transcript files with `0600`.
- Transcripts stay on the local machine. They are never sent anywhere,
  consistent with the no-telemetry guarantee.
- Nothing is deleted automatically. The README documents how to prune
  (`find ~/.mima/transcripts -name '*.jsonl' -mtime +30 -delete`); a
  `retention_days` setting can come later.
- The model cannot read transcripts: `~/.mima` is outside `allowed_paths`.
  Recall of masked outputs goes through a dedicated tool (below), not
  through file access.

## Configuration

```toml
[session]
# transcripts = false                   # record every session (default: off)
# transcript_dir = "~/.mima/transcripts"
# max_output_bytes = 1048576            # per tool output stored in the transcript
```

If a transcript cannot be opened, `mima` says so and continues without it; if
a write fails later, it warns once and keeps working. Recording failures never
stop a task.

## Integration with context management

- Each `compaction` record lists the masked `call_ids`, so the transcript
  shows exactly what the model stopped seeing, and when.
- Placeholders can then name the source, e.g. `... 18342 bytes; full output:
  call_7 in this session's transcript`.
- A later read-only `recall_output(call_id)` tool returns a stored output
  from the current session's transcript (subject to the stage 0 cap). This
  replaces "re-run the tool" in the placeholder text and avoids re-running
  commands with side effects.

## Implementation

- `src/session.rs`: `Session` (id, UTC start time, turn and record counters,
  optional transcript file), UTC formatting without extra dependencies,
  0700/0600 permissions, per-line flush, the 1 MiB output cap.
- `src/main.rs`: records at turn start, model response or error, approval,
  tool result (full output, before the stage 0 cap), loop guard, turn end and
  session end; the startup message; `--transcript` and the REPL commands.
- `src/context.rs`: records each compaction with the affected call ids.

Planned: `--resume`, `recall_output`, `mima sessions` (list), retention.
