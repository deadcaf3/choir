//! The git speculator (D5): the second implementation of
//! [`Speculator`], and the one a production caller wants.
//!
//! A state is a commit id and a merge is `git merge --no-ff` in a
//! worktree this owns. That makes the queue's window, its
//! dependency-aware ejection and its refusal to land a change twice
//! apply to a real repository instead of to one file's text, which is
//! the whole reason the seam exists.
//!
//! **This owns the worktree it is given.** Every [`Speculator::step`]
//! detaches it and forces it to a speculative state, so a repository
//! anyone else is working in is the wrong argument. A clone made for
//! the queue is the right one.
//!
//! Nothing here is speculative in git's sense of the word: the commits
//! it writes are ordinary objects in that repository, unreferenced by
//! any branch until a caller decides to move one. Refusing to move a
//! branch is not this type's job; it never touches a ref.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use choir_hash::ContentHash;

use crate::speculate::{Speculator, Step};
use crate::Change;

/// A speculator over a real repository: states are commit ids.
pub struct GitSpeculator {
    repo: PathBuf,
}

/// Runs git in `dir` under a fixed identity, returning stdout.
///
/// The identity is pinned, and signing is off, because these commits
/// are the queue's own and are made without a person present. A machine
/// inheriting the operator's `user.email` would author speculative
/// merges under a human's name, which is the D60 rule one layer down.
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-c")
        .arg("user.name=choir-queue")
        .arg("-c")
        .arg("user.email=queue@choir.invalid")
        .arg("-c")
        .arg("commit.gpgsign=false")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("spawn git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

impl GitSpeculator {
    /// A speculator over `repo`, a non-bare clone whose worktree it owns.
    #[must_use]
    pub fn new(repo: PathBuf) -> Self {
        Self { repo }
    }

    /// Whether `state` names a commit this repository holds.
    ///
    /// The queue's base comes from its caller, and a base that is not a
    /// commit would otherwise be discovered as a merge failure against
    /// the first change submitted — reported as that author's problem.
    ///
    /// # Errors
    ///
    /// The oid is unknown, unparseable, or not a commit.
    pub fn verify(&self, state: &str) -> Result<String, String> {
        git(
            &self.repo,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{state}^{{commit}}"),
            ],
        )
        .map(|out| out.trim().to_string())
        .map_err(|why| {
            if why.is_empty() {
                format!("{state} is not a commit in this repository")
            } else {
                why
            }
        })
    }
}

impl Speculator for GitSpeculator {
    fn name(&self) -> &'static str {
        "git"
    }

    /// `base` is ignored: git finds the merge base itself, and the one
    /// it finds is better than the one the caller declared. The
    /// argument exists for the text implementation, which has no way to
    /// find it.
    fn step(&mut self, _base: &str, onto: &str, proposed: &str) -> Step {
        // `--force` is load-bearing and not belt-and-braces: it is what
        // clears a `MERGE_HEAD` an earlier run left behind. Without it
        // git refuses the next merge with "you have not concluded your
        // merge" -- the same nonzero exit a conflict gets -- and
        // somebody else's debris would be reported as this author's
        // conflict. Verified rather than assumed; an explicit `merge
        // --abort` here was removed after a mutation showed it changed
        // nothing.
        if let Err(why) = git(&self.repo, &["checkout", "-q", "--detach", "--force", onto]) {
            return Step::Unavailable(format!("cannot detach at {onto}: {why}"));
        }
        let message = format!("choir queue: speculative merge of {proposed}");
        match git(
            &self.repo,
            &["merge", "--no-ff", "-q", "-m", &message, proposed],
        ) {
            Ok(_) => match git(&self.repo, &["rev-parse", "HEAD"]) {
                Ok(oid) => Step::Advanced(oid.trim().to_string()),
                Err(why) => Step::Unavailable(format!("merge left no head: {why}")),
            },
            Err(why) => {
                // git exits nonzero for a conflict and for being handed
                // an oid it does not have, and the queue's response to
                // those must differ: one evicts an author, the other is
                // ours. `MERGE_HEAD` exists only in the first case, so
                // it is the question to ask rather than the stderr text,
                // which is localized and version-dependent.
                let conflicted = git(
                    &self.repo,
                    &["rev-parse", "--verify", "--quiet", "MERGE_HEAD"],
                )
                .is_ok();
                let _ = git(&self.repo, &["merge", "--abort"]);
                if conflicted {
                    Step::Conflict
                } else {
                    Step::Unavailable(format!("cannot merge {proposed}: {why}"))
                }
            }
        }
    }

    /// The patch identity of `base..proposed`, which is what survives a
    /// rebase: the train rewriting the tip under an author gives the
    /// same edit a new commit id and the same identity here.
    ///
    /// An unreadable pair yields a unique identity rather than a shared
    /// one. Two changes we could not read are not thereby the same
    /// change, and collapsing them would make the queue refuse the
    /// second as already landed.
    fn identity(&self, change: &Change) -> String {
        let unknown = || {
            ContentHash::blake3(
                format!("unreadable:{}:{}", change.base, change.proposed).as_bytes(),
            )
            .to_hex()
        };
        let Ok(diff) = git(
            &self.repo,
            &["diff", "--no-color", "-U0", &change.base, &change.proposed],
        ) else {
            return unknown();
        };
        // Deliberately the same normalization
        // [`choir_merge::normalized_diff`] performs for text, reached
        // with git's own diff: keep the added and removed lines and the
        // path they belong to, drop everything positional. Dropping
        // context is not incidental -- a three-line file rebased onto a
        // change to its last line has different context for the same
        // edit, so keeping it would break identity in exactly the case
        // identity exists for. The filter is what enforces that, since
        // a context line starts with a space; `-U0` only stops git
        // producing bytes we would discard, and a mutation removing it
        // is correctly invisible.
        //
        // Computed from our own bytes rather than by piping into `git
        // patch-id`: the diff is already in hand, and a second process
        // reading a pipe we are still writing is a deadlock shape this
        // workspace has been bitten by before.
        let stripped: String = diff
            .lines()
            .filter(|l| {
                l.starts_with("diff --git ")
                    || ((l.starts_with('+') || l.starts_with('-'))
                        && !l.starts_with("+++")
                        && !l.starts_with("---"))
            })
            .map(|l| format!("{l}\n"))
            .collect();
        ContentHash::blake3(stripped.as_bytes()).to_hex()
    }

    /// The commit id, as a git-codec [`ContentHash`].
    ///
    /// # Panics
    ///
    /// If `state` is not a git object id. Every state after the first
    /// is one this type produced with `rev-parse`; the first is the
    /// caller's base, which [`GitSpeculator::verify`] exists to check
    /// before a queue is built on it.
    fn subject(&self, state: &str) -> ContentHash {
        ContentHash::from_git_oid(state)
            .unwrap_or_else(|| panic!("speculative state `{state}` is not a git object id"))
    }
}
