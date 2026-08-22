//! The local subprocess executor: the second implementation of the D18
//! seam, and the one that ships with it.
//!
//! It runs each job as an ordinary child process on this machine. No
//! virtual machine, no shared cache, no isolation beyond what the OS
//! gives a subprocess — which makes it unsuitable for untrusted code
//! and exactly right as the backend that keeps the seam honest. A
//! conformance suite with one implementation has never been tested
//! against disagreement.
//!
//! # Examples
//!
//! ```
//! use choir_queue::executor::{CiExecutor, Job, Verdict};
//! use choir_queue::local::LocalRunner;
//! use choir_hash::ContentHash;
//!
//! let mut ci = LocalRunner::new();
//! let job = Job::new(ContentHash::blake3(b"tree"), vec!["true".into()]);
//! assert_eq!(ci.run(&[job]).unwrap(), vec![Verdict::Passed]);
//! ```

use crate::executor::{CiExecutor, ExecutorError, ExecutorInfo, Job, Verdict, PROTOCOL};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How often a running child is checked against its deadline.
const POLL: Duration = Duration::from_millis(10);

/// Runs jobs as child processes of this one.
#[derive(Debug, Default)]
pub struct LocalRunner {
    /// Most jobs to have in flight at once. `None` means one thread per
    /// job.
    parallelism: Option<usize>,
}

impl LocalRunner {
    /// A runner that gives every job its own thread.
    #[must_use]
    pub fn new() -> Self {
        Self { parallelism: None }
    }

    /// A runner that keeps at most `n` jobs in flight.
    ///
    /// # Panics
    ///
    /// If `n` is zero, which would accept jobs and never run them.
    #[must_use]
    pub fn with_parallelism(n: usize) -> Self {
        assert!(n > 0, "a runner with no parallelism would never run a job");
        Self {
            parallelism: Some(n),
        }
    }
}

/// Runs one job to completion, or to its deadline.
///
/// Every failure to *start* is [`Verdict::Errored`] and every completed
/// run is `Passed`/`Failed`. That split is the seam's central claim, so
/// it is made in one place rather than at each call site.
fn run_one(job: &Job) -> Verdict {
    let Some((program, args)) = job.command.split_first() else {
        return Verdict::Errored {
            provider: "local".to_string(),
            detail: "job has no command".to_string(),
        };
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .env_clear()
        .envs(&job.environment)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(dir) = &job.directory {
        // A directory that does not exist makes the spawn fail, which
        // lands in the `Err` arm below as `Errored` — correct, and the
        // reason this is not checked separately here. It is our
        // misconfiguration, not the change's fault.
        command.current_dir(dir);
    }
    let spawned = command.spawn();
    let mut child = match spawned {
        Ok(c) => c,
        // A command that does not exist is our problem, not the
        // change's: it means the job was configured wrong or the
        // executor's image is missing a tool.
        Err(e) => {
            return Verdict::Errored {
                provider: "local".to_string(),
                detail: format!("could not start `{program}`: {e}"),
            }
        }
    };

    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return if status.success() {
                    Verdict::Passed
                } else {
                    Verdict::Failed {
                        exit_code: status.code(),
                    }
                }
            }
            Ok(None) => {
                if started.elapsed() >= job.deadline {
                    // Kill *and reap*. A timed-out job that leaves a
                    // zombie holding the tree would make the next run
                    // fail for a reason that has nothing to do with it.
                    let _ = child.kill();
                    let _ = child.wait();
                    return Verdict::TimedOut;
                }
                std::thread::sleep(POLL);
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Verdict::Errored {
                    provider: "local".to_string(),
                    detail: format!("could not wait on `{program}`: {e}"),
                };
            }
        }
    }
}

impl CiExecutor for LocalRunner {
    fn info(&mut self) -> Result<ExecutorInfo, ExecutorError> {
        Ok(ExecutorInfo {
            name: "local".to_string(),
            protocol: PROTOCOL,
        })
    }

    fn run(&mut self, jobs: &[Job]) -> Result<Vec<Verdict>, ExecutorError> {
        let width = self.parallelism.unwrap_or(jobs.len()).max(1);
        let mut out = Vec::with_capacity(jobs.len());
        for chunk in jobs.chunks(width) {
            // Scoped threads so the jobs may borrow, and so a panicking
            // job cannot outlive this call.
            let results: Vec<Verdict> = std::thread::scope(|s| {
                // The `collect` is load-bearing and clippy wants it gone.
                // Without it the iterator is lazy: each `join` runs
                // immediately after its own `spawn`, and the batch
                // executes one job at a time. That is the exact defect
                // this seam was redesigned to make impossible, it is
                // invisible in every ordering assertion, and
                // `the_runner_runs_a_batch_concurrently` is the test that
                // fails if this line is "simplified".
                #[allow(clippy::needless_collect)]
                let handles = chunk
                    .iter()
                    .map(|j| s.spawn(|| run_one(j)))
                    .collect::<Vec<_>>();
                handles
                    .into_iter()
                    .map(|h| {
                        h.join().unwrap_or(Verdict::Errored {
                            provider: "local".to_string(),
                            detail: "the job thread panicked".to_string(),
                        })
                    })
                    .collect()
            });
            out.extend(results);
        }
        Ok(out)
    }
}
