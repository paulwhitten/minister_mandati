//! Sessions and transcripts. A session is one continuous conversation (one
//! context); its optional transcript is an append-only JSON Lines file of
//! timestamped events under `~/.mima/transcripts/`. Transcripts are off by
//! default. All times are UTC. See `docs/sessions.md`.

use serde_json::{Map, Value, json};
use std::collections::hash_map::RandomState;
use std::fs::{DirBuilder, File, OpenOptions};
use std::hash::{BuildHasher, Hasher};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Transcript format version; bumped only for breaking changes.
const SCHEMA: u32 = 1;
/// Longest project label in a session id.
const MAX_LABEL: usize = 32;

pub struct Session {
    id: String,
    /// Project label in the id (repository or folder name), if any.
    label: Option<String>,
    started: SystemTime,
    dir: PathBuf,
    /// Largest tool output stored in the transcript, in bytes.
    max_output_bytes: usize,
    transcript: Option<Transcript>,
    turns: u64,
    /// Per-session record counter; continues across disable/enable.
    seq: u64,
    /// Explicit transcript file (`--transcript-path`), instead of `<dir>/<id>.jsonl`.
    path_override: Option<PathBuf>,
}

struct Transcript {
    file: File,
    path: PathBuf,
    /// Set after the first write failure, so it is reported only once.
    failed: bool,
}

impl Session {
    /// A new session (fresh id, empty history), labeled with the project the
    /// working directory belongs to. No transcript until `enable`.
    pub fn new(dir: PathBuf, max_output_bytes: usize) -> Self {
        let label = std::env::current_dir()
            .ok()
            .and_then(|cwd| project_label(&cwd));
        Self::with_label(dir, max_output_bytes, label)
    }

    /// Id format: `<UTC start>-<label>-<4 hex>`, or `<UTC start>-<4 hex>`
    /// without a label. The id is also the transcript's file name, so time
    /// order is file-name order. The label is for reading a listing, not for
    /// uniqueness (the timestamp and random suffix provide that).
    fn with_label(dir: PathBuf, max_output_bytes: usize, label: Option<String>) -> Self {
        let started = SystemTime::now();
        let id = match &label {
            Some(l) => format!("{}-{l}-{:04x}", compact_utc(started), random_u16()),
            None => format!("{}-{:04x}", compact_utc(started), random_u16()),
        };
        Self {
            id,
            label,
            started,
            dir,
            max_output_bytes,
            transcript: None,
            turns: 0,
            seq: 0,
            path_override: None,
        }
    }

    /// The next session in the same process: new id, same directory and
    /// limits. The caller re-enables the transcript if it was on.
    pub fn successor(&self) -> Self {
        Self::with_label(self.dir.clone(), self.max_output_bytes, self.label.clone())
    }

    /// Writes the transcript to `path` instead of the transcript directory.
    pub fn set_path(&mut self, path: PathBuf) {
        self.path_override = Some(path);
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn started_utc(&self) -> String {
        rfc3339_utc(self.started)
    }

    pub fn transcript_path(&self) -> Option<&Path> {
        self.transcript.as_ref().map(|t| t.path.as_path())
    }

    pub fn is_recording(&self) -> bool {
        self.transcript.is_some()
    }

    /// Number of the turn being started (1-based), for transcript records.
    pub fn next_turn(&mut self) -> u64 {
        self.turns += 1;
        self.turns
    }

    pub fn turn(&self) -> u64 {
        self.turns
    }

    /// Opens this session's transcript and writes `session_start` with the
    /// given header fields. Creates the directory (mode 0700) and the file
    /// (mode 0600). Re-enabling within a session appends to the same file.
    /// Idempotent while recording.
    pub fn enable(&mut self, header: Value) -> std::io::Result<PathBuf> {
        if let Some(path) = self.transcript_path() {
            return Ok(path.to_path_buf());
        }
        let path = match &self.path_override {
            Some(p) => p.clone(),
            None => self.dir.join(format!("{}.jsonl", self.id)),
        };
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
        }
        let file = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(&path)?;
        self.transcript = Some(Transcript {
            file,
            path: path.clone(),
            failed: false,
        });
        let mut fields = json!({
            "schema": SCHEMA,
            "session": self.id,
            "session_started": self.started_utc(),
            "turns_before": self.turns,
        });
        merge(&mut fields, header);
        self.record("session_start", fields);
        Ok(path)
    }

    /// Stops recording for the rest of this session (the session continues).
    pub fn disable(&mut self) {
        self.record("transcript_disabled", json!({ "turns": self.turns }));
        self.transcript = None;
    }

    /// Writes `session_end` and closes the transcript. No-op when off.
    pub fn end(&mut self, reason: &str, fields: Value) {
        if self.transcript.is_none() {
            return;
        }
        let mut all = json!({ "reason": reason, "turns": self.turns });
        merge(&mut all, fields);
        self.record("session_end", all);
        self.transcript = None;
    }

    /// Appends one event: `seq`, `ts` (UTC) and `type`, then `fields`. Each
    /// line is written and flushed immediately. A write failure is reported
    /// once and never stops the agent. No-op when transcripts are off.
    pub fn record(&mut self, kind: &str, fields: Value) {
        let Some(t) = self.transcript.as_mut() else {
            return;
        };
        let mut line =
            json!({ "seq": self.seq, "ts": rfc3339_utc(SystemTime::now()), "type": kind });
        merge(&mut line, fields);
        self.seq += 1;
        let mut text = line.to_string();
        text.push('\n');
        if let Err(e) = t
            .file
            .write_all(text.as_bytes())
            .and_then(|_| t.file.flush())
            && !t.failed
        {
            t.failed = true;
            tracing::warn!(error = %e, path = %t.path.display(), "transcript write failed; continuing without it");
        }
    }

    /// Tool output as stored in the transcript: full, up to the configured cap.
    pub fn stored_output(&self, output: &str) -> Value {
        if output.len() <= self.max_output_bytes {
            return json!(output);
        }
        let mut end = self.max_output_bytes;
        while !output.is_char_boundary(end) {
            end -= 1;
        }
        json!(format!(
            "{}\n[... transcript copy cut at {} of {} bytes ...]",
            &output[..end],
            end,
            output.len()
        ))
    }
}

/// Copies `extra`'s fields into `base` (both JSON objects).
fn merge(base: &mut Value, extra: Value) {
    if let (Some(b), Value::Object(e)) = (base.as_object_mut(), extra) {
        let e: Map<String, Value> = e;
        b.extend(e);
    }
}

/// Label for the project `cwd` belongs to: the name of the nearest ancestor
/// (or `cwd` itself) containing `.git` (a directory, or a file for worktrees
/// and submodules), else the last component of `cwd`. Sanitized for file
/// names; `None` if nothing usable remains (e.g. `/`).
fn project_label(cwd: &Path) -> Option<String> {
    let root = cwd
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .unwrap_or(cwd);
    sanitize_label(&root.file_name()?.to_string_lossy())
}

/// Keeps `A-Z a-z 0-9 . _ -`, replaces anything else with `_`, and caps the
/// length. `None` when the result is empty.
fn sanitize_label(name: &str) -> Option<String> {
    let clean: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(MAX_LABEL)
        .collect();
    (!clean.is_empty()).then_some(clean)
}

/// 16 random bits from std's per-process random hasher keys.
fn random_u16() -> u16 {
    let mut h = RandomState::new().build_hasher();
    h.write_u128(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    );
    h.finish() as u16
}

/// UTC civil date and time for a point in time.
struct Utc {
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    millis: u32,
}

fn utc(t: SystemTime) -> Utc {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil-from-days (H. Hinnant), valid for the proleptic Gregorian calendar.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    Utc {
        year,
        month,
        day,
        hour: (rem / 3_600) as u32,
        minute: (rem % 3_600 / 60) as u32,
        second: (rem % 60) as u32,
        millis: d.subsec_millis(),
    }
}

/// RFC 3339 UTC with milliseconds, e.g. `2026-09-27T14:32:05.123Z`.
pub fn rfc3339_utc(t: SystemTime) -> String {
    let u = utc(t);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        u.year, u.month, u.day, u.hour, u.minute, u.second, u.millis
    )
}

/// Basic ISO 8601 UTC for ids and file names, e.g. `20260927T143205Z`.
fn compact_utc(t: SystemTime) -> String {
    let u = utc(t);
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        u.year, u.month, u.day, u.hour, u.minute, u.second
    )
}

/// Expands a leading `~/` using `$HOME`.
pub fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    fn at(secs: u64, millis: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs) + Duration::from_millis(millis)
    }

    #[test]
    fn formats_utc() {
        assert_eq!(rfc3339_utc(at(0, 0)), "1970-01-01T00:00:00.000Z");
        // 2026-09-27T14:32:05.123Z
        assert_eq!(
            rfc3339_utc(at(1_790_519_525, 123)),
            "2026-09-27T14:32:05.123Z"
        );
        assert_eq!(compact_utc(at(1_790_519_525, 999)), "20260927T143205Z");
        // Leap day and year boundaries.
        assert_eq!(
            rfc3339_utc(at(1_709_164_800, 0)),
            "2024-02-29T00:00:00.000Z"
        );
        assert_eq!(
            rfc3339_utc(at(1_798_761_599, 0)),
            "2026-12-31T23:59:59.000Z"
        );
    }

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mima-session-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d.join("transcripts")
    }

    fn lines(path: &Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn off_by_default_and_records_nothing() {
        let dir = temp_dir("off");
        let mut s = Session::new(dir.clone(), 1024);
        s.record("turn_start", json!({ "turn": 1 }));
        s.end("exit", json!({}));
        assert!(!s.is_recording());
        assert!(!dir.exists());
    }

    #[test]
    fn transcript_lines_are_ordered_timestamped_and_private() {
        let dir = temp_dir("on");
        let mut s = Session::new(dir.clone(), 1024);
        let path = s.enable(json!({ "mode": "test" })).unwrap();
        assert!(
            path.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with(s.id())
        );
        s.record("turn_start", json!({ "turn": 1, "instruction": "hi" }));
        s.end("exit", json!({}));

        let recs = lines(&path);
        let types: Vec<&str> = recs.iter().map(|r| r["type"].as_str().unwrap()).collect();
        assert_eq!(types, ["session_start", "turn_start", "session_end"]);
        for (i, r) in recs.iter().enumerate() {
            assert_eq!(r["seq"], i as u64);
            let ts = r["ts"].as_str().unwrap();
            assert!(ts.ends_with('Z') && ts.len() == 24, "{ts}");
        }
        assert_eq!(recs[0]["schema"], SCHEMA);
        assert_eq!(recs[0]["mode"], "test");
        assert_eq!(recs[0]["session"], s.id());

        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&dir), 0o700);
    }

    #[test]
    fn successor_gets_a_new_id_and_file() {
        let dir = temp_dir("next");
        let mut a = Session::new(dir.clone(), 1024);
        let pa = a.enable(json!({})).unwrap();
        a.end("new", json!({}));
        let mut b = a.successor();
        assert_ne!(a.id(), b.id());
        let pb = b.enable(json!({})).unwrap();
        assert_ne!(pa, pb);
    }

    #[test]
    fn disable_then_enable_appends_with_continuing_seq() {
        let dir = temp_dir("toggle");
        let mut s = Session::new(dir, 1024);
        let path = s.enable(json!({})).unwrap();
        s.disable();
        s.record("turn_start", json!({})); // not recorded while off
        assert_eq!(s.enable(json!({})).unwrap(), path);
        s.end("exit", json!({}));
        let recs = lines(&path);
        let types: Vec<&str> = recs.iter().map(|r| r["type"].as_str().unwrap()).collect();
        assert_eq!(
            types,
            [
                "session_start",
                "transcript_disabled",
                "session_start",
                "session_end"
            ]
        );
        let seqs: Vec<u64> = recs.iter().map(|r| r["seq"].as_u64().unwrap()).collect();
        assert_eq!(seqs, [0, 1, 2, 3]);
    }

    #[test]
    fn stored_output_is_capped() {
        let s = Session::new(temp_dir("cap"), 10);
        assert_eq!(s.stored_output("short"), json!("short"));
        let long = s.stored_output(&"é".repeat(20));
        assert!(long.as_str().unwrap().contains("cut at 10 of 40 bytes"));
    }

    #[test]
    fn unwritable_directory_is_an_error_not_a_panic() {
        let mut s = Session::new(PathBuf::from("/proc/mima-cannot-create"), 10);
        assert!(s.enable(json!({})).is_err());
        assert!(!s.is_recording());
        s.record("turn_start", json!({})); // still a no-op
    }

    #[test]
    fn sanitizes_labels() {
        assert_eq!(sanitize_label("cli_agent").as_deref(), Some("cli_agent"));
        assert_eq!(
            sanitize_label("my repo: v2").as_deref(),
            Some("my_repo__v2")
        );
        assert_eq!(sanitize_label("café").as_deref(), Some("caf_"));
        assert_eq!(sanitize_label(&"x".repeat(50)).unwrap().len(), MAX_LABEL);
        assert_eq!(sanitize_label(""), None);
    }

    #[test]
    fn label_is_the_repository_root_from_any_subfolder() {
        let base = std::env::temp_dir().join(format!("mima-label-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("my project");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src/tools")).unwrap();
        std::fs::create_dir_all(base.join("plain/dir")).unwrap();

        assert_eq!(
            project_label(&repo.join("src/tools")).as_deref(),
            Some("my_project")
        );
        assert_eq!(project_label(&repo).as_deref(), Some("my_project"));
        // Outside a repository: the last folder of the working directory.
        assert_eq!(
            project_label(&base.join("plain/dir")).as_deref(),
            Some("dir")
        );
        assert_eq!(project_label(Path::new("/")), None);

        // A `.git` file (worktree or submodule) also marks a root.
        let wt = base.join("worktree");
        std::fs::create_dir_all(wt.join("a")).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: elsewhere").unwrap();
        assert_eq!(project_label(&wt.join("a")).as_deref(), Some("worktree"));
    }

    #[test]
    fn id_and_file_name_carry_the_label() {
        let dir = temp_dir("label");
        let mut s = Session::with_label(dir, 1024, Some("cli_agent".into()));
        let parts: Vec<&str> = s.id().split('-').collect();
        assert_eq!(parts.len(), 3, "{}", s.id());
        assert_eq!(parts[0].len(), 16); // 20260927T143205Z
        assert_eq!(parts[1], "cli_agent");
        assert_eq!(parts[2].len(), 4);
        let path = s.enable(json!({})).unwrap();
        assert_eq!(path.file_stem().unwrap().to_str().unwrap(), s.id());
        assert!(s.successor().id().contains("-cli_agent-"));

        let unlabeled = Session::with_label(temp_dir("nolabel"), 1024, None);
        assert_eq!(unlabeled.id().split('-').count(), 2);
    }

    #[test]
    fn expands_home() {
        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(expand_home("~/x/y"), PathBuf::from(home).join("x/y"));
        }
        assert_eq!(expand_home("/abs"), PathBuf::from("/abs"));
    }
}
