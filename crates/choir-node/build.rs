//! Stamps the daemon with the commit it was built from.
//!
//! `choirctl status` could already say which *file* is serving; it could
//! never say what that file was built from, and a rebuild that never
//! reached the running process looks identical to one that did. The stamp
//! is what makes that checkable.
//!
//! Two sources, in order:
//!
//! 1. `CHOIR_GIT_HEAD` in the build environment. This is the sound one:
//!    cargo reruns a build script when an env var it declares changes, so
//!    the stamp cannot go stale behind a cached script. The installer sets
//!    it from `git rev-parse HEAD`.
//! 2. Best-effort `git rev-parse HEAD` in the crate's directory, for plain
//!    `cargo build` in a checkout.
//!
//! Neither can be fabricated when git is absent: the stamp is then the
//! literal `unknown`, which the daemon reports as unknown rather than
//! guessing. The dirty flag is best-effort in case 2 only and is
//! deliberately not trusted: cargo has no way to rerun this script on
//! every source edit, so `dirty` can be stale where `commit` cannot.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=CHOIR_GIT_HEAD");

    let (commit, source, dirty) = match std::env::var("CHOIR_GIT_HEAD") {
        Ok(head) if !head.trim().is_empty() => (head.trim().to_string(), "env", false),
        _ => match git(&["rev-parse", "HEAD"]) {
            Some(head) => {
                // Rerun when HEAD moves. `--git-path` resolves through
                // worktrees and separate git dirs, where a hardcoded
                // `../../.git/HEAD` would silently watch nothing.
                for path in ["HEAD", "packed-refs"] {
                    if let Some(resolved) = git(&["rev-parse", "--git-path", path]) {
                        println!("cargo:rerun-if-changed={resolved}");
                    }
                }
                if let Some(reference) = git(&["symbolic-ref", "-q", "HEAD"]) {
                    if let Some(resolved) = git(&["rev-parse", "--git-path", &reference]) {
                        println!("cargo:rerun-if-changed={resolved}");
                    }
                }
                // Untracked files are excluded: a module can only reach the
                // build through a tracked file naming it, and counting them
                // stamped the VM's build `+dirty` over its own target/
                // artifacts sitting in the checkout.
                let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
                    .is_some_and(|s| !s.is_empty());
                (head, "git", dirty)
            }
            None => ("unknown".to_string(), "unavailable", false),
        },
    };

    println!("cargo:rustc-env=CHOIR_BUILD_COMMIT={commit}");
    println!("cargo:rustc-env=CHOIR_BUILD_SOURCE={source}");
    println!("cargo:rustc-env=CHOIR_BUILD_DIRTY={dirty}");
}

/// Trimmed stdout of a git command run in the crate directory, or `None`
/// if git is missing or the command failed.
fn git(args: &[&str]) -> Option<String> {
    let dir = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let out = Command::new("git").current_dir(dir).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    Some(text.trim().to_string())
}
