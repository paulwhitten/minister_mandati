//! Build metadata for the audit record (docs/design/design-audit-record.md):
//! the commit, whether the tree had uncommitted changes, the compiler, the
//! target and the profile. Without git (an exported source tree) the commit
//! is "unknown".

use std::path::Path;
use std::process::Command;

fn main() {
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let commit = git(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let dirty = match git(&["status", "--porcelain", "--untracked-files=no"]) {
        Some(s) if s.is_empty() => "false",
        Some(_) => "true",
        None => "unknown",
    };
    let rustc = std::env::var("RUSTC")
        .ok()
        .and_then(|r| Command::new(r).arg("-V").output().ok())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    println!("cargo:rustc-env=MIMA_GIT_COMMIT={commit}");
    println!("cargo:rustc-env=MIMA_GIT_DIRTY={dirty}");
    println!("cargo:rustc-env=MIMA_RUSTC={rustc}");
    println!("cargo:rustc-env=MIMA_TARGET={}", env("TARGET"));
    println!("cargo:rustc-env=MIMA_PROFILE={}", env("PROFILE"));
    // Rebuild the metadata when sources or the checked-out commit change.
    // Only existing paths: a missing one would rerun this on every build.
    for p in [
        "src",
        "build.rs",
        "Cargo.toml",
        "Cargo.lock",
        ".git/HEAD",
        ".git/index",
    ] {
        if Path::new(p).exists() {
            println!("cargo:rerun-if-changed={p}");
        }
    }
}
