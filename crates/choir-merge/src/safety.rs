//! Merge-safety verdict: did a resolved merge stay inside what the author
//! proposed? (internal/oak.md item 1.)
//!
//! Adapted from Oak's four-tree merge-safety invariant (oak.space, repo
//! `oak/oak`, `cli/src/commands/merge_safety.rs`, Apache-2.0): *a path the
//! target changed since fork and the branch never touched must survive the
//! merge unchanged.* Re-derived for choir's single-file model as a
//! containment claim: every edit the landing applies to the target must be
//! an edit the author proposed. Any line the merge removes from the target
//! beyond what `base -> proposed` removes has been silently reverted; any
//! line it adds beyond what the author added has been silently injected.
//!
//! The comparison is over line *bags* (multisets), not edit scripts, so it
//! is independent of which minimal diff a strategy's algorithm happens to
//! produce. The trade-off is positional blindness: a merge that moves a
//! target line, or applies a proposed edit at the wrong occurrence of a
//! repeated line, is bag-neutral and passes. That misplacement class is
//! CI's and review's to catch; this check exists for the reversion class,
//! which CI misses precisely because reverted code still compiles and its
//! tests were green before the work it reverts landed (DECISIONS.md D23).
//!
//! A violation is not a conflict. A conflict is the pipeline saying "I
//! cannot resolve this"; a violation is a strategy claiming it resolved
//! while its output discards work. The distinction matters most for the
//! non-deterministic strategy slots — mergiraf and the future D19 LLM
//! resolver — whose failure mode is exactly a confident wrong answer.

use std::collections::BTreeMap;

/// Evidence that a resolved merge edited the target beyond the proposal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// Lines removed from the target that `base -> proposed` never removed:
    /// target-side work the merge silently reverted. Sorted, deduplicated.
    pub reverted: Vec<String>,
    /// Lines added to the result that `base -> proposed` never added:
    /// content the merge invented or resurrected. Sorted, deduplicated.
    pub injected: Vec<String>,
}

/// Verdict of [`check`] on one resolved merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SafetyVerdict {
    /// Every edit the merge applied to the target was proposed by the author.
    Upholds,
    /// The merge edited the target beyond the proposal; the evidence names
    /// the unattributable lines.
    Violation(Violation),
}

/// Line-occurrence bag of `text`, keyed by line content.
fn bag(text: &str) -> BTreeMap<&str, usize> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        *out.entry(line).or_insert(0) += 1;
    }
    out
}

/// Lines of `from` missing from `to` (with excess counts), and vice versa.
fn bag_delta<'a>(
    from: &BTreeMap<&'a str, usize>,
    to: &BTreeMap<&'a str, usize>,
) -> (BTreeMap<&'a str, usize>, BTreeMap<&'a str, usize>) {
    let mut removed = BTreeMap::new();
    let mut added = BTreeMap::new();
    for (line, n) in from {
        let have = to.get(line).copied().unwrap_or(0);
        if *n > have {
            removed.insert(*line, n - have);
        }
    }
    for (line, n) in to {
        let have = from.get(line).copied().unwrap_or(0);
        if *n > have {
            added.insert(*line, n - have);
        }
    }
    (removed, added)
}

/// Checks a resolved merge against the safety invariant.
///
/// `base` is what the author wrote against, `proposed` is what they wrote,
/// `target` is the state being merged onto (the speculative train state, or
/// a ref head), and `result` is the strategy's resolved output. Returns
/// [`SafetyVerdict::Upholds`] when `target -> result` removes and adds only
/// lines that `base -> proposed` removes and adds, counted as bags.
///
/// An author who *explicitly* proposes reverting target-side content passes
/// this check: the lines are attributable to their proposal, so nothing is
/// silent. Refusing deliberate reverts is policy (review, D24), not safety.
pub fn check(base: &str, target: &str, proposed: &str, result: &str) -> SafetyVerdict {
    let (landing_removed, landing_added) = bag_delta(&bag(target), &bag(result));
    let (proposed_removed, proposed_added) = bag_delta(&bag(base), &bag(proposed));

    let excess = |landing: &BTreeMap<&str, usize>, proposal: &BTreeMap<&str, usize>| {
        landing
            .iter()
            .filter(|(line, n)| **n > proposal.get(**line).copied().unwrap_or(0))
            .map(|(line, _)| line.to_string())
            .collect::<Vec<String>>()
    };
    let reverted = excess(&landing_removed, &proposed_removed);
    let injected = excess(&landing_added, &proposed_added);

    if reverted.is_empty() && injected.is_empty() {
        SafetyVerdict::Upholds
    } else {
        SafetyVerdict::Violation(Violation { reverted, injected })
    }
}
