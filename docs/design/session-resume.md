# Design: Session resume and context persistence

Status: implemented (`--sessions`, `--resume`, `/sessions`, `/resume`;
`src/transcript.rs`, `schemas/transcript.schema.json`).
Related: [../sessions.md](../sessions.md) (sessions and transcripts),
[../context.md](../context.md) (context management).

## Summary

A recorded session can be resumed: `mima --resume <id|last>` (or `/resume` in
the REPL) rebuilds the context the model last saw and continues the
conversation, appending to the same transcript file.

The context is persisted **by reference, inside the transcript**. At each
turn end and after each compaction, mima appends a small `context` record that
lists, in order, which earlier transcript records make up the model's current
messages and how each one is shown (full, capped, masked, and so on). Resuming
loads the latest `context` record and dereferences it against the same file.
If that record is missing or unusable, mima **replays** the transcript
instead.

The format is defined by **JSON Schema (draft 2020-12), generated from mima's
Rust types**, and changes follow the evolution rules below, checked by
`cargo test`. mima gains no new runtime dependency.

## Goals and non-goals

Goals:

- Continue a recorded conversation after mima exits, crashes, or is
  interrupted.
- Restore what the model saw as exactly as practical, without storing the
  conversation twice.
- Keep transcripts readable by every later mima, and keep old mima working
  with newer files where possible.
- Stay auditable: plain JSON Lines, a documented schema, no hidden state.

Non-goals:

- Resuming sessions that were never recorded (transcripts are off by
  default). There is nothing to rebuild from, and that is acceptable.
- Bit-exact continuation. The system prompt comes from the current config,
  the server's prompt cache starts cold, and files may have changed on disk.
- Resuming in the middle of a turn. An interrupted turn is rolled back, as it
  is today.

## User-facing behavior

| Command | Effect |
|---|---|
| `mima --sessions` / `/sessions` | List recorded sessions, newest first: id (UTC start, project label), start time, turns, first instruction, model, how it ended (`exit`, `eof`, `interrupted`, `fatal`, still open) |
| `mima --resume <id\|last>` | Rebuild the session's context and open the interactive prompt; with an instruction, run it first (like `-i`) |
| `/resume [id]` | End the current session and continue the given one (default: the last) |

On resume, mima prints how the context was rebuilt (`from its last context
record` or `by replaying the transcript`, with the reason), notes when the
model or working directory differs from the original, and lists files that
changed on disk since the session last read or wrote them. The same
information goes into the `session_resumed` record. The model is not told
separately: stale-file protection already makes it re-read any file before
changing it.

Resuming requires recording: the continuation is appended to the original
transcript file, so `--resume` turns transcripts on for that session.

## Design

### Where the continuation goes

The resumed conversation is **appended to the same transcript file**, after a
`session_resumed` record. One conversation stays one file, timestamps show
the gap, and every reference in a `context` record points into the same file.
This relaxes the rule in `sessions.md` that a session never spans processes:
a session may span several processes, one at a time. Two processes must not
append to one transcript at once, so resume takes an exclusive advisory lock
on the file (`flock`) and refuses if another mima holds it.

### The `context` record

Written at each turn end (`cause: "turn_end"`) and after each compaction
(`cause: "compaction"`), after the transcript records it refers to:

```json
{"seq":57,"ts":"2026-09-30T10:12:03.004Z","type":"context","context_schema":1,"turn":3,"cause":"turn_end","window":32768,"budget":27689,"tokens":14210,"count_source":"tokenize","masked_total":2,"evicted_total":0,"entries":[{"seq":1},{"seq":2},{"seq":4,"view":"masked","text":"[output elided to save context: read_file {\"path\":\"src/tools/mod.rs\"}, 9120 bytes. Re-run the tool if you need it again.]"},{"seq":9},{"seq":11,"view":"capped","cap_bytes":16384},{"role":"user","view":"synthetic","text":"A repeated identical action was detected ..."}],"check":{"messages":14,"chars":55210,"fnv1a64":"9f3c2a1b7d4e5f60"}}
```

Each entry is one message of the request, in order:

| Message | Source record (`seq`) | Rebuilt as |
|---|---|---|
| system prompt | none | always first; composed from the current config (not an entry) |
| instruction | `turn_start` | user message with `instruction` |
| assistant with tool calls | `model_response` | assistant message with `content` and `tool_calls` |
| tool result | `tool_result` | tool message with `output` |
| final answer | `turn_end` | assistant message with `answer` |
| loop-guard nudge, "not executed" results | none | `view: "synthetic"`, with `role`, `text` and, for tool results, `call_id` |

`view` says how the model's view differs from the record. It is a closed enum
(rule 8): `full` (default), `capped` (tool output cut to `cap_bytes` by the
stage 0 rule), `masked` (body replaced by `text`, the placeholder),
`noted` (record text plus `text` appended: the eviction note on the first
instruction), `synthetic` (no record). Evicted messages are simply absent.

`check` detects drift between what was written and what a later mima
rebuilds (for example if the capping code changes): the message count, total
characters, and an FNV-1a 64 hash of the rebuilt messages, excluding the
system prompt. FNV-1a is about ten lines in mima; the standard library's
hasher is explicitly not stable across Rust releases. It is drift detection,
not security.

A record is a few kilobytes: placeholders dominate, and message bodies are
never copied.

### Additions to existing records

All additive under the rules below (no version bump):

- `tool_result.cap_bytes`: the stage 0 cap in effect, so a capped output can
  be rebuilt exactly (today only the resulting `sent_bytes` is recorded).
- `tool_result.file` on `read_file`, `edit_file` and `write_file` results:
  `{path, fnv1a64}` of the file's content after the call. Resume compares it
  with the file on disk to list files changed since.
- `session_resumed` (new record type): `method` (`context` or `replay`),
  `reason` for a fallback, `from_seq` of the context record used, mima
  version, model and working directory, and the list of changed files.

### Resume algorithm

```
open the transcript; take an exclusive lock (refuse if held)
read session_start: transcript `schema` newer than this mima -> refuse
                    (listing still works)
find the last `context` record
  none                                  -> replay
  context_schema missing or not integer -> replay
  context_schema newer than known       -> replay ("context record vN is newer")
  context_schema older than known       -> convert if a converter exists, else replay
  deserialize fails                     -> replay
  any entry: unknown view, dangling seq,
    or a record of an unexpected type   -> replay
  records after it other than session
    and file bookkeeping (a turn that
    started after the last context)     -> the turn never finished: rebuild
                                           from the context, drop the rest
  rebuild; recompute check
  mismatch                              -> replay (log which part differed)
  otherwise                             -> use the rebuilt context
then: new system prompt from the current config, budget pass for the current
model window (compaction may run), file-change report, session_resumed record
```

Every fallback is logged and recorded in `session_resumed`, so the audit trail
shows how the context was rebuilt.

### Replay (the fallback)

Replay rebuilds the conversation from the event records, then runs the normal
budget pass once:

1. Walk records in order. `turn_start` adds the instruction; `model_response`
   adds the assistant message; `tool_result` adds the result (full output,
   capped with its recorded `cap_bytes`); `loop_guard` with `action: nudge`
   adds the fixed nudge text after that step's results; `turn_end` with an
   answer adds it.
2. A turn that ended `cancelled`, or never ended, is dropped entirely, as the
   live rollback did.
3. `compaction` records are skipped; the budget pass decides afresh for the
   current window, which may differ from the original.
4. Where recording was switched off (`transcript_disabled` ... next
   `session_start`), a user message notes that part of the conversation was
   not recorded.
5. Normalization repairs any unanswered tool calls, as before every request.

Replay makes no model calls. It is linear in the transcript size, and the
number of past compactions does not matter.

### Files

Replay and context records restore what the model *saw*; files are never
re-read from disk into history. The read tracker (stale-file protection,
[../editing.md](../editing.md)) starts empty after resume, so the model must
re-read a file before editing it. The file-change report tells the operator
which files differ from what the session last saw.

## Schema technology

**Choice: JSON Schema (draft 2020-12), generated from the Rust serde types
with `schemars`, committed as `schemas/transcript.schema.json`.**

- The Rust types are the single source. The committed schema is the
  human-readable contract, the diff reviewed in pull requests, and the
  document other tools (`jq`, Python) can read. It covers every transcript
  record type as a `oneOf` discriminated by `type`.
- **No new runtime dependency.** Reading and writing stay `serde` +
  `serde_json`. `schemars` and a JSON Schema validator (`jsonschema` without
  default features, or the lighter `boon`) are **test-only**, attached with
  `#[cfg_attr(test, derive(schemars::JsonSchema))]`; a prototype confirmed the
  release dependency tree is unchanged.
- **mima never validates at runtime.** A prototype showed a schema generated
  for version 1 rejects a newer record (an unknown enum value) that the
  tolerant serde reader handles correctly. So the rule is **closed for
  writers, open for readers**: tests validate everything mima writes against
  a closed copy of the schema (`additionalProperties: false`), while readers
  ignore what they do not know.
- Schema-level extensions `x-mima-enum-kind` (open or closed per enum) and
  `x-mima-retired` (retired names) are read only by the CI rule check;
  validators ignore unknown keywords.

## Evolution rules

Compatibility mode:

- **Within a major version: FULL_TRANSITIVE under tolerant-reader
  semantics.** Every mima that knows major N reads every record of major N,
  written by older or newer mima.
- **Across majors, transcript records: BACKWARD_TRANSITIVE.** A mima that
  writes major N reads all majors up to N (transcripts are the audit record).
  A transcript with a newer major is listed but not resumed.
- **Across majors, `context` records: no compatibility, with fallback.** An
  unknown `context_schema` means "ignore the record and replay".

Versions are plain integers: `schema` (transcript major, in `session_start`)
and `context_schema` (on `context` records), bumped independently and only
for breaking changes. Additive changes bump nothing.

Rules (for every record type; "field" includes nested fields):

1. **Tolerant reader.** Readers ignore unknown fields and unknown record
   types. Read types never use `deny_unknown_fields`.
2. **Additive by default.** A new field is allowed within a major only if an
   older reader that ignores it still behaves correctly; otherwise it is a
   breaking change (rule 10).
3. **New fields are optional on read.** Fields added after a major's first
   release have a default (`#[serde(default)]` or `Option`) meaning "the old
   behavior".
4. **Required fields are written for the life of the major.** A field a
   released reader needs without a default keeps being written, with the same
   meaning.
5. **Never change a field's type or meaning** (including units). Add a new
   field under a new name instead, and keep writing the old one until the next
   major.
6. **Never reuse a name.** Removed fields and record types are listed in
   `x-mima-retired`, and CI rejects reuse.
7. **Renames are add, then deprecate.** Write the new name and keep writing
   the old one for the rest of the major; readers accept both
   (`#[serde(alias)]`). Remove the old name only at a major bump, then retire
   it.
8. **Enums are declared open or closed.** Open enums (`outcome`, `decision`,
   `reason`, `stage`, `cause`) may gain values within a major; readers map
   unknown values to an `Unknown` variant and treat them as informational.
   Closed enums (`view`) are needed to rebuild the context; a new value is a
   breaking change, and a reader that meets an unknown value falls back
   rather than guessing. Enum values are never removed or renamed within a
   major.
9. **Prefer enums to booleans for states that may grow** (`view: "masked"`
   rather than `masked: true`, so a `summarized` view can be added later).
10. **A breaking change bumps the integer.** Transcript-wide: bump `schema`
    and add a converter from the previous major. `context`-only: bump
    `context_schema` (older records then fall back to replay). Each bump adds
    a fixture directory and a changelog entry in `sessions.md`.
11. **Integers stay JSON numbers** (`seq`, token and byte counts), not quoted
    strings; they stay far below 2^53.
12. **The transcript is the source of truth.** A `context` record is a cache
    and is never the only place something is recorded, so ignoring it can
    never lose information.

## Checks (all in `cargo test`)

| Check | Catches |
|---|---|
| **Schema is current**: generate with `schemars`, compare with the committed file (`MIMA_BLESS=1` rewrites it) | Format changed without the contract changing |
| **Writer output validates** against a closed copy of the schema | Undocumented or mistyped fields |
| **Golden fixtures** per released version (`tests/fixtures/transcripts/s{schema}-c{context_schema}/`), never edited; each parses, and each `context` rebuilds to a committed expected message list | Newer mima cannot read older files |
| **Future fixtures**: unknown fields, record types and open-enum values, `context_schema: 999`, an unknown `view`, a dangling `seq`; assert tolerant parsing and the specific fallback reason | Readers became strict; fallback paths untested |
| **Schema diff against the last release**: a small rule checker enforcing rules 3-8 (no removed or retyped fields, no reused retired names, closed enums unchanged, open enums only growing) unless the version was bumped | Breaking change without a version bump |
| **Round trip**: parse, serialize, parse again is stable; injecting an unknown key does not change the parsed value | serde attribute mistakes |
| Optional, on release tags: run the previous release's fixture test against files written by the new build | Old binary reading new files |

## Alternatives considered

**Replay the transcript only (no persisted context).** Rebuild the
conversation from the event records and re-run compaction for the current
window. Cheap (local parsing, no model calls), uses the single documented
record, needs no new record type, and adapts to a larger or smaller window.
But it is not an exact restore: compaction may hide different outputs than
the model last saw. **Kept as the fallback**, and it is what makes the
`context` record safe to ignore.

**Re-read files from disk during replay.** Rejected: history would claim the
model saw content it never saw, including its own later edits appearing in an
earlier read. Instead, the transcript's recorded outputs are replayed as they
were, and file hashes let resume report which files changed since.

**Persist the last n full contexts** (every LLM interaction, keeping a few).
Exact and trivial to load, but it duplicates the conversation outside the
transcript (two sources of truth that can disagree, and one more copy of
sensitive content), stores placeholders so masked outputs can never be
recovered even with a larger window, and serializes internal state (pinned
positions, masking flags, estimator anchors) that would break across mima
upgrades. Only turn boundaries matter anyway, since an interrupted turn is
rolled back. **Rejected.**

**The same snapshots under a versioned schema with evolution rules.** Fixes
the upgrade fragility, but not the duplication or the lost outputs, and a
schema cannot stop a field's *meaning* going stale when compaction rules
change. The idea of a schema with evolution rules was kept and applied to the
reference record instead. **Rejected in favor of the reference record.**

**Context record by reference (chosen).** Exact restore, a few KB per record,
content stored once, masked outputs recoverable from the full outputs the
references point to, and a clean fallback to replay.

**A new transcript file per resumed session, linked to the original.** Keeps
"a session never spans processes", but later resumes would have to follow
references across a chain of files. **Rejected** for appending to the same
file under a lock.

**Record a hash of the system prompt** to warn when it changed. Not needed:
the system prompt is rebuilt from the current config by design.

**Resume unrecorded sessions.** Not possible (nothing to rebuild from) and
not required.

### Schema technologies considered

| Option | Why not chosen |
|---|---|
| Protocol Buffers (proto3) with ProtoJSON | Its JSON mapping rejects unknown fields by default and writes 64-bit integers as strings; needs `protoc` or `protox` and a `build.rs` step, which adds build and audit surface |
| Apache Avro | Its JSON encoding wraps union values (`{"string": "a"}`), which is awkward in human-readable JSONL; the Rust crate does not implement JSON encoding |
| CDDL (RFC 8610) | Concise and fits JSON, but the Rust tooling only validates: no derive from types, no compatibility checking |
| Cap'n Proto, FlatBuffers | Binary formats; transcripts must stay human-readable JSON |
| TypeSpec, Smithy | Good IDLs, but add a non-Rust toolchain to generate what `schemars` derives from the existing types |
| serde structs only, no schema | No reviewable contract and nothing to check breaking changes against |
| Existing JSON Schema diff tools (e.g. `json-schema-diff`) | Draft-07 oriented and work in progress by their own description, and they encode validator semantics (adding an enum value counts as safe), which is wrong for closed enums here; a small in-repo rule checker is used instead |

The evolution rules draw on protobuf's rules for updating message types
(never reuse field numbers or names; `reserved`), Avro's schema resolution
(reader and writer schemas, defaults for missing fields), Confluent Schema
Registry's compatibility modes (BACKWARD, FORWARD, FULL and their transitive
forms), Fowler's Tolerant Reader, and API versioning practice (Stripe,
JSON:API). SchemaVer (MODEL-REVISION-ADDITION) was reviewed and not adopted:
its REVISION level exists for data written by other parties that may conflict
with a new schema, which cannot happen when mima is the only writer.

## Implementation plan

1. Transcript additions: `tool_result.cap_bytes`, `tool_result.file` hashes,
   typed record structs with `schemars` derives (test-only), the committed
   schema, and the schema-is-current, writer-validates and round-trip checks.
2. `context` record writing at turn end and after compaction, with `check`.
3. Session listing (`--sessions`, `/sessions`).
4. Resume from a `context` record, the replay fallback, locking, the
   file-change report, `session_resumed`, and golden and future fixtures.
5. The schema-diff rule check, with the first released schema copied to
   `schemas/released/`.

## Open questions

- **Derived text drift.** `full` and `capped` entries depend on the
  message-building and capping code. The `check` catches drift and falls back
  to replay; if drift proves common, more text can be stored inline.
- **Key order and golden files.** Typed structs serialize in declaration
  order; records built as `serde_json::Value` depend on serde_json's
  `preserve_order` feature. Moving records to typed structs makes golden
  files byte-stable.
- **`#[serde(other)]` on plain string enums** works in the current serde but
  goes beyond what serde documents; a fixture test pins it.
- **Validator choice** (`jsonschema` or `boon`) is test-only and does not
  affect the binary.

## References

- Protocol Buffers, *Updating a message type* and *Proto best practices*:
  https://protobuf.dev/programming-guides/proto3/,
  https://protobuf.dev/best-practices/dos-donts/; ProtoJSON:
  https://protobuf.dev/programming-guides/json/
- Buf, breaking change detection: https://buf.build/docs/breaking/
- Apache Avro specification, schema resolution:
  https://avro.apache.org/docs/++version++/specification/
- Confluent, schema evolution and compatibility:
  https://docs.confluent.io/platform/current/schema-registry/fundamentals/schema-evolution.html
- R. Yokota, *Understanding JSON Schema Compatibility* (2021):
  https://yokota.blog/2021/03/29/understanding-json-schema-compatibility/
- M. Fowler, *Tolerant Reader*: https://martinfowler.com/bliki/TolerantReader.html
- Snowplow, *Introducing SchemaVer*:
  https://snowplow.io/blog/introducing-schemaver-for-semantic-versioning-of-schemas
- Stripe, *API versioning*: https://stripe.com/blog/api-versioning
- JSON:API specification: https://jsonapi.org/format/
- JSON Schema 2020-12: https://json-schema.org/specification
- RFC 8610, *Concise Data Definition Language (CDDL)*
- `schemars`: https://graham.cool/schemars/; `json-schema-diff`:
  https://github.com/getsentry/json-schema-diff
