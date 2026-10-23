//! The agent loop: one turn from instruction to final answer, shared by the
//! interactive and one-shot modes and the evaluation runner. Each step:
//! bring the request within budget (`docs/context.md`), call the model, run
//! the requested tools behind approval, and record everything to the session
//! transcript (`docs/sessions.md`).

use serde_json::{Value, json};
use snafu::prelude::*;
use std::time::Instant;

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

    let mut continues = 0;
    for step in 0..max_steps {
        let span = tracing::info_span!("step", n = step);
        let _enter = span.enter();

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
                        let env = ToolEnv {
                            output_budget: ctx.output_cap_bytes(),
                        };
                        // Required mode: the approval is on disk before the action.
                        audit_gate(ctx)?;
                        let watch = if call.name == "execute_bash" && ctx.session.is_recording() {
                            crate::evidence::marks(registry.runner()).await
                        } else {
                            None
                        };
                        let ran = Instant::now();
                        let executed = registry.execute(&call.name, &call.args, &env).await;
                        ran_ms = ran.elapsed().as_millis() as u64;
                        if let Some((_, before)) = watch
                            && let Some((_, after)) =
                                crate::evidence::marks(registry.runner()).await
                        {
                            workspace_changes = Some(crate::evidence::changes(&before, &after));
                        }
                        match executed {
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

    tracing::warn!(max = max_steps, "reached step cap without completion");
    presenter.step_cap_reached(max_steps);
    let summary = final_summary(ctx, presenter, turn).await;
    log_token_summary(ctx);
    Ok(TurnOutcome::StepCap(summary))
}

/// At the step limit: one request without tools asking for a summary, so the
/// turn ends with an answer. Errors and empty replies give `None`.
async fn final_summary(
    ctx: &mut AgentContext,
    presenter: &mut dyn Presenter,
    turn: u64,
) -> Option<String> {
    let seq = ctx.session.record(
        "loop_guard",
        json!({ "turn": turn, "action": "final", "repeats": 0, "tool": "" }),
    );
    ctx.add_message(Message::notice(context::FINAL_SUMMARY).with_origin(seq));
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
