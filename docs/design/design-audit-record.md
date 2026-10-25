# Design: Audit record

Status: implemented (`src/audit.rs`, `src/evidence.rs`; steps 1-7 below).
Reference documentation: [../sessions.md](../sessions.md), "Audit chain and
session ledger". Differences from the first draft are listed under
"Implementation notes" at the end.
Related: [../sessions.md](../sessions.md) (sessions and transcripts),
[session-resume.md](session-resume.md) (transcript schema and evolution
rules), [design-shell-sandboxing.md](design-shell-sandboxing.md).

## Summary

The transcript becomes an audit record: a complete, attributable,
tamper-evident account of what mima was asked, what it did, who or what
decided each step, and with which binary, configuration, prompt and model.

Five changes, all additive to transcript schema 1:

1. **Hash chain.** Every record carries its own `hash`: the SHA-256 of the
   previous record's hash and this record's bytes. Editing, deleting or
   reordering a record breaks the chain. The last record's hash (the
   **head**) is printed at session end and can be anchored outside the file.
2. **Attribution.** Every record says which kind of actor caused it
   (`operator`, `model`, `rule`, `tool`, `system`), and every deterministic
   decision names the rule that made it (`rule: "approval.auto_approve_bash"`).
   An approval granted by `--approve-all` is no longer recorded as if a
   person approved it.
3. **Complete header.** `session_start` records the build, the binary's
   hash, where the configuration came from and its effective values, the full
   system prompt, the tool specifications' hash, sampling settings, the
   server's model, the sandbox state, the operator and the workspace's git
   state.
4. **Evidence hashes.** SHA-256 of every full tool output (also when the
   stored copy is cut), of each file before and after a change, of each
   request sent to the model, and of files changed by shell commands.
5. **Required mode and a verifier.** With `[audit].required`, recording is
   always on, a record is on disk before the action it describes, and a
   recording failure stops the agent rather than letting it continue
   unrecorded. In every mode, turning recording off is itself recorded, so a
   gap is visible. `mima --audit-verify` checks a transcript offline.

No new crate: SHA-256 comes from `ring`, already linked through `rustls`.

## Why

mima's positioning is an auditable agent, and its transcripts already record
most events ([../sessions.md](../sessions.md)). An auditor still cannot rely
on them:

| # | Gap today | Where |
|---|---|---|
| G1 | Recording is optional, can be disabled mid-session, and a write failure lets the agent continue unrecorded ("Recording failures never stop a task"). | `src/session.rs` `record`, `disable` |
| G2 | Nothing detects an edited, removed or reordered line; `seq` only shows gaps if nobody renumbers. | transcript format |
| G3 | The header lacks the build commit, a binary hash, the configuration source and effective values, the system prompt, tool specifications, sampling settings, sandbox state, `auto_approve_sandboxed`, `--approve-all` and the operator. | `src/main.rs` `enable_transcript` |
| G4 | An approval record says what was decided but not which rule decided it. Under `--approve-all` the decision is written as `approved`, the same value as a person answering yes. | `src/agent.rs:329`, `src/presenter.rs:204` |
| G5 | File evidence is an FNV-1a hash (not collision resistant), taken only after the call and only for the three file tools. Files changed by shell commands are not recorded at all. | `src/agent.rs` `file_state` |
| G6 | A tool output larger than 1 MiB is stored cut, with no hash of the full output. | `src/session.rs` `stored_output` |
| G7 | Records do not say uniformly whether the model, a deterministic rule or a person caused them (a loop-guard nudge, a continue prompt and a compaction are all mima's code, but are not labelled as such). | record types |
| G8 | There is no tool to check a transcript. | |

G7 matters beyond auditing. mima's design keeps deterministic decisions in
code and leaves to the model only what needs language understanding (the
responsibility boundary argued for in Whitten, HICSS-60, 2027). The record
should make that boundary visible: for every step, an auditor can see whether
a rule, the model or a person decided it, and check the rule's decisions by
replaying them.

## Goals and non-goals

Goals:

- **Complete.** Every action that can change state is recorded, and in
  required mode it is on disk before the action happens.
- **Attributable.** Each record names its actor; each deterministic decision
  names its rule; each human decision records what was shown and the answer.
- **Tamper-evident.** Modification, deletion, insertion and reordering are
  detected by the chain; truncation and wholesale rewriting are detected when
  the head is anchored outside the file.
- **Reproducible inputs.** The record holds enough to rerun a session with
  the same binary, configuration, prompt, tools and sampling settings, and to
  check that a rebuilt request is byte-identical (model output is not
  deterministic; the inputs are).
- **Verifiable offline** by one command, with a machine-readable result.
- **Local and small.** No network access beyond the configured model
  endpoint, no new crates, no measurable slowdown of a step.

Non-goals:

- Certifying compliance with any regulation. The record is designed to
  support an assessment; the mapping to ALCOA+ below is a guide, not a claim.
- Protection against someone who controls the operator's account (or root)
  and rewrites the whole chain before its head is anchored elsewhere. Only an
  external anchor can detect that (see threat model); signing keys are not
  planned.
- Explaining steps (`mima explain`), human sign-off states, workflow files and
  evidence bundles. They build on this record and get their own designs; this
  design reserves header fields for them.
- Shipping logs or anchors anywhere. For now anchors are stored in a file
  (the session ledger); other destinations are later work.

## Threat model

| Event | Detected by | Notes |
|---|---|---|
| Partial write (crash, full disk) | verifier: last line incomplete | reported as incomplete, not as tampering |
| A line edited after the fact | chain: its `hash` and every later one no longer match | unless every later hash is recomputed; then only an anchor shows it |
| Recording turned off mid-session | `transcript_disabled` record and the gap report | |
| A session run without recording | session ledger | |
| A line deleted, inserted or moved | chain and `seq` | |
| The tail cut off | anchored head, or missing `session_end` | without an anchor, a cut at a record boundary only shows as "no `session_end`" |
| The whole chain rewritten by the operator's account | external anchor only | a ledger in the default location does not help here; a ledger kept where that account cannot rewrite it does |
| Metadata altered in a shared copy with content withheld | chain, with the head compared against an anchored copy | without the anchor, the sharer could recompute the chain |
| The model reads or alters the transcript | not possible by design | `~/.mima` is outside `allowed_paths`; in required mode the sandbox also denies the transcript directory to shell commands |
| A modified mima binary | `binary_sha256` in the header, compared with a known build | reproducible builds are later work (Phase 6) |

## Design

### Modes

The chain, attribution, header and evidence hashes apply to **every**
transcript; they cost little and make every recorded session checkable.

**Required mode** adds guarantees that change behavior:

| Behavior | Default | Required |
|---|---|---|
| Transcript | off unless enabled | always on |
| `/disable_transcript` | allowed and recorded (below) | allowed and recorded (below) |
| Transcript cannot be opened at start | warn, continue | refuse to start |
| A record cannot be written | warn once, continue | the pending action is not run; the turn ends with outcome `audit_failure`; the session ends |
| Order | record after the fact is acceptable | the `approval` record is written and synced (`fsync`) before the tool runs |
| Resume | continue the file | verify the chain first; refuse on a break unless `--accept-chain-break`, which writes a `chain_reset` record |
| Anchor | head printed and written to the ledger | as default; a ledger write failure stops the session |

Enabled by any of: `--audit`, `MIMA_AUDIT=1`, `[audit].required = true`.
Required mode is meant for regulated environments; the default suits
unregulated use. Both write the same record format.

**Disabling is recorded, never silent.** Turning recording off writes a
`transcript_disabled` record (it exists today) with `actor: "operator"`, the
turn, and `required` (whether required mode was on). Turning it back on
appends a new `session_start` to the same file, chained to the
`transcript_disabled` record, with `gap_after_seq` naming it. The verifier
reports each unrecorded interval (from when to when, which turns), so a gap
is visible in the record itself. Sessions that never record at all are
covered by the session ledger (see Anchors).

```toml
[audit]
# required = false               # see the table above
# ledger = "~/.mima/ledger.jsonl" # session ledger, holds each session's head; see Anchors
# record_host = true             # host name in session_start and the ledger
```

Trust: the workspace's `./agent.toml` may set `required = true` but never
`false`, like other security settings (`Security::restrict_to_untrusted`).
Today the first configuration file found wins, so a workspace file hides the
user's file entirely. For `[audit]` only, mima also reads the user's file
when a workspace file exists, and `required` is true if any source sets it.
There is no system-wide policy file.

### Record envelope

Every record gains three fields:

| Field | Meaning |
|---|---|
| `actor` | open enum: `operator` (instruction, approval answer, slash command), `model` (a completion), `rule` (a deterministic decision by mima's code), `tool` (a tool's execution and result), `system` (session lifecycle) |
| `rule` | stable rule id when a rule decided, for example `loop_guard.nudge`, `context.mask`, `approval.auto_approve_bash` |
| `salt` | 16 random bytes (hex) for content commitments (below) |
| `hash` | the record's chain hash (below) |

```json
{"actor":"rule","call_id":"call_4","decision":"auto_approved","hash":"9c41…7be2","matched":"cargo test","rule":"approval.auto_approve_bash","salt":"a01f…3c","seq":12,"ts":"2026-10-18T14:03:09.114Z","turn":2,"type":"approval"}
```

**Inline chain hash.** Each record carries its own hash, computed from the
record and the hash before it:

```
hash[0] = SHA-256( 0x00 * 32            || body[0] )
hash[n] = SHA-256( hash[n-1] (32 bytes) || body[n] )
```

Each line is self-describing: the last line's `hash` is the transcript's
**head**, readable without computing anything. Changing, deleting, inserting
or reordering any record changes every hash after it. The first record of a
file is a `session_start`, so the session id is bound into `hash[0]`. The
chain spans the whole file, across disable and enable and across resumed
continuations.

**Sharing without source code.** A transcript may be shared with the code
and other content withheld, and the shared copy must still verify. So
`body[n]` is not the raw line but the record's **disclosure form**:

- Every record has a `salt` (16 random bytes, hex).
- Fields are classified as **content** or **metadata** in one table in the
  code (`CONTENT` in `src/audit.rs`). Content is
  anything that can hold source code, secrets or prose: `instruction`,
  `content`, `reasoning`, tool call `args`, `output`, `preview`, `answer`,
  the system prompt `text`, and `context` entry `text`. Metadata is
  everything else: types, times, tool names, call ids, decisions, actors,
  rules, token counts, byte counts, paths, file hashes.
- In the disclosure form, each content field's value is replaced by
  `"c:" + hex(SHA-256(field salt || 0x00 || canonical value))`, where the
  field salt is `SHA-256(salt || field path)`; `salt`, `salts`, `withheld`
  and `hash` are left out.
- `body[n]` is the disclosure form serialized canonically: object keys
  sorted, no whitespace, minimal string escaping, integers as written, and
  floats in Rust's shortest round-trip form. mima uses its own small encoder
  for this, so a serializer update cannot change the bytes; golden tests pin
  it.

The full transcript keeps every value and the salts. `mima --audit-export
<id> --withhold <set>` writes a copy in which the chosen content fields are
already in their `c:` form (listed in `withheld`), disclosed content fields
carry their own field salt (in `salts`), and record salts are dropped, so a
withheld value cannot be guessed from a salt in the copy. Sets: `code` (all
content fields), `outputs` (tool outputs and previews, and the compaction records,
context snapshots and verifier results that quote them), or a list of
field names. The verifier computes the disclosure form from either copy and
gets the same hashes. Checked against an anchored head, a shared copy proves
its metadata (what ran, when, who approved it, which rule decided) is
unaltered, while the withheld content stays private.

The salt keeps short content from being guessed in a shared copy. Without
it, a digest of `exit: 0` or `yes` could be matched by trying likely
values. File hashes (`before_sha256`, `after_sha256`) are deliberately
unsalted metadata: anyone with a copy of the repository can confirm the
transcript's file states against it, and whole source files cannot
practically be guessed.

The owner of the full transcript can later disclose any single withheld
value with its salt, and the recipient can check it against the shared copy.

What the chain cannot do on its own: SHA-256 has no key, so someone who
edits a record can recompute every later hash, and someone who cuts records
off the end leaves a valid shorter chain. Both are caught only by comparing
the head with a copy kept elsewhere (see Anchors). This holds for any
unkeyed chain; a keyed one would need a key the editor cannot reach, and
signing keys are out of scope (see Decisions).

**Rule ids** are constants in the code and listed in a table in
[../sessions.md](../sessions.md). A test fails if the code can write an id
the table does not list. Initial ids:

| Area | Ids |
|---|---|
| Approval | `approval.read_only`, `approval.bash_not_required`, `approval.writes_not_required`, `approval.auto_approve_sandboxed`, `approval.auto_approve_bash`, `approval.approve_all`, `approval.invalid_call` |
| Tools | `tools.convert_args` (an argument converted to its declared type; `tool_result.converted`) |
| Loop guard | `loop_guard.skip_duplicate`, `loop_guard.nudge`, `loop_guard.terminate`, `loop_guard.continue`, `loop_guard.steps_left` |
| Context | `context.normalize`, `context.mask`, `context.evict` (an overflow retry is a mask or evict with reason `overflow`) |
| Session | `session.step_cap_summary` |

### Header (`session_start`)

New fields, all optional on read:

| Field | Content | Source |
|---|---|---|
| `build` | `version`, `git_commit`, `git_dirty`, `profile`, `target`, `rustc` | `build.rs` (new; uses `git` if present, else `unknown`) |
| `binary_sha256` | SHA-256 of the running executable | `std::env::current_exe()` |
| `config` | `source` (path or `defaults`), `sha256` of the file, `workspace` (bool), `ignored` (settings reset as untrusted), `env_overrides` (names of applied `MIMA_*` variables), `effective` (the effective configuration with the API key removed) | config loader |
| `system_prompt` | `sha256` and `text` (the full composed prompt; where segments came from shows in `config.effective` and `config.env_overrides`) | prompt composition |
| `tools` | `names`, `sha256` of the tool specifications as sent | tool registry |
| `sampling` | `temperature`, `top_p`, `top_k`, `min_p`, `max_tokens`, `extra_body`, `stream` | config |
| `server` | the model's `/v1/models` entry (`id`, `max_model_len`, `owned_by`, `root`); server version where the endpoint reports one | configured endpoint only |
| `sandbox` | `platform`, `available`, `tier`, `landlock_abi`, `seccomp`, `network` | sandbox probe |
| `approvals` | adds `auto_approve_sandboxed`, `approve_all` | config, CLI |
| `operator` | `uid`, `user`, `host`, `tty` (bool) | `getuid`, passwd entry, `gethostname` |
| `workspace` | `root`, `git` (`head`, `branch`, `status_sha256`, `dirty` count) or `null` | `git` (safe invocation, below) |
| `os` | `platform`, kernel release | `uname` |
| `audit` | `required`, `chain: "sha256"`, `anchor` (configured or not) | |
| `workflow` | reserved for the workflow design | |

The system prompt is stored once per `session_start` (a few kilobytes). It is
needed to reproduce a session and is not otherwise derivable when it was set
through the environment.

### Approval records

`needs_approval` returns the rule with the answer:

```rust
pub struct Policy { pub needs: bool, pub rule: &'static str, pub matched: Option<String> }
```

| Situation | `decision` | `actor` | `rule` | Extra fields |
|---|---|---|---|---|
| Read-only tool | `not_required` | `rule` | `approval.read_only` | |
| Approval for bash turned off | `not_required` | `rule` | `approval.bash_not_required` | |
| Sandboxed and `auto_approve_sandboxed` | `auto_approved` | `rule` | `approval.auto_approve_sandboxed` | |
| Allowlist match | `auto_approved` | `rule` | `approval.auto_approve_bash` | `matched` (the entry) |
| `--approve-all` (evaluation harness) | `auto_approved` | `rule` | `approval.approve_all` | |
| Person answered | `approved` or `denied` | `operator` | | `shown_sha256` (hash of the exact text displayed), `answer` (`yes`, `no`, `empty`, `eof`), `wait_ms` |
| Call cannot succeed | `not_requested_invalid` | `rule` | `approval.invalid_call` | |

`decision` is an open enum, so readers are unaffected. The `--approve-all`
change corrects G4: those decisions were never made by a person.

### Tool results and file evidence

- Tool output integrity comes from the chain, which commits to `output` (a
  content field). When the stored copy is cut at `max_output_bytes`,
  `output_full_sha256` (also content) records the SHA-256 of the complete
  output, so the original can be identified; `bytes` already records its
  length.
- File tools: `file` gains `before_sha256` and `after_sha256` (for
  `read_file`, the content read). `fnv1a64` keeps being written, because
  resume uses it (evolution rule 4).
- Shell commands in a git workspace: before and after each `execute_bash`,
  mima runs `git status --porcelain=v2 -z --untracked-files=all` and records
  `workspace_changes`: each path whose status or content changed, with its
  status and `after_sha256`. The number of paths is capped (default 1000;
  `truncated: true` beyond). The earlier content of a path is the previous
  `after_sha256` in the record, or the committed blob at `head`.
- Outside a git workspace, the header records `workspace.git: null` and
  shell side effects are not tracked. The verifier states this rather than
  passing silently.
- `turn_start` and `turn_end` record `workspace` (`head`, `status_sha256`),
  so changes made between turns (by the operator) are distinguishable from
  changes made during a turn.

**Safe git invocation.** A repository's own configuration can make `git
status` run programs (`core.fsmonitor`, for example). mima runs git with
`-c core.fsmonitor=false -c core.untrackedCache=false`, `GIT_CONFIG_NOSYSTEM=1`,
no pager, and a timeout. A git failure is recorded, not hidden.

### Model records

- `model_response.request_sha256`: SHA-256 of the exact request body sent.
  The `context` record and the stored system prompt rebuild the messages;
  this hash checks that the rebuild is exact.
- `model_response.response_id` and `served_model`, when the server returns
  them.
- Synthetic messages (nudge, continue, steps-left warning) are already
  recorded as `loop_guard` records; they gain `actor: "rule"` and their rule
  id.

### Anchors

At session end mima prints the head, the record count and the path:

```
Transcript head: sha256:9c41…7be2 (214 records) ~/.mima/transcripts/20261018T140301Z-minister_mandati-3f9a.jsonl
```

**Session ledger.** `~/.mima/ledger.jsonl` (mode 0600) gets one record per
session event, chained with the same inline `hash` scheme:

| Event | Fields |
|---|---|
| `session_start` | session id, time, `user`, `host`, `cwd`, `recording` (`on`, `off`, `required`) |
| `recording_changed` | session id, time, `on` or `off`, turn |
| `session_end` | session id, time, record count, head |

The ledger is written even when the transcript is off, so a session that ran
unrecorded still leaves a line saying it ran, where and by whom. It holds no
content (no instructions, outputs or code). It is in the same account as the
transcripts, so it catches accidental damage, casual edits and missing
transcripts, not a determined rewrite of both.

**Anchor storage: a file, for now.** The ledger is the anchor store: each
`session_end` holds the session's head. Its location is configurable
(`[audit].ledger`), so an operator can keep it apart from the transcripts,
for example on another mount or under another account's control with
append-only permissions. Other destinations (write-once storage, a ticket
system, `git notes`, a journal) are deferred; they would copy the same
`session_end` line elsewhere, so the format does not change when they are
added. If the ledger cannot be written, mima warns; in required mode it
stops, as for the transcript.

### Verifier: `mima --audit-verify`

```
mima --audit-verify <id|path|last>          # implemented
mima --audit-verify <...> --head <hex> --json   # planned
```

| # | Check | Result on failure |
|---|---|---|
| V1 | every line is JSON and newline-terminated | fail; an incomplete last line is reported as such |
| V2 | every `hash` recomputes from the previous hash and the line's body; `chain_reset` records listed | fail |
| V3 | `seq` increases by one | fail |
| V4 | header has the audit fields | warn ("written before audit records") |
| V5 | every `tool_result` follows an `approval` for its `call_id`; every allowed call has a result (a `tool_result`, or for a verifier run its `verification` record) or a cancelled turn; denied calls were not executed | fail |
| V6 | rule ids are known to this mima | warn (written by a newer mima) |
| V7 | each file's `before_sha256` equals its previous `after_sha256` in the record | warn: "changed outside the record", with the turn boundary if it happened between turns |
| V8 | `session_end` present | warn: incomplete (crash or truncation) |
| V9 | head equals `--head` or the session's `session_end` in the ledger; the ledger's own chain verifies | fail |
| V10 | each `transcript_disabled` is listed as an unrecorded interval (times, turns, whether required mode was on); the ledger's `recording_changed` events match the transcript | report; fail on a mismatch with the ledger |

Exit status 0 only when no check fails. `--json` gives one object per check.
`mima-eval` runs the verifier on every trial transcript, so every evaluation
run also tests the record at scale.

### Interaction with existing features

- **Schema.** All changes are additive under the evolution rules in
  [session-resume.md](session-resume.md): optional new fields, one new record
  type (`chain_reset`), new values for open enums. No `schema` bump. The
  closed writer schema and the released baseline are updated.
- **Resume.** Before appending, mima verifies the chain (required mode:
  refuses on a break). The continuation chains onto the file's last line;
  `session_resumed` records the head it continued from.
- **Context records.** Unchanged. Their FNV-1a `check` remains drift
  detection; integrity comes from the chain.
- **Evaluation.** Metrics are unchanged. `approved` counts under
  `--approve-all` become `auto_approved` with `approval.approve_all`.
- **Privacy.** Audit transcripts are not redacted: they hold everything the
  agent read, including any secrets that passed through, because redaction
  would break the "original record" property. The files are protected
  instead: same protections as today (0700/0600, local only, outside
  `allowed_paths`), and required mode also denies the transcript directory
  to shell commands. Sharing goes through `mima --audit-export --withhold`.

## Cost

- SHA-256 per record over a few kilobytes is negligible; a 1 MiB tool output
  is a few milliseconds (`ring` uses the ARMv8 crypto extensions on the Thor).
  To be measured on the smoke suite: added time per step and transcript size.
- `fsync` per record in required mode: about three records per step.
- Two `git status` runs per shell command in a git workspace: the main cost
  on large repositories; measured, and capped by the timeout.
- Each record is serialized twice (the line and its canonical disclosure
  form), plus one salted digest per content field.
- Binary size: `ring`'s digest code is already linked; `build.rs` adds
  nothing at run time; the canonical encoder is a few dozen lines.

## ALCOA+ mapping (informative)

ALCOA+ is the data-integrity vocabulary used by the FDA and other
regulators. How the record addresses each property:

| Property | Mechanism |
|---|---|
| Attributable | `actor`, `rule`, `operator`, approval answers |
| Legible | JSON Lines with a published JSON Schema |
| Contemporaneous | `ts` at the event; required mode writes before acting |
| Original | full outputs stored, with hashes of the originals when cut |
| Accurate | hashes of outputs, files and requests; the verifier |
| Complete | required mode; V5, V8, V10 |
| Consistent | `seq`, the chain, UTC timestamps |
| Enduring | local files, never deleted automatically |
| Available | offline verifier, documented format |

This is not a compliance claim. For example, 21 CFR Part 11 also requires
access controls, system validation and electronic signatures that state the
signer, the time and the meaning, which belong to the sign-off design.

## Alternatives considered

- **HMAC per record with a local key.** The key sits in the same account as
  the file, so it adds key management without detecting more than the chain.
- **Signing (Ed25519).** Cheap with `ring`, but useful only with a key held
  by another principal (a hardware token, a helper running as another user).
  Not planned (see Decisions); the external anchor covers the same need
  without key management.
- **Chain over raw line bytes.** Simpler (no canonical encoder), but removing
  any content from a line breaks the chain, so a transcript could not be
  shared without its source code.
- **Merkle tree (as in Certificate Transparency).** Enables proofs that one
  record belongs to a log without revealing the others. Partial disclosure is
  needed at the level of fields within records, which salted field
  commitments in a linear chain provide; a tree adds nothing for a single
  local writer.
- **Forward-secure sealing** (as in the systemd journal). Protects records
  written before a compromise. Needs key evolution and an offline
  verification key; a candidate for later.
- **A separate audit log file.** Two files can disagree; the transcript is
  already the single source of truth (session-resume rule 12) and is locked
  while open.
- **RFC 8785 (JSON Canonicalization Scheme) as the canonical form.** Close
  to what mima's encoder does; its number rules follow JavaScript's
  formatting, which matters only for floats. mima's own encoder is smaller
  and pinned by golden tests; adopting RFC 8785 exactly remains possible if
  outside tools need to verify transcripts.
- **journald or syslog only.** Not portable to macOS and records less; an
  they can be added later as anchor destinations.
- **Committing transcripts to git.** Heavy, mixes the record into the
  repository it describes, and can be rewritten.

## Implementation plan

Each step lands with its tests; none changes what the agent does in a task,
so no pass-rate A/B is needed. Overhead is measured on the smoke suite.

1. **Chain and verifier skeleton.** `salt` and inline `hash` on every
   record, the content-field table, the canonical encoder with golden tests,
   SHA-256 via `ring`, head at session end, the session ledger, `mima
   --audit-verify` with V1-V3, V8, V9. Tests: flip a byte, delete, insert, swap and
   truncate lines, forge an appended line, resume continuation, crash with a
   partial line, every string field classified as content or metadata.
2. **Attribution.** `actor` and `rule` on every record; `Policy` with rule
   ids and the matched entry; the `--approve-all` correction; operator answer,
   shown text hash and wait time; rule-id table test; V5, V6.
3. **Header.** `build.rs`, binary hash, configuration provenance and
   effective values, system prompt, tool specification hash, sampling,
   server, sandbox, operator, workspace, OS; V4.
4. **Evidence.** `output_full_sha256`, file hashes before and after, shell
   workspace changes with safe git invocation, workspace state at turn
   boundaries, `request_sha256`; V7.
5. **Required mode and disabling.** Flag, environment and configuration with
   trusted-source resolution, fail closed, `fsync`, recorded disable and
   re-enable with `gap_after_seq`, ledger `recording_changed` events, chain
   check before resume, `chain_reset`, configurable ledger location; V10.
6. **Export.** `mima --audit-export <id> --withhold code|outputs|<fields>
   [--output FILE]`, single value disclosure (another export that withholds
   less), and verification of exported copies.
7. **Integration.** `mima-eval` verifies every trial transcript and reports
   failures; [../sessions.md](../sessions.md) record tables, rule-id table,
   content-field table and an audit section in the README.

## Decisions

Made with the project owner on 2026-10-17:

1. **Both regulated and unregulated use.** The default mode serves
   unregulated work; required mode serves regulated work. No specific
   standard is targeted; ALCOA+ is the checklist.
2. **No system-wide policy file.** Audit settings come from the flag, the
   environment and the user's configuration; a workspace file can only turn
   required mode on.
3. **Disabling is allowed and recorded,** in every mode, in the transcript
   and in the session ledger, and the verifier reports each gap.
4. **Inline chain hash:** each record holds the hash of the previous hash and
   itself, so the head is the last line's `hash`.
5. **No signing keys.**
6. **No redaction** of audit transcripts; the files are protected instead.
7. **The host name is recorded** by default.
8. **Transcripts can be shared without source code,** via salted field
   commitments and `mima --audit-export --withhold`.
9. **Anchors are stored in a file** (the session ledger, location
   configurable). Other storage is later work.
10. **File paths are metadata,** not withheld in shared copies.

## Open questions

None at present. Later work: anchor destinations beyond a file.

## Implementation notes

Where the implementation differs from the first draft of this design:

- **Command names.** `mima audit verify` and `mima audit export` became the
  flags `--audit-verify`, `--head`, `--audit-export`, `--withhold` and
  `--output`, because mima takes free-text instructions as positional
  arguments.
- **Per-field salts.** One salt per record would have to travel with a copy
  that discloses some of a record's content fields and withholds others,
  and would expose the withheld ones to guessing. Each field's salt is
  derived from the record salt; a copy carries only the disclosed fields'.
- **V10** reports each unrecorded interval. Comparing the ledger's
  `recording_changed` events with the transcript is not implemented.
- **Rule ids** for overflow retries and the output cap were dropped: an
  overflow retry is a mask or evict record (reason `overflow`), and the cap
  is not a record of its own.
- **`--approve-all`** moved from the presenter into the approval policy
  (`[security].approve_all`, never read from a file), so it is recorded as
  `auto_approved` with `approval.approve_all`.
- **`MIMA_AUDIT`** applies however the configuration was found (also with
  `--config`), since it only tightens.
- **Required mode and the sandbox.** mima refuses to start when a sandbox
  path covers the transcript or ledger directory; with the sandbox off it
  warns rather than refusing, and the header records `sandbox.active`.
- **git** runs through the shell's sandbox and environment (the tool
  registry's `CommandRunner`), so a workspace's `.git/config` gains nothing
  it could not already do through `execute_bash`.
- **Session order.** `last` and session listings now order by the recorded
  start time (milliseconds), so sessions begun in the same second resolve
  correctly.

## References

- B. Schneier, J. Kelsey, "Secure Audit Logs to Support Computer Forensics",
  ACM TISSEC 2(2), 1999.
- S. Crosby, D. Wallach, "Efficient Data Structures for Tamper-Evident
  Logging", USENIX Security 2009.
- RFC 6962 and RFC 9162, Certificate Transparency.
- G. Marson, B. Poettering, "Practical Secure Logging: Seekable Sequential
  Key Generators", ESORICS 2013 (the basis of systemd journal sealing).
- RFC 8785, JSON Canonicalization Scheme.
- NIST SP 800-92, *Guide to Computer Security Log Management*, 2006.
- U.S. FDA, *Data Integrity and Compliance With Drug CGMP: Questions and
  Answers*, guidance, 2018 (ALCOA); 21 CFR Part 11, section 11.10(e).
- in-toto attestation framework and SLSA provenance (build metadata).
- P. Whitten, "Deterministic-Dimension Reduction for Reliable LLM Agents: A
  Design-Science Study of Spatial Construction and Edge Deployment",
  HICSS-60, 2027.
- `ring`: https://docs.rs/ring
