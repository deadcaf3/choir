//! Executable merged-vs-both-parents differential testing (D23).
//!
//! The same explicit command runs in three caller-supplied trees. A failure is
//! merge-specific only when both parents pass and the merged tree fails. If a
//! parent already fails, the observation is inconclusive rather than evidence
//! about the merge. This is intentionally smaller than a test orchestrator:
//! callers own checkout/sandbox construction, while this module fixes the
//! classification and calibration semantics shared by queue implementations.
//!
//! Commands are argv, not shell strings, and the environment they run in is
//! explicit: the child sees exactly the map the caller passes, never this
//! process's inherited variables. Execution is synchronous and adds no
//! runtime or dependency.

use std::collections::BTreeMap;
use std::path::Path;

/// One command's process outcome in one revision tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observation {
    /// Whether the process exited successfully.
    pub success: bool,
    /// Numeric exit code, or `None` when the process ended by signal.
    pub exit_code: Option<i32>,
}

impl Observation {
    /// Versioned-report representation used by the calibration ledger.
    #[must_use]
    pub fn to_json(self) -> serde_json::Value {
        serde_json::json!({
            "success": self.success,
            "exit_code": self.exit_code,
        })
    }

    fn from_json(value: &serde_json::Value) -> Result<Self, String> {
        let success = value["success"]
            .as_bool()
            .ok_or("differential observation needs boolean success")?;
        let exit_code = match &value["exit_code"] {
            serde_json::Value::Null => None,
            value => Some(
                i32::try_from(
                    value
                        .as_i64()
                        .ok_or("differential observation exit_code must be an integer or null")?,
                )
                .map_err(|_| "differential observation exit_code is outside i32")?,
            ),
        };
        Ok(Self { success, exit_code })
    }
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

impl Verdict {
    /// Stable spelling used in versioned receipts.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::InteractionFailure => "interaction_failure",
            Self::InconclusiveParentFailure => "inconclusive_parent_failure",
        }
    }

    fn from_str(value: &str) -> Result<Self, String> {
        match value {
            "clean" => Ok(Self::Clean),
            "interaction_failure" => Ok(Self::InteractionFailure),
            "inconclusive_parent_failure" => Ok(Self::InconclusiveParentFailure),
            _ => Err("unknown differential verdict".to_string()),
        }
    }
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

impl DifferentialReport {
    /// Stable JSON embedded in each versioned observation row.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "parent_a": self.parent_a.to_json(),
            "parent_b": self.parent_b.to_json(),
            "merged": self.merged.to_json(),
            "verdict": self.verdict.as_str(),
        })
    }

    /// Parses and independently rechecks the recorded classification.
    ///
    /// # Errors
    ///
    /// A required field is absent, malformed, or claims a verdict that does
    /// not follow from the three process observations.
    pub fn from_json(value: &serde_json::Value) -> Result<Self, String> {
        let parent_a = Observation::from_json(&value["parent_a"])?;
        let parent_b = Observation::from_json(&value["parent_b"])?;
        let merged = Observation::from_json(&value["merged"])?;
        let recorded = Verdict::from_str(
            value["verdict"]
                .as_str()
                .ok_or("differential report needs a verdict")?,
        )?;
        let derived = if !parent_a.success || !parent_b.success {
            Verdict::InconclusiveParentFailure
        } else if !merged.success {
            Verdict::InteractionFailure
        } else {
            Verdict::Clean
        };
        if recorded != derived {
            return Err("differential report verdict disagrees with its observations".to_string());
        }
        Ok(Self {
            parent_a,
            parent_b,
            merged,
            verdict: derived,
        })
    }
}

/// Ends a timed-out run and everything it spawned.
///
/// `Child::kill` signals one pid. The commands this module runs are whole
/// test invocations, and a shell forks rather than execs anything that is
/// not its tail call, so the pid we hold is usually a shell whose death
/// leaves the real work running. Signalling the group covers the
/// descendants; the direct kill after it is the fallback for a child that
/// never got a group of its own. Without this the deadline bounded the
/// verdict but not the machine: each timeout left a live process behind.
fn kill_run_and_descendants(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // std has no `killpg` and the workspace has no libc dependency, so
        // the one call is declared here — the trade `choir-spike` already
        // makes for `clonefile`. It has to be a syscall rather than a
        // spawned `kill`: what this recovers from is a machine filling with
        // survivors, which is exactly when spawning anything is least
        // likely to work.
        extern "C" {
            fn killpg(pgrp: std::ffi::c_int, sig: std::ffi::c_int) -> std::ffi::c_int;
            fn getpgid(pid: std::ffi::c_int) -> std::ffi::c_int;
        }
        // `process_group(0)` above made the child its own group leader.
        // Checked rather than assumed, because the two live thirty lines
        // apart and the failure is not local: a pid that is *not* also a
        // group id names some other group, and signalling that would kill
        // processes this module never started. If the spawn ever stops
        // setting the group, this degrades to the direct kill below.
        if let Ok(pid) = std::ffi::c_int::try_from(child.id()) {
            if unsafe { getpgid(pid) } == pid {
                // SIGKILL, not SIGTERM: the run has already ignored its
                // deadline, so this is the reap, not a wind-down request.
                unsafe { killpg(pid, 9) };
            }
        }
    }
    child.kill().ok();
}

fn run_one(
    program: &str,
    args: &[String],
    dir: &Path,
    env: &BTreeMap<String, String>,
    timeout: Option<std::time::Duration>,
) -> Result<Observation, String> {
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .env_clear()
        .envs(env)
        .current_dir(dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Its own process group, so a deadline can reach what the command
    // spawned and not only the command. The cost is that the child no
    // longer shares this process's terminal group, so an interactive
    // Ctrl-C reaches it through us rather than directly.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("run differential command in {}: {error}", dir.display()))?;
    let status = match timeout {
        None => child.wait().map_err(|error| {
            format!(
                "wait for differential command in {}: {error}",
                dir.display()
            )
        })?,
        Some(timeout) => {
            // Hand-rolled deadline poll: no wait-with-timeout in std, and no
            // dependency for something this small. 100 ms of granularity is
            // noise against runs measured in tens of seconds.
            let deadline = std::time::Instant::now() + timeout;
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => break status,
                    Ok(None) if std::time::Instant::now() >= deadline => {
                        kill_run_and_descendants(&mut child);
                        child.wait().ok();
                        // Operational error, never a verdict: a timeout cannot
                        // distinguish a hang from a slow run, so it must not
                        // be allowed to mint an interaction failure.
                        return Err(format!(
                            "differential command in {} timed out after {} s",
                            dir.display(),
                            timeout.as_secs()
                        ));
                    }
                    Ok(None) => std::thread::sleep(std::time::Duration::from_millis(100)),
                    Err(error) => {
                        return Err(format!(
                            "wait for differential command in {}: {error}",
                            dir.display()
                        ))
                    }
                }
            }
        }
    };
    Ok(Observation {
        success: status.success(),
        exit_code: status.code(),
    })
}

/// Runs one explicit command in both parent trees and the merged tree.
///
/// `env` is the complete environment every run sees; nothing is inherited
/// from this process. That closes the reproducibility gap where "same
/// command" enforced same argv while an ambient variable (a leaked
/// `CARGO_TARGET_DIR`, a changed `RUSTFLAGS`) silently changed what the
/// three runs measured. Callers building the map from a command file should
/// use [`crate::differential_ledger::effective_environment`], which is what
/// the ledger's recorded environment hash is computed over.
///
/// The three runs happen **concurrently**, one thread each. They are
/// independent by construction — three separate checkouts, and the caller
/// owns their isolation — so this is a straight 3x on the dominant cost
/// without touching what is measured: every run is still really run, which
/// is what the calibration's declared `independent_runs` assumption needs.
/// A result cache would be faster still and would quietly void that
/// assumption, since determinism is the property under test.
///
/// The one thing to know before pointing a command at this: if the command
/// writes to a location the three trees *share* — an absolute
/// `--target-dir`, say — they will serialize on that tool's own lock
/// rather than run in parallel. Correctness is unaffected either way; the
/// speedup is not. Keeping such state per-tree is what makes this pay.
///
/// `timeout` bounds each of the three runs individually; `None` waits
/// forever, which was the only behavior before the parameter existed. A
/// run that exceeds it is killed and reported as an **error**, never as a
/// verdict — a timeout cannot distinguish a hung command from a slow one,
/// so it must not become evidence about the merge. Unattended corpus
/// walks (`choir-bridge harvest`, D27) are the reason it exists: one hung
/// test run must not stall a multi-hour walk forever.
///
/// # Errors
///
/// The program could not be spawned in one of the three directories, or a
/// run exceeded `timeout`. All
/// three are attempted before reporting: unlike the previous sequential
/// form, a spawn failure in `parent_a` no longer prevents the other two
/// from running. The reported error is still the earliest in
/// `parent_a`, `parent_b`, `merged` order, so the message a caller sees
/// for a given failure is unchanged.
///
/// # Panics
///
/// If one of the three worker threads panics, this propagates that panic
/// rather than reporting a verdict computed from two runs.
pub fn run_merged_vs_parents(
    program: &str,
    args: &[String],
    parent_a: &Path,
    parent_b: &Path,
    merged: &Path,
    env: &BTreeMap<String, String>,
    timeout: Option<std::time::Duration>,
) -> Result<DifferentialReport, String> {
    if program.is_empty() {
        return Err("differential program must not be empty".to_string());
    }
    // `scope` rather than `spawn`: the borrows of `program`, `args` and the
    // three paths outlive the threads without cloning anything, and the
    // scope will not return until all three have been joined.
    let (parent_a, parent_b, merged) = std::thread::scope(|scope| {
        let a = scope.spawn(|| run_one(program, args, parent_a, env, timeout));
        let b = scope.spawn(|| run_one(program, args, parent_b, env, timeout));
        let m = scope.spawn(|| run_one(program, args, merged, env, timeout));
        (
            a.join().expect("parent-a differential thread panicked"),
            b.join().expect("parent-b differential thread panicked"),
            m.join().expect("merged differential thread panicked"),
        )
    });
    let (parent_a, parent_b, merged) = (parent_a?, parent_b?, merged?);
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

/// Minimum conclusive sample for the fixed 95% zero-spurious confidence rule.
///
/// At the target boundary, `(999 / 1000)^2994` remains above 5%, while
/// `(999 / 1000)^2995` is below 5%. The rule is deliberately conservative:
/// any observed spurious failure refuses the confidence claim rather than
/// selecting a more favorable test after seeing the data.
pub const CONFIDENCE_MIN_EVALUATED_MERGES: u64 = 2_995;

/// Frozen statistical rule attached to every calibration receipt.
///
/// This rule only covers sampling error. Its independence and
/// representativeness assumptions are named rather than inferred from counts.
#[must_use]
pub fn confidence_policy() -> serde_json::Value {
    serde_json::json!({
        "format_version": 1,
        "method": "one_sided_exact_binomial_zero_spurious",
        "confidence": {
            "numerator": 95,
            "denominator": 100,
        },
        "target": {
            "numerator": 1,
            "denominator": 1000,
            "comparison": "strictly_less_than",
        },
        "minimum_evaluated_merges": CONFIDENCE_MIN_EVALUATED_MERGES,
        "requires_zero_spurious_failures": true,
        "assumptions": [
            "independent_runs",
            "representative_queue_command_and_merge_population",
        ],
    })
}

/// Count-based false-positive calibration for differential failures.
///
/// Every interaction failure must be adjudicated before the target has a
/// verdict. The operational D23 target is exact rational arithmetic: spurious
/// failures / evaluated merges must be strictly below 1/1000. This reports the
/// observed rate. The frozen confidence rule is a sufficient zero-spurious
/// test and remains indeterminate until its minimum sample is reached.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Calibration {
    evaluated_merges: u64,
    inconclusive_merges: u64,
    interaction_failures: u64,
    confirmed_interactions: u64,
    spurious_failures: u64,
    pending_interactions: u64,
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

    /// Records an interaction flag whose ground truth has not been decided.
    /// It enters the observed denominator, but suppresses the target verdict
    /// until an adjudication-led replay replaces it with [`Self::record`].
    ///
    /// # Errors
    ///
    /// The report is not an interaction failure.
    pub fn record_pending(&mut self, report: &DifferentialReport) -> Result<(), String> {
        if report.verdict != Verdict::InteractionFailure {
            return Err("only an interaction failure can be pending adjudication".to_string());
        }
        self.evaluated_merges = self.evaluated_merges.saturating_add(1);
        self.interaction_failures = self.interaction_failures.saturating_add(1);
        self.pending_interactions = self.pending_interactions.saturating_add(1);
        Ok(())
    }

    /// Whether the observed spurious-failure rate is strictly below 0.1%.
    /// `None` means no merge has produced a conclusive three-revision result.
    #[must_use]
    pub fn target_met(&self) -> Option<bool> {
        (self.evaluated_merges != 0 && self.pending_interactions == 0)
            .then(|| u128::from(self.spurious_failures) * 1000 < u128::from(self.evaluated_merges))
    }

    fn confidence_claim(&self) -> Option<bool> {
        (self.evaluated_merges >= CONFIDENCE_MIN_EVALUATED_MERGES && self.pending_interactions == 0)
            .then_some(self.spurious_failures == 0)
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
            "pending_interactions": self.pending_interactions,
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
            "confidence_claim": self.confidence_claim(),
            "confidence_policy": confidence_policy(),
            "landing_gate_enabled": false,
        })
    }
}
