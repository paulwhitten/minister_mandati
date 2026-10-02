use clap::Parser;
use snafu::prelude::*;
use std::io::{self, IsTerminal, Read};
use std::path::Path;

mod agent;
mod approval;
mod budget;
mod client;
mod config;
mod context;
mod loop_guard;
mod presenter;
#[cfg(test)]
mod resume_tests;
mod schema;
#[cfg(test)]
mod schema_tests;
mod session;
#[cfg(test)]
mod testutil;
mod tools;
mod transcript;

use agent::{finish_turn, ledger_json, record_outcome, run_turn};
use config::Config;
use context::AgentContext;
use presenter::{CliPresenter, Presenter};
use serde_json::json;
use tools::ToolRegistry;

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

    /// Use this config file instead of discovering agent.toml.
    #[arg(long, value_name = "FILE")]
    config: Option<std::path::PathBuf>,

    /// Write the transcript to FILE (implies --transcript).
    #[arg(long, value_name = "FILE")]
    transcript_path: Option<std::path::PathBuf>,

    /// Override [agent].max_steps for this run.
    #[arg(long, value_name = "N")]
    max_steps: Option<usize>,

    /// List recorded sessions (newest first) and exit.
    #[arg(long)]
    sessions: bool,

    /// Continue a recorded session: an id (a unique prefix is enough) or
    /// "last". Opens the interactive prompt; an INSTRUCTION runs first.
    #[arg(long, value_name = "ID")]
    resume: Option<String>,

    /// Approve every tool call without asking. For the evaluation harness
    /// only: refused unless MIMA_EVAL_SANDBOX=1 is set (see docs/eval.md).
    #[arg(long)]
    approve_all: bool,
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

#[tokio::main]
async fn main() {
    init_tracing();
    let cli = Cli::parse();
    let mut presenter = CliPresenter::default();
    if cli.approve_all {
        if std::env::var("MIMA_EVAL_SANDBOX").as_deref() != Ok("1") {
            eprintln!(
                "error: --approve-all is only for the evaluation harness (docs/eval.md); \
                 it requires MIMA_EVAL_SANDBOX=1"
            );
            std::process::exit(2);
        }
        presenter.approve_all = true;
    }
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
    let mut config = match &cli.config {
        Some(path) => Config::load(path).whatever_context("failed to load configuration")?,
        None => Config::load_or_discover().whatever_context("failed to load configuration")?,
    };
    if let Some(n) = cli.max_steps {
        config.agent.max_steps = n;
    }
    let registry = ToolRegistry::init_default(&config);
    let specs = registry.specs();
    let mut ctx = AgentContext::new(config, specs);
    agent::configure_window(&mut ctx).await;
    agent::configure_token_counter(&mut ctx).await;

    if cli.sessions {
        print_sessions(&ctx);
        return Ok(());
    }

    let joined = cli.instruction.join(" ");
    let mut task = joined.trim().to_string();
    let mode = if cli.resume.is_some()
        || cli.interactive
        || (task.is_empty() && io::stdin().is_terminal())
    {
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

    if let Some(path) = &cli.transcript_path {
        ctx.session.set_path(path.clone());
    }
    if let Some(id) = &cli.resume {
        resume_session(&mut ctx, id).whatever_context("cannot resume")?;
    } else if cli.transcript || cli.transcript_path.is_some() || ctx.config.session.transcripts {
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
                "sessions" => print_sessions(ctx),
                c if c.starts_with("resume") => {
                    let id = c.trim_start_matches("resume").trim();
                    let id = if id.is_empty() { "last" } else { id };
                    end_session(ctx, "resume", None);
                    ctx.reset();
                    registry.reset_files();
                    ctx.session = ctx.session.successor();
                    if let Err(e) = resume_session(ctx, id) {
                        println!("cannot resume: {e}");
                    }
                }
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

/// Prints recorded sessions, newest first (`--sessions`, `/sessions`).
fn print_sessions(ctx: &AgentContext) {
    let dir = session::expand_home(&ctx.config.session.transcript_dir);
    let sessions = transcript::list_sessions(&dir);
    if sessions.is_empty() {
        println!("No recorded sessions in {}.", dir.display());
        return;
    }
    for s in sessions.iter().take(30) {
        let first: String = s.first_instruction.chars().take(60).collect();
        let model = s.model.rsplit('/').next().unwrap_or(&s.model);
        println!(
            "{}  started {}  {} turns  {:<11}  {}  \"{}\"{}",
            s.id,
            s.started.get(..16).unwrap_or(&s.started).replace('T', " "),
            s.turns,
            s.ended,
            model,
            first.replace('\n', " "),
            if s.resumable {
                ""
            } else {
                "  (newer format; cannot resume)"
            }
        );
    }
    if sessions.len() > 30 {
        println!(
            "... {} older sessions in {}",
            sessions.len() - 30,
            dir.display()
        );
    }
}

/// Continues a recorded session in `ctx` (docs/design/session-resume.md):
/// rebuilds the context from its latest `context` record or by replay,
/// reopens its transcript for appending, and reports what changed since.
fn resume_session(ctx: &mut AgentContext, id: &str) -> Result<(), String> {
    let dir = session::expand_home(&ctx.config.session.transcript_dir);
    let path = transcript::find(&dir, id)?;
    let t = transcript::Transcript::load(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let start = t
        .start()
        .ok_or("not a transcript (no session_start)")?
        .clone();
    if start.schema > session::SCHEMA {
        return Err(format!(
            "{} was written by a newer mima (transcript schema {}); this mima reads up to {}",
            path.display(),
            start.schema,
            session::SCHEMA
        ));
    }
    let rebuilt = transcript::rebuild(&t, ctx.output_cap_bytes());
    let resumed = session::Session::resume(
        &path,
        ctx.config.session.max_output_bytes,
        start.session_started.clone(),
        t.next_seq(),
        t.turns(),
    )
    .map_err(|e| e.to_string())?;
    ctx.session = resumed;
    let count = rebuilt.messages.len();
    ctx.restore(
        rebuilt.messages,
        rebuilt.first_instruction,
        rebuilt.masked_total,
        rebuilt.evicted_total,
    );

    let cwd = std::env::current_dir().unwrap_or_default();
    let changed = transcript::changed_files(&t, &cwd);
    let model = ctx.config.provider.default_model.clone();
    ctx.session.record(
        "session_resumed",
        json!({ "method": rebuilt.method, "reason": rebuilt.reason, "from_seq": rebuilt.from_seq,
                "mima_version": env!("CARGO_PKG_VERSION"), "model": model,
                "cwd": cwd.display().to_string(), "changed_files": changed }),
    );

    let how = match &rebuilt.reason {
        None => "from its last context record".to_string(),
        Some(r) => format!("by replaying the transcript ({r})"),
    };
    println!(
        "Resumed session {} ({} turns, {count} messages), rebuilt {how}.",
        ctx.session.id(),
        t.turns()
    );
    if t.bad_lines > 0 {
        println!(
            "Note: {} unreadable transcript line(s) were skipped.",
            t.bad_lines
        );
    }
    if start.model.as_deref().is_some_and(|m| m != model) {
        println!(
            "Note: the session used {}; now using {model}.",
            start.model.unwrap_or_default()
        );
    }
    if start.cwd.as_deref().is_some_and(|c| Path::new(c) != cwd) {
        println!(
            "Note: the session ran in {}; now in {}.",
            start.cwd.unwrap_or_default(),
            cwd.display()
        );
    }
    if !changed.is_empty() {
        println!("Files changed on disk since the session last saw them (re-read before editing):");
        for c in &changed {
            println!("  {} ({})", c.path, c.status);
        }
    }
    Ok(())
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
         /sessions            list recorded sessions\n  \
         /resume [id]         continue a recorded session (default: the last)\n  \
         /enable_transcript   record this session to a transcript\n  \
         /disable_transcript  stop recording\n  \
         /exit                quit (also Ctrl-D)"
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
