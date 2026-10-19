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

/// How the approval policy treats one tool call. `needs` means a person must
/// decide; otherwise `decision` and `rule` say what decided and how, for the
/// audit record (docs/design/design-audit-record.md).
#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    pub needs: bool,
    pub decision: &'static str,
    pub rule: Option<&'static str>,
    /// The allowlist entry that matched, for `approval.auto_approve_bash`.
    pub matched: Option<String>,
}

impl Policy {
    fn ask() -> Self {
        Self {
            needs: true,
            decision: "",
            rule: None,
            matched: None,
        }
    }

    fn rule(decision: &'static str, rule: &'static str) -> Self {
        Self {
            needs: false,
            decision,
            rule: Some(rule),
            matched: None,
        }
    }
}

/// Core policy: whether a tool call is state-changing and must be approved,
/// and if not, which rule lets it through. `sandboxed` says whether shell
/// commands run inside the Tier 1 sandbox: automatic approval (the
/// allowlist, or `auto_approve_sandboxed`) applies only then, because an
/// allowlist alone is easy to get around.
pub fn policy(cfg: &Config, call: &ToolCall, sandboxed: bool) -> Policy {
    match call.name.as_str() {
        "execute_bash" => {
            if !cfg.security.require_approval_for_bash {
                return Policy::rule("not_required", "approval.bash_not_required");
            }
            if !sandboxed {
                return Policy::ask();
            }
            let cmd = call.args["command"].as_str().unwrap_or("");
            if cfg.security.auto_approve_sandboxed {
                tracing::info!(command = %cmd, "sandboxed bash command auto-approved");
                return Policy::rule("auto_approved", "approval.auto_approve_sandboxed");
            }
            if let Some(entry) = bash_allowlist_match(cmd, &cfg.security.auto_approve_bash) {
                tracing::info!(command = %cmd, "bash command auto-approved by allowlist");
                return Policy {
                    matched: Some(entry.to_string()),
                    ..Policy::rule("auto_approved", "approval.auto_approve_bash")
                };
            }
            Policy::ask()
        }
        "write_file" | "edit_file" if cfg.security.require_approval_for_writes => Policy::ask(),
        // What the next session loads as instructions or configuration:
        // always a person's decision (docs/design/design-project-instructions.md).
        "write_file" | "edit_file"
            if crate::instructions::is_protected_name(
                call.args["path"].as_str().unwrap_or(""),
                &cfg.instructions.files,
            ) =>
        {
            Policy::ask()
        }
        "write_file" | "edit_file" => Policy::rule("not_required", "approval.writes_not_required"),
        _ => Policy::rule("not_required", "approval.read_only"), // read-only tools never prompt
    }
}

/// Whether a tool call must be approved by a person.
#[cfg(test)]
pub fn needs_approval(cfg: &Config, call: &ToolCall, sandboxed: bool) -> bool {
    policy(cfg, call, sandboxed).needs
}

/// True when `cmd` is a plain command (no shell metacharacters) whose leading
/// words equal one of the `allowed` entries. Matching is whole-word, so
/// `git status` matches `git status --short` but not `git statusx`. Empty
/// entries are ignored rather than matching everything.
#[cfg(test)]
pub fn bash_auto_approved(cmd: &str, allowed: &[String]) -> bool {
    bash_allowlist_match(cmd, allowed).is_some()
}

/// The allowlist entry `cmd` matches, under the rules of `bash_auto_approved`.
pub fn bash_allowlist_match<'a>(cmd: &str, allowed: &'a [String]) -> Option<&'a str> {
    if cmd.contains(SHELL_METACHARS) {
        return None;
    }
    let words: Vec<&str> = cmd.split_whitespace().collect();
    allowed
        .iter()
        .find(|entry| {
            let prefix: Vec<&str> = entry.split_whitespace().collect();
            !prefix.is_empty() && words.starts_with(&prefix)
        })
        .map(String::as_str)
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
        assert!(!needs_approval(&cfg, &bash("cargo test"), true));
        assert!(needs_approval(&cfg, &bash("cargo build"), true));

        cfg.security.require_approval_for_bash = false;
        assert!(!needs_approval(&cfg, &bash("cargo build"), true));
        assert!(!needs_approval(&cfg, &bash("cargo build"), false));
    }

    #[test]
    fn policy_names_the_rule_that_decided() {
        let mut cfg = Config::default();
        cfg.security.auto_approve_bash = list(&["git status", "cargo test"]);
        let read = ToolCall {
            id: "1".into(),
            name: "read_file".into(),
            args: json!({ "path": "a" }),
        };
        assert_eq!(policy(&cfg, &read, true).rule, Some("approval.read_only"));
        let p = policy(&cfg, &bash("cargo test --release"), true);
        assert_eq!(
            (p.needs, p.decision, p.rule, p.matched.as_deref()),
            (
                false,
                "auto_approved",
                Some("approval.auto_approve_bash"),
                Some("cargo test")
            )
        );
        let p = policy(&cfg, &bash("cargo build"), true);
        assert!(p.needs && p.rule.is_none());
        cfg.security.auto_approve_sandboxed = true;
        assert_eq!(
            policy(&cfg, &bash("cargo build"), true).rule,
            Some("approval.auto_approve_sandboxed")
        );
        cfg.security.require_approval_for_bash = false;
        assert_eq!(
            policy(&cfg, &bash("rm x"), false).rule,
            Some("approval.bash_not_required")
        );
        cfg.security.require_approval_for_writes = false;
        let write = ToolCall {
            id: "2".into(),
            name: "edit_file".into(),
            args: json!({}),
        };
        assert_eq!(
            policy(&cfg, &write, true).rule,
            Some("approval.writes_not_required")
        );
        for p in [
            policy(&cfg, &read, true),
            policy(&cfg, &bash("ls"), true),
            policy(&cfg, &write, true),
        ] {
            assert!(crate::audit::RULES.contains(&p.rule.unwrap()));
        }
    }

    #[test]
    fn writes_to_instruction_files_always_ask() {
        let mut cfg = Config::default();
        cfg.security.require_approval_for_writes = false;
        let write = |path: &str| ToolCall {
            id: "1".into(),
            name: "write_file".into(),
            args: json!({ "path": path, "content": "x" }),
        };
        assert!(!needs_approval(&cfg, &write("src/lib.rs"), true));
        assert!(needs_approval(&cfg, &write("AGENTS.md"), true));
        assert!(needs_approval(&cfg, &write("sub/agents.md"), true));
        assert!(needs_approval(&cfg, &write("agent.toml"), true));
        cfg.instructions.files.push("CLAUDE.md".into());
        assert!(needs_approval(&cfg, &write("CLAUDE.md"), true));
    }

    #[test]
    fn automatic_approval_needs_the_sandbox() {
        let mut cfg = Config::default();
        cfg.security.auto_approve_bash = list(&["cargo test"]);
        // Without the sandbox the allowlist does not apply.
        assert!(needs_approval(&cfg, &bash("cargo test"), false));
        cfg.security.auto_approve_sandboxed = true;
        assert!(needs_approval(&cfg, &bash("rm -rf /"), false));
        assert!(!needs_approval(&cfg, &bash("make && ./run_tests"), true));
    }
}
