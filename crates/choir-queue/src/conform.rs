//! The D18 executor conformance suite, runnable against a helper this
//! repository cannot build.
//!
//! The assertions themselves used to live only in
//! `choir-queue/tests/it/executor.rs`, which gates the four backends
//! cargo can link. That is every backend except the one the seam was
//! carved out for: a Firecracker or Cloud Hypervisor driver needs KVM,
//! so it is written and run on a Linux machine, and a suite it cannot
//! be pointed at gates nothing. Same checks, same order, in the library
//! instead, with `choir-ci-conform` as the caller that takes a helper
//! argv.
//!
//! [`conform`] reports rather than panics. A remote helper is usually
//! wrong in more than one way at once, and a suite that stops at the
//! first assertion costs a round trip per defect. The one exception is
//! the handshake: nothing after it means anything if the far side is
//! not the protocol we speak, so its failure skips the rest rather
//! than producing seven derived ones.
//!
//! # Examples
//!
//! ```no_run
//! use choir_queue::conform::{conform, Fixtures, Outcome};
//! use choir_queue::executor::Job;
//! use choir_queue::remote::ProtocolRunner;
//!
//! let subject = choir_hash::ContentHash::blake3(b"conform");
//! let sh = |script: &str| {
//!     Job::new(subject.clone(), vec!["/bin/sh".into(), "-c".into(), script.into()])
//! };
//! let mut slow = sh("sleep 30");
//! slow.deadline = std::time::Duration::from_millis(100);
//! let checks = conform(
//!     &mut ProtocolRunner::new(vec!["choir-ci-local".to_string()]),
//!     Fixtures {
//!         passing: sh("exit 0"),
//!         failing: sh("exit 3"),
//!         erroring: Job::new(subject.clone(), vec!["/nonexistent/helper".into()]),
//!         slow,
//!         in_directory: None,
//!     },
//! );
//! assert!(checks.iter().all(|c| !matches!(c.outcome, Outcome::Failed(_))));
//! ```

use crate::executor::{CiExecutor, Job, Verdict, PROTOCOL};

/// Jobs meaningful to one backend.
///
/// Supplied by the caller because "a command that exits 3" is spelled
/// differently in a VM than in a subprocess, which is exactly what the
/// seam abstracts. The checks are about the verdicts, never about the
/// commands that produced them.
pub struct Fixtures {
    /// A command that exits zero.
    pub passing: Job,
    /// A command that exits nonzero.
    pub failing: Job,
    /// A job the provider cannot start at all.
    pub erroring: Job,
    /// A job that outlives its deadline.
    pub slow: Job,
    /// A job that passes only if the executor ran it in
    /// [`Job::directory`], and fails otherwise. `None` for a backend
    /// with no directory to honor -- an isolation boundary the caller's
    /// filesystem does not cross is the ordinary case, and skipping is
    /// the honest answer rather than a failure.
    pub in_directory: Option<Job>,
}

/// What one check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The executor satisfied the check.
    Passed,
    /// It did not, and this is what was expected instead.
    Failed(String),
    /// The check did not run, and this is why. Never a pass: a report
    /// naming what it could not establish is the point of the variant.
    Skipped(String),
}

/// One named check and its outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// Stable name, so a helper's author can be told which one failed.
    pub name: &'static str,
    /// What it found.
    pub outcome: Outcome,
}

impl std::fmt::Display for Check {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.outcome {
            Outcome::Passed => write!(f, "ok       {}", self.name),
            Outcome::Failed(why) => write!(f, "FAILED   {}: {why}", self.name),
            Outcome::Skipped(why) => write!(f, "skipped  {}: {why}", self.name),
        }
    }
}

/// Every check an implementation of [`CiExecutor`] must satisfy.
///
/// Returns one [`Check`] per rule, in a fixed order, whatever happens.
/// The caller decides what a failure means: the in-tree suite asserts
/// none, `choir-ci-conform` prints them and exits nonzero.
#[must_use]
pub fn conform(ci: &mut dyn CiExecutor, f: Fixtures) -> Vec<Check> {
    let mut checks = Vec::new();

    // 1. The handshake precedes work and pins the protocol.
    let handshake = match ci.info() {
        Err(e) => Err(format!("the handshake did not complete: {e}")),
        Ok(info) if info.protocol != PROTOCOL => Err(format!(
            "executor `{}` speaks protocol {} and we speak {PROTOCOL}",
            info.name, info.protocol
        )),
        Ok(info) if info.name.is_empty() => Err("an executor must name itself".to_string()),
        Ok(_) => Ok(()),
    };
    let refused = handshake.is_err();
    checks.push(check("handshake", handshake));
    if refused {
        // Nothing after this is evidence about anything.
        for name in [
            "empty-batch",
            "passed",
            "failed",
            "errored",
            "timed-out",
            "alignment",
            "directory",
        ] {
            checks.push(Check {
                name,
                outcome: Outcome::Skipped("the handshake failed".to_string()),
            });
        }
        return checks;
    }

    // 2. An empty batch is an empty answer, not an error. The train can
    //    legitimately be empty and that must not read as a fault.
    checks.push(check(
        "empty-batch",
        match ci.run(&[]) {
            Err(e) => Err(format!("an empty batch must not error: {e}")),
            Ok(v) if v.is_empty() => Ok(()),
            Ok(v) => Err(format!("an empty batch must answer nothing, got {v:?}")),
        },
    ));

    // 3. A command that succeeds passes.
    checks.push(check(
        "passed",
        one(ci, &f.passing).and_then(|v| match v {
            Verdict::Passed => Ok(()),
            other => Err(format!("a zero exit must be Passed, got {other:?}")),
        }),
    ));

    // 4. A command that exits nonzero is `Failed` -- about the change --
    //    and is the only verdict permitted to evict.
    checks.push(check(
        "failed",
        one(ci, &f.failing).and_then(|v| match v {
            Verdict::Failed { .. } if v.evicts() && v.is_conclusive() => Ok(()),
            Verdict::Failed { .. } => {
                Err("a test failure must evict and must be conclusive".to_string())
            }
            other => Err(format!("a nonzero exit must be Failed, got {other:?}")),
        }),
    ));

    // 5. A provider that cannot start the job is `Errored` -- about us --
    //    and must NOT evict. This is the distinction the whole seam
    //    exists for, so it is checked rather than assumed.
    checks.push(check(
        "errored",
        one(ci, &f.erroring).and_then(|v| match v {
            Verdict::Errored { .. } if !v.evicts() && !v.is_conclusive() => Ok(()),
            Verdict::Errored { .. } => {
                Err("a provider fault must never evict and is not conclusive".to_string())
            }
            other => Err(format!(
                "a provider fault must be Errored, not a verdict about the change, got {other:?}"
            )),
        }),
    ));

    // 6. A job past its deadline is `TimedOut`, and also does not evict:
    //    a job can time out because the executor was oversubscribed.
    checks.push(check(
        "timed-out",
        one(ci, &f.slow).and_then(|v| match v {
            Verdict::TimedOut if !v.evicts() => Ok(()),
            Verdict::TimedOut => Err("a timeout must never evict a change".to_string()),
            other => Err(format!(
                "a job past its deadline must be TimedOut, got {other:?}"
            )),
        }),
    ));

    // 7. Index alignment. A provider that reorders or drops has
    //    attributed one change's result to another, and the batch call
    //    is worthless without this.
    let batch = vec![f.passing.clone(), f.failing.clone(), f.passing.clone()];
    checks.push(check(
        "alignment",
        match ci.run(&batch) {
            Err(e) => Err(format!("the mixed batch produced no verdicts: {e}")),
            Ok(out) if out.len() != batch.len() => Err(format!(
                "one verdict per job: {} jobs answered by {}",
                batch.len(),
                out.len()
            )),
            Ok(out)
                if out[0] == Verdict::Passed
                    && matches!(out[1], Verdict::Failed { .. })
                    && out[2] == Verdict::Passed =>
            {
                Ok(())
            }
            Ok(out) => Err(format!(
                "pass/fail/pass came back as {out:?}; a reordered batch attributes \
                 one change's result to another"
            )),
        },
    ));

    // 8. A provider that can run on a caller's checkout runs it in the
    //    directory the job names. The probe passes only from inside
    //    that directory, so an executor that ignores the field returns
    //    `Failed` -- a well-formed verdict about the wrong tree, which
    //    is the failure mode that made this worth a protocol bump.
    checks.push(match f.in_directory {
        None => Check {
            name: "directory",
            outcome: Outcome::Skipped(
                "no probe job supplied; this executor was not claimed to honor \
                 Job::directory"
                    .to_string(),
            ),
        },
        Some(job) => check(
            "directory",
            one(ci, &job).and_then(|v| match v {
                Verdict::Passed => Ok(()),
                other => Err(format!(
                    "the job did not run in the directory it named, got {other:?}"
                )),
            }),
        ),
    });

    checks
}

/// One job, one verdict, with the shapes that are not a verdict at all
/// flattened into the same failure string.
fn one(ci: &mut dyn CiExecutor, job: &Job) -> Result<Verdict, String> {
    match ci.run(std::slice::from_ref(job)) {
        Err(e) => Err(format!("the call produced no verdicts: {e}")),
        Ok(v) => v
            .into_iter()
            .next()
            .ok_or_else(|| "one job was answered by no verdict".to_string()),
    }
}

fn check(name: &'static str, result: Result<(), String>) -> Check {
    Check {
        name,
        outcome: match result {
            Ok(()) => Outcome::Passed,
            Err(why) => Outcome::Failed(why),
        },
    }
}
