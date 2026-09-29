//! Presentation decoupling: the agent core emits typed events and collects
//! decisions through a `Presenter`, so the same loop can drive a plain CLI, a
//! TUI, or a REST front end without touching core logic (see `REQ-UI-*`).

use std::io::{self, BufRead, IsTerminal, Write};

use crate::tools::ToolCall;

/// An operator decision for a state-changing action gated by approval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Approval {
    Allow,
    Deny,
}

/// Interprets a line of user input as an approval decision: `y`/`yes` allow
/// and `n`/`no` deny, in any mix of case; an empty answer takes the default,
/// deny. Anything else is `None` (not understood; ask again).
pub fn parse_approval(input: &str) -> Option<Approval> {
    match input.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Some(Approval::Allow),
        "n" | "no" | "" => Some(Approval::Deny),
        _ => None,
    }
}

/// What the operator is being asked to approve, in plain terms: the full
/// shell command, or the file and size for a write.
pub fn describe_action(call: &ToolCall) -> String {
    let arg = |key: &str| call.args.get(key).and_then(|v| v.as_str()).unwrap_or("");
    match call.name.as_str() {
        "execute_bash" => format!("Run shell command:\n  {}", display_safe(arg("command"))),
        "write_file" => format!(
            "Write {} bytes to {}",
            arg("content").len(),
            display_safe(arg("path"))
        ),
        name => format!("Call {name} with {}", display_safe(&call.args.to_string())),
    }
}

/// A tool's preview (header line, then a unified diff) made safe to print,
/// optionally colored: additions green, removals red, hunk headers cyan.
pub fn render_preview(preview: &str, color: bool) -> String {
    let paint = |code: &str, line: &str| {
        if color {
            format!("\x1b[{code}m{line}\x1b[0m")
        } else {
            line.to_string()
        }
    };
    preview
        .lines()
        .enumerate()
        .map(|(i, raw)| {
            let line = escape_controls(raw);
            if i == 0 {
                paint("1", &line)
            } else if line.starts_with("@@") {
                paint("36", &line)
            } else if line.starts_with('+') && !line.starts_with("+++") {
                paint("32", &line)
            } else if line.starts_with('-') && !line.starts_with("---") {
                paint("31", &line)
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Control characters (other than TAB) shown escaped, so model-supplied text
/// cannot drive the terminal.
fn escape_controls(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    for c in line.chars() {
        if c.is_control() && c != '\t' {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

/// Model-supplied text made safe to print: control characters (which could
/// move the cursor or hide text in the terminal) are shown escaped. Newlines
/// are kept, indented so multi-line commands stay readable.
fn display_safe(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\n' => out.push_str("\n  "),
            '\t' => out.push('\t'),
            c if c.is_control() => out.extend(c.escape_default()),
            c => out.push(c),
        }
    }
    out
}

/// Asks until the answer is understood. End of input counts as deny, so a
/// closed stdin can never loop or approve.
pub fn ask_approval(
    question: &str,
    input: &mut impl BufRead,
    out: &mut impl Write,
) -> io::Result<Approval> {
    writeln!(out, "{question}")?;
    loop {
        write!(out, "Approve? (y/N): ")?;
        out.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            writeln!(out)?;
            return Ok(Approval::Deny);
        }
        match parse_approval(&line) {
            Some(decision) => return Ok(decision),
            None => writeln!(out, "Sorry, I didn't get that. Please answer y or n.")?,
        }
    }
}

/// Renders agent events and collects operator input. The core calls these
/// methods instead of writing to the terminal directly. Event hooks default to
/// no-ops so a front end implements only what it displays.
pub trait Presenter {
    /// A new task has begun with the given instruction.
    fn task_started(&mut self, _instruction: &str) {}

    /// The model requested a tool call.
    fn tool_requested(&mut self, _call: &ToolCall) {}

    /// A tool finished; `output` is the payload fed back to the model.
    fn tool_completed(&mut self, _name: &str, _output: &str) {}

    /// A chunk of streamed assistant content arrived (streaming mode only).
    fn stream_delta(&mut self, _delta: &str) {}

    /// The model finished responding (streamed or not), before any logging of
    /// the step. Lets a front end terminate a live line.
    fn stream_end(&mut self) {}

    /// The loop hit its step cap without the model signaling completion.
    fn step_cap_reached(&mut self, _max_steps: usize) {}

    /// The loop guard stopped the turn after repeated identical actions.
    fn loop_detected(&mut self, _repeats: usize) {}

    /// Ask the operator to approve a state-changing action. `preview` is
    /// what the tool will change (a diff for edits and overwrites), if it
    /// provides one; otherwise the front end describes the call itself.
    fn request_approval(&mut self, call: &ToolCall, preview: Option<&str>) -> io::Result<Approval>;

    /// The final assistant answer for the task.
    fn final_answer(&mut self, content: &str);
}

/// Plain terminal front end: final answer to stdout, approval prompt on
/// stdout/stdin. Requires only the core dependencies.
#[derive(Default)]
pub struct CliPresenter {
    /// Approve every request without asking (evaluation harness only).
    pub approve_all: bool,
    /// A streamed line is open on stdout and needs a closing newline.
    line_open: bool,
    /// Content was streamed this step, so `final_answer` must not reprint it.
    streamed: bool,
}

impl CliPresenter {
    fn close_line(&mut self) {
        if self.line_open {
            println!();
            let _ = io::stdout().flush();
            self.line_open = false;
        }
    }
}

impl Presenter for CliPresenter {
    fn tool_requested(&mut self, _call: &ToolCall) {
        self.close_line();
        self.streamed = false;
    }

    fn stream_delta(&mut self, delta: &str) {
        if !self.line_open {
            println!();
            self.line_open = true;
            self.streamed = true;
        }
        print!("{delta}");
        let _ = io::stdout().flush();
    }

    fn stream_end(&mut self) {
        self.close_line();
    }

    fn request_approval(&mut self, call: &ToolCall, preview: Option<&str>) -> io::Result<Approval> {
        if self.approve_all {
            return Ok(Approval::Allow);
        }
        let question = match preview {
            Some(p) => render_preview(p, io::stdout().is_terminal()),
            None => describe_action(call),
        };
        let decision = ask_approval(&question, &mut io::stdin().lock(), &mut io::stdout())?;
        if decision == Approval::Deny {
            println!("Denied.");
        }
        Ok(decision)
    }

    fn final_answer(&mut self, content: &str) {
        self.close_line();
        if !self.streamed {
            println!("\n{content}");
        }
        self.streamed = false;
    }

    fn loop_detected(&mut self, repeats: usize) {
        self.close_line();
        eprintln!("loop guard: stopped after {repeats} repeated identical actions.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    #[test]
    fn approval_parsing() {
        for yes in ["y", " Y\n", "yes", "YES", "Yes", "yEs"] {
            assert_eq!(parse_approval(yes), Some(Approval::Allow), "{yes:?}");
        }
        for no in ["n", "N", "no", "NO", "No", "nO", "", "  \n"] {
            assert_eq!(parse_approval(no), Some(Approval::Deny), "{no:?}");
        }
        for unclear in ["r", "q", "yess", "nope", "y n", "ok"] {
            assert_eq!(parse_approval(unclear), None, "{unclear:?}");
        }
    }

    fn ask(answers: &str) -> (Approval, String) {
        let mut out = Vec::new();
        let d = ask_approval(
            "Run shell command:\n  ls",
            &mut answers.as_bytes(),
            &mut out,
        )
        .unwrap();
        (d, String::from_utf8(out).unwrap())
    }

    #[test]
    fn asks_again_until_understood() {
        let (d, out) = ask("r\nq\nYes\n");
        assert_eq!(d, Approval::Allow);
        assert_eq!(out.matches("Sorry, I didn't get that").count(), 2);
        assert_eq!(out.matches("Approve? (y/N): ").count(), 3);
    }

    #[test]
    fn end_of_input_denies() {
        assert_eq!(ask("").0, Approval::Deny);
        assert_eq!(ask("maybe\n").0, Approval::Deny);
    }

    #[test]
    fn preview_is_escaped_and_optionally_colored() {
        let p = "Edit f.rs  (+1 -1, exact match)\n--- a/f.rs\n+++ b/f.rs\n@@ -1,1 +1,1 @@\n-old\n+new\u{1b}[2J";
        let plain = render_preview(p, false);
        assert!(plain.contains("+new\\u{1b}[2J"), "{plain}");
        assert!(!plain.contains('\u{1b}'));
        let colored = render_preview(p, true);
        assert!(colored.contains("\x1b[32m+new"));
        assert!(colored.contains("\x1b[31m-old"));
        assert!(
            colored.contains("--- a/f.rs\n"),
            "file headers are not colored"
        );
    }

    #[test]
    fn describes_actions_and_escapes_control_characters() {
        let call = |name: &str, args| ToolCall {
            id: "1".into(),
            name: name.into(),
            args,
        };
        let bash = describe_action(&call(
            "execute_bash",
            json!({ "command": "gcc -o hi hi.c" }),
        ));
        assert_eq!(bash, "Run shell command:\n  gcc -o hi hi.c");
        let write = describe_action(&call(
            "write_file",
            json!({ "path": "hi.c", "content": "abc" }),
        ));
        assert_eq!(write, "Write 3 bytes to hi.c");
        // An escape sequence in a model-supplied command is shown, not executed
        // by the terminal.
        let sneaky = describe_action(&call("execute_bash", json!({ "command": "ls\u{1b}[2K" })));
        assert!(sneaky.contains("\\u{1b}[2K"), "{sneaky}");
        assert!(!sneaky.contains('\u{1b}'));
    }
}
