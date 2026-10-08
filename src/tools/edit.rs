//! `edit_file`: exact search/replace editing, plus the shared pieces the file
//! tools use: read tracking (stale-file protection), matching, and unified
//! diffs for the approval prompt. Design and evidence: `docs/editing.md`.
//!
//! Matching is exact first. A few deterministic, low-risk tolerances follow,
//! each of which must still find exactly one place (see `plan_edit`); there
//! is no similarity-threshold matching. Any non-exact match is reported.

use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::fs::{atomic_write, ensure_allowed};
use super::{BaseTool, FsSnafu, RefusedSnafu, Result, ToolEnv, ToolSpec};
use snafu::prelude::*;

/// Context lines around each change in a diff.
const DIFF_CONTEXT: usize = 3;
/// Diff lines shown in an approval prompt before truncating.
const MAX_DIFF_LINES: usize = 200;
/// Numbered lines of the edited region returned to the model on success.
const MAX_SNIPPET_LINES: usize = 20;
/// Closest-match search is skipped for files longer than this.
const MAX_SIMILARITY_LINES: usize = 20_000;
/// Minimum similarity for showing a closest match in a not-found error.
const MIN_SIMILARITY: f64 = 0.6;

/// Which files the model has read, and their content at that time. Edits and
/// overwrites require a read of the current content (any range), so the model
/// never edits text it has not seen or that changed underneath it.
#[derive(Default)]
pub struct FileTracker {
    seen: Mutex<HashMap<PathBuf, u64>>,
}

/// Whether a file's current content matches what the model last read.
#[derive(Debug, PartialEq)]
pub enum Freshness {
    Unread,
    Stale,
    Fresh,
}

impl FileTracker {
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Records `bytes` as the content the model now knows for `path`.
    pub fn record(&self, path: &Path, bytes: &[u8]) {
        if let Ok(mut seen) = self.seen.lock() {
            seen.insert(path.to_path_buf(), content_hash(bytes));
        }
    }

    pub fn check(&self, path: &Path, bytes: &[u8]) -> Freshness {
        match self.seen.lock().ok().and_then(|s| s.get(path).copied()) {
            None => Freshness::Unread,
            Some(h) if h == content_hash(bytes) => Freshness::Fresh,
            Some(_) => Freshness::Stale,
        }
    }

    /// Forgets all reads (a new session starts with no knowledge of files).
    pub fn clear(&self) {
        if let Ok(mut seen) = self.seen.lock() {
            seen.clear();
        }
    }
}

/// Change detection only (not a security boundary): a fixed-key SipHash of
/// the bytes.
fn content_hash(bytes: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

/// Refuses an edit or overwrite of `path` unless its current content is what
/// the model last read.
pub fn require_fresh(
    tracker: &FileTracker,
    tool: &str,
    shown: &str,
    path: &Path,
    bytes: &[u8],
) -> Result<()> {
    match tracker.check(path, bytes) {
        Freshness::Fresh => Ok(()),
        Freshness::Unread => RefusedSnafu {
            message: format!(
                "{tool} refused: read {shown} (at least the lines you are changing) before \
                 changing it. No changes were made."
            ),
        }
        .fail(),
        Freshness::Stale => RefusedSnafu {
            message: format!(
                "{tool} refused: {shown} changed on disk after you last read it (by the user \
                 or another process). Re-read the part you want to change, then retry. No \
                 changes were made."
            ),
        }
        .fail(),
    }
}

// ---------------------------------------------------------------------------
// Matching

/// A planned edit: the new file content and where it changed.
#[derive(Debug)]
pub struct Planned {
    pub new_content: String,
    pub blocks: Vec<Block>,
    /// Set when a tolerance (not an exact match) found the text.
    pub note: Option<&'static str>,
    pub count: usize,
}

/// One changed region, in whole lines (0-based line indices).
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub old_start: usize,
    pub old_len: usize,
    pub new_start: usize,
    pub new_len: usize,
}

/// Line-ending and byte-order-mark handling around the LF-only matcher.
struct Encoding {
    bom: bool,
    crlf: bool,
}

impl Encoding {
    /// Detects the file's conventions and returns its text with LF endings.
    /// A file counts as CRLF only if every line feed is part of a CRLF, so
    /// mixed files are matched as they are and never rewritten wholesale.
    fn decode(content: &str) -> (Self, String) {
        let (bom, body) = match content.strip_prefix('\u{feff}') {
            Some(rest) => (true, rest),
            None => (false, content),
        };
        let lf = body.matches('\n').count();
        let crlf = lf > 0 && body.matches("\r\n").count() == lf;
        let text = if crlf {
            body.replace("\r\n", "\n")
        } else {
            body.to_string()
        };
        (Self { bom, crlf }, text)
    }

    fn encode(&self, text: &str) -> String {
        let body = if self.crlf {
            text.replace('\n', "\r\n")
        } else {
            text.to_string()
        };
        if self.bom {
            format!("\u{feff}{body}")
        } else {
            body
        }
    }
}

/// Plans replacing `old` with `new` in `content`. On failure returns the
/// message for the model. Order of attempts, each requiring exactly one
/// match (or any number with `replace_all`, exact match only):
///
/// 1. exact (after normalizing line endings to the file's);
/// 2. with `read_file` line-number prefixes removed, if every line has one;
/// 3. without one trailing newline (tool-call parsers may add or drop it);
/// 4. ignoring trailing whitespace on each line (whole lines only);
/// 5. with a uniform indentation difference (every line of `old_string`
///    indented by the same amount more or less than the file); `new_string`
///    is re-indented to match.
pub fn plan_edit(
    content: &str,
    old: &str,
    new: &str,
    replace_all: bool,
    shown: &str,
) -> Result<Planned, String> {
    let (enc, text) = Encoding::decode(content);
    let mut old = old.replace("\r\n", "\n");
    let mut new = new.replace("\r\n", "\n");

    if let Some(planned) = exact(&text, &old, &new, replace_all, shown, None)? {
        return Ok(planned.encoded(&enc));
    }

    let mut note = None;
    if let Some(stripped) = strip_line_numbers(&old) {
        old = stripped;
        if let Some(s) = strip_line_numbers(&new) {
            new = s;
        }
        note = Some("ignored read_file line-number prefixes in old_string");
        if let Some(planned) = exact(&text, &old, &new, replace_all, shown, note)? {
            return Ok(planned.encoded(&enc));
        }
    }

    if let Some(trimmed) = old.strip_suffix('\n').filter(|t| !t.trim().is_empty()) {
        let trimmed_new = new.strip_suffix('\n').unwrap_or(&new);
        let n = Some("ignored a trailing newline in old_string");
        if let Some(planned) = exact(&text, trimmed, trimmed_new, false, shown, n)? {
            return Ok(planned.encoded(&enc));
        }
    }

    if let Some(planned) = trailing_whitespace_match(&text, &old, &new, shown)? {
        return Ok(planned.encoded(&enc));
    }

    if let Some(planned) = indentation_match(&text, &old, &new, shown)? {
        return Ok(planned.encoded(&enc));
    }

    Err(not_found_message(&text, &old, &new, shown, note.is_some()))
}

impl Planned {
    fn encoded(mut self, enc: &Encoding) -> Self {
        self.new_content = enc.encode(&self.new_content);
        self
    }
}

/// Exact substring replacement. `Ok(None)` when not found.
fn exact(
    text: &str,
    old: &str,
    new: &str,
    replace_all: bool,
    shown: &str,
    note: Option<&'static str>,
) -> Result<Option<Planned>, String> {
    let positions: Vec<usize> = text.match_indices(old).map(|(p, _)| p).collect();
    match positions.len() {
        0 => Ok(None),
        n if n > 1 && !replace_all => Err(multiple_message(text, &positions, shown)),
        _ => Ok(Some(apply(text, &positions, old.len(), new, note))),
    }
}

/// Replaces `old_len` bytes at each position (ascending, non-overlapping).
fn apply(
    text: &str,
    positions: &[usize],
    old_len: usize,
    new: &str,
    note: Option<&'static str>,
) -> Planned {
    let mut out = String::with_capacity(text.len() + positions.len() * new.len());
    let mut ranges = Vec::new(); // (old start, old end, new start, new end)
    let mut last = 0;
    for &p in positions {
        out.push_str(&text[last..p]);
        let np = out.len();
        out.push_str(new);
        ranges.push((p, p + old_len, np, out.len()));
        last = p + old_len;
    }
    out.push_str(&text[last..]);
    let blocks = ranges
        .iter()
        .map(|&(p, q, np, nq)| block_for(text, &out, p, q, np, nq))
        .collect();
    Planned {
        new_content: out,
        blocks,
        note,
        count: positions.len(),
    }
}

/// The whole-line region a replacement of `old[p..q]` by `new[np..nq]` changed.
fn block_for(old: &str, new: &str, p: usize, q: usize, np: usize, nq: usize) -> Block {
    let ls = old[..p].rfind('\n').map_or(0, |i| i + 1);
    let replaced_whole_lines = q > p && old.as_bytes()[q - 1] == b'\n';
    // If whole lines were replaced by text without a final newline, the next
    // line joins the replacement, so it belongs to the block too.
    let joins_next = replaced_whole_lines && nq > np && !new[np..nq].ends_with('\n');
    let le = if replaced_whole_lines && !joins_next {
        q
    } else {
        old[q..].find('\n').map_or(old.len(), |i| q + i)
    };
    let nls = np - (p - ls);
    let nle = nq + (le - q);
    Block {
        old_start: old[..ls].matches('\n').count(),
        old_len: lines(&old[ls..le]).len(),
        new_start: new[..nls].matches('\n').count(),
        new_len: lines(&new[nls..nle]).len(),
    }
}

/// Lines of a text region (a final newline does not start another line).
fn lines(s: &str) -> Vec<&str> {
    if s.is_empty() {
        Vec::new()
    } else {
        s.strip_suffix('\n').unwrap_or(s).split('\n').collect()
    }
}

/// `old` with `read_file`'s `   12\t` prefixes removed, if every non-empty
/// line has one (so ordinary text with a leading number is never touched).
fn strip_line_numbers(s: &str) -> Option<String> {
    let prefixed = |l: &str| {
        let t = l.trim_start();
        let digits = t.len() - t.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        digits > 0 && t[digits..].starts_with('\t')
    };
    let mut any = false;
    for l in s.split('\n').filter(|l| !l.is_empty()) {
        if !prefixed(l) {
            return None;
        }
        any = true;
    }
    if !any {
        return None;
    }
    Some(
        s.split('\n')
            .map(|l| l.split_once('\t').map_or(l, |(_, rest)| rest))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// Line-block match ignoring trailing whitespace on each line. Requires at
/// least one non-blank line and a unique match; replaces those whole lines.
fn trailing_whitespace_match(
    text: &str,
    old: &str,
    new: &str,
    shown: &str,
) -> Result<Option<Planned>, String> {
    let old_lines = lines(old);
    if old_lines.iter().all(|l| l.trim().is_empty()) {
        return Ok(None);
    }
    let file_lines: Vec<&str> = text.split('\n').collect();
    let k = old_lines.len();
    if k > file_lines.len() {
        return Ok(None);
    }
    let hits: Vec<usize> = (0..=file_lines.len() - k)
        .filter(|&i| (0..k).all(|j| file_lines[i + j].trim_end() == old_lines[j].trim_end()))
        .collect();
    match hits.len() {
        0 => Ok(None),
        1 => {
            let start = line_offset(text, hits[0]);
            let last = hits[0] + k - 1;
            let end = line_offset(text, last) + file_lines[last].len();
            let replacement = if old.ends_with('\n') {
                new.strip_suffix('\n').unwrap_or(new)
            } else {
                new
            };
            let mut planned = apply_range(text, start, end, replacement);
            planned.note = Some("ignored trailing whitespace in old_string");
            Ok(Some(planned))
        }
        _ => {
            let offsets: Vec<usize> = hits.iter().map(|&i| line_offset(text, i)).collect();
            Err(multiple_message(text, &offsets, shown))
        }
    }
}

/// Leading whitespace of a line.
fn indent(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

/// Line-block match where every non-blank line of `old` differs from the
/// file only by one common indentation change (e.g. the model indented the
/// whole snippet by four extra spaces, or dropped a level). The match must be
/// unique; `new` is re-indented by the same change.
fn indentation_match(
    text: &str,
    old: &str,
    new: &str,
    shown: &str,
) -> Result<Option<Planned>, String> {
    let old_lines = lines(old);
    let non_blank: Vec<&str> = old_lines
        .iter()
        .copied()
        .filter(|l| !l.trim().is_empty())
        .collect();
    if non_blank.is_empty() {
        return Ok(None);
    }
    // The common indentation of old_string, removed before comparing.
    let common = non_blank
        .iter()
        .map(|l| indent(l))
        .min_by_key(|i| i.len())
        .unwrap_or("");
    if !non_blank.iter().all(|l| l.starts_with(common)) {
        return Ok(None);
    }
    let dedented: Vec<&str> = old_lines
        .iter()
        .map(|l| l.strip_prefix(common).unwrap_or(l.trim_start()))
        .collect();
    let file_lines: Vec<&str> = text.split('\n').collect();
    let k = old_lines.len();
    if k > file_lines.len() {
        return Ok(None);
    }
    // For each window, the file's extra indentation must be the same on
    // every non-blank line.
    let window_indent = |i: usize| -> Option<&str> {
        let mut found: Option<&str> = None;
        for (j, d) in dedented.iter().enumerate() {
            let f = file_lines[i + j];
            if d.trim().is_empty() {
                if !f.trim().is_empty() {
                    return None;
                }
                continue;
            }
            let extra = f.strip_suffix(d)?;
            if !extra.chars().all(|c| c == ' ' || c == '\t') {
                return None;
            }
            match found {
                None => found = Some(extra),
                Some(e) if e == extra => {}
                Some(_) => return None,
            }
        }
        found
    };
    let hits: Vec<(usize, &str)> = (0..=file_lines.len() - k)
        .filter_map(|i| window_indent(i).map(|e| (i, e)))
        .collect();
    match hits.len() {
        0 => Ok(None),
        1 => {
            let (i, extra) = hits[0];
            if extra == common {
                return Ok(None); // same indentation: an exact match would have found it
            }
            let reindented: Vec<String> = lines(new)
                .iter()
                .map(|l| {
                    if l.trim().is_empty() {
                        String::new()
                    } else {
                        format!(
                            "{extra}{}",
                            l.strip_prefix(common).unwrap_or(l.trim_start())
                        )
                    }
                })
                .collect();
            let mut replacement = reindented.join("\n");
            if new.ends_with('\n') && !old.ends_with('\n') {
                replacement.push('\n');
            }
            let start = line_offset(text, i);
            let last = i + k - 1;
            let end = line_offset(text, last) + file_lines[last].len();
            let mut planned = apply_range(text, start, end, &replacement);
            planned.note =
                Some("adjusted indentation (old_string was indented differently from the file)");
            Ok(Some(planned))
        }
        _ => {
            let offsets: Vec<usize> = hits.iter().map(|&(i, _)| line_offset(text, i)).collect();
            Err(multiple_message(text, &offsets, shown))
        }
    }
}

fn apply_range(text: &str, start: usize, end: usize, new: &str) -> Planned {
    let mut out = String::with_capacity(text.len() + new.len());
    out.push_str(&text[..start]);
    out.push_str(new);
    let nq = out.len();
    out.push_str(&text[end..]);
    let blocks = vec![block_for(text, &out, start, end, start, nq)];
    Planned {
        new_content: out,
        blocks,
        note: None,
        count: 1,
    }
}

/// Byte offset where 0-based line `n` starts.
fn line_offset(text: &str, n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    text.match_indices('\n')
        .nth(n - 1)
        .map_or(text.len(), |(i, _)| i + 1)
}

/// 1-based line number of a byte offset.
fn line_number(text: &str, offset: usize) -> usize {
    text[..offset].matches('\n').count() + 1
}

// ---------------------------------------------------------------------------
// Messages for the model

fn multiple_message(text: &str, positions: &[usize], shown: &str) -> String {
    let nums: Vec<String> = positions
        .iter()
        .take(10)
        .map(|&p| line_number(text, p).to_string())
        .collect();
    let more = if positions.len() > 10 { ", ..." } else { "" };
    format!(
        "edit_file failed: old_string occurs {n} times in {shown} (lines {lines}{more}). No \
         changes were made. Add more surrounding lines to old_string so it matches exactly one \
         place, or set replace_all=true to change all {n}.",
        n = positions.len(),
        lines = nums.join(", ")
    )
}

fn not_found_message(text: &str, old: &str, new: &str, shown: &str, had_prefixes: bool) -> String {
    let mut msg =
        format!("edit_file failed: old_string was not found in {shown}. No changes were made.");
    if !new.trim().is_empty() && text.contains(new) {
        let line = line_number(text, text.find(new).unwrap_or(0));
        msg.push_str(&format!(
            " new_string is already present (line {line}), so the change may already be \
             applied; re-read the file before editing again."
        ));
        return msg;
    }
    let looks_prefixed = |l: &str| {
        l.trim_start()
            .split_once('\t')
            .is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
    };
    if !had_prefixes && old.lines().any(looks_prefixed) {
        msg.push_str(
            " old_string appears to contain read_file line-number prefixes (a number and a TAB); \
             remove them, they are not part of the file.",
        );
    }
    let file_lines: Vec<&str> = text.split('\n').collect();
    let old_lines = lines(old);
    if let Some((start, score)) = closest_window(&file_lines, &old_lines) {
        let end = (start + old_lines.len()).min(file_lines.len());
        let shown_lines: Vec<String> = (start..end)
            .take(15)
            .map(|i| format!("{:>6}\t{}", i + 1, file_lines[i]))
            .collect();
        msg.push_str(&format!(
            "\nClosest match (lines {}-{}, {} similar):\n{}",
            start + 1,
            end,
            similarity_text(score),
            shown_lines.join("\n")
        ));
        if let Some(diff) = first_difference(&old_lines, &file_lines[start..end], start) {
            msg.push_str(&format!("\n{diff}"));
        }
        let offset = start.saturating_sub(3) + 1;
        msg.push_str(&format!(
            "\nRe-read that region (read_file path={shown} offset={offset} limit={}) and copy \
             old_string exactly, without line-number prefixes.",
            old_lines.len() + 6
        ));
    } else {
        msg.push_str(
            " Re-read the file and copy old_string exactly, without line-number prefixes.",
        );
    }
    msg.push_str(" Do not resend the same call.");
    msg
}

/// A similarity as a percentage, never rounded up to 100% unless identical.
fn similarity_text(score: f64) -> String {
    let pct = score * 100.0;
    if score < 1.0 && pct >= 99.95 {
        ">99.9%".into()
    } else if pct >= 99.0 {
        format!("{pct:.1}%")
    } else {
        format!("{pct:.0}%")
    }
}

/// Where `old_lines` first differs from the file window starting at line
/// `start` (0-based): line, column, and both versions around the spot, with
/// invisible and non-ASCII characters escaped so a one-character difference
/// (`';'` vs `";"`, a stray Unicode escape) is visible.
fn first_difference(old_lines: &[&str], window: &[&str], start: usize) -> Option<String> {
    for (i, (o, f)) in old_lines.iter().zip(window.iter()).enumerate() {
        if o == f {
            continue;
        }
        let (oc, fc): (Vec<char>, Vec<char>) = (o.chars().collect(), f.chars().collect());
        let col = oc.iter().zip(fc.iter()).take_while(|(a, b)| a == b).count();
        let around = |cs: &[char]| -> String {
            let from = col.saturating_sub(12);
            let to = (col + 12).min(cs.len());
            cs[from.min(cs.len())..to]
                .iter()
                .map(|c| {
                    if c.is_ascii_graphic() || *c == ' ' {
                        c.to_string()
                    } else {
                        c.escape_unicode().to_string()
                    }
                })
                .collect()
        };
        return Some(format!(
            "First difference: line {}, column {}. old_string has `{}` where the file has `{}`.",
            start + i + 1,
            col + 1,
            around(&oc),
            around(&fc)
        ));
    }
    None
}

/// The window of the file most similar to `old_lines` (average per-line
/// similarity of trimmed lines), if at least `MIN_SIMILARITY`.
fn closest_window(file_lines: &[&str], old_lines: &[&str]) -> Option<(usize, f64)> {
    let k = old_lines.len();
    if k == 0 || k > file_lines.len() || file_lines.len() > MAX_SIMILARITY_LINES {
        return None;
    }
    let file_grams: Vec<Vec<u16>> = file_lines.iter().map(|l| bigrams(l.trim())).collect();
    let old_grams: Vec<Vec<u16>> = old_lines.iter().map(|l| bigrams(l.trim())).collect();
    let mut best: Option<(usize, f64)> = None;
    for i in 0..=file_lines.len() - k {
        let score = (0..k)
            .map(|j| dice(&file_grams[i + j], &old_grams[j]))
            .sum::<f64>()
            / k as f64;
        if best.is_none_or(|(_, b)| score > b) {
            best = Some((i, score));
        }
    }
    best.filter(|&(_, s)| s >= MIN_SIMILARITY)
}

/// Sorted byte bigrams of a line (a single byte or empty line maps to itself).
fn bigrams(s: &str) -> Vec<u16> {
    let b = s.as_bytes();
    let mut v: Vec<u16> = if b.len() < 2 {
        b.iter().map(|&x| u16::from(x)).collect()
    } else {
        b.windows(2)
            .map(|w| (u16::from(w[0]) << 8) | u16::from(w[1]))
            .collect()
    };
    v.sort_unstable();
    v
}

/// Dice coefficient of two sorted bigram multisets (1.0 for two empty lines).
fn dice(a: &[u16], b: &[u16]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let (mut i, mut j, mut common) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Equal => {
                common += 1;
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
        }
    }
    2.0 * common as f64 / (a.len() + b.len()) as f64
}

// ---------------------------------------------------------------------------
// Diffs

/// Changed lines between two versions of a file, as one block from their
/// common prefix and suffix (enough for an approval preview of a rewrite).
pub fn whole_file_blocks(old: &str, new: &str) -> Vec<Block> {
    let (a, b) = (lines(old), lines(new));
    let pre = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suf = a[pre..]
        .iter()
        .rev()
        .zip(b[pre..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    if pre == a.len() && pre == b.len() {
        return Vec::new();
    }
    vec![Block {
        old_start: pre,
        old_len: a.len() - pre - suf,
        new_start: pre,
        new_len: b.len() - pre - suf,
    }]
}

/// Unified diff of `blocks` with `DIFF_CONTEXT` lines of context; nearby
/// blocks share a hunk. Returns the text and the added/removed line counts.
pub fn unified_diff(shown: &str, old: &str, new: &str, blocks: &[Block]) -> (String, usize, usize) {
    let (a, b) = (lines(old), lines(new));
    let refined = refine(&a, &b, &merge_overlapping(blocks));
    let blocks = refined.as_slice();
    let mut out = vec![format!("--- a/{shown}"), format!("+++ b/{shown}")];
    let (mut added, mut removed) = (0, 0);
    let mut i = 0;
    while i < blocks.len() {
        let mut j = i;
        while j + 1 < blocks.len()
            && blocks[j + 1].old_start <= blocks[j].old_start + blocks[j].old_len + 2 * DIFF_CONTEXT
        {
            j += 1;
        }
        let first = &blocks[i];
        let last = &blocks[j];
        let start = first.old_start.saturating_sub(DIFF_CONTEXT);
        let end = (last.old_start + last.old_len + DIFF_CONTEXT).min(a.len());
        let shift = first.new_start as isize - first.old_start as isize;
        let new_start = (start as isize + shift).max(0) as usize;
        let mut body = Vec::new();
        let mut cursor = start;
        let mut new_count = 0;
        for blk in &blocks[i..=j] {
            for line in &a[cursor..blk.old_start] {
                body.push(format!(" {line}"));
                new_count += 1;
            }
            for line in &a[blk.old_start..blk.old_start + blk.old_len] {
                body.push(format!("-{line}"));
                removed += 1;
            }
            for line in &b[blk.new_start..blk.new_start + blk.new_len] {
                body.push(format!("+{line}"));
                added += 1;
                new_count += 1;
            }
            cursor = blk.old_start + blk.old_len;
        }
        for line in &a[cursor..end] {
            body.push(format!(" {line}"));
            new_count += 1;
        }
        out.push(format!(
            "@@ -{},{} +{},{} @@",
            start + 1,
            end - start,
            new_start + 1,
            new_count
        ));
        out.extend(body);
        i = j + 1;
    }
    if out.len() > MAX_DIFF_LINES + 2 {
        let hidden = out.len() - MAX_DIFF_LINES - 2;
        out.truncate(MAX_DIFF_LINES + 2);
        out.push(format!("... {hidden} more diff lines not shown"));
    }
    (out.join("\n"), added, removed)
}

/// Shrinks each block past lines that are identical at its start and end
/// (a replacement often repeats unchanged lines), dropping empty blocks.
/// Sorts blocks and merges those whose old line ranges overlap or touch.
/// `replace_all` with several matches on one line yields one block per match,
/// all for the same line; the diff needs disjoint blocks.
fn merge_overlapping(blocks: &[Block]) -> Vec<Block> {
    let mut sorted = blocks.to_vec();
    sorted.sort_by_key(|b| (b.old_start, b.new_start));
    let mut out: Vec<Block> = Vec::new();
    for b in sorted {
        match out.last_mut() {
            Some(prev) if b.old_start <= prev.old_start + prev.old_len => {
                let old_end = (prev.old_start + prev.old_len).max(b.old_start + b.old_len);
                let new_end = (prev.new_start + prev.new_len).max(b.new_start + b.new_len);
                prev.new_start = prev.new_start.min(b.new_start);
                prev.old_len = old_end - prev.old_start;
                prev.new_len = new_end - prev.new_start;
            }
            _ => out.push(b),
        }
    }
    out
}

fn refine(a: &[&str], b: &[&str], blocks: &[Block]) -> Vec<Block> {
    blocks
        .iter()
        .filter_map(|blk| {
            let mut x = blk.clone();
            while x.old_len > 0 && x.new_len > 0 && a[x.old_start] == b[x.new_start] {
                x.old_start += 1;
                x.new_start += 1;
                x.old_len -= 1;
                x.new_len -= 1;
            }
            while x.old_len > 0
                && x.new_len > 0
                && a[x.old_start + x.old_len - 1] == b[x.new_start + x.new_len - 1]
            {
                x.old_len -= 1;
                x.new_len -= 1;
            }
            (x.old_len > 0 || x.new_len > 0).then_some(x)
        })
        .collect()
}

/// Numbered lines around the first changed block of the new content, so the
/// model can continue without re-reading.
fn snippet(new: &str, block: &Block) -> String {
    let all: Vec<&str> = new.split('\n').collect();
    let start = block.new_start.saturating_sub(3);
    let end = (block.new_start + block.new_len + 3).min(all.len());
    (start..end)
        .take(MAX_SNIPPET_LINES)
        .map(|i| format!("{:>6}\t{}", i + 1, all[i]))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// The tool

fn refuse<T>(message: String) -> Result<T> {
    RefusedSnafu { message }.fail()
}

pub struct EditFile {
    allowed: Vec<String>,
    tracker: Arc<FileTracker>,
}

impl EditFile {
    pub fn new(allowed: Vec<String>, tracker: Arc<FileTracker>) -> Self {
        Self { allowed, tracker }
    }

    /// Validates the call and plans the edit without side effects. Used for
    /// the approval preview and again, against the file as it is then, just
    /// before writing.
    async fn prepare(&self, args: &Value) -> Result<(PathBuf, String, Planned, String)> {
        let text_arg = |key: &str| match args.get(key) {
            Some(Value::String(s)) => Ok(s.clone()),
            Some(Value::Null) | None => refuse(format!(
                "edit_file failed: `{key}` is missing or null. Pass the exact text as a string. \
                 No changes were made."
            )),
            Some(_) => refuse(format!(
                "edit_file failed: `{key}` must be a string. No changes were made."
            )),
        };
        let shown = text_arg("path")?;
        let old = text_arg("old_string")?;
        let new = text_arg("new_string")?;
        let replace_all = args
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let path = ensure_allowed(Path::new(&shown), &self.allowed)?;
        if !path.is_file() {
            return refuse(format!(
                "edit_file failed: {shown} does not exist. Use write_file to create it."
            ));
        }
        let bytes = tokio::fs::read(&path).await.context(FsSnafu {
            path: path.display().to_string(),
        })?;
        let Ok(content) = String::from_utf8(bytes.clone()) else {
            return refuse(format!(
                "edit_file failed: {shown} is not valid UTF-8 text."
            ));
        };
        require_fresh(&self.tracker, "edit_file", &shown, &path, &bytes)?;
        if old.is_empty() {
            return refuse(
                "edit_file failed: old_string is empty. To create or fully rewrite a file, use \
                 write_file."
                    .to_string(),
            );
        }
        if old == new {
            return refuse(
                "edit_file failed: old_string and new_string are identical; nothing to change."
                    .to_string(),
            );
        }
        let planned = match plan_edit(&content, &old, &new, replace_all, &shown) {
            Ok(p) => p,
            Err(message) => return refuse(message),
        };
        Ok((path, content, planned, shown))
    }
}

#[async_trait]
impl BaseTool for EditFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit_file".into(),
            description: "Replace text in an existing file. old_string must match the file \
                exactly (including indentation) and occur exactly once, unless replace_all is \
                true. Copy old_string from read_file output WITHOUT the line-number prefix, and \
                include enough surrounding lines to be unique. Read the file (or the region) \
                first. For several changes, call edit_file several times."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File to edit" },
                    "old_string": { "type": "string", "description": "Exact existing text to replace (no line-number prefixes)" },
                    "new_string": { "type": "string", "description": "Replacement text (must differ from old_string)" },
                    "replace_all": { "type": "boolean", "description": "Replace every occurrence (default false)" }
                },
                "required": ["path", "old_string", "new_string"]
            }),
        }
    }

    async fn preview(&self, args: &Value) -> Result<Option<String>> {
        let (_, content, planned, shown) = self.prepare(args).await?;
        let (diff, added, removed) =
            unified_diff(&shown, &content, &planned.new_content, &planned.blocks);
        let how = planned.note.unwrap_or("exact match");
        Ok(Some(format!(
            "Edit {shown}  (+{added} -{removed}, {how})\n{diff}"
        )))
    }

    #[tracing::instrument(skip_all)]
    async fn execute(&self, args: &Value, _env: &ToolEnv) -> Result<String> {
        let (path, _, planned, shown) = self.prepare(args).await?;
        atomic_write(&path, planned.new_content.as_bytes()).await?;
        self.tracker.record(&path, planned.new_content.as_bytes());

        let first = &planned.blocks[0];
        let mut msg = if planned.count > 1 {
            format!("Edited {shown}: replaced {} occurrences.", planned.count)
        } else {
            format!(
                "Edited {shown}: replaced lines {}-{} with {} lines.",
                first.old_start + 1,
                first.old_start + first.old_len.max(1),
                first.new_len
            )
        };
        if let Some(note) = planned.note {
            msg.push_str(&format!(" Note: {note}."));
        }
        let (_, text) = Encoding::decode(&planned.new_content);
        msg.push_str(&format!("\n{}", snippet(&text, first)));
        Ok(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(content: &str, old: &str, new: &str) -> Result<Planned, String> {
        plan_edit(content, old, new, false, "f.rs")
    }

    #[test]
    fn exact_unique_replacement() {
        let p = plan("a\nb\nc\n", "b\n", "B\nB2\n").unwrap();
        assert_eq!(p.new_content, "a\nB\nB2\nc\n");
        assert_eq!(p.note, None);
        assert_eq!(
            p.blocks,
            vec![Block {
                old_start: 1,
                old_len: 1,
                new_start: 1,
                new_len: 2
            }]
        );
    }

    #[test]
    fn multiple_matches_need_replace_all() {
        let err = plan("x = 1\ny = 2\nx = 1\n", "x = 1", "x = 3").unwrap_err();
        assert!(
            err.contains("occurs 2 times") && err.contains("lines 1, 3"),
            "{err}"
        );
        let p = plan_edit("x = 1\ny = 2\nx = 1\n", "x = 1", "x = 3", true, "f.rs").unwrap();
        assert_eq!(p.new_content, "x = 3\ny = 2\nx = 3\n");
        assert_eq!(p.count, 2);
    }

    #[test]
    fn strips_read_file_line_number_prefixes() {
        let content = "fn a() {\n    let x = 1;\n}\n";
        let old = "     2\t    let x = 1;";
        let new = "     2\t    let x = 2;";
        let p = plan(content, old, new).unwrap();
        assert_eq!(p.new_content, "fn a() {\n    let x = 2;\n}\n");
        assert!(p.note.unwrap().contains("line-number"));
        // A line that merely starts with a number is not a prefix.
        assert!(strip_line_numbers("10 apples").is_none());
    }

    #[test]
    fn tolerates_trailing_newline_and_trailing_whitespace() {
        // File's last line has no newline; old_string has one.
        let p = plan("a\nlast", "last\n", "LAST\n").unwrap();
        assert_eq!(p.new_content, "a\nLAST");
        // Trailing spaces in the file that old_string lacks.
        let p = plan(
            "fn a() {   \n    x();  \n}\n",
            "fn a() {\n    x();\n",
            "fn a() {\n    y();\n",
        )
        .unwrap();
        assert_eq!(p.new_content, "fn a() {\n    y();\n}\n");
        assert!(p.note.unwrap().contains("trailing whitespace"));
    }

    #[test]
    fn tolerates_a_uniform_indentation_difference() {
        let content = "/* Multiply */\nint mul(int a, int b) {\n    return a + b;\n}\n";
        // The model indented its whole snippet by four extra spaces (seen live).
        let old = "    /* Multiply */\n    int mul(int a, int b) {\n        return a + b;\n    }";
        let new = "    /* Multiply */\n    int mul(int a, int b) {\n        return a * b;\n    }";
        let p = plan(content, old, new).unwrap();
        assert_eq!(
            p.new_content,
            "/* Multiply */\nint mul(int a, int b) {\n    return a * b;\n}\n"
        );
        assert!(p.note.unwrap().contains("indentation"));
        // Dropped indentation is restored in new_string.
        let content = "def f():\n    if x:\n        return 1\n";
        let p = plan(content, "if x:\n    return 1", "if x:\n    return 2").unwrap();
        assert_eq!(p.new_content, "def f():\n    if x:\n        return 2\n");
        // Inconsistent offsets are not guessed at.
        assert!(plan("a:\n    b\n", "  a:\n  b", "x").is_err());
    }

    #[test]
    fn replace_all_with_two_matches_on_one_line_diffs_cleanly() {
        // Regression: two matches on the same line gave two blocks for that
        // line, and the diff sliced backwards and panicked.
        let old = "# Guide\n\nClients recieve a token. Then they recieve updates.\nend\n";
        let p = plan_edit(old, "recieve", "receive", true, "docs/guide.md").unwrap();
        assert_eq!(p.count, 2);
        let (diff, added, removed) = unified_diff("docs/guide.md", old, &p.new_content, &p.blocks);
        assert_eq!((added, removed), (1, 1), "{diff}");
        assert!(
            diff.contains("+Clients receive a token. Then they receive updates."),
            "{diff}"
        );
    }

    #[test]
    fn near_miss_names_the_first_difference() {
        let content = "fn f() {\n    let s = \";\";\n    g(s);\n}\n";
        let err = plan(content, "    let s = ';';\n    g(s);\n", "x").unwrap_err();
        assert!(err.contains("First difference: line 2, column 13"), "{err}");
        assert!(
            err.contains("`    let s = ';';`") && err.contains("`    let s = \";\";`"),
            "{err}"
        );
        assert!(!err.contains("100% similar"), "{err}");
        assert_eq!(similarity_text(0.9996), ">99.9%");
        assert_eq!(similarity_text(1.0), "100.0%");
        assert_eq!(similarity_text(0.873), "87%");
    }

    #[test]
    fn diff_omits_unchanged_lines_inside_a_replacement() {
        let old = "/* m */\nint mul() {\n    return a + b;\n}\n";
        let p = plan(
            old,
            "/* m */\nint mul() {\n    return a + b;\n}",
            "/* m */\nint mul() {\n    return a * b;\n}",
        )
        .unwrap();
        let (diff, added, removed) = unified_diff("c.c", old, &p.new_content, &p.blocks);
        assert_eq!((added, removed), (1, 1), "{diff}");
        assert!(
            diff.contains("-    return a + b;\n+    return a * b;"),
            "{diff}"
        );
    }

    #[test]
    fn keeps_crlf_and_bom() {
        let p = plan("\u{feff}a\r\nb\r\n", "b\n", "c\n").unwrap();
        assert_eq!(p.new_content, "\u{feff}a\r\nc\r\n");
        // Mixed endings are not rewritten wholesale.
        let p = plan("a\r\nb\nc\n", "c", "d").unwrap();
        assert_eq!(p.new_content, "a\r\nb\nd\n");
    }

    #[test]
    fn not_found_shows_closest_match_and_hints() {
        let content = "fn main() {\n    let port = cfg.port.unwrap_or(8080);\n    run(port);\n}\n";
        let err = plan(
            content,
            "    let port = cfg.port.unwrap_or(80);\n    run(port);",
            "x",
        )
        .unwrap_err();
        assert!(
            err.contains("was not found in f.rs. No changes were made."),
            "{err}"
        );
        assert!(err.contains("Closest match (lines 2-3"), "{err}");
        assert!(err.contains("offset=1"), "{err}");
        assert!(err.contains("Do not resend the same call."), "{err}");

        let err = plan("let a = 2;\n", "let a = 1;", "let a = 2;").unwrap_err();
        assert!(err.contains("already present"), "{err}");
    }

    #[test]
    fn no_similarity_based_replacement() {
        // A near miss is reported, never applied.
        assert!(plan("let value = compute();\n", "let valeu = compute();", "x").is_err());
    }

    #[test]
    fn diff_shows_changes_with_context() {
        let old = "1\n2\n3\n4\n5\n6\n7\n8\n";
        let p = plan(old, "5\n", "five\n").unwrap();
        let (diff, added, removed) = unified_diff("f.rs", old, &p.new_content, &p.blocks);
        assert_eq!((added, removed), (1, 1));
        assert_eq!(
            diff,
            "--- a/f.rs\n+++ b/f.rs\n@@ -2,7 +2,7 @@\n 2\n 3\n 4\n-5\n+five\n 6\n 7\n 8"
        );
    }

    #[test]
    fn diff_for_deletion_and_join() {
        let p = plan("a\nb\nc\n", "b\n", "").unwrap();
        assert_eq!(
            p.blocks,
            vec![Block {
                old_start: 1,
                old_len: 1,
                new_start: 1,
                new_len: 0
            }]
        );
        // Whole line replaced by text without a newline joins the next line.
        let p = plan("a\nb\nc\n", "b\n", "x").unwrap();
        assert_eq!(p.new_content, "a\nxc\n");
        assert_eq!(
            p.blocks,
            vec![Block {
                old_start: 1,
                old_len: 2,
                new_start: 1,
                new_len: 1
            }]
        );
    }

    #[test]
    fn whole_file_blocks_find_the_changed_middle() {
        assert_eq!(
            whole_file_blocks("a\nb\nc\n", "a\nX\nY\nc\n"),
            vec![Block {
                old_start: 1,
                old_len: 1,
                new_start: 1,
                new_len: 2
            }]
        );
        assert!(whole_file_blocks("same\n", "same\n").is_empty());
    }

    #[test]
    fn tracker_detects_unread_fresh_and_stale() {
        let t = FileTracker::default();
        let p = Path::new("/x");
        assert_eq!(t.check(p, b"v1"), Freshness::Unread);
        t.record(p, b"v1");
        assert_eq!(t.check(p, b"v1"), Freshness::Fresh);
        assert_eq!(t.check(p, b"v2"), Freshness::Stale);
        t.clear();
        assert_eq!(t.check(p, b"v1"), Freshness::Unread);
    }
}
