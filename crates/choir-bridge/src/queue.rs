//! Queue-as-bot v0 (plan.md D21, queue stage): speculative merge
//! trains with the host forge's CI as the check signal.
//!
//! v0 is verdict-only: it builds a train commit (base tip + each open
//! PR merged in submission order, conflicts excluded), publishes it as
//! the `choir/train` branch so the forge's CI runs on it, then reports
//! a per-PR verdict as a commit status. It never moves the protected
//! branch — landing the train is the next stage.
//!
//! Everything in this module is local git; the forge API glue lives in
//! [`crate::github`] so the train mechanics stay testable offline.

use std::path::Path;

/// One PR's fate in a train build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrainEntry {
    /// PR number (or any caller-chosen id in tests).
    pub id: u64,
    /// The PR head oid this entry was built from.
    pub head: String,
    /// Whether the merge onto the train succeeded.
    pub merged: bool,
    /// Human-readable note (merge position or the conflict reason).
    pub note: String,
}

/// Result of building one speculative train.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Train {
    /// Tip commit of the train (== base when nothing merged).
    pub tip: String,
    /// Per-PR outcomes, in build order.
    pub entries: Vec<TrainEntry>,
}

/// Runs git in `dir` with a fixed bot identity, returning stdout.
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .arg("-c")
        .arg("user.name=choir-queue")
        .arg("-c")
        .arg("user.email=queue@choir.invalid")
        .arg("-c")
        .arg("commit.gpgsign=false")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| format!("spawn git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

/// Builds a speculative train in `repo` (a non-bare clone whose
/// worktree this function owns): detaches at `base`, then merges each
/// `(id, head)` in order with `--no-ff`. A conflicting PR is excluded
/// (merge aborted) and the train continues without it.
///
/// # Errors
///
/// Git failures other than merge conflicts (missing oids, dirty repo).
pub fn build_train(repo: &Path, base: &str, prs: &[(u64, String)]) -> Result<Train, String> {
    git(repo, &["checkout", "-q", "--detach", base])?;
    let mut entries = Vec::new();
    let mut position = 0usize;
    for (id, head) in prs {
        let msg = format!("choir train: PR #{id}");
        match git(repo, &["merge", "--no-ff", "-q", "-m", &msg, head]) {
            Ok(_) => {
                position += 1;
                entries.push(TrainEntry {
                    id: *id,
                    head: head.clone(),
                    merged: true,
                    note: format!("train position {position}"),
                });
            }
            Err(_) => {
                // Conflict (or unmergeable): drop this PR from the train.
                git(repo, &["merge", "--abort"]).ok();
                entries.push(TrainEntry {
                    id: *id,
                    head: head.clone(),
                    merged: false,
                    note: "conflicts with train".to_string(),
                });
            }
        }
    }
    let tip = git(repo, &["rev-parse", "HEAD"])?.trim().to_string();
    Ok(Train { tip, entries })
}

/// Lands a green train: pushes `tip` to `branch` on the remote at
/// `url` WITHOUT force, so git's fast-forward rule is the race guard —
/// if the branch moved since the train was built, the push is rejected
/// and the caller should rebuild on the next round.
///
/// # Errors
///
/// Push failures, including the non-fast-forward rejection.
pub fn land(repo: &Path, url: &str, tip: &str, branch: &str) -> Result<(), String> {
    git(repo, &["push", "-q", url, &format!("{tip}:refs/heads/{branch}")]).map(|_| ())
}
