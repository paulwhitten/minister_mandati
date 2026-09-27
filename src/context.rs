//! Conversation state: message history, system prompt, and budget enforcement.

use serde::Serialize;

use crate::client::{CompletionResponse, TokenUsage};
use crate::config::Config;
use crate::tools::ToolSpec;

/// A single chat message in OpenAI wire format.
#[derive(Clone, Serialize)]
pub struct Message {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Present on assistant turns that requested tools (raw OpenAI `tool_calls`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<serde_json::Value>,
    /// Present on `tool` role messages, matching the originating call id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn system(content: &str) -> Self {
        Self {
            role: "system".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn user(content: &str) -> Self {
        Self {
            role: "user".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    /// Echo the assistant turn (content + its tool-call requests) back into history.
    pub fn assistant_tool_calls(response: &CompletionResponse) -> Self {
        Self {
            role: "assistant".into(),
            content: response.content.clone().filter(|c| !c.is_empty()),
            tool_calls: response.raw_tool_calls.clone(),
            tool_call_id: None,
        }
    }

    pub fn tool_result(call_id: &str, content: &str) -> Self {
        Self {
            role: "tool".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(call_id.into()),
        }
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

pub struct AgentContext {
    pub config: Config,
    pub tool_specs: Vec<ToolSpec>,
    messages: Vec<Message>,
    /// Number of leading messages that are never evicted (system + first user turn).
    pinned: usize,
    usage: TokenLedger,
}

impl AgentContext {
    pub fn new(config: Config, tool_specs: Vec<ToolSpec>) -> Self {
        let system_prompt = config.system_prompt();
        tracing::debug!(%system_prompt, "composed system prompt");
        Self {
            config,
            tool_specs,
            messages: vec![Message::system(&system_prompt)],
            pinned: 1,
            usage: TokenLedger::default(),
        }
    }

    /// Fold one response's token usage into the running task total. A missing
    /// `usage` is counted separately so cumulative totals stay trustworthy.
    pub fn record_usage(&mut self, usage: Option<TokenUsage>) {
        self.usage.requests += 1;
        match usage {
            Some(u) => {
                self.usage.prompt_tokens += u.prompt_tokens;
                self.usage.completion_tokens += u.completion_tokens;
                self.usage.total_tokens += u.total_tokens;
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

    pub fn add_message(&mut self, message: Message) {
        // Pin the first user instruction alongside the system prompt.
        if self.pinned == 1 && message.role == "user" {
            self.pinned = 2;
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
    }

    /// Start a fresh session: keep only the system prompt and clear the ledger.
    pub fn reset(&mut self) {
        self.messages.truncate(1);
        self.pinned = 1;
        self.usage = TokenLedger::default();
    }

    /// Trim history to satisfy both the turn window and a coarse token budget.
    pub fn enforce_budget(&mut self) {
        let max_messages = self.pinned + self.config.agent.max_history_turns;
        while self.messages.len() > max_messages || self.estimated_tokens() > self.token_ceiling() {
            if !self.evict_oldest_unpinned() {
                break;
            }
        }
    }

    fn evict_oldest_unpinned(&mut self) -> bool {
        if self.messages.len() > self.pinned {
            self.messages.remove(self.pinned);
            true
        } else {
            false
        }
    }

    /// Coarse chars/4 token estimate across all message content.
    fn estimated_tokens(&self) -> usize {
        let chars: usize = self
            .messages
            .iter()
            .filter_map(|m| m.content.as_ref())
            .map(|c| c.len())
            .sum();
        chars / 4
    }

    fn token_ceiling(&self) -> usize {
        // Leave headroom for the response. A real tokenizer/context window can replace this.
        32_000usize.saturating_sub(self.config.agent.max_tokens)
    }
}
