//! What evicted steps read, changed and ran, built by code from their tool
//! calls (docs/design/design-context-management.md). It stands in for the
//! steps in the compaction record and merges across compactions.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::context::{Kind, Message, is_failure};

/// Entries kept per list; older ones are dropped and counted.
const MAX_ENTRIES: usize = 200;
/// Line ranges shown per file read.
const MAX_RANGES: usize = 8;
const MAX_COMMAND_CHARS: usize = 120;
const MAX_ERROR_CHARS: usize = 160;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Digest {
    /// Files and directories read, oldest first, each once.
    #[serde(default)]
    pub read: Vec<Read>,
    /// Searches (`find_files`, `search_files`), oldest first.
    #[serde(default)]
    pub searches: Vec<String>,
    /// Files written or edited, with the number of successful changes.
    #[serde(default)]
    pub changed: Vec<Changed>,
    /// Shell commands with their exit status, oldest first.
    #[serde(default)]
    pub commands: Vec<String>,
    /// First lines of failed tool results, oldest first.
    #[serde(default)]
    pub errors: Vec<String>,
    /// Tool calls covered.
    #[serde(default)]
    pub calls: u64,
    /// Operator requests removed.
    #[serde(default)]
    pub requests_removed: u64,
    /// Entries dropped from the lists above to bound their size.
    #[serde(default)]
    pub dropped: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Changed {
    pub path: String,
    pub count: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct Read {
    pub path: String,
    /// Line ranges read (first, last), merged; empty means the whole file.
    #[serde(default)]
    pub lines: Vec<[u64; 2]>,
}

impl Read {
    fn add(&mut self, range: Option<[u64; 2]>) {
        let Some(r) = range else {
            self.lines.clear();
            return;
        };
        if self.lines.is_empty() {
            return; // the whole file was read already
        }
        self.lines.push(r);
        self.lines.sort_unstable();
        let mut merged: Vec<[u64; 2]> = Vec::with_capacity(self.lines.len());
        for r in self.lines.drain(..) {
            match merged.last_mut() {
                Some(last) if r[0] <= last[1].saturating_add(1) => last[1] = last[1].max(r[1]),
                _ => merged.push(r),
            }
        }
        self.lines = merged;
    }

    fn render(&self) -> String {
        if self.lines.is_empty() {
            return self.path.clone();
        }
        let mut ranges: Vec<String> = self
            .lines
            .iter()
            .take(MAX_RANGES)
            .map(|[a, b]| match *b {
                u64::MAX => format!("{a}-end"),
                b if b == *a => a.to_string(),
                b => format!("{a}-{b}"),
            })
            .collect();
        if self.lines.len() > MAX_RANGES {
            ranges.push(format!("{} more ranges", self.lines.len() - MAX_RANGES));
        }
        format!("{} (lines {})", self.path, ranges.join(", "))
    }
}

fn clip(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or_default();
    if line.chars().count() > max || s.lines().nth(1).is_some() {
        format!("{}...", line.chars().take(max).collect::<String>())
    } else {
        line.to_string()
    }
}

/// The first line of a failed result, from its text or its placeholder.
fn failure(m: &Message) -> Option<String> {
    let content = m.content.as_deref()?;
    if m.masked {
        let (_, rest) = content.split_once(" It failed: ")?;
        let first = rest.split(". Re-run the tool").next().unwrap_or(rest);
        return Some(first.to_string());
    }
    is_failure(content).then(|| {
        content
            .lines()
            .find(|l| !l.starts_with("exit: ") && !l.trim().is_empty() && *l != "STDOUT:")
            .unwrap_or(content.lines().next().unwrap_or_default())
            .to_string()
    })
}

fn push_unique(list: &mut Vec<String>, item: String) {
    if !list.contains(&item) {
        list.push(item);
    }
}

impl Digest {
    /// Folds in the tool calls of evicted messages.
    pub fn add(&mut self, removed: &[Message]) {
        self.requests_removed += removed.iter().filter(|m| m.kind == Kind::Request).count() as u64;
        for m in removed {
            let Some(calls) = m.tool_calls.as_ref().and_then(Value::as_array) else {
                continue;
            };
            for c in calls {
                let id = c["id"].as_str().unwrap_or_default();
                let name = c["function"]["name"].as_str().unwrap_or("tool");
                let args: Value = c["function"]["arguments"]
                    .as_str()
                    .and_then(|a| serde_json::from_str(a).ok())
                    .unwrap_or(Value::Null);
                let result = removed
                    .iter()
                    .find(|r| r.role == "tool" && r.tool_call_id.as_deref() == Some(id));
                self.add_call(name, &args, result);
            }
        }
        self.bound();
    }

    fn add_call(&mut self, name: &str, args: &Value, result: Option<&Message>) {
        self.calls += 1;
        let arg = |k: &str| args.get(k).and_then(Value::as_str);
        let ran = result.is_some_and(|r| !r.synthetic);
        let failed = result.and_then(failure);
        match name {
            "read_file" | "list_dir" => {
                let path = arg("path").unwrap_or(".").to_string();
                let num = |k: &str| args.get(k).and_then(Value::as_u64);
                let range = num("offset").map(|o| {
                    let last = num("limit").map_or(u64::MAX, |l| o + l.max(1) - 1);
                    [o, last]
                });
                match self.read.iter_mut().find(|r| r.path == path) {
                    Some(r) => r.add(range),
                    None => self.read.push(Read {
                        path,
                        lines: range.into_iter().collect(),
                    }),
                }
            }
            "find_files" | "search_files" => {
                let item = format!(
                    "{name} `{}` in {}",
                    arg("pattern").unwrap_or_default(),
                    arg("path").unwrap_or(".")
                );
                push_unique(&mut self.searches, item);
            }
            "write_file" | "edit_file" if ran && failed.is_none() => {
                let path = arg("path").unwrap_or("?").to_string();
                match self.changed.iter_mut().find(|c| c.path == path) {
                    Some(c) => c.count += 1,
                    None => self.changed.push(Changed { path, count: 1 }),
                }
            }
            "execute_bash" => {
                let cmd = clip(arg("command").unwrap_or_default(), MAX_COMMAND_CHARS);
                let content = result.and_then(|r| r.content.as_deref()).unwrap_or("");
                let status = if !ran {
                    "not run".to_string()
                } else if let Some(code) = content
                    .strip_prefix("exit: ")
                    .and_then(|r| r.lines().next())
                {
                    format!("exit {code}")
                } else if failed.is_some() {
                    "failed".to_string()
                } else {
                    "ran".to_string()
                };
                self.commands.push(format!("`{cmd}` {status}"));
            }
            _ => {}
        }
        if let Some(f) = failed {
            self.errors
                .push(format!("{name}: {}", clip(&f, MAX_ERROR_CHARS)));
        }
    }

    fn bound(&mut self) {
        let mut dropped = 0;
        for list in [&mut self.searches, &mut self.commands, &mut self.errors] {
            if list.len() > MAX_ENTRIES {
                let n = list.len() - MAX_ENTRIES;
                list.drain(..n);
                dropped += n;
            }
        }
        if self.read.len() > MAX_ENTRIES {
            let n = self.read.len() - MAX_ENTRIES;
            self.read.drain(..n);
            dropped += n;
        }
        if self.changed.len() > MAX_ENTRIES {
            let n = self.changed.len() - MAX_ENTRIES;
            self.changed.drain(..n);
            dropped += n;
        }
        self.dropped += dropped as u64;
    }

    /// The digest as text of at most `max_chars`, dropping the oldest
    /// entries of the longest list first (each list notes how many).
    pub fn render(&self, max_chars: usize) -> String {
        let changed: Vec<String> = self
            .changed
            .iter()
            .map(|c| match c.count {
                1 => c.path.clone(),
                n => format!("{} ({n} changes)", c.path),
            })
            .collect();
        let mut lists: [(&str, &str, Vec<String>, usize); 5] = [
            ("Files changed", ", ", changed, 0),
            (
                "Files read",
                ", ",
                self.read.iter().map(Read::render).collect(),
                0,
            ),
            ("Searches", "; ", self.searches.clone(), 0),
            ("Commands", "; ", self.commands.clone(), 0),
            ("Errors", "; ", self.errors.clone(), 0),
        ];
        let text = |lists: &[(&str, &str, Vec<String>, usize)]| {
            let mut s = String::new();
            for (label, sep, items, skipped) in lists {
                if items.is_empty() && *skipped == 0 {
                    continue;
                }
                s.push_str(label);
                s.push_str(": ");
                if *skipped > 0 {
                    s.push_str(&format!("({skipped} earlier not shown) "));
                }
                s.push_str(&items.join(sep));
                s.push('\n');
            }
            s.push_str(&format!("Tool calls removed: {}", self.calls));
            s
        };
        loop {
            let s = text(&lists);
            if s.len() <= max_chars {
                return s;
            }
            let longest = lists
                .iter_mut()
                .filter(|l| l.2.len() > 1)
                .max_by_key(|l| l.2.iter().map(String::len).sum::<usize>());
            match longest {
                Some(l) => {
                    l.2.remove(0);
                    l.3 += 1;
                }
                None => {
                    let mut end = max_chars.min(s.len());
                    while !s.is_char_boundary(end) {
                        end -= 1;
                    }
                    return s[..end].to_string();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::CompletionResponse;
    use crate::tools::ToolCall;
    use serde_json::json;

    fn step(id: &str, name: &str, args: Value, result: &str) -> Vec<Message> {
        let r = CompletionResponse {
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: id.into(),
                name: name.into(),
                args,
            }]),
            usage: None,
            finish_reason: None,
            reasoning: None,
            request_sha256: None,
            response_id: None,
            served_model: None,
        };
        vec![
            Message::assistant_tool_calls(&r),
            Message::tool_result(id, result),
        ]
    }

    #[test]
    fn collects_reads_changes_commands_and_errors() {
        let mut removed = Vec::new();
        removed.extend(step("1", "read_file", json!({"path": "src/a.rs"}), "1| x"));
        removed.extend(step(
            "2",
            "read_file",
            json!({"path": "src/b.rs", "offset": 10, "limit": 5}),
            "10| y",
        ));
        removed.extend(step(
            "3",
            "edit_file",
            json!({"path": "src/a.rs"}),
            "Edited",
        ));
        removed.extend(step(
            "4",
            "edit_file",
            json!({"path": "src/a.rs"}),
            "Edited",
        ));
        removed.extend(step(
            "5",
            "edit_file",
            json!({"path": "src/c.rs"}),
            "Error: old_string not found",
        ));
        removed.extend(step(
            "6",
            "execute_bash",
            json!({"command": "cargo test"}),
            "exit: 101\nSTDOUT:\nthread panicked\nSTDERR:\n",
        ));
        removed.extend(step(
            "7",
            "search_files",
            json!({"pattern": "fn main", "path": "src"}),
            "src/main.rs:1",
        ));
        let mut d = Digest::default();
        d.add(&removed);
        let read: Vec<String> = d.read.iter().map(Read::render).collect();
        assert_eq!(read, vec!["src/a.rs", "src/b.rs (lines 10-14)"]);
        assert_eq!(
            d.changed,
            vec![Changed {
                path: "src/a.rs".into(),
                count: 2
            }]
        );
        assert_eq!(d.commands, vec!["`cargo test` exit 101"]);
        assert_eq!(d.searches, vec!["search_files `fn main` in src"]);
        assert_eq!(d.errors.len(), 2, "{:?}", d.errors);
        assert!(d.errors[0].contains("old_string not found"));
        assert!(d.errors[1].contains("thread panicked"));
        assert_eq!(d.calls, 7);

        let text = d.render(10_000);
        assert!(
            text.contains("Files changed: src/a.rs (2 changes)"),
            "{text}"
        );
        assert!(text.contains("Tool calls removed: 7"), "{text}");
    }

    #[test]
    fn masked_failures_and_unrun_calls() {
        let mut removed = step(
            "1",
            "execute_bash",
            json!({"command": "make"}),
            "[output elided to save context: execute_bash {}, 900 bytes. It failed: exit: 2. \
             Re-run the tool if you need it again.]",
        );
        removed[1].masked = true;
        let mut unrun = step(
            "2",
            "execute_bash",
            json!({"command": "rm -rf x"}),
            "Not executed",
        );
        unrun[1].synthetic = true;
        removed.extend(unrun);
        let mut d = Digest::default();
        d.add(&removed);
        assert_eq!(d.commands, vec!["`make` failed", "`rm -rf x` not run"]);
        assert_eq!(d.errors, vec!["execute_bash: exit: 2"]);
    }

    #[test]
    fn paged_reads_merge_into_ranges() {
        let mut removed = Vec::new();
        for (i, (o, l)) in [(1, 20), (21, 20), (100, 5), (50, 10), (41, 9)]
            .into_iter()
            .enumerate()
        {
            removed.extend(step(
                &i.to_string(),
                "read_file",
                json!({"path": "a.txt", "offset": o, "limit": l}),
                "x",
            ));
        }
        removed.extend(step("w", "read_file", json!({"path": "b.txt"}), "x"));
        removed.extend(step(
            "x",
            "read_file",
            json!({"path": "b.txt", "offset": 3}),
            "x",
        ));
        let mut d = Digest::default();
        d.add(&removed);
        let read: Vec<String> = d.read.iter().map(Read::render).collect();
        assert_eq!(read, vec!["a.txt (lines 1-59, 100-104)", "b.txt"]);
    }

    #[test]
    fn render_drops_oldest_entries_of_the_longest_list() {
        let mut d = Digest::default();
        for i in 0..50 {
            d.read.push(Read {
                path: format!("src/file_{i}.rs"),
                lines: Vec::new(),
            });
        }
        d.commands.push("`cargo build` exit 0".into());
        let text = d.render(300);
        assert!(text.len() <= 300, "{}", text.len());
        assert!(text.contains("src/file_49.rs"), "newest kept: {text}");
        assert!(!text.contains("src/file_0.rs,"), "oldest dropped: {text}");
        assert!(text.contains("earlier not shown"));
        assert!(text.contains("`cargo build` exit 0"));
    }

    #[test]
    fn lists_stay_bounded() {
        let mut d = Digest::default();
        let mut removed = Vec::new();
        for i in 0..(MAX_ENTRIES + 10) {
            removed.extend(step(
                &i.to_string(),
                "execute_bash",
                json!({"command": format!("echo {i}")}),
                "exit: 0\nSTDOUT:\n",
            ));
        }
        d.add(&removed);
        assert_eq!(d.commands.len(), MAX_ENTRIES);
        assert_eq!(d.dropped, 10);
    }
}
