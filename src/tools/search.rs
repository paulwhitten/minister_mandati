//! Read-only search tools, confined to `security.allowed_paths`:
//! `find_files` (paths by glob) and `search_files` (lines by regex). They
//! need no approval and replace ad-hoc `grep`/`find` shell commands.
//!
//! The walk never follows symlinks (so it cannot leave the sandbox) and
//! skips version-control, build and dependency directories.

use async_trait::async_trait;
use regex::{Regex, RegexBuilder};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

use super::fs::ensure_allowed;
use super::{BaseTool, RefusedSnafu, Result, ToolEnv, ToolSpec};

/// Directories never searched.
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "target",
    "node_modules",
    "__pycache__",
    ".venv",
    "venv",
    ".mypy_cache",
    ".pytest_cache",
    ".tox",
    "build",
    "dist",
];
/// Files larger than this are not searched.
const MAX_FILE_BYTES: u64 = 2 << 20;
/// Longest line shown in search results.
const MAX_LINE_CHARS: usize = 300;
/// Most files visited per call, as a guard against huge trees.
const MAX_FILES_VISITED: usize = 50_000;

fn refuse<T>(message: String) -> Result<T> {
    RefusedSnafu { message }.fail()
}

/// Files under `root` (sorted, relative paths with `/`), skipping symlinks
/// and the directories in `SKIP_DIRS`.
fn walk(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(root.join(&rel)) else {
            continue;
        };
        let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries.into_iter().rev() {
            let Ok(ft) = e.file_type() else { continue };
            let name = e.file_name().to_string_lossy().into_owned();
            let child = rel.join(&name);
            if ft.is_dir() {
                if !SKIP_DIRS.contains(&name.as_str()) {
                    stack.push(child);
                }
            } else if ft.is_file() {
                out.push(child.to_string_lossy().replace('\\', "/"));
                if out.len() >= MAX_FILES_VISITED {
                    return sorted(out);
                }
            }
        }
    }
    sorted(out)
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

/// A glob as a regex: `**` any depth, `*` and `?` within one path segment.
/// A pattern without `/` matches file names at any depth.
pub fn glob_regex(glob: &str) -> std::result::Result<Regex, regex::Error> {
    let mut re = String::from("^");
    if !glob.contains('/') {
        re.push_str("(?:.*/)?");
    }
    let mut chars = glob.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                if chars.peek() == Some(&'/') {
                    chars.next();
                    re.push_str("(?:.*/)?");
                } else {
                    re.push_str(".*");
                }
            }
            '*' => re.push_str("[^/]*"),
            '?' => re.push_str("[^/]"),
            c => re.push_str(&regex::escape(&c.to_string())),
        }
    }
    re.push('$');
    Regex::new(&re)
}

/// The search root (default `.`) checked against the sandbox, how to show
/// paths under it, and, when `path` names a file and `allow_file` is set,
/// that single file (relative to the returned root).
fn root(
    args: &Value,
    allowed: &[String],
    allow_file: bool,
) -> Result<(PathBuf, String, Option<String>)> {
    let shown = args.get("path").and_then(Value::as_str).unwrap_or(".");
    let safe = ensure_allowed(Path::new(shown), allowed)?;
    if !safe.exists() {
        return refuse(format!(
            "{shown} does not exist. Paths are relative to the working directory; \
             use find_files or list_dir to locate it."
        ));
    }
    if safe.is_file() {
        if !allow_file {
            return refuse(format!(
                "{shown} is a file, not a directory; give its directory, or read it with read_file."
            ));
        }
        let parent = safe.parent().map(Path::to_path_buf).unwrap_or_default();
        let name = safe
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let dir_shown = Path::new(shown)
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let prefix = if dir_shown.is_empty() {
            String::new()
        } else {
            format!("{}/", dir_shown.trim_end_matches('/'))
        };
        return Ok((parent, prefix, Some(name)));
    }
    let prefix = if shown == "." || shown.is_empty() {
        String::new()
    } else {
        format!("{}/", shown.trim_end_matches('/'))
    };
    Ok((safe, prefix, None))
}

fn limit(args: &Value, default: usize) -> usize {
    args.get("limit")
        .and_then(Value::as_u64)
        .map_or(default, |l| l.clamp(1, 1000) as usize)
}

pub struct FindFiles {
    allowed: Vec<String>,
}

impl FindFiles {
    pub fn new(allowed: Vec<String>) -> Self {
        Self { allowed }
    }
}

#[async_trait]
impl BaseTool for FindFiles {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "find_files".into(),
            description: "Find files by name pattern (glob). `*.rs` matches at any depth; \
                `src/**/*.c` matches under src. Skips .git, target, node_modules and similar. \
                Read-only; use it instead of `find` or `ls -R`."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Glob, e.g. *.py or src/**/test_*.c" },
                    "path": { "type": "string", "description": "Directory to search (default .)" },
                    "limit": { "type": "integer", "description": "Most results to return (default 200)" }
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn execute(&self, args: &Value, env: &ToolEnv) -> Result<String> {
        let Some(pattern) = args.get("pattern").and_then(Value::as_str) else {
            return refuse(
                "find_files failed: `pattern` is required (a glob such as *.rs).".into(),
            );
        };
        let re = match glob_regex(pattern) {
            Ok(r) => r,
            Err(e) => return refuse(format!("find_files failed: bad pattern {pattern:?}: {e}")),
        };
        let (root, prefix, _) = root(args, &self.allowed, false)?;
        let max = limit(args, 200);
        let hits: Vec<String> = walk(&root).into_iter().filter(|p| re.is_match(p)).collect();
        if hits.is_empty() {
            return Ok(format!("[no files match {pattern:?}]"));
        }
        let mut out = format!("[{} file(s) match {pattern:?}]\n", hits.len());
        let mut shown = 0;
        for h in &hits {
            let line = format!("{prefix}{h}\n");
            if shown >= max || out.len() + line.len() + 80 > env.output_budget {
                break;
            }
            out.push_str(&line);
            shown += 1;
        }
        if shown < hits.len() {
            out.push_str(&format!(
                "[{} more not shown; narrow the pattern or path]",
                hits.len() - shown
            ));
        }
        Ok(out.trim_end().to_string())
    }
}

pub struct SearchFiles {
    allowed: Vec<String>,
}

impl SearchFiles {
    pub fn new(allowed: Vec<String>) -> Self {
        Self { allowed }
    }
}

#[async_trait]
impl BaseTool for SearchFiles {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "search_files".into(),
            description: "Search file contents for a regular expression (Rust regex syntax). \
                Returns path:line: text for each matching line. Optionally limit to files \
                matching a glob. Skips binary files, .git, target and similar. Read-only; use \
                it instead of `grep`."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regular expression, e.g. fn\\s+parse_" },
                    "path": { "type": "string", "description": "Directory to search (default .)" },
                    "glob": { "type": "string", "description": "Only files matching this glob, e.g. *.c" },
                    "ignore_case": { "type": "boolean", "description": "Case-insensitive (default false)" },
                    "limit": { "type": "integer", "description": "Most matching lines to return (default 100)" }
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn execute(&self, args: &Value, env: &ToolEnv) -> Result<String> {
        let Some(pattern) = args.get("pattern").and_then(Value::as_str) else {
            return refuse("search_files failed: `pattern` is required.".into());
        };
        let ignore_case = args
            .get("ignore_case")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let re = match RegexBuilder::new(pattern)
            .case_insensitive(ignore_case)
            .build()
        {
            Ok(r) => r,
            Err(e) => {
                return refuse(format!(
                    "search_files failed: {pattern:?} is not a valid regular expression ({e}). \
                     Escape special characters such as ( [ . * with a backslash to search for \
                     them literally."
                ));
            }
        };
        let filter = match args.get("glob").and_then(Value::as_str) {
            Some(g) => match glob_regex(g) {
                Ok(r) => Some(r),
                Err(e) => return refuse(format!("search_files failed: bad glob {g:?}: {e}")),
            },
            None => None,
        };
        let (root, prefix, single) = root(args, &self.allowed, true)?;
        let max = limit(args, 100);

        let mut lines_out = Vec::new();
        let (mut total, mut files_with) = (0usize, 0usize);
        let files = match single {
            Some(f) => vec![f],
            None => walk(&root),
        };
        for rel in files {
            if filter.as_ref().is_some_and(|f| !f.is_match(&rel)) {
                continue;
            }
            let path = root.join(&rel);
            if std::fs::metadata(&path).map_or(true, |m| m.len() > MAX_FILE_BYTES) {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            if bytes.iter().take(8192).any(|b| *b == 0) {
                continue; // binary
            }
            let text = String::from_utf8_lossy(&bytes);
            let mut any = false;
            for (i, line) in text.lines().enumerate() {
                if re.is_match(line) {
                    any = true;
                    total += 1;
                    if lines_out.len() < max {
                        let shown: String = line.trim_end().chars().take(MAX_LINE_CHARS).collect();
                        lines_out.push(format!("{prefix}{rel}:{}: {shown}", i + 1));
                    }
                }
            }
            files_with += any as usize;
        }
        if total == 0 {
            return Ok(format!("[no matches for {pattern:?}]"));
        }
        let mut out = format!("[{total} match(es) in {files_with} file(s)]\n");
        let mut shown = 0;
        for l in &lines_out {
            if out.len() + l.len() + 100 > env.output_budget {
                break;
            }
            out.push_str(l);
            out.push('\n');
            shown += 1;
        }
        if shown < total {
            out.push_str(&format!(
                "[{} more not shown; narrow the pattern, path or glob]",
                total - shown
            ));
        }
        Ok(out.trim_end().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tree(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mima-search-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        for (p, c) in [
            ("src/main.rs", "fn main() {\n    parse_args();\n}\n"),
            ("src/args.rs", "pub fn parse_args() {}\n// TODO: flags\n"),
            ("src/deep/x.c", "int parse_args(void);\n"),
            ("README.md", "Run it.\n"),
            ("target/debug/junk.rs", "fn parse_args() {}\n"),
            (".git/config", "parse_args\n"),
            ("bin.dat", "a\0parse_args"),
        ] {
            let f = d.join(p);
            fs::create_dir_all(f.parent().unwrap()).unwrap();
            fs::write(f, c).unwrap();
        }
        d
    }

    fn env() -> ToolEnv {
        ToolEnv {
            output_budget: 10_000,
        }
    }

    #[test]
    fn globs() {
        let m = |g: &str, p: &str| glob_regex(g).unwrap().is_match(p);
        assert!(m("*.rs", "src/main.rs"));
        assert!(m("*.rs", "main.rs"));
        assert!(!m("*.rs", "src/main.rsx"));
        assert!(m("src/**/*.c", "src/deep/x.c"));
        assert!(m("src/**/*.c", "src/x.c"));
        assert!(!m("src/*.c", "src/deep/x.c"));
        assert!(m("src/ma?n.rs", "src/main.rs"));
        assert!(m("**", "any/thing"));
    }

    #[tokio::test]
    async fn search_files_accepts_a_file_and_explains_missing_paths() {
        let d = tree("one-file");
        let t = SearchFiles::new(vec![d.display().to_string()]);
        let file = d.join("src/args.rs").display().to_string();
        let out = t
            .execute(&json!({ "pattern": "TODO", "path": file }), &env())
            .await
            .unwrap();
        assert!(out.contains("src/args.rs:2: // TODO: flags"), "{out}");
        assert!(!out.contains("main.rs"), "{out}");
        let missing = d.join("nope").display().to_string();
        let err = t
            .execute(&json!({ "pattern": "x", "path": missing }), &env())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not exist"), "{err}");
    }

    #[tokio::test]
    async fn find_files_skips_build_and_vcs_dirs() {
        let d = tree("find");
        let t = FindFiles::new(vec![d.display().to_string()]);
        let out = t
            .execute(
                &json!({ "pattern": "*.rs", "path": d.display().to_string() }),
                &env(),
            )
            .await
            .unwrap();
        assert!(out.starts_with("[2 file(s) match"), "{out}");
        assert!(out.contains("src/args.rs") && out.contains("src/main.rs"));
        assert!(!out.contains("target"));
    }

    #[tokio::test]
    async fn search_files_finds_lines_and_skips_binary() {
        let d = tree("search");
        let t = SearchFiles::new(vec![d.display().to_string()]);
        let root = d.display().to_string();
        let out = t
            .execute(&json!({ "pattern": r"parse_\w+\(", "path": root }), &env())
            .await
            .unwrap();
        assert!(out.starts_with("[3 match(es) in 3 file(s)]"), "{out}");
        assert!(out.contains("src/main.rs:2:     parse_args();"), "{out}");
        assert!(!out.contains("bin.dat") && !out.contains(".git") && !out.contains("target"));

        let only_c = t
            .execute(
                &json!({ "pattern": "parse", "path": root, "glob": "*.c" }),
                &env(),
            )
            .await
            .unwrap();
        assert!(only_c.starts_with("[1 match(es) in 1 file(s)]"), "{only_c}");

        let ci = t
            .execute(
                &json!({ "pattern": "todo", "path": root, "ignore_case": true }),
                &env(),
            )
            .await
            .unwrap();
        assert!(ci.contains("args.rs:2:"), "{ci}");

        let err = t
            .execute(&json!({ "pattern": "parse(", "path": root }), &env())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a valid regular expression"), "{err}");
    }

    #[tokio::test]
    async fn search_stays_in_the_sandbox() {
        let d = tree("sandbox");
        let t = SearchFiles::new(vec![d.join("src").display().to_string()]);
        let err = t
            .execute(
                &json!({ "pattern": "x", "path": d.display().to_string() }),
                &env(),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("outside the allowed"), "{err}");
    }
}
