//! Tasks built from third-party git repositories without copying them into
//! mima's repository. A task names a repository, a pinned commit, the
//! expected license, and exact edits that introduce the bug:
//!
//! ```toml
//! [source]
//! repo = "https://github.com/owner/project"
//! commit = "6d9f2443ab071f86e5d9b43025a40929ec41c46c"
//! license = "MIT"
//! [[source.edit]]
//! file = "src/thing.c"
//! find = "..."       # must occur exactly once
//! replace = "..."
//! ```
//!
//! `mima-eval fetch` clones the commit into a local cache (outside the repo)
//! and refuses it unless the license text at that commit matches. Each trial
//! then gets the commit's files via `git archive` (no history, which would
//! contain the fix), with the edits applied. The reference solution is the
//! edits reversed, unless the task has its own `solution/solve.sh`.

use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub repo: String,
    pub commit: String,
    /// Only "MIT" is accepted today; checked against the license file.
    pub license: String,
    #[serde(default, rename = "edit")]
    pub edits: Vec<Edit>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Edit {
    pub file: String,
    pub find: String,
    pub replace: String,
}

/// Where fetched repositories are kept: `$MIMA_EVAL_CACHE`, else
/// `<evals>/cache/repos` next to the tasks directory (gitignored).
pub fn cache_root(task_dir: &Path) -> PathBuf {
    if let Ok(dir) = std::env::var("MIMA_EVAL_CACHE") {
        return PathBuf::from(dir);
    }
    task_dir
        .parent()
        .and_then(Path::parent)
        .map(|evals| evals.join("cache").join("repos"))
        .unwrap_or_else(|| PathBuf::from("evals/cache/repos"))
}

fn repo_dir(root: &Path, src: &Source) -> PathBuf {
    let name: String = src
        .repo
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .rsplit('/')
        .take(2)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("__")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    root.join(format!("{name}.git"))
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Makes sure the pinned commit is in the cache (fetching only that commit)
/// and that its license is what the task declares.
pub fn fetch(src: &Source, root: &Path) -> Result<(), String> {
    if src.commit.len() != 40 || !src.commit.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "commit must be a full 40-hex id, got {:?}",
            src.commit
        ));
    }
    let dir = repo_dir(root, src);
    let have = git(
        &dir,
        &["cat-file", "-e", &format!("{}^{{commit}}", src.commit)],
    )
    .is_ok();
    if !have {
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        if !dir.join("HEAD").exists() {
            git(&dir, &["init", "-q", "--bare"])?;
        }
        eprintln!("  fetching {} @ {}", src.repo, &src.commit[..12]);
        git(
            &dir,
            &["fetch", "-q", "--depth", "1", &src.repo, &src.commit],
        )?;
    }
    check_license(src, &dir)
}

fn check_license(src: &Source, dir: &Path) -> Result<(), String> {
    if src.license != "MIT" {
        return Err(format!(
            "license {:?} is not accepted (only MIT)",
            src.license
        ));
    }
    let names = git(dir, &["ls-tree", "--name-only", &src.commit])?;
    let file = names
        .lines()
        .find(|n| {
            let u = n.to_ascii_uppercase();
            u.starts_with("LICENSE") || u.starts_with("LICENCE") || u.starts_with("COPYING")
        })
        .ok_or(format!(
            "{}: no license file at {}",
            src.repo,
            &src.commit[..12]
        ))?;
    let text = git(dir, &["show", &format!("{}:{file}", src.commit)])?;
    if text.contains("Permission is hereby granted, free of charge") {
        Ok(())
    } else {
        Err(format!("{}: {file} is not the MIT license text", src.repo))
    }
}

/// Writes the commit's files into `work` (no history) and applies the edits.
pub fn export(src: &Source, root: &Path, work: &Path) -> Result<(), String> {
    fetch(src, root)?;
    let dir = repo_dir(root, src);
    let status = Command::new("sh")
        .arg("-c")
        .arg("git -C \"$1\" archive \"$2\" | tar -x -C \"$3\"")
        .arg("sh")
        .arg(&dir)
        .arg(&src.commit)
        .arg(work)
        .status()
        .map_err(|e| format!("archive: {e}"))?;
    if !status.success() {
        return Err(format!("git archive of {} failed", src.repo));
    }
    apply(&src.edits, work, false)
}

/// Applies the edits (or their reverse). Each `find` must occur exactly
/// once, so a task can never silently apply to the wrong place.
pub fn apply(edits: &[Edit], work: &Path, reverse: bool) -> Result<(), String> {
    let ordered: Vec<&Edit> = if reverse {
        edits.iter().rev().collect()
    } else {
        edits.iter().collect()
    };
    for (i, e) in ordered.iter().enumerate() {
        let (from, to) = if reverse {
            (&e.replace, &e.find)
        } else {
            (&e.find, &e.replace)
        };
        let path = work.join(&e.file);
        let text =
            std::fs::read_to_string(&path).map_err(|err| format!("edit {i}: {}: {err}", e.file))?;
        let n = text.matches(from.as_str()).count();
        if n != 1 {
            return Err(format!(
                "edit {i} ({}) matches {n} times; it must match exactly once",
                e.file
            ));
        }
        std::fs::write(&path, text.replacen(from.as_str(), to, 1))
            .map_err(|err| format!("edit {i}: {}: {err}", e.file))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_apply_once_and_reverse() {
        let d = std::env::temp_dir().join(format!("mima-src-edit-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("a.c"), "if (x <= n) {}\n").unwrap();
        let edits = vec![Edit {
            file: "a.c".into(),
            find: "x <= n".into(),
            replace: "x < n".into(),
        }];
        apply(&edits, &d, false).unwrap();
        assert_eq!(
            std::fs::read_to_string(d.join("a.c")).unwrap(),
            "if (x < n) {}\n"
        );
        apply(&edits, &d, true).unwrap();
        assert_eq!(
            std::fs::read_to_string(d.join("a.c")).unwrap(),
            "if (x <= n) {}\n"
        );
        let twice = vec![Edit {
            file: "a.c".into(),
            find: "{".into(),
            replace: "(".into(),
        }];
        std::fs::write(d.join("a.c"), "{ {").unwrap();
        assert!(
            apply(&twice, &d, false)
                .unwrap_err()
                .contains("matches 2 times")
        );
    }

    #[test]
    fn cache_names_are_stable() {
        let s = Source {
            repo: "https://github.com/owner/project.git".into(),
            commit: "x".into(),
            license: "MIT".into(),
            edits: vec![],
        };
        assert_eq!(
            repo_dir(Path::new("/c"), &s),
            PathBuf::from("/c/owner__project.git")
        );
    }
}
