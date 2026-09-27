use clap::Parser;
use snafu::prelude::*;
use std::io::{self, IsTerminal, Read, Write};

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
mod tools;

use approval::needs_approval;
use config::Config;
use context::{AgentContext, Message};
use loop_guard::{Intervention, LoopGuards};
use presenter::{Approval, CliPresenter, Presenter};
use tools::ToolRegistry;

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

    let joined = cli.instruction.join(" ");
    let instruction = joined.trim();

    if cli.interactive {
        let seed = (!instruction.is_empty()).then(|| instruction.to_string());
        return run_repl(&mut ctx, &registry, presenter, seed).await;
    }

    if !instruction.is_empty() {
        return run_single_shot(&mut ctx, &registry, presenter, instruction).await;
    }

    // No instruction: REPL on a terminal, otherwise consume piped stdin once.
    if io::stdin().is_terminal() {
        run_repl(&mut ctx, &registry, presenter, None).await
    } else {
        let mut piped = String::new();
        io::stdin()
            .read_to_string(&mut piped)
            .whatever_context("failed to read stdin")?;
        let piped = piped.trim();
        if piped.is_empty() {
            return Ok(());
        }
        run_single_shot(&mut ctx, &registry, presenter, piped).await
    }
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

/// One task with top-level Ctrl-C cancellation (exit 130).
async fn run_single_shot(
    ctx: &mut AgentContext,
    registry: &ToolRegistry,
    presenter: &mut dyn Presenter,
    instruction: &str,
) -> Result<(), snafu::Whatever> {
    tokio::select! {
        result = run_turn(ctx, registry, presenter, instruction) => result,
        _ = tokio::signal::ctrl_c() => {
            eprintln!("\ninterrupted");
            tracing::info!("cancelled by user (ctrl-c)");
            std::process::exit(130);
        }
    }
}

/// Interactive loop over a single persistent `AgentContext`. Ctrl-C cancels the
/// current turn and returns to the prompt; Ctrl-D (EOF) or `/exit` quits.
async fn run_repl(
    ctx: &mut AgentContext,
    registry: &ToolRegistry,
    presenter: &mut dyn Presenter,
    seed: Option<String>,
) -> Result<(), snafu::Whatever> {
    use tokio::signal::unix::{SignalKind, signal};

    // A persistent SIGINT stream keeps Ctrl-C from killing the REPL process.
    let mut sigint =
        signal(SignalKind::interrupt()).whatever_context("failed to install SIGINT handler")?;

    println!("minister_mandati (mima) — interactive mode. /help for commands, Ctrl-D to exit.");

    if let Some(seed) = seed {
        run_turn_cancellable(ctx, registry, presenter, &seed, &mut sigint).await;
    }

    loop {
        print!("\n> ");
        io::stdout().flush().ok();
        let mut line = String::new();
        let read = io::stdin()
            .read_line(&mut line)
            .whatever_context("failed to read input")?;
        if read == 0 {
            println!();
            break; // Ctrl-D / EOF
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(cmd) = line.strip_prefix('/') {
            match cmd {
                "exit" | "quit" => break,
                "reset" => {
                    ctx.reset();
                    println!("context reset.");
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
    Ok(())
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
    let cancelled = tokio::select! {
        result = run_turn(ctx, registry, presenter, instruction) => {
            if let Err(e) = result {
                eprintln!("error: {e}"); // keep the REPL alive on turn errors
            }
            false
        }
        _ = sigint.recv() => true,
    };
    if cancelled {
        ctx.rollback_to(checkpoint);
        eprintln!("\n^C — cancelled; back to prompt.");
    }
}

/// A single agent task: seed the instruction, then run the tool loop to a final
/// answer or the step cap.
async fn run_turn(
    ctx: &mut AgentContext,
    registry: &ToolRegistry,
    presenter: &mut dyn Presenter,
    instruction: &str,
) -> Result<(), snafu::Whatever> {
    tracing::info!(instruction = %instruction, "starting task");
    presenter.task_started(instruction);
    ctx.begin_turn(instruction);

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
            ctx.prepare_request();
            let result =
                client::generate_completion(ctx, &mut |delta| presenter.stream_delta(delta)).await;
            // Terminate any streamed line before logging, so log lines never run
            // into the model's output.
            presenter.stream_end();
            match result {
                Err(e) if e.is_context_overflow() && overflow_retries < MAX_OVERFLOW_RETRIES => {
                    overflow_retries += 1;
                    tracing::warn!(error = %e, attempt = overflow_retries, "prompt too long for the model; compacting and retrying");
                    if !ctx.handle_overflow(e.reported_window(), e.reported_prompt_tokens()) {
                        snafu::whatever!(
                            "the conversation no longer fits the model's context window \
                             and nothing more can be removed: {e}"
                        );
                    }
                }
                other => break other.whatever_context("model completion failed")?,
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
                return Ok(());
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

            let payload = if let Some(skipped) = guards.skip_result(&call.name, policy, fp) {
                tracing::info!(tool = %call.name, "duplicate call skipped");
                skipped
            } else {
                if needs_approval(&ctx.config, &call) {
                    let decision = presenter
                        .request_approval(&call)
                        .whatever_context("approval prompt failed")?;
                    if decision == Approval::Deny {
                        ctx.add_message(Message::tool_result(
                            &call.id,
                            "Error: user denied permission for this action.",
                        ));
                        continue;
                    }
                }
                match registry
                    .execute(&call.name, &call.args, ctx.output_cap_bytes())
                    .await
                {
                    Ok(out) => {
                        guards.record_success(policy, fp, &out);
                        out
                    }
                    Err(e) => {
                        tracing::warn!(tool = %call.name, error = %e, "tool failed");
                        format!("Error: {e}")
                    }
                }
            };
            presenter.tool_completed(&call.name, &payload);
            ctx.add_message(Message::tool_result(&call.id, &payload));

            match guards.intervention(repeats) {
                Intervention::None => {}
                Intervention::Nudge => {
                    tracing::warn!(repeats, tool = %call.name, "loop guard: nudge injected");
                    nudge = true;
                }
                Intervention::Terminate => {
                    tracing::warn!(repeats, tool = %call.name, "loop guard tripped: terminating turn");
                    ctx.normalize();
                    presenter.loop_detected(repeats);
                    log_token_summary(ctx);
                    return Ok(());
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
    Ok(())
}

/// Prints the context budget and current estimate (REPL `/context`).
fn print_context(ctx: &AgentContext) {
    let s = ctx.stats();
    let b = s.budget;
    let pct = s.estimated_tokens * 100 / b.operating.max(1);
    println!(
        "context — window: {}, usable: {}, budget: {}, estimated: {} ({pct}% of budget, {}), \
         messages: {}, masked: {}, evicted: {}",
        b.window,
        b.usable,
        b.operating,
        s.estimated_tokens,
        s.estimate_source,
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
        "commands:\n  /help    show this help\n  /tokens  show token usage\n  /context show context budget\n  /reset   \
         clear the conversation\n  /exit    quit (also Ctrl-D)"
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
