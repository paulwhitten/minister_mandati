//! Reading transcripts: the typed record definitions (the source of
//! `schemas/transcript.schema.json`), session listing, and rebuilding a
//! session's context for `--resume`, from a `context` record or by replay.
//! Design: docs/design/session-resume.md. Writers live in session.rs,
//! agent.rs and context.rs; tests check that what they write matches these
//! types and the committed schema.
//!
//! Readers are tolerant (evolution rule 1): unknown fields are ignored and
//! unknown record types or open-enum values parse as `Unknown`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::context::{Message, NOT_EXECUTED, NUDGE};
use crate::tools::truncate_middle;

/// Layout version of `context` records. Bump only for breaking changes
/// (evolution rule 10); readers that do not know a version replay instead.
pub const CONTEXT_SCHEMA: u32 = 1;

// ---------------------------------------------------------------------------
// Record types

/// One transcript line: the common header and a typed record.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Line {
    pub seq: u64,
    /// UTC, RFC 3339 with milliseconds.
    pub ts: String,
    #[serde(flatten)]
    pub record: Record,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Record {
    SessionStart(SessionStart),
    SessionResumed(SessionResumed),
    TurnStart(TurnStart),
    ModelResponse(ModelResponse),
    ModelError(ModelError),
    Approval(ApprovalRecord),
    ToolResult(ToolResult),
    Compaction(Compaction),
    LoopGuard(LoopGuard),
    TurnEnd(TurnEnd),
    Context(ContextRecord),
    TranscriptDisabled(TranscriptDisabled),
    SessionEnd(SessionEnd),
    /// A record type this mima does not know (written by a newer one).
    #[serde(other)]
    #[cfg_attr(test, schemars(skip))]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct SessionStart {
    /// Transcript major version (see `session::SCHEMA`).
    pub schema: u32,
    pub session: String,
    pub session_started: String,
    #[serde(default)]
    pub turns_before: u64,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub mima_version: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub window: Option<u64>,
    #[serde(default)]
    pub budget: Option<u64>,
    #[serde(default)]
    pub approvals: Option<Approvals>,
    #[serde(default)]
    pub allowed_paths: Option<Vec<String>>,
    #[serde(default)]
    pub token_counting: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Approvals {
    pub bash: bool,
    pub writes: bool,
    #[serde(default)]
    pub auto_approve_bash: Vec<String>,
}

/// Written when a session is continued (`--resume`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct SessionResumed {
    /// "context" (rebuilt from a context record) or "replay".
    pub method: String,
    /// Why replay was used, when it was.
    #[serde(default)]
    pub reason: Option<String>,
    /// The context record used, when method is "context".
    #[serde(default)]
    pub from_seq: Option<u64>,
    pub mima_version: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// Files that differ on disk from what the session last saw.
    #[serde(default)]
    pub changed_files: Vec<FileChange>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct FileChange {
    pub path: String,
    /// "changed" or "missing".
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct TurnStart {
    pub turn: u64,
    pub instruction: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ModelResponse {
    pub turn: u64,
    pub step: u64,
    pub duration_ms: u64,
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<CallRecord>,
    #[serde(default)]
    pub usage: Option<Usage>,
    #[serde(default)]
    pub counted_prompt_tokens: Option<u64>,
    #[serde(default)]
    pub count_source: Option<String>,
    /// "length" means the reply was cut off at `max_tokens`.
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct CallRecord {
    pub id: String,
    pub name: String,
    /// The call's arguments as the model gave them (a JSON object).
    pub args: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Usage {
    pub prompt: u64,
    pub completion: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ModelError {
    pub turn: u64,
    pub step: u64,
    pub duration_ms: u64,
    pub error: String,
    pub overflow: bool,
    pub retry: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ApprovalRecord {
    pub turn: u64,
    pub call_id: String,
    /// Open enum: not_required, auto_approved, approved, denied,
    /// skipped_duplicate, not_requested_invalid.
    pub decision: String,
    #[serde(default)]
    pub preview: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ToolResult {
    pub turn: u64,
    pub call_id: String,
    pub tool: String,
    pub duration_ms: u64,
    pub failed: bool,
    /// False when the call never ran (denied).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executed: Option<bool>,
    pub bytes: u64,
    pub sent_bytes: u64,
    /// Full output (cut at `[session].max_output_bytes`).
    pub output: String,
    /// Stage 0 cap in effect: the model saw `output` cut to this size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cap_bytes: Option<u64>,
    /// The file a read/edit/write left behind, to detect later changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<FileState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct FileState {
    /// As given to the tool (relative paths are relative to the session cwd).
    pub path: String,
    /// FNV-1a 64 of the file's bytes, 16 hex digits.
    pub fnv1a64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Compaction {
    /// Open enum: normalize, mask, evict.
    pub stage: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_ids: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_results: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped_results: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages_removed: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub est_tokens_before: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub est_tokens_after: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct LoopGuard {
    pub turn: u64,
    /// Open enum: nudge, terminate.
    pub action: String,
    pub repeats: u64,
    pub tool: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct TurnEnd {
    pub turn: u64,
    /// Open enum: answered, loop_guard, step_cap, error, cancelled.
    pub outcome: String,
    pub answer: Option<String>,
    pub error: Option<String>,
    #[serde(default)]
    pub tokens: Option<Tokens>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Tokens {
    pub requests: u64,
    pub prompt: u64,
    pub completion: u64,
    pub unreported_requests: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct TranscriptDisabled {
    pub turns: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct SessionEnd {
    /// Open enum: exit, eof, new, task_done, interrupted, fatal, resume.
    pub reason: String,
    pub turns: u64,
    #[serde(default)]
    pub tokens: Option<Tokens>,
    #[serde(default)]
    pub error: Option<String>,
}

/// What the model sees, by reference to earlier records of this transcript.
/// Written at each turn end and after each compaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ContextRecord {
    pub context_schema: u32,
    pub turn: u64,
    /// Open enum: turn_end, compaction.
    pub cause: String,
    pub window: u64,
    pub budget: u64,
    pub tokens: u64,
    pub count_source: String,
    pub masked_total: u64,
    pub evicted_total: u64,
    /// The request's messages after the system prompt, in order.
    pub entries: Vec<Entry>,
    pub check: Check,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Entry {
    /// Record this message comes from; absent for synthetic messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(default, skip_serializing_if = "View::is_full")]
    pub view: View,
    /// Placeholder (masked), appended note (noted), or the whole message
    /// (synthetic).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cap_bytes: Option<u64>,
    /// Synthetic messages only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
}

/// How the model's view of a message differs from its record. A closed enum
/// (evolution rule 8): an unknown value makes the record unusable, and the
/// reader replays instead of guessing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum View {
    #[default]
    Full,
    Capped,
    Masked,
    Noted,
    Synthetic,
}

impl View {
    fn is_full(&self) -> bool {
        *self == View::Full
    }
}

/// Drift detection over the rebuilt messages (not security).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Check {
    pub messages: u64,
    pub chars: u64,
    pub fnv1a64: String,
}

// ---------------------------------------------------------------------------
// Hashing

/// FNV-1a 64: small and stable across Rust releases (unlike std's hasher).
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

pub fn hex(h: u64) -> String {
    format!("{h:016x}")
}

/// The check over a message list: count, serialized size, and hash.
pub fn check(messages: &[Message]) -> Check {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut chars = 0u64;
    for m in messages {
        let text = serde_json::to_string(m).unwrap_or_default();
        chars += text.len() as u64;
        for b in text.bytes().chain(std::iter::once(b'\n')) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    Check {
        messages: messages.len() as u64,
        chars,
        fnv1a64: hex(h),
    }
}

// ---------------------------------------------------------------------------
// Loading and listing

/// A parsed transcript.
pub struct Transcript {
    pub lines: Vec<Line>,
    /// Lines that did not parse (reported, never fatal).
    pub bad_lines: usize,
}

impl Transcript {
    pub fn load(path: &Path) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let mut lines = Vec::new();
        let mut bad_lines = 0;
        for l in text.lines().filter(|l| !l.trim().is_empty()) {
            match serde_json::from_str::<Line>(l) {
                Ok(line) => lines.push(line),
                Err(_) => bad_lines += 1,
            }
        }
        Ok(Self { lines, bad_lines })
    }

    /// The first `session_start` (the session's identity and settings).
    pub fn start(&self) -> Option<&SessionStart> {
        self.lines.iter().find_map(|l| match &l.record {
            Record::SessionStart(s) => Some(s),
            _ => None,
        })
    }

    /// The most recent settings: the last `session_start` (recording may
    /// have been re-enabled) followed by any resumes.
    fn latest_start(&self) -> Option<&SessionStart> {
        self.lines.iter().rev().find_map(|l| match &l.record {
            Record::SessionStart(s) => Some(s),
            _ => None,
        })
    }

    pub fn next_seq(&self) -> u64 {
        self.lines.iter().map(|l| l.seq + 1).max().unwrap_or(0)
    }

    pub fn turns(&self) -> u64 {
        self.lines
            .iter()
            .filter_map(|l| match &l.record {
                Record::TurnStart(t) => Some(t.turn),
                _ => None,
            })
            .max()
            .unwrap_or(0)
    }

    fn by_seq(&self) -> HashMap<u64, &Record> {
        self.lines.iter().map(|l| (l.seq, &l.record)).collect()
    }
}

/// One line of `mima --sessions`.
pub struct SessionSummary {
    pub id: String,
    pub path: PathBuf,
    pub started: String,
    pub turns: u64,
    pub first_instruction: String,
    pub model: String,
    /// How it last ended, or "open" if it never recorded an end.
    pub ended: String,
    pub resumable: bool,
}

/// Recorded sessions in `dir`, newest first.
pub fn list_sessions(dir: &Path) -> Vec<SessionSummary> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|r| {
            r.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
                .collect()
        })
        .unwrap_or_default();
    paths.sort();
    paths.reverse();
    paths
        .into_iter()
        .filter_map(|path| {
            let t = Transcript::load(&path).ok()?;
            let start = t.start()?;
            let first_instruction = t
                .lines
                .iter()
                .find_map(|l| match &l.record {
                    Record::TurnStart(ts) => Some(ts.instruction.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            let ended = t
                .lines
                .iter()
                .rev()
                .find_map(|l| match &l.record {
                    Record::SessionEnd(e) => Some(e.reason.clone()),
                    Record::SessionResumed(_) | Record::TurnStart(_) => Some("open".into()),
                    _ => None,
                })
                .unwrap_or_else(|| "open".into());
            Some(SessionSummary {
                id: path.file_stem()?.to_string_lossy().into_owned(),
                started: start.session_started.clone(),
                turns: t.turns(),
                model: t.latest_start()?.model.clone().unwrap_or_default(),
                resumable: start.schema <= crate::session::SCHEMA,
                first_instruction,
                ended,
                path,
            })
        })
        .collect()
}

/// Finds a transcript by id (a unique prefix is enough) or "last".
pub fn find(dir: &Path, id: &str) -> Result<PathBuf, String> {
    let sessions = list_sessions(dir);
    if id == "last" {
        return sessions
            .first()
            .map(|s| s.path.clone())
            .ok_or_else(|| format!("no recorded sessions in {}", dir.display()));
    }
    let matches: Vec<&SessionSummary> = sessions.iter().filter(|s| s.id.starts_with(id)).collect();
    match matches.as_slice() {
        [one] => Ok(one.path.clone()),
        [] => Err(format!(
            "no recorded session matches {id:?} in {}",
            dir.display()
        )),
        many => Err(format!(
            "{id:?} matches {} sessions; give more of the id",
            many.len()
        )),
    }
}

// ---------------------------------------------------------------------------
// Rebuilding

/// A rebuilt conversation (without the system prompt).
pub struct Rebuilt {
    pub messages: Vec<Message>,
    pub first_instruction: Option<String>,
    pub masked_total: usize,
    pub evicted_total: usize,
    /// "context" or "replay".
    pub method: &'static str,
    /// Why replay was used.
    pub reason: Option<String>,
    pub from_seq: Option<u64>,
}

/// Rebuilds from the latest usable `context` record, else by replay.
/// `default_cap` caps tool outputs whose record has no `cap_bytes`.
pub fn rebuild(t: &Transcript, default_cap: usize) -> Rebuilt {
    match from_context(t) {
        Ok(r) => r,
        Err(reason) => {
            tracing::info!(%reason, "resume: replaying the transcript");
            let mut r = replay(t, default_cap);
            r.reason = Some(reason);
            r
        }
    }
}

fn tool_message(r: &ToolResult, cap: Option<u64>) -> Message {
    let content = match cap {
        Some(c) => truncate_middle(r.output.clone(), c as usize),
        None => r.output.clone(),
    };
    let mut m = Message::tool_result(&r.call_id, &content);
    m.cap_bytes = cap.filter(|c| (*c as usize) < r.output.len());
    m
}

fn assistant_message(r: &ModelResponse) -> Message {
    let calls: Vec<(&str, &str, &Value)> = r
        .tool_calls
        .iter()
        .map(|c| (c.id.as_str(), c.name.as_str(), &c.args))
        .collect();
    Message::assistant_calls(r.content.clone(), &calls)
}

/// Rebuilds exactly what the latest `context` record describes. Any doubt
/// (unknown version, dangling reference, failed check) is an error, and the
/// caller replays instead.
fn from_context(t: &Transcript) -> Result<Rebuilt, String> {
    let (seq, rec) = t
        .lines
        .iter()
        .rev()
        .find_map(|l| match &l.record {
            Record::Context(c) => Some((l.seq, c)),
            _ => None,
        })
        .ok_or("no context record")?;
    if rec.context_schema != CONTEXT_SCHEMA {
        return Err(format!(
            "context record version {} is not {CONTEXT_SCHEMA}",
            rec.context_schema
        ));
    }
    // A turn that started after the last context record never completed.
    let later_turn = t
        .lines
        .iter()
        .any(|l| l.seq > seq && matches!(l.record, Record::TurnEnd(_)));
    if later_turn {
        return Err("turns completed after the last context record".into());
    }
    let records = t.by_seq();
    let mut messages = Vec::with_capacity(rec.entries.len());
    let mut first_instruction = None;
    for e in &rec.entries {
        if e.view == View::Synthetic {
            let role = e.role.as_deref().ok_or("synthetic entry without a role")?;
            let text = e.text.as_deref().unwrap_or_default();
            let mut m = match (role, &e.call_id) {
                ("tool", Some(id)) => Message::tool_result(id, text),
                ("user", _) => Message::user(text),
                ("assistant", _) => Message::assistant(text),
                _ => return Err(format!("unsupported synthetic entry ({role})")),
            };
            m.synthetic = true;
            messages.push(m);
            continue;
        }
        let s = e.seq.ok_or("entry without seq")?;
        let record = records
            .get(&s)
            .ok_or(format!("dangling reference to seq {s}"))?;
        let mut m = match record {
            Record::TurnStart(r) => {
                if first_instruction.is_none() && messages.is_empty() {
                    first_instruction = Some(r.instruction.clone());
                }
                Message::user(&r.instruction)
            }
            Record::ModelResponse(r) => assistant_message(r),
            Record::ToolResult(r) => {
                tool_message(r, e.cap_bytes.filter(|_| e.view == View::Capped))
            }
            Record::TurnEnd(r) => Message::assistant(r.answer.as_deref().unwrap_or_default()),
            Record::LoopGuard(_) => Message::user(NUDGE),
            _ => return Err(format!("seq {s} is not a message record")),
        };
        match e.view {
            View::Full | View::Capped => {}
            View::Masked => {
                m.content = e.text.clone();
                m.masked = true;
            }
            View::Noted => {
                let base = m.content.take().unwrap_or_default();
                m.content = Some(format!("{base}{}", e.text.as_deref().unwrap_or_default()));
            }
            View::Synthetic => unreachable!("handled above"),
        }
        m.origin = Some(s);
        messages.push(m);
    }
    let got = check(&messages);
    if got != rec.check {
        return Err(format!(
            "context check mismatch (messages {} vs {}, chars {} vs {})",
            got.messages, rec.check.messages, got.chars, rec.check.chars
        ));
    }
    Ok(Rebuilt {
        messages,
        first_instruction,
        masked_total: rec.masked_total as usize,
        evicted_total: rec.evicted_total as usize,
        method: "context",
        reason: None,
        from_seq: Some(seq),
    })
}

/// Rebuilds the conversation from the event records. Cancelled and
/// unfinished turns are dropped (as the live rollback did); gaps where
/// recording was off are noted for the model. Compaction is not replayed:
/// the budget pass decides afresh at the next request.
pub fn replay(t: &Transcript, default_cap: usize) -> Rebuilt {
    let mut messages: Vec<Message> = Vec::new();
    let mut turn_begin: Option<usize> = None; // index where the open turn starts
    let mut pending_nudge: Option<u64> = None;
    let mut first_instruction = None;
    let mut gap = false;
    let flush_nudge = |messages: &mut Vec<Message>, nudge: &mut Option<u64>| {
        if let Some(s) = nudge.take() {
            messages.push(Message::user(NUDGE).with_origin(Some(s)));
        }
    };
    for l in &t.lines {
        match &l.record {
            Record::TranscriptDisabled(_) => gap = true,
            Record::SessionStart(_) if gap => {
                messages.push(Message::user(
                    "[Note: part of this conversation was not recorded and is missing here.]",
                ));
                gap = false;
            }
            Record::TurnStart(r) => {
                flush_nudge(&mut messages, &mut pending_nudge);
                // A previous turn that never ended is dropped.
                if let Some(b) = turn_begin.take() {
                    messages.truncate(b);
                }
                turn_begin = Some(messages.len());
                if messages.is_empty() {
                    first_instruction = Some(r.instruction.clone());
                }
                messages.push(Message::user(&r.instruction).with_origin(Some(l.seq)));
            }
            Record::ModelResponse(r) if !r.tool_calls.is_empty() => {
                flush_nudge(&mut messages, &mut pending_nudge);
                messages.push(assistant_message(r).with_origin(Some(l.seq)));
            }
            Record::ToolResult(r) => {
                let cap = r.cap_bytes.or(Some(default_cap as u64));
                messages.push(tool_message(r, cap).with_origin(Some(l.seq)));
            }
            Record::LoopGuard(g) if g.action == "nudge" => pending_nudge = Some(l.seq),
            Record::TurnEnd(r) => {
                pending_nudge = None;
                let begin = turn_begin.take();
                if r.outcome == "cancelled" {
                    if let Some(b) = begin {
                        messages.truncate(b);
                    }
                    continue;
                }
                if let Some(answer) = r.answer.as_deref().filter(|a| !a.is_empty()) {
                    messages.push(Message::assistant(answer).with_origin(Some(l.seq)));
                }
            }
            _ => {}
        }
    }
    // An unfinished last turn (crash) is dropped.
    if let Some(b) = turn_begin {
        messages.truncate(b);
    }
    if gap {
        messages.push(Message::user(
            "[Note: the end of this conversation was not recorded.]",
        ));
    }
    if messages.is_empty() {
        first_instruction = None;
    }
    // Answer any tool calls left open (normalize would do it too).
    let mut open: Vec<String> = Vec::new();
    let mut fixed = Vec::with_capacity(messages.len());
    for m in messages {
        if m.role != "tool" && !open.is_empty() {
            for id in open.drain(..) {
                let mut s = Message::tool_result(&id, NOT_EXECUTED);
                s.synthetic = true;
                fixed.push(s);
            }
        }
        if let Some(calls) = m.tool_calls.as_ref().and_then(Value::as_array) {
            open = calls
                .iter()
                .filter_map(|c| c["id"].as_str().map(String::from))
                .collect();
        } else if m.role == "tool" {
            let id = m.tool_call_id.clone().unwrap_or_default();
            open.retain(|o| *o != id);
        }
        fixed.push(m);
    }
    for id in open {
        let mut s = Message::tool_result(&id, NOT_EXECUTED);
        s.synthetic = true;
        fixed.push(s);
    }
    Rebuilt {
        messages: fixed,
        first_instruction,
        masked_total: 0,
        evicted_total: 0,
        method: "replay",
        reason: None,
        from_seq: None,
    }
}

/// Files whose content differs from what the session last saw (by the
/// hashes recorded on read/edit/write results), resolved against `cwd`.
pub fn changed_files(t: &Transcript, cwd: &Path) -> Vec<FileChange> {
    let mut last: HashMap<&str, &str> = HashMap::new();
    let mut order: Vec<&str> = Vec::new();
    for l in &t.lines {
        if let Record::ToolResult(ToolResult { file: Some(f), .. }) = &l.record {
            if !last.contains_key(f.path.as_str()) {
                order.push(&f.path);
            }
            last.insert(&f.path, &f.fnv1a64);
        }
    }
    order
        .into_iter()
        .filter_map(|p| {
            let full = if Path::new(p).is_absolute() {
                PathBuf::from(p)
            } else {
                cwd.join(p)
            };
            let status = match std::fs::read(&full) {
                Ok(bytes) if hex(fnv1a64(&bytes)) == last[p] => return None,
                Ok(_) => "changed",
                Err(_) => "missing",
            };
            Some(FileChange {
                path: p.to_string(),
                status: status.into(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fnv_known_values() {
        assert_eq!(hex(fnv1a64(b"")), "cbf29ce484222325");
        assert_eq!(hex(fnv1a64(b"a")), "af63dc4c8601ec8c");
    }

    fn lines(records: &[Value]) -> Transcript {
        let lines = records
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let mut v = r.clone();
                v["seq"] = json!(i);
                v["ts"] = json!("2026-09-30T00:00:00.000Z");
                serde_json::from_value(v).unwrap()
            })
            .collect();
        Transcript {
            lines,
            bad_lines: 0,
        }
    }

    fn start() -> Value {
        json!({"type":"session_start","schema":1,"session":"s","session_started":"2026-09-30T00:00:00.000Z"})
    }

    #[test]
    fn tolerant_parsing_of_unknown_types_fields_and_values() {
        let l: Line =
            serde_json::from_str(r#"{"seq":1,"ts":"t","type":"hologram","anything":1}"#).unwrap();
        assert!(matches!(l.record, Record::Unknown));
        let l: Line = serde_json::from_str(
            r#"{"seq":2,"ts":"t","type":"turn_start","turn":1,"instruction":"hi","new_field":[1]}"#,
        )
        .unwrap();
        assert!(matches!(l.record, Record::TurnStart(ref t) if t.instruction == "hi"));
        // Unknown open-enum values are plain strings.
        let l: Line = serde_json::from_str(
            r#"{"seq":3,"ts":"t","type":"turn_end","turn":1,"outcome":"teleported","answer":null,"error":null}"#,
        )
        .unwrap();
        assert!(matches!(l.record, Record::TurnEnd(ref e) if e.outcome == "teleported"));
    }

    #[test]
    fn replay_rebuilds_turns_and_drops_cancelled_ones() {
        let t = lines(&[
            start(),
            json!({"type":"turn_start","turn":1,"instruction":"read it"}),
            json!({"type":"model_response","turn":1,"step":0,"duration_ms":1,"content":null,
                   "tool_calls":[{"id":"c1","name":"read_file","args":{"path":"a"}}]}),
            json!({"type":"tool_result","turn":1,"call_id":"c1","tool":"read_file","duration_ms":1,
                   "failed":false,"bytes":5,"sent_bytes":5,"output":"hello","cap_bytes":1000}),
            json!({"type":"model_response","turn":1,"step":1,"duration_ms":1,"content":"done","tool_calls":[]}),
            json!({"type":"turn_end","turn":1,"outcome":"answered","answer":"done","error":null}),
            json!({"type":"turn_start","turn":2,"instruction":"never mind"}),
            json!({"type":"model_response","turn":2,"step":0,"duration_ms":1,"content":null,
                   "tool_calls":[{"id":"c2","name":"list_dir","args":{}}]}),
            json!({"type":"turn_end","turn":2,"outcome":"cancelled","answer":null,"error":null}),
            json!({"type":"turn_start","turn":3,"instruction":"crashed turn"}),
        ]);
        let r = replay(&t, 100);
        let roles: Vec<&str> = r.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, ["user", "assistant", "tool", "assistant"]);
        assert_eq!(r.messages[2].content.as_deref(), Some("hello"));
        assert_eq!(r.messages[3].content.as_deref(), Some("done"));
        assert_eq!(r.first_instruction.as_deref(), Some("read it"));
    }

    #[test]
    fn replay_notes_gaps_and_closes_open_calls() {
        let t = lines(&[
            start(),
            json!({"type":"turn_start","turn":1,"instruction":"go"}),
            json!({"type":"model_response","turn":1,"step":0,"duration_ms":1,"content":null,
                   "tool_calls":[{"id":"a","name":"read_file","args":{}},{"id":"b","name":"read_file","args":{}}]}),
            json!({"type":"tool_result","turn":1,"call_id":"a","tool":"read_file","duration_ms":1,
                   "failed":false,"bytes":1,"sent_bytes":1,"output":"x"}),
            json!({"type":"turn_end","turn":1,"outcome":"loop_guard","answer":null,"error":null}),
            json!({"type":"transcript_disabled","turns":1}),
            start(),
        ]);
        let r = replay(&t, 100);
        let last = r.messages.last().unwrap();
        assert!(last.content.as_deref().unwrap().contains("not recorded"));
        assert!(
            r.messages
                .iter()
                .any(|m| m.synthetic && m.tool_call_id.as_deref() == Some("b"))
        );
    }

    #[test]
    fn context_record_falls_back_on_unknown_version_and_view() {
        let base = || {
            vec![
                start(),
                json!({"type":"turn_start","turn":1,"instruction":"hi"}),
                json!({"type":"turn_end","turn":1,"outcome":"answered","answer":"hello","error":null}),
            ]
        };
        let good_check = check(&[Message::user("hi"), Message::assistant("hello")]);
        let ctx = |schema: u32, view: &str| {
            json!({"type":"context","context_schema":schema,"turn":1,"cause":"turn_end","window":1,
                   "budget":1,"tokens":1,"count_source":"x","masked_total":0,"evicted_total":0,
                   "entries":[{"seq":1},{"seq":2,"view":view}],"check":good_check})
        };
        let mut ok = base();
        ok.push(ctx(1, "full"));
        let r = rebuild(&lines(&ok), 100);
        assert_eq!((r.method, r.messages.len()), ("context", 2));

        let mut newer = base();
        newer.push(ctx(999, "full"));
        let r = rebuild(&lines(&newer), 100);
        assert_eq!(r.method, "replay");
        assert!(r.reason.unwrap().contains("999"));

        // An unknown view does not parse: the record is unusable, so replay.
        let mut unknown_view = base();
        unknown_view.push(ctx(1, "summarized"));
        let t = lines(&unknown_view[..3]);
        let mut t2 = t;
        let bad: Result<Line, _> = serde_json::from_value({
            let mut v = ctx(1, "summarized");
            v["seq"] = json!(3);
            v["ts"] = json!("t");
            v
        });
        assert!(bad.is_err());
        t2.bad_lines += 1;
        assert_eq!(rebuild(&t2, 100).method, "replay");
    }

    #[test]
    fn changed_files_compares_recorded_hashes() {
        let d = std::env::temp_dir().join(format!("mima-tx-files-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("same.txt"), "a").unwrap();
        std::fs::write(d.join("edited.txt"), "new").unwrap();
        let fs = |p: &str, content: &[u8]| {
            json!({"type":"tool_result","turn":1,"call_id":p,"tool":"read_file","duration_ms":1,
                   "failed":false,"bytes":1,"sent_bytes":1,"output":"",
                   "file":{"path":p,"fnv1a64":hex(fnv1a64(content))}})
        };
        let t = lines(&[
            start(),
            fs("same.txt", b"a"),
            fs("edited.txt", b"old"),
            fs("gone.txt", b"x"),
        ]);
        let got = changed_files(&t, &d);
        assert_eq!(
            got,
            vec![
                FileChange {
                    path: "edited.txt".into(),
                    status: "changed".into()
                },
                FileChange {
                    path: "gone.txt".into(),
                    status: "missing".into()
                },
            ]
        );
    }
}
