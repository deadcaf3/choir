//! Executable merged-vs-both-parents differential testing (D23).
//!
//! The same explicit command runs in three caller-supplied trees. A failure is
//! merge-specific only when both parents pass and the merged tree fails. If a
//! parent already fails, the observation is inconclusive rather than evidence
//! about the merge. This is intentionally smaller than a test orchestrator:
//! callers own checkout/sandbox construction, while this module fixes the
//! classification and calibration semantics shared by queue implementations.
//!
//! Commands are argv, not shell strings. Execution is synchronous and adds no
//! runtime or dependency.

use std::path::Path;

/// One command's process outcome in one revision tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observation {
    /// Whether the process exited successfully.
    pub success: bool,
    /// Numeric exit code, or `None` when the process ended by signal.
    pub exit_code: Option<i32>,
}

/// What the three executions establish about the merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Both parents and the merge passed this command.
    Clean,
    /// Both parents passed and only the merge failed.
    InteractionFailure,
    /// At least one parent failed, so the merge cannot be blamed.
    InconclusiveParentFailure,
}

/// Complete three-revision receipt for one command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DifferentialReport {
    /// First parent outcome.
    pub parent_a: Observation,
    /// Second parent outcome.
    pub parent_b: Observation,
    /// Merged-tree outcome.
    pub merged: Observation,
    /// Classification derived from the three outcomes.
    pub verdict: Verdict,
}

fn run_one(program: &str, args: &[String], dir: &Path) -> Result<Observation, String> {
    let status = std::process::Command::new(program)
        .args(args)
        .current_dir(dir)
        .status()
        .map_err(|error| format!("run differential command in {}: {error}", dir.display()))?;
    Ok(Observation {
        success: status.success(),
        exit_code: status.code(),
    })
}

/// Runs one explicit command in both parent trees and the merged tree.
///
/// # Errors
///
/// The program could not be spawned in one of the three directories.
pub fn run_merged_vs_parents(
    program: &str,
    args: &[String],
    parent_a: &Path,
    parent_b: &Path,
    merged: &Path,
) -> Result<DifferentialReport, String> {
    if program.is_empty() {
        return Err("differential program must not be empty".to_string());
    }
    let parent_a = run_one(program, args, parent_a)?;
    let parent_b = run_one(program, args, parent_b)?;
    let merged = run_one(program, args, merged)?;
    let verdict = if !parent_a.success || !parent_b.success {
        Verdict::InconclusiveParentFailure
    } else if !merged.success {
        Verdict::InteractionFailure
    } else {
        Verdict::Clean
    };
    Ok(DifferentialReport {
        parent_a,
        parent_b,
        merged,
        verdict,
    })
}

/// Count-based false-positive calibration for differential failures.
///
/// Every interaction failure must be adjudicated before it enters the
/// denominator. The operational D23 target is exact rational arithmetic:
/// spurious failures / evaluated merges must be strictly below 1/1000. This
/// reports the observed rate; it makes no statistical confidence claim.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Calibration {
    evaluated_merges: u64,
    inconclusive_merges: u64,
    interaction_failures: u64,
    confirmed_interactions: u64,
    spurious_failures: u64,
}

impl Calibration {
    /// Records one report. Interaction failures require a ground-truth
    /// adjudication (`true` = real interaction, `false` = spurious).
    /// Clean and inconclusive reports must not carry an adjudication.
    ///
    /// # Errors
    ///
    /// An interaction is unadjudicated, or an adjudication is attached to a
    /// report that did not flag an interaction.
    pub fn record(
        &mut self,
        report: &DifferentialReport,
        real_interaction: Option<bool>,
    ) -> Result<(), String> {
        match report.verdict {
            Verdict::Clean => {
                if real_interaction.is_some() {
                    return Err("a clean report must not carry an adjudication".to_string());
                }
                self.evaluated_merges = self.evaluated_merges.saturating_add(1);
            }
            Verdict::InteractionFailure => {
                let real = real_interaction
                    .ok_or("an interaction failure needs adjudication before calibration")?;
                self.evaluated_merges = self.evaluated_merges.saturating_add(1);
                self.interaction_failures = self.interaction_failures.saturating_add(1);
                if real {
                    self.confirmed_interactions = self.confirmed_interactions.saturating_add(1);
                } else {
                    self.spurious_failures = self.spurious_failures.saturating_add(1);
                }
            }
            Verdict::InconclusiveParentFailure => {
                if real_interaction.is_some() {
                    return Err("an inconclusive report must not carry an adjudication".to_string());
                }
                self.inconclusive_merges = self.inconclusive_merges.saturating_add(1);
            }
        }
        Ok(())
    }

    /// Whether the observed spurious-failure rate is strictly below 0.1%.
    /// `None` means no merge has produced a conclusive three-revision result.
    #[must_use]
    pub fn target_met(&self) -> Option<bool> {
        (self.evaluated_merges != 0)
            .then(|| u128::from(self.spurious_failures) * 1000 < u128::from(self.evaluated_merges))
    }

    /// Versioned JSON receipt. The exact numerator and denominator are
    /// action-driving; basis points are presentation only and floor-rounded.
    #[must_use]
    pub fn receipt(&self) -> serde_json::Value {
        let rate_basis_points = (self.evaluated_merges != 0).then(|| {
            (u128::from(self.spurious_failures) * 10_000 / u128::from(self.evaluated_merges)) as u64
        });
        serde_json::json!({
            "format_version": 1,
            "evaluated_merges": self.evaluated_merges,
            "inconclusive_parent_failures": self.inconclusive_merges,
            "interaction_failures": self.interaction_failures,
            "confirmed_interactions": self.confirmed_interactions,
            "spurious_failures": self.spurious_failures,
            "spurious_failure_rate": {
                "numerator": self.spurious_failures,
                "denominator": self.evaluated_merges,
                "basis_points_floor": rate_basis_points,
            },
            "target": {
                "numerator": 1,
                "denominator": 1000,
                "comparison": "strictly_less_than",
                "met": self.target_met(),
            },
            "confidence_claim": null,
        })
    }
}
