//! Falsification (c): how often does a landed merge remove work that
//! neither side removed?
//!
//! [`crate::safety::check`] answers that question for one merge. This
//! module points it at a repository's real merge history so the question
//! can be answered with a number instead of an argument. The plan it
//! serves, its kill criterion and the reasoning behind both live outside
//! the repository; what matters here is that **the criterion was fixed
//! before the number was known**, so a near-zero result is an answer and
//! not a prompt to re-cut the measurement.
//!
//! # What is compared, and to what
//!
//! For a merge commit `M` with first parent `T` (the target branch, the
//! side that was being merged *onto*) and second parent `P` (the
//! proposal), with `B = merge-base(T, P)`:
//!
//! ```text
//! base     = B:<path>      what the author wrote against
//! target   = T:<path>      the state being merged onto
//! proposed = P:<path>      what the author wrote
//! result   = M:<path>      what actually landed
//! ```
//!
//! which is exactly [`crate::safety::check`]'s signature. A path missing
//! from a tree reads as empty, so an added file compares as an addition
//! and a deleted file as a removal, both of which the bag comparison
//! already handles.
//!
//! Only paths where `M` differs from `T` are examined. A path the merge
//! left identical to the target has an empty landing delta and therefore
//! always upholds, so scanning it would cost four blob reads to learn
//! nothing.
//!
//! # What a finding is worth, stated before any are counted
//!
//! A violation here is **evidence of a silent revert, not proof of a
//! defect**, and the honest reading is bounded on both sides:
//!
//! - *False positives are expected and are the dominant noise.* A merge
//!   whose conflicts a human resolved by hand is free to remove lines
//!   neither parent removed, and that is a correct resolution, not a
//!   silent revert. Automatic merges — a merge queue's, `--no-ff` on a
//!   clean tree — are where a finding means what it says. The report
//!   keeps the two apart only as far as git lets it, which is not far;
//!   any non-zero count wants eyes on the individual findings before it
//!   is quoted at anybody.
//! - *False negatives dominate the other direction.* The comparison is
//!   over line bags, so a merge that moves a line, or applies an edit at
//!   the wrong occurrence of a repeated line, is bag-neutral and passes.
//!   That is [`crate::safety`]'s stated positional blindness, inherited
//!   here whole.
//!
//! So the count is a **lower bound on a noisy signal**. It is worth
//! having anyway, for the same reason [`choir-queue`'s corpus module]
//! is: it needs nothing but git — no CI history, no issue tracker, no
//! human labelling — and an unmeasured rate cannot be argued with at
//! all.
//!
//! [`choir-queue`'s corpus module]: https://docs.rs/choir-queue
//!
//! # Shape
//!
//! Same split as `choir-queue::corpus`: parsing and scanning are pure
//! functions over text, and every shell-out is a separate call, so the
//! tests are hermetic and the expensive part is the caller's problem.
//!
//! # Examples
//!
//! ```
//! use choir_merge::silent_revert::{scan_merge, Blobs, ScanReport};
//!
//! // The target added a line after the fork; the proposal touched a
//! // different one; the landed result dropped the target's line.
//! let blobs = Blobs {
//!     base: "a\n".into(),
//!     target: "a\nkeep\n".into(),
//!     proposed: "a\nb\n".into(),
//!     result: "a\nb\n".into(),
//! };
//! let mut report = ScanReport::default();
//! scan_merge("deadbeef", &[("src/x.rs".to_string(), blobs)], &mut report);
//!
//! assert_eq!(report.findings.len(), 1);
//! assert_eq!(report.findings[0].reverted, vec!["keep".to_string()]);
//! ```

use crate::safety::{check, SafetyVerdict};

/// The `--format` string [`parse_merges`] expects.
///
/// Oid and parents, unit-separated, records separated by 0x1e — the same
/// framing `choir-queue::corpus` uses, and for the same reason: neither
/// byte can appear in an oid.
pub const MERGE_LOG_FORMAT: &str = "--format=%H%x1f%P%x1e";

/// Blobs larger than this are skipped rather than compared.
///
/// A minified bundle or a checked-in binary that happens to decode as
/// UTF-8 would otherwise dominate both the runtime and any finding it
/// produced, and a line-bag comparison says nothing useful about either.
pub const MAX_BLOB_BYTES: usize = 1 << 20;

/// One merge commit, reduced to what the scan needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeCommit {
    /// Full object id of the merge.
    pub id: String,
    /// Parent oids, first parent first.
    pub parents: Vec<String>,
}

/// The four texts [`crate::safety::check`] compares, for one path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blobs {
    /// `merge-base(target, proposed)` at this path; empty if absent.
    pub base: String,
    /// First parent at this path; empty if absent.
    pub target: String,
    /// Second parent at this path; empty if absent.
    pub proposed: String,
    /// The merge commit itself at this path; empty if absent.
    pub result: String,
}

/// One path in one merge that removed or added lines neither side did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Oid of the merge commit.
    pub merge: String,
    /// Repository-relative path.
    pub path: String,
    /// Lines the landing removed that the proposal never removed.
    pub reverted: Vec<String>,
    /// Lines the landing added that the proposal never added.
    pub injected: Vec<String>,
}

/// What one repository's scan counted.
///
/// Every skip is counted rather than dropped: a rate whose denominator
/// quietly excluded the hard cases is the failure mode this whole
/// measurement exists to avoid.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanReport {
    /// Merge commits the log produced.
    pub merges_seen: usize,
    /// Merge commits actually compared.
    pub merges_scanned: usize,
    /// Merges with three or more parents, which this comparison does not
    /// model: there is no single `proposed` side.
    pub skipped_octopus: usize,
    /// Merges whose parents share no merge base, or whose objects are
    /// missing from a filtered clone.
    pub skipped_no_base: usize,
    /// Paths compared.
    pub paths_scanned: usize,
    /// Paths where some blob was not valid UTF-8.
    pub skipped_binary: usize,
    /// Paths where some blob exceeded [`MAX_BLOB_BYTES`].
    pub skipped_large: usize,
    /// Distinct lines cancelled as moved between paths within one
    /// merge rather than lost. See [`scan_merge`].
    pub lines_relocated: usize,
    /// Paths whose whole apparent violation was relocation, and which
    /// therefore produced no finding.
    pub paths_all_relocated: usize,
    /// Distinct lines reported as reverted that are nonetheless present
    /// somewhere in the merge result. Counted, never cancelled: see
    /// [`scan_merge`] on why this is a second number and not a filter.
    pub lines_surviving_elsewhere: usize,
    /// Findings every one of whose reverted lines survives somewhere in
    /// the result, and which carry no injection either. These are the
    /// candidates for refactor noise rather than lost work.
    pub findings_all_surviving: usize,
    /// The violations, in the order found.
    pub findings: Vec<Finding>,
}

impl ScanReport {
    /// Merges carrying at least one finding, over merges scanned.
    ///
    /// Returns 0.0 for an empty scan rather than a NaN: "nothing was
    /// examined" and "nothing was found" must not print the same way.
    #[must_use]
    pub fn merge_violation_rate(&self) -> f64 {
        if self.merges_scanned == 0 {
            return 0.0;
        }
        let mut ids: Vec<&str> = self.findings.iter().map(|f| f.merge.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        ids.len() as f64 / self.merges_scanned as f64
    }

    /// Folds another repository's report into this one.
    pub fn absorb(&mut self, other: Self) {
        self.merges_seen += other.merges_seen;
        self.merges_scanned += other.merges_scanned;
        self.skipped_octopus += other.skipped_octopus;
        self.skipped_no_base += other.skipped_no_base;
        self.paths_scanned += other.paths_scanned;
        self.skipped_binary += other.skipped_binary;
        self.skipped_large += other.skipped_large;
        self.lines_relocated += other.lines_relocated;
        self.paths_all_relocated += other.paths_all_relocated;
        self.lines_surviving_elsewhere += other.lines_surviving_elsewhere;
        self.findings_all_surviving += other.findings_all_surviving;
        self.findings.extend(other.findings);
    }
}

/// Parses `git log --merges MERGE_LOG_FORMAT` output.
///
/// Records with no parents, or unparseable framing, are dropped rather
/// than erroring: a corpus is not a wire format, and one malformed
/// record must not cost the other ten thousand.
#[must_use]
pub fn parse_merges(log: &str) -> Vec<MergeCommit> {
    log.split('\u{1e}')
        .filter_map(|record| {
            let record = record.trim_start_matches('\n');
            let mut fields = record.split('\u{1f}');
            let id = fields.next()?.trim();
            let parents: Vec<String> = fields
                .next()?
                .split_whitespace()
                .map(str::to_string)
                .collect();
            (!id.is_empty() && !parents.is_empty()).then(|| MergeCommit {
                id: id.to_string(),
                parents,
            })
        })
        .collect()
}

/// Whether a line is worth reporting as evidence.
///
/// Blank and whitespace-only lines are not. They move between files
/// constantly, they carry no work, and on the first real corpus this
/// scanner was pointed at they were the single most common "finding".
fn is_evidence(line: &str) -> bool {
    !line.trim().is_empty()
}

/// Applies the safety check to every path of one merge, counting into
/// `report`.
///
/// Pure: the caller supplies the blobs. `merges_scanned` is incremented
/// here, so a caller that skips a merge before reaching this function
/// must count that skip itself.
///
/// # Relocation
///
/// [`crate::safety::check`] compares one path against itself, so a merge
/// that *moves* content from one file to another looks like a reversion
/// in the source and an injection in the destination. That is not a
/// silent revert: nothing was lost. The first corpus this was pointed at
/// produced exactly that, a documentation section moved between two
/// files during the merge, and it was 40% of the raw findings.
///
/// So after every path of a merge is checked, a line that appears as
/// reverted in one path *and* injected in another path of the same merge
/// is cancelled from both and counted in
/// [`ScanReport::lines_relocated`]. The cancellation is per merge and
/// never across merges: content leaving one commit and appearing in
/// another, later, is not a move, and treating it as one would hide the
/// exact class this scan exists to count.
///
/// # Survival, which is a second number rather than a second filter
///
/// That cancellation requires the line to be *unattributable at both
/// ends*. A refactor that moves a function into a file the author was
/// already editing does not qualify: the destination addition is
/// attributable to `base -> proposed`, so it never enters the injected
/// set, so it cannot cancel anything, and the source removal is reported
/// as a reversion of work that is sitting in the result untouched. On
/// `git/git` that is most of what the raw findings are -- the object
/// database refactor moving blocks out of `object-file.c` reads as
/// fourteen reverted lines.
///
/// [`ScanReport::lines_surviving_elsewhere`] counts reverted lines that
/// are present somewhere in this merge's result, and
/// [`ScanReport::findings_all_surviving`] counts findings made entirely
/// of them.
///
/// **They are counted and still reported.** Cancelling them would be the
/// stronger detector and the weaker measurement: line presence anywhere
/// in a result is a cheap test that a short or idiomatic line passes by
/// accident, so silently dropping on it would remove true findings with
/// no way to see how many. The kill criterion for this measurement was
/// fixed before any number was known, and a filter added after seeing
/// the data is exactly the move that discipline forbids. Two numbers let
/// a reader bound the answer from both sides; one number chosen after
/// the fact lets them do neither.
pub fn scan_merge(merge: &str, paths: &[(String, Blobs)], report: &mut ScanReport) {
    use std::collections::BTreeSet;

    report.merges_scanned += 1;
    let mut raw: Vec<Finding> = Vec::new();
    for (path, blobs) in paths {
        report.paths_scanned += 1;
        if let SafetyVerdict::Violation(v) =
            check(&blobs.base, &blobs.target, &blobs.proposed, &blobs.result)
        {
            raw.push(Finding {
                merge: merge.to_string(),
                path: path.clone(),
                reverted: v.reverted.into_iter().filter(|l| is_evidence(l)).collect(),
                injected: v.injected.into_iter().filter(|l| is_evidence(l)).collect(),
            });
        }
    }

    let reverted_anywhere: BTreeSet<&str> = raw
        .iter()
        .flat_map(|f| f.reverted.iter().map(String::as_str))
        .collect();
    let injected_anywhere: BTreeSet<&str> = raw
        .iter()
        .flat_map(|f| f.injected.iter().map(String::as_str))
        .collect();
    let relocated: BTreeSet<String> = reverted_anywhere
        .intersection(&injected_anywhere)
        .map(|l| (*l).to_string())
        .collect();
    report.lines_relocated += relocated.len();

    // Every line the merge result holds, across the paths this merge
    // touched. A reverted line found here left its file and did not
    // leave the tree.
    let survives: BTreeSet<&str> = paths
        .iter()
        .flat_map(|(_, blobs)| blobs.result.lines())
        .filter(|l| is_evidence(l))
        .collect();

    let mut surviving_lines: BTreeSet<String> = BTreeSet::new();
    for mut finding in raw {
        finding.reverted.retain(|l| !relocated.contains(l));
        finding.injected.retain(|l| !relocated.contains(l));
        if finding.reverted.is_empty() && finding.injected.is_empty() {
            report.paths_all_relocated += 1;
            continue;
        }
        let outlives: Vec<&String> = finding
            .reverted
            .iter()
            .filter(|l| survives.contains(l.as_str()))
            .collect();
        if finding.injected.is_empty() && outlives.len() == finding.reverted.len() {
            report.findings_all_surviving += 1;
        }
        surviving_lines.extend(outlives.into_iter().cloned());
        report.findings.push(finding);
    }
    report.lines_surviving_elsewhere += surviving_lines.len();
}

/// Runs `git` in `repo` with lazy fetching disabled, returning stdout.
///
/// Lazy fetching is off for the same reason `choir-queue::corpus` turns
/// it off: on a partial clone, any command touching a missing object
/// otherwise reaches for the network, which turns an offline measurement
/// into a failed fetch that aborts the run.
fn git(repo: &std::path::Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_NO_LAZY_FETCH", "1")
        .output()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if !out.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out.stdout)
}

/// The merge commits on `repo`'s first-parent mainline, newest first.
///
/// `max` caps them; 0 means all of them.
///
/// # Errors
///
/// Git failing to run, or exiting nonzero.
pub fn merge_log(repo: &std::path::Path, max: usize) -> Result<Vec<MergeCommit>, String> {
    let cap = format!("-{max}");
    let mut args = vec!["log", "--first-parent", "--merges", MERGE_LOG_FORMAT];
    if max > 0 {
        args.push(&cap);
    }
    // Lossy: oids and the framing bytes are ASCII, and this format
    // carries no message text for a replacement character to land in.
    Ok(parse_merges(&String::from_utf8_lossy(&git(repo, &args)?)))
}

/// `git merge-base a b`, or `None` when they share none.
///
/// The first base is taken when there are several. Criss-cross histories
/// have more than one and git's own `recursive` strategy synthesises a
/// merge of them; approximating that with the first is a known
/// imprecision, and it biases toward *findings* rather than away, which
/// is the direction that gets looked at rather than believed.
///
/// # Errors
///
/// Never: a merge base git cannot produce is `None`, which the caller
/// counts as a skip.
pub fn merge_base(repo: &std::path::Path, a: &str, b: &str) -> Result<Option<String>, String> {
    let out = std::process::Command::new("git")
        .args(["merge-base", a, b])
        .current_dir(repo)
        .env("GIT_NO_LAZY_FETCH", "1")
        .output()
        .map_err(|e| format!("git merge-base: {e}"))?;
    match out.status.code() {
        Some(0) => Ok(String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .next()
            .map(str::to_string)),
        // 1 is "no merge base"; anything else is a missing object on a
        // filtered clone, and both mean the same thing to the caller.
        _ => Ok(None),
    }
}

/// Paths where `to` differs from `from`.
///
/// NUL-separated, because a repository is free to contain a path with a
/// newline in it and `--name-only` alone would quote it.
///
/// # Errors
///
/// Git failing to run, or exiting nonzero.
pub fn changed_paths(repo: &std::path::Path, from: &str, to: &str) -> Result<Vec<String>, String> {
    let out = git(repo, &["diff", "--name-only", "-z", from, to])?;
    Ok(out
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect())
}

/// Reads many `<rev>:<path>` blobs in one `cat-file --batch`.
///
/// Results are positional -- `None` where git said `missing` -- so no
/// path parsing is needed on the way back, which is what makes paths
/// with spaces or newlines in them safe here.
///
/// # Errors
///
/// Git failing to spawn, or its output not matching the batch format.
fn cat_file_batch(repo: &std::path::Path, revs: &[String]) -> Result<Vec<Option<Vec<u8>>>, String> {
    use std::io::Write;
    let mut child = std::process::Command::new("git")
        .args(["cat-file", "--batch"])
        .current_dir(repo)
        .env("GIT_NO_LAZY_FETCH", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("git cat-file: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("git cat-file: no stdin")?;
    let input: Vec<String> = revs.to_vec();
    // The writer runs on its own thread. Blob output can be megabytes,
    // so writing the whole request before reading any of it deadlocks
    // as soon as the pipe buffer fills -- which is why this is not the
    // write-then-wait shape `corpus::existing_commits` uses for
    // --batch-check, whose output is one short line per request.
    let writer = std::thread::spawn(move || {
        for rev in &input {
            if writeln!(stdin, "{rev}").is_err() {
                return;
            }
        }
    });
    let out = child
        .wait_with_output()
        .map_err(|e| format!("git cat-file: {e}"))?;
    writer.join().map_err(|_| "git cat-file: writer panicked")?;
    parse_batch(&out.stdout, revs.len())
}

/// Splits `cat-file --batch` output into `count` positional results.
///
/// # Errors
///
/// A header that is neither `<oid> <type> <size>` nor `... missing`, or
/// output that ends mid-blob.
pub fn parse_batch(out: &[u8], count: usize) -> Result<Vec<Option<Vec<u8>>>, String> {
    let mut results = Vec::with_capacity(count);
    let mut at = 0usize;
    while results.len() < count {
        let end = out
            .get(at..)
            .and_then(|rest| rest.iter().position(|b| *b == b'\n'))
            .ok_or("git cat-file: output ended before the last request")?
            + at;
        let header = String::from_utf8_lossy(&out[at..end]).into_owned();
        at = end + 1;
        if header.ends_with(" missing") {
            results.push(None);
            continue;
        }
        let size: usize = header
            .rsplit(' ')
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| format!("git cat-file: unparseable header {header:?}"))?;
        if at + size > out.len() {
            return Err("git cat-file: output ended mid-blob".to_string());
        }
        results.push(Some(out[at..at + size].to_vec()));
        // The blob is followed by a newline git added, not one the blob
        // contains.
        at += size + 1;
    }
    Ok(results)
}

/// Scans one repository's merge history.
///
/// `max` caps the merges examined; 0 means all of them. One
/// `cat-file --batch` runs per merge, carrying every blob that merge
/// needs, which is what keeps a fifty-repository sweep to one subprocess
/// per merge rather than four per path.
///
/// # Errors
///
/// Git failing to run at all. A merge git cannot answer for is counted
/// as a skip and the scan continues; a directory that is not a
/// repository is an error.
pub fn scan_repo(repo: &std::path::Path, max: usize) -> Result<ScanReport, String> {
    let merges = merge_log(repo, max)?;
    let mut report = ScanReport {
        merges_seen: merges.len(),
        ..ScanReport::default()
    };

    for merge in &merges {
        if merge.parents.len() != 2 {
            report.skipped_octopus += 1;
            continue;
        }
        let (target, proposed) = (&merge.parents[0], &merge.parents[1]);
        let Some(base) = merge_base(repo, target, proposed)? else {
            report.skipped_no_base += 1;
            continue;
        };
        let Ok(paths) = changed_paths(repo, target, &merge.id) else {
            report.skipped_no_base += 1;
            continue;
        };
        if paths.is_empty() {
            report.merges_scanned += 1;
            continue;
        }

        let mut revs = Vec::with_capacity(paths.len() * 4);
        for path in &paths {
            for rev in [&base, target, proposed, &merge.id] {
                revs.push(format!("{rev}:{path}"));
            }
        }
        let blobs = cat_file_batch(repo, &revs)?;

        let mut scannable = Vec::with_capacity(paths.len());
        for (i, path) in paths.iter().enumerate() {
            let four = &blobs[i * 4..i * 4 + 4];
            if four.iter().flatten().any(|b| b.len() > MAX_BLOB_BYTES) {
                report.skipped_large += 1;
                continue;
            }
            let mut texts = Vec::with_capacity(4);
            for blob in four {
                match blob {
                    // A path absent from a tree reads as empty, which is
                    // what makes an added or deleted file compare
                    // correctly rather than being skipped.
                    None => texts.push(String::new()),
                    Some(bytes) => match std::str::from_utf8(bytes) {
                        Ok(s) => texts.push(s.to_string()),
                        Err(_) => break,
                    },
                }
            }
            if texts.len() != 4 {
                report.skipped_binary += 1;
                continue;
            }
            let mut it = texts.into_iter();
            scannable.push((
                path.clone(),
                Blobs {
                    base: it.next().unwrap_or_default(),
                    target: it.next().unwrap_or_default(),
                    proposed: it.next().unwrap_or_default(),
                    result: it.next().unwrap_or_default(),
                },
            ));
        }
        scan_merge(&merge.id, &scannable, &mut report);
    }
    Ok(report)
}
