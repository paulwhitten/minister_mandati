//! The agent loop: one turn from instruction to final answer, shared by the
//! interactive and one-shot modes and the evaluation runner. Each step:
//! bring the request within budget (`docs/context.md`), call the model, run
//! the requested tools behind approval, and record everything to the session
//! transcript (`docs/sessions.md`).

use serde_json::{Value, json};
use snafu::prelude::*;
use std::time::{Duration, Instant};

use crate::approval;
use crate::budget;
use crate::client;
use crate::context::{self, AgentContext, Message};
use crate::loop_guard::{Intervention, LoopGuards};
use crate::presenter::{Approval, Presenter};
use crate::tools::{ToolEnv, ToolRegistry, truncate_middle};

/// Times per turn an empty or truncated reply is answered with a request to
/// continue before it is accepted as the (empty) answer.
const MAX_CONTINUES: usize = 2;
/// Steps before the limit at which the model is told to finish.
const STEPS_WARNING: usize = 3;
/// Retries after the server rejects a prompt as too long (see docs/context.md).
pub const MAX_OVERFLOW_RETRIES: u32 = 2;

/// How a turn ended.
#[derive(Debug)]
pub enum TurnOutcome {
    Answered(String),
    /// The model stopped without an answer or a tool call after the allowed
    /// continue prompts: an empty reply (`truncated` false) or one still cut
    /// off at the reply limit (`truncated` true, with any partial text).
    /// Never counted as an answer.
    NoAnswer {
        truncated: bool,
        partial: String,
    },
    LoopGuard,
    /// The step limit was reached; the summary the model gave afterwards,
    /// if any.
    StepCap(Option<String>),
    /// The time limit (`[agent].max_seconds`) was reached; the summary, as
    /// for the step limit.
    TimeLimit(Option<String>),
}

/// Sets the context window: `[context].window` if configured, else what the
/// server reports, else the fallback (with a warning, since it may be wrong).
pub async fn configure_window(ctx: &mut AgentContext) {
    // The model entry is kept for the audit record even when the window is
    // configured.
    ctx.server_model = client::discover_model(&ctx.config).await;
    ctx.server_version = client::server_version(&ctx.config).await;
    if ctx.config.context.window.is_some() {
        return;
    }
    let window = ctx
        .server_model
        .as_ref()
        .and_then(|m| m["max_model_len"].as_u64())
        .map(|w| w as usize);
    match window {
        Some(w) => {
            tracing::info!(window = w, "context window discovered from server");
            ctx.set_window(w, crate::context_report::WINDOW_SERVER);
        }
        None => tracing::warn!(
            window = budget::FALLBACK_WINDOW,
            "context window not reported by the server; assuming the fallback. \
             Set [context].window in agent.toml if this is wrong"
        ),
    }
}

/// Enables exact token counting when the server offers `/tokenize` (and
/// `[context].server_tokenize` allows it); otherwise counts are anchored on the
/// usage the server reports after each response.
pub async fn configure_token_counter(ctx: &mut AgentContext) {
    if !ctx.config.context.server_tokenize {
        tracing::info!("server token counting disabled; using reported usage plus estimates");
        return;
    }
    match client::probe_tokenize(&ctx.config).await {
        Some(url) => {
            tracing::info!(%url, "exact token counting via the server's /tokenize");
            ctx.token_counter = Some(url);
        }
        None => {
            tracing::info!("server has no /tokenize endpoint; using reported usage plus estimates")
        }
    }
}

/// Counts the request exactly via the server, when available. A failure
/// falls back to the estimate for this request.
pub async fn count_exactly(ctx: &mut AgentContext) {
    let Some(url) = ctx.token_counter.clone() else {
        return;
    };
    match client::count_prompt_tokens(ctx, &url).await {
        Ok(n) => ctx.record_counted(n),
        Err(e) => tracing::warn!(error = %e, "exact token count failed; using the estimate"),
    }
}

/// Brings the next request within budget: normalize, count, compact, and
/// recount if compaction changed the history, then seal.
pub async fn prepare_request(ctx: &mut AgentContext) {
    ctx.normalize();
    count_exactly(ctx).await;
    if ctx.compact() {
        count_exactly(ctx).await;
        ctx.write_context("compaction");
    }
    ctx.seal_request();
}

pub fn ledger_json(ctx: &AgentContext) -> Value {
    let l = ctx.token_ledger();
    json!({
        "requests": l.requests,
        "prompt": l.prompt_tokens,
        "completion": l.completion_tokens,
        "unreported_requests": l.unreported_requests,
    })
}

/// Records how a turn ended (`turn_end`).
pub fn finish_turn(
    ctx: &mut AgentContext,
    outcome: &str,
    answer: Option<&str>,
    error: Option<String>,
) {
    let tokens = ledger_json(ctx);
    let turn = ctx.session.turn();
    let workspace = ctx.turn_end_workspace.take();
    let seq = ctx.session.record(
        "turn_end",
        json!({ "turn": turn, "outcome": outcome, "answer": answer, "error": error,
                "tokens": tokens, "workspace": workspace }),
    );
    // The answers kept in the context (see `answer_in_context`) refer to
    // this record, so eviction lists them and resume restores them exactly.
    if crate::transcript::answer_in_context(outcome) {
        ctx.set_answer_origin(seq);
    }
    // After the turn (including a cancelled one, already rolled back), so a
    // resume restores exactly what the model will see next.
    ctx.write_context("turn_end");
}

pub fn record_outcome(ctx: &mut AgentContext, result: &Result<TurnOutcome, snafu::Whatever>) {
    match result {
        Ok(TurnOutcome::Answered(a)) => finish_turn(ctx, "answered", Some(a), None),
        Ok(TurnOutcome::NoAnswer { truncated, partial }) => finish_turn(
            ctx,
            if *truncated { "truncated" } else { "no_answer" },
            (!partial.is_empty()).then_some(partial.as_str()),
            None,
        ),
        Ok(TurnOutcome::LoopGuard) => finish_turn(ctx, "loop_guard", None, None),
        Ok(TurnOutcome::StepCap(summary)) => finish_turn(ctx, "step_cap", summary.as_deref(), None),
        Ok(TurnOutcome::TimeLimit(summary)) => {
            finish_turn(ctx, "time_limit", summary.as_deref(), None)
        }
        Err(e) => finish_turn(ctx, "error", None, Some(e.to_string())),
    }
}

/// A single agent task: seed the instruction, then run the tool loop to a final
/// answer, a loop-guard stop, or the step cap.
pub async fn run_turn(
    ctx: &mut AgentContext,
    registry: &ToolRegistry,
    presenter: &mut dyn Presenter,
    instruction: &str,
) -> Result<TurnOutcome, snafu::Whatever> {
    let result = run_turn_inner(ctx, registry, presenter, instruction).await;
    // The workspace as the turn leaves it, for `turn_end` (audit record).
    if ctx.session.is_recording() {
        ctx.turn_end_workspace = Some(crate::evidence::git_summary(registry.runner()).await);
    }
    result
}

async fn run_turn_inner(
    ctx: &mut AgentContext,
    registry: &ToolRegistry,
    presenter: &mut dyn Presenter,
    instruction: &str,
) -> Result<TurnOutcome, snafu::Whatever> {
    tracing::info!(instruction = %instruction, "starting task");
    presenter.task_started(instruction);
    audit_gate(ctx)?;
    let turn = ctx.session.next_turn();
    let workspace = if ctx.session.is_recording() {
        crate::evidence::git_summary(registry.runner()).await
    } else {
        Value::Null
    };
    let origin = ctx.session.record(
        "turn_start",
        json!({ "turn": turn, "instruction": instruction, "workspace": workspace }),
    );
    ctx.begin_turn(instruction, origin);

    let max_steps = ctx.config.agent.max_steps;
    let mut guards = LoopGuards::new(
        ctx.config.agent.dedupe_identical_writes,
        ctx.config.agent.loop_guard_window,
        ctx.config.agent.loop_guard_repeat_threshold,
    );

    let mut gate = Gate::default();
    gate.start(ctx, registry).await;
    let started = Instant::now();
    let max_seconds = ctx.config.agent.max_seconds;
    let wrap_up_percent = usize::from(ctx.config.agent.wrap_up_percent);
    // End check: the last steps of the limit are kept for repair.
    let reserve = if ctx.config.verify.enabled && ctx.config.verify.end_check {
        ctx.config.verify.repair_steps.min(max_steps / 2)
    } else {
        0
    };
    let mut wrapped_up = wrap_up_percent == 0;
    let mut time_limited = false;
    // After the time limit, the step at which repair ends.
    let mut repair_until: Option<usize> = None;
    let mut continues = 0;
    for step in 0..max_steps {
        let span = tracing::info_span!("step", n = step);
        let _enter = span.enter();

        let elapsed = started.elapsed().as_secs();
        if let Some(end) = repair_until {
            if step >= end {
                break;
            }
        } else if max_seconds.is_some_and(|max| elapsed >= max) {
            tracing::warn!(max = ?max_seconds, "reached the time limit");
            time_limited = true;
            if reserve == 0 {
                break;
            }
            let limit = crate::verify::Limit::Time {
                minutes: max_seconds.unwrap_or(0).div_ceil(60),
                steps: reserve,
            };
            match gate.at_limit(ctx, registry, presenter, turn, limit).await? {
                Some((message, seq)) => {
                    ctx.add_message(Message::notice(&message).with_origin(seq));
                    repair_until = Some(step + reserve);
                }
                _ => break,
            }
        } else if reserve > 0 && step == max_steps - reserve {
            let limit = crate::verify::Limit::Steps { left: reserve };
            if let Some((message, seq)) =
                gate.at_limit(ctx, registry, presenter, turn, limit).await?
            {
                ctx.add_message(Message::notice(&message).with_origin(seq));
                wrapped_up = true;
            }
        }
        if !wrapped_up
            && repair_until.is_none()
            && (step * 100 >= wrap_up_percent * max_steps
                || max_seconds.is_some_and(|max| elapsed * 100 >= wrap_up_percent as u64 * max))
        {
            wrapped_up = true;
            let budget = crate::verify::budget_line(step, max_steps, elapsed, max_seconds);
            let (message, seq) = gate
                .wrap_up(ctx, registry, presenter, turn, &budget)
                .await?;
            ctx.add_message(Message::notice(&message).with_origin(seq));
        }

        let mut overflow_retries = 0;
        let (response, response_seq) = loop {
            prepare_request(ctx).await;
            let counted = ctx.stats();
            let started = Instant::now();
            let result =
                client::generate_completion(ctx, &mut |delta| presenter.stream_delta(delta)).await;
            // Terminate any streamed line before logging, so log lines never run
            // into the model's output.
            presenter.stream_end();
            let duration_ms = started.elapsed().as_millis() as u64;
            match result {
                Ok(r) => {
                    let calls: Vec<Value> = r
                        .tool_calls
                        .iter()
                        .flatten()
                        .map(|c| json!({ "id": c.id, "name": c.name, "args": c.args }))
                        .collect();
                    let usage = r.usage.map(
                        |u| json!({ "prompt": u.prompt_tokens, "completion": u.completion_tokens }),
                    );
                    let seq = ctx.session.record(
                        "model_response",
                        json!({ "turn": turn, "step": step, "duration_ms": duration_ms,
                                "content": r.content, "tool_calls": calls, "usage": usage,
                                "counted_prompt_tokens": counted.tokens,
                                "count_source": counted.token_source,
                                "finish_reason": r.finish_reason,
                                "reasoning": r.reasoning,
                                "request_sha256": r.request_sha256,
                                "response_id": r.response_id,
                                "served_model": r.served_model }),
                    );
                    break (r, seq);
                }
                Err(e) => {
                    let overflow = e.is_context_overflow();
                    let retry = overflow && overflow_retries < MAX_OVERFLOW_RETRIES;
                    ctx.session.record(
                        "model_error",
                        json!({ "turn": turn, "step": step, "duration_ms": duration_ms,
                                "error": e.to_string(), "overflow": overflow, "retry": retry }),
                    );
                    if !retry {
                        return Err(e).whatever_context("model completion failed");
                    }
                    overflow_retries += 1;
                    tracing::warn!(error = %e, attempt = overflow_retries, "prompt too long for the model; compacting and retrying");
                    if !ctx.handle_overflow(e.reported_window(), e.reported_prompt_tokens()) {
                        snafu::whatever!(
                            "the conversation no longer fits the model's context window \
                             and nothing more can be removed: {e}"
                        );
                    }
                }
            }
        };

        ctx.record_usage(response.usage);
        if let Some(u) = response.usage {
            tracing::info!(
                prompt = u.prompt_tokens,
                completion = u.completion_tokens,
                total = u.total_tokens,
                "token usage (step)"
            );
        }

        let calls = match response.tool_calls.clone() {
            Some(c) if !c.is_empty() => c,
            // An empty or truncated reply is not an answer: the model ran out
            // of output tokens (often while reasoning) or said nothing. Ask
            // it to continue, a limited number of times per turn.
            _ if continues < MAX_CONTINUES
                && (response.content.as_deref().unwrap_or("").trim().is_empty()
                    || response.finish_reason.as_deref() == Some("length")) =>
            {
                continues += 1;
                tracing::warn!(
                    finish_reason = ?response.finish_reason,
                    attempt = continues,
                    "empty or truncated reply; asking the model to continue"
                );
                let seq = ctx.session.record(
                    "loop_guard",
                    json!({ "turn": turn, "action": "continue", "repeats": continues,
                            "tool": "" }),
                );
                ctx.add_message(Message::notice(context::CONTINUE).with_origin(seq));
                continue;
            }
            // Still empty or cut off after the continue prompts: no answer.
            _ if response.content.as_deref().unwrap_or("").trim().is_empty()
                || response.finish_reason.as_deref() == Some("length") =>
            {
                let truncated = response.finish_reason.as_deref() == Some("length");
                let partial = response.content.clone().unwrap_or_default();
                let why = if truncated {
                    format!(
                        "the model's replies were cut off at the reply limit ({} tokens) \
                         {} times without a tool call or an answer; raise [agent].max_tokens",
                        ctx.config.agent.max_tokens,
                        continues + 1
                    )
                } else {
                    "the model gave no answer and no tool call".to_string()
                };
                tracing::warn!("{why}");
                presenter.final_answer(&format!("[no answer: {why}]"));
                log_token_summary(ctx);
                return Ok(TurnOutcome::NoAnswer { truncated, partial });
            }
            _ => {
                let final_msg = response.content.clone().unwrap_or_default();
                // Verifier gate: new failures send the answer back once more,
                // unless no step is left to use it.
                let no_steps_left = step + 1 >= repair_until.unwrap_or(max_steps)
                    || (repair_until.is_none()
                        && reserve == 0
                        && max_seconds.is_some_and(|m| started.elapsed().as_secs() >= m));
                if let Some((message, seq)) = gate
                    .check_answer(ctx, registry, presenter, turn, &final_msg, no_steps_left)
                    .await?
                {
                    if !final_msg.is_empty() {
                        ctx.add_message(Message::assistant(&final_msg).with_origin(response_seq));
                    }
                    ctx.add_message(Message::notice(&message).with_origin(seq));
                    continue;
                }
                presenter.final_answer(&final_msg);
                // Keep the answer so follow-up turns (REPL) can see it.
                if !final_msg.is_empty() {
                    ctx.add_message(Message::assistant(&final_msg));
                }
                log_token_summary(ctx);
                tracing::info!("task complete");
                return Ok(TurnOutcome::Answered(final_msg));
            }
        };

        ctx.add_message(Message::assistant_tool_calls(&response).with_origin(response_seq));

        // A nudge is a user message, so it must wait until every tool result
        // of this step is in; results have to follow their request directly.
        let mut nudge: Option<Option<u64>> = None;
        for call in calls {
            // Calls written for other agents' tools map onto mima's, before
            // approval and loop checks see them.
            let mut call = crate::tools::normalize_call(call);
            // Arguments in the types the tool declares: exact conversions
            // are made and recorded, anything else is refused with the reason.
            let (invalid_args, converted) = match registry.conform_args(&call.name, &call.args) {
                Ok((args, converted)) => {
                    call.args = args;
                    (None, converted)
                }
                Err(e) => (Some(format!("Error: {e}")), Vec::new()),
            };
            tracing::info!(tool = %call.name, args = %call.args, "tool requested");
            presenter.tool_requested(&call);

            let fp = LoopGuards::fingerprint(&call.name, &call.args);
            let repeats = guards.observe(fp);
            let policy = registry.dedupe_policy(&call.name);

            let mut ran_ms = 0u64;
            // Evidence (audit record): files changed by a shell command, and
            // a file tool's target before the call.
            let mut workspace_changes: Option<Value> = None;
            let file_before = (ctx.session.is_recording()
                && matches!(call.name.as_str(), "edit_file" | "write_file"))
            .then(|| call.args.get("path").and_then(Value::as_str))
            .flatten()
            .map(|p| crate::evidence::content_hash(std::path::Path::new(p)).0);
            let output = if let Some(skipped) = guards.skip_result(&call.name, policy, fp) {
                tracing::info!(tool = %call.name, "duplicate call skipped");
                record_approval(
                    ctx,
                    turn,
                    &call.id,
                    "skipped_duplicate",
                    None,
                    json!({ "rule": "loop_guard.skip_duplicate" }),
                );
                skipped
            } else if let Some(invalid) = invalid_args {
                // The call cannot succeed as given: nothing to approve.
                record_approval(
                    ctx,
                    turn,
                    &call.id,
                    "not_requested_invalid",
                    None,
                    json!({ "rule": "approval.invalid_call" }),
                );
                invalid
            } else {
                let decided = approval::policy(&ctx.config, &call, registry.shell_sandboxed());
                // Tools with a preview (edits, overwrites) validate first and
                // show the operator exactly what will change.
                let preview = if decided.needs {
                    registry
                        .preview(&call.name, &call.args)
                        .await
                        .map_err(|e| format!("Error: {e}"))
                } else {
                    Ok(None)
                };
                match preview {
                    // The call cannot succeed as given: nothing to approve.
                    Err(invalid) => {
                        record_approval(
                            ctx,
                            turn,
                            &call.id,
                            "not_requested_invalid",
                            None,
                            json!({ "rule": "approval.invalid_call" }),
                        );
                        invalid
                    }
                    Ok(preview) => {
                        let (decision, fields) = if !decided.needs {
                            let mut f = json!({ "rule": decided.rule });
                            if let Some(m) = &decided.matched {
                                f["matched"] = json!(m);
                            }
                            (decided.decision, f)
                        } else if ctx.config.security.approve_all {
                            // Evaluation harness: approved by flag, not by a person.
                            ("auto_approved", json!({ "rule": "approval.approve_all" }))
                        } else {
                            let asked = Instant::now();
                            let reply = presenter
                                .request_approval(&call, preview.as_deref())
                                .whatever_context("approval prompt failed")?;
                            let fields = json!({
                                "actor": "operator",
                                "answer": reply.answer,
                                "shown_sha256": crate::audit::hex(&crate::audit::sha256(&[reply.shown.as_bytes()])),
                                "wait_ms": asked.elapsed().as_millis() as u64,
                            });
                            if reply.decision == Approval::Deny {
                                record_approval(
                                    ctx,
                                    turn,
                                    &call.id,
                                    "denied",
                                    preview.as_deref(),
                                    fields,
                                );
                                let denied = "Error: user denied permission for this action.";
                                let seq = ctx.session.record(
                                    "tool_result",
                                    json!({ "turn": turn, "call_id": call.id, "tool": call.name,
                                            "duration_ms": 0, "failed": true, "executed": false,
                                            "bytes": denied.len(), "sent_bytes": denied.len(),
                                            "output": denied }),
                                );
                                ctx.add_message(
                                    Message::tool_result(&call.id, denied).with_origin(seq),
                                );
                                continue;
                            }
                            ("approved", fields)
                        };
                        record_approval(ctx, turn, &call.id, decision, preview.as_deref(), fields);
                        let changes_file = matches!(call.name.as_str(), "edit_file" | "write_file");
                        let shell = call.name == "execute_bash";
                        if changes_file || shell {
                            gate.before_change(ctx, registry, presenter, turn).await?;
                        }
                        let guard = if changes_file && ctx.config.verify.syntax_guard {
                            SyntaxGuard::before(registry, &call).await
                        } else {
                            None
                        };
                        let env = ToolEnv {
                            output_budget: ctx.output_cap_bytes(),
                            timeout: None,
                        };
                        // Required mode: the approval is on disk before the action.
                        audit_gate(ctx)?;
                        // The workspace before a shell command, for the audit
                        // record and for the gate (did the command change files?).
                        let watch = if shell && (ctx.session.is_recording() || gate.active(ctx)) {
                            match gate_dir() {
                                Some(d) => crate::evidence::marks_in(registry.runner(), &d).await,
                                None => None,
                            }
                        } else {
                            None
                        };
                        let ran = Instant::now();
                        let executed = registry.execute(&call.name, &call.args, &env).await;
                        ran_ms = ran.elapsed().as_millis() as u64;
                        if let Some((_, before)) = watch
                            && let Some(d) = gate_dir()
                            && let Some((_, after)) =
                                crate::evidence::marks_in(registry.runner(), &d).await
                        {
                            if crate::verify::changed_paths(&before, &after)
                                .next()
                                .is_some()
                            {
                                gate.changed = true;
                            }
                            if ctx.session.is_recording() {
                                workspace_changes = Some(crate::evidence::changes(&before, &after));
                            }
                        }
                        match executed {
                            Ok(out) => {
                                let checked = match guard {
                                    Some(g) => g.after(ctx, registry, turn, out).await,
                                    None => Ok(out),
                                };
                                match checked {
                                    Err(reverted) => reverted,
                                    Ok(out) => {
                                        if changes_file {
                                            gate.changed = true;
                                        }
                                        guards.record_success(policy, fp, &out);
                                        out
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!(tool = %call.name, error = %e, "tool failed");
                                format!("Error: {e}")
                            }
                        }
                    }
                }
            };
            // Stage 0 (docs/context.md): the model sees a capped copy; the
            // transcript keeps the full output.
            let cap = ctx.output_cap_bytes();
            let sent = truncate_middle(output.clone(), cap);
            let stored = ctx.session.stored_output(&output);
            let failed = context::is_failure(&output);
            let mut fields = json!({ "turn": turn, "call_id": call.id, "tool": call.name,
                    "duration_ms": ran_ms,
                    "failed": failed, "bytes": output.len(), "sent_bytes": sent.len(),
                    "output": stored, "cap_bytes": cap });
            if !failed && let Some(mut file) = file_state(&call) {
                if let Some(before) = file_before {
                    file["before_sha256"] = json!(before);
                }
                fields["file"] = file;
            }
            if let Some(changes) = workspace_changes {
                fields["workspace_changes"] = changes;
            }
            if !converted.is_empty() {
                fields["converted"] = json!(converted);
            }
            if output.len() > ctx.config.session.max_output_bytes {
                // The stored copy is cut; this identifies the original.
                fields["output_full_sha256"] = json!(crate::audit::hex(&crate::audit::sha256(&[
                    output.as_bytes()
                ])));
            }
            let seq = ctx.session.record("tool_result", fields);
            presenter.tool_completed(&call.name, &sent);
            let mut result = Message::tool_result(&call.id, &sent).with_origin(seq);
            if sent.len() < output.len() {
                result.cap_bytes = Some(cap as u64);
            }
            ctx.add_message(result);

            match guards.intervention(repeats) {
                Intervention::None => {}
                Intervention::Nudge => {
                    tracing::warn!(repeats, tool = %call.name, "loop guard: nudge injected");
                    let seq = ctx.session.record(
                        "loop_guard",
                        json!({ "turn": turn, "action": "nudge", "repeats": repeats, "tool": call.name }),
                    );
                    nudge = Some(seq);
                }
                Intervention::Terminate => {
                    tracing::warn!(repeats, tool = %call.name, "loop guard tripped: terminating turn");
                    ctx.session.record(
                        "loop_guard",
                        json!({ "turn": turn, "action": "terminate", "repeats": repeats, "tool": call.name }),
                    );
                    ctx.normalize();
                    presenter.loop_detected(repeats);
                    log_token_summary(ctx);
                    return Ok(TurnOutcome::LoopGuard);
                }
            }
        }
        if let Some(seq) = nudge {
            ctx.add_message(Message::notice(context::NUDGE).with_origin(seq));
        }
        if max_steps > STEPS_WARNING * 2 && step + 1 == max_steps - STEPS_WARNING {
            let seq = ctx.session.record(
                "loop_guard",
                json!({ "turn": turn, "action": "steps", "repeats": STEPS_WARNING, "tool": "" }),
            );
            ctx.add_message(Message::notice(context::STEPS_LEFT).with_origin(seq));
        }
    }

    gate.final_check(ctx, registry, presenter, turn).await?;
    if time_limited {
        presenter.step_cap_reached(max_steps);
        let summary = final_summary(ctx, presenter, turn, true).await;
        log_token_summary(ctx);
        return Ok(TurnOutcome::TimeLimit(summary));
    }
    tracing::warn!(max = max_steps, "reached step cap without completion");
    presenter.step_cap_reached(max_steps);
    let summary = final_summary(ctx, presenter, turn, false).await;
    log_token_summary(ctx);
    Ok(TurnOutcome::StepCap(summary))
}

/// Per-turn state of the verifier gate (`docs/design/design-verifier-gate.md`).
#[derive(Default)]
struct Gate {
    /// Verifier commands, resolved on first use (configured or detected).
    commands: Option<Vec<String>>,
    /// Failure lines and exit code of each command before the turn's first
    /// change; only failures beyond these are the model's to fix.
    baseline: Option<Vec<(Option<i32>, Vec<String>)>>,
    /// The turn changed files: through edit_file or write_file, or a shell
    /// command that changed the workspace's git state.
    changed: bool,
    /// Feedback rounds given this turn.
    rounds: usize,
    /// Off for the rest of the turn (a verifier command was not approved).
    off: bool,
    /// End check: the workspace's changed paths when the turn began.
    start: Option<std::collections::BTreeMap<String, crate::evidence::Mark>>,
    /// End check: the workspace facts were sent once with an answer.
    facts_sent: bool,
    /// End check: a repair notice was sent (the verifier runs once more when
    /// the turn ends).
    repaired: bool,
    /// Verifier runs this turn, numbering their call ids.
    runs: usize,
    /// End check: paths the verifier's own runs changed (caches, coverage
    /// files); not reported as the task's.
    written: std::collections::BTreeSet<String>,
}

#[cfg(test)]
thread_local! {
    /// Tests run in parallel in one process, so each sets the directory its
    /// gate works in instead of changing the process's.
    pub static GATE_DIR: std::cell::RefCell<Option<std::path::PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// The directory the gate detects commands in and compares git state of:
/// the working directory.
fn gate_dir() -> Option<std::path::PathBuf> {
    #[cfg(test)]
    if let Some(d) = GATE_DIR.with(|d| d.borrow().clone()) {
        return Some(d);
    }
    std::env::current_dir().ok()
}

/// One verifier command's result against the turn's baseline.
struct Checked {
    run: CheckRun,
    /// Failure lines beyond the baseline's (empty when it passed or fails as
    /// before).
    new: Vec<String>,
}

/// One verifier run.
struct CheckRun {
    /// Call id of its approval record (empty for a message-only record).
    call_id: String,
    command: String,
    exit: Option<i32>,
    output: String,
    duration_ms: u64,
}

impl Gate {
    fn commands(&mut self, ctx: &AgentContext) -> Vec<String> {
        self.commands
            .get_or_insert_with(|| {
                let v = &ctx.config.verify;
                if !v.commands.is_empty() {
                    v.commands.clone()
                } else if v.detect {
                    gate_dir()
                        .and_then(|d| crate::verify::detect(&d))
                        .into_iter()
                        .collect()
                } else {
                    Vec::new()
                }
            })
            .clone()
    }

    fn active(&self, ctx: &AgentContext) -> bool {
        ctx.config.verify.enabled && !self.off
    }

    fn end_check(&self, ctx: &AgentContext) -> bool {
        self.active(ctx) && ctx.config.verify.end_check
    }

    /// End check: remembers the workspace's changed paths as the turn begins.
    async fn start(&mut self, ctx: &AgentContext, registry: &ToolRegistry) {
        if self.end_check(ctx)
            && let Some(dir) = gate_dir()
        {
            self.start = crate::evidence::marks_in(registry.runner(), &dir)
                .await
                .map(|(_, m)| m);
        }
    }

    /// End check: what the turn changed besides what the tests show.
    async fn facts(
        &self,
        ctx: &AgentContext,
        registry: &ToolRegistry,
    ) -> crate::verify::WorkspaceFacts {
        match &self.start {
            Some(start) if self.end_check(ctx) => {
                let now = match gate_dir() {
                    Some(dir) => crate::evidence::marks_in(registry.runner(), &dir).await,
                    None => None,
                };
                match now {
                    Some((_, now)) => crate::verify::workspace_facts(start, &now, &self.written),
                    None => Default::default(),
                }
            }
            _ => Default::default(),
        }
    }

    /// Runs one verifier command (see [`run_check`]). With the end check,
    /// the paths the run changes are remembered as the verifier's own.
    async fn run(
        &mut self,
        ctx: &mut AgentContext,
        registry: &ToolRegistry,
        presenter: &mut dyn Presenter,
        turn: u64,
        command: &str,
    ) -> Result<Option<CheckRun>, snafu::Whatever> {
        self.runs += 1;
        let id = format!("verify-{turn}-{}", self.runs);
        let dir = gate_dir().filter(|_| self.end_check(ctx));
        let before = match &dir {
            Some(d) => crate::evidence::marks_in(registry.runner(), d).await,
            None => None,
        };
        let run = run_check(ctx, registry, presenter, turn, command, id).await?;
        if run.is_some()
            && let (Some(d), Some((_, before))) = (&dir, before)
            && let Some((_, after)) = crate::evidence::marks_in(registry.runner(), d).await
        {
            self.written
                .extend(crate::verify::changed_paths(&before, &after).cloned());
        }
        Ok(run)
    }

    /// Runs every verifier command and compares it with the baseline.
    /// `None` when a command was not approved (the gate is then off).
    async fn run_all(
        &mut self,
        ctx: &mut AgentContext,
        registry: &ToolRegistry,
        presenter: &mut dyn Presenter,
        turn: u64,
    ) -> Result<Option<Vec<Checked>>, snafu::Whatever> {
        let mut out = Vec::new();
        for (i, cmd) in self.commands(ctx).iter().enumerate() {
            let Some(run) = self.run(ctx, registry, presenter, turn, cmd).await? else {
                self.off = true;
                return Ok(None);
            };
            let new = if run.exit == Some(0) {
                Vec::new()
            } else {
                let current = crate::verify::failure_lines(&run.output);
                match self.baseline.as_ref().and_then(|b| b.get(i)) {
                    Some((Some(0), _)) | None => current,
                    Some((_, base)) => crate::verify::new_failures(base, &current),
                }
            };
            out.push(Checked { run, new });
        }
        Ok(Some(out))
    }

    /// The notice at the wrap-up point: the budget line, and the verifier's
    /// result when the turn has changed files.
    async fn wrap_up(
        &mut self,
        ctx: &mut AgentContext,
        registry: &ToolRegistry,
        presenter: &mut dyn Presenter,
        turn: u64,
        budget: &str,
    ) -> Result<(String, Option<u64>), snafu::Whatever> {
        let checked = if self.active(ctx) && self.changed {
            self.run_all(ctx, registry, presenter, turn)
                .await?
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let lines: Vec<String> = checked
            .iter()
            .map(|c| crate::verify::check_line(&c.run.command, c.run.exit, &c.new))
            .collect();
        let facts = if self.changed {
            self.facts(ctx, registry).await
        } else {
            Default::default()
        };
        let message = crate::verify::wrap_up_notice(budget, &lines, &facts);
        presenter.tool_completed("verify", budget);
        let seq = record_runs(
            ctx,
            turn,
            "wrap_up",
            self.rounds,
            &checked,
            checked.len().checked_sub(1),
            &message,
        );
        Ok((message, seq))
    }

    /// End check at the repair point or the time limit: runs the verifier
    /// and, if it finds new failures, returns the repair notice.
    async fn at_limit(
        &mut self,
        ctx: &mut AgentContext,
        registry: &ToolRegistry,
        presenter: &mut dyn Presenter,
        turn: u64,
        limit: crate::verify::Limit,
    ) -> Result<Option<(String, Option<u64>)>, snafu::Whatever> {
        if !self.end_check(ctx) || !self.changed || self.commands(ctx).is_empty() {
            return Ok(None);
        }
        let Some(checked) = self.run_all(ctx, registry, presenter, turn).await? else {
            return Ok(None);
        };
        let Some(first) = checked.iter().position(|c| !c.new.is_empty()) else {
            record_runs(ctx, turn, "limit", self.rounds, &checked, None, "");
            presenter.tool_completed("verify", "checks pass at the repair point");
            return Ok(None);
        };
        let facts = self.facts(ctx, registry).await;
        let c = &checked[first];
        let message =
            crate::verify::limit_notice(limit, (&c.run.command, c.run.exit, &c.new), &facts);
        let seq = record_runs(
            ctx,
            turn,
            "limit",
            self.rounds,
            &checked,
            Some(first),
            &message,
        );
        presenter.tool_completed(
            "verify",
            &format!("`{}` found new failures; repair notice sent", c.run.command),
        );
        self.repaired = true;
        Ok(Some((message, seq)))
    }

    /// End check: after a repair notice, the verifier's result as the turn
    /// ends at a limit (recorded, not sent).
    async fn final_check(
        &mut self,
        ctx: &mut AgentContext,
        registry: &ToolRegistry,
        presenter: &mut dyn Presenter,
        turn: u64,
    ) -> Result<(), snafu::Whatever> {
        if !self.repaired || !self.end_check(ctx) {
            return Ok(());
        }
        if let Some(checked) = self.run_all(ctx, registry, presenter, turn).await? {
            record_runs(ctx, turn, "end", self.rounds, &checked, None, "");
        }
        Ok(())
    }

    /// Runs the verifier once before the turn's first file change or shell
    /// command (a shell command may change files; whether it did is known
    /// only afterwards).
    async fn before_change(
        &mut self,
        ctx: &mut AgentContext,
        registry: &ToolRegistry,
        presenter: &mut dyn Presenter,
        turn: u64,
    ) -> Result<(), snafu::Whatever> {
        if !self.active(ctx) || self.baseline.is_some() {
            return Ok(());
        }
        let mut base = Vec::new();
        for cmd in self.commands(ctx) {
            let Some(run) = self.run(ctx, registry, presenter, turn, &cmd).await? else {
                self.off = true;
                return Ok(());
            };
            let lines = crate::verify::failure_lines(&run.output);
            record_check(ctx, turn, "baseline", 0, &run, None);
            base.push((run.exit, lines));
        }
        self.baseline = Some(base);
        Ok(())
    }

    /// Checks a final answer. `Some((message, seq))` when the verifier found
    /// new failures and the model gets another round; `None` to accept.
    /// With `no_steps_left` the answer is accepted (as unverified when the
    /// verifier still fails): a round sent back could not be used.
    async fn check_answer(
        &mut self,
        ctx: &mut AgentContext,
        registry: &ToolRegistry,
        presenter: &mut dyn Presenter,
        turn: u64,
        answer: &str,
        no_steps_left: bool,
    ) -> Result<Option<(String, Option<u64>)>, snafu::Whatever> {
        if !self.active(ctx) || !self.changed {
            return Ok(None);
        }
        let commands = self.commands(ctx);
        let end_check = self.end_check(ctx);
        if (commands.is_empty() && !end_check)
            || answer.trim_start().starts_with(crate::verify::UNVERIFIED)
        {
            return Ok(None);
        }
        let max = ctx.config.verify.max_rounds;
        let last = self.rounds >= max || no_steps_left;
        let facts = if end_check && !self.facts_sent {
            self.facts(ctx, registry).await
        } else {
            Default::default()
        };
        for (i, cmd) in commands.iter().enumerate() {
            let Some(run) = self.run(ctx, registry, presenter, turn, cmd).await? else {
                self.off = true;
                return Ok(None);
            };
            if run.exit == Some(0) {
                record_check(ctx, turn, "gate", self.rounds, &run, None);
                presenter.tool_completed("verify", &format!("`{cmd}` passed"));
                continue;
            }
            let current = crate::verify::failure_lines(&run.output);
            let new = match self.baseline.as_ref().and_then(|b| b.get(i)) {
                Some((Some(0), _)) | None => current.clone(),
                Some((_, base)) => crate::verify::new_failures(base, &current),
            };
            if new.is_empty() {
                // Only failures that were there before the turn's changes.
                record_check(ctx, turn, "gate", self.rounds, &run, None);
                presenter.tool_completed("verify", &format!("`{cmd}` fails as before the change"));
                continue;
            }
            if last {
                tracing::warn!(command = %cmd, rounds = self.rounds, "verifier still failing; answer accepted as unverified");
                record_check(ctx, turn, "unverified", self.rounds, &run, None);
                presenter.tool_completed(
                    "verify",
                    &format!("`{cmd}` still fails; accepted as unverified"),
                );
                return Ok(None);
            }
            self.rounds += 1;
            self.facts_sent |= !facts.is_empty();
            let message =
                crate::verify::feedback(Some((cmd, run.exit, &new)), &facts, self.rounds, max);
            let seq = record_check(ctx, turn, "gate", self.rounds, &run, Some(&message));
            presenter.tool_completed(
                "verify",
                &format!(
                    "`{cmd}` found new failures (round {} of {max})",
                    self.rounds
                ),
            );
            return Ok(Some((message, seq)));
        }
        if facts.is_empty() || last {
            return Ok(None);
        }
        // The checks pass, but the turn changed tests or left new files.
        self.rounds += 1;
        self.facts_sent = true;
        let message = crate::verify::feedback(None, &facts, self.rounds, max);
        let seq = record_runs(ctx, turn, "gate", self.rounds, &[], None, &message);
        presenter.tool_completed("verify", "the task changed test files or left new files");
        Ok(Some((message, seq)))
    }
}

/// Runs one verifier command the way a model's execute_bash call runs:
/// approval policy, sandbox, clean environment. `None` if not approved.
async fn run_check(
    ctx: &mut AgentContext,
    registry: &ToolRegistry,
    presenter: &mut dyn Presenter,
    turn: u64,
    command: &str,
    call_id: String,
) -> Result<Option<CheckRun>, snafu::Whatever> {
    let secs = ctx.config.verify.timeout_secs;
    let wrapped = format!(
        "timeout --kill-after=10 {secs} sh -c '{}'",
        command.replace('\'', r"'\''")
    );
    // Approval is judged on the command as configured (so allowlists and
    // prompts see `make test`), execution adds only the timeout.
    let call = crate::tools::ToolCall {
        id: call_id,
        name: "execute_bash".into(),
        args: json!({ "command": command }),
    };
    let decided = approval::policy(&ctx.config, &call, registry.shell_sandboxed());
    let (decision, fields) = if !decided.needs {
        let mut f = json!({ "rule": decided.rule });
        if let Some(m) = &decided.matched {
            f["matched"] = json!(m);
        }
        (decided.decision, f)
    } else if ctx.config.security.approve_all {
        ("auto_approved", json!({ "rule": "approval.approve_all" }))
    } else {
        let asked = Instant::now();
        let reply = presenter
            .request_approval(&call, None)
            .whatever_context("approval prompt failed")?;
        let fields = json!({
            "actor": "operator",
            "answer": reply.answer,
            "shown_sha256": crate::audit::hex(&crate::audit::sha256(&[reply.shown.as_bytes()])),
            "wait_ms": asked.elapsed().as_millis() as u64,
        });
        if reply.decision == Approval::Deny {
            record_approval(ctx, turn, &call.id, "denied", None, fields);
            tracing::warn!(
                command,
                "verifier command not approved; gate off for this turn"
            );
            return Ok(None);
        }
        ("approved", fields)
    };
    record_approval(ctx, turn, &call.id, decision, None, fields);
    // The command's own `timeout` ends it first; the tool's limit only
    // backs that up (its default, [security].bash_timeout_secs, may be lower).
    let env = ToolEnv {
        output_budget: 1 << 20,
        timeout: Some(Duration::from_secs(secs + 30)),
    };
    let started = Instant::now();
    let args = json!({ "command": wrapped });
    let output = match registry.execute("execute_bash", &args, &env).await {
        Ok(out) => out,
        Err(e) => format!("Error: {e}"),
    };
    Ok(Some(CheckRun {
        call_id: call.id,
        command: command.to_string(),
        exit: crate::verify::exit_code(&output),
        output,
        duration_ms: started.elapsed().as_millis() as u64,
    }))
}

/// Writes a `verification` record; returns its seq.
fn record_check(
    ctx: &mut AgentContext,
    turn: u64,
    kind: &str,
    round: usize,
    run: &CheckRun,
    message: Option<&str>,
) -> Option<u64> {
    let stored = ctx.session.stored_output(&run.output);
    let mut fields = json!({ "turn": turn, "kind": kind, "round": round, "command": run.command,
            "exit_code": run.exit, "passed": run.exit == Some(0),
            "duration_ms": run.duration_ms, "output": stored, "message": message });
    // The run is the result of the approval with this call id.
    if !run.call_id.is_empty() {
        fields["call_id"] = json!(run.call_id);
    }
    ctx.session.record("verification", fields)
}

/// Records each run; the message goes with run `with` (by index), or, when
/// there is none, in a record of its own without a command. Returns the
/// seq of the record carrying the message.
fn record_runs(
    ctx: &mut AgentContext,
    turn: u64,
    kind: &str,
    round: usize,
    checked: &[Checked],
    with: Option<usize>,
    message: &str,
) -> Option<u64> {
    let mut carrier = None;
    for (i, c) in checked.iter().enumerate() {
        let m = (with == Some(i) && !message.is_empty()).then_some(message);
        let seq = record_check(ctx, turn, kind, round, &c.run, m);
        if m.is_some() {
            carrier = seq;
        }
    }
    if with.is_none() && !message.is_empty() {
        let none = CheckRun {
            call_id: String::new(),
            command: String::new(),
            exit: None,
            output: String::new(),
            duration_ms: 0,
        };
        carrier = record_check(ctx, turn, kind, round, &none, Some(message));
    }
    carrier
}

/// Tier 1: a file's parse state before an edit, to revert an edit that
/// breaks a file which parsed before.
struct SyntaxGuard {
    path: std::path::PathBuf,
    before: Option<Vec<u8>>,
    parsed_before: bool,
}

impl SyntaxGuard {
    async fn before(registry: &ToolRegistry, call: &crate::tools::ToolCall) -> Option<Self> {
        let path = std::path::PathBuf::from(call.args.get("path")?.as_str()?);
        let before = std::fs::read(&path).ok();
        let parsed_before = match &before {
            None => true, // a new file: noted, not reverted (see `after`)
            Some(_) => !matches!(
                syntax(registry, &path).await,
                crate::verify::Syntax::Error(_)
            ),
        };
        // Only file types with a checker are guarded.
        (crate::verify::syntax_command(&path).is_some()
            || !matches!(
                crate::verify::syntax_check_bytes(&path, b""),
                crate::verify::Syntax::Unknown
            ))
        .then_some(Self {
            path,
            before,
            parsed_before,
        })
    }

    /// After a successful edit with output `out`: `Err(message)` if the
    /// edit was reverted. A new file that does not parse is kept and the
    /// parser output added to `out`: it may be meant that way (a fixture for
    /// a parser test, a dialect the parser does not know).
    async fn after(
        self,
        ctx: &mut AgentContext,
        registry: &ToolRegistry,
        turn: u64,
        out: String,
    ) -> Result<String, String> {
        if !self.parsed_before {
            return Ok(out);
        }
        let crate::verify::Syntax::Error(err) = syntax(registry, &self.path).await else {
            return Ok(out);
        };
        let shown = self.path.display();
        let Some(bytes) = &self.before else {
            ctx.session.record(
                "verification",
                json!({ "turn": turn, "kind": "syntax_note", "round": 0,
                        "command": format!("parse {shown}"), "exit_code": 1, "passed": false,
                        "duration_ms": 0, "output": err, "message": null }),
            );
            return Ok(format!(
                "{out}\nNote: the new file {shown} does not parse; it was written as given. \
                 Parser output:\n{err}"
            ));
        };
        let restored = std::fs::write(&self.path, bytes);
        let msg = match restored {
            Ok(()) => format!(
                "Error: edit reverted: it made {shown} fail to parse, and the file parsed \
                 before. The file is unchanged. Parser output:\n{err}"
            ),
            Err(e) => format!(
                "Error: the edit made {shown} fail to parse ({err}), and restoring it failed: {e}"
            ),
        };
        tracing::warn!(path = %shown, "syntax guard reverted an edit");
        ctx.session.record(
            "verification",
            json!({ "turn": turn, "kind": "syntax", "round": 0, "command": format!("parse {shown}"),
                    "exit_code": 1, "passed": false, "duration_ms": 0, "output": err, "message": null }),
        );
        Err(msg)
    }
}

/// Parses a file with the in-process parser or a parse-only command.
async fn syntax(registry: &ToolRegistry, path: &std::path::Path) -> crate::verify::Syntax {
    use crate::verify::Syntax;
    let Ok(bytes) = std::fs::read(path) else {
        return Syntax::Unknown;
    };
    match crate::verify::syntax_check_bytes(path, &bytes) {
        Syntax::Unknown => {}
        known => return known,
    }
    let Some(cmd) = crate::verify::syntax_command(path) else {
        return Syntax::Unknown;
    };
    let env = ToolEnv {
        output_budget: 1 << 16,
        timeout: None,
    };
    match registry
        .execute("execute_bash", &json!({ "command": cmd }), &env)
        .await
    {
        Ok(out) => match crate::verify::exit_code(&out) {
            Some(0) => Syntax::Ok,
            // 127: the checker (python3) is not installed.
            Some(127) | None => Syntax::Unknown,
            Some(_) => Syntax::Error(crate::verify::failure_lines(&out).join("\n")),
        },
        Err(_) => Syntax::Unknown,
    }
}

/// At the step limit: one request without tools asking for a summary, so the
/// turn ends with an answer. Errors and empty replies give `None`.
async fn final_summary(
    ctx: &mut AgentContext,
    presenter: &mut dyn Presenter,
    turn: u64,
    time_limit: bool,
) -> Option<String> {
    let (action, text) = if time_limit {
        ("final_time", context::TIME_SUMMARY)
    } else {
        ("final", context::FINAL_SUMMARY)
    };
    let seq = ctx.session.record(
        "loop_guard",
        json!({ "turn": turn, "action": action, "repeats": 0, "tool": "" }),
    );
    ctx.add_message(Message::notice(text).with_origin(seq));
    prepare_request(ctx).await;
    ctx.tools_disabled = true;
    let started = Instant::now();
    let result = client::generate_completion(ctx, &mut |d| presenter.stream_delta(d)).await;
    ctx.tools_disabled = false;
    presenter.stream_end();
    let r = match result {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "no final summary at the step limit");
            return None;
        }
    };
    ctx.record_usage(r.usage);
    let usage = r
        .usage
        .map(|u| json!({ "prompt": u.prompt_tokens, "completion": u.completion_tokens }));
    ctx.session.record(
        "model_response",
        json!({ "turn": turn, "step": ctx.config.agent.max_steps, "duration_ms": started.elapsed().as_millis() as u64,
                "content": r.content, "tool_calls": [], "usage": usage,
                "finish_reason": r.finish_reason, "request_sha256": r.request_sha256,
                "response_id": r.response_id, "served_model": r.served_model }),
    );
    let text = r.content.unwrap_or_default();
    if text.trim().is_empty() {
        return None;
    }
    presenter.final_answer(&text);
    ctx.add_message(Message::assistant(&text));
    Some(text)
}

/// For file tools, the file's path and content hash after the call, so a
/// resumed session can tell which files changed since (session-resume.md).
fn file_state(call: &crate::tools::ToolCall) -> Option<Value> {
    if !matches!(call.name.as_str(), "read_file" | "edit_file" | "write_file") {
        return None;
    }
    let path = call.args.get("path")?.as_str()?;
    let bytes = std::fs::read(path).ok()?;
    let hash = crate::transcript::hex(crate::transcript::fnv1a64(&bytes));
    let sha = crate::audit::hex(&crate::audit::sha256(&[&bytes]));
    Some(json!({ "path": path, "fnv1a64": hash, "after_sha256": sha }))
}

/// Required audit mode: stop before acting when the record could not be
/// written (docs/design/design-audit-record.md).
fn audit_gate(ctx: &AgentContext) -> Result<(), snafu::Whatever> {
    if !ctx.session.audit_ok() {
        snafu::whatever!(
            "the audit record could not be written; stopping before the next action (required mode)"
        );
    }
    Ok(())
}

/// Records an approval decision; `fields` adds the rule or the operator's
/// answer (docs/design/design-audit-record.md).
pub fn record_approval(
    ctx: &mut AgentContext,
    turn: u64,
    call_id: &str,
    decision: &str,
    preview: Option<&str>,
    fields: Value,
) {
    let mut record =
        json!({ "turn": turn, "call_id": call_id, "decision": decision, "preview": preview });
    if let (Some(r), Value::Object(f)) = (record.as_object_mut(), fields) {
        r.extend(f);
    }
    ctx.session.record("approval", record);
}

/// Emits cumulative token accounting for the task via the observability layer.
pub fn log_token_summary(ctx: &AgentContext) {
    let led = ctx.token_ledger();
    tracing::info!(
        requests = led.requests,
        prompt_tokens = led.prompt_tokens,
        completion_tokens = led.completion_tokens,
        total_tokens = led.total_tokens,
        unreported_requests = led.unreported_requests,
        "token usage (task total)"
    );
}
