//! Base-rate measurement over a real repository's history (D23).
//!
//! The revised D23 tripwire's first clause is "measure our own
//! semantic-conflict base rate", and nothing else on D23 can proceed
//! without it: a detector's recall is meaningless at an unknown
//! prevalence, and the escalation threshold `t = 1 - c/r` cannot be
//! calibrated against a rate nobody has counted.
//!
//! Our own history cannot supply it — 60-odd commits, zero reverts, no
//! CI — so the corpus is **borrowed**: point this at a repository the
//! D21 bridge has mirrored and measure there.
//!
//! # What the label actually means
//!
//! This weak-labels a merge as bad if it was **reverted within a window
//! of following commits**. That is a proxy, and it is wrong in both
//! directions in ways worth stating rather than discovering:
//!
//! - *False negatives dominate.* A bad merge that was fixed forward, or
//!   noticed after the window, or never noticed, is labelled good. So the
//!   measured rate is a **lower bound** on the true rate.
//! - *False positives exist.* Reverts also happen for reasons that are
//!   not defects — a feature deferred, a release scoped down.
//!
//! It is used anyway because it needs nothing but git: no CI history, no
//! issue tracker, no human labelling. A lower bound on prevalence is
//! enough to answer the question that actually blocks D23, which is
//! whether a detector's false-positive rate is survivable at our rate.
//!
//! Same split as [`crate::blast`]: parsing is a pure function over `git
//! log` output, and the shell-out is a separate call, so tests are
//! hermetic.

use std::collections::{BTreeMap, BTreeSet};

/// Field separator in the `git log` format this module parses (ASCII
/// unit separator; cannot appear in a commit subject or body).
const FIELD: char = '\u{1f}';

/// Record separator (ASCII record separator).
const RECORD: char = '\u{1e}';

/// The `--format` string [`parse_history`] expects.
pub const LOG_FORMAT: &str = "--format=%H%x1f%P%x1f%B%x1e";

/// One commit, reduced to what the labelling needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    /// Full object id.
    pub id: String,
    /// Parent object ids; 2+ means a merge.
    pub parents: Vec<String>,
    /// Full commit message.
    pub body: String,
}

impl Commit {
    /// Whether this commit has two or more parents.
    #[must_use]
    pub fn is_merge(&self) -> bool {
        self.parents.len() > 1
    }

    /// The commit this one reverts, if its message says so.
    ///
    /// Matches git's own generated wording, `This reverts commit <oid>.`,
    /// which is what `git revert` writes and therefore what our own
    /// queue's auto-revert (`revert -m 1`) produces.
    #[must_use]
    pub fn reverts(&self) -> Option<String> {
        let rest = self.body.split("This reverts commit ").nth(1)?;
        let oid: String = rest.chars().take_while(char::is_ascii_hexdigit).collect();
        (oid.len() >= 7).then_some(oid)
    }
}

/// Maps a reverted commit back to the mainline commit that introduced it.
///
/// Needed because the obvious labelling — "a revert whose target is a
/// merge" — sees almost nothing in a pull-request workflow. Measured on
/// rust-lang/rust: of 307 unique revert targets, **39 are merges and 267
/// are ordinary commits**, because a revert names the change, not the
/// merge that landed it. Without attribution the proxy undercounts by
/// roughly eight to one.
///
/// A struct rather than a bare map so it can carry *why* a target went
/// unresolved, which is the part that would otherwise be silently lost.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Attribution {
    resolved: BTreeMap<String, String>,
    unresolved: BTreeSet<String>,
}

impl Attribution {
    /// An empty attribution: every revert is matched by exact oid only,
    /// which is the behaviour before attribution existed.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records that `target` arrived on the mainline at `introduced_by`.
    pub fn insert(&mut self, target: String, introduced_by: String) {
        self.resolved.insert(target, introduced_by);
    }

    /// Records a target that could not be placed — not an ancestor of the
    /// branch tip, or absent from the clone.
    pub fn insert_unresolved(&mut self, target: String) {
        self.unresolved.insert(target);
    }

    /// The mainline commit that introduced `target`, if known.
    #[must_use]
    pub fn get(&self, target: &str) -> Option<&str> {
        self.resolved.get(target).map(String::as_str)
    }

    /// How many targets were placed.
    #[must_use]
    pub fn resolved_count(&self) -> usize {
        self.resolved.len()
    }

    /// Targets that could not be placed. Non-empty is normal — a revert
    /// may target work that never reached this branch — but a large
    /// fraction means the corpus or the branch is the wrong one.
    #[must_use]
    pub fn unresolved(&self) -> &BTreeSet<String> {
        &self.unresolved
    }
}

/// Every distinct commit named by a `This reverts commit` line.
#[must_use]
pub fn revert_targets(history: &[Commit]) -> BTreeSet<String> {
    history.iter().filter_map(Commit::reverts).collect()
}

/// One merge and whether history later took it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeRecord {
    /// The merge commit's object id.
    pub merge: String,
    /// The reverting commit, when one was found inside the window.
    pub reverted_by: Option<String>,
    /// How many commits after the merge the revert landed. `None` when
    /// there was no revert.
    pub distance: Option<usize>,
}

/// Base rate over a labelled history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseRate {
    /// Merges examined.
    pub merges: usize,
    /// Merges reverted within the window.
    pub reverted: usize,
    /// Commits in the history that was scanned, for context: a rate over
    /// 40 merges and a rate over 40,000 are not the same evidence.
    pub commits: usize,
    /// Commits that revert *anything*, anywhere in the scanned history.
    ///
    /// This is the corpus-suitability signal, and measuring git/git is
    /// what showed it was needed: 15,579 merges, 46 revert commits in the
    /// whole first-parent history, giving a labelled rate of 0.001. That
    /// is not "git's merges are 99.9% safe" — it is a project that drops
    /// bad topics from an integration branch before they reach the
    /// mainline instead of reverting them. The proxy measures revert
    /// *culture*, and where there is none it reads as safety.
    ///
    /// A corpus with near-zero reverts cannot supply a base rate at all.
    /// Check this before believing [`Self::rate`].
    pub revert_commits: usize,
    /// The window the labelling used, in commits.
    pub window: usize,
}

impl BaseRate {
    /// Reverted merges as a fraction of merges, in `0.0..=1.0`.
    ///
    /// **A lower bound on the true bad-merge rate**, for the reasons in
    /// the module docs. A history with no merges scores 0.0 rather than
    /// dividing by zero — check [`Self::merges`] before believing it.
    #[must_use]
    pub fn rate(&self) -> f64 {
        if self.merges == 0 {
            return 0.0;
        }
        self.reverted as f64 / self.merges as f64
    }

    /// Whether the corpus reverts often enough for [`Self::rate`] to mean
    /// anything, at a deliberately low bar: at least one revert commit
    /// per 200 scanned commits.
    ///
    /// The threshold is a judgement, not a measurement, and it is set
    /// where it is because git/git sits an order of magnitude below it
    /// (46 reverts in 24,234 first-parent commits) while a
    /// merge-queue-driven project sits above. A `false` here means "find
    /// another corpus", never "this project's merges are safe".
    #[must_use]
    pub fn corpus_is_suitable(&self) -> bool {
        self.commits > 0 && self.revert_commits * 200 >= self.commits
    }
}

/// Parses `git log LOG_FORMAT` output, newest commit first.
///
/// Tolerates trailing whitespace between records, which git emits.
#[must_use]
pub fn parse_history(log: &str) -> Vec<Commit> {
    log.split(RECORD)
        .filter_map(|record| {
            let record = record.trim_start_matches(['\n', '\r']);
            if record.is_empty() {
                return None;
            }
            let mut fields = record.split(FIELD);
            let id = fields.next()?.trim().to_string();
            if id.is_empty() {
                return None;
            }
            let parents = fields
                .next()?
                .split_whitespace()
                .map(String::from)
                .collect();
            let body = fields.next().unwrap_or_default().to_string();
            Some(Commit { id, parents, body })
        })
        .collect()
}

/// Labels every merge in `history` (newest first) as reverted or not,
/// looking at most `window` commits forward from each merge.
///
/// Distance is measured in positions within `history`, so the caller is
/// responsible for handing over a sequence where that means something —
/// [`history`] uses `--first-parent` for exactly this reason.
///
/// A `window` of 0 finds nothing; the caller picks it, because the right
/// value is a property of the project's release cadence, not of this
/// code.
#[must_use]
pub fn label_merges(
    history: &[Commit],
    window: usize,
    attribution: &Attribution,
) -> Vec<MergeRecord> {
    // position in history -> commit, so "within N commits after" is a
    // slice rather than a graph walk. History is newest-first, so a
    // commit *after* the merge in time sits at a *lower* index.
    let index: BTreeMap<&str, usize> = history
        .iter()
        .enumerate()
        .map(|(i, c)| (c.id.as_str(), i))
        .collect();

    // Reverted oid -> the commit that reverted it. A commit may be
    // reverted more than once in a messy history; the newest reverting
    // commit wins, which is the one this loop sees last.
    let mut reverts: BTreeMap<String, &Commit> = BTreeMap::new();
    for commit in history {
        if let Some(target) = commit.reverts() {
            reverts.insert(target, commit);
        }
    }

    history
        .iter()
        .filter(|c| c.is_merge())
        .map(|merge| {
            let hit = reverts
                .iter()
                .find(|(target, _)| {
                    // Direct hit: the revert names this merge. git's
                    // message may abbreviate the oid, so match by prefix
                    // in whichever direction is shorter.
                    if merge.id.starts_with(target.as_str()) || target.starts_with(&merge.id) {
                        return true;
                    }
                    // Attributed hit: the revert names a commit this merge
                    // introduced. This is the common case in a
                    // pull-request workflow and the whole reason
                    // `Attribution` exists.
                    attribution.get(target) == Some(merge.id.as_str())
                })
                .map(|(_, c)| *c);
            let merge_pos = index.get(merge.id.as_str()).copied();
            let (reverted_by, distance) = match (hit, merge_pos) {
                (Some(rev), Some(mpos)) => match index.get(rev.id.as_str()) {
                    // Newest-first: the revert must sit at a lower index
                    // than the merge, or it predates it and is unrelated.
                    Some(&rpos) if rpos < mpos && mpos - rpos <= window => {
                        (Some(rev.id.clone()), Some(mpos - rpos))
                    }
                    _ => (None, None),
                },
                _ => (None, None),
            };
            MergeRecord {
                merge: merge.id.clone(),
                reverted_by,
                distance,
            }
        })
        .collect()
}

/// Rolls labelled merges up into a base rate.
#[must_use]
pub fn base_rate(history: &[Commit], window: usize, attribution: &Attribution) -> BaseRate {
    let labelled = label_merges(history, window, attribution);
    BaseRate {
        merges: labelled.len(),
        reverted: labelled.iter().filter(|m| m.reverted_by.is_some()).count(),
        commits: history.len(),
        revert_commits: history.iter().filter(|c| c.reverts().is_some()).count(),
        window,
    }
}

/// Runs `git log --first-parent` in `repo` and returns output for
/// [`parse_history`].
///
/// **`--first-parent` is load-bearing, not a tidying flag.** Without it,
/// the position of a commit in `git log` output depends on git's
/// date-and-topology ordering heuristics, so "reverted within N commits"
/// would mean something slightly different in every branchy history —
/// two commits could sit 2 or 3 apart depending on how their timestamps
/// happened to fall. Along first-parent order the listing *is* the
/// mainline, so distance is exactly "N landings later", which is the
/// quantity the window is trying to express.
///
/// It also fixes what gets counted: first-parent order lists the merges
/// that landed on this branch and skips commits internal to the branches
/// they merged, which is the population D23 cares about.
///
/// `max` caps the commits scanned; 0 means the whole history. Shelling
/// out rather than linking a git library is the workspace's standing
/// posture.
///
/// # Errors
///
/// Returns a description if git cannot be spawned or exits nonzero.
pub fn history(repo: &std::path::Path, max: usize) -> Result<String, String> {
    let mut cmd = std::process::Command::new("git");
    cmd.arg("log").arg("--first-parent").arg(LOG_FORMAT);
    if max > 0 {
        cmd.arg(format!("-{max}"));
    }
    let out = cmd
        .current_dir(repo)
        .output()
        .map_err(|e| format!("git log: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git log failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    // Lossy, not strict. Real histories carry commit messages that are
    // not valid UTF-8 — git/git has them at around 9.5 MB into its log,
    // from the pre-UTF-8 era — and refusing the whole corpus over an
    // author's name in Latin-1 would be absurd. Everything this module
    // reads is ASCII: the 0x1f/0x1e framing, hex oids, and the literal
    // "This reverts commit ". Replacement characters land only inside
    // message text the labelling never inspects.
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Which of `oids` are commits present in `repo`, in one `cat-file`
/// pass.
///
/// Lazy fetching is disabled. On a partial clone any command touching a
/// missing object otherwise reaches for the network, and a revert can
/// name a commit that no longer exists upstream at all — rust-lang/rust
/// has one — which turns an offline analysis into a failed fetch that
/// aborts the whole run.
///
/// # Errors
///
/// Git failing to run at all.
fn existing_commits(
    repo: &std::path::Path,
    oids: &BTreeSet<String>,
) -> Result<BTreeSet<String>, String> {
    use std::io::Write;
    let mut child = std::process::Command::new("git")
        .args(["cat-file", "--batch-check"])
        .current_dir(repo)
        .env("GIT_NO_LAZY_FETCH", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("git cat-file: {e}"))?;
    {
        let mut stdin = child.stdin.take().ok_or("git cat-file: no stdin")?;
        for oid in oids {
            writeln!(stdin, "{oid}").map_err(|e| format!("git cat-file: {e}"))?;
        }
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("git cat-file: {e}"))?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            let oid = f.next()?;
            (f.next() == Some("commit")).then(|| oid.to_string())
        })
        .collect())
}

/// Whether `ancestor` is an ancestor of `descendant` in `repo`.
///
/// # Errors
///
/// Git failing for any reason other than a clean "no" — most often an
/// object missing from a filtered clone.
fn is_ancestor(repo: &std::path::Path, ancestor: &str, descendant: &str) -> Result<bool, String> {
    let out = std::process::Command::new("git")
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .current_dir(repo)
        // Never reach for the network mid-analysis; callers have already
        // established that both objects are present locally.
        .env("GIT_NO_LAZY_FETCH", "1")
        .output()
        .map_err(|e| format!("git merge-base: {e}"))?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(format!(
            "git merge-base: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )),
    }
}

/// Places every target in `targets` on the mainline `history`.
///
/// A target already on the mainline maps to itself. Otherwise it arrived
/// through some merge's second parent, and the mainline commit that
/// introduced it is the **oldest** one having it as an ancestor.
///
/// Found by binary search, not a scan: ancestry is monotone along
/// first-parent order — if a commit is an ancestor of some mainline
/// commit it is an ancestor of every later one — so the predicate flips
/// exactly once. That is ~log2(n) git calls per target instead of n. On
/// rust-lang/rust's 45,061-commit mainline it is 16 calls, about 0.15 s.
///
/// Targets that are not ancestors of the branch tip at all, or whose
/// object is missing from a filtered clone, are recorded as unresolved
/// rather than dropped: how many failed to place is part of reading the
/// result.
///
/// # Errors
///
/// Git failing in a way that is not a clean answer, which would
/// otherwise be silently miscounted as "not an ancestor".
pub fn attribute(
    repo: &std::path::Path,
    history: &[Commit],
    targets: &BTreeSet<String>,
) -> Result<Attribution, String> {
    let mut attribution = Attribution::new();
    if history.is_empty() {
        for t in targets {
            attribution.insert_unresolved(t.clone());
        }
        return Ok(attribution);
    }
    let on_mainline: BTreeSet<&str> = history.iter().map(|c| c.id.as_str()).collect();
    let tip = history[0].id.as_str();
    // A revert can name a commit this clone does not have — force-pushed
    // away, or from a branch that never landed. Establish that up front
    // in one pass rather than discovering it as a git failure per target.
    let present = existing_commits(repo, targets)?;

    for target in targets {
        if on_mainline.contains(target.as_str()) {
            attribution.insert(target.clone(), target.clone());
            continue;
        }
        if !present.contains(target) {
            attribution.insert_unresolved(target.clone());
            continue;
        }
        // Not on the mainline, and not even behind the tip: it belongs to
        // another branch, so no merge here introduced it.
        if !is_ancestor(repo, target, tip)? {
            attribution.insert_unresolved(target.clone());
            continue;
        }
        // Newest-first, so the predicate is true for every index up to
        // some k and false after: find the largest such k.
        let (mut lo, mut hi) = (0usize, history.len() - 1);
        while lo < hi {
            // Round up, so `lo == hi - 1` advances instead of looping.
            let mid = lo + (hi - lo).div_ceil(2);
            if is_ancestor(repo, target, &history[mid].id)? {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        attribution.insert(target.clone(), history[lo].id.clone());
    }
    Ok(attribution)
}
