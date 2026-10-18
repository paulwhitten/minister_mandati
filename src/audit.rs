//! Audit record (docs/design/design-audit-record.md): the inline hash chain
//! on transcript records, the session ledger, and `--audit-verify`.
//!
//! Every chained record carries `hash = SHA-256(prev || body)`, where `prev`
//! is the previous record's hash and `body` is the canonical encoding of the
//! record's disclosure form: the record without `hash`, `salt` and
//! `withheld`, with each content field replaced by a salted digest. So a copy
//! with content withheld verifies with the same hashes as the full record.
//!
//! When the previous line carries no hash (a transcript written before
//! chaining, or a partial line left by a crash), `prev` is the SHA-256 of all
//! bytes before the line. Writer and verifier apply the same rule.

use ring::digest::{Context as Digest, SHA256};
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::{Map, Value, json};
use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Version of the chain rules, recorded in `session_start` as `chain`.
pub const CHAIN: &str = "v1";

pub type Hash = [u8; 32];

/// Content fields of transcript records, as (record type, path). A `*`
/// segment matches every element of an array. Content is anything that can
/// hold source code, secrets or prose; everything else is metadata.
const CONTENT: &[(&str, &str)] = &[
    ("turn_start", "instruction"),
    ("model_response", "content"),
    ("model_response", "reasoning"),
    ("model_response", "tool_calls/*/args"),
    ("approval", "preview"),
    ("tool_result", "output"),
    ("tool_result", "output_full_sha256"),
    ("turn_end", "answer"),
    ("context", "entries/*/text"),
];

/// Every rule id mima can write. Listed in docs/sessions.md; a test keeps
/// the two in step.
pub const RULES: &[&str] = &[
    "approval.read_only",
    "approval.bash_not_required",
    "approval.writes_not_required",
    "approval.auto_approve_sandboxed",
    "approval.auto_approve_bash",
    "approval.approve_all",
    "approval.invalid_call",
    "loop_guard.skip_duplicate",
    "loop_guard.nudge",
    "loop_guard.terminate",
    "loop_guard.continue",
    "loop_guard.steps_left",
    "context.normalize",
    "context.mask",
    "context.evict",
    "session.step_cap_summary",
];

/// Fills in `actor` (and `rule`, where the record type implies one) when the
/// writer did not set them, so every record says who or what caused it:
/// `operator`, `model`, `rule` (mima's deterministic code), `tool`, `system`.
pub fn attribute(kind: &str, record: &mut Value) {
    let Some(obj) = record.as_object_mut() else {
        return;
    };
    let actor = match kind {
        "turn_start" | "transcript_disabled" => "operator",
        "model_response" => "model",
        "tool_result" => "tool",
        "approval" | "compaction" | "loop_guard" => "rule",
        _ => "system",
    };
    obj.entry("actor").or_insert_with(|| json!(actor));
    if obj.contains_key("rule") {
        return;
    }
    let text = |k: &str| obj.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let rule = match kind {
        "compaction" => match text("stage").as_str() {
            "normalize" => Some("context.normalize"),
            "mask" => Some("context.mask"),
            "evict" => Some("context.evict"),
            _ => None,
        },
        "loop_guard" => match text("action").as_str() {
            "continue" => Some("loop_guard.continue"),
            "nudge" => Some("loop_guard.nudge"),
            "terminate" => Some("loop_guard.terminate"),
            "steps" => Some("loop_guard.steps_left"),
            "final" => Some("session.step_cap_summary"),
            _ => None,
        },
        _ => None,
    };
    if let Some(r) = rule {
        obj.insert("rule".into(), json!(r));
    }
}

/// The audit header fields `session_start` carries (design step 3).
const HEADER: &[&str] = &[
    "build",
    "binary_sha256",
    "config",
    "system_prompt",
    "tools",
    "sampling",
    "server",
    "sandbox",
    "operator",
    "workspace",
    "os",
];

/// Bytes read from the end of a file to find its last line.
const TAIL_CHUNK: u64 = 64 * 1024;

// ---------------------------------------------------------------------------
// Hashing

pub fn sha256(parts: &[&[u8]]) -> Hash {
    let mut d = Digest::new(&SHA256);
    for p in parts {
        d.update(p);
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(d.finish().as_ref());
    out
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

fn parse_hash(v: &Value) -> Option<Hash> {
    let bytes = unhex(v.as_str()?)?;
    bytes.try_into().ok()
}

/// The `hash` a record claims, if it has a well-formed one.
pub fn claimed_hash(record: &Value) -> Option<Hash> {
    record.get("hash").and_then(parse_hash)
}

/// Canonical JSON: object keys sorted, no whitespace, minimal string
/// escaping, integers as written, floats in Rust's shortest round-trip form.
/// mima's own encoder, so a serializer update cannot change hashed bytes.
pub fn canonical(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::Number(n) => {
            let text = if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(u) = n.as_u64() {
                u.to_string()
            } else {
                format!("{:?}", n.as_f64().unwrap_or(0.0))
            };
            out.extend_from_slice(text.as_bytes());
        }
        Value::String(s) => canonical_str(s, out),
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                canonical(item, out);
            }
            out.push(b']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push(b'{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                canonical_str(k, out);
                out.push(b':');
                canonical(&map[*k], out);
            }
            out.push(b'}');
        }
    }
}

fn canonical_str(s: &str, out: &mut Vec<u8>) {
    out.push(b'"');
    for c in s.chars() {
        match c {
            '"' => out.extend_from_slice(b"\\\""),
            '\\' => out.extend_from_slice(b"\\\\"),
            '\n' => out.extend_from_slice(b"\\n"),
            '\r' => out.extend_from_slice(b"\\r"),
            '\t' => out.extend_from_slice(b"\\t"),
            '\u{8}' => out.extend_from_slice(b"\\b"),
            '\u{c}' => out.extend_from_slice(b"\\f"),
            c if (c as u32) < 0x20 => {
                out.extend_from_slice(format!("\\u{:04x}", c as u32).as_bytes())
            }
            c => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    out.push(b'"');
}

/// A content field's own salt: SHA-256 of the record's salt and the field's
/// path. A shared copy can carry the salts of the fields it discloses
/// without revealing those of the fields it withholds.
fn field_salt(record_salt: &[u8], path: &str) -> Hash {
    sha256(&[record_salt, path.as_bytes()])
}

/// Salted digest standing in for a content value: `c:` and the hex SHA-256
/// of the field's salt, a zero byte and the value's canonical encoding.
fn commit(field_salt: &[u8], value: &Value) -> String {
    let mut enc = Vec::new();
    canonical(value, &mut enc);
    format!("c:{}", hex(&sha256(&[field_salt, &[0], &enc])))
}

/// Concrete content fields of a record as (name, path): the name is
/// `type.pattern`, as `--withhold` takes it; the path is the JSON path.
fn content_paths(record: &Value) -> Vec<(String, String)> {
    let kind = record.get("type").and_then(Value::as_str).unwrap_or("");
    let mut out = Vec::new();
    for (t, pattern) in CONTENT.iter().filter(|(t, _)| *t == kind) {
        let segs: Vec<&str> = pattern.split('/').collect();
        visit(&mut record.clone(), &segs, "", &mut |path, _| {
            out.push((format!("{t}.{pattern}"), path.to_string()));
        });
    }
    out
}

/// Calls `f` with the concrete path of every value matching `segs`.
fn visit(value: &mut Value, segs: &[&str], prefix: &str, f: &mut dyn FnMut(&str, &mut Value)) {
    let Some((first, rest)) = segs.split_first() else {
        f(prefix, value);
        return;
    };
    let join = |p: &str, s: &str| {
        if p.is_empty() {
            s.to_string()
        } else {
            format!("{p}/{s}")
        }
    };
    if *first == "*" {
        if let Value::Array(items) = value {
            for (i, item) in items.iter_mut().enumerate() {
                visit(item, rest, &join(prefix, &i.to_string()), f);
            }
        }
    } else if let Some(child) = value.get_mut(*first) {
        visit(child, rest, &join(prefix, first), f);
    }
}

/// The record as hashed: without `hash`, `salt`, `salts` and `withheld`;
/// with content fields (when `content` is true) replaced by their salted
/// digests, except fields listed in `withheld`, which already hold theirs.
/// A field's salt comes from the record's `salt` (a full transcript) or from
/// `salts` (a shared copy, which carries only the disclosed fields' salts).
fn disclosure_form(record: &Value, content: bool) -> Result<Value, String> {
    let mut v = record.clone();
    let obj = v.as_object_mut().ok_or("record is not a JSON object")?;
    obj.remove("hash");
    let salt = obj.remove("salt");
    let salts = obj.remove("salts");
    let withheld: Vec<String> = obj
        .remove("withheld")
        .and_then(|w| serde_json::from_value(w).ok())
        .unwrap_or_default();
    if !content {
        return Ok(v);
    }
    let salt = salt.as_ref().and_then(Value::as_str).and_then(unhex);
    let salt_for = |path: &str| -> Option<Vec<u8>> {
        match &salt {
            Some(s) => Some(field_salt(s, path).to_vec()),
            None => salts.as_ref()?.get(path)?.as_str().and_then(unhex),
        }
    };
    let mut error = None;
    for (_, path) in content_paths(&v) {
        if withheld.contains(&path) {
            continue;
        }
        let ptr = format!("/{path}");
        match (salt_for(&path), v.pointer_mut(&ptr)) {
            (Some(fs), Some(value)) => *value = Value::String(commit(&fs, value)),
            _ => error = Some(format!("content field {path} but no salt")),
        }
    }
    match error {
        Some(e) => Err(e),
        None => Ok(v),
    }
}

/// A shared copy of a chained transcript with the content fields named in
/// `withhold` (a set name: `code` = all content, `outputs` = tool outputs
/// and previews; or field names such as `turn_start.instruction`) replaced
/// by their digests. Disclosed content keeps its value and its field salt;
/// record salts are dropped. The copy verifies with the same hashes.
pub fn export(transcript: &[u8], withhold: &[String]) -> Result<String, String> {
    let wanted = |name: &str| {
        withhold.iter().any(|w| match w.as_str() {
            "code" => true,
            "outputs" => matches!(name, "tool_result.output" | "approval.preview"),
            other => other == name,
        })
    };
    for w in withhold {
        let known = matches!(w.as_str(), "code" | "outputs")
            || CONTENT.iter().any(|(t, p)| format!("{t}.{p}") == *w);
        if !known {
            return Err(format!(
                "unknown field to withhold: {w} (use code, outputs, or one of: {})",
                CONTENT
                    .iter()
                    .map(|(t, p)| format!("{t}.{p}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    let mut out = String::new();
    for (n, line) in transcript.split(|&b| b == b'\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        let mut r: Value =
            serde_json::from_slice(line).map_err(|e| format!("line {}: not JSON: {e}", n + 1))?;
        let Some(salt) = r.get("salt").and_then(Value::as_str).and_then(unhex) else {
            if r.get("hash").is_some() {
                return Err(format!("line {}: chained record without a salt", n + 1));
            }
            return Err(format!(
                "line {}: written before audit chaining; its content cannot be withheld",
                n + 1
            ));
        };
        let mut withheld = Vec::new();
        let mut salts = Map::new();
        for (name, path) in content_paths(&r) {
            let fs = field_salt(&salt, &path);
            let ptr = format!("/{path}");
            let Some(value) = r.pointer_mut(&ptr) else {
                continue;
            };
            if wanted(&name) {
                *value = Value::String(commit(&fs, value));
                withheld.push(path);
            } else {
                salts.insert(path, Value::String(hex(&fs)));
            }
        }
        let obj = r.as_object_mut().ok_or("record is not a JSON object")?;
        obj.remove("salt");
        if !withheld.is_empty() {
            obj.insert("withheld".into(), json!(withheld));
        }
        if !salts.is_empty() {
            obj.insert("salts".into(), Value::Object(salts));
        }
        out.push_str(&r.to_string());
        out.push('\n');
    }
    Ok(out)
}

/// `SHA-256(prev || canonical(disclosure form))`.
pub fn record_hash(prev: &Hash, record: &Value, content: bool) -> Result<Hash, String> {
    let mut body = Vec::new();
    canonical(&disclosure_form(record, content)?, &mut body);
    Ok(sha256(&[prev, &body]))
}

/// Adds `salt` (16 random bytes) and `hash` to a transcript record and
/// returns the hash.
pub fn seal(record: &mut Value, prev: &Hash) -> Result<Hash, String> {
    let mut salt = [0u8; 16];
    SystemRandom::new()
        .fill(&mut salt)
        .map_err(|_| "no system randomness")?;
    let obj = record
        .as_object_mut()
        .ok_or("record is not a JSON object")?;
    obj.insert("salt".into(), Value::String(hex(&salt)));
    let h = record_hash(prev, record, true)?;
    record
        .as_object_mut()
        .map(|o| o.insert("hash".into(), Value::String(hex(&h))));
    Ok(h)
}

/// Where a chain continues in an existing file: the last line's `hash`, or
/// the SHA-256 of all bytes when the last line has none. The flag is true
/// when the file ends in a partial line; the writer then appends a newline
/// first, and the returned hash already covers it.
pub fn chain_tail(file: &mut File) -> std::io::Result<(Hash, bool)> {
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok((sha256(&[]), false));
    }
    let start = len.saturating_sub(TAIL_CHUNK);
    file.seek(SeekFrom::Start(start))?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail)?;
    // The last line's hash, if `buf` holds the whole line (it does when a
    // newline precedes it, or when `buf` starts at the file's beginning).
    let last_hash = |buf: &[u8], from_start: bool| -> Option<Hash> {
        let body = buf.strip_suffix(b"\n")?;
        let line_start = body.iter().rposition(|&b| b == b'\n').map(|i| i + 1);
        if line_start.is_none() && !from_start {
            return None;
        }
        let line = &body[line_start.unwrap_or(0)..];
        serde_json::from_slice::<Value>(line)
            .ok()
            .as_ref()
            .and_then(claimed_hash)
    };
    if let Some(h) = last_hash(&tail, start == 0) {
        return Ok((h, false));
    }
    // A last line longer than the chunk, or one without a usable hash.
    file.seek(SeekFrom::Start(0))?;
    let mut all = Vec::new();
    file.read_to_end(&mut all)?;
    if start > 0
        && let Some(h) = last_hash(&all, true)
    {
        return Ok((h, false));
    }
    // No usable hash on the last line: chain to everything before.
    let partial = all.last() != Some(&b'\n');
    if partial {
        all.push(b'\n');
    }
    Ok((sha256(&[&all]), partial))
}

// ---------------------------------------------------------------------------
// Session ledger

/// The session ledger: one chained line per session start, recording change
/// and session end, written even when transcripts are off. No content.
#[derive(Clone, Debug)]
pub struct Ledger {
    path: PathBuf,
    record_host: bool,
    failed: bool,
}

impl Ledger {
    pub fn new(path: PathBuf, record_host: bool) -> Self {
        Self {
            path,
            record_host,
            failed: false,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Who and where: user name, uid, and (unless turned off) host.
    pub fn identity(&self) -> Value {
        let user = std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .ok();
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        let mut v = json!({ "user": user, "uid": uid });
        if self.record_host {
            v["host"] = json!(hostname());
        }
        v
    }

    /// Appends one event under an exclusive lock, so concurrent mima
    /// processes extend the chain in turn. A failure is reported once and
    /// never stops the agent.
    /// Returns whether the entry was written.
    pub fn append(&mut self, kind: &str, fields: Value) -> bool {
        match self.try_append(kind, fields) {
            Ok(()) => true,
            Err(e) => {
                if !self.failed {
                    self.failed = true;
                    tracing::warn!(error = %e, path = %self.path.display(), "audit ledger write failed");
                    eprintln!(
                        "warning: cannot write the audit ledger {}: {e}",
                        self.path.display()
                    );
                }
                false
            }
        }
    }

    fn try_append(&self, kind: &str, fields: Value) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
        }
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .mode(0o600)
            .open(&self.path)?;
        file.lock()?;
        let (prev, partial) = chain_tail(&mut file)?;
        let mut record =
            json!({ "type": kind, "ts": crate::session::rfc3339_utc(SystemTime::now()) });
        if let (Some(r), Value::Object(f)) = (record.as_object_mut(), fields) {
            let f: Map<String, Value> = f;
            r.extend(f);
        }
        let h = record_hash(&prev, &record, false).map_err(std::io::Error::other)?;
        record["hash"] = Value::String(hex(&h));
        let mut line = if partial {
            "\n".to_string()
        } else {
            String::new()
        };
        line.push_str(&record.to_string());
        line.push('\n');
        file.write_all(line.as_bytes())?;
        file.sync_data()
    }
}

pub fn hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is valid for its length; gethostname writes at most
    // that many bytes.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    Some(String::from_utf8_lossy(&buf[..end]).into_owned())
}

/// Absolute form of a path for the ledger (canonical when it exists).
pub fn absolute(path: &Path) -> String {
    std::fs::canonicalize(path)
        .or_else(|_| std::env::current_dir().map(|d| d.join(path)))
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

// ---------------------------------------------------------------------------
// Verification

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Pass,
    Warn,
    Fail,
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub level: Level,
    pub check: &'static str,
    pub detail: String,
}

fn finding(level: Level, check: &'static str, detail: impl Into<String>) -> Finding {
    Finding {
        level,
        check,
        detail: detail.into(),
    }
}

/// One line of a chained file, as read.
struct Line {
    record: Option<Value>,
    hash: Option<Hash>,
}

/// The result of walking a chained file.
struct Chain {
    lines: Vec<Line>,
    partial_tail: bool,
    bad_json: Vec<usize>,
    broken: Vec<usize>,
    unchained_after_start: Vec<usize>,
    legacy: usize,
    chained: usize,
}

/// Walks every line, recomputing each claimed hash from its predecessor.
/// Line numbers in the result are 1-based.
fn walk(bytes: &[u8], content: bool) -> Chain {
    let mut c = Chain {
        lines: Vec::new(),
        partial_tail: false,
        bad_json: Vec::new(),
        broken: Vec::new(),
        unchained_after_start: Vec::new(),
        legacy: 0,
        chained: 0,
    };
    let mut offset = 0;
    let mut prev_hash: Option<Hash> = None;
    while offset < bytes.len() {
        let end = bytes[offset..].iter().position(|&b| b == b'\n');
        let (text, next) = match end {
            Some(e) => (&bytes[offset..offset + e], offset + e + 1),
            None => {
                c.partial_tail = true;
                (&bytes[offset..], bytes.len())
            }
        };
        let n = c.lines.len() + 1;
        let prev = prev_hash.unwrap_or_else(|| sha256(&[&bytes[..offset]]));
        let record = serde_json::from_slice::<Value>(text).ok();
        let claimed = record.as_ref().and_then(claimed_hash);
        match (&record, claimed) {
            (None, _) => c.bad_json.push(n),
            (Some(r), Some(h)) => {
                c.chained += 1;
                if record_hash(&prev, r, content).ok() != Some(h) {
                    c.broken.push(n);
                }
            }
            (Some(_), None) if c.chained > 0 => c.unchained_after_start.push(n),
            (Some(_), None) => c.legacy += 1,
        }
        prev_hash = claimed;
        c.lines.push(Line {
            record,
            hash: claimed,
        });
        offset = next;
    }
    c
}

fn list(lines: &[usize]) -> String {
    let shown: Vec<String> = lines.iter().take(10).map(|n| n.to_string()).collect();
    let more = if lines.len() > 10 { ", ..." } else { "" };
    format!("{}{more}", shown.join(", "))
}

/// Checks a transcript's chain, record order and completeness, and its
/// heads against the session ledger. See the design's checks V1-V3, V8, V9.
pub fn verify(transcript: &Path, ledger: Option<&Path>) -> Vec<Finding> {
    verify_with_head(transcript, ledger, None)
}

/// `verify`, plus a comparison of the transcript's head with `head` (an
/// anchor kept elsewhere) when given.
pub fn verify_with_head(
    transcript: &Path,
    ledger: Option<&Path>,
    head: Option<&str>,
) -> Vec<Finding> {
    let mut out = Vec::new();
    let bytes = match std::fs::read(transcript) {
        Ok(b) => b,
        Err(e) => {
            out.push(finding(
                Level::Fail,
                "read",
                format!("{}: {e}", transcript.display()),
            ));
            return out;
        }
    };
    let c = walk(&bytes, true);

    // V1: well-formed lines.
    if c.bad_json.is_empty() && !c.partial_tail {
        out.push(finding(
            Level::Pass,
            "lines",
            format!("{} records, all complete JSON", c.lines.len()),
        ));
    } else {
        let mut why = Vec::new();
        if !c.bad_json.is_empty() {
            why.push(format!("not JSON: line {}", list(&c.bad_json)));
        }
        if c.partial_tail {
            why.push("the last line is incomplete (crash or full disk?)".to_string());
        }
        out.push(finding(Level::Fail, "lines", why.join("; ")));
    }

    // V2: the chain.
    if c.chained == 0 {
        out.push(finding(
            Level::Warn,
            "chain",
            "no chained records (written before audit chaining)",
        ));
    } else if c.broken.is_empty() && c.unchained_after_start.is_empty() {
        out.push(finding(
            Level::Pass,
            "chain",
            format!("{} records chained, every hash recomputes", c.chained),
        ));
    } else {
        let mut why = Vec::new();
        if !c.broken.is_empty() {
            why.push(format!(
                "hash does not match at line {} (changed, inserted, removed or reordered at or before it)",
                list(&c.broken)
            ));
        }
        if !c.unchained_after_start.is_empty() {
            why.push(format!(
                "records without a hash after chaining began: line {}",
                list(&c.unchained_after_start)
            ));
        }
        out.push(finding(Level::Fail, "chain", why.join("; ")));
    }
    if c.legacy > 0 && c.chained > 0 {
        out.push(finding(
            Level::Warn,
            "chain",
            format!(
                "the first {} records were written before chaining; the chain covers their bytes",
                c.legacy
            ),
        ));
    }

    // V3: record order.
    let mut gaps = Vec::new();
    let mut last_seq: Option<u64> = None;
    for (i, line) in c.lines.iter().enumerate() {
        let Some(seq) = line
            .record
            .as_ref()
            .and_then(|r| r.get("seq"))
            .and_then(Value::as_u64)
        else {
            continue;
        };
        if let Some(prev) = last_seq
            && seq != prev + 1
        {
            gaps.push(i + 1);
        }
        last_seq = Some(seq);
    }
    if gaps.is_empty() {
        out.push(finding(Level::Pass, "order", "seq increases by one"));
    } else {
        out.push(finding(
            Level::Fail,
            "order",
            format!(
                "seq does not follow its predecessor at line {}",
                list(&gaps)
            ),
        ));
    }

    // V4: the audit header.
    let starts: Vec<&Value> = c
        .lines
        .iter()
        .filter(|l| l.hash.is_some())
        .filter_map(|l| l.record.as_ref())
        .filter(|r| r.get("type").and_then(Value::as_str) == Some("session_start"))
        .collect();
    let incomplete = starts
        .iter()
        .filter(|r| HEADER.iter().any(|k| r.get(*k).is_none()))
        .count();
    if !starts.is_empty() {
        if incomplete == 0 {
            out.push(finding(Level::Pass, "header", "build, config, prompt, tools, sampling, server, sandbox, operator and workspace recorded"));
        } else {
            out.push(finding(
                Level::Warn,
                "header",
                format!("{incomplete} session_start record(s) without the full audit header (written by an older mima)"),
            ));
        }
    }

    // V7: file continuity.
    out.push(check_files(&c));

    // V5: every tool result follows its approval; denied calls did not run.
    out.push(check_approvals(&c));

    // V6: attribution and known rules.
    let mut unknown: Vec<String> = Vec::new();
    let mut unattributed = 0;
    for r in c
        .lines
        .iter()
        .filter(|l| l.hash.is_some())
        .filter_map(|l| l.record.as_ref())
    {
        if r.get("actor").is_none() {
            unattributed += 1;
        }
        if let Some(rule) = r.get("rule").and_then(Value::as_str)
            && !RULES.contains(&rule)
            && !unknown.iter().any(|u| u == rule)
        {
            unknown.push(rule.to_string());
        }
    }
    if unattributed > 0 {
        out.push(finding(
            Level::Warn,
            "attribute",
            format!(
                "{unattributed} chained record(s) without an actor (written before attribution)"
            ),
        ));
    } else if unknown.is_empty() {
        out.push(finding(
            Level::Pass,
            "attribute",
            "every record names its actor; rules known",
        ));
    }
    if !unknown.is_empty() {
        out.push(finding(
            Level::Warn,
            "attribute",
            format!(
                "rule id(s) unknown to this mima (written by a newer one?): {}",
                unknown.join(", ")
            ),
        ));
    }

    // V10: unrecorded intervals (recording turned off, then on again).
    out.push(check_gaps(&c));

    // V8: completeness.
    let last_type = c
        .lines
        .iter()
        .rev()
        .find_map(|l| l.record.as_ref())
        .and_then(|r| r.get("type"))
        .and_then(Value::as_str);
    if last_type == Some("session_end") {
        out.push(finding(Level::Pass, "complete", "ends with session_end"));
    } else {
        out.push(finding(
            Level::Warn,
            "complete",
            "no session_end at the end (crash, interruption, still running, or truncated)",
        ));
    }

    // V9: heads anchored in the ledger, and against a head given by hand.
    match ledger {
        None => out.push(finding(Level::Warn, "ledger", "no ledger configured")),
        Some(lp) => out.extend(check_ledger(transcript, &c, lp)),
    }
    if let Some(given) = head {
        let last = c.lines.iter().rev().find_map(|l| l.hash).map(|h| hex(&h));
        let given = given
            .trim()
            .trim_start_matches("sha256:")
            .to_ascii_lowercase();
        if last.as_deref() == Some(given.as_str()) {
            out.push(finding(
                Level::Pass,
                "head",
                "the transcript's head matches the given head",
            ));
        } else if c
            .lines
            .iter()
            .any(|l| l.hash.map(|h| hex(&h)) == Some(given.clone()))
        {
            out.push(finding(
                Level::Warn,
                "head",
                "the given head is an earlier record; records follow it",
            ));
        } else {
            out.push(finding(
                Level::Fail,
                "head",
                "the given head is not in the transcript",
            ));
        }
    }
    out
}

/// V10: each `transcript_disabled` starts an interval the record does not
/// cover; the next `session_start` (with `gap_after_seq`) ends it.
fn check_gaps(c: &Chain) -> Finding {
    let records: Vec<&Value> = c.lines.iter().filter_map(|l| l.record.as_ref()).collect();
    let text = |r: &Value, k: &str| r.get(k).and_then(Value::as_str).unwrap_or("?").to_string();
    let mut gaps = Vec::new();
    for (i, r) in records.iter().enumerate() {
        if r.get("type").and_then(Value::as_str) != Some("transcript_disabled") {
            continue;
        }
        let from_turn = r.get("turns").and_then(Value::as_u64).unwrap_or(0);
        let resumed = records[i + 1..]
            .iter()
            .find(|n| n.get("type").and_then(Value::as_str) == Some("session_start"));
        gaps.push(match resumed {
            Some(n) => {
                let to_turn = n
                    .get("turns_before")
                    .and_then(Value::as_u64)
                    .unwrap_or(from_turn);
                let turns = if to_turn > from_turn {
                    format!("turns {}-{} unrecorded", from_turn + 1, to_turn)
                } else {
                    "no turns in between".to_string()
                };
                format!("{} to {} ({turns})", text(r, "ts"), text(n, "ts"))
            }
            None => format!("from {} to the end of the session", text(r, "ts")),
        });
    }
    if gaps.is_empty() {
        finding(Level::Pass, "gaps", "recording was never turned off")
    } else {
        finding(
            Level::Warn,
            "gaps",
            format!("recording was off: {}", gaps.join("; ")),
        )
    }
}

/// V7: a file's state before a change should be the state the record last
/// saw. A difference means it changed outside the record: between turns
/// (usually the operator) or during a turn (something mima did not record).
fn check_files(c: &Chain) -> Finding {
    use std::collections::HashMap;
    // path -> (last known hash, turn it was seen in)
    let mut known: HashMap<String, (Option<String>, u64)> = HashMap::new();
    let mut between = Vec::new();
    let mut within = Vec::new();
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    for (n, r) in c
        .lines
        .iter()
        .enumerate()
        .filter_map(|(i, l)| Some((i + 1, l.record.as_ref()?)))
    {
        if r.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        let turn = r.get("turn").and_then(Value::as_u64).unwrap_or(0);
        let mut observe = |path: String, before: Option<Option<String>>, after: Option<String>| {
            if let (Some(before), Some((last, seen))) = (&before, known.get(&path))
                && before != last
            {
                if *seen == turn {
                    within.push(format!("{path} (line {n})"));
                } else {
                    between.push(format!("{path} (line {n})"));
                }
            }
            known.insert(path, (after, turn));
        };
        if let Some(f) = r.get("file")
            && let Some(path) = s(f, "path")
        {
            let before = f
                .get("before_sha256")
                .map(|v| v.as_str().map(str::to_string));
            observe(path, before, s(f, "after_sha256"));
        }
        for p in r
            .pointer("/workspace_changes/paths")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(path) = s(p, "path") {
                // Null here means "as committed", which the record may not
                // have seen: compare only known hashes.
                let before = s(p, "before_sha256").map(Some);
                observe(path, before, s(p, "after_sha256"));
            }
        }
    }
    if within.is_empty() && between.is_empty() {
        return finding(
            Level::Pass,
            "files",
            "every file change starts from the state last recorded",
        );
    }
    let mut why = Vec::new();
    if !within.is_empty() {
        why.push(format!(
            "changed during a turn outside the record: {}",
            within.join(", ")
        ));
    }
    if !between.is_empty() {
        why.push(format!(
            "changed between turns (by the operator?): {}",
            between.join(", ")
        ));
    }
    finding(Level::Warn, "files", why.join("; "))
}

/// V5: a `tool_result` must follow an `approval` for its call id; a denied
/// call must not have run; an allowed call should have a result unless its
/// turn was cancelled or the transcript ends early.
fn check_approvals(c: &Chain) -> Finding {
    use std::collections::HashMap;
    let mut pending: HashMap<String, (usize, String)> = HashMap::new();
    let mut orphans = Vec::new();
    let mut ran_when_denied = Vec::new();
    let mut unanswered = Vec::new();
    let text = |r: &Value, k: &str| r.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    for (i, r) in c
        .lines
        .iter()
        .enumerate()
        .filter_map(|(i, l)| Some((i + 1, l.record.as_ref()?)))
    {
        match text(r, "type").as_str() {
            "approval" => {
                pending.insert(text(r, "call_id"), (i, text(r, "decision")));
            }
            "tool_result" => match pending.remove(&text(r, "call_id")) {
                None => orphans.push(i),
                Some((_, d)) if d == "denied" && r.get("executed") != Some(&Value::Bool(false)) => {
                    ran_when_denied.push(i)
                }
                Some(_) => {}
            },
            "turn_end" => {
                let cancelled = text(r, "outcome") == "cancelled";
                for (_, (n, _)) in pending.drain() {
                    if !cancelled {
                        unanswered.push(n);
                    }
                }
            }
            _ => {}
        }
    }
    unanswered.sort_unstable();
    if !orphans.is_empty() || !ran_when_denied.is_empty() {
        let mut why = Vec::new();
        if !orphans.is_empty() {
            why.push(format!(
                "tool result without an approval: line {}",
                list(&orphans)
            ));
        }
        if !ran_when_denied.is_empty() {
            why.push(format!(
                "denied call not marked as not executed: line {}",
                list(&ran_when_denied)
            ));
        }
        finding(Level::Fail, "approvals", why.join("; "))
    } else if !unanswered.is_empty() {
        finding(
            Level::Warn,
            "approvals",
            format!(
                "approved call(s) without a result in a completed turn: line {}",
                list(&unanswered)
            ),
        )
    } else {
        finding(
            Level::Pass,
            "approvals",
            "every tool result follows its approval",
        )
    }
}

fn check_ledger(transcript: &Path, c: &Chain, ledger: &Path) -> Vec<Finding> {
    let mut out = Vec::new();
    let bytes = match std::fs::read(ledger) {
        Ok(b) => b,
        Err(e) => {
            out.push(finding(
                Level::Warn,
                "ledger",
                format!("cannot read {}: {e}", ledger.display()),
            ));
            return out;
        }
    };
    let l = walk(&bytes, false);
    if l.broken.is_empty() && l.unchained_after_start.is_empty() && l.bad_json.is_empty() {
        out.push(finding(
            Level::Pass,
            "ledger",
            format!("{} ledger entries chained", l.chained),
        ));
    } else {
        out.push(finding(
            Level::Fail,
            "ledger",
            format!(
                "the ledger's own chain is broken (lines {}{}{})",
                list(&l.broken),
                if l.unchained_after_start.is_empty() {
                    ""
                } else {
                    "; unchained "
                },
                list(&l.unchained_after_start)
            ),
        ));
    }

    // Ledger entries for this transcript: by path, or by session id (a copy
    // exported elsewhere keeps its session ids).
    let me = absolute(transcript);
    let sessions: Vec<&str> = c
        .lines
        .iter()
        .filter_map(|line| line.record.as_ref())
        .filter(|r| r.get("type").and_then(Value::as_str) == Some("session_start"))
        .filter_map(|r| r.get("session").and_then(Value::as_str))
        .collect();
    let ends: Vec<&Value> = l
        .lines
        .iter()
        .filter_map(|line| line.record.as_ref())
        .filter(|r| {
            let text = |k: &str| r.get(k).and_then(Value::as_str);
            text("type") == Some("session_end")
                && (text("transcript") == Some(me.as_str())
                    || text("session").is_some_and(|s| sessions.contains(&s)))
                && r.get("head").is_some_and(|h| !h.is_null())
        })
        .collect();
    if ends.is_empty() {
        out.push(finding(
            Level::Warn,
            "anchor",
            "no head for this transcript in the ledger",
        ));
        return out;
    }
    let position = |h: &Hash| {
        c.lines
            .iter()
            .position(|line| line.hash.as_ref() == Some(h))
    };
    let mut last_anchor = None;
    let mut missing = Vec::new();
    for end in &ends {
        let head = end.get("head").and_then(parse_hash);
        match head.as_ref().and_then(position) {
            Some(i) => last_anchor = Some(last_anchor.map_or(i, |a: usize| a.max(i))),
            None => missing.push(
                end.get("ts")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_string(),
            ),
        }
    }
    if !missing.is_empty() {
        out.push(finding(
            Level::Fail,
            "anchor",
            format!(
                "{} recorded head(s) not found in the transcript (records changed or removed); session end at {}",
                missing.len(),
                missing.join(", ")
            ),
        ));
    }
    match last_anchor {
        Some(i) if i + 1 == c.lines.len() => out.push(finding(
            Level::Pass,
            "anchor",
            format!(
                "transcript head matches the ledger ({} session end(s))",
                ends.len()
            ),
        )),
        Some(i) => out.push(finding(
            Level::Warn,
            "anchor",
            format!(
                "{} record(s) after the last head recorded in the ledger",
                c.lines.len() - i - 1
            ),
        )),
        None => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mima-audit-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Writes `records` as a chained transcript, the way Session does.
    fn write_chain(path: &Path, records: &[Value]) {
        let mut prev = sha256(&[]);
        let mut text = String::new();
        for r in records {
            let mut r = r.clone();
            prev = seal(&mut r, &prev).unwrap();
            text.push_str(&r.to_string());
            text.push('\n');
        }
        std::fs::write(path, text).unwrap();
    }

    fn sample() -> Vec<Value> {
        vec![
            json!({"seq":0,"ts":"t0","type":"session_start","schema":1,"session":"s","session_started":"t0","chain":CHAIN}),
            json!({"seq":1,"ts":"t1","type":"turn_start","turn":1,"instruction":"fix the bug"}),
            json!({"seq":2,"ts":"t2","type":"model_response","turn":1,"step":0,"content":null,
                   "tool_calls":[{"id":"c1","name":"read_file","args":{"path":"src/x.rs"}}],"temperature":0.6}),
            json!({"seq":3,"ts":"t3","type":"approval","turn":1,"call_id":"c1","decision":"not_required","actor":"rule","rule":"approval.read_only","preview":null}),
            json!({"seq":4,"ts":"t4","type":"tool_result","turn":1,"call_id":"c1","tool":"read_file","output":"fn x() {}\n\"quoted\" \u{1}"}),
            json!({"seq":5,"ts":"t5","type":"session_end","reason":"exit","turns":1}),
        ]
        .into_iter()
        .map(|mut r| {
            let kind = r["type"].as_str().unwrap().to_string();
            attribute(&kind, &mut r);
            r
        })
        .collect()
    }

    fn level(f: &[Finding], check: &str) -> Level {
        f.iter()
            .filter(|x| x.check == check)
            .map(|x| x.level)
            .max_by_key(|l| *l as u8)
            .unwrap()
    }

    #[test]
    fn canonical_encoding_is_stable() {
        let v: Value = serde_json::from_str(
            r#"{"b":[1,-2,0.95,1.0,1e300,true,null],"a":"q\"\\\n\t\u0001é","c":{}}"#,
        )
        .unwrap();
        let mut out = Vec::new();
        canonical(&v, &mut out);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "{\"a\":\"q\\\"\\\\\\n\\t\\u0001é\",\"b\":[1,-2,0.95,1.0,1e300,true,null],\"c\":{}}"
        );
    }

    #[test]
    fn floats_survive_a_write_and_parse() {
        // Hashes are recomputed from parsed lines; every float must read back
        // exactly as written (serde_json float_roundtrip).
        for f in [
            0.1,
            0.6,
            0.95,
            1.0 / 3.0,
            2.2250738585072014e-308,
            123456.789e10,
        ] {
            let line = json!({ "x": f }).to_string();
            let back: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(back["x"].as_f64(), Some(f), "{line}");
        }
    }

    #[test]
    fn a_clean_chain_verifies() {
        let d = temp("clean");
        let p = d.join("t.jsonl");
        write_chain(&p, &sample());
        let f = verify(&p, None);
        assert_eq!(level(&f, "lines"), Level::Pass);
        assert_eq!(level(&f, "chain"), Level::Pass);
        assert_eq!(level(&f, "order"), Level::Pass);
        assert_eq!(level(&f, "complete"), Level::Pass);
        assert_eq!(level(&f, "approvals"), Level::Pass);
        assert_eq!(level(&f, "attribute"), Level::Pass);
    }

    #[test]
    fn records_get_an_actor_and_implied_rules() {
        let mut r = json!({ "stage": "mask" });
        attribute("compaction", &mut r);
        assert_eq!(
            (r["actor"].as_str(), r["rule"].as_str()),
            (Some("rule"), Some("context.mask"))
        );
        let mut r = json!({ "action": "final" });
        attribute("loop_guard", &mut r);
        assert_eq!(r["rule"], "session.step_cap_summary");
        let mut r = json!({ "actor": "operator", "decision": "approved" });
        attribute("approval", &mut r);
        assert_eq!(r["actor"], "operator");
        assert!(r.get("rule").is_none());
        for (kind, actor) in [
            ("turn_start", "operator"),
            ("model_response", "model"),
            ("tool_result", "tool"),
            ("session_start", "system"),
            ("transcript_disabled", "operator"),
        ] {
            let mut r = json!({});
            attribute(kind, &mut r);
            assert_eq!(r["actor"], actor, "{kind}");
        }
    }

    #[test]
    fn every_rule_id_is_documented() {
        let doc = include_str!("../docs/sessions.md");
        for rule in RULES {
            assert!(
                doc.contains(&format!("`{rule}`")),
                "{rule} missing from docs/sessions.md"
            );
        }
    }

    #[test]
    fn tool_results_must_follow_their_approval() {
        let d = temp("approvals");
        let p = d.join("t.jsonl");
        let mut records = sample();
        records.remove(3); // the approval
        for (i, r) in records.iter_mut().enumerate() {
            r["seq"] = json!(i);
        }
        write_chain(&p, &records);
        assert_eq!(level(&verify(&p, None), "approvals"), Level::Fail);

        let mut records = sample();
        records[3]["decision"] = json!("denied");
        write_chain(&p, &records);
        assert_eq!(level(&verify(&p, None), "approvals"), Level::Fail);
        records[4]["executed"] = json!(false);
        write_chain(&p, &records);
        assert_eq!(level(&verify(&p, None), "approvals"), Level::Pass);
    }

    #[test]
    fn file_changes_outside_the_record_are_reported() {
        let d = temp("files");
        let p = d.join("t.jsonl");
        let tr = |seq: u64, turn: u64, file: Value| {
            let mut r = json!({"seq":seq,"ts":"t","type":"tool_result","turn":turn,"call_id":format!("c{seq}"),
                               "tool":"edit_file","output":"ok","file":file});
            attribute("tool_result", &mut r);
            r
        };
        let base = |records: Vec<Value>| {
            let mut all = vec![sample()[0].clone()];
            all.extend(records);
            all
        };
        // Continuous: the second edit starts from the first edit's result.
        write_chain(
            &p,
            &base(vec![
                tr(
                    1,
                    1,
                    json!({"path":"a","before_sha256":"00","after_sha256":"11"}),
                ),
                tr(
                    2,
                    1,
                    json!({"path":"a","before_sha256":"11","after_sha256":"22"}),
                ),
            ]),
        );
        assert_eq!(level(&verify(&p, None), "files"), Level::Pass);
        // Changed between turns: reported as such.
        write_chain(
            &p,
            &base(vec![
                tr(
                    1,
                    1,
                    json!({"path":"a","before_sha256":"00","after_sha256":"11"}),
                ),
                tr(
                    2,
                    2,
                    json!({"path":"a","before_sha256":"99","after_sha256":"22"}),
                ),
            ]),
        );
        let f = verify(&p, None);
        assert!(
            f.iter()
                .any(|x| x.check == "files" && x.detail.contains("between turns"))
        );
        // Within a turn: something unrecorded changed it.
        write_chain(
            &p,
            &base(vec![
                tr(
                    1,
                    1,
                    json!({"path":"a","before_sha256":"00","after_sha256":"11"}),
                ),
                tr(
                    2,
                    1,
                    json!({"path":"a","before_sha256":"99","after_sha256":"22"}),
                ),
            ]),
        );
        let f = verify(&p, None);
        assert!(
            f.iter()
                .any(|x| x.check == "files" && x.detail.contains("during a turn"))
        );
        // A shell command's "committed" (null) before-state is not compared.
        let mut shell = tr(2, 1, Value::Null);
        shell.as_object_mut().unwrap().remove("file");
        shell["workspace_changes"] =
            json!({"paths":[{"path":"a","status":".M","before_sha256":null,"after_sha256":"33"}]});
        write_chain(
            &p,
            &base(vec![
                tr(
                    1,
                    1,
                    json!({"path":"a","before_sha256":"00","after_sha256":"11"}),
                ),
                shell,
            ]),
        );
        assert_eq!(level(&verify(&p, None), "files"), Level::Pass);
    }

    #[test]
    fn a_given_head_is_compared() {
        let d = temp("head");
        let p = d.join("t.jsonl");
        write_chain(&p, &sample());
        let text = std::fs::read_to_string(&p).unwrap();
        let hashes: Vec<String> = text
            .lines()
            .map(|l| {
                serde_json::from_str::<Value>(l).unwrap()["hash"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        let check = |head: &str| level(&verify_with_head(&p, None, Some(head)), "head");
        assert_eq!(
            check(&format!("sha256:{}", hashes.last().unwrap())),
            Level::Pass
        );
        assert_eq!(check(&hashes[2]), Level::Warn);
        assert_eq!(check(&"0".repeat(64)), Level::Fail);
    }

    #[test]
    fn unrecorded_intervals_are_listed() {
        let d = temp("gaps");
        let p = d.join("t.jsonl");
        let mut records = sample();
        records.truncate(5);
        records.push(json!({"seq":5,"ts":"t5","type":"transcript_disabled","turns":1}));
        records.push(
            json!({"seq":6,"ts":"t9","type":"session_start","schema":1,"session":"s",
                            "session_started":"t0","turns_before":3,"gap_after_seq":5}),
        );
        records.push(json!({"seq":7,"ts":"t10","type":"session_end","reason":"exit","turns":3}));
        for r in records.iter_mut() {
            let kind = r["type"].as_str().unwrap().to_string();
            attribute(&kind, r);
        }
        write_chain(&p, &records);
        let f = verify(&p, None);
        let gap = f.iter().find(|x| x.check == "gaps").unwrap();
        assert_eq!(gap.level, Level::Warn);
        assert!(
            gap.detail.contains("t5 to t9 (turns 2-3 unrecorded)"),
            "{}",
            gap.detail
        );
    }

    #[test]
    fn unknown_rules_are_reported() {
        let d = temp("rules");
        let p = d.join("t.jsonl");
        let mut records = sample();
        records[3]["rule"] = json!("approval.from_the_future");
        write_chain(&p, &records);
        let f = verify(&p, None);
        assert!(
            f.iter()
                .any(|x| x.check == "attribute" && x.detail.contains("from_the_future"))
        );
    }

    fn tampered(name: &str, change: impl Fn(&mut Vec<String>)) -> Vec<Finding> {
        let d = temp(name);
        let p = d.join("t.jsonl");
        write_chain(&p, &sample());
        let text = std::fs::read_to_string(&p).unwrap();
        let mut lines: Vec<String> = text.lines().map(String::from).collect();
        change(&mut lines);
        std::fs::write(&p, lines.join("\n") + "\n").unwrap();
        verify(&p, None)
    }

    #[test]
    fn edits_deletions_insertions_and_swaps_are_detected() {
        let edit = tampered("edit", |l| l[4] = l[4].replace("fn x()", "fn y()"));
        assert_eq!(level(&edit, "chain"), Level::Fail);
        let meta = tampered("meta", |l| l[2] = l[2].replace("read_file", "write_file"));
        assert_eq!(level(&meta, "chain"), Level::Fail);
        let delete = tampered("delete", |l| {
            l.remove(2);
        });
        assert_eq!(level(&delete, "chain"), Level::Fail);
        let swap = tampered("swap", |l| l.swap(1, 2));
        assert_eq!(level(&swap, "chain"), Level::Fail);
        let insert = tampered("insert", |l| {
            let copy = l[1].clone();
            l.insert(2, copy);
        });
        assert_eq!(level(&insert, "chain"), Level::Fail);
        let strip = tampered("strip", |l| {
            let mut v: Value = serde_json::from_str(&l[5]).unwrap();
            v.as_object_mut().unwrap().remove("hash");
            l[5] = v.to_string();
        });
        assert_eq!(level(&strip, "chain"), Level::Fail);
    }

    #[test]
    fn truncation_shows_as_incomplete_and_partial_lines_fail() {
        let cut = tampered("cut", |l| {
            l.pop();
        });
        assert_eq!(level(&cut, "chain"), Level::Pass);
        assert_eq!(level(&cut, "complete"), Level::Warn);

        let d = temp("partial");
        let p = d.join("t.jsonl");
        write_chain(&p, &sample());
        let mut text = std::fs::read_to_string(&p).unwrap();
        text.push_str("{\"seq\":5,\"ts\":");
        std::fs::write(&p, text).unwrap();
        assert_eq!(level(&verify(&p, None), "lines"), Level::Fail);
    }

    #[test]
    fn withheld_content_verifies_with_the_same_hashes() {
        let d = temp("withheld");
        let p = d.join("t.jsonl");
        write_chain(&p, &sample());
        let full = std::fs::read(&p).unwrap();
        let shared = d.join("shared.jsonl");

        let code = export(&full, &["code".into()]).unwrap();
        assert!(
            !code.contains("fix the bug") && !code.contains("fn x()") && !code.contains("\"salt\"")
        );
        std::fs::write(&shared, &code).unwrap();
        assert_eq!(level(&verify(&shared, None), "chain"), Level::Pass);
        // The shared copy's metadata is still bound.
        std::fs::write(&shared, code.replace("read_file", "write_file")).unwrap();
        assert_eq!(level(&verify(&shared, None), "chain"), Level::Fail);

        // Withhold outputs only: the instruction stays readable and checkable,
        // and the copy carries no salt that would expose the output.
        let outputs = export(&full, &["outputs".into()]).unwrap();
        assert!(outputs.contains("fix the bug") && !outputs.contains("fn x()"));
        std::fs::write(&shared, &outputs).unwrap();
        assert_eq!(level(&verify(&shared, None), "chain"), Level::Pass);
        std::fs::write(&shared, outputs.replace("fix the bug", "fix the bag")).unwrap();
        assert_eq!(level(&verify(&shared, None), "chain"), Level::Fail);

        assert!(export(&full, &["nonsense".into()]).is_err());
        assert!(export(b"{\"seq\":0}\n", &["code".into()]).is_err());
    }

    #[test]
    fn content_commitments_are_salted() {
        let a = commit(&field_salt(b"salt-one", "output"), &json!("exit: 0"));
        let b = commit(&field_salt(b"salt-two", "output"), &json!("exit: 0"));
        assert_ne!(a, b);
        assert!(a.starts_with("c:") && a.len() == 66);
    }

    #[test]
    fn chain_tail_continues_or_binds_earlier_bytes() {
        let d = temp("tail");
        let p = d.join("t.jsonl");
        write_chain(&p, &sample());
        let last: Value =
            serde_json::from_str(std::fs::read_to_string(&p).unwrap().lines().last().unwrap())
                .unwrap();
        let (h, partial) = chain_tail(&mut File::open(&p).unwrap()).unwrap();
        assert_eq!(Some(h), claimed_hash(&last));
        assert!(!partial);

        // A legacy file (no hashes) and one ending in a partial line.
        let legacy = d.join("legacy.jsonl");
        std::fs::write(&legacy, "{\"seq\":0}\n").unwrap();
        let (h, partial) = chain_tail(&mut File::open(&legacy).unwrap()).unwrap();
        assert_eq!(h, sha256(&[b"{\"seq\":0}\n"]));
        assert!(!partial);
        std::fs::write(&legacy, "{\"seq\":0}\n{\"se").unwrap();
        let (h, partial) = chain_tail(&mut File::open(&legacy).unwrap()).unwrap();
        assert_eq!(h, sha256(&[b"{\"seq\":0}\n{\"se\n"]));
        assert!(partial);
    }

    #[test]
    fn long_last_lines_are_found() {
        let d = temp("long");
        let p = d.join("t.jsonl");
        let mut records = sample();
        records[4]["output"] = json!("x".repeat(200_000));
        write_chain(&p, &records[..5]);
        let last: Value =
            serde_json::from_str(std::fs::read_to_string(&p).unwrap().lines().last().unwrap())
                .unwrap();
        let (h, _) = chain_tail(&mut File::open(&p).unwrap()).unwrap();
        assert_eq!(Some(h), claimed_hash(&last));
    }

    #[test]
    fn ledger_chains_and_anchors_heads() {
        let d = temp("ledger");
        let t = d.join("t.jsonl");
        write_chain(&t, &sample());
        let head = std::fs::read_to_string(&t)
            .unwrap()
            .lines()
            .last()
            .map(|l| serde_json::from_str::<Value>(l).unwrap()["hash"].clone())
            .unwrap();
        let lp = d.join("audit/ledger.jsonl");
        let mut ledger = Ledger::new(lp.clone(), true);
        let me = absolute(&t);
        ledger.append(
            "session_start",
            json!({ "session": "s", "transcript": me, "recording": "on" }),
        );
        ledger.append(
            "session_end",
            json!({ "session": "s", "transcript": me, "records": 6, "head": head }),
        );
        let f = verify(&t, Some(&lp));
        assert_eq!(level(&f, "ledger"), Level::Pass);
        assert_eq!(level(&f, "anchor"), Level::Pass);
        let mode = std::fs::metadata(&lp).unwrap().permissions();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
            0o600
        );

        // A transcript rewritten consistently (every hash recomputed) passes
        // its own chain but no longer matches the recorded head.
        let mut records = sample();
        records[1]["instruction"] = json!("something else");
        write_chain(&t, &records);
        let f = verify(&t, Some(&lp));
        assert_eq!(level(&f, "chain"), Level::Pass);
        assert_eq!(level(&f, "anchor"), Level::Fail);

        // Editing the ledger breaks its own chain.
        let text = std::fs::read_to_string(&lp).unwrap();
        std::fs::write(&lp, text.replace("\"s\"", "\"z\"")).unwrap();
        assert_eq!(level(&verify(&t, Some(&lp)), "ledger"), Level::Fail);
    }

    #[test]
    fn concurrent_ledger_writers_keep_one_chain() {
        let d = temp("concurrent");
        let lp = d.join("ledger.jsonl");
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let lp = lp.clone();
                std::thread::spawn(move || {
                    let mut l = Ledger::new(lp, false);
                    for j in 0..20 {
                        l.append("session_start", json!({ "session": format!("{i}-{j}") }));
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let c = walk(&std::fs::read(&lp).unwrap(), false);
        assert_eq!(c.chained, 160);
        assert!(c.broken.is_empty() && c.bad_json.is_empty());
    }
}
