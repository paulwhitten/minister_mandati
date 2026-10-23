//! Conversation state and context-window management. The strategy and its
//! evidence are described in `docs/context.md`; in short, before each request:
//!
//! 1. normalize: every tool call has exactly one result, right after it;
//! 2. mask: above `mask_at`, replace old tool-result bodies with placeholders
//!    down to `mask_to`;
//! 3. evict: if masking cannot reach `mask_to`, drop whole oldest steps down
//!    to it (never split a call from its results; never the pinned leaders,
//!    the current instruction, or the newest step).
//!
//! Stage 0 (capping each tool output) happens where results enter history.

use serde::Serialize;
use serde_json::{Value, json};

use crate::budget::{Budget, CountSource, Estimator, FALLBACK_WINDOW, MIN_MASK_GAIN};
use crate::client::{CompletionResponse, TokenUsage};
use crate::config::Config;
use crate::context_report::{self as report, ContextReport, LastCompaction};
use crate::digest::Digest;
use crate::schema;
use crate::session::{Session, expand_home};
use crate::tools::ToolSpec;

/// A single chat message in OpenAI wire format.
#[derive(Clone, Serialize)]
pub struct Message {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Present on assistant turns that requested tools (OpenAI `tool_calls`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Value>,
    /// Present on `tool` role messages, matching the originating call id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// True once a tool result's body has been replaced by a placeholder.
    #[serde(skip)]
    pub masked: bool,
    /// Transcript record (`seq`) this message was built from, if recorded.
    /// Lets a `context` record refer to it instead of copying it
    /// (docs/design/session-resume.md).
    #[serde(skip)]
    pub origin: Option<u64>,
    /// Stage 0 cap applied to this tool output, when it was cut.
    #[serde(skip)]
    pub cap_bytes: Option<u64>,
    /// Made up by mima rather than recorded (a "not executed" result).
    #[serde(skip)]
    pub synthetic: bool,
    #[serde(skip)]
    pub kind: Kind,
}

/// Who a message speaks for, beyond its role.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Kind {
    #[default]
    Normal,
    /// An instruction from the operator (a turn's request).
    Request,
    /// A prompt mima adds (loop-guard nudge, continue, step limit, notes).
    Notice,
    /// A compaction record standing in for evicted steps.
    Record,
}

impl Message {
    fn new(role: &str, content: Option<String>) -> Self {
        Self {
            role: role.into(),
            content,
            tool_calls: None,
            tool_call_id: None,
            masked: false,
            origin: None,
            cap_bytes: None,
            synthetic: false,
            kind: Kind::Normal,
        }
    }

    /// Records which transcript line this message came from.
    pub fn with_origin(mut self, seq: Option<u64>) -> Self {
        self.origin = seq;
        self
    }

    pub fn system(content: &str) -> Self {
        Self::new("system", Some(content.into()))
    }

    /// An operator request.
    pub fn user(content: &str) -> Self {
        let mut m = Self::new("user", Some(content.into()));
        m.kind = Kind::Request;
        m
    }

    /// A user-role prompt written by mima, not the operator.
    pub fn notice(content: &str) -> Self {
        let mut m = Self::new("user", Some(content.into()));
        m.kind = Kind::Notice;
        m
    }

    /// A compaction record (user role, written by mima).
    pub fn record(content: &str) -> Self {
        let mut m = Self::new("user", Some(content.into()));
        m.kind = Kind::Record;
        m
    }

    /// A plain assistant reply (no tool calls), e.g. a turn's final answer.
    pub fn assistant(content: &str) -> Self {
        Self::new("assistant", Some(content.into()))
    }

    /// The assistant turn with its tool calls. The calls are rebuilt from the
    /// parsed `ToolCall`s rather than echoed raw, so their ids always match the
    /// results: this covers the ReAct fallback (no native `tool_calls`) and
    /// native calls whose id was generated locally.
    pub fn assistant_tool_calls(response: &CompletionResponse) -> Self {
        let calls: Vec<(&str, &str, &Value)> = response
            .tool_calls
            .iter()
            .flatten()
            .map(|c| (c.id.as_str(), c.name.as_str(), &c.args))
            .collect();
        Self::assistant_calls(response.content.clone(), &calls)
    }

    /// An assistant message with tool calls given as (id, name, arguments).
    /// Shared by the live loop and transcript rebuilding so both produce the
    /// same message.
    pub fn assistant_calls(content: Option<String>, calls: &[(&str, &str, &Value)]) -> Self {
        let mut m = Self::new("assistant", content.filter(|c| !c.is_empty()));
        if !calls.is_empty() {
            m.tool_calls = Some(Value::Array(
                calls
                    .iter()
                    .map(|(id, name, args)| {
                        json!({
                            "id": id,
                            "type": "function",
                            "function": { "name": name, "arguments": args.to_string() },
                        })
                    })
                    .collect(),
            ));
        }
        m
    }

    pub fn tool_result(call_id: &str, content: &str) -> Self {
        let mut m = Self::new("tool", Some(content.into()));
        m.tool_call_id = Some(call_id.into());
        m
    }

    /// Characters this message contributes to a request, including a small
    /// allowance for role and chat-template framing.
    fn chars(&self) -> usize {
        const FRAMING: usize = 16;
        FRAMING
            + self.content.as_ref().map_or(0, String::len)
            + self.tool_calls.as_ref().map_or(0, |t| t.to_string().len())
    }
}

/// Cumulative token accounting across all model requests in a task.
#[derive(Debug, Default, Clone, Copy)]
pub struct TokenLedger {
    pub requests: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    /// Requests whose response omitted usage, so totals understate the truth.
    pub unreported_requests: u64,
}

/// Snapshot of the context budget and current size, for logs.
#[derive(Debug, Clone, Copy)]
pub struct ContextStats {
    pub budget: Budget,
    pub tokens: usize,
    pub token_source: &'static str,
}

/// Result text for a requested tool call that never ran (turn aborted, loop
/// guard stop, error). Keeps the request/result pairing the API requires.
pub const NOT_EXECUTED: &str = "Not executed: the turn ended before this call ran.";
/// The loop guard's nudge (a user message after a step's tool results).
pub const NUDGE: &str = "A repeated identical action was detected that already succeeded. \
                         If the task is complete, reply without further tool calls.";
/// Sent when a reply has no tool call and no usable answer: empty, or cut
/// off at `max_tokens` (typically mid-reasoning).
pub const CONTINUE: &str = "Your last reply was empty or was cut off before it finished. \
                            Do not repeat long reasoning: call a tool to make progress, or \
                            give your final answer briefly.";
/// Sent a few steps before the step limit, so the model finishes instead of
/// running out mid-investigation.
pub const STEPS_LEFT: &str = "Only a few tool steps remain before the step limit. \
                              Make your change now if you have not, verify it if you can, \
                              and then reply with a short summary.";
/// Sent at the step limit, with tools disabled, to get a final answer.
pub const FINAL_SUMMARY: &str = "The step limit has been reached. Without calling any more \
                                 tools, reply with a brief summary: what you changed, whether \
                                 you verified it, and what remains to be done.";
/// Longest tool-call argument text quoted in a placeholder.
const PLACEHOLDER_ARGS_CHARS: usize = 200;
/// Share of E for a compaction record's digest (clamped to 256-2048 tokens).
const RECORD_SHARE: f64 = 0.04;
/// Room for the record's header line, in characters.
const RECORD_HEADER_CHARS: usize = 400;
/// Most tokens of earlier operator requests kept by eviction.
const MAX_KEPT_REQUEST_TOKENS: usize = 20_000;

/// Ids of the calls in an assistant message's OpenAI `tool_calls` array.
fn tool_call_ids(tool_calls: &Value) -> Vec<String> {
    tool_calls
        .as_array()
        .map(|calls| {
            calls
                .iter()
                .filter_map(|c| c["id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Whether a tool result reports failure: a tool error, or a shell command
/// with a non-zero exit status.
pub fn is_failure(content: &str) -> bool {
    content.starts_with("Error:")
        || (content.starts_with("exit: ") && !content.starts_with("exit: 0\n"))
}

/// Rollback point for a cancellable turn (`AgentContext::checkpoint`).
#[derive(Clone, Copy, Debug)]
pub struct Checkpoint {
    len: usize,
    turns: u64,
}

/// Removes the messages from `start` on (a cancelled or unfinished turn),
/// keeping a compaction record among them: it covers steps evicted earlier.
pub fn drop_turn(messages: &mut Vec<Message>, start: usize) {
    if start >= messages.len() {
        return;
    }
    let tail = messages.split_off(start);
    messages.extend(tail.into_iter().filter(|m| m.kind == Kind::Record));
}

/// Prefix of `s` at most `max` bytes long, cut on a character boundary.
fn clip(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

pub struct AgentContext {
    pub config: Config,
    pub tool_specs: Vec<ToolSpec>,
    /// Send the next request without tools (the final summary at the step
    /// limit asks for an answer, not more calls).
    pub tools_disabled: bool,
    /// The current session and its (optional) transcript.
    pub session: Session,
    /// Server endpoint for exact token counts (vLLM `/tokenize`), when available.
    pub token_counter: Option<String>,
    /// The model's entry in the server's `/models` list, for the audit record.
    pub server_model: Option<serde_json::Value>,
    /// The server's version, when it reports one.
    pub server_version: Option<String>,
    /// The workspace's git state when the last turn ended, for `turn_end`.
    pub turn_end_workspace: Option<serde_json::Value>,
    /// Project instruction files loaded into the system prompt.
    pub instructions: crate::instructions::Loaded,
    messages: Vec<Message>,
    /// Number of leading messages that are never evicted (system + first user turn).
    pinned: usize,
    /// Index of the current turn's instruction; protected from eviction.
    turn_start: Option<usize>,
    /// Original text of the pinned first instruction (before any eviction note).
    first_instruction: Option<String>,
    usage: TokenLedger,
    budget: Budget,
    estimator: Estimator,
    /// Characters of the serialized tool schemas, sent with every request.
    tool_schema_chars: usize,
    /// Characters sent with the last request, paired with its reported usage.
    chars_at_last_request: usize,
    masked_total: usize,
    evicted_total: usize,
    mask_events: usize,
    /// Where the window came from (`context_report::WINDOW_*`).
    window_source: &'static str,
    last_compaction: Option<LastCompaction>,
    /// What evicted steps read, changed and ran (the compaction record).
    digest: Digest,
    /// Compaction records written this session.
    records: usize,
    /// Turns begun on this context, to tell whether a checkpoint's turn began.
    turns_begun: u64,
}

impl AgentContext {
    /// Appends the project instructions to the system prompt (once, before
    /// the first request) and keeps them for the audit record.
    pub fn set_instructions(&mut self, loaded: crate::instructions::Loaded) {
        if let Some(section) = loaded.prompt_section() {
            let base = self.messages[0].content.clone().unwrap_or_default();
            self.messages[0] = Message::system(&format!("{base}\n\n{section}"));
        }
        self.instructions = loaded;
    }

    /// The system prompt as sent.
    pub fn system_prompt_text(&self) -> &str {
        self.messages[0].content.as_deref().unwrap_or("")
    }

    pub fn new(config: Config, tool_specs: Vec<ToolSpec>) -> Self {
        let mut system_prompt = config.system_prompt();
        if let Some(note) = config.environment_note() {
            system_prompt = format!("{system_prompt}\n\n{note}");
        }
        tracing::debug!(%system_prompt, "composed system prompt");
        let window = config.context.window.unwrap_or(FALLBACK_WINDOW);
        let window_source = if config.context.window.is_some() {
            report::WINDOW_CONFIGURED
        } else {
            report::WINDOW_FALLBACK
        };
        let budget = Budget::new(window, &config);
        let tool_schema_chars = schema::to_openai_tools(&tool_specs).to_string().len();
        let session = Session::new(
            expand_home(&config.session.transcript_dir),
            config.session.max_output_bytes,
        );
        Self {
            config,
            tool_specs,
            session,
            token_counter: None,
            server_model: None,
            server_version: None,
            turn_end_workspace: None,
            instructions: crate::instructions::Loaded::default(),
            messages: vec![Message::system(&system_prompt)],
            pinned: 1,
            turn_start: None,
            first_instruction: None,
            usage: TokenLedger::default(),
            budget,
            window_source,
            estimator: Estimator::default(),
            tool_schema_chars,
            chars_at_last_request: 0,
            masked_total: 0,
            evicted_total: 0,
            mask_events: 0,
            last_compaction: None,
            digest: Digest::default(),
            records: 0,
            turns_begun: 0,
            tools_disabled: false,
        }
    }

    /// Adopt a context window learned from the server (`/v1/models` or an
    /// overflow error) and recompute the budget.
    pub fn set_window(&mut self, window: usize, source: &'static str) {
        self.window_source = source;
        if window == self.budget.window {
            return;
        }
        self.budget = Budget::new(window, &self.config);
        tracing::info!(
            window,
            usable = self.budget.usable,
            operating = self.budget.operating,
            "context budget set"
        );
    }

    pub fn stats(&self) -> ContextStats {
        ContextStats {
            budget: self.budget,
            tokens: self.estimated_tokens(),
            token_source: self.estimator.source(self.total_chars()),
        }
    }

    /// `max_tokens` for the next request: the configured limit, but never more
    /// than the room left in the window after the prompt (vLLM rejects a
    /// request whose prompt plus max_tokens exceeds the window). The closing
    /// summary request (tools disabled) is capped lower: a summary is short,
    /// and a slow model reasoning up to `max_tokens` there took minutes.
    pub fn reply_limit(&self) -> usize {
        const MIN_REPLY: usize = 256;
        const SLACK: usize = 64;
        const SUMMARY_REPLY: usize = 1024;
        let room = self
            .budget
            .window
            .saturating_sub(self.estimated_tokens() + SLACK);
        let limit = self.config.agent.max_tokens.min(room.max(MIN_REPLY));
        if self.tools_disabled {
            limit.min(SUMMARY_REPLY)
        } else {
            limit
        }
    }

    /// Stage 0 limit: the largest tool output (in bytes) kept verbatim.
    pub fn output_cap_bytes(&self) -> usize {
        self.estimator.chars(self.budget.output_cap_tokens())
    }

    /// Fold one response's token usage into the running task total and
    /// calibrate the estimator. A missing `usage` is counted separately so
    /// cumulative totals stay trustworthy.
    pub fn record_usage(&mut self, usage: Option<TokenUsage>) {
        self.usage.requests += 1;
        match usage {
            Some(u) => {
                self.usage.prompt_tokens += u.prompt_tokens;
                self.usage.completion_tokens += u.completion_tokens;
                self.usage.total_tokens += u.total_tokens;
                self.estimator.record_exact(
                    self.chars_at_last_request,
                    u.prompt_tokens as usize,
                    CountSource::Usage,
                );
            }
            None => self.usage.unreported_requests += 1,
        }
    }

    pub fn token_ledger(&self) -> TokenLedger {
        self.usage
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Start a turn: repair any tool calls a previous turn left unanswered,
    /// then append the instruction and protect it from eviction for this turn.
    pub fn begin_turn(&mut self, instruction: &str, origin: Option<u64>) {
        self.normalize();
        self.turns_begun += 1;
        self.turn_start = Some(self.messages.len());
        self.add_message(Message::user(instruction).with_origin(origin));
    }

    /// Links the turn's final answer (the last message) to its `turn_end`
    /// record, which is written after the answer is added.
    pub fn set_answer_origin(&mut self, seq: Option<u64>) {
        if let Some(last) = self.messages.last_mut()
            && last.role == "assistant"
            && last.tool_calls.is_none()
            && last.origin.is_none()
        {
            last.origin = seq;
        }
    }

    /// What the model currently sees, by reference to transcript records
    /// (docs/design/session-resume.md). `None` when a message has no record
    /// to refer to (recording started mid-session); resume then replays.
    pub fn context_record(&self, cause: &str) -> Option<crate::transcript::ContextRecord> {
        use crate::transcript::{ContextRecord, Entry, View, check};
        let body = self.messages.get(1..).unwrap_or_default();
        let mut entries = Vec::with_capacity(body.len());
        for (i, m) in body.iter().enumerate() {
            let entry = if m.synthetic {
                Entry {
                    view: View::Synthetic,
                    role: Some(m.role.clone()),
                    text: m.content.clone(),
                    call_id: m.tool_call_id.clone(),
                    ..Entry::default()
                }
            } else {
                let mut e = Entry {
                    seq: Some(m.origin?),
                    ..Entry::default()
                };
                let noted = (i == 0 && self.pinned == 2)
                    .then(|| {
                        let original = self.first_instruction.as_deref()?;
                        let content = m.content.as_deref()?;
                        content.strip_prefix(original).filter(|n| !n.is_empty())
                    })
                    .flatten();
                if m.kind == Kind::Record {
                    e.view = View::Record;
                } else if m.masked {
                    e.view = View::Masked;
                    e.text = m.content.clone();
                } else if let Some(note) = noted {
                    e.view = View::Noted;
                    e.text = Some(note.to_string());
                } else if let Some(cap) = m.cap_bytes {
                    e.view = View::Capped;
                    e.cap_bytes = Some(cap);
                }
                e
            };
            entries.push(entry);
        }
        let stats = self.stats();
        Some(ContextRecord {
            context_schema: crate::transcript::CONTEXT_SCHEMA,
            turn: self.session.turn(),
            cause: cause.to_string(),
            window: stats.budget.window as u64,
            budget: stats.budget.operating as u64,
            tokens: stats.tokens as u64,
            count_source: stats.token_source.to_string(),
            masked_total: self.masked_total as u64,
            evicted_total: self.evicted_total as u64,
            entries,
            check: check(body),
        })
    }

    /// Writes a `context` record to the transcript (no-op when not recording
    /// or when the context cannot be described by reference).
    pub fn write_context(&mut self, cause: &str) {
        if !self.session.is_recording() {
            return;
        }
        match self.context_record(cause) {
            Some(record) => {
                let value = serde_json::to_value(&record).unwrap_or_default();
                self.session.record("context", value);
            }
            None => tracing::debug!("context not written: a message has no transcript record"),
        }
    }

    /// Replaces the conversation with one rebuilt from a transcript (resume).
    /// The system prompt comes from the current configuration.
    pub fn restore(
        &mut self,
        messages: Vec<Message>,
        first_instruction: Option<String>,
        masked_total: usize,
        evicted_total: usize,
    ) {
        let system = self.messages[0].clone();
        let first_is_user = messages.first().is_some_and(|m| m.role == "user");
        self.messages = std::iter::once(system).chain(messages).collect();
        self.pinned = if first_is_user { 2 } else { 1 };
        self.first_instruction = if first_is_user {
            first_instruction
        } else {
            None
        };
        self.turn_start = None;
        self.masked_total = masked_total;
        self.evicted_total = evicted_total;
    }

    /// Restores the digest and record count of a resumed session, so the
    /// next compaction record continues them.
    pub fn restore_compaction(&mut self, digest: Digest, records: usize) {
        self.digest = digest;
        self.records = records;
    }

    pub fn add_message(&mut self, message: Message) {
        // Pin the first user instruction alongside the system prompt.
        if self.pinned == 1 && message.role == "user" {
            self.pinned = 2;
            self.first_instruction = message.content.clone();
        }
        self.messages.push(message);
    }

    /// Where a cancellable turn starts, for `rollback_to`.
    pub fn checkpoint(&self) -> Checkpoint {
        Checkpoint {
            len: self.messages.len(),
            turns: self.turns_begun,
        }
    }

    /// Removes a cancelled turn, so no assistant tool call is left without
    /// its results. Compaction during the turn may have removed earlier
    /// messages and moved the turn, so the turn is found by its start, not by
    /// the old length; a compaction record is kept. Never trims the pinned
    /// leaders (system prompt + first instruction).
    pub fn rollback_to(&mut self, cp: Checkpoint) {
        let start = match self.turn_start {
            Some(t) if self.turns_begun > cp.turns => t,
            _ => cp.len,
        };
        drop_turn(&mut self.messages, start.max(self.pinned.max(1)));
        if self.turns_begun > cp.turns {
            self.turn_start = None;
        }
    }

    /// Start a fresh session: keep only the system prompt and clear the ledger.
    pub fn reset(&mut self) {
        self.messages.truncate(1);
        self.pinned = 1;
        self.turn_start = None;
        self.first_instruction = None;
        self.usage = TokenLedger::default();
        self.masked_total = 0;
        self.evicted_total = 0;
        self.mask_events = 0;
        self.last_compaction = None;
        self.digest = Digest::default();
        self.records = 0;
    }

    /// Run before every model request without a server count: normalize,
    /// compact as the budget requires, and seal. With exact counting, `main`
    /// calls the steps separately and counts in between.
    #[cfg(test)]
    pub fn prepare_request(&mut self) {
        self.normalize();
        self.compact();
        self.seal_request();
    }

    /// Records an exact token count for the request as it stands now (from
    /// the server's `/tokenize`), anchoring all later estimates on it.
    pub fn record_counted(&mut self, tokens: usize) {
        self.estimator
            .record_exact(self.total_chars(), tokens, CountSource::Tokenize);
    }

    /// Masks and evicts as the budget requires. Returns true if the history
    /// changed (so an exact count taken before is stale).
    pub fn compact(&mut self) -> bool {
        let before = (self.masked_total, self.evicted_total);
        let b = self.budget;
        let (mask_at, mask_to) = (self.config.context.mask_at, self.config.context.mask_to);
        if self.estimated_tokens() > b.of(mask_at) {
            self.mask(b.of(mask_to), b.of(MIN_MASK_GAIN), "threshold");
            // Placeholders, reasoning and protected output accumulate; once
            // masking alone cannot reach the target, drop the oldest steps
            // (mostly placeholders by now) so edits stay batched.
            if self.estimated_tokens() > b.of(mask_to) {
                self.evict(b.of(mask_to), "mask-insufficient");
            }
        }
        before != (self.masked_total, self.evicted_total)
    }

    /// Final check before sending: warns if the request still exceeds the
    /// usable window, and records its size to pair with the reported usage.
    pub fn seal_request(&mut self) {
        let b = self.budget;
        let est = self.estimated_tokens();
        if est > b.usable {
            tracing::warn!(
                est_tokens = est,
                usable = b.usable,
                source = self.estimator.source(self.total_chars()),
                "context exceeds the usable window; nothing more is safely removable"
            );
        }
        self.chars_at_last_request = self.total_chars();
    }

    /// Reactive path after the server rejected a request as too long. Adopts
    /// the server's stated window and, when stated, recalibrates the estimate
    /// from its measured prompt size; then frees space by masking and
    /// eviction. Returns false when nothing could be removed (caller stops).
    pub fn handle_overflow(
        &mut self,
        server_window: Option<usize>,
        server_prompt_tokens: Option<u64>,
    ) -> bool {
        if let Some(w) = server_window {
            self.set_window(w, report::WINDOW_SERVER);
        }
        if let Some(p) = server_prompt_tokens {
            self.estimator
                .record_exact(self.chars_at_last_request, p as usize, CountSource::Usage);
        }
        let est = self.estimated_tokens();
        // Our estimate was evidently too low, so aim below both it and the target.
        let target = self
            .budget
            .of(self.config.context.mask_to)
            .min(est * 7 / 10);
        let masked = self.mask(target, 0, "overflow");
        let evicted = if self.estimated_tokens() > target {
            self.evict(target, "overflow")
        } else {
            0
        };
        masked + evicted > 0
    }

    /// Enforces the pairing invariant: every assistant tool call is followed by
    /// exactly one result for it (a synthetic "not executed" one if missing),
    /// and results with no matching call are dropped.
    pub fn normalize(&mut self) {
        let old = std::mem::take(&mut self.messages);
        let old_turn_start = self.turn_start;
        let mut out: Vec<Message> = Vec::with_capacity(old.len());
        let mut new_turn_start = None;
        let (mut added, mut dropped) = (0usize, 0usize);
        let mut iter = old.into_iter().enumerate().peekable();

        while let Some((i, m)) = iter.next() {
            if Some(i) == old_turn_start {
                new_turn_start = Some(out.len());
            }
            if m.role == "tool" {
                dropped += 1; // orphan: not directly after its call
                continue;
            }
            let ids = m.tool_calls.as_ref().map(tool_call_ids);
            out.push(m);
            let Some(ids) = ids else { continue };
            let mut answered: Vec<String> = Vec::new();
            while let Some((_, r)) = iter.next_if(|(_, r)| r.role == "tool") {
                match r.tool_call_id.clone() {
                    Some(id) if ids.contains(&id) && !answered.contains(&id) => {
                        answered.push(id);
                        out.push(r);
                    }
                    _ => dropped += 1,
                }
            }
            for id in ids.iter().filter(|id| !answered.contains(id)) {
                let mut m = Message::tool_result(id, NOT_EXECUTED);
                m.synthetic = true;
                out.push(m);
                added += 1;
            }
        }

        self.messages = out;
        self.turn_start = new_turn_start;
        if added + dropped > 0 {
            tracing::info!(
                stage = "normalize",
                added_results = added,
                dropped_results = dropped,
                "repaired tool call/result pairing"
            );
            self.session.record(
                "compaction",
                json!({ "stage": "normalize", "added_results": added, "dropped_results": dropped }),
            );
        }
    }

    /// Stage 1: replace the oldest unprotected tool-result bodies with
    /// placeholders until the estimate is at or below `target`. Does nothing
    /// unless at least `min_gain` tokens can be freed (batching keeps the
    /// server's prefix cache useful). Returns the number of results masked.
    fn mask(&mut self, target: usize, min_gain: usize, reason: &str) -> usize {
        let protected = self.protected_results();
        let names = self.call_descriptions();
        let candidates: Vec<(usize, String, usize)> = self
            .messages
            .iter()
            .enumerate()
            .filter(|(i, m)| m.role == "tool" && !m.masked && !protected.contains(i))
            .map(|(i, m)| {
                let placeholder = placeholder(m, &names);
                let saved = self
                    .estimator
                    .tokens(m.chars())
                    .saturating_sub(self.estimator.tokens(placeholder.len()));
                (i, placeholder, saved)
            })
            .filter(|(_, _, saved)| *saved > 0)
            .collect();

        let possible: usize = candidates.iter().map(|(_, _, s)| s).sum();
        if possible == 0 || possible < min_gain {
            tracing::debug!(possible, min_gain, "masking skipped: too little to free");
            return 0;
        }

        let before = self.estimated_tokens();
        let mut est = before;
        let mut count = 0;
        let mut call_ids = Vec::new();
        for (i, placeholder, saved) in candidates {
            if est <= target {
                break;
            }
            let m = &mut self.messages[i];
            m.content = Some(placeholder);
            m.masked = true;
            call_ids.extend(m.tool_call_id.clone());
            est = est.saturating_sub(saved);
            count += 1;
        }
        self.masked_total += count;
        self.mask_events += 1;
        tracing::info!(
            stage = "mask",
            reason,
            results_masked = count,
            est_tokens_before = before,
            est_tokens_after = self.estimated_tokens(),
            target,
            source = self.estimator.source(self.total_chars()),
            "context compaction"
        );
        let after = self.estimated_tokens();
        self.note_compaction("mask", reason, before, after);
        self.session.record(
            "compaction",
            json!({ "stage": "mask", "reason": reason, "call_ids": call_ids,
                    "est_tokens_before": before, "est_tokens_after": after, "target": target }),
        );
        count
    }

    /// Indices of tool results exempt from masking: the newest results up to
    /// `keep_recent` of the budget (always at least the newest one), and the
    /// most recent failure.
    fn protected_results(&self) -> Vec<usize> {
        let keep = self.budget.of(self.config.context.keep_recent);
        let mut protected = Vec::new();
        let mut kept = 0;
        for (i, m) in self.messages.iter().enumerate().rev() {
            if m.role != "tool" || m.masked {
                continue;
            }
            let tokens = self.estimator.tokens(m.chars());
            if protected.is_empty() || kept + tokens <= keep {
                protected.push(i);
                kept += tokens;
            } else {
                break;
            }
        }
        let latest_failure = self.messages.iter().enumerate().rev().find(|(_, m)| {
            m.role == "tool" && !m.masked && m.content.as_deref().is_some_and(is_failure)
        });
        if let Some((i, _)) = latest_failure {
            protected.push(i);
        }
        protected
    }

    /// Map of call id -> "name args" for placeholders.
    fn call_descriptions(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for m in &self.messages {
            let Some(calls) = m.tool_calls.as_ref().and_then(Value::as_array) else {
                continue;
            };
            for c in calls {
                let id = c["id"].as_str().unwrap_or_default().to_string();
                let name = c["function"]["name"].as_str().unwrap_or("tool");
                let args = c["function"]["arguments"].as_str().unwrap_or_default();
                let args = clip(args, PLACEHOLDER_ARGS_CHARS);
                out.push((id, format!("{name} {args}")));
            }
        }
        out
    }

    /// Stage 2: drop whole oldest steps until the estimate is at or below
    /// `target` and put one compaction record, with a digest of everything
    /// removed so far, in their place. Operator requests stay within
    /// `keep_requests`; beyond it, and as a last resort, the oldest go.
    /// Returns the number of messages removed.
    fn evict(&mut self, target: usize, reason: &str) -> usize {
        let before = self.estimated_tokens();
        let old_record = self
            .messages
            .iter()
            .find(|m| m.kind == Kind::Record)
            .map_or(0, |m| self.estimator.tokens(m.chars()));
        let allowance = self
            .estimator
            .tokens(self.record_cap_chars() + RECORD_HEADER_CHARS);
        let goal = target.saturating_sub(allowance.saturating_sub(old_record));
        let keep = self
            .budget
            .of(self.config.context.keep_requests)
            .min(MAX_KEPT_REQUEST_TOKENS);

        let mut removed: Vec<Message> = Vec::new();
        while self.request_tokens() > keep {
            let Some(m) = self.evict_oldest_request() else {
                break;
            };
            removed.push(m);
        }
        while self.estimated_tokens() > goal {
            let group = self.evict_oldest_group();
            if group.is_empty() {
                break;
            }
            removed.extend(group);
        }
        while self.estimated_tokens() > goal {
            let Some(m) = self.evict_oldest_request() else {
                break;
            };
            removed.push(m);
        }
        if removed.is_empty() {
            return 0;
        }

        self.evicted_total += removed.len();
        self.digest.add(&removed);
        self.records += 1;
        if let Some(i) = self.messages.iter().position(|m| m.kind == Kind::Record) {
            self.remove_at(i);
        }
        let text = self.record_text();
        let record = Message::record(&text);
        let after = self.estimated_tokens() + self.estimator.tokens(record.chars());
        let call_ids: Vec<String> = removed
            .iter()
            .filter_map(|m| m.tool_call_id.clone())
            .collect();
        let seqs: Vec<u64> = removed.iter().filter_map(|m| m.origin).collect();
        let requests = removed.iter().filter(|m| m.kind == Kind::Request).count();
        tracing::info!(
            stage = "evict",
            reason,
            messages_removed = removed.len(),
            requests_removed = requests,
            record = self.records,
            est_tokens_before = before,
            est_tokens_after = after,
            target,
            source = self.estimator.source(self.total_chars()),
            "context compaction"
        );
        self.note_compaction("evict", reason, before, after);
        let seq = self.session.record(
            "compaction",
            json!({ "stage": "evict", "reason": reason, "method": "digest",
                    "record": self.records, "messages_removed": removed.len(),
                    "requests_removed": requests, "call_ids": call_ids, "seqs": seqs,
                    "digest": self.digest, "text": text,
                    "est_tokens_before": before, "est_tokens_after": after,
                    "target": target }),
        );
        // Everything removed is older than the first remaining non-request.
        let at = (self.pinned..self.messages.len())
            .find(|&i| self.messages[i].kind != Kind::Request)
            .unwrap_or(self.messages.len());
        self.insert_at(at, record.with_origin(seq));
        removed.len()
    }

    /// Largest compaction record text, in characters.
    fn record_cap_chars(&self) -> usize {
        self.estimator
            .chars(self.budget.of(RECORD_SHARE).clamp(256, 2_048))
    }

    fn record_text(&self) -> String {
        let mut head = format!(
            "[Compaction record {}: mima removed {} earlier messages to fit the context \
             window. The digest lists what the removed steps read, changed and ran, \
             taken from their tool calls; re-run a tool if you need its output again.",
            self.records, self.evicted_total
        );
        if self.digest.requests_removed > 0 {
            head.push_str(&format!(
                " {} earlier operator requests were removed too.",
                self.digest.requests_removed
            ));
        }
        format!(
            "{head}]\n\n## Digest\n{}",
            self.digest.render(self.record_cap_chars())
        )
    }

    /// Estimated tokens of operator requests that eviction may remove (not
    /// the pinned first one, not the current turn's).
    fn request_tokens(&self) -> usize {
        self.messages
            .iter()
            .enumerate()
            .skip(self.pinned)
            .filter(|(i, m)| m.kind == Kind::Request && self.turn_start != Some(*i))
            .map(|(_, m)| self.estimator.tokens(m.chars()))
            .sum()
    }

    fn evict_oldest_request(&mut self) -> Option<Message> {
        let i = (self.pinned..self.messages.len())
            .find(|&i| self.messages[i].kind == Kind::Request && self.turn_start != Some(i))?;
        Some(self.remove_at(i))
    }

    fn remove_at(&mut self, i: usize) -> Message {
        let m = self.messages.remove(i);
        if let Some(t) = self.turn_start.as_mut()
            && *t > i
        {
            *t -= 1;
        }
        m
    }

    fn insert_at(&mut self, i: usize, m: Message) {
        self.messages.insert(i, m);
        if let Some(t) = self.turn_start.as_mut()
            && *t >= i
        {
            *t += 1;
        }
    }

    /// Removes the oldest evictable group and returns it (empty when none).
    /// Earlier turns go first, then the current turn after its instruction.
    /// Never removed: the pinned leaders, the current instruction, the newest
    /// group, and groups holding protected results (see `protected_results`).
    fn evict_oldest_group(&mut self) -> Vec<Message> {
        let protected = self.protected_results();
        let len = self.messages.len();
        let mut start = self.pinned;
        while start < len {
            if self.turn_start == Some(start)
                || matches!(self.messages[start].kind, Kind::Request | Kind::Record)
            {
                start += 1; // requests and the record are handled by `evict`
                continue;
            }
            let end = self.group_end(start);
            if end >= len {
                return Vec::new(); // the newest group stays
            }
            if protected.iter().any(|&p| (start..end).contains(&p)) {
                start = end;
                continue;
            }
            let group: Vec<Message> = self.messages.drain(start..end).collect();
            if let Some(t) = self.turn_start.as_mut()
                && *t > start
            {
                *t -= end - start;
            }
            return group;
        }
        Vec::new()
    }

    /// End (exclusive) of the group starting at `start`: an assistant tool-call
    /// message plus its following tool results, or a single message.
    fn group_end(&self, start: usize) -> usize {
        let mut end = start + 1;
        if self.messages[start].tool_calls.is_some() {
            while end < self.messages.len() && self.messages[end].role == "tool" {
                end += 1;
            }
        }
        end
    }

    /// Characters of the full request: messages plus tool schemas.
    fn total_chars(&self) -> usize {
        self.tool_schema_chars + self.messages.iter().map(Message::chars).sum::<usize>()
    }

    fn note_compaction(&mut self, stage: &'static str, reason: &str, before: usize, after: usize) {
        self.last_compaction = Some(LastCompaction {
            turn: self.session.turn(),
            stage,
            reason: reason.to_string(),
            before,
            after,
        });
    }

    /// Characters of the system message taken by the project instructions
    /// section (`set_instructions` appends it after a blank line).
    fn instructions_chars(&self) -> usize {
        let system = self.messages[0].content.as_deref().unwrap_or_default();
        match self.instructions.prompt_section() {
            Some(section) if system.ends_with(&section) => (section.len() + 2).min(system.len()),
            _ => 0,
        }
    }

    /// What fills the context, by category and by message, for
    /// `/context-status`. Category and message figures share the total in
    /// proportion to their characters.
    pub fn report(&self) -> ContextReport {
        use report::{
            ASSISTANT, INSTRUCTIONS, NOTICES, OUTPUTS_FULL, OUTPUTS_MASKED, RECORDS, REQUESTS,
            SYSTEM, TOOLS,
        };
        let total = self.estimated_tokens();
        let names: Vec<(String, String)> = self
            .messages
            .iter()
            .filter_map(|m| m.tool_calls.as_ref().and_then(Value::as_array))
            .flatten()
            .map(|c| {
                let f = &c["function"];
                (
                    c["id"].as_str().unwrap_or_default().to_string(),
                    report::describe_call(
                        f["name"].as_str().unwrap_or("tool"),
                        f["arguments"].as_str().unwrap_or_default(),
                    ),
                )
            })
            .collect();
        let instructions = self.instructions_chars();

        // Parts to share the total: the system message as two parts, every
        // other message, then the tool schemas.
        let mut weights = Vec::with_capacity(self.messages.len() + 2);
        let mut rows = Vec::with_capacity(self.messages.len());
        let (mut calls, mut capped) = (0usize, 0usize);
        for (i, m) in self.messages.iter().enumerate() {
            let content = m.content.as_deref().unwrap_or_default();
            let (category, view, what) = if i == 0 {
                weights.push(m.chars() - instructions);
                weights.push(instructions);
                (SYSTEM, "full", String::new())
            } else {
                weights.push(m.chars());
                match m.role.as_str() {
                    "user" if m.kind == Kind::Record => (
                        RECORDS,
                        "record",
                        format!("compaction record {}", self.records),
                    ),
                    "user" if m.synthetic || m.kind == Kind::Notice => {
                        (NOTICES, "notice", report::snippet(content, 60))
                    }
                    "user" => {
                        let noted = i == 1
                            && self.pinned == 2
                            && self.first_instruction.as_deref() != Some(content);
                        let view = if noted { "noted" } else { "full" };
                        (
                            REQUESTS,
                            view,
                            format!("\"{}\"", report::snippet(content, 60)),
                        )
                    }
                    "assistant" => {
                        let mut what = Vec::new();
                        if let Some(tc) = m.tool_calls.as_ref().and_then(Value::as_array) {
                            calls += tc.len();
                            let mut counts: Vec<(&str, usize)> = Vec::new();
                            for c in tc {
                                let name = c["function"]["name"].as_str().unwrap_or("tool");
                                match counts.iter_mut().find(|(n, _)| *n == name) {
                                    Some((_, k)) => *k += 1,
                                    None => counts.push((name, 1)),
                                }
                            }
                            what.extend(counts.into_iter().map(|(n, k)| match k {
                                1 => n.to_string(),
                                k => format!("{n} x{k}"),
                            }));
                        }
                        if !content.is_empty() {
                            what.push(format!("\"{}\"", report::snippet(content, 50)));
                        }
                        (ASSISTANT, "full", what.join(", "))
                    }
                    _ => {
                        let id = m.tool_call_id.as_deref().unwrap_or_default();
                        let call = names
                            .iter()
                            .find(|(c, _)| c == id)
                            .map_or("tool call", |(_, d)| d.as_str());
                        let what = report::snippet(call, 70);
                        if m.synthetic {
                            (NOTICES, "synthetic", what)
                        } else if m.masked {
                            (OUTPUTS_MASKED, "masked", what)
                        } else if m.cap_bytes.is_some() {
                            capped += 1;
                            (OUTPUTS_FULL, "capped", what)
                        } else {
                            (OUTPUTS_FULL, "full", what)
                        }
                    }
                }
            };
            rows.push(report::Row {
                index: i,
                role: m.role.clone(),
                view,
                category,
                tokens: 0,
                what,
            });
        }
        weights.push(self.tool_schema_chars);
        let parts = report::allocate(total, &weights);

        let mut tokens = [0usize; report::CATEGORY_NAMES.len()];
        let mut items = [0usize; report::CATEGORY_NAMES.len()];
        let (system, instructions_tokens) = (parts[0], parts[1]);
        tokens[SYSTEM] = system;
        tokens[INSTRUCTIONS] = instructions_tokens;
        tokens[TOOLS] = parts[parts.len() - 1];
        for (row, &t) in rows.iter_mut().skip(1).zip(&parts[2..]) {
            row.tokens = t;
            tokens[row.category] += t;
            items[row.category] += 1;
        }
        rows[0].tokens = system + instructions_tokens;
        rows[0].what = format!(
            "system prompt {} + project instructions {}",
            report::num(system),
            report::num(instructions_tokens)
        );

        let loaded: Vec<String> = self
            .instructions
            .files
            .iter()
            .filter(|f| f.skipped.is_none())
            .map(|f| f.source.clone())
            .collect();
        let label = |c: usize| -> String {
            match c {
                SYSTEM => "1".into(),
                INSTRUCTIONS if instructions == 0 => "none".into(),
                INSTRUCTIONS => loaded.join(", "),
                TOOLS => format!("{} tools", self.tool_specs.len()),
                ASSISTANT => format!("{} messages, {calls} calls", items[c]),
                OUTPUTS_FULL if capped > 0 => format!("{} ({capped} capped)", items[c]),
                OUTPUTS_MASKED => format!("{} placeholders", items[c]),
                _ => items[c].to_string(),
            }
        };
        let categories = report::CATEGORY_NAMES
            .iter()
            .enumerate()
            .map(|(c, name)| report::Category {
                name,
                tokens: tokens[c],
                items: label(c),
            })
            .collect();

        let spec_chars: Vec<usize> = self
            .tool_specs
            .iter()
            .map(|s| {
                schema::to_openai_tools(std::slice::from_ref(s))
                    .to_string()
                    .len()
            })
            .collect();
        let tools = self
            .tool_specs
            .iter()
            .map(|s| s.name.clone())
            .zip(report::allocate(tokens[TOOLS], &spec_chars))
            .collect();

        ContextReport {
            session: self.session.id().to_string(),
            turn: self.session.turn(),
            model: self.config.provider.default_model.clone(),
            budget: self.budget,
            window_source: self.window_source,
            budget_configured: self.budget.operating < self.budget.usable,
            total,
            source: self.estimator.source(self.total_chars()),
            categories,
            rows,
            tools,
            mask_at: self.config.context.mask_at,
            mask_to: self.config.context.mask_to,
            keep_recent: self.config.context.keep_recent,
            output_cap: self.budget.output_cap_tokens(),
            masked_total: self.masked_total,
            mask_events: self.mask_events,
            evicted_total: self.evicted_total,
            records: self.records,
            last: self.last_compaction.clone(),
        }
    }

    fn estimated_tokens(&self) -> usize {
        self.estimator.total(self.total_chars())
    }
}

/// One-line stand-in for a masked tool result. Keeps the call (name and
/// arguments), the original size, and for failures the first line.
fn placeholder(m: &Message, names: &[(String, String)]) -> String {
    let content = m.content.as_deref().unwrap_or_default();
    let id = m.tool_call_id.as_deref().unwrap_or_default();
    let call = names
        .iter()
        .find(|(i, _)| i == id)
        .map_or("tool call", |(_, d)| d.as_str());
    let mut text = format!(
        "[output elided to save context: {call}, {} bytes.",
        content.len()
    );
    if is_failure(content) {
        let first = clip(content.lines().next().unwrap_or_default(), 200);
        text.push_str(&format!(" It failed: {first}."));
    }
    text.push_str(" Re-run the tool if you need it again.]");
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolCall;

    /// A context with a small window so the stages trigger quickly. With the
    /// default 3 chars/token: W=4000, R=100, S=512 -> U=E=3388 tokens.
    fn ctx() -> AgentContext {
        let mut config = Config::default();
        config.context.window = Some(4_000);
        config.agent.max_tokens = 100;
        // A fixed prompt keeps these budget tests independent of the
        // default prompt's length.
        config.agent.system_prompt_override = "You are a coding agent under test.".into();
        AgentContext::new(config, Vec::new())
    }

    #[test]
    fn reply_limit_fits_the_window() {
        let mut c = ctx();
        // An empty conversation leaves room for the full configured limit.
        assert_eq!(c.reply_limit(), 100);
        c.config.agent.max_tokens = 10_000;
        let limit = c.reply_limit();
        assert!((256..4_000).contains(&limit), "{limit}");
        assert!(limit + c.stats().tokens <= 4_000);
        c.tools_disabled = true;
        assert_eq!(c.reply_limit(), limit.min(1024));
    }

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "read_file".into(),
            args: json!({ "path": format!("{id}.rs") }),
        }
    }

    fn response(calls: Vec<ToolCall>) -> CompletionResponse {
        CompletionResponse {
            content: None,
            tool_calls: Some(calls),
            usage: None,
            finish_reason: None,
            reasoning: None,
            request_sha256: None,
            response_id: None,
            served_model: None,
        }
    }

    /// One model step: a tool-call request followed by all of its results,
    /// each `size` bytes, then the pre-request budget pass.
    fn step(c: &mut AgentContext, ids: &[&str], size: usize) {
        c.add_message(Message::assistant_tool_calls(&response(
            ids.iter().map(|id| call(id)).collect(),
        )));
        for id in ids {
            c.add_message(Message::tool_result(id, &"x".repeat(size)));
        }
        c.prepare_request();
    }

    /// The API invariant: every tool result directly follows the assistant
    /// message that requested it, and every request is fully answered before
    /// any other message.
    fn assert_valid(c: &AgentContext) {
        let mut open: Vec<String> = Vec::new();
        for (i, m) in c.messages().iter().enumerate() {
            if m.role == "tool" {
                let id = m.tool_call_id.clone().unwrap();
                let pos = open.iter().position(|o| *o == id);
                assert!(pos.is_some(), "orphan tool result {id} at {i}");
                open.remove(pos.unwrap());
            } else {
                assert!(
                    open.is_empty(),
                    "unanswered calls {open:?} before message {i}"
                );
                if let Some(tc) = &m.tool_calls {
                    open = tool_call_ids(tc);
                }
            }
        }
        assert!(open.is_empty(), "unanswered calls {open:?} at end");
    }

    fn texts(c: &AgentContext) -> Vec<String> {
        c.messages()
            .iter()
            .filter_map(|m| m.content.clone())
            .collect()
    }

    #[test]
    fn masking_comes_before_eviction() {
        let mut c = ctx();
        c.begin_turn("task", None);
        // ~500 tokens per result: crosses mask_at (60% of 3388) after a few steps.
        for n in 0..5 {
            step(&mut c, &[&format!("c{n}")], 1_500);
        }
        let s = c.report();
        assert!(s.masked_total > 0, "old outputs masked");
        assert_eq!(s.evicted_total, 0, "no step evicted while masking suffices");
        assert!(s.total <= c.budget.of(0.6));
        // All assistant tool-call messages are still present.
        let calls = c
            .messages()
            .iter()
            .filter(|m| m.tool_calls.is_some())
            .count();
        assert_eq!(calls, 5);
        assert_valid(&c);
    }

    #[test]
    fn placeholder_keeps_call_and_size() {
        let mut c = ctx();
        c.begin_turn("task", None);
        for n in 0..5 {
            step(&mut c, &[&format!("c{n}")], 1_500);
        }
        let first = c
            .messages()
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("c0"))
            .unwrap();
        assert!(first.masked);
        let text = first.content.as_deref().unwrap();
        assert!(text.contains(r#"read_file {"path":"c0.rs"}"#), "{text}");
        assert!(text.contains("1500 bytes"), "{text}");
    }

    #[test]
    fn newest_output_and_latest_failure_are_protected() {
        let mut c = ctx();
        c.begin_turn("task", None);
        step(&mut c, &["old"], 1_500);
        c.add_message(Message::assistant_tool_calls(&response(vec![call("bad")])));
        c.add_message(Message::tool_result(
            "bad",
            &format!("Error: boom\n{}", "y".repeat(1_500)),
        ));
        c.prepare_request();
        for n in 0..3 {
            step(&mut c, &[&format!("c{n}")], 1_500);
        }
        let by_id = |id: &str| {
            c.messages()
                .iter()
                .find(|m| m.tool_call_id.as_deref() == Some(id))
                .cloned()
        };
        // The oldest output is masked, or evicted if masking alone fell short.
        assert!(by_id("old").is_none_or(|m| m.masked));
        let bad = by_id("bad").expect("latest failure kept");
        assert!(!bad.masked, "latest failure kept verbatim");
        assert!(!by_id("c2").unwrap().masked, "newest output kept verbatim");
    }

    #[test]
    fn masking_skipped_when_gain_is_small() {
        let mut c = ctx();
        c.begin_turn(&"t".repeat(7_000), None); // big instruction, not maskable
        step(&mut c, &["a"], 200);
        step(&mut c, &["b"], 200);
        // Over mask_at, but masking small outputs cannot free 10% of E.
        assert!(c.stats().tokens > c.budget.of(0.6));
        assert_eq!(c.report().masked_total, 0);
    }

    #[test]
    fn eviction_never_orphans_tool_results() {
        for size in [400, 1_500, 3_000, 6_000] {
            let mut c = ctx();
            c.begin_turn("task", None);
            for n in 0..40 {
                let (a, b) = (format!("a{n}"), format!("b{n}"));
                step(&mut c, &[&a, &b], size);
                assert_valid(&c);
            }
            assert!(
                c.report().evicted_total > 0,
                "size {size}: eviction exercised"
            );
        }
    }

    #[test]
    fn eviction_leaves_one_compaction_record_and_keeps_requests() {
        let mut c = ctx();
        c.begin_turn("first task", None);
        step(&mut c, &["x"], 100);
        c.add_message(Message::assistant("done"));
        c.begin_turn("second task", None);
        // Masked steps still cost ~85 tokens each, so enough of them push the
        // context past evict_at once masking has nothing left to free.
        for n in 0..60 {
            step(&mut c, &[&format!("s{n}")], 1_500);
        }
        assert_valid(&c);
        let rep = c.report();
        assert!(rep.evicted_total > 0);
        assert!(rep.records >= 2, "several evictions: {}", rep.records);
        let t = texts(&c);
        assert_eq!(t[1], "first task", "pinned first instruction, unchanged");
        assert_eq!(t[2], "second task", "current instruction kept in order");
        let records: Vec<&Message> = c
            .messages()
            .iter()
            .filter(|m| m.kind == Kind::Record)
            .collect();
        assert_eq!(records.len(), 1, "one record, replaced at each eviction");
        assert_eq!(
            c.messages()[3].kind,
            Kind::Record,
            "record after the requests"
        );
        let record = records[0].content.as_deref().unwrap();
        assert!(record.starts_with(&format!("[Compaction record {}:", rep.records)));
        assert!(record.contains("## Digest\nFiles read: "), "{record}");
        assert!(
            record.contains("x.rs"),
            "digest covers earlier turns: {record}"
        );
        assert!(record.contains("s0.rs"), "{record}");
        let last = c.messages().last().unwrap();
        assert_eq!(last.tool_call_id.as_deref(), Some("s59"));
        assert!(
            c.stats().tokens <= c.budget.of(0.4) + 100,
            "near the target"
        );
    }

    #[test]
    fn old_requests_beyond_the_budget_are_removed_and_counted() {
        let mut c = ctx();
        c.config.context.keep_requests = 0.05; // ~170 tokens of earlier requests
        for turn in 0..8 {
            c.begin_turn(&format!("request {turn} {}", "r".repeat(300)), None);
            for n in 0..6 {
                step(&mut c, &[&format!("t{turn}s{n}")], 1_500);
            }
            c.add_message(Message::assistant("done"));
        }
        assert_valid(&c);
        let kept: Vec<&str> = c
            .messages()
            .iter()
            .filter(|m| m.kind == Kind::Request)
            .filter_map(|m| m.content.as_deref())
            .collect();
        assert!(kept[0].starts_with("request 0"), "first request pinned");
        assert!(
            kept.last().unwrap().starts_with("request 7"),
            "current kept"
        );
        assert!(kept.len() < 8, "some old requests removed: {}", kept.len());
        let record = c
            .messages()
            .iter()
            .find(|m| m.kind == Kind::Record)
            .and_then(|m| m.content.as_deref())
            .unwrap();
        assert!(
            record.contains("earlier operator requests were removed"),
            "{record}"
        );
    }

    #[test]
    fn rollback_removes_a_turn_compacted_midway_and_keeps_the_record() {
        let mut c = ctx();
        c.begin_turn("first task", None);
        step(&mut c, &["x"], 100);
        c.add_message(Message::assistant("first answer"));
        let before: Vec<String> = texts(&c);

        let cp = c.checkpoint();
        c.begin_turn("second task", None);
        for n in 0..60 {
            step(&mut c, &[&format!("s{n}")], 1_500);
        }
        assert!(c.report().evicted_total > 0, "the turn compacted midway");
        c.rollback_to(cp);

        assert_valid(&c);
        let ids: Vec<&str> = c
            .messages()
            .iter()
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();
        assert!(
            ids.iter().all(|id| !id.starts_with('s')),
            "no step of the cancelled turn remains: {ids:?}"
        );
        let t = texts(&c);
        assert!(!t.contains(&"second task".to_string()));
        assert_eq!(t[1], "first task", "pinned first instruction kept");
        assert_eq!(
            c.messages()
                .iter()
                .filter(|m| m.kind == Kind::Record)
                .count(),
            1,
            "the record of evicted earlier steps stays"
        );
        assert!(t.len() < before.len() + 1, "{t:?}");

        // A later turn starts cleanly after the rollback.
        c.begin_turn("third task", None);
        step(&mut c, &["t0"], 100);
        assert_valid(&c);
    }

    #[test]
    fn rollback_without_a_new_turn_changes_nothing() {
        let mut c = ctx();
        c.begin_turn("task", None);
        step(&mut c, &["a"], 100);
        let before = texts(&c);
        let cp = c.checkpoint();
        c.rollback_to(cp);
        assert_eq!(texts(&c), before);
    }

    #[test]
    fn cancelling_the_first_turn_keeps_its_instruction() {
        let mut c = ctx();
        let cp = c.checkpoint();
        c.begin_turn("only task", None);
        step(&mut c, &["a"], 100);
        c.rollback_to(cp);
        assert_eq!(texts(&c)[1..], ["only task".to_string()]);
        assert_valid(&c);
    }

    #[test]
    fn normalize_repairs_pairing() {
        let mut c = ctx();
        c.begin_turn("task", None);
        c.add_message(Message::tool_result("stray", "orphan before any call"));
        c.add_message(Message::assistant_tool_calls(&response(vec![
            call("p"),
            call("q"),
        ])));
        c.add_message(Message::tool_result("p", "ok"));
        c.add_message(Message::tool_result("p", "duplicate"));
        c.add_message(Message::tool_result("zzz", "unknown id"));
        c.begin_turn("next", None);
        assert_valid(&c);
        let t = texts(&c);
        assert!(
            !t.iter()
                .any(|s| s.contains("orphan") || s.contains("duplicate") || s.contains("unknown"))
        );
        let q = c
            .messages()
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("q"));
        assert_eq!(q.and_then(|m| m.content.as_deref()), Some(NOT_EXECUTED));
        assert_eq!(
            c.messages().last().unwrap().content.as_deref(),
            Some("next")
        );
    }

    #[test]
    fn react_calls_are_recorded_with_matching_ids() {
        // ReAct: no native tool_calls from the server, only a parsed call.
        let r = CompletionResponse {
            content: Some("```action\n{}\n```".into()),
            tool_calls: Some(vec![call("local-1")]),
            usage: None,
            finish_reason: None,
            reasoning: None,
            request_sha256: None,
            response_id: None,
            served_model: None,
        };
        let m = Message::assistant_tool_calls(&r);
        assert_eq!(
            tool_call_ids(m.tool_calls.as_ref().unwrap()),
            vec!["local-1"]
        );
    }

    #[test]
    fn overflow_adopts_server_window_and_frees_space() {
        let mut c = ctx();
        c.begin_turn("task", None);
        for n in 0..3 {
            step(&mut c, &[&format!("c{n}")], 600);
        }
        let before = c.stats().tokens;
        assert!(c.handle_overflow(Some(3_000), None));
        assert_eq!(c.stats().budget.window, 3_000);
        assert!(c.stats().tokens < before);
        assert_valid(&c);
    }

    #[test]
    fn estimate_counts_tool_call_arguments_and_calibrates() {
        let mut c = ctx();
        let before = c.estimated_tokens();
        let big = ToolCall {
            id: "w".into(),
            name: "write_file".into(),
            args: json!({ "content": "x".repeat(3_000) }),
        };
        c.add_message(Message::assistant_tool_calls(&response(vec![big])));
        assert!(c.estimated_tokens() >= before + 1_000);

        c.prepare_request();
        c.record_usage(Some(TokenUsage {
            prompt_tokens: 1_234,
            completion_tokens: 1,
            total_tokens: 0,
        }));
        // Exact: the reported usage for the request just sent.
        assert_eq!(c.stats().tokens, 1_234);
        assert_eq!(c.stats().token_source, "usage");
        // New messages are the reported count plus an estimate of the delta.
        c.add_message(Message::tool_result("w", &"y".repeat(400)));
        assert!(c.stats().tokens > 1_234);
        assert_eq!(c.stats().token_source, "usage+estimate");
    }

    #[test]
    fn report_categories_cover_every_message_and_add_up() {
        use crate::context_report as r;
        let mut c = ctx();
        c.set_instructions(crate::instructions::Loaded {
            files: vec![crate::instructions::File {
                source: "AGENTS.md".into(),
                path: "/repo/AGENTS.md".into(),
                scope: crate::instructions::Scope::Repository,
                bytes: 600,
                raw_sha256: "00".repeat(32),
                text: "Run `cargo test` before finishing.\n".repeat(15),
                truncated: false,
                removed_chars: 0,
                skipped: None,
            }],
            cap: 8192,
            overridden: false,
        });
        c.begin_turn("task", None);
        for n in 0..5 {
            step(&mut c, &[&format!("c{n}")], 1_500);
        }
        c.messages.last_mut().unwrap().cap_bytes = Some(1_500);
        c.add_message(Message::notice(NUDGE));
        c.add_message(Message::assistant("done"));

        let rep = c.report();
        assert_eq!(rep.rows.len(), c.messages().len(), "one row per message");
        let sum: usize = rep.categories.iter().map(|k| k.tokens).sum();
        assert_eq!(sum, rep.total, "categories add up to the total");
        let rows: usize = rep.rows.iter().map(|k| k.tokens).sum();
        assert_eq!(rows + rep.categories[r::TOOLS].tokens, rep.total);
        assert!(rep.categories[r::INSTRUCTIONS].tokens > 100);
        assert_eq!(rep.categories[r::INSTRUCTIONS].items, "AGENTS.md");
        assert!(rep.categories[r::OUTPUTS_MASKED].tokens > 0);
        assert_eq!(rep.categories[r::NOTICES].items, "1");
        assert_eq!(rep.categories[r::REQUESTS].items, "1");
        assert!(rep.categories[r::OUTPUTS_FULL].items.contains("1 capped"));
        assert!(rep.rows.iter().any(|k| k.view == "capped"));
        assert!(rep.mask_events > 0 && rep.last.is_some());

        let text = rep.render(false);
        for line in [
            "Window W 4,000 (configured) = usable U 3,388 + reply reserve 100 + safety margin 512",
            "Project instructions",
            "Largest items",
            "Last: turn 0, mask (threshold)",
            "Hint: The window is small",
        ] {
            assert!(text.contains(line), "missing {line:?} in\n{text}");
        }
        let detail = rep.render(true);
        assert!(detail.contains("read_file c4.rs"), "{detail}");
    }

    #[test]
    fn report_without_instructions_or_requests() {
        let c = ctx();
        let rep = c.report();
        assert_eq!(
            rep.categories.iter().map(|k| k.tokens).sum::<usize>(),
            rep.total
        );
        assert_eq!(
            rep.categories[crate::context_report::INSTRUCTIONS].tokens,
            0
        );
        assert_eq!(rep.source, "default-ratio");
        assert!(rep.render(false).contains("Last: none"));
    }

    #[test]
    fn report_names_the_window_source() {
        let mut config = Config::default();
        config.context.window = None;
        let mut c = AgentContext::new(config, Vec::new());
        let text = c.report().render(false);
        assert!(text.contains("Window W 32,768 (assumed) ="), "{text}");
        assert!(text.contains("set\n        [context].window"), "{text}");
        let mut hints = text.lines().skip_while(|l| !l.starts_with("  Hint:"));
        assert!(hints.clone().count() >= 2, "{text}");
        assert!(hints.all(|l| l.len() <= 80), "{text}");
        c.set_window(65_536, crate::context_report::WINDOW_SERVER);
        let text = c.report().render(false);
        assert!(text.contains("Window W 65,536 (from server) ="), "{text}");
        assert!(!text.contains("[context].window"), "{text}");
    }

    #[test]
    fn tokenize_count_is_exact_until_history_changes() {
        let mut c = ctx();
        c.begin_turn("task", None);
        step(&mut c, &["a"], 900);
        c.normalize();
        c.record_counted(777);
        assert_eq!(c.stats().tokens, 777);
        assert_eq!(c.stats().token_source, "tokenize");
        c.seal_request();
        // The reported usage for the same request agrees, so the count stays exact.
        c.record_usage(Some(TokenUsage {
            prompt_tokens: 777,
            completion_tokens: 5,
            total_tokens: 782,
        }));
        assert_eq!(c.stats().tokens, 777);
    }
}
