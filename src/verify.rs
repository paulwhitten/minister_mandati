//! Verifier gate (`docs/design/design-verifier-gate.md`): the model revises
//! its work only on external verifier signals. This module holds the pure
//! parts: choosing the verifier command, extracting failure lines from its
//! output, comparing against the turn's baseline, the feedback message, and
//! the syntax checks of tier 1. The agent loop runs the commands.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

use crate::evidence::Mark;

/// Prefix the model may use to finish a turn whose checks it cannot make
/// pass (the tests or the task are wrong). The answer is then accepted.
pub const UNVERIFIED: &str = "UNVERIFIED:";
/// Largest feedback message sent to the model, in bytes.
const MAX_FEEDBACK: usize = 4096;
/// Most failure lines kept from one verifier run. The baseline comparison
/// needs all of them (a cut window shifts as failures are fixed); what the
/// model is shown is cut separately, at `MAX_FEEDBACK`.
const MAX_LINES: usize = 5000;

/// The project's test command, guessed from files in `dir`: Cargo, a
/// Makefile `test:` target, a Go module, a `package.json` test script, or
/// Python test files. `None` when unsure.
pub fn detect(dir: &Path) -> Option<String> {
    if dir.join("Cargo.toml").is_file() {
        return Some("cargo test".into());
    }
    for name in ["Makefile", "makefile", "GNUmakefile"] {
        if let Ok(text) = std::fs::read_to_string(dir.join(name))
            && text.lines().any(|l| l.starts_with("test:"))
        {
            return Some("make test".into());
        }
    }
    if dir.join("go.mod").is_file() {
        return Some("go test ./...".into());
    }
    if let Ok(text) = std::fs::read_to_string(dir.join("package.json"))
        && let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&text)
        && let Some(test) = pkg["scripts"]["test"].as_str()
        && !test.contains("no test specified")
    {
        return Some("npm test --silent".into());
    }
    if has_python_tests(dir, 0) {
        // pytest finds unittest tests too; fall back when it is missing.
        return Some(
            "if python3 -c 'import pytest' 2>/dev/null; then python3 -m pytest -q; \
             else python3 -m unittest discover -q; fi"
                .into(),
        );
    }
    None
}

fn has_python_tests(dir: &Path, depth: usize) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let path = e.path();
        if path.is_dir() {
            if depth < 2
                && !name.starts_with('.')
                && !matches!(name.as_str(), "target" | "node_modules" | "venv" | ".venv")
                && has_python_tests(&path, depth + 1)
            {
                return true;
            }
        } else if name.ends_with(".py") && (name.starts_with("test_") || name.ends_with("_test.py"))
        {
            return true;
        }
    }
    false
}

/// Exit code from `execute_bash` output (`exit: N` on the first line);
/// `None` for errors such as a timeout.
pub fn exit_code(output: &str) -> Option<i32> {
    output
        .lines()
        .next()?
        .strip_prefix("exit: ")?
        .trim()
        .parse()
        .ok()
}

/// Lines that say what failed: compiler errors, failing tests, panics,
/// assertion values, tracebacks and `file:line` locations, with their
/// immediate neighbours. Falls back to the last lines of output.
pub fn failure_lines(output: &str) -> Vec<String> {
    let lines: Vec<&str> = output.lines().collect();
    let interesting = |l: &str| {
        let t = l.trim_start();
        let lower = t.to_ascii_lowercase();
        lower.starts_with("error")
            || lower.contains(": error")
            || t.contains("FAILED")
            || t.contains("FAIL:")
            || t.starts_with("ERROR")
            || t.contains("panicked at")
            || lower.contains("assert")
            || t.starts_with("--> ")
            || t.starts_with("E ")
            || t.starts_with("File \"")
            || t.starts_with("left:")
            || t.starts_with("right:")
            || lower.contains("undefined reference")
            || lower.contains("traceback")
            || lower.contains("segmentation fault")
            || lower.contains("addresssanitizer")
            || lower.contains("syntax error")
            || t.contains("Error:")
    };
    let mut keep = vec![false; lines.len()];
    for (i, l) in lines.iter().enumerate() {
        if interesting(l) {
            keep[i] = true;
            if i + 1 < lines.len() {
                keep[i + 1] = true;
            }
        }
    }
    let mut out: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for (i, l) in lines.iter().enumerate() {
        let l = l.trim_end();
        if keep[i]
            && !l.trim().is_empty()
            && !matches!(l, "STDOUT:" | "STDERR:")
            && seen.insert(l.to_string())
        {
            out.push(l.to_string());
        }
        if out.len() >= MAX_LINES {
            break;
        }
    }
    if out.is_empty() {
        out = lines
            .iter()
            .rev()
            .filter(|l| !l.trim().is_empty() && !matches!(l.trim_end(), "STDOUT:" | "STDERR:"))
            .take(20)
            .map(|l| l.trim_end().to_string())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
    }
    out
}

/// A failure line with run-specific numbers (durations, addresses) and
/// line numbers (`file:12:5`, `line 12`) removed, so the same failure
/// compares equal across runs and after edits move it.
fn normalize(line: &str) -> String {
    let mut s = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '0' && chars.peek() == Some(&'x') {
            // Hex address: drop it.
            chars.next();
            while chars.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                chars.next();
            }
            s.push_str("0x_");
        } else if c.is_ascii_digit() {
            let mut num = String::from(c);
            while chars
                .peek()
                .is_some_and(|c| c.is_ascii_digit() || *c == '.')
            {
                num.push(chars.next().unwrap_or('0'));
            }
            // Durations like 0.12s or 35ms vary between runs.
            let unit: String = chars
                .clone()
                .take_while(|c| c.is_ascii_alphabetic())
                .collect();
            if matches!(unit.as_str(), "s" | "ms" | "us" | "sec" | "secs") {
                for _ in 0..unit.len() {
                    chars.next();
                }
                s.push_str("_t");
            } else if s.ends_with(':') || s.ends_with("line ") {
                s.push_str("_n");
            } else {
                s.push_str(&num);
            }
        } else {
            s.push(c);
        }
    }
    s
}

/// Failure lines in `current` that were not in `baseline`. Summary lines
/// ("1 failed, 2 passed", "test result: FAILED") are left out: their counts
/// change whenever anything changes, including improvements.
pub fn new_failures(baseline: &[String], current: &[String]) -> Vec<String> {
    let old: HashSet<String> = baseline.iter().map(|l| normalize(l)).collect();
    current
        .iter()
        .filter(|l| !is_summary(l) && !old.contains(&normalize(l)))
        .cloned()
        .collect()
}

fn is_summary(line: &str) -> bool {
    let t = line.trim();
    t.starts_with("test result:")
        || t.starts_with("FAILED (")
        || t.starts_with("Ran ")
        || t.starts_with("exit: ")
        || t.starts_with("error: test failed")
        || t.starts_with("make: ***")
        || (t.contains(" failed")
            && (t.contains(" passed") || t.contains(" in "))
            && !t.contains("::"))
}

/// What the turn changed in the workspace that a test run does not show:
/// existing test files it modified and new files it left, compared with the
/// workspace's git state when the turn began (end check).
#[derive(Debug, Default, PartialEq)]
pub struct WorkspaceFacts {
    pub tests_changed: Vec<String>,
    pub new_files: Vec<String>,
}

impl WorkspaceFacts {
    pub fn is_empty(&self) -> bool {
        self.tests_changed.is_empty() && self.new_files.is_empty()
    }

    fn text(&self) -> String {
        let mut out = String::new();
        if !self.tests_changed.is_empty() {
            out.push_str(&format!(
                "Existing test files changed in this task: {}.\n",
                list(&self.tests_changed)
            ));
        }
        if !self.new_files.is_empty() {
            out.push_str(&format!(
                "New files not tracked by git: {}.\n",
                list(&self.new_files)
            ));
        }
        out
    }
}

/// Most paths named in one list.
const MAX_LISTED: usize = 20;

fn list(paths: &[String]) -> String {
    let mut shown: Vec<&str> = paths.iter().take(MAX_LISTED).map(String::as_str).collect();
    let more = paths.len().saturating_sub(MAX_LISTED);
    let tail = format!("and {more} more");
    if more > 0 {
        shown.push(&tail);
    }
    shown.join(", ")
}

/// Compares two snapshots of the workspace's changed paths (see
/// `evidence::marks_in`): tracked test files whose content differs from the
/// start, and untracked files that were not there at the start. Paths in
/// `skip` (written by the verifier's own runs) and generated files (see
/// [`is_generated`]) are left out.
pub fn workspace_facts(
    start: &BTreeMap<String, Mark>,
    end: &BTreeMap<String, Mark>,
    skip: &BTreeSet<String>,
) -> WorkspaceFacts {
    let mut facts = WorkspaceFacts::default();
    for (path, now) in end {
        let before = start.get(path);
        if before == Some(now) || skip.contains(path) || is_generated(path) {
            continue;
        }
        if now.status == "??" {
            if before.is_none() {
                facts.new_files.push(path.clone());
            }
        } else if !now.status.starts_with('A') && is_test_path(path) {
            facts.tests_changed.push(path.clone());
        }
    }
    facts
}

/// Paths whose mark differs between two snapshots, generated files left out.
pub fn changed_paths<'a>(
    before: &'a BTreeMap<String, Mark>,
    after: &'a BTreeMap<String, Mark>,
) -> impl Iterator<Item = &'a String> {
    before
        .keys()
        .chain(after.keys().filter(|p| !before.contains_key(*p)))
        .filter(|p| before.get(*p) != after.get(*p) && !is_generated(p))
}

/// A file that running a build or tests writes, by common convention: caches
/// and byte code (`__pycache__`, `.pytest_cache`, `*.pyc`, `node_modules`, a
/// top-level `target`), and coverage output.
pub fn is_generated(path: &str) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    let name = parts.last().copied().unwrap_or(path);
    parts.first() == Some(&"target") && parts.len() > 1
        || parts.iter().any(|d| {
            matches!(
                *d,
                "__pycache__"
                    | ".pytest_cache"
                    | ".mypy_cache"
                    | ".ruff_cache"
                    | ".tox"
                    | ".nox"
                    | "node_modules"
                    | ".nyc_output"
                    | "htmlcov"
                    | ".gradle"
            )
        })
        || [".pyc", ".pyo", ".o", ".class"]
            .iter()
            .any(|e| name.ends_with(e))
        || name == ".coverage"
        || name.starts_with(".coverage.")
}

/// A path that holds tests by common convention: a `test`, `tests`,
/// `__tests__` or `spec` directory, or a file named like `*_test.go`,
/// `test_*.py`, `*_test.py`, `*.test.js` or `*.spec.ts`.
pub fn is_test_path(path: &str) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    let (dirs, name) = parts.split_at(parts.len() - 1);
    if dirs
        .iter()
        .any(|d| matches!(*d, "test" | "tests" | "__tests__" | "spec" | "testdata"))
    {
        return true;
    }
    let name = name[0];
    let stem = name.split('.').next().unwrap_or(name);
    name.ends_with("_test.go")
        || (name.ends_with(".py") && (stem.starts_with("test_") || stem.ends_with("_test")))
        || name.contains(".test.")
        || name.contains(".spec.")
        || name.ends_with("_spec.rb")
}

/// One verifier command that found new failures, as shown to the model.
fn failures_block(command: &str, exit: Option<i32>, failures: &[String]) -> String {
    let status = match exit {
        Some(c) => format!("failed (exit {c})"),
        None => "did not complete (error or timeout)".to_string(),
    };
    let mut body = String::new();
    for l in failures {
        if body.len() + l.len() + 1 > MAX_FEEDBACK {
            body.push_str("[more failure lines omitted]\n");
            break;
        }
        body.push_str(l);
        body.push('\n');
    }
    format!("`{command}` {status}. New failures since the start of this task:\n{body}")
}

/// One verifier command's state for the wrap-up notice.
pub fn check_line(command: &str, exit: Option<i32>, new: &[String]) -> String {
    if exit == Some(0) {
        format!("`{command}` passes.")
    } else if new.is_empty() {
        format!("`{command}` fails as it did before this task's changes.")
    } else {
        failures_block(command, exit, new)
    }
}

/// A verifier command's new failures: command, exit code, failure lines.
pub type Failing<'a> = (&'a str, Option<i32>, &'a [String]);

/// The message sent to the model when the gate holds an answer back: new
/// failures, and (end check) workspace facts. It is attributed to the
/// check, gives the location-bearing lines only, and offers an honest exit
/// instead of gaming the tests.
pub fn feedback(
    failing: Option<Failing>,
    facts: &WorkspaceFacts,
    round: usize,
    max_rounds: usize,
) -> String {
    let mut m = format!("[automatic check {round} of {max_rounds}] ");
    match failing {
        Some((command, exit, failures)) => {
            m.push_str(&failures_block(command, exit, failures));
            m.push_str(&facts.text());
            m.push_str(
                "\nFix the cause of these failures, then give your answer again. Do not \
                 change existing tests to make them pass.",
            );
        }
        None => {
            m.push_str("The checks pass. What this task changed besides that:\n");
            m.push_str(&facts.text());
            m.push_str(
                "\nUndo any of these changes the task did not ask for, then give your answer \
                 again; if they are all intended, give your answer again as it is.",
            );
        }
    }
    m.push_str(&format!(
        " If the tests or the task are wrong and cannot be satisfied, say why and start your \
         answer with \"{UNVERIFIED}\"."
    ));
    m
}

/// Where the turn stands against its limits, e.g. `Step 48 of 60, 20 of 25
/// minutes used`.
pub fn budget_line(
    step: usize,
    max_steps: usize,
    elapsed_secs: u64,
    max_secs: Option<u64>,
) -> String {
    let mut line = format!("Step {step} of {max_steps}");
    if let Some(max) = max_secs {
        line.push_str(&format!(
            ", {} of {} minutes used",
            elapsed_secs / 60,
            max.div_ceil(60)
        ));
    }
    line
}

/// The notice at the wrap-up point: the budget, and the state of the checks
/// when the turn has changed files.
pub fn wrap_up_notice(budget: &str, checks: &[String], facts: &WorkspaceFacts) -> String {
    let mut m = format!("[budget] {budget}.");
    for c in checks {
        m.push(' ');
        m.push_str(c);
    }
    let facts = facts.text();
    if !facts.is_empty() {
        m.push('\n');
        m.push_str(&facts);
    }
    m.push_str("\nThe task ends at the limit; finish and check the work within it.");
    m
}

/// Why the turn reached its repair point (end check).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Limit {
    /// `left` steps remain of the step limit.
    Steps { left: usize },
    /// The time limit is reached; `steps` more are allowed for repair.
    Time { minutes: u64, steps: usize },
}

/// The notice at the repair point when the checks found new failures.
pub fn limit_notice(limit: Limit, failing: Failing, facts: &WorkspaceFacts) -> String {
    let (head, tail) = match limit {
        Limit::Steps { left } => (
            format!("{left} steps are left in this task."),
            "Use them to fix these failures.".to_string(),
        ),
        Limit::Time { minutes, steps } => (
            format!("The time limit ({minutes} minutes) is reached."),
            format!("You have {steps} more steps to fix these failures; then the task ends."),
        ),
    };
    let (command, exit, failures) = failing;
    format!(
        "[automatic check] {head} {}{}\n{tail} Do not change existing tests to make them pass.",
        failures_block(command, exit, failures),
        facts.text()
    )
}

/// Result of a tier-1 syntax check.
#[derive(Debug, PartialEq)]
pub enum Syntax {
    Ok,
    Error(String),
    /// No cheap checker for this file type, or the checker is unavailable.
    Unknown,
}

/// In-process syntax check for formats parsed without running anything.
/// Python and shell scripts need a command; see [`syntax_command`].
pub fn syntax_check_bytes(path: &Path, bytes: &[u8]) -> Syntax {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Syntax::Unknown;
    };
    match ext {
        // Tools that read these allow comments (JSONC); strict JSON would
        // reject a valid file.
        "json" if allows_comments(path) => Syntax::Unknown,
        "json" => match serde_json::from_str::<serde_json::Value>(text) {
            Ok(_) => Syntax::Ok,
            Err(e) => Syntax::Error(format!("invalid JSON: {e}")),
        },
        "toml" => match text.parse::<toml::Table>() {
            Ok(_) => Syntax::Ok,
            Err(e) => Syntax::Error(format!("invalid TOML: {}", e.to_string().trim())),
        },
        _ => Syntax::Unknown,
    }
}

/// JSON files read as JSON with comments: TypeScript and JavaScript
/// project files, VS Code settings, dev containers.
fn allows_comments(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    name.starts_with("tsconfig")
        || name.starts_with("jsconfig")
        || name.starts_with("devcontainer")
        || name == ".devcontainer.json"
        || path
            .components()
            .any(|c| matches!(c.as_os_str().to_str(), Some(".vscode" | ".devcontainer")))
}

/// A shell command that parses (never runs) the file at `path`, for file
/// types without an in-process parser. Exit 0 means it parses.
pub fn syntax_command(path: &Path) -> Option<String> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let quoted = shell_quote(&path.display().to_string());
    match ext {
        "py" => Some(format!(
            "python3 -c 'import ast,sys; ast.parse(open(sys.argv[1]).read(), sys.argv[1])' {quoted}"
        )),
        "sh" | "bash" => Some(format!("bash -n {quoted}")),
        _ => None,
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("mima-verify-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn detects_common_test_commands() {
        let d = tmp("cargo");
        std::fs::write(d.join("Cargo.toml"), "[package]\n").unwrap();
        assert_eq!(detect(&d).as_deref(), Some("cargo test"));
        let d = tmp("make");
        std::fs::write(d.join("Makefile"), "all:\n\tcc x.c\ntest: all\n\t./x\n").unwrap();
        assert_eq!(detect(&d).as_deref(), Some("make test"));
        let d = tmp("make-no-test");
        std::fs::write(d.join("Makefile"), "all:\n\tcc x.c\n").unwrap();
        assert_eq!(detect(&d), None);
        let d = tmp("py");
        std::fs::create_dir_all(d.join("tests")).unwrap();
        std::fs::write(d.join("tests/test_x.py"), "def test_a(): pass\n").unwrap();
        assert!(detect(&d).unwrap().contains("pytest"));
        let d = tmp("none");
        std::fs::write(d.join("x.c"), "int main(){}\n").unwrap();
        assert_eq!(detect(&d), None);
        let d = tmp("go");
        std::fs::write(d.join("go.mod"), "module x\n").unwrap();
        assert_eq!(detect(&d).as_deref(), Some("go test ./..."));
        let d = tmp("npm");
        std::fs::write(
            d.join("package.json"),
            r#"{"scripts":{"test":"node --test"}}"#,
        )
        .unwrap();
        assert_eq!(detect(&d).as_deref(), Some("npm test --silent"));
        // npm init's placeholder script is not a test command.
        let d = tmp("npm-none");
        std::fs::write(
            d.join("package.json"),
            r#"{"scripts":{"test":"echo \"Error: no test specified\" && exit 1"}}"#,
        )
        .unwrap();
        assert_eq!(detect(&d), None);
    }

    #[test]
    fn recognizes_test_paths() {
        for p in [
            "mux_test.go",
            "tests/parse.rs",
            "src/test/Foo.java",
            "pkg/test_util.py",
            "a/b_test.py",
            "index.test.js",
            "x.spec.ts",
            "__tests__/a.js",
            "spec/model_spec.rb",
        ] {
            assert!(is_test_path(p), "{p}");
        }
        for p in [
            "mux.go",
            "src/lib.rs",
            "contest.py",
            "testing.go",
            "latest/a.js",
        ] {
            assert!(!is_test_path(p), "{p}");
        }
    }

    #[test]
    fn workspace_facts_name_changed_tests_and_new_files() {
        let mark = |status: &str, sha: &str| Mark {
            status: status.into(),
            sha256: Some(sha.into()),
            size: Some(1),
        };
        let start = BTreeMap::from([
            ("old_test.go".to_string(), mark(".M", "a")),
            ("notes.txt".to_string(), mark("??", "n")),
        ]);
        let end = BTreeMap::from([
            ("old_test.go".to_string(), mark(".M", "b")), // edited again
            ("mux_test.go".to_string(), mark(".M", "c")), // edited now
            ("mux.go".to_string(), mark(".M", "d")),      // not a test
            ("notes.txt".to_string(), mark("??", "n2")),  // was there before
            ("new_test.go".to_string(), mark("??", "e")), // new
            ("added.go".to_string(), mark("A.", "f")),    // new and staged
        ]);
        let none = BTreeSet::new();
        let f = workspace_facts(&start, &end, &none);
        assert_eq!(f.tests_changed, vec!["mux_test.go", "old_test.go"]);
        assert_eq!(f.new_files, vec!["new_test.go"]);
        assert!(workspace_facts(&end, &end, &none).is_empty());
        // Files the verifier wrote and generated files are not the task's.
        let mut end = end;
        end.insert("cover.out".into(), mark("??", "g"));
        end.insert("__pycache__/m.cpython-312.pyc".into(), mark("??", "h"));
        end.insert("tests/__pycache__/t.pyc".into(), mark("??", "i"));
        let skip = BTreeSet::from(["cover.out".to_string()]);
        assert_eq!(
            workspace_facts(&start, &end, &skip).new_files,
            vec!["new_test.go"]
        );
        let changed: Vec<&String> = changed_paths(&start, &end).collect();
        assert_eq!(
            changed,
            vec![
                "notes.txt",
                "old_test.go",
                "added.go",
                "cover.out",
                "mux.go",
                "mux_test.go",
                "new_test.go"
            ]
        );
    }

    #[test]
    fn recognizes_generated_paths() {
        for p in [
            "__pycache__/a.cpython-312.pyc",
            "pkg/.pytest_cache/v/cache",
            "target/debug/app",
            "web/node_modules/x/index.js",
            "a.pyc",
            "src/a.o",
            ".coverage",
            ".coverage.host.123",
        ] {
            assert!(is_generated(p), "{p}");
        }
        for p in ["src/target/mod.rs", "target", "main.py", "coverage.md"] {
            assert!(!is_generated(p), "{p}");
        }
    }

    #[test]
    fn notices_say_what_was_found_and_what_is_left() {
        let facts = WorkspaceFacts {
            tests_changed: vec!["mux_test.go".into()],
            new_files: vec!["scratch/main.go".into()],
        };
        let m = feedback(None, &facts, 1, 3);
        assert!(
            m.starts_with("[automatic check 1 of 3] The checks pass."),
            "{m}"
        );
        assert!(m.contains("Existing test files changed in this task: mux_test.go."));
        assert!(m.contains("New files not tracked by git: scratch/main.go."));
        assert!(m.contains(UNVERIFIED));
        let fails = vec!["--- FAIL: TestRoutes".to_string()];
        let m = limit_notice(
            Limit::Steps { left: 5 },
            ("go test ./...", Some(1), &fails),
            &facts,
        );
        assert!(
            m.starts_with(
                "[automatic check] 5 steps are left in this task. `go test ./...` failed (exit 1)."
            ),
            "{m}"
        );
        assert!(m.contains("--- FAIL: TestRoutes") && m.contains("mux_test.go"));
        let m = limit_notice(
            Limit::Time {
                minutes: 25,
                steps: 5,
            },
            ("go test ./...", Some(1), &fails),
            &WorkspaceFacts::default(),
        );
        assert!(
            m.contains("The time limit (25 minutes) is reached.") && m.contains("5 more steps")
        );
        assert_eq!(
            budget_line(48, 60, 1230, Some(1500)),
            "Step 48 of 60, 20 of 25 minutes used"
        );
        assert_eq!(budget_line(48, 60, 1230, None), "Step 48 of 60");
        let w = wrap_up_notice(
            "Step 48 of 60",
            &[check_line("make test", Some(0), &[])],
            &WorkspaceFacts::default(),
        );
        assert_eq!(
            w,
            "[budget] Step 48 of 60. `make test` passes.\nThe task ends at the limit; finish and check the work within it."
        );
    }

    #[test]
    fn reads_exit_codes() {
        assert_eq!(exit_code("exit: 0\nSTDOUT:\nok\n"), Some(0));
        assert_eq!(exit_code("exit: 101\nSTDOUT:\n"), Some(101));
        assert_eq!(exit_code("Error: timed out"), None);
    }

    #[test]
    fn extracts_cargo_and_pytest_failures() {
        let cargo = "exit: 101\nSTDOUT:\nrunning 2 tests\ntest a ... ok\ntest b ... FAILED\n\n\
                     failures:\n---- b stdout ----\nthread 'b' panicked at src/lib.rs:7:5:\n\
                     assertion `left == right` failed\n  left: 3\n right: -3\n\
                     test result: FAILED. 1 passed; 1 failed; 0 ignored; finished in 0.01s\n";
        let f = failure_lines(cargo);
        assert!(f.iter().any(|l| l.contains("test b ... FAILED")));
        assert!(f.iter().any(|l| l.contains("panicked at src/lib.rs:7:5")));
        assert!(f.iter().any(|l| l.trim() == "left: 3"));
        let py = "exit: 1\nSTDOUT:\n.F\n=== FAILURES ===\n___ test_sub ___\n\
                  E       assert -3 == 3\ntest_calc.py:9: AssertionError\n\
                  FAILED test_calc.py::test_sub - assert -3 == 3\n1 failed, 1 passed in 0.03s\n";
        let f = failure_lines(py);
        assert!(f.iter().any(|l| l.starts_with("E ")));
        assert!(
            f.iter()
                .any(|l| l.starts_with("FAILED test_calc.py::test_sub"))
        );
        // Nothing recognisable: the tail is kept.
        assert_eq!(
            failure_lines("exit: 2\nSTDOUT:\nsomething odd\n"),
            vec!["exit: 2", "something odd"]
        );
    }

    #[test]
    fn only_new_failures_count() {
        let base = vec![
            "test a ... FAILED".to_string(),
            "finished in 0.31s".to_string(),
        ];
        let now = vec![
            "test a ... FAILED".to_string(),
            "finished in 1.02s".to_string(),
            "test b ... FAILED".to_string(),
        ];
        assert_eq!(new_failures(&base, &now), vec!["test b ... FAILED"]);
        // Fewer failures is not a new failure, even though the summary changed.
        let base = vec![
            "FAILED t.py::a".to_string(),
            "FAILED t.py::b".to_string(),
            "2 failed in 0.1s".to_string(),
        ];
        let now = vec![
            "FAILED t.py::a".to_string(),
            "1 failed, 1 passed in 0.1s".to_string(),
        ];
        assert!(new_failures(&base, &now).is_empty());
        let addr = vec![
            "ERROR: AddressSanitizer: heap-buffer-overflow on address 0x602000000014".to_string(),
        ];
        let addr2 = vec![
            "ERROR: AddressSanitizer: heap-buffer-overflow on address 0x6020000000f0".to_string(),
        ];
        assert!(new_failures(&addr, &addr2).is_empty());
        // An edit above a failure moves its line number; it is not new.
        let base = vec![
            "test_calc.py:9: AssertionError".to_string(),
            "thread 'a' panicked at src/lib.rs:7:5:".to_string(),
            "  File \"t.py\", line 9, in test_a".to_string(),
        ];
        let now = vec![
            "test_calc.py:12: AssertionError".to_string(),
            "thread 'a' panicked at src/lib.rs:10:5:".to_string(),
            "  File \"t.py\", line 12, in test_a".to_string(),
        ];
        assert!(new_failures(&base, &now).is_empty());
        // Values in a failure still count.
        let base = vec!["left: 5".to_string()];
        let now = vec!["left: 6".to_string()];
        assert_eq!(new_failures(&base, &now), now);
    }

    #[test]
    fn failure_lines_keep_every_failure_for_the_comparison() {
        let out = |n: usize, from: usize| {
            let mut s = "exit: 1\nSTDOUT:\n".to_string();
            for i in from..n {
                s.push_str(&format!("test t{i} ... FAILED\n"));
            }
            s
        };
        let base = failure_lines(&out(100, 0));
        assert_eq!(base.len(), 100);
        // Ten early failures fixed: nothing new.
        assert!(new_failures(&base, &failure_lines(&out(100, 10))).is_empty());
    }

    #[test]
    fn feedback_is_attributed_capped_and_offers_an_exit() {
        let many: Vec<String> = (0..500)
            .map(|i| format!("error[E0308]: mismatched types in item {i}"))
            .collect();
        let m = feedback(
            Some(("cargo test", Some(101), &many)),
            &WorkspaceFacts::default(),
            1,
            3,
        );
        assert!(m.starts_with("[automatic check 1 of 3] `cargo test` failed (exit 101)"));
        assert!(m.len() < MAX_FEEDBACK + 600, "{}", m.len());
        assert!(m.contains("[more failure lines omitted]"));
        assert!(m.contains(UNVERIFIED));
        assert!(m.contains("Do not change existing tests"));
    }

    #[test]
    fn syntax_checks() {
        assert_eq!(
            syntax_check_bytes(Path::new("a.json"), b"{\"a\": 1}"),
            Syntax::Ok
        );
        assert!(matches!(
            syntax_check_bytes(Path::new("a.json"), b"{\"a\": }"),
            Syntax::Error(_)
        ));
        assert_eq!(
            syntax_check_bytes(Path::new("a.toml"), b"a = 1\n"),
            Syntax::Ok
        );
        assert!(matches!(
            syntax_check_bytes(Path::new("a.toml"), b"a = \n"),
            Syntax::Error(_)
        ));
        assert_eq!(
            syntax_check_bytes(Path::new("a.rs"), b"fn x("),
            Syntax::Unknown
        );
        // JSON with comments is not judged as strict JSON.
        for p in [
            "tsconfig.json",
            "web/tsconfig.build.json",
            ".vscode/settings.json",
        ] {
            assert_eq!(
                syntax_check_bytes(Path::new(p), b"{ // c\n}"),
                Syntax::Unknown,
                "{p}"
            );
        }
        assert!(
            syntax_command(Path::new("x/it's.py"))
                .unwrap()
                .contains(r"'x/it'\''s.py'")
        );
        assert!(
            syntax_command(Path::new("a.sh"))
                .unwrap()
                .starts_with("bash -n")
        );
        assert_eq!(syntax_command(Path::new("a.c")), None);
        // Python and bash parser messages are kept.
        let py = "exit: 1\nSTDOUT:\nSTDERR:\n  File \"x.py\", line 2\n    def f(:\n          ^\nSyntaxError: invalid syntax\n";
        assert!(
            failure_lines(py)
                .iter()
                .any(|l| l.starts_with("SyntaxError"))
        );
        let sh =
            "exit: 2\nSTDOUT:\nSTDERR:\nx.sh: line 3: syntax error near unexpected token `fi'\n";
        assert!(
            failure_lines(sh)
                .iter()
                .any(|l| l.contains("syntax error near"))
        );
    }
}
