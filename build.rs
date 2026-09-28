//! Build metadata for the version string (pebble GWLJ-4c0qq3): the short
//! commit id and a dirty marker, exposed to the crate as
//! `BUILD_GIT_COMMIT` / `BUILD_GIT_DIRTY`. Both degrade to absent when git
//! is unavailable (e.g. building from a tarball), and the version string
//! omits the bracket when the commit is unknown.

use std::process::Command;

fn main() {
    // Re-run when the git HEAD moves so the baked commit id stays fresh. In
    // a linked worktree `.git/HEAD` does not resolve from the package root,
    // so there the worst case is a stale id until any source file changes
    // (and a `git status`-dirty tree always coincides with a source change).
    println!("cargo:rerun-if-changed=.git/HEAD");

    let commit = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output();
    if let Ok(out) = commit
        && out.status.success()
    {
        let commit = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if !commit.is_empty() {
            println!("cargo:rustc-env=BUILD_GIT_COMMIT={commit}");

            let status = Command::new("git").args(["status", "--porcelain"]).output();
            if let Ok(out) = status
                && out.status.success()
                && !out.stdout.is_empty()
            {
                println!("cargo:rustc-env=BUILD_GIT_DIRTY=1");
            }
        }
    }
}
