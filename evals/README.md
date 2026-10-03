# Evaluation smoke test

Six small seed tasks that exercise `mima-eval` itself: task loading, the
sandbox, hidden checks, validation and its cheating baselines, and a short run.
They are too few to compare models (see "What a small suite can show" in
[docs/eval.md](../docs/eval.md)).

```bash
cargo build --release
./target/release/mima-eval validate evals/suites/smoke.toml
./target/release/mima-eval run evals/suites/smoke.toml --profiles <profiles.toml>
```

The full task set (106 tasks, including tasks built from third-party
repositories) is kept in a separate repository, so that its reference
solutions and hidden checks are not published with the agent; published
solutions end up in model training data and spoil later evaluations. Its
layout is the same (`tasks/`, `suites/`), so `mima-eval` runs it by path:
`mima-eval run <task-repo>/suites/all.toml --profiles ...`.

These six tasks are copies of seed tasks in that repository and are not kept
in sync automatically.
