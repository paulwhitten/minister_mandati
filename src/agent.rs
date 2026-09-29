//! The agent loop: one turn from instruction to final answer, shared by the
//! interactive and one-shot modes and the evaluation runner. Each step:
//! bring the request within budget (`docs/context.md`), call the model, run
//! the requested tools behind approval, and record everything to the session
//! transcript (`docs/sessions.md`).

use serde_json::{Value, json};
use snafu::prelude::*;
use std::time::Instant;

use crate::approval::needs_approval;
use crate::budget;
use crate::client;
use crate::context::{self, AgentContext, Message};
use crate::loop_guard::{Intervention, LoopGuards};
use crate::presenter::{Approval, Presenter};
use crate::tools::{ToolEnv, ToolRegistry, truncate_middle};

/// Retries after the server rejects a prompt as too long (see docs/context.md).
pub const MAX_OVERFLOW_RETRIES: u32 = 2;

/// How a turn ended.
pub enum TurnOutcome {
    Answered(String),
    LoopGuard,
    StepCap,
}

/// Sets the context window: `[context].window` if configured, else what the
/// server reports, else the fallback (with a warning, since it may be wrong).
pub async fn configure_window(ctx: &mut AgentContext) {
    if ctx.config.context.window.is_some() {
        return;
    }
    match client::discover_context_window(&ctx.config).await {
        Some(w) => {
            tracing::info!(window = w, "context window discovered from server");
            ctx.set_window(w);
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
    ctx.session.record(
        "turn_end",
        json!({ "turn": turn, "outcome": outcome, "answer": answer, "error": error, "tokens": tokens }),
    );
}

pub fn record_outcome(ctx: &mut AgentContext, result: &Result<TurnOutcome, snafu::Whatever>) {
    match result {
        Ok(TurnOutcome::Answered(a)) => finish_turn(ctx, "answered", Some(a), None),
        Ok(TurnOutcome::LoopGuard) => finish_turn(ctx, "loop_guard", None, None),
        Ok(TurnOutcome::StepCap) => finish_turn(ctx, "step_cap", None, None),
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
    tracing::info!(instruction = %instruction, "starting task");
    presenter.task_started(instruction);
    ctx.begin_turn(instruction);
    let turn = ctx.session.next_turn();
    ctx.session.record(
        "turn_start",
        json!({ "turn": turn, "instruction": instruction }),
    );

    let max_steps = ctx.config.agent.max_steps;
    let mut guards = LoopGuards::new(
        ctx.config.agent.dedupe_identical_writes,
        ctx.config.agent.loop_guard_window,
        ctx.config.agent.loop_guard_repeat_threshold,
    );

    for step in 0..max_steps {
        let span = tracing::info_span!("step", n = step);
        let _enter = span.enter();

        let mut overflow_retries = 0;
        let response = loop {
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
                    ctx.session.record(
                        "model_response",
                        json!({ "turn": turn, "step": step, "duration_ms": duration_ms,
                                "content": r.content, "tool_calls": calls, "usage": usage,
                                "counted_prompt_tokens": counted.tokens,
                                "count_source": counted.token_source }),
                    );
                    break r;
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
            _ => {
                let final_msg = response.content.clone().unwrap_or_default();
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

        ctx.add_message(Message::assistant_tool_calls(&response));

        // A nudge is a user message, so it must wait until every tool result
        // of this step is in; results have to follow their request directly.
        let mut nudge = false;
        for call in calls {
            tracing::info!(tool = %call.name, args = %call.args, "tool requested");
            presenter.tool_requested(&call);

            let fp = LoopGuards::fingerprint(&call.name, &call.args);
            let repeats = guards.observe(fp);
            let policy = registry.dedupe_policy(&call.name);

            let started = Instant::now();
            let output = if let Some(skipped) = guards.skip_result(&call.name, policy, fp) {
                tracing::info!(tool = %call.name, "duplicate call skipped");
                record_approval(ctx, turn, &call.id, "skipped_duplicate", None);
                skipped
            } else {
                let needs = needs_approval(&ctx.config, &call);
                // Tools with a preview (edits, overwrites) validate first and
                // show the operator exactly what will change.
                let preview = if needs {
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
                        record_approval(ctx, turn, &call.id, "not_requested_invalid", None);
                        invalid
                    }
                    Ok(preview) => {
                        let decision = if needs {
                            let decision = presenter
                                .request_approval(&call, preview.as_deref())
                                .whatever_context("approval prompt failed")?;
                            if decision == Approval::Deny {
                                record_approval(ctx, turn, &call.id, "denied", preview.as_deref());
                                let denied = "Error: user denied permission for this action.";
                                ctx.session.record(
                                    "tool_result",
                                    json!({ "turn": turn, "call_id": call.id, "tool": call.name,
                                            "duration_ms": 0, "failed": true, "executed": false,
                                            "bytes": denied.len(), "sent_bytes": denied.len(),
                                            "output": denied }),
                                );
                                ctx.add_message(Message::tool_result(&call.id, denied));
                                continue;
                            }
                            "approved"
                        } else if call.name == "execute_bash"
                            && ctx.config.security.require_approval_for_bash
                        {
                            "auto_approved"
                        } else {
                            "not_required"
                        };
                        record_approval(ctx, turn, &call.id, decision, preview.as_deref());
                        let env = ToolEnv {
                            output_budget: ctx.output_cap_bytes(),
                        };
                        match registry.execute(&call.name, &call.args, &env).await {
                            Ok(out) => {
                                guards.record_success(policy, fp, &out);
                                out
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
            let sent = truncate_middle(output.clone(), ctx.output_cap_bytes());
            let stored = ctx.session.stored_output(&output);
            ctx.session.record(
                "tool_result",
                json!({ "turn": turn, "call_id": call.id, "tool": call.name,
                        "duration_ms": started.elapsed().as_millis() as u64,
                        "failed": context::is_failure(&output),
                        "bytes": output.len(), "sent_bytes": sent.len(), "output": stored }),
            );
            presenter.tool_completed(&call.name, &sent);
            ctx.add_message(Message::tool_result(&call.id, &sent));

            match guards.intervention(repeats) {
                Intervention::None => {}
                Intervention::Nudge => {
                    tracing::warn!(repeats, tool = %call.name, "loop guard: nudge injected");
                    ctx.session.record(
                        "loop_guard",
                        json!({ "turn": turn, "action": "nudge", "repeats": repeats, "tool": call.name }),
                    );
                    nudge = true;
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
        if nudge {
            ctx.add_message(Message::user(
                "A repeated identical action was detected that already succeeded. \
                 If the task is complete, reply without further tool calls.",
            ));
        }
    }

    tracing::warn!(max = max_steps, "reached step cap without completion");
    presenter.step_cap_reached(max_steps);
    log_token_summary(ctx);
    Ok(TurnOutcome::StepCap)
}

pub fn record_approval(
    ctx: &mut AgentContext,
    turn: u64,
    call_id: &str,
    decision: &str,
    preview: Option<&str>,
) {
    ctx.session.record(
        "approval",
        json!({ "turn": turn, "call_id": call_id, "decision": decision, "preview": preview }),
    );
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
