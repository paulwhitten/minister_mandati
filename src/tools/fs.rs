//! Filesystem tools, sandboxed to `security.allowed_paths`.

use async_trait::async_trait;
use serde_json::{Value, json};
use snafu::prelude::*;
use std::path::{Path, PathBuf};

use super::{
    BaseTool, DedupePolicy, FsSnafu, MissingArgSnafu, PathNotAllowedSnafu, Result, ToolSpec,
};

/// Resolve `target` and verify it lives under one of the allowed roots.
/// Canonicalizes the nearest existing ancestor so `..` and symlinks cannot escape.
fn ensure_allowed(target: &Path, roots: &[String]) -> Result<PathBuf> {
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

pub struct ReadFile {
    allowed: Vec<String>,
}

impl ReadFile {
    pub fn new(allowed: Vec<String>) -> Self {
        Self { allowed }
    }
}

#[async_trait]
impl BaseTool for ReadFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".into(),
            description: "Read a UTF-8 text file and return its contents.".into(),
            parameters: json!({
                "type": "object",
                "properties": { "path": { "type": "string", "description": "File path to read" } },
                "required": ["path"]
            }),
        }
    }

    #[tracing::instrument(skip_all)]
    async fn execute(&self, args: &Value) -> Result<String> {
        let path = arg_str(args, "read_file", "path")?;
        let safe = ensure_allowed(Path::new(path), &self.allowed)?;
        tokio::fs::read_to_string(&safe).await.context(FsSnafu {
            path: safe.display().to_string(),
        })
    }
}

pub struct WriteFile {
    allowed: Vec<String>,
}

impl WriteFile {
    pub fn new(allowed: Vec<String>) -> Self {
        Self { allowed }
    }
}

#[async_trait]
impl BaseTool for WriteFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write_file".into(),
            description: "Create or overwrite a text file with the given contents.".into(),
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

    #[tracing::instrument(skip_all)]
    async fn execute(&self, args: &Value) -> Result<String> {
        let path = arg_str(args, "write_file", "path")?;
        let content = arg_str(args, "write_file", "content")?;
        let safe = ensure_allowed(Path::new(path), &self.allowed)?;
        tokio::fs::write(&safe, content).await.context(FsSnafu {
            path: safe.display().to_string(),
        })?;
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
    async fn execute(&self, args: &Value) -> Result<String> {
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
}
