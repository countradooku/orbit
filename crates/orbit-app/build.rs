//! Build facts for About, crash reports and the updater: the commit, the
//! profile and the target triple (the updater's platform key).
//!
//! `ORBIT_COMMIT` in the environment wins (release CI sets it); otherwise
//! git is asked, and a checkout without git says "unknown".

use std::path::PathBuf;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned()).filter(|s| !s.is_empty())
}

fn main() {
    println!("cargo:rerun-if-env-changed=ORBIT_COMMIT");
    let commit = std::env::var("ORBIT_COMMIT")
        .ok()
        .filter(|c| !c.trim().is_empty())
        .or_else(|| git(&["rev-parse", "--short=10", "HEAD"]))
        .unwrap_or_else(|| "unknown".to_owned());
    // Rebuild when HEAD moves: HEAD itself (a branch switch) and the ref it
    // points at (a commit), wherever this worktree keeps them.
    for path in ["HEAD", "packed-refs"] {
        if let Some(file) = git(&["rev-parse", "--git-path", path]) {
            println!("cargo:rerun-if-changed={}", PathBuf::from(file).display());
        }
    }
    if let Some(head_ref) = git(&["symbolic-ref", "-q", "HEAD"])
        && let Some(file) = git(&["rev-parse", "--git-path", &head_ref])
    {
        println!("cargo:rerun-if-changed={}", PathBuf::from(file).display());
    }
    println!("cargo:rustc-env=ORBIT_COMMIT={commit}");
    println!("cargo:rustc-env=ORBIT_TARGET={}", std::env::var("TARGET").unwrap_or_default());
    println!("cargo:rustc-env=ORBIT_PROFILE={}", std::env::var("PROFILE").unwrap_or_default());
}
