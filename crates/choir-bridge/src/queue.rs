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

use std::path::{Path, PathBuf};
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

fn compatible_confidence_policy(value: &serde_json::Value) -> bool {
    value["format_version"].as_u64() == Some(1)
        && value["method"].as_str() == Some("one_sided_exact_binomial_zero_spurious")
        && value["confidence"]["numerator"].as_u64() == Some(95)
        && value["confidence"]["denominator"].as_u64() == Some(100)
        && value["target"]["numerator"].as_u64() == Some(1)
        && value["target"]["denominator"].as_u64() == Some(1000)
        && value["target"]["comparison"].as_str() == Some("strictly_less_than")
        && value["minimum_evaluated_merges"].as_u64() == Some(2_995)
        && value["requires_zero_spurious_failures"].as_bool() == Some(true)
        && value["assumptions"].as_array().is_some_and(|assumptions| {
            assumptions
                == &[
                    serde_json::Value::String("independent_runs".to_string()),
                    serde_json::Value::String(
                        "representative_queue_command_and_merge_population".to_string(),
                    ),
                ]
        })
}

fn cleanup_worktrees(repo: &Path, root: &Path, paths: &[PathBuf]) -> Result<(), String> {
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

/// Three revision worktrees whose lifetime is a whole **calibration run**
/// rather than a single observation.
///
/// The observation cost the harness cannot avoid is compiling the project
/// under test three times. The cost it *can* avoid is compiling the project's
/// dependencies three times per observation, which is what a fresh worktree
/// per observation forces: the operator's `--target-dir` is tree-relative (the
/// ledger refuses any target directory that escapes its worktree), so
/// destroying the tree destroys the build directory with it and the next
/// observation starts from zero. Holding the same three trees open across
/// observations and moving them with `git checkout` keeps those build
/// directories alive. Measured on `rust-lang/log`, one run: **16.1 s** in a
/// fresh tree against **12.0-13.4 s** in a held tree switched to a new
/// revision.
///
/// Three properties are deliberately preserved, because each of them is load
/// bearing for what the calibration claims:
///
/// - **Every run is still really run.** A held tree makes a run cheaper, never
///   skipped: `cargo test` re-links and re-executes the test binaries even when
///   nothing changed (measured: four test binaries execute in a fully warm
///   tree). That is what the ledger's `independent_runs` assumption needs, and
///   it is why this is not a result cache.
/// - **The three trees stay separate.** Each keeps its own build directory, so
///   the cross-tree artifact contamination that invalidated an earlier receipt
///   cannot recur, and the three concurrent runs still cannot contend on one
///   tool lock.
/// - **Nothing survives the run.** The worktrees and their root are removed by
///   [`Self::close`], and again by `Drop` if a caller returns early, so an
///   operator's repository is left as it was found.
///
/// What it does change, stated rather than buried: an observation now starts in
/// a tree that holds the *previous* observation's untracked build output.
/// Tracked content is exact — the checkout is forced, so the tree matches the
/// revision — but a command that writes untracked files into its tree will see
/// them again. Callers that need a pristine tree per observation still have
/// one: [`run_differential`] opens and closes a session around a single
/// observation, and `choir-bridge calibrate --fresh-worktrees` selects that
/// path for a whole run.
pub struct DifferentialSession {
    repo: PathBuf,
    root: PathBuf,
    /// `None` until the first observation names the revisions to check out.
    /// Worktree creation needs a revision, so there is nothing useful to
    /// create at open time.
    trees: Option<[PathBuf; 3]>,
}

impl DifferentialSession {
    /// Names a session root under `repo`. Creates nothing until the first
    /// observation; opening cannot fail.
    #[must_use]
    pub fn open(repo: &Path) -> Self {
        Self {
            repo: repo.to_path_buf(),
            root: repo.join(".choir-differential").join(format!(
                "run-{}-{}",
                std::process::id(),
                DIFFERENTIAL_RUN_ID.fetch_add(1, Ordering::Relaxed)
            )),
            trees: None,
        }
    }

    /// Puts the three trees on the three requested revisions, creating them on
    /// the first call and moving them on every later one.
    fn prepare(
        &mut self,
        parent_a: &str,
        parent_b: &str,
        merged: &str,
    ) -> Result<[PathBuf; 3], String> {
        let revisions = [parent_a, parent_b, merged];
        if let Some(trees) = &self.trees {
            for (tree, revision) in trees.iter().zip(revisions) {
                // `--force` because the tree is the harness's own scratch
                // checkout and an observation must start from exactly this
                // revision's tracked content: a command that dirties a tracked
                // file (a lockfile, say) must not be able to fail the next
                // observation's checkout. It leaves untracked build output
                // alone, which is the point of holding the tree at all.
                git(tree, &["checkout", "-q", "--detach", "--force", revision])?;
            }
            return Ok(trees.clone());
        }
        std::fs::create_dir_all(&self.root)
            .map_err(|error| format!("create differential checkout root: {error}"))?;
        let mut paths = Vec::with_capacity(revisions.len());
        for (name, revision) in [
            ("parent-a", parent_a),
            ("parent-b", parent_b),
            ("merged", merged),
        ] {
            let path = self.root.join(name);
            let path_text = path.to_string_lossy().into_owned();
            if let Err(error) = git(
                &self.repo,
                &["worktree", "add", "-q", "--detach", &path_text, revision],
            ) {
                cleanup_worktrees(&self.repo, &self.root, &paths).ok();
                return Err(error);
            }
            paths.push(path);
        }
        let trees: [PathBuf; 3] = paths
            .try_into()
            .map_err(|_| "differential session needs exactly three worktrees".to_string())?;
        self.trees = Some(trees.clone());
        Ok(trees)
    }

    fn cleanup(&mut self) -> Result<(), String> {
        let paths = self.trees.take().unwrap_or_default();
        cleanup_worktrees(&self.repo, &self.root, &paths)
    }

    /// Removes the three worktrees and the session root, reporting the first
    /// failure. `Drop` repeats this as a best-effort backstop, so a caller that
    /// returns early still leaves nothing behind — but only `close` can tell
    /// the caller that cleanup failed.
    ///
    /// # Errors
    ///
    /// A worktree or the session root could not be removed.
    pub fn close(mut self) -> Result<(), String> {
        self.cleanup()
    }
}

impl Drop for DifferentialSession {
    fn drop(&mut self) {
        // Best effort: `close` is the reporting path. This exists so an early
        // return or a panic mid-run cannot leave `.choir-differential` in an
        // operator's repository.
        self.cleanup().ok();
    }
}

/// Resolves a merge commit to its canonical oid and its exact first and second
/// parents. Resolution happens before any checkout so an abbreviated or
/// non-canonical spelling cannot reach the ledger.
fn resolve_merge_revisions(repo: &Path, merge: &str) -> Result<(String, String, String), String> {
    let commit = format!("{merge}^{{commit}}");
    let merged_revision = git(repo, &["rev-parse", "--verify", &commit])?
        .trim()
        .to_string();
    let first = format!("{merged_revision}^1");
    let second = format!("{merged_revision}^2");
    let parent_a = git(repo, &["rev-parse", &first])?.trim().to_string();
    let parent_b = git(repo, &["rev-parse", &second])?.trim().to_string();
    Ok((parent_a, parent_b, merged_revision))
}

/// Runs one observation in an already-open session's three worktrees.
///
/// This is the loop body of a calibration run: the session is opened once and
/// reused, so the build directories the operator's command creates survive
/// between observations. Everything about the result is unchanged — the same
/// runner, the same argv, the same structured receipt validation as the
/// single-observation [`run_differential`].
///
/// The result is always advisory. This function only validates and returns a
/// closed structured result; callers have no landing-gate output to consume.
/// The state directory and command file are explicit arguments, never
/// environment configuration.
///
/// # Errors
///
/// The merge does not have two parents, a worktree cannot be created or moved
/// to the requested revision, the runner fails operationally, or its structured
/// result is malformed/claims that landing gating is enabled.
pub fn run_differential_in(
    session: &mut DifferentialSession,
    merge: &str,
    runner: &Path,
    command_file: &Path,
    state_dir: &Path,
) -> Result<DifferentialOutcome, String> {
    let (parent_a, parent_b, merged_revision) = resolve_merge_revisions(&session.repo, merge)?;
    let trees = session.prepare(&parent_a, &parent_b, &merged_revision)?;
    let output = std::process::Command::new(runner)
        .arg("run")
        .arg(command_file)
        .arg(state_dir)
        .arg(&parent_a)
        .arg(&trees[0])
        .arg(&parent_b)
        .arg(&trees[1])
        .arg(&merged_revision)
        .arg(&trees[2])
        .output()
        .map_err(|error| format!("spawn differential runner: {error}"))?;
    if !output.status.success() {
        return Err("differential runner exited unsuccessfully".to_string());
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|_| "differential runner returned malformed JSON".to_string())?;
    if value["format_version"].as_u64() != Some(1)
        || value["merge"].as_str() != Some(merged_revision.as_str())
        || value["calibration"]["landing_gate_enabled"].as_bool() != Some(false)
        || !matches!(
            &value["calibration"]["confidence_claim"],
            serde_json::Value::Null | serde_json::Value::Bool(_)
        )
        || !compatible_confidence_policy(&value["calibration"]["confidence_policy"])
    {
        return Err("differential runner returned an incompatible receipt".to_string());
    }
    let verdict = match value["report"]["verdict"].as_str() {
        Some("clean") => DifferentialVerdict::Clean,
        Some("interaction_failure") => DifferentialVerdict::InteractionFailure,
        Some("inconclusive_parent_failure") => DifferentialVerdict::InconclusiveParentFailure,
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
}

/// Builds isolated worktrees for a speculative merge's two parents and the
/// merge itself, then invokes the explicit `choir-differential` runner.
///
/// This is [`run_differential_in`] wrapped in a session of its own, so the
/// three worktrees are created and destroyed around this one observation and
/// the command sees a pristine tree. That is the right shape for a speculative
/// train, where each merge is checked once, and it is the escape hatch for an
/// operator re-checking a flagged interaction without the previous
/// observation's build output present.
///
/// # Errors
///
/// As [`run_differential_in`], plus a failure to clean the worktrees up.
pub fn run_differential(
    repo: &Path,
    merge: &str,
    runner: &Path,
    command_file: &Path,
    state_dir: &Path,
) -> Result<DifferentialOutcome, String> {
    let mut session = DifferentialSession::open(repo);
    let result = run_differential_in(&mut session, merge, runner, command_file, state_dir);
    let cleanup = session.close();
    match (result, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
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
