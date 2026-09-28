# Sessions and transcripts

Status: implemented (sessions, transcripts, `/new`, `/session`,
`/enable_transcript`, `/disable_transcript`, `--transcript`). Resume and
recall are planned.

A **session** is one continuous conversation between an operator and `mima`,
with one context (history, token ledger, budget) and, when enabled, one
**transcript**: an append-only, timestamped record of everything that
happened, saved under `~/.mima/transcripts/`.

**Transcripts are off by default.** `mima` states the transcript state when it
starts:

```
Transcripts: off. Enable with /enable_transcript.     (interactive)
Transcripts: off. Enable with --transcript.            (one-shot, on stderr)
Transcripts: on (/home/user/.mima/transcripts/20260927T143205Z-cli_agent-3f9a.jsonl)
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

A session never spans processes. Two `mima` processes running at once have
two sessions and two files; no locking is needed.

## Session id and file name

```
~/.mima/transcripts/20260927T143205Z-cli_agent-3f9a.jsonl
                    └── start time ──┘ └ label ┘ └┘ 4 hex chars of randomness
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
`ts`, `type`, and `seq` (a per-session counter, to detect gaps or reordering).

```json
{"seq":0,"ts":"2026-09-27T14:32:05.123Z","type":"session_start","schema":1,"session":"20260927T143205Z-cli_agent-3f9a","session_started":"2026-09-27T14:32:05.120Z","turns_before":0,"mima_version":"0.1.0","mode":"interactive","cwd":"/home/user/cli_agent/src","model":"nvidia/NVIDIA-Nemotron-3-Super-120B-A12B-NVFP4","base_url":"http://thor.local:8000/v1","window":32768,"budget":27689,"approvals":{"bash":true,"writes":true,"auto_approve_bash":["cargo test"]}}
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
| `session_start` | recording begins (at session start, or when enabled later) | `schema`, `session`, `session_started`, `turns_before`, `mima_version`, `mode`, `cwd`, `model`, `base_url`, `window`, `budget`, `approvals`, `allowed_paths`, `token_counting` (`tokenize` or `usage+estimate`) |
| `turn_start` | an instruction arrives | `turn`, `instruction` |
| `model_response` | a completion returns | `turn`, `step`, `duration_ms`, `content`, `tool_calls`, `usage` (server-reported), `counted_prompt_tokens` and `count_source` (the count before sending; see [context.md](context.md)) |
| `model_error` | a completion fails | `turn`, `step`, `error`, `overflow` (bool), `retry` |
| `approval` | before a tool runs | `call_id`, `decision`: `not_required`, `auto_approved`, `approved`, `denied`, `skipped_duplicate`, `not_requested_invalid` (the call could not succeed, so nothing was asked); `preview` (the diff shown, for edits and overwrites) |
| `tool_result` | a tool finishes | `call_id`, `tool`, `duration_ms`, `failed`, `bytes`, `sent_bytes` (after the stage 0 cap), `output` (full) |
| `compaction` | a context stage acts | `stage` (`normalize`, `mask`, `evict`), `reason`, `call_ids`, `messages_removed`, token estimates before and after |
| `loop_guard` | a nudge or stop | `action`, `repeats`, `tool` |
| `turn_end` | a turn finishes | `turn`, `outcome` (`answered`, `loop_guard`, `step_cap`, `cancelled`, `error`), `answer`, `tokens` |
| `transcript_disabled` | `/disable_transcript` | `turns` |
| `session_end` | session ends | `reason` (`exit`, `eof`, `new`, `task_done`, `interrupted`, `fatal`), `turns`, `tokens`, `error` |

Readers must ignore unknown fields and unknown types, so later schema
versions stay backward compatible; `schema` increments only for breaking
changes.

### What is recorded

- The **full** tool output, not the capped copy the model saw (`bytes`
  versus `sent_bytes` shows the difference). A single output is stored up to
  1 MiB; beyond that it is cut with a marker, to keep one runaway command
  from filling the disk.
- Final answers in full. Streamed chunks are not recorded individually.
- Never the API key. `base_url` is recorded; the key is not.

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
