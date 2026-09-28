use clap::Parser;
use snafu::prelude::*;
use std::io::{self, IsTerminal, Read};

mod approval;
mod budget;
mod client;
mod config;
mod context;
// Phase 2 evaluation subsystem; scaffolding, not yet wired into the CLI.
#[allow(dead_code)]
mod eval;
mod loop_guard;
mod presenter;
mod schema;
mod session;
mod tools;

use approval::needs_approval;
use config::Config;
use context::{AgentContext, Message};
use loop_guard::{Intervention, LoopGuards};
use presenter::{Approval, CliPresenter, Presenter};
use serde_json::{Value, json};
use std::time::Instant;
use tools::{ToolEnv, ToolRegistry, truncate_middle};

/// Retries after the server rejects a prompt as too long (see docs/context.md).
const MAX_OVERFLOW_RETRIES: u32 = 2;

/// minister_mandati (`mima`) — an auditable, edge-native coding agent.
///
/// Run a one-shot instruction, or start an interactive REPL when invoked with no
/// instruction on a terminal.
#[derive(Parser, Debug)]
#[command(
    name = "mima",
    version,
    about = "Auditable, edge-native coding agent",
    after_help = "\
Modes:
  mima                     Interactive REPL (no instruction on a terminal)
  mima \"<instruction>\"     Run once, then exit
  mima -i \"<instruction>\"  Run the instruction, then stay in the REPL
  echo \"...\" | mima        Run piped text once, then exit"
)]
struct Cli {
    /// Instruction to run. If omitted on a terminal, an interactive REPL starts;
    /// if omitted with piped stdin, the piped text is used as the instruction.
    instruction: Vec<String>,

    /// Stay in the interactive REPL after running INSTRUCTION (implied when no
    /// instruction is given on a terminal).
    #[arg(short, long)]
    interactive: bool,

    /// Write a transcript of this session to the transcript directory
    /// (default ~/.mima/transcripts). Off by default.
    #[arg(long)]
    transcript: bool,
}

/// How mima was invoked; recorded in the transcript and used for messages.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    OneShot,
    Piped,
    Interactive,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::OneShot => "one_shot",
            Mode::Piped => "piped",
            Mode::Interactive => "interactive",
        }
    }
}

/// How a turn ended.
enum TurnOutcome {
    Answered(String),
    LoopGuard,
    StepCap,
}

#[tokio::main]
async fn main() {
    init_tracing();
    let cli = Cli::parse();
    let mut presenter = CliPresenter::default();
    if let Err(e) = run(cli, &mut presenter).await {
        tracing::error!(error = %e, "fatal error");
        eprintln!("error: {e}");
        let mut source = std::error::Error::source(&e);
        while let Some(cause) = source {
            eprintln!("  caused by: {cause}");
            source = cause.source();
        }
        std::process::exit(1);
    }
}

#[tracing::instrument(skip_all)]
async fn run(cli: Cli, presenter: &mut dyn Presenter) -> Result<(), snafu::Whatever> {
    let config = Config::load_or_discover().whatever_context("failed to load configuration")?;
    let registry = ToolRegistry::init_default(&config);
    let specs = registry.specs();
    let mut ctx = AgentContext::new(config, specs);
    configure_window(&mut ctx).await;
    configure_token_counter(&mut ctx).await;

    let joined = cli.instruction.join(" ");
    let mut task = joined.trim().to_string();
    let mode = if cli.interactive || (task.is_empty() && io::stdin().is_terminal()) {
        Mode::Interactive
    } else if task.is_empty() {
        let mut piped = String::new();
        io::stdin()
            .read_to_string(&mut piped)
            .whatever_context("failed to read stdin")?;
        task = piped.trim().to_string();
        if task.is_empty() {
            return Ok(());
        }
        Mode::Piped
    } else {
        Mode::OneShot
    };

    if cli.transcript || ctx.config.session.transcripts {
        enable_transcript(&mut ctx, mode);
    }
    if mode != Mode::Interactive {
        announce_transcript(&ctx, mode); // the REPL announces after its banner
    }

    let result = match mode {
        Mode::Interactive => {
            let seed = (!task.is_empty()).then_some(task);
            run_repl(&mut ctx, &registry, presenter, seed).await
        }
        _ => run_single_shot(&mut ctx, &registry, presenter, &task)
            .await
            .map(|()| "task_done"),
    };
    match &result {
        Ok(reason) => end_session(&mut ctx, reason, None),
        Err(e) => end_session(&mut ctx, "fatal", Some(e.to_string())),
    }
    result.map(|_| ())
}

/// Sets the context window: `[context].window` if configured, else what the
/// server reports, else the fallback (with a warning, since it may be wrong).
async fn configure_window(ctx: &mut AgentContext) {
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
async fn configure_token_counter(ctx: &mut AgentContext) {
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
async fn count_exactly(ctx: &mut AgentContext) {
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
async fn prepare_request(ctx: &mut AgentContext) {
    ctx.normalize();
    count_exactly(ctx).await;
    if ctx.compact() {
        count_exactly(ctx).await;
    }
    ctx.seal_request();
}

/// Starts recording the current session. Returns false (after telling the
/// operator) if the transcript cannot be opened; the agent keeps working.
fn enable_transcript(ctx: &mut AgentContext, mode: Mode) -> bool {
    let c = &ctx.config;
    let b = ctx.stats().budget;
    let header = json!({
        "mode": mode.as_str(),
        "mima_version": env!("CARGO_PKG_VERSION"),
        "cwd": std::env::current_dir().ok(),
        "model": c.provider.default_model,
        "base_url": c.provider.base_url,
        "window": b.window,
        "budget": b.operating,
        "approvals": {
            "bash": c.security.require_approval_for_bash,
            "writes": c.security.require_approval_for_writes,
            "auto_approve_bash": c.security.auto_approve_bash,
        },
        "allowed_paths": c.security.allowed_paths,
        "token_counting": if ctx.token_counter.is_some() { "tokenize" } else { "usage+estimate" },
    });
    match ctx.session.enable(header) {
        Ok(path) => {
            tracing::info!(path = %path.display(), "transcript enabled");
            true
        }
        Err(e) => {
            eprintln!("could not enable transcripts: {e}");
            tracing::warn!(error = %e, "transcript could not be opened");
            false
        }
    }
}

/// States at startup whether this session is being recorded. Interactive
/// mode prints with the banner; one-shot modes print to stderr so stdout
/// stays reserved for the answer.
fn announce_transcript(ctx: &AgentContext, mode: Mode) {
    let msg = match ctx.session.transcript_path() {
        Some(path) => format!("Transcripts: on ({})", path.display()),
        None if mode == Mode::Interactive => {
            "Transcripts: off. Enable with /enable_transcript.".to_string()
        }
        None => "Transcripts: off. Enable with --transcript.".to_string(),
    };
    if mode == Mode::Interactive {
        println!("{msg}");
    } else {
        eprintln!("{msg}");
    }
}

/// Writes `session_end` (when recording) with the session's token totals.
fn end_session(ctx: &mut AgentContext, reason: &str, error: Option<String>) {
    let tokens = ledger_json(ctx);
    ctx.session
        .end(reason, json!({ "tokens": tokens, "error": error }));
}

fn ledger_json(ctx: &AgentContext) -> Value {
    let l = ctx.token_ledger();
    json!({
        "requests": l.requests,
        "prompt": l.prompt_tokens,
        "completion": l.completion_tokens,
        "unreported_requests": l.unreported_requests,
    })
}

/// Records how a turn ended (`turn_end`).
fn finish_turn(ctx: &mut AgentContext, outcome: &str, answer: Option<&str>, error: Option<String>) {
    let tokens = ledger_json(ctx);
    let turn = ctx.session.turn();
    ctx.session.record(
        "turn_end",
        json!({ "turn": turn, "outcome": outcome, "answer": answer, "error": error, "tokens": tokens }),
    );
}

fn record_outcome(ctx: &mut AgentContext, result: &Result<TurnOutcome, snafu::Whatever>) {
    match result {
        Ok(TurnOutcome::Answered(a)) => finish_turn(ctx, "answered", Some(a), None),
        Ok(TurnOutcome::LoopGuard) => finish_turn(ctx, "loop_guard", None, None),
        Ok(TurnOutcome::StepCap) => finish_turn(ctx, "step_cap", None, None),
        Err(e) => finish_turn(ctx, "error", None, Some(e.to_string())),
    }
}

/// One task with top-level Ctrl-C cancellation (exit 130).
async fn run_single_shot(
    ctx: &mut AgentContext,
    registry: &ToolRegistry,
    presenter: &mut dyn Presenter,
    instruction: &str,
) -> Result<(), snafu::Whatever> {
    let result = tokio::select! {
        result = run_turn(ctx, registry, presenter, instruction) => Some(result),
        _ = tokio::signal::ctrl_c() => None,
    };
    match result {
        Some(result) => {
            record_outcome(ctx, &result);
            result.map(|_| ())
        }
        None => {
            eprintln!("\ninterrupted");
            tracing::info!("cancelled by user (ctrl-c)");
            finish_turn(ctx, "cancelled", None, None);
            end_session(ctx, "interrupted", None);
            std::process::exit(130);
        }
    }
}

/// Interactive loop over a persistent `AgentContext`. Ctrl-C cancels the
/// current turn and returns to the prompt; Ctrl-D (EOF) or `/exit` quits.
/// Returns why the session ended.
async fn run_repl(
    ctx: &mut AgentContext,
    registry: &ToolRegistry,
    presenter: &mut dyn Presenter,
    seed: Option<String>,
) -> Result<&'static str, snafu::Whatever> {
    use tokio::signal::unix::{SignalKind, signal};

    // A persistent SIGINT stream keeps Ctrl-C from killing the REPL process.
    let mut sigint =
        signal(SignalKind::interrupt()).whatever_context("failed to install SIGINT handler")?;

    println!("minister_mandati (mima) — interactive mode. /help for commands, Ctrl-D to exit.");
    announce_transcript(ctx, Mode::Interactive);

    // Line editing (arrow keys, Home/End, Delete, history with Up/Down).
    // History stays in memory for this process; nothing is written to disk.
    let mut editor =
        rustyline::DefaultEditor::new().whatever_context("failed to initialize line editor")?;

    if let Some(seed) = seed {
        run_turn_cancellable(ctx, registry, presenter, &seed, &mut sigint).await;
    }

    loop {
        println!();
        let line = match editor.readline("> ") {
            Ok(line) => line,
            // Ctrl-C at the prompt clears the line, as in a shell.
            Err(rustyline::error::ReadlineError::Interrupted) => continue,
            Err(rustyline::error::ReadlineError::Eof) => return Ok("eof"), // Ctrl-D
            Err(e) => return Err(e).whatever_context("failed to read input"),
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let _ = editor.add_history_entry(line);
        if let Some(cmd) = line.strip_prefix('/') {
            match cmd {
                "exit" | "quit" => return Ok("exit"),
                "new" | "reset" => new_session(ctx, registry),
                "session" => print_session(ctx),
                "enable_transcript" => {
                    if let Some(path) = ctx.session.transcript_path() {
                        println!("Transcripts: already on ({})", path.display());
                    } else if enable_transcript(ctx, Mode::Interactive) {
                        announce_transcript(ctx, Mode::Interactive);
                    }
                }
                "disable_transcript" => {
                    ctx.session.disable();
                    println!("Transcripts: off.");
                }
                "tokens" => print_tokens(ctx),
                "context" => print_context(ctx),
                "help" => print_help(),
                other => println!("unknown command: /{other} (try /help)"),
            }
            continue;
        }
        run_turn_cancellable(ctx, registry, presenter, line, &mut sigint).await;
    }
}

/// `/new`: ends the current session and starts a fresh one (new id, empty
/// context). Recording carries over: if the old session had a transcript,
/// the new one gets its own.
fn new_session(ctx: &mut AgentContext, registry: &ToolRegistry) {
    let recording = ctx.session.is_recording();
    end_session(ctx, "new", None);
    ctx.reset();
    registry.reset_files();
    ctx.session = ctx.session.successor();
    if recording {
        enable_transcript(ctx, Mode::Interactive);
    }
    println!("New session {}.", ctx.session.id());
    announce_transcript(ctx, Mode::Interactive);
}

/// Prints the session id, start time and transcript state (REPL `/session`).
fn print_session(ctx: &AgentContext) {
    let s = &ctx.session;
    let transcript = s
        .transcript_path()
        .map_or("off".to_string(), |p| p.display().to_string());
    println!(
        "session {} — started {}, turns: {}, transcript: {transcript}",
        s.id(),
        s.started_utc(),
        s.turn()
    );
}

/// Run one turn but abandon it cleanly on Ctrl-C, rolling history back to its
/// pre-turn checkpoint so no partial (tool-call-without-result) turn lingers.
async fn run_turn_cancellable(
    ctx: &mut AgentContext,
    registry: &ToolRegistry,
    presenter: &mut dyn Presenter,
    instruction: &str,
    sigint: &mut tokio::signal::unix::Signal,
) {
    let checkpoint = ctx.checkpoint();
    let result = tokio::select! {
        result = run_turn(ctx, registry, presenter, instruction) => Some(result),
        _ = sigint.recv() => None,
    };
    match result {
        Some(result) => {
            record_outcome(ctx, &result);
            if let Err(e) = result {
                eprintln!("error: {e}"); // keep the REPL alive on turn errors
            }
        }
        None => {
            ctx.rollback_to(checkpoint);
            finish_turn(ctx, "cancelled", None, None);
            eprintln!("\n^C — cancelled; back to prompt.");
        }
    }
}

/// A single agent task: seed the instruction, then run the tool loop to a final
/// answer, a loop-guard stop, or the step cap.
async fn run_turn(
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

fn record_approval(
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

/// Prints the context budget and current estimate (REPL `/context`).
fn print_context(ctx: &AgentContext) {
    let s = ctx.stats();
    let b = s.budget;
    let pct = s.tokens * 100 / b.operating.max(1);
    println!(
        "context — window: {}, usable: {}, budget: {}, tokens: {} ({pct}% of budget, {}), \
         counting: {}, messages: {}, masked: {}, evicted: {}",
        b.window,
        b.usable,
        b.operating,
        s.tokens,
        s.token_source,
        if ctx.token_counter.is_some() {
            "exact (/tokenize)"
        } else {
            "usage + estimate"
        },
        s.messages,
        s.masked_total,
        s.evicted_total
    );
}

/// Prints the cumulative token ledger to stdout (REPL `/tokens`).
fn print_tokens(ctx: &AgentContext) {
    let l = ctx.token_ledger();
    println!(
        "tokens — requests: {}, prompt: {}, completion: {}, total: {}, unreported: {}",
        l.requests, l.prompt_tokens, l.completion_tokens, l.total_tokens, l.unreported_requests
    );
}

/// Lists the REPL slash-commands.
fn print_help() {
    println!(
        "commands:\n  \
         /help                show this help\n  \
         /tokens              show token usage\n  \
         /context             show context budget\n  \
         /session             show session id, start time and transcript\n  \
         /new                 start a new session (alias: /reset)\n  \
         /enable_transcript   record this session to a transcript\n  \
         /disable_transcript  stop recording\n  \
         /exit                quit (also Ctrl-D)"
    );
}

/// Emits cumulative token accounting for the task via the observability layer.
fn log_token_summary(ctx: &AgentContext) {
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

/// Human-readable logs by default; set `MIMA_LOG_FORMAT=json` for structured logs.
/// Filter with `MIMA_LOG` (e.g. `MIMA_LOG=debug`).
fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_env("MIMA_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let json = std::env::var("MIMA_LOG_FORMAT")
        .map(|v| v.eq_ignore_ascii_case("json"))
        .unwrap_or(false);
    if json {
        fmt()
            .with_env_filter(filter)
            .with_writer(io::stderr)
            .json()
            .init();
    } else {
        fmt().with_env_filter(filter).with_writer(io::stderr).init();
    }
}
