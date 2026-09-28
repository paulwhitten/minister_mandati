//! Approval policy: decides which state-changing tool calls need an operator
//! prompt. The prompt itself is delegated to the `Presenter`.

use crate::config::Config;
use crate::tools::ToolCall;

/// Characters that let a shell command chain, substitute, or redirect. A command
/// containing any of these never matches `auto_approve_bash`, so an allowed
/// prefix like `cargo test` cannot smuggle in `cargo test; rm -rf ~`.
const SHELL_METACHARS: &[char] = &[
    ';', '&', '|', '`', '$', '(', ')', '<', '>', '\n', '\r', '\\',
];

/// Core policy: whether a tool call is state-changing and must be approved.
pub fn needs_approval(cfg: &Config, call: &ToolCall) -> bool {
    match call.name.as_str() {
        "execute_bash" => {
            if !cfg.security.require_approval_for_bash {
                return false;
            }
            let cmd = call.args["command"].as_str().unwrap_or("");
            if bash_auto_approved(cmd, &cfg.security.auto_approve_bash) {
                tracing::info!(command = %cmd, "bash command auto-approved by allowlist");
                return false;
            }
            true
        }
        "write_file" | "edit_file" => cfg.security.require_approval_for_writes,
        _ => false, // read-only tools never prompt
    }
}

/// True when `cmd` is a plain command (no shell metacharacters) whose leading
/// words equal one of the `allowed` entries. Matching is whole-word, so
/// `git status` matches `git status --short` but not `git statusx`. Empty
/// entries are ignored rather than matching everything.
pub fn bash_auto_approved(cmd: &str, allowed: &[String]) -> bool {
    if cmd.contains(SHELL_METACHARS) {
        return false;
    }
    let words: Vec<&str> = cmd.split_whitespace().collect();
    allowed.iter().any(|entry| {
        let prefix: Vec<&str> = entry.split_whitespace().collect();
        !prefix.is_empty() && words.starts_with(&prefix)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn list(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn bash(cmd: &str) -> ToolCall {
        ToolCall {
            id: "1".into(),
            name: "execute_bash".into(),
            args: json!({ "command": cmd }),
        }
    }

    #[test]
    fn matches_exact_and_with_args() {
        let a = list(&["cargo test", "git status"]);
        assert!(bash_auto_approved("cargo test", &a));
        assert!(bash_auto_approved("  cargo   test  --release ", &a));
        assert!(bash_auto_approved("git status --short", &a));
    }

    #[test]
    fn requires_whole_word_prefix() {
        let a = list(&["git status"]);
        assert!(!bash_auto_approved("git statusx", &a));
        assert!(!bash_auto_approved("git", &a));
        assert!(!bash_auto_approved("git push", &a));
    }

    #[test]
    fn rejects_shell_metacharacters() {
        let a = list(&["cargo test"]);
        for cmd in [
            "cargo test; rm -rf ~",
            "cargo test && curl x",
            "cargo test | sh",
            "cargo test > out.txt",
            "cargo test $(whoami)",
            "cargo test `whoami`",
            "cargo test\nrm -rf ~",
            "cargo test &",
        ] {
            assert!(!bash_auto_approved(cmd, &a), "should reject: {cmd:?}");
        }
    }

    #[test]
    fn empty_entries_and_list_match_nothing() {
        assert!(!bash_auto_approved("ls", &list(&["", "   "])));
        assert!(!bash_auto_approved("ls", &[]));
    }

    #[test]
    fn policy_respects_flags_and_allowlist() {
        let mut cfg = Config::default();
        cfg.security.auto_approve_bash = list(&["cargo test"]);
        assert!(!needs_approval(&cfg, &bash("cargo test")));
        assert!(needs_approval(&cfg, &bash("cargo build")));

        cfg.security.require_approval_for_bash = false;
        assert!(!needs_approval(&cfg, &bash("cargo build")));
    }
}
