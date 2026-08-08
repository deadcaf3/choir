//! What precision a detector can reach at *our* prevalence (D23).
//!
//! Published detector numbers are measured on corpora that are roughly
//! one-third positive, because they sample same-declaration parallel
//! edits. Deployed against a mainline where bad merges are well under
//! one percent, the same detector reports the same recall and a
//! completely different precision — the false positives are drawn from a
//! vastly larger pool of negatives.
//!
//! That transformation is the difference between "60% recall at 43%
//! precision, promising" and "one true alarm per 160", so it should be
//! computed rather than eyeballed. Recall and specificity are properties
//! of the detector and carry across; precision is a property of the
//! detector *and the population* and does not.
//!
//! # What the prevalence here actually is
//!
//! The measured 0.004 is `choir-queue::corpus`'s revert-labelled bad-merge
//! rate on rust-lang/rust — merges that history took back. That is not
//! the same event as "merges containing a semantic conflict": it misses
//! defects fixed forward and includes reverts that were not defects. It
//! is used because it is the only prevalence anyone has measured on a
//! real mainline, and because the operational question is about flagging
//! merges, not about conflicts in the abstract.

/// A detector's prevalence-independent behaviour.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Detector {
    /// Fraction of genuinely bad merges it flags. Carries across
    /// populations.
    pub recall: f64,
    /// Fraction of genuinely good merges it flags anyway. Carries across
    /// populations, and is the number that decides everything at low
    /// prevalence.
    pub false_positive_rate: f64,
}

impl Detector {
    /// Recovers the prevalence-independent behaviour from a paper's
    /// reported `precision` and `recall` at that paper's corpus
    /// `prevalence`.
    ///
    /// # Errors
    ///
    /// Inputs outside `0.0..=1.0`, a zero precision or recall (nothing to
    /// recover), or a corpus prevalence of 1.0 (no negatives to have
    /// produced the reported false positives).
    pub fn from_reported(precision: f64, recall: f64, prevalence: f64) -> Result<Self, String> {
        for (name, v) in [
            ("precision", precision),
            ("recall", recall),
            ("prevalence", prevalence),
        ] {
            if !(0.0..=1.0).contains(&v) || v.is_nan() {
                return Err(format!("{name} must be in 0.0..=1.0, got {v}"));
            }
        }
        if precision <= 0.0 || recall <= 0.0 {
            return Err("precision and recall must be positive to recover a rate".to_string());
        }
        if prevalence >= 1.0 {
            return Err("a corpus with no negatives cannot yield a false-positive rate".to_string());
        }
        // true positives per merge, then false positives per merge from
        // precision = TP / (TP + FP).
        let tp = prevalence * recall;
        let fp = tp * (1.0 - precision) / precision;
        Ok(Self {
            recall,
            false_positive_rate: fp / (1.0 - prevalence),
        })
    }

    /// Precision this detector reaches at `prevalence`.
    ///
    /// Returns 0.0 when it would flag nothing at all, which is the honest
    /// reading: a detector that never fires has no precision to speak of.
    #[must_use]
    pub fn precision_at(&self, prevalence: f64) -> f64 {
        let tp = prevalence * self.recall;
        let fp = (1.0 - prevalence) * self.false_positive_rate;
        if tp + fp <= 0.0 {
            return 0.0;
        }
        tp / (tp + fp)
    }

    /// False alarms per true alarm at `prevalence` — the number an
    /// on-call human actually experiences. `None` when it never fires on
    /// a true positive, so the ratio is undefined rather than infinite.
    #[must_use]
    pub fn false_alarms_per_hit(&self, prevalence: f64) -> Option<f64> {
        let tp = prevalence * self.recall;
        if tp <= 0.0 {
            return None;
        }
        Some((1.0 - prevalence) * self.false_positive_rate / tp)
    }
}

/// The false-positive rate a detector must not exceed to reach
/// `target_precision` at `prevalence` with `recall`.
///
/// This is the build-or-don't-build number: compare it against what the
/// literature actually achieves, or against a test suite's flake rate.
///
/// # Errors
///
/// Inputs outside `0.0..=1.0`, or a target precision of 1.0 (which
/// demands a zero false-positive rate and is not a useful target).
pub fn required_false_positive_rate(
    prevalence: f64,
    recall: f64,
    target_precision: f64,
) -> Result<f64, String> {
    for (name, v) in [
        ("prevalence", prevalence),
        ("recall", recall),
        ("target_precision", target_precision),
    ] {
        if !(0.0..=1.0).contains(&v) || v.is_nan() {
            return Err(format!("{name} must be in 0.0..=1.0, got {v}"));
        }
    }
    if target_precision >= 1.0 {
        return Err("a target precision of 1.0 demands zero false positives".to_string());
    }
    if prevalence >= 1.0 {
        return Err("prevalence must leave some negatives".to_string());
    }
    // precision = TP / (TP + FP) = target  =>  FP = TP * (1 - target) / target
    let tp = prevalence * recall;
    let fp = tp * (1.0 - target_precision) / target_precision;
    Ok(fp / (1.0 - prevalence))
}
