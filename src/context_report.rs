//! `/context-status`: what fills the context and against which limit
//! (docs/design/design-context-reporting.md). `AgentContext::report` builds
//! the data; this module renders it.

use crate::budget::Budget;
use crate::presenter::escape_controls;

/// Where the context window came from, as shown after `Window W`.
pub const WINDOW_CONFIGURED: &str = "configured";
pub const WINDOW_SERVER: &str = "from server";
pub const WINDOW_FALLBACK: &str = "assumed";
/// Width hints are wrapped to.
const HINT_WIDTH: usize = 80;

/// The most recent compaction event.
#[derive(Debug, Clone)]
pub struct LastCompaction {
    pub turn: u64,
    pub stage: &'static str,
    pub reason: String,
    pub before: usize,
    pub after: usize,
}

/// Report categories, in display order.
pub const SYSTEM: usize = 0;
pub const INSTRUCTIONS: usize = 1;
pub const TOOLS: usize = 2;
pub const REQUESTS: usize = 3;
pub const ASSISTANT: usize = 4;
pub const OUTPUTS_FULL: usize = 5;
pub const OUTPUTS_MASKED: usize = 6;
pub const RECORDS: usize = 7;
pub const NOTICES: usize = 8;
pub const CATEGORY_NAMES: [&str; 9] = [
    "System prompt",
    "Project instructions",
    "Tool definitions",
    "Operator requests",
    "Assistant text and tool calls",
    "Tool outputs, full",
    "Tool outputs, masked",
    "Compaction records",
    "mima notices",
];

#[derive(Debug, Clone)]
pub struct Category {
    pub name: &'static str,
    pub tokens: usize,
    pub items: String,
}

/// One message in the context.
#[derive(Debug, Clone)]
pub struct Row {
    pub index: usize,
    pub role: String,
    /// full, capped, masked, noted, synthetic, notice, record
    pub view: &'static str,
    pub category: usize,
    pub tokens: usize,
    pub what: String,
}

#[derive(Debug, Clone)]
pub struct ContextReport {
    pub session: String,
    pub turn: u64,
    pub model: String,
    pub budget: Budget,
    /// One of the `WINDOW_*` labels.
    pub window_source: &'static str,
    /// `[context].budget` lowers E below U.
    pub budget_configured: bool,
    pub total: usize,
    pub source: &'static str,
    pub categories: Vec<Category>,
    pub rows: Vec<Row>,
    /// Tool definitions as (name, tokens).
    pub tools: Vec<(String, usize)>,
    pub mask_at: f64,
    pub mask_to: f64,
    pub keep_recent: f64,
    pub output_cap: usize,
    pub masked_total: usize,
    pub mask_events: usize,
    pub evicted_total: usize,
    /// Compaction records written this session.
    pub records: usize,
    pub last: Option<LastCompaction>,
}

/// Splits `total` in proportion to `weights` (largest remainder), so the
/// parts always add up to `total` when any weight is non-zero.
pub fn allocate(total: usize, weights: &[usize]) -> Vec<usize> {
    let sum: u128 = weights.iter().map(|&w| w as u128).sum();
    if sum == 0 {
        return vec![0; weights.len()];
    }
    let share = |w: usize| w as u128 * total as u128;
    let mut out: Vec<usize> = weights.iter().map(|&w| (share(w) / sum) as usize).collect();
    let mut rest = total - out.iter().sum::<usize>();
    let mut order: Vec<usize> = (0..weights.len()).filter(|&i| weights[i] > 0).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(share(weights[i]) % sum));
    for i in order {
        if rest == 0 {
            break;
        }
        out[i] += 1;
        rest -= 1;
    }
    out
}

/// `n` with thousands separators.
pub fn num(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Rounded percentage of `part` in `whole`.
fn pct(part: usize, whole: usize) -> usize {
    (part * 100 + whole / 2) / whole.max(1)
}

/// First non-blank line of `text`, at most `max` characters, with a marker
/// when cut, and control characters escaped: it is printed to the terminal.
pub fn snippet(text: &str, max: usize) -> String {
    let text = text.trim_start();
    let line = text.lines().next().unwrap_or_default();
    let cut: String = line.chars().take(max).collect();
    let cut = escape_controls(&cut);
    if line.chars().count() > max || text.lines().nth(1).is_some() {
        format!("{cut}...")
    } else {
        cut
    }
}

/// Model-, tool- or file-supplied text made safe to print.
pub fn safe(text: &str) -> String {
    escape_controls(text)
}

/// `text` after `lead`, wrapped at word boundaries to `width` columns, with
/// continuation lines indented to match.
fn wrap(text: &str, lead: &str, width: usize) -> String {
    let indent = " ".repeat(lead.chars().count());
    let mut out = String::from(lead);
    let mut col = lead.chars().count();
    for (i, word) in text.split_whitespace().enumerate() {
        let len = word.chars().count();
        if i > 0 {
            if col + 1 + len > width {
                out.push('\n');
                out.push_str(&indent);
                col = indent.len();
            } else {
                out.push(' ');
                col += 1;
            }
        }
        out.push_str(word);
        col += len;
    }
    out
}

/// A tool call as `name` plus its command, pattern or path when it has one,
/// else its raw arguments.
pub fn describe_call(name: &str, args: &str) -> String {
    let parsed: serde_json::Value = serde_json::from_str(args).unwrap_or_default();
    match ["command", "pattern", "path"]
        .iter()
        .find_map(|k| parsed.get(*k).and_then(serde_json::Value::as_str))
    {
        Some(v) => safe(&format!("{name} {v}")),
        None => safe(&format!("{name} {args}")),
    }
}

/// The count source in words.
fn counted(source: &str) -> &'static str {
    match source {
        "tokenize" => "exact: server /tokenize",
        "usage" => "exact: token usage the server reported for the last request",
        "tokenize+estimate" => {
            "server /tokenize for the last request, plus an estimate for changes since"
        }
        "usage+estimate" => {
            "server-reported usage for the last request, plus an estimate for changes since"
        }
        "default-ratio" => "estimate only: nothing sent to the model yet (3 characters per token)",
        _ => "unknown",
    }
}

impl ContextReport {
    fn tokens(&self, category: usize) -> usize {
        self.categories[category].tokens
    }

    /// At most two fixed-rule hints, each naming what helps.
    pub fn hints(&self) -> Vec<String> {
        let b = self.budget;
        let mask_at = b.of(self.mask_at);
        let mut hints = Vec::new();
        if self.window_source == WINDOW_FALLBACK {
            hints.push(format!(
                "The server did not report the context window, so {} is assumed; set \
                 [context].window to the model's real window.",
                num(b.window)
            ));
        }
        if b.window < 16_384 {
            hints.push(format!(
                "The window is small ({}); serve the model with 32k or more.",
                num(b.window)
            ));
        }
        let instructions = self.tokens(INSTRUCTIONS);
        if instructions * 20 > b.operating {
            hints.push(format!(
                "Project instructions take {}% of E; shorter instruction files leave more room.",
                pct(instructions, b.operating)
            ));
        }
        if self.tokens(OUTPUTS_FULL) * 2 > self.total && self.total * 10 > mask_at * 8 {
            hints.push(format!(
                "Tool outputs are most of the context; masking starts at {}.",
                num(mask_at)
            ));
        }
        if self.source == "default-ratio" {
            hints.push(
                "Nothing has been sent to the model yet; counts are rough, deliberately \
                 high estimates until the first reply."
                    .into(),
            );
        }
        if self.records >= 3 {
            hints.push(format!(
                "The session has been compacted {} times; /new may work better.",
                self.records
            ));
        }
        hints.truncate(2);
        hints
    }

    pub fn render(&self, detail: bool) -> String {
        if detail {
            return self.render_messages();
        }
        let b = self.budget;
        let mask_at = b.of(self.mask_at);
        let mut s = format!(
            "Context: session {}, turn {}, model {}\n\n",
            self.session,
            self.turn,
            safe(&self.model)
        );
        s += &format!(
            "  Window W {} ({}) = usable U {} + reply reserve {} + safety margin {}\n",
            num(b.window),
            self.window_source,
            num(b.usable),
            num(b.reserve),
            num(b.margin)
        );
        s += &if self.budget_configured {
            format!(
                "  Budget E {} (set by [context].budget)\n",
                num(b.operating)
            )
        } else {
            format!(
                "  Budget E {} (= U; set [context].budget to run lower)\n",
                num(b.operating)
            )
        };
        s += &format!(
            "  In use   {} tokens: {}% of E, {}% of W\n",
            num(self.total),
            pct(self.total, b.operating),
            pct(self.total, b.window)
        );
        let filled = (pct(self.total, b.operating) * 40 / 100).min(40);
        s += &format!(
            "           [{}{}]  masking starts at {}% of E\n",
            "#".repeat(filled),
            "-".repeat(40 - filled),
            pct(mask_at, b.operating)
        );
        s += &format!("  Counted  {}\n\n", counted(self.source));

        s += &format!(
            "  {:<32} {:>8}  {:>5}  Items\n",
            "Category", "~Tokens", "Share"
        );
        for c in &self.categories {
            s += &format!(
                "  {:<32} {:>8}  {:>4}%  {}\n",
                c.name,
                num(c.tokens),
                pct(c.tokens, self.total),
                c.items
            );
        }
        s += "  Category values are estimates, scaled so they add up to the total.\n\n";

        s += "  Room\n";
        if self.total <= mask_at {
            s += &format!(
                "    until masking ({}% of E) {:>12}\n",
                pct(mask_at, b.operating),
                num(mask_at - self.total)
            );
        } else {
            s += &format!(
                "    over the masking threshold by {}; masking runs before the next request\n",
                num(self.total - mask_at)
            );
        }
        s += &format!(
            "    in budget E {:>25}\n\n",
            num(b.operating.saturating_sub(self.total))
        );

        let mut largest: Vec<&Row> = self.rows.iter().filter(|r| r.index > 0).collect();
        largest.sort_by_key(|r| std::cmp::Reverse(r.tokens));
        if !largest.is_empty() {
            s += "  Largest items\n";
            for r in largest.iter().take(3) {
                let kind = match r.category {
                    OUTPUTS_FULL | OUTPUTS_MASKED => "tool output",
                    ASSISTANT => "assistant",
                    REQUESTS => "request",
                    RECORDS => "record",
                    _ => "notice",
                };
                let view = if r.view == "full" {
                    String::new()
                } else {
                    format!(" ({})", r.view)
                };
                s += &format!("    {:>6}  {kind}  {}{view}\n", num(r.tokens), r.what);
            }
            s += "\n";
        }

        s += "  Compaction\n";
        s += &format!(
            "    Thresholds: mask at {} ({}% of E) down to {} ({}%); newest output\n      \
             protected up to {} ({}%); each tool output capped at {}\n",
            num(mask_at),
            pct(mask_at, b.operating),
            num(b.of(self.mask_to)),
            pct(b.of(self.mask_to), b.operating),
            num(b.of(self.keep_recent)),
            pct(b.of(self.keep_recent), b.operating),
            num(self.output_cap)
        );
        s += &format!(
            "    So far: {} outputs masked in {} events; {} messages evicted; {} compaction records\n",
            num(self.masked_total),
            num(self.mask_events),
            num(self.evicted_total),
            num(self.records)
        );
        s += &match &self.last {
            Some(l) => format!(
                "    Last: turn {}, {} ({}): {} -> {} tokens\n",
                l.turn,
                l.stage,
                l.reason,
                num(l.before),
                num(l.after)
            ),
            None => "    Last: none\n".into(),
        };
        for h in self.hints() {
            s += "\n";
            s += &wrap(&h, "  Hint: ", HINT_WIDTH);
        }
        s.trim_end().to_string()
    }

    fn render_messages(&self) -> String {
        let mut s = format!(
            "  {:>3}  {:<9}  {:<9}  {:>8}  content\n",
            "#", "role", "view", "~tokens"
        );
        for r in &self.rows {
            s += &format!(
                "  {:>3}  {:<9}  {:<9}  {:>8}  {}\n",
                r.index,
                r.role,
                r.view,
                num(r.tokens),
                r.what
            );
        }
        s += &format!(
            "  Tool definitions (sent with every request): {} across {} tools\n    ",
            num(self.tokens(TOOLS)),
            self.tools.len()
        );
        let tools: Vec<String> = self
            .tools
            .iter()
            .map(|(name, t)| format!("{name} {}", num(*t)))
            .collect();
        s += &tools.join(", ");
        s += &format!(
            "\n  Total {} tokens ({}); per-message values are estimates.",
            num(self.total),
            counted(self.source)
        );
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocate_adds_up_and_follows_weights() {
        assert_eq!(allocate(10, &[1, 1, 1]).iter().sum::<usize>(), 10);
        assert_eq!(allocate(100, &[3, 1]), vec![75, 25]);
        assert_eq!(allocate(7, &[0, 5, 0]), vec![0, 7, 0]);
        assert_eq!(allocate(5, &[0, 0]), vec![0, 0]);
        let parts = allocate(14_212, &[1_180, 410, 1_960, 320, 2_340, 7_190, 430, 1]);
        assert_eq!(parts.iter().sum::<usize>(), 14_212);
    }

    #[test]
    fn formats_numbers_and_snippets() {
        assert_eq!(num(0), "0");
        assert_eq!(num(999), "999");
        assert_eq!(num(27_689), "27,689");
        assert_eq!(num(1_234_567), "1,234,567");
        assert_eq!(snippet("short", 10), "short");
        assert_eq!(snippet("a longer line", 4), "a lo...");
        assert_eq!(snippet("one\ntwo", 10), "one...");
        assert_eq!(snippet("\n\nI fixed it", 20), "I fixed it");
        assert_eq!(snippet("ls\u{1b}[2J", 20), "ls\\u{1b}[2J");
        assert_eq!(
            describe_call("execute_bash", r#"{"command":"ls\u001b[2J"}"#),
            "execute_bash ls\\u{1b}[2J"
        );
        assert_eq!(
            describe_call("edit_file", r#"{"new_string":"x","path":"a.c"}"#),
            "edit_file a.c"
        );
        assert_eq!(
            describe_call("execute_bash", r#"{"command":"make"}"#),
            "execute_bash make"
        );
        assert_eq!(describe_call("x", "{}"), "x {}");
        assert_eq!(wrap("aaa bbb ccc", "> ", 9), "> aaa bbb\n  ccc");
    }

    #[test]
    fn every_count_source_has_wording() {
        for s in [
            "tokenize",
            "usage",
            "tokenize+estimate",
            "usage+estimate",
            "default-ratio",
        ] {
            assert_ne!(counted(s), "unknown", "{s}");
        }
    }
}
