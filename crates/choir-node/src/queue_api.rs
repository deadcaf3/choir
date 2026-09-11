//! Driving the node's merge queue from outside (D5, D68).
//!
//! [`crate::Platform::run_proposal_queue`] is the round; this is how
//! anybody asks for one. A single endpoint, `POST /api/queue/run`, and
//! deliberately no timer: a node that spends CI on its own schedule
//! surprises whoever pays for it, and a round that only a clock can
//! start is a round no test can reach without waiting on one. An
//! operator's cron, a hook, or a person decides the cadence.
//!
//! The queue's trees are made here rather than by the operator, because
//! the one contract that matters cannot be checked once it is wrong:
//! the tree a round speculates in must share the served repository's
//! object store, or the log names merge commits git cannot see and
//! [`crate::Platform::reconcile_git_refs`] compensates the whole round
//! back out. A `git worktree` of the bare repo satisfies it by
//! construction; a clone that looks identical does not.

use std::path::{Path, PathBuf};

use choir_queue::differential_ledger::CommandSpec;

/// What a node needs before it can run a round.
///
/// Set by `--queue-tree` and `--ci-command`. Absent, `/api/queue/run`
/// answers 501: a node with no CI command has no way to decide whether
/// a candidate is good, and a queue that landed everything unchecked
/// would be a worse `git push`.
#[derive(Debug, Clone)]
pub struct QueueConfig {
    /// Scratch root the queue owns. Worktrees are made under it, one
    /// per `repo:branch`, and reused between rounds.
    pub tree: PathBuf,
    /// What to run against each candidate state.
    pub command: CommandSpec,
}

/// Where one target's two trees live.
///
/// Two, not one. The speculator forces its tree to each candidate state
/// in turn, and the executor gives every job in a batch its own tree
/// (D18); sharing one directory between them would have the executor
/// checking out over the merge in progress.
struct Trees {
    speculation: PathBuf,
    jobs: PathBuf,
}

impl QueueConfig {
    fn trees(&self, repo: &str, branch: &str) -> Trees {
        // The repo name carries a slash and a `.git`; neither is a
        // legal path component here, so it is flattened rather than
        // joined, which also keeps a repo called `a/b` from colliding
        // with one called `a-b` in the same root.
        let slug = format!(
            "{}@{}",
            repo.replace(['/', '\\'], "_"),
            branch.replace(['/', '\\'], "_")
        );
        let base = self.tree.join(slug);
        Trees {
            speculation: base.join("speculation"),
            jobs: base.join("jobs"),
        }
    }
}

/// Ensures `at` is a worktree of the bare repository at `repo`.
///
/// Idempotent: an existing worktree is reused, because the alternative
/// is making one per round and paying a full checkout each time. A
/// directory that exists but is not a worktree of this repository is an
/// error rather than something to repair, since removing whatever a
/// caller put there is not this function's decision to make.
fn ensure_worktree(repo: &Path, at: &Path, branch: &str) -> Result<(), String> {
    if at.join(".git").exists() {
        return Ok(());
    }
    if at.exists() {
        return Err(format!(
            "{} exists and is not a worktree of this repository",
            at.display()
        ));
    }
    if let Some(parent) = at.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let out = std::process::Command::new("git")
        .args(["worktree", "add", "--detach"])
        .arg(at)
        .arg(branch)
        .current_dir(repo)
        .output()
        .map_err(|e| format!("spawn git: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// The rounds currently running, by `<repo>:<branch>`.
///
/// A second request for a target already in flight is refused rather
/// than queued behind it. Two overlapping rounds would each read the
/// branch, speculate from it, and race to land: the loser's whole round
/// is void by construction, so the CI it spent was wasted before it
/// started. Refusing says so immediately.
#[derive(Debug, Default)]
pub struct InFlight(std::sync::Mutex<std::collections::BTreeSet<String>>);

impl InFlight {
    /// Claims `target`, or reports that somebody else holds it.
    fn claim(&self, target: &str) -> Option<Claim<'_>> {
        let mut held = self.0.lock().expect("in-flight lock");
        held.insert(target.to_string()).then(|| Claim {
            owner: self,
            target: target.to_string(),
        })
    }
}

/// Releases its target when the round ends, however it ends.
struct Claim<'a> {
    owner: &'a InFlight,
    target: String,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.owner
            .0
            .lock()
            .expect("in-flight lock")
            .remove(&self.target);
    }
}

/// Runs one round for the target named in `body`, as JSON.
///
/// # Errors
///
/// Answered as a status and a JSON body rather than returned: a
/// malformed request, a repository or branch that does not exist, a
/// round already in flight for that target, or a tree that cannot be
/// made.
pub fn run(
    root: &Path,
    platform: &crate::Platform,
    config: &QueueConfig,
    in_flight: &InFlight,
    body: &[u8],
) -> (u16, String) {
    let request: serde_json::Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(error) => return bad(400, &format!("body is not json: {error}")),
    };
    let (Some(repo), Some(branch)) = (request["repo"].as_str(), request["branch"].as_str()) else {
        return bad(400, "both `repo` and `branch` are required");
    };
    // A branch name is a path component of a ref and of a directory
    // here, so the two characters that would make it neither are
    // refused before either is built from it.
    if branch.contains("..") || branch.starts_with('/') {
        return bad(400, "that is not a branch name");
    }

    let target = format!("{repo}:refs/heads/{branch}");
    let Some(_claim) = in_flight.claim(&target) else {
        return bad(409, "a round for that target is already running");
    };

    let bare = root.join(repo);
    if !bare.exists() {
        return bad(404, "no such repository");
    }
    let Some(round) = platform.proposal_round(repo, branch) else {
        return bad(404, "no such branch on that repository");
    };

    let trees = config.trees(repo, branch);
    if let Err(why) = ensure_worktree(&bare, &trees.speculation, branch) {
        return bad(500, &format!("the queue has no tree to work in: {why}"));
    }

    let mut ci = choir_queue::worktree::WorktreeRunner::new(bare.clone(), trees.jobs);
    let template = choir_queue::JobTemplate {
        command: {
            let mut argv = Vec::with_capacity(config.command.args.len() + 1);
            argv.push(config.command.program.clone());
            argv.extend(config.command.args.iter().cloned());
            argv
        },
        environment: choir_queue::differential_ledger::effective_environment(&config.command.env),
        deadline: config
            .command
            .timeout_seconds
            .map(std::time::Duration::from_secs),
        // Speculative by definition: a shared cache written from a
        // state nobody approved is the CREEP shape, and a queue open to
        // proposals from outside is exactly where it is reached.
        may_write_cache: false,
    };

    // Named in the answer so a caller can tell "nothing to do" from
    // "nothing proposed": these were in the round and already on the
    // branch, so the queue never saw them.
    let already_integrated = round.already_integrated(&trees.speculation);
    let Some(report) =
        platform.run_proposal_queue(repo, branch, &trees.speculation, &mut ci, template)
    else {
        return bad(404, "no such branch on that repository");
    };
    // The log led; git follows now rather than at the next startup.
    // Without this every landing left git's ref behind until a restart,
    // and every push to the branch in between lost its CAS (the pusher's
    // `old` is git's value, the view holds the landing). CAS'd on the
    // round's base at the git level too: a push that beat the round
    // would already have made the landing itself lose, so whichever
    // side wins here, nothing is clobbered.
    let git_lag = if report.merged.is_empty() {
        None
    } else {
        crate::platform::write_git_ref(
            &bare,
            &format!("refs/heads/{branch}"),
            &report.final_state,
            Some(&round.base),
        )
        .err()
    };
    let body = serde_json::json!({
        "format_version": 1,
        "repo": repo,
        "branch": branch,
        "merged": report.merged,
        "rejected": report.rejected.iter()
            .map(|(id, why)| serde_json::json!({ "id": id, "why": format!("{why:?}") }))
            .collect::<Vec<_>>(),
        "tip": report.final_state,
        "stalled": report.provider_error,
        "unreported_checks": report.unreported_checks,
        // Null when git holds what the log landed; otherwise why it does
        // not yet, and startup reconciliation will bring it forward.
        "git_lag": git_lag,
        "already_integrated": already_integrated,
    });
    (200, body.to_string())
}

fn bad(status: u16, reason: &str) -> (u16, String) {
    (status, serde_json::json!({ "error": reason }).to_string())
}
