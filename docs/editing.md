# Editing files

How `mima` lets the model change files, and why it works this way.

## Summary

- **`edit_file`** replaces an exact piece of text in an existing file. It is
  the main way to change code.
- **`read_file`** shows numbered lines and reads large files in parts.
- **`write_file`** creates new files and fully rewrites small ones.
- An edit must match **exactly one** place (or use `replace_all`). A few
  deterministic tolerances are allowed; similarity-based guessing is not.
- The model must have **read the current content** before changing a file.
- The approval prompt shows a **unified diff** of the change.

## Tools

### `read_file {path, offset?, limit?}`

```
[src/calc.c lines 6-10 of 15]
     6	
     7	/* Multiply two integers. */
     8	int mul(int a, int b) {
     9	    return a + b;
    10	}
[5 more lines; next: offset=11]
```

- Each line starts with its number and a TAB, like `cat -n`. The prefix is
  not part of the file.
- Output stops at a whole line within the context budget
  ([context.md](context.md)) and says where to continue, instead of cutting
  the middle out of a large file. Lines over 2,000 characters are cut.

### `edit_file {path, old_string, new_string, replace_all?}`

`old_string` is copied from the file (without line-number prefixes) and must
occur exactly once unless `replace_all` is true. For several changes the
model calls `edit_file` several times; arguments stay flat strings (see
[Why](#why)).

Matching, in order; each step must still find exactly one place:

1. **Exact**, after converting line endings to the file's own.
2. **Without `read_file` line-number prefixes**, if every line of
   `old_string` has one (so text that merely starts with a number is left
   alone).
3. **Without one trailing newline** (tool-call parsers can add or drop one).
4. **Ignoring trailing whitespace** on each line (whole lines only).
5. **With a uniform indentation difference**: every line of `old_string`
   indented by the same amount more or less than the file. `new_string` is
   re-indented to match. (Seen live: Nemotron 3 Super indented a whole
   snippet by four extra spaces.)

When step 2 or later matched, the result says so (for example `Note: ignored
trailing whitespace in old_string`). There is no similarity-threshold or
"closest block" replacement: a near miss is reported, never applied.

Files keep their line endings (LF or CRLF), a leading byte-order mark, and
their permissions. Writes are atomic (temporary file, then rename).

### `write_file {path, content}`

Creates a new file without further checks. Overwriting an existing file
requires a current read, like `edit_file`, and shows a diff.

## Stale-file protection

`mima` remembers a hash of each file's content when the model reads it and
after each of its own writes. An edit or overwrite is refused if the file was
not read in this session, or if it changed on disk since (by you or another
process). Reading any part of the file is enough; a full read is not needed.
The check runs again just before writing, after approval. `/new` forgets all
reads.

## Approval

`edit_file` and overwriting `write_file` require approval when
`[security].require_approval_for_writes` is on (the default). The tool first
validates and plans the change without touching the file; if the call cannot
succeed (no match, several matches, not read), the error goes straight back
to the model and there is nothing to approve. Otherwise the prompt shows the
change:

```
Edit calc.c  (+1 -1, exact match)
--- a/calc.c
+++ b/calc.c
@@ -6,7 +6,7 @@
 
 /* Multiply two integers. */
 int mul(int a, int b) {
-    return a + b;
+    return a * b;
 }
 
 int main(void) {
Approve? (y/N):
```

The diff has three lines of context, omits unchanged lines inside a
replacement, is colored on a terminal, and is cut after 200 lines. Control
characters in model-supplied text are shown escaped. The diff and the
decision are recorded in the session transcript ([sessions.md](sessions.md)).

## Errors the model sees

Every failure says that nothing changed, why, and what to do next, and
invites a different call rather than a repeat:

- **Not found**: the closest matching lines (by line similarity, shown only
  if at least 60% similar) and the `read_file` range to re-read. If
  `new_string` is already present, it says the change may already be
  applied.
- **Several matches**: the line numbers, and the choice between more context
  and `replace_all`.
- **Not read / changed on disk**: re-read the lines being changed.
- Identical `old_string`/`new_string`, empty `old_string`, a missing file
  (use `write_file`), or a `null` argument.

In the first live test the model's first edit failed on indentation; the
error's closest match and re-read range led it to succeed on the next call.

## Why

The research behind this (source code of eleven agents, documentation, and
published benchmarks) points to one design:

- **Exact text, not line numbers or diffs.** On the Diff-XYZ benchmark,
  Qwen2.5-Coder-32B scored 0.68 exact match with search/replace blocks versus
  0.23 with unified diffs (plain-text output, not tool calls)[^diffxyz]. With a
  fine-tuned Qwen2.5-Coder-7B, line-numbered unified diffs scored 14-38 pass@1
  against 54 for a content-addressed format[^adaedit]. Aider,
  Claude Code, OpenHands, Gemini CLI, Cline and OpenCode all use exact
  search/replace; `edit_file`'s argument names follow Claude Code's `Edit`
  tool.
- **Not OpenAI's `apply_patch`.** OpenAI trained its models on that format;
  other agents enable it only for GPT models.
- **Few, deterministic tolerances.** Aider's edit-distance fallback is
  disabled in its code, OpenCode had to guard against its fuzzy matchers
  replacing too much, and Claude Code's documentation states it does no fuzzy
  matching. The tolerances above each handle a specific, observed failure
  (line-number prefixes copied from `read_file` output were a leading cause
  of failed edits in one small study) and require a unique match.
- **Flat arguments.** Nemotron 3 Super and Qwen3.6 write tool calls in an XML
  format where string arguments are raw text, so code needs no JSON escaping.
  A nested list of edits would bring escaping back, so each edit is its own
  call.
- **Whole-file writes stay available.** Aider's data shows whole-file edits
  are always well-formed but cost output tokens; they remain the tool for new
  and small files.

## Not yet

- An operator `/undo` for the last edit (use git meanwhile).
- An optional syntax check after an edit (e.g. `python3 -m py_compile`),
  reported as a warning.
- Measuring edit failure rates per model with the evaluation harness, before
  adding anything more.

## References

[^diffxyz]: Glukhov et al., *Diff-XYZ: A Benchmark for Evaluating Diff
    Understanding*, arXiv:2510.12487 (2025).
[^adaedit]: Cheng et al., *To Diff or Not to Diff? Structure-Aware and
    Adaptive Output Formats for Efficient LLM-based Code Editing*,
    arXiv:2604.27296 (2026).
