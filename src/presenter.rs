//! Presentation decoupling: the agent core emits typed events and collects
//! decisions through a `Presenter`, so the same loop can drive a plain CLI, a
//! TUI, or a REST front end without touching core logic (see `REQ-UI-*`).

use std::io::{self, Write};

use crate::tools::ToolCall;

/// An operator decision for a state-changing action gated by approval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Approval {
    Allow,
    Deny,
}

/// Interprets a line of user input as an approval decision. Only `y`/`Y`
/// (case-insensitive, whitespace-trimmed) allows; everything else denies.
pub fn parse_approval(input: &str) -> Approval {
    if input.trim().eq_ignore_ascii_case("y") {
        Approval::Allow
    } else {
        Approval::Deny
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

    /// Ask the operator to approve a state-changing action.
    fn request_approval(&mut self, call: &ToolCall) -> io::Result<Approval>;

    /// The final assistant answer for the task.
    fn final_answer(&mut self, content: &str);
}

/// Plain terminal front end: final answer to stdout, approval prompt on
/// stdout/stdin. Requires only the core dependencies.
#[derive(Default)]
pub struct CliPresenter {
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

    fn request_approval(&mut self, call: &ToolCall) -> io::Result<Approval> {
        print!("Approve `{}`? (y/N): ", call.name);
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        Ok(parse_approval(&input))
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

    #[test]
    fn approval_parsing() {
        assert_eq!(parse_approval("y"), Approval::Allow);
        assert_eq!(parse_approval(" Y\n"), Approval::Allow);
        assert_eq!(parse_approval("n"), Approval::Deny);
        assert_eq!(parse_approval(""), Approval::Deny);
        assert_eq!(parse_approval("yes"), Approval::Deny);
    }
}
