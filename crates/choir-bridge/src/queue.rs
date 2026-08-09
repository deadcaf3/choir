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
use std::sync::atomic::{AtomicU64, Ordering};

static DIFFERENTIAL_RUN_ID: AtomicU64 = AtomicU64::new(0);

/// One PR's fate in a train build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrainEntry {
    /// PR number (or any caller-chosen id in tests).
    pub id: u64,
    /// The PR head oid this entry was built from.
    pub head: String,
    /// Whether the merge onto the train succeeded.
    pub merged: bool,
    /// The speculative merge commit, absent when the PR conflicted.
    pub merge: Option<String>,
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
                let merge = git(repo, &["rev-parse", "HEAD"])?.trim().to_string();
                entries.push(TrainEntry {
                    id: *id,
                    head: head.clone(),
                    merged: true,
                    merge: Some(merge),
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
                    merge: None,
                    note: "conflicts with train".to_string(),
                });
            }
        }
    }
    let tip = git(repo, &["rev-parse", "HEAD"])?.trim().to_string();
    Ok(Train { tip, entries })
}

/// Structured advisory result returned by the separately built D23 runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DifferentialOutcome {
    /// Monotonic calibration-ledger observation id.
    pub observation_id: u64,
    /// Stable classifier spelling from the runner.
    pub verdict: DifferentialVerdict,
    /// Number of interaction flags still awaiting ground truth.
    pub pending_interactions: u64,
}

/// Closed set of D23 classifications accepted from the runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DifferentialVerdict {
    /// Both parents and the merge passed.
    Clean,
    /// Both parents passed and the merge failed.
    InteractionFailure,
    /// At least one parent failed.
    InconclusiveParentFailure,
}

fn cleanup_worktrees(repo: &Path, root: &Path, paths: &[std::path::PathBuf]) -> Result<(), String> {
    let mut first_error = None;
    for path in paths.iter().rev() {
        let path_text = path.to_string_lossy().into_owned();
        if let Err(error) = git(repo, &["worktree", "remove", "--force", &path_text]) {
            first_error.get_or_insert(error);
        }
    }
    if root.exists() {
        if let Err(error) = std::fs::remove_dir_all(root) {
            first_error.get_or_insert_with(|| format!("remove differential checkout root: {error}"));
        }
    }
    if let Some(parent) = root.parent() {
        std::fs::remove_dir(parent).ok();
    }
    first_error.map_or(Ok(()), Err)
}

fn with_isolated_revisions<T>(
    repo: &Path,
    parent_a: &str,
    parent_b: &str,
    merged: &str,
    action: impl FnOnce(&Path, &Path, &Path) -> Result<T, String>,
) -> Result<T, String> {
    let root = repo.join(".choir-differential").join(format!(
        "run-{}-{}",
        std::process::id(),
        DIFFERENTIAL_RUN_ID.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root)
        .map_err(|error| format!("create differential checkout root: {error}"))?;
    let revisions = [
        ("parent-a", parent_a),
        ("parent-b", parent_b),
        ("merged", merged),
    ];
    let mut paths = Vec::with_capacity(revisions.len());
    for (name, revision) in revisions {
        let path = root.join(name);
        let path_text = path.to_string_lossy().into_owned();
        if let Err(error) = git(
            repo,
            &["worktree", "add", "-q", "--detach", &path_text, revision],
        ) {
            cleanup_worktrees(repo, &root, &paths).ok();
            return Err(error);
        }
        paths.push(path);
    }
    let result = action(&paths[0], &paths[1], &paths[2]);
    let cleanup = cleanup_worktrees(repo, &root, &paths);
    match (result, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}

/// Builds isolated worktrees for a speculative merge's two parents and the
/// merge itself, then invokes the explicit `choir-differential` runner.
///
/// The result is always advisory. This function only validates and returns a
/// closed structured result; callers have no landing-gate output to consume.
/// The state directory and command file are explicit arguments, never
/// environment configuration.
///
/// # Errors
///
/// The merge does not have two parents, a worktree cannot be created or
/// cleaned up, the runner fails operationally, or its structured result is
/// malformed/claims that landing gating is enabled.
pub fn run_differential(
    repo: &Path,
    merge: &str,
    runner: &Path,
    command_file: &Path,
    state_dir: &Path,
) -> Result<DifferentialOutcome, String> {
    let first = format!("{merge}^1");
    let second = format!("{merge}^2");
    let parent_a = git(repo, &["rev-parse", &first])?.trim().to_string();
    let parent_b = git(repo, &["rev-parse", &second])?.trim().to_string();
    with_isolated_revisions(repo, &parent_a, &parent_b, merge, |a, b, merged| {
        let output = std::process::Command::new(runner)
            .arg("run")
            .arg(command_file)
            .arg(state_dir)
            .arg(&parent_a)
            .arg(a)
            .arg(&parent_b)
            .arg(b)
            .arg(merge)
            .arg(merged)
            .output()
            .map_err(|error| format!("spawn differential runner: {error}"))?;
        if !output.status.success() {
            return Err("differential runner exited unsuccessfully".to_string());
        }
        let value: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|_| "differential runner returned malformed JSON".to_string())?;
        if value["format_version"].as_u64() != Some(1)
            || value["merge"].as_str() != Some(merge)
            || value["calibration"]["landing_gate_enabled"].as_bool() != Some(false)
            || !value["calibration"]["confidence_claim"].is_null()
            || !value["calibration"]["confidence_policy"].is_null()
        {
            return Err("differential runner returned an incompatible receipt".to_string());
        }
        let verdict = match value["report"]["verdict"].as_str() {
            Some("clean") => DifferentialVerdict::Clean,
            Some("interaction_failure") => DifferentialVerdict::InteractionFailure,
            Some("inconclusive_parent_failure") => {
                DifferentialVerdict::InconclusiveParentFailure
            }
            _ => return Err("differential runner returned an unknown verdict".to_string()),
        };
        if !matches!(
            &value["calibration"]["target"]["met"],
            serde_json::Value::Null | serde_json::Value::Bool(_)
        ) {
            return Err("differential target verdict must be boolean or null".to_string());
        }
        Ok(DifferentialOutcome {
            observation_id: value["observation_id"]
                .as_u64()
                .ok_or("differential result needs an observation id")?,
            verdict,
            pending_interactions: value["calibration"]["pending_interactions"]
                .as_u64()
                .ok_or("differential receipt needs pending_interactions")?,
        })
    })
}

/// Runs the advisory detector for every merge commit in a train, preserving
/// train order and returning operational errors per entry instead of turning
/// them into a landing decision.
#[must_use]
pub fn run_train_differentials(
    repo: &Path,
    train: &Train,
    runner: &Path,
    command_file: &Path,
    state_dir: &Path,
) -> Vec<(u64, Result<DifferentialOutcome, String>)> {
    train
        .entries
        .iter()
        .filter(|entry| entry.merged)
        .map(|entry| {
            let result = entry
                .merge
                .as_deref()
                .ok_or("merged train entry is missing its merge commit".to_string())
                .and_then(|merge| {
                    run_differential(repo, merge, runner, command_file, state_dir)
                });
            (entry.id, result)
        })
        .collect()
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

/// Reverts a landed train (D23 auto-revert arm): reverts each of the
/// train's merge commits (`base..tip`, first-parent, newest first) on
/// top of `tip`, then pushes the result to `branch` WITHOUT force — if
/// the branch moved past `tip` since landing, the push is rejected and
/// a human decides. Returns the new branch tip. The reverted tree is
/// byte-identical to `base`'s tree; history keeps the full record.
///
/// # Errors
///
/// Git failures, including a revert that itself conflicts (possible
/// when later commits touched the same lines) and the non-fast-forward
/// rejection — both leave the remote branch untouched.
pub fn revert_train(repo: &Path, url: &str, base: &str, tip: &str, branch: &str) -> Result<String, String> {
    let merges = git(
        repo,
        &["rev-list", "--first-parent", "--merges", &format!("{base}..{tip}")],
    )?;
    let merges: Vec<&str> = merges.split_whitespace().collect();
    if merges.is_empty() {
        return Err("no train merges between base and tip".to_string());
    }
    git(repo, &["checkout", "-q", "--detach", tip])?;
    for merge in &merges {
        // -m 1 = revert to the first parent (the train spine).
        if let Err(e) = git(repo, &["revert", "-m", "1", "--no-edit", merge]) {
            git(repo, &["revert", "--abort"]).ok();
            return Err(format!("revert of {merge} conflicts, leaving branch alone: {e}"));
        }
    }
    let new_tip = git(repo, &["rev-parse", "HEAD"])?.trim().to_string();
    git(repo, &["push", "-q", url, &format!("{new_tip}:refs/heads/{branch}")])?;
    Ok(new_tip)
}
