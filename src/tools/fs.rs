//! Filesystem tools, sandboxed to `security.allowed_paths`.

use async_trait::async_trait;
use serde_json::{Value, json};
use snafu::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::edit::{FileTracker, require_fresh, unified_diff, whole_file_blocks};
use super::{
    BaseTool, DedupePolicy, FsSnafu, MissingArgSnafu, PathNotAllowedSnafu, RefusedSnafu, Result,
    ToolEnv, ToolSpec,
};

/// Resolve `target` and verify it lives under one of the allowed roots.
/// Canonicalizes the nearest existing ancestor so `..` and symlinks cannot escape.
pub fn ensure_allowed(target: &Path, roots: &[String]) -> Result<PathBuf> {
    let resolved = canonical_or_parent(target)?;
    for root in roots {
        if let Ok(root_canon) = Path::new(root).canonicalize()
            && resolved.starts_with(&root_canon)
        {
            return Ok(resolved);
        }
    }
    PathNotAllowedSnafu {
        path: target.display().to_string(),
    }
    .fail()
}

fn canonical_or_parent(target: &Path) -> Result<PathBuf> {
    if target.exists() {
        return target.canonicalize().context(FsSnafu {
            path: target.display().to_string(),
        });
    }
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file = target.file_name().context(PathNotAllowedSnafu {
        path: target.display().to_string(),
    })?;
    let parent_canon = parent.canonicalize().context(FsSnafu {
        path: parent.display().to_string(),
    })?;
    Ok(parent_canon.join(file))
}

fn arg_str<'a>(args: &'a Value, tool: &str, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .context(MissingArgSnafu {
            tool: tool.to_string(),
            arg: key.to_string(),
        })
}

/// Lines longer than this are cut in `read_file` output.
const MAX_LINE_CHARS: usize = 2_000;

/// Writes via a temporary file in the same directory and a rename, so a
/// crash never leaves a half-written file. Keeps an existing file's
/// permissions.
pub async fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let ctx = || FsSnafu {
        path: path.display().to_string(),
    };
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.mima-tmp-{}", std::process::id()));
    let perms = tokio::fs::metadata(path)
        .await
        .ok()
        .map(|m| m.permissions());
    let result = async {
        let mut f = tokio::fs::File::create(&tmp).await.context(ctx())?;
        tokio::io::AsyncWriteExt::write_all(&mut f, bytes)
            .await
            .context(ctx())?;
        f.sync_all().await.context(ctx())?;
        if let Some(p) = perms {
            tokio::fs::set_permissions(&tmp, p).await.context(ctx())?;
        }
        tokio::fs::rename(&tmp, path).await.context(ctx())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    result
}

/// `read_file` output: `cat -n`-style numbered lines (number, TAB, text) from
/// `offset` (1-based), at most `limit` lines and `budget` bytes, with a
/// header and, if more remains, where to continue.
fn number_lines(
    shown: &str,
    content: &str,
    offset: usize,
    limit: Option<usize>,
    budget: usize,
) -> String {
    let all: Vec<&str> = content.lines().collect();
    let total = all.len();
    if total == 0 {
        return format!("[{shown} is empty]");
    }
    let first = offset.max(1);
    if first > total {
        return format!("[{shown} has {total} lines; offset {first} is past the end]");
    }
    let mut body = String::new();
    let mut last = first - 1;
    for (i, line) in all.iter().enumerate().skip(first - 1) {
        if limit.is_some_and(|l| i + 1 - first >= l) {
            break;
        }
        let text = if line.chars().count() > MAX_LINE_CHARS {
            let cut: String = line.chars().take(MAX_LINE_CHARS).collect();
            format!("{cut} [line truncated]")
        } else {
            (*line).to_string()
        };
        let entry = format!("{:>6}\t{text}\n", i + 1);
        // Leave room for the header and footer; always show at least one line.
        if last >= first && body.len() + entry.len() + 200 > budget {
            break;
        }
        body.push_str(&entry);
        last = i + 1;
    }
    let mut out = format!("[{shown} lines {first}-{last} of {total}]\n{body}");
    if last < total {
        out.push_str(&format!(
            "[{} more lines; next: offset={}]",
            total - last,
            last + 1
        ));
    }
    out
}

pub struct ReadFile {
    allowed: Vec<String>,
    tracker: Arc<FileTracker>,
}

impl ReadFile {
    pub fn new(allowed: Vec<String>, tracker: Arc<FileTracker>) -> Self {
        Self { allowed, tracker }
    }
}

#[async_trait]
impl BaseTool for ReadFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".into(),
            description: "Read a text file. Each output line starts with its line number and a \
                TAB (like `cat -n`); that prefix is NOT part of the file. Large files are shown \
                in parts: use offset/limit to read a range."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path to read" },
                    "offset": { "type": "integer", "description": "First line to show, 1-based (default 1)" },
                    "limit": { "type": "integer", "description": "Maximum number of lines to show" }
                },
                "required": ["path"]
            }),
        }
    }

    #[tracing::instrument(skip_all)]
    async fn execute(&self, args: &Value, env: &ToolEnv) -> Result<String> {
        let shown = arg_str(args, "read_file", "path")?;
        let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(1) as usize;
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|l| l as usize);
        let safe = ensure_allowed(Path::new(shown), &self.allowed)?;
        let bytes = tokio::fs::read(&safe).await.context(FsSnafu {
            path: safe.display().to_string(),
        })?;
        let Ok(content) = String::from_utf8(bytes) else {
            return RefusedSnafu {
                message: format!("read_file failed: {shown} is not valid UTF-8 text."),
            }
            .fail();
        };
        self.tracker.record(&safe, content.as_bytes());
        Ok(number_lines(
            shown,
            &content,
            offset,
            limit,
            env.output_budget,
        ))
    }
}

pub struct WriteFile {
    allowed: Vec<String>,
    tracker: Arc<FileTracker>,
}

impl WriteFile {
    pub fn new(allowed: Vec<String>, tracker: Arc<FileTracker>) -> Self {
        Self { allowed, tracker }
    }

    /// Validates the call: new files are always allowed; overwriting an
    /// existing file requires a current read. Returns the path, the content
    /// to write, and the existing content (if any).
    async fn prepare(&self, args: &Value) -> Result<(PathBuf, String, String, Option<String>)> {
        let shown = arg_str(args, "write_file", "path")?.to_string();
        let content = arg_str(args, "write_file", "content")?.to_string();
        let safe = ensure_allowed(Path::new(&shown), &self.allowed)?;
        let existing = match tokio::fs::read(&safe).await {
            Ok(bytes) => {
                require_fresh(&self.tracker, "write_file", &shown, &safe, &bytes)?;
                Some(String::from_utf8_lossy(&bytes).into_owned())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(e).context(FsSnafu {
                    path: safe.display().to_string(),
                });
            }
        };
        Ok((safe, shown, content, existing))
    }
}

#[async_trait]
impl BaseTool for WriteFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write_file".into(),
            description: "Create a new text file, or completely rewrite a small one. To change \
                part of an existing file, use edit_file instead. Overwriting an existing file \
                requires reading it first."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path to write" },
                    "content": { "type": "string", "description": "Full file contents" }
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn preview(&self, args: &Value) -> Result<Option<String>> {
        let (_, shown, content, existing) = self.prepare(args).await?;
        let old = existing.unwrap_or_default();
        let blocks = whole_file_blocks(&old, &content);
        if blocks.is_empty() {
            return Ok(Some(format!("Write {shown}  (no changes)")));
        }
        let (diff, added, removed) = unified_diff(&shown, &old, &content, &blocks);
        let what = if removed == 0 && old.is_empty() {
            "Create"
        } else {
            "Overwrite"
        };
        Ok(Some(format!(
            "{what} {shown}  (+{added} -{removed})\n{diff}"
        )))
    }

    #[tracing::instrument(skip_all)]
    async fn execute(&self, args: &Value, _env: &ToolEnv) -> Result<String> {
        let (safe, _, content, _) = self.prepare(args).await?;
        atomic_write(&safe, content.as_bytes()).await?;
        self.tracker.record(&safe, content.as_bytes());
        Ok(format!(
            "wrote {} bytes to {}",
            content.len(),
            safe.display()
        ))
    }

    fn dedupe_policy(&self) -> DedupePolicy {
        // Overwriting with identical content is a genuine no-op.
        DedupePolicy::SkipIfIdenticalSuccess
    }
}

pub struct ListDir {
    allowed: Vec<String>,
}

impl ListDir {
    pub fn new(allowed: Vec<String>) -> Self {
        Self { allowed }
    }
}

#[async_trait]
impl BaseTool for ListDir {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "list_dir".into(),
            description: "List the entries of a directory (directories end with '/').".into(),
            parameters: json!({
                "type": "object",
                "properties": { "path": { "type": "string", "description": "Directory path (defaults to '.')" } }
            }),
        }
    }

    #[tracing::instrument(skip_all)]
    async fn execute(&self, args: &Value, _env: &ToolEnv) -> Result<String> {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let safe = ensure_allowed(Path::new(path), &self.allowed)?;
        let mut entries = tokio::fs::read_dir(&safe).await.context(FsSnafu {
            path: safe.display().to_string(),
        })?;
        let mut names = Vec::new();
        while let Some(entry) = entries.next_entry().await.context(FsSnafu {
            path: safe.display().to_string(),
        })? {
            let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
            let suffix = if is_dir { "/" } else { "" };
            names.push(format!("{}{}", entry.file_name().to_string_lossy(), suffix));
        }
        names.sort();
        Ok(names.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A fresh directory under the system temp dir, with an `inside/` sandbox
    /// root and an `outside/` sibling holding a secret file.
    fn fixture(name: &str) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("mima-sandbox-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("inside/sub")).unwrap();
        fs::create_dir_all(base.join("outside")).unwrap();
        fs::write(base.join("inside/ok.txt"), "ok").unwrap();
        fs::write(base.join("outside/secret.txt"), "secret").unwrap();
        (base.join("inside"), base)
    }

    fn roots(root: &Path) -> Vec<String> {
        vec![root.display().to_string()]
    }

    #[test]
    fn allows_existing_and_new_files_inside() {
        let (root, _) = fixture("inside");
        assert!(ensure_allowed(&root.join("ok.txt"), &roots(&root)).is_ok());
        assert!(ensure_allowed(&root.join("sub/new.txt"), &roots(&root)).is_ok());
    }

    #[test]
    fn rejects_dotdot_escape() {
        let (root, _) = fixture("dotdot");
        let target = root.join("../outside/secret.txt");
        assert!(matches!(
            ensure_allowed(&target, &roots(&root)),
            Err(super::super::Error::PathNotAllowed { .. })
        ));
        let new_target = root.join("sub/../../outside/new.txt");
        assert!(ensure_allowed(&new_target, &roots(&root)).is_err());
    }

    #[test]
    fn rejects_absolute_path_outside() {
        let (root, base) = fixture("absolute");
        assert!(ensure_allowed(&base.join("outside/secret.txt"), &roots(&root)).is_err());
        assert!(ensure_allowed(Path::new("/etc/passwd"), &roots(&root)).is_err());
    }

    #[test]
    fn rejects_symlink_escape() {
        let (root, base) = fixture("symlink");
        std::os::unix::fs::symlink(base.join("outside"), root.join("link")).unwrap();
        assert!(ensure_allowed(&root.join("link/secret.txt"), &roots(&root)).is_err());
        // A new file written through the symlinked directory must also be refused.
        assert!(ensure_allowed(&root.join("link/new.txt"), &roots(&root)).is_err());
    }

    #[test]
    fn root_prefix_is_not_a_string_prefix() {
        // `/tmp/x/inside` must not admit `/tmp/x/inside-evil`.
        let (root, base) = fixture("prefix");
        fs::create_dir_all(base.join("inside-evil")).unwrap();
        fs::write(base.join("inside-evil/f.txt"), "x").unwrap();
        assert!(ensure_allowed(&base.join("inside-evil/f.txt"), &roots(&root)).is_err());
    }

    // ---- read_file / edit_file / write_file working together ----

    use crate::tools::edit::EditFile;
    use serde_json::json;

    fn env(budget: usize) -> ToolEnv {
        ToolEnv {
            output_budget: budget,
        }
    }

    struct Tools {
        read: ReadFile,
        edit: EditFile,
        write: WriteFile,
    }

    fn tools(root: &Path) -> Tools {
        let allowed = roots(root);
        let t = FileTracker::shared();
        Tools {
            read: ReadFile::new(allowed.clone(), t.clone()),
            edit: EditFile::new(allowed.clone(), t.clone()),
            write: WriteFile::new(allowed, t),
        }
    }

    #[test]
    fn numbers_lines_and_pages_within_budget() {
        let content: String = (1..=100).map(|i| format!("line {i}\n")).collect();
        let out = number_lines("f.txt", &content, 1, Some(3), 10_000);
        assert_eq!(
            out,
            "[f.txt lines 1-3 of 100]\n     1\tline 1\n     2\tline 2\n     3\tline 3\n[97 more lines; next: offset=4]"
        );
        let out = number_lines("f.txt", &content, 99, None, 10_000);
        assert!(out.starts_with("[f.txt lines 99-100 of 100]") && !out.contains("next:"));
        // A small budget stops at a whole line and says where to continue.
        let out = number_lines("f.txt", &content, 1, None, 300);
        assert!(out.contains("next: offset="), "{out}");
        assert!(out.len() <= 300, "{}", out.len());
        assert_eq!(number_lines("e.txt", "", 1, None, 100), "[e.txt is empty]");
    }

    #[tokio::test]
    async fn edit_requires_a_current_read() {
        let (root, _) = fixture("edit-flow");
        let f = root.join("code.rs");
        fs::write(&f, "fn main() {\n    let x = 1;\n}\n").unwrap();
        let path = f.display().to_string();
        let t = tools(&root);
        let edit = json!({ "path": path, "old_string": "let x = 1;", "new_string": "let x = 2;" });

        let err = t
            .edit
            .execute(&edit, &env(10_000))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("read") && err.contains("before"), "{err}");

        t.read
            .execute(&json!({ "path": path }), &env(10_000))
            .await
            .unwrap();
        let preview = t.edit.preview(&edit).await.unwrap().unwrap();
        assert!(
            preview.contains("-    let x = 1;\n+    let x = 2;"),
            "{preview}"
        );
        let ok = t.edit.execute(&edit, &env(10_000)).await.unwrap();
        assert!(
            ok.starts_with(&format!("Edited {path}: replaced lines 2-2 with 1 lines.")),
            "{ok}"
        );
        assert_eq!(
            fs::read_to_string(&f).unwrap(),
            "fn main() {\n    let x = 2;\n}\n"
        );

        // Our own edit keeps the file current: a second edit needs no re-read.
        let again = json!({ "path": path, "old_string": "let x = 2;", "new_string": "let x = 3;" });
        t.edit.execute(&again, &env(10_000)).await.unwrap();

        // A change made outside mima makes the next edit refuse.
        fs::write(&f, "fn main() {\n    let x = 99;\n}\n").unwrap();
        let stale =
            json!({ "path": path, "old_string": "let x = 99;", "new_string": "let x = 4;" });
        let err = t
            .edit
            .execute(&stale, &env(10_000))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("changed on disk"), "{err}");
    }

    #[tokio::test]
    async fn write_creates_freely_but_overwrites_only_after_a_read() {
        let (root, _) = fixture("write-flow");
        let f = root.join("new.txt");
        let path = f.display().to_string();
        let t = tools(&root);

        let create = json!({ "path": path, "content": "a\nb\n" });
        let preview = t.write.preview(&create).await.unwrap().unwrap();
        assert!(
            preview.starts_with(&format!("Create {path}  (+2 -0)")),
            "{preview}"
        );
        t.write.execute(&create, &env(10_000)).await.unwrap();

        // Written by us, so it is known; overwriting it is allowed.
        let rewrite = json!({ "path": path, "content": "a\nB\n" });
        t.write.execute(&rewrite, &env(10_000)).await.unwrap();

        // A file never read cannot be overwritten.
        let other = root.join("ok.txt").display().to_string();
        let err = t
            .write
            .execute(&json!({ "path": other, "content": "x" }), &env(10_000))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("write_file refused"), "{err}");
        assert_eq!(fs::read_to_string(root.join("ok.txt")).unwrap(), "ok");
    }

    #[tokio::test]
    async fn edit_errors_are_actionable_and_change_nothing() {
        let (root, _) = fixture("edit-errors");
        let f = root.join("dup.txt");
        fs::write(&f, "x = 1\ny = 2\nx = 1\n").unwrap();
        let path = f.display().to_string();
        let t = tools(&root);
        t.read
            .execute(&json!({ "path": path }), &env(10_000))
            .await
            .unwrap();

        let dup = json!({ "path": path, "old_string": "x = 1", "new_string": "x = 3" });
        let err = t.edit.preview(&dup).await.unwrap_err().to_string();
        assert!(
            err.contains("occurs 2 times") && err.contains("replace_all=true"),
            "{err}"
        );

        let missing = json!({ "path": root.join("nope.txt").display().to_string(),
                              "old_string": "a", "new_string": "b" });
        let err = t.edit.preview(&missing).await.unwrap_err().to_string();
        assert!(err.contains("does not exist. Use write_file"), "{err}");

        // A tool-call parser may turn the text `null` into JSON null.
        let null = json!({ "path": path, "old_string": null, "new_string": "b" });
        let err = t.edit.preview(&null).await.unwrap_err().to_string();
        assert!(err.contains("`old_string` is missing or null"), "{err}");

        assert_eq!(fs::read_to_string(&f).unwrap(), "x = 1\ny = 2\nx = 1\n");
    }
}
