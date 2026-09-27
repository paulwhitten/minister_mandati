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

use crate::budget::{Budget, Estimator, FALLBACK_WINDOW, MIN_MASK_GAIN};
use crate::client::{CompletionResponse, TokenUsage};
use crate::config::Config;
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
}

impl Message {
    fn new(role: &str, content: Option<String>) -> Self {
        Self {
            role: role.into(),
            content,
            tool_calls: None,
            tool_call_id: None,
            masked: false,
        }
    }

    pub fn system(content: &str) -> Self {
        Self::new("system", Some(content.into()))
    }

    pub fn user(content: &str) -> Self {
        Self::new("user", Some(content.into()))
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
        let mut m = Self::new(
            "assistant",
            response.content.clone().filter(|c| !c.is_empty()),
        );
        m.tool_calls = response
            .tool_calls
            .as_ref()
            .filter(|calls| !calls.is_empty())
            .map(|calls| {
                Value::Array(
                    calls
                        .iter()
                        .map(|c| {
                            json!({
                                "id": c.id,
                                "type": "function",
                                "function": { "name": c.name, "arguments": c.args.to_string() },
                            })
                        })
                        .collect(),
                )
            });
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

/// Snapshot of the context budget, for `/context` and logs.
#[derive(Debug, Clone, Copy)]
pub struct ContextStats {
    pub budget: Budget,
    pub estimated_tokens: usize,
    pub estimate_source: &'static str,
    pub messages: usize,
    pub masked_total: usize,
    pub evicted_total: usize,
}

/// Result text for a requested tool call that never ran (turn aborted, loop
/// guard stop, error). Keeps the request/result pairing the API requires.
const NOT_EXECUTED: &str = "Not executed: the turn ended before this call ran.";
/// Longest tool-call argument text quoted in a placeholder.
const PLACEHOLDER_ARGS_CHARS: usize = 200;

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
    /// The current session and its (optional) transcript.
    pub session: Session,
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
}

impl AgentContext {
    pub fn new(config: Config, tool_specs: Vec<ToolSpec>) -> Self {
        let system_prompt = config.system_prompt();
        tracing::debug!(%system_prompt, "composed system prompt");
        let window = config.context.window.unwrap_or(FALLBACK_WINDOW);
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
            messages: vec![Message::system(&system_prompt)],
            pinned: 1,
            turn_start: None,
            first_instruction: None,
            usage: TokenLedger::default(),
            budget,
            estimator: Estimator::default(),
            tool_schema_chars,
            chars_at_last_request: 0,
            masked_total: 0,
            evicted_total: 0,
        }
    }

    /// Adopt a context window learned from the server (`/v1/models` or an
    /// overflow error) and recompute the budget.
    pub fn set_window(&mut self, window: usize) {
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
            estimated_tokens: self.estimated_tokens(),
            estimate_source: self.estimator.source(),
            messages: self.messages.len(),
            masked_total: self.masked_total,
            evicted_total: self.evicted_total,
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
                self.estimator
                    .calibrate(self.chars_at_last_request, u.prompt_tokens);
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
    pub fn begin_turn(&mut self, instruction: &str) {
        self.normalize();
        self.turn_start = Some(self.messages.len());
        self.add_message(Message::user(instruction));
    }

    pub fn add_message(&mut self, message: Message) {
        // Pin the first user instruction alongside the system prompt.
        if self.pinned == 1 && message.role == "user" {
            self.pinned = 2;
            self.first_instruction = message.content.clone();
        }
        self.messages.push(message);
    }

    /// Current history length, used as a rollback point for a cancellable turn.
    pub fn checkpoint(&self) -> usize {
        self.messages.len()
    }

    /// Discard messages appended since a `checkpoint`, so a cancelled turn leaves
    /// no dangling assistant tool-call without its tool results. Never trims the
    /// pinned leaders (system prompt + first instruction).
    pub fn rollback_to(&mut self, len: usize) {
        let target = len.max(self.pinned.max(1));
        if target < self.messages.len() {
            self.messages.truncate(target);
        }
        if self.turn_start.is_some_and(|t| t >= self.messages.len()) {
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
    }

    /// Run before every model request: normalize, then mask and evict as the
    /// budget requires. Records the size sent, for estimator calibration.
    pub fn prepare_request(&mut self) {
        self.normalize();
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
        let est = self.estimated_tokens();
        if est > b.usable {
            tracing::warn!(
                est_tokens = est,
                usable = b.usable,
                "context estimate exceeds the usable window; nothing more is safely removable"
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
            self.set_window(w);
        }
        if let Some(p) = server_prompt_tokens {
            self.estimator.calibrate(self.chars_at_last_request, p);
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
                out.push(Message::tool_result(id, NOT_EXECUTED));
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
        tracing::info!(
            stage = "mask",
            reason,
            results_masked = count,
            est_tokens_before = before,
            est_tokens_after = self.estimated_tokens(),
            target,
            source = self.estimator.source(),
            "context compaction"
        );
        let after = self.estimated_tokens();
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
    /// `target`, then note the removal on the pinned first instruction.
    /// Returns the number of messages removed.
    fn evict(&mut self, target: usize, reason: &str) -> usize {
        let before = self.estimated_tokens();
        let mut removed = 0;
        let mut call_ids: Vec<String> = Vec::new();
        while self.estimated_tokens() > target {
            let group = self.evict_oldest_group();
            if group.is_empty() {
                break;
            }
            removed += group.len();
            call_ids.extend(group.into_iter().filter_map(|m| m.tool_call_id));
        }
        if removed == 0 {
            return 0;
        }
        self.evicted_total += removed;
        if self.pinned == 2
            && let Some(original) = &self.first_instruction
        {
            self.messages[1].content = Some(format!(
                "{original}\n\n[Note: {} earlier messages were removed to fit the context window.]",
                self.evicted_total
            ));
        }
        tracing::info!(
            stage = "evict",
            reason,
            messages_removed = removed,
            est_tokens_before = before,
            est_tokens_after = self.estimated_tokens(),
            target,
            source = self.estimator.source(),
            "context compaction"
        );
        let after = self.estimated_tokens();
        self.session.record(
            "compaction",
            json!({ "stage": "evict", "reason": reason, "messages_removed": removed,
                    "call_ids": call_ids, "est_tokens_before": before,
                    "est_tokens_after": after, "target": target }),
        );
        removed
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
            if self.turn_start == Some(start) {
                start += 1; // the current instruction stays
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

    fn estimated_tokens(&self) -> usize {
        self.estimator.tokens(self.total_chars())
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
        AgentContext::new(config, Vec::new())
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
        c.begin_turn("task");
        // ~500 tokens per result: crosses mask_at (60% of 3388) after a few steps.
        for n in 0..5 {
            step(&mut c, &[&format!("c{n}")], 1_500);
        }
        let s = c.stats();
        assert!(s.masked_total > 0, "old outputs masked");
        assert_eq!(s.evicted_total, 0, "no step evicted while masking suffices");
        assert!(s.estimated_tokens <= c.budget.of(0.6));
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
        c.begin_turn("task");
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
        c.begin_turn("task");
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
        c.begin_turn(&"t".repeat(7_000)); // big instruction, not maskable
        step(&mut c, &["a"], 200);
        step(&mut c, &["b"], 200);
        // Over mask_at, but masking small outputs cannot free 10% of E.
        assert!(c.stats().estimated_tokens > c.budget.of(0.6));
        assert_eq!(c.stats().masked_total, 0);
    }

    #[test]
    fn eviction_never_orphans_tool_results() {
        for size in [400, 1_500, 3_000, 6_000] {
            let mut c = ctx();
            c.begin_turn("task");
            for n in 0..40 {
                let (a, b) = (format!("a{n}"), format!("b{n}"));
                step(&mut c, &[&a, &b], size);
                assert_valid(&c);
            }
            assert!(
                c.stats().evicted_total > 0,
                "size {size}: eviction exercised"
            );
        }
    }

    #[test]
    fn eviction_keeps_instructions_and_newest_step_and_adds_note() {
        let mut c = ctx();
        c.begin_turn("first task");
        step(&mut c, &["x"], 100);
        c.add_message(Message::assistant("done"));
        c.begin_turn("second task");
        // Masked steps still cost ~85 tokens each, so enough of them push the
        // context past evict_at once masking has nothing left to free.
        for n in 0..60 {
            step(&mut c, &[&format!("s{n}")], 1_500);
        }
        assert_valid(&c);
        assert!(c.stats().evicted_total > 0);
        let t = texts(&c);
        assert!(t[1].starts_with("first task"), "pinned first instruction");
        assert!(
            t[1].contains("earlier messages were removed"),
            "eviction note"
        );
        assert!(
            t.contains(&"second task".to_string()),
            "current instruction"
        );
        let last = c.messages().last().unwrap();
        assert_eq!(last.tool_call_id.as_deref(), Some("s59"));
    }

    #[test]
    fn normalize_repairs_pairing() {
        let mut c = ctx();
        c.begin_turn("task");
        c.add_message(Message::tool_result("stray", "orphan before any call"));
        c.add_message(Message::assistant_tool_calls(&response(vec![
            call("p"),
            call("q"),
        ])));
        c.add_message(Message::tool_result("p", "ok"));
        c.add_message(Message::tool_result("p", "duplicate"));
        c.add_message(Message::tool_result("zzz", "unknown id"));
        c.begin_turn("next");
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
        c.begin_turn("task");
        for n in 0..3 {
            step(&mut c, &[&format!("c{n}")], 600);
        }
        let before = c.stats().estimated_tokens;
        assert!(c.handle_overflow(Some(3_000), None));
        assert_eq!(c.stats().budget.window, 3_000);
        assert!(c.stats().estimated_tokens < before);
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
        let sent = c.chars_at_last_request;
        c.record_usage(Some(TokenUsage {
            prompt_tokens: (sent / 4) as u64,
            completion_tokens: 1,
            total_tokens: 0,
        }));
        assert_eq!(c.stats().estimate_source, "usage-calibrated");
    }
}
