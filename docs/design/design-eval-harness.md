# Design: Evaluation Harness

Status: implemented as `mima-eval`; see [../eval.md](../eval.md) for usage,
the task format, isolation, and the statistics.

The earlier skeleton (`src/eval.rs`: graders, a verifier vector, and a pooled
Bernoulli SEM) was replaced. Two decisions changed from that skeleton:

- **Black-box runner.** `mima-eval` runs the real `mima` binary as a
  subprocess per trial (with `--approve-all`, `--transcript-path`,
  `--max-steps` and `--config`), so evaluations measure exactly what ships and
  the harness stays out of the audited agent. Metrics come from mima's own
  transcript.
- **Statistics.** The pooled SEM is only valid with one trial per task; the
  harness now averages per-task pass rates and clusters standard errors by
  task family (Miller, arXiv:2411.00640), with Wilson intervals per task and
  paired comparisons between models.
