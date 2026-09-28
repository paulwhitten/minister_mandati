# Context management

How `mima` keeps the conversation it sends to the model within the model's
context window, and why it works this way.

## Summary

- The budget comes from the model's **context window**, not a message count.
- Old **tool outputs are hidden first** (replaced by a one-line placeholder),
  keeping the agent's own reasoning and tool calls. Whole old steps are
  dropped only when hiding outputs is not enough.
- History is always a **valid request**: every tool call has exactly one
  result, immediately after the call that requested it.
- Edits happen **rarely and in large batches**, so the server's prompt cache
  stays useful between them.
- Every edit is **logged** (stage, tokens before and after, messages affected).

## Background

Tool-using coding agents fill their context mostly with tool output: about 84%
of the tokens in an average SWE-agent turn[^trap]. Three findings shape the
design:

1. **Hiding old tool outputs works as well as LLM summarization, for less.**
   On SWE-bench Verified, replacing tool outputs older than the last 10 turns
   with a placeholder roughly halved cost and matched LLM summarization on
   solve rate; summarization made runs 13-15% longer and needs extra model
   calls[^trap]. Independent studies point the same way[^discovery]. The same
   technique ships in Gemini CLI, OpenCode, LangChain (`ClearToolUsesEdit`),
   SWE-agent (`LastNObservations`) and Anthropic's API (`clear_tool_uses`).
2. **Dropping whole turns is the weakest option.** Keeping only the last five
   turns cut AppWorld accuracy from 56.0 to 45.8, and on hard tasks from 39.7
   to 15.9[^acon]. It discards the agent's reasoning along with the output.
3. **Models degrade well before their advertised window is full.** Effective
   context is often half the trained length or less[^ruler][^nolima][^string],
   and length alone costs accuracy even with perfect retrieval[^length].

Open-source agents converge on the same mechanics: a token budget expressed as
a fraction of the window, counted from the token usage the server reports plus
a cheap estimate for new messages; a cap on each tool output as it arrives;
and triggers that fire rarely and free a lot at once, because every edit to
earlier history invalidates the server's prefix cache.

## Budget

```
W  context window     [context].window, else the server's max_model_len
                      (vLLM GET /v1/models), else 32768 with a warning
R  reply reserve      [agent].max_tokens
S  safety margin      max(512, 3% of W)
U  usable prompt      W - R - S                  hard limit
E  operating budget   min(U, [context].budget)   default: U
```

`[context].budget` lets you run a large-window model below its advertised
size. If the server rejects a request because the prompt is too long and
states its limit, `mima` adopts that limit as `W`; if it states the prompt's
measured size (vLLM does), `mima` recalibrates its estimate from it.

**Counting tokens.** Counts come from the server, not from a local
tokenizer:

- **Exact, before sending** (servers with `/tokenize`, such as vLLM). At
  startup `mima` checks for the endpoint. Before each request it sends the
  exact messages and tool schemas to `/tokenize`, which applies the model's
  own chat template, so the count equals the `prompt_tokens` the server then
  reports. If compaction changes the history, the request is counted again.
  One extra call to the local server per step.
- **Anchored estimate** (servers without `/tokenize`, such as Ollama). The
  `usage.prompt_tokens` reported for the last request is exact for that
  request; messages added or removed since are estimated with a
  characters-per-token ratio calibrated from those exact counts. Before the
  first response the ratio is 3.0, deliberately conservative.
- A prompt-too-long error that states the measured size (vLLM does) also
  anchors the count.

Cumulative totals (`/tokens`, transcripts) always use the server's reported
usage. Logs, `/context` and transcripts label each count's source:
`tokenize`, `usage`, `tokenize+estimate`, `usage+estimate`, or
`default-ratio`. Set `[context].server_tokenize = false` to skip the
`/tokenize` calls.

Measured against Nemotron 3 Super on vLLM, the `/tokenize` count matched the
reported `prompt_tokens` on every step. Local tokenizer libraries were not
used: the Hugging Face `tokenizers` crate would still need each model's chat
template reproduced to be exact, adds a large dependency, and needs the
model's tokenizer file supplied locally; `tiktoken-rs` only covers OpenAI
tokenizers.

## Stages

| Stage | Trigger | Action | Target |
|---|---|---|---|
| 0 cap | every tool result | keep the head and tail of output over `clamp(8% of E, 1k, 16k)` tokens, with a notice of how much was cut | -- |
| 1 mask | above 60% of E | replace the oldest tool-result bodies with a placeholder; skipped unless it frees at least 10% of E | 40% of E |
| 2 evict | masking cannot reach the target, or the server rejects the prompt as too long | drop the oldest whole steps (a tool-call message with all its results) | 40% of E (on rejection: below 70% of the current size) |
| 3 stop | the server still rejects the prompt and nothing more can be removed | end the turn with an error | -- |

**Stage 1 (masking).** A masked result keeps its message and `tool_call_id`
(so pairing is untouched) and its body becomes, for example:

```
[output elided to save context: read_file {"path":"src/main.rs"}, 18342 bytes. Re-run the tool if you need it again.]
```

Errors keep their first line. Protected from masking and eviction: the most
recent tool output up to 25% of E (always at least the newest result), and
the most recent failed result.

The gap between the trigger (60%) and the target (40%) batches edits, so
several steps in a row are append-only and reuse the prefix cache. How many
depends on output size: with outputs near the stage 0 cap (8% of E), a
compaction happens about every two to three steps; smaller outputs space them
out further. This is a tuning point for evaluation.

**Stage 2 (eviction).** Placeholders, the agent's reasoning and protected
output accumulate, so eventually masking alone cannot reach the target. Steps
are then removed oldest first (by that point mostly placeholders), earlier
turns before the current one. Never removed: the system prompt, the first
instruction, the current turn's instruction, the newest step, and steps
holding protected output. The first instruction gets a short note saying how
many earlier messages were removed. On a prompt-too-long error, `mima` masks
and evicts regardless of the minimum-gain rule, then retries the request at
most twice.

**Normalization** runs before every request: any tool call without a result
gets a `Not executed` result, and any result without a matching call is
dropped. Tool calls are recorded in history from the parsed calls, so the
text-based (ReAct) fallback and calls without a server-assigned id pair up
too.

## Behavior across window sizes

With the default reply reserve `R = 4096`:

| W | E | Stage 0 cap | Mask at | Compact to |
|---|---|---|---|---|
| 8k | ~3.6k | 1k (floor) | ~2.2k | ~1.4k |
| 32k | ~27.7k | ~2.2k | ~16.6k | ~11.1k |
| 128k | ~123k | ~9.8k | ~74k | ~49k |
| 256k, `budget = 96000` | 96k | ~7.7k | ~58k | ~38k |

At 8k the system prompt and tool schemas take a large share and one file read
nearly fills the budget. Serve models with a window of 32k or more.

## Configuration

```toml
[context]
# window = 32768      # override the discovered context window (tokens)
# budget = 96000      # operate below the window (tokens)
# mask_at = 0.6       # compact above this fraction of the operating budget E
# mask_to = 0.4       # ... down to this fraction
# keep_recent = 0.25  # newest tool output protected (fraction of E)
# server_tokenize = true  # exact counts via the server's /tokenize when available
```

The REPL command `/context` shows the window, budget, current estimate and
the counts of masked and evicted messages.

## Status and next steps

Implemented: budget from the window (config, `/v1/models`, or the server's
error), exact counting via `/tokenize` with an anchored estimate as fallback, stages 0-3,
normalization, eviction notice, per-event logging, overflow retry. Tested
with unit tests and against a mock OpenAI-compatible server; exact counting
also checked live against vLLM. Not yet evaluated on real tasks.

Planned, in order:

1. **Evaluation** on the target models to tune thresholds and the protected
   budget, measuring tokens, steps and tool calls per task as well as pass
   rate (forgetting shows up as re-reading, not only as failures[^cost]).
   Small models may tolerate masking less well: Qwen3-32B scored 15.0% with
   masking versus 17.0% with no management[^trap].
2. **Recall from the transcript.** Session transcripts now keep full tool
   outputs and record which calls were masked ([sessions.md](sessions.md)).
   Next: a read-only `recall_output(call_id)` tool so placeholders can point
   to the stored output instead of asking for a re-run. Only possible when the
   session is being recorded (transcripts are off by default).
3. **Optional `/compact`**: a structured LLM summary of the masked span
   (goal, constraints, files, done and pending work, errors, next step),
   treating history as untrusted input and rejected if it does not shrink the
   context. Untrained summarization has weak evidence over masking, so it
   stays opt-in.
4. **Notes/todo recitation**: re-inject a short notes file near the end of
   context after compaction.

## References

[^trap]: Lindenbauer et al., *The Complexity Trap: Simple Observation Masking
    Is as Efficient as LLM Summarization for Agent Context Management*,
    arXiv:2508.21433 (2025).
[^discovery]: Chintalapati et al., *Evaluating Memory Condensation Strategies
    for Coding Agents in Data-Driven Scientific Discovery*, arXiv:2605.18854
    (2026).
[^acon]: Kang et al., *ACON: Optimizing Context Compression for Long-horizon
    LLM Agents*, arXiv:2510.00615 (2025).
[^ruler]: Hsieh et al., *RULER: What's the Real Context Size of Your
    Long-Context Language Models?*, arXiv:2404.06654 (2024).
[^nolima]: Modarressi et al., *NoLiMa: Long-Context Evaluation Beyond Literal
    Matching*, arXiv:2502.05167 (2025).
[^string]: An et al., *Why Does the Effective Context Length of LLMs Fall
    Short?*, arXiv:2410.18745 (2024).
[^length]: Du et al., *Context Length Alone Hurts LLM Performance Despite
    Perfect Retrieval*, arXiv:2510.05381 (2025).
[^cost]: Liu, *What Does Context Compression Cost an Agent?*,
    arXiv:2608.16370 (2026).
