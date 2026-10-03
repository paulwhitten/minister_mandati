//! `mima-eval probe`: does a model already know the code a repo task is cut
//! from?
//!
//! For each task built from a public repository, the model gets only the
//! repository name and the task's instruction, no files and no tools, and is
//! asked which file holds the bug and what the correct code is. Because the
//! bugs are injected, the correct code is the upstream original: a model that
//! names the file and reproduces the original line has memorized the
//! project, and its score on these tasks partly measures recall. Compare the
//! rates across models, and the repo group's pass rate against the other
//! groups (SWE-Bench Illusion, arXiv 2506.12286).

use serde_json::{Value, json};
use std::fmt::Write;
use std::time::Duration;

use crate::run::Profile;
use crate::task::Suite;

fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The most distinctive line of the original code: the longest line of the
/// `find` text that the injected edit removed or changed.
fn original_line(find: &str, replace: &str) -> Option<String> {
    let replaced: Vec<String> = replace.lines().map(norm).collect();
    find.lines()
        .map(norm)
        .filter(|l| l.len() >= 8 && !replaced.contains(l))
        .max_by_key(|l| l.len())
}

pub fn run(suite: &Suite, p: &Profile) -> Result<String, String> {
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|e| e.to_string())?;
    let key = p.api_key.clone().unwrap_or_default();
    let base = p.base_url.trim_end_matches('/').to_string();
    let mut s = String::new();
    let _ = writeln!(
        s,
        "# Memorization probe: {}\n\nNo files, no tools: the model sees only the repository name and the task instruction.\n",
        p.name
    );
    let _ = writeln!(
        s,
        "| Task | Repository | File named | Original line recalled |"
    );
    let _ = writeln!(s, "|---|---|---|---|");
    let (mut n, mut files, mut lines) = (0, 0, 0);
    for task in &suite.tasks {
        let Some(src) = &task.source else { continue };
        let Some(edit) = src.edits.first() else {
            continue;
        };
        let repo = src.repo.trim_end_matches(".git").trim_end_matches('/');
        let prompt = format!(
            "This is a bug report for the open-source project {repo}. You cannot see the code.\n\n\
             Report:\n{}\n\n\
             From memory of the project's source: which file most likely contains the bug, and \
             what is the correct code of the lines involved? Answer with the file path on the \
             first line, then the correct code in a code block.",
            task.instruction
        );
        let body = json!({ "model": p.model, "max_tokens": 4096, "temperature": 0.0,
                           "messages": [{ "role": "user", "content": prompt }] });
        let answer = rt.block_on(async {
            let r = client
                .post(format!("{base}/chat/completions"))
                .bearer_auth(&key)
                .json(&body)
                .send()
                .await
                .ok()?;
            let v: Value = r.json().await.ok()?;
            v["choices"][0]["message"]["content"]
                .as_str()
                .map(String::from)
        });
        let Some(answer) = answer else {
            let _ = writeln!(s, "| {} | {repo} | error | error |", task.id);
            continue;
        };
        n += 1;
        let name = edit.file.rsplit('/').next().unwrap_or(&edit.file);
        let file_hit = answer.contains(name);
        let line_hit =
            original_line(&edit.find, &edit.replace).is_some_and(|l| norm(&answer).contains(&l));
        files += file_hit as usize;
        lines += line_hit as usize;
        let yn = |b: bool| if b { "yes" } else { "no" };
        let _ = writeln!(
            s,
            "| {} | {repo} | {} | {} |",
            task.id,
            yn(file_hit),
            yn(line_hit)
        );
    }
    if n == 0 {
        return Err("no repository-sourced tasks in this suite".into());
    }
    let _ = writeln!(
        s,
        "\nFile named: {files}/{n}. Original line recalled verbatim (whitespace-normalized): {lines}/{n}.\n\n\
         A named file can come from the report plus general knowledge of the project's layout; a verbatim original line is strong evidence of memorization."
    );
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_changed_original_line() {
        let find = "    while (x) {\n        index--;\n    }";
        let replace = "    while (x) {\n        index -= 2;\n    }";
        assert_eq!(original_line(find, replace).as_deref(), Some("index--;"));
    }
}
