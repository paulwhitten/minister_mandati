# Design: Context reporting (`/context-status`)

Status: implemented (`src/context_report.rs`, `AgentContext::report` in
`src/context.rs`), 2026-10-19. Differences from the proposal below:

- The "Compaction records" row and record count arrived with the digest
  records of [design-context-management.md](design-context-management.md);
  messages carry a `kind` (request, notice, record), so notices are no
  longer told apart by their text.
- No turn column: messages do not record their turn, and counting requests
  is wrong once early ones are evicted. Rows show the message index.
- With an anchored estimate the counting line says "plus an estimate for
  changes since", without a message count.
- The tool-output hint names the masking threshold (no `/compact` yet).
- Rendering lives in `context_report.rs` with the data types, not in
  `main.rs`, so it is unit tested; the transcript `usage` object is not
  added yet.
Related: [../context.md](../context.md) (budget, stages, counting),
[design-context-management.md](design-context-management.md) (compaction
records and `/compact`),
[design-project-instructions.md](design-project-instructions.md) (project
instructions in the system prompt).

## Summary

A REPL command, `/context-status`, shows how full the context is and what
fills it: the window and budgets, the tokens in use against each limit, a
breakdown by category (system prompt, project instructions, tool
definitions, operator requests, assistant messages, tool outputs in full and
masked, compaction records, mima's notices), the largest items, the
compaction thresholds, and what has been masked, evicted or summarized so
far. Every number says how it was counted. `/context-status messages` lists
every message in the context. The existing one-line `/context` becomes an
alias.

## Why

Today `/context` prints one line: window, usable, budget, tokens, count
source, messages, masked and evicted counts. It does not say what the tokens
are spent on, when compaction will start, or what was lost. The questions an
operator actually has are "how close am I to compaction", "what is taking the
room", and "what did the model stop seeing".

How other agents answer them (details at pinned commits in the private
research note `context-status-and-compact-commands.md`):

| Agent | Command | Breakdown | Percent of |
|---|---|---|---|
| Claude Code | `/context` | by category, with suggestions | full window (status line) |
| Letta Code | `/context` | system, memory, tools, messages, summary; labeled "estimated" | configured window |
| Aider | `/tokens` | system, history, repository map, each file; labeled "approximate" | none (remaining shown) |
| Roo Code | header bar | used, reserved for output, available | usable input (header), window (bar) |
| Codex | `/status`, footer | none | window less a fixed baseline |
| Gemini CLI, OpenCode, goose, Cline | footer or bar | none | window |

Observations that shape this design:

- Each agent uses a different basis for its percentage and few say which.
  mima has three limits (window W, usable U, operating budget E) and
  compaction thresholds as fractions of E, so an unlabeled percentage would
  mislead.
- Only one agent (goose desktop) shows the point where automatic compaction
  starts. That is the number an operator most needs.
- Category breakdowns (Claude Code, Letta Code, Aider) are estimates even
  where totals are exact, and the better ones say so.
- Showing what was removed is rare; mima already records it per event.

## Goals and non-goals

Goals:

- One view that answers: how full, of what, against which limit, when the
  next compaction happens, and what has been compacted.
- Each number labeled with its basis (E, U or W) and its source (exact or
  estimate).
- No extra model or server calls: the command reads mima's own state.

Non-goals:

- Exact per-category token counts. They would need a local tokenizer and the
  model's chat template, which mima avoids (see [../context.md](../context.md),
  "Counting tokens").
- A graphical bar or TUI panel; the plain output can feed one later through
  the `Presenter`.
- Changing what is compacted (that is
  [design-context-management.md](design-context-management.md)).

## Command

```
/context-status            the report below
/context-status messages   one row per message in the context
/context                   alias of /context-status
```

REPL only, between turns, like all slash commands. It works before the first
request (counts are then estimates; the report says so).

## Output

Example at a 32k window with the default reply reserve (4,096 tokens):

```
Context: session 20261019T140211Z-cli_agent-3f9a, turn 4, model Qwen3.6-35B-A3B

  Window W 32,768 (from server) = usable U 27,689 + reply reserve 4,096 + safety margin 983
  Budget E 27,689 (= U; set [context].budget to run lower)
  In use   14,212 tokens: 51% of E, 43% of W
           [####################--------------------]  masking starts at 60% of E
  Counted  exact: server /tokenize for the last request, plus an estimate for 2 messages since

  Category                          ~Tokens  Share  Items
  System prompt                       1,180     8%  1
  Project instructions                  410     3%  AGENTS.md (1.6 KiB)
  Tool definitions                    1,960    14%  9 tools
  Operator requests                     320     2%  4
  Assistant text and tool calls       2,340    16%  23 messages, 31 calls
  Tool outputs, full                  7,190    51%  11 (2 capped)
  Tool outputs, masked                  430     3%  14 placeholders
  Compaction records                    382     3%  1
  mima notices                            0     0%  0
  Category values are estimates, scaled so they add up to the total.

  Room
    until masking (60% of E)          2,401
    in budget E                      13,477

  Largest items
    2,180  tool output  read_file {"path":"src/context.rs"} (capped), turn 4
    1,410  tool output  execute_bash {"command":"cargo test context"}, turn 4
      890  tool output  read_file {"path":"src/budget.rs"}, turn 4

  Compaction
    Thresholds: mask at 16,613 (60% of E) down to 11,075 (40%); newest output
      protected up to 6,922 (25%); each tool output capped at 2,215
    Summaries: manual (/compact); automatic summarizing is off
    So far: 14 outputs masked in 3 events; 0 messages evicted; 1 compaction record
    Last: turn 3, /compact (summary): 18,940 -> 6,215 tokens

  Hint: tool outputs are most of the context; /compact frees room now.
```

`/context-status messages`:

```
   #  turn  role       view      ~tokens  content
   0     -  system     full        1,590  system prompt 1,180 + project instructions 410
   1     1  user       full           85  "Fix the off-by-one in ring_push and add a test"
   2     2  user       full           60  "Now run the full test suite"
   3     3  user       record        382  compaction record 1 (summary, 38 messages)
   4     4  user       full           75  "Why does test_wrap still fail?"
   5     4  assistant  full          140  read_file x2
   6     4  tool       capped      2,180  read_file {"path":"src/context.rs"}
   7     4  tool       full          890  read_file {"path":"src/budget.rs"}
 ...
  Tool definitions (sent with every request): 1,960 across 9 tools
    edit_file 410, execute_bash 260, find_files 180, ...
```

## What each part means

### Limits

- `W`, `U`, `E`, the reply reserve and the safety margin are the values of
  the budget in [../context.md](../context.md#budget). The report shows how
  they add up, and whether E is the usable window or a configured
  `[context].budget`.
- Percentages are always printed with their basis: `of E` (what the
  thresholds use) and `of W` (what other tools show).
- The bar is 40 characters, filled to the share of E in use.

### Counting

The total is the context's current token figure, labeled with its source,
in words:

| Source (logs, transcripts) | Shown as |
|---|---|
| `tokenize` | exact: server /tokenize |
| `usage` | exact: token usage the server reported for the last request |
| `tokenize+estimate`, `usage+estimate` | exact for the last request, plus an estimate for N messages since |
| `default-ratio` | estimate only: nothing sent to the model yet (3 characters per token) |

Category values are estimates in every case. Each category's character
count, as mima measures it for a request, gets the same share of the total;
the last category absorbs the rounding, so the rows always add up to the
total. Headers carry `~` to say so.

### Categories

Every message is counted in exactly one category; the system message is
split in two.

| Category | What it counts |
|---|---|
| System prompt | the system message without the project instructions section (prefix, body, suffix, environment note) |
| Project instructions | the labeled instructions section appended to the system message |
| Tool definitions | the tool schemas sent with every request |
| Operator requests | user messages from the operator (turn instructions) |
| Assistant text and tool calls | assistant messages, including tool-call arguments |
| Tool outputs, full | tool results as received, including stage 0 capped ones |
| Tool outputs, masked | placeholders that replaced masked results |
| Compaction records | records written by compaction (digest and summary) |
| mima notices | messages mima adds: loop-guard nudges, continue and step-limit prompts, "not executed" results |

This needs a `kind` on each message (operator request, notice, compaction
record), which the compaction design adds; today a notice is told apart from
a request only by its text.

### Room

- Until masking: `mask_at` share of E minus the total; when negative, the
  report says masking will run before the next request.
- In budget E: E minus the total.

### Largest items

The three largest messages by estimated tokens, with their tool call
(name and clipped arguments) for tool outputs, view (full, capped) and turn.
These are what masking will replace first, unless protected.

### Compaction

- Thresholds in tokens and as fractions of E: mask at, mask to, protected
  recent output, the stage 0 cap per output.
- Summaries: the `summarize` setting, and whether automatic summarizing is
  currently off and why (setting, budget under 12k, or stopped after two
  summaries that freed too little).
- So far: masked outputs and events, evicted messages, compaction records by
  method (summary, digest only).
- Last: turn, stage, trigger (threshold, `/compact`, server rejection), and
  tokens before and after, from the most recent event.

### Hints

At most two, from fixed rules, each with the action that helps:

| Condition | Hint |
|---|---|
| Window is the fallback (not configured, not reported by the server) | The server did not report the context window, so N is assumed; set `[context].window` to the model's real window. |
| W below 16k | The window is small; serve the model with 32k or more. |
| Project instructions above 5% of E | Shorter instruction files leave more room. |
| Tool outputs above half the total and the total above 80% of `mask_at` | Tool outputs are most of the context; `/compact` frees room now. |
| Three or more compaction records | The session has been compacted N times; `/new` may work better. |
| Count source `default-ratio` | Nothing has been sent to the model yet; counts are rough, deliberately high estimates until the first reply. |

## Implementation

- `context.rs`: `AgentContext::report() -> ContextReport`, a plain data
  snapshot (limits, total and source, categories with characters, tokens and
  item counts, largest items, thresholds, compaction counters and the last
  event). `Budget` keeps the reply reserve and margin it already computes;
  `AgentContext` keeps the last compaction event, which today is only
  logged.
- `main.rs`: `print_context_status(&ContextReport, detail)` renders it;
  `/context` and `/context-status` call it; `/help` lists both.
- No new configuration.
- Transcript (optional, additive): the `context` record gains a `usage`
  object with the category token figures, so `mima-eval` can report what
  filled the context at its peak. These are metadata, not content fields, in
  the audit export.

## Testing

- Categories cover every message once and add up to the total, for a context
  with masked, capped, synthetic and compaction-record messages, with and
  without project instructions.
- Rounding: totals match for small totals and many categories.
- Labels: each count source renders its wording; the room line handles a
  total above `mask_at`.
- Hints: each rule fires at its boundary and at most two are shown.
- A rendering test with a fixed report, so format changes are deliberate.

## Alternatives considered

- Keep the one-line `/context`: answers "how much" but not "of what" or
  "when".
- Exact per-category counts with one `/tokenize` call per category: not
  additive (the chat template adds framing per request), and many server
  calls per command.
- A local tokenizer: rejected for counting in general (dependency size, chat
  template reproduction; see [../context.md](../context.md)).
- One percentage of the window, as most agents show: hides that compaction
  is driven by E and its fractions.

## References

- Claude Code documentation, `/context` and context window (public docs);
  Letta Code, Aider, Roo Code, Codex, Gemini CLI, OpenCode, goose and Cline
  source at pinned commits: details in the research notes.
