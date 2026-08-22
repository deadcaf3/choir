//! The CI executor seam (D18): what runs a candidate merge, and what it
//! is allowed to say about the result.
//!
//! This replaces a `fn verdict(&Change, &str) -> bool` that could not
//! carry a real executor. Three things the bool could not express, each
//! of which changed a decision the queue makes:
//!
//! 1. **A provider fault is not a test failure.** A VM that fails to
//!    boot and a test that legitimately fails both returned `false`, and
//!    the queue ejected the change either way — along with everything
//!    that transitively depended on it. [`Verdict::evicts`] is now the
//!    single place that rule lives, and only [`Verdict::Failed`] says
//!    yes.
//! 2. **A batch is the unit, so a provider may be concurrent.** The
//!    train's cost model assumes every member's CI runs in parallel;
//!    a one-job-at-a-time method with `&mut self` made that impossible
//!    to implement no matter what the model said.
//! 3. **A job is content-addressed**, so a shared build cache has
//!    something to key on. A queue-local change id does not survive
//!    being asked the same question twice.
//!
//! # Examples
//!
//! ```
//! use choir_queue::executor::{CiExecutor, Job, Synthetic, Verdict};
//! use choir_hash::ContentHash;
//!
//! let mut ci = Synthetic::passing();
//! let job = Job::new(ContentHash::blake3(b"tree"), vec!["true".into()]);
//! assert_eq!(ci.run(&[job]).unwrap(), vec![Verdict::Passed]);
//! ```

use choir_hash::ContentHash;
use std::collections::BTreeMap;
use std::time::Duration;

/// Wire-protocol version a provider must echo before receiving work.
///
/// A provider built against a different schema is refused at the
/// handshake rather than discovered through a verdict that means
/// something else than it appears to.
pub const PROTOCOL: u32 = 1;

/// Default wall-clock ceiling for one job.
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(600);

/// One unit of work, addressed by content so a shared cache can hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// Content address of the speculative tree state under test.
    pub subject: ContentHash,
    /// What this job is testing, for reports and for an operator
    /// reading a failure. Typically the change id.
    ///
    /// Deliberately outside [`Job::cache_key`]: two jobs testing
    /// byte-identical trees with the same command are the same work
    /// whichever change produced them, and that is precisely the case a
    /// shared cache exists to collapse. A label inside the key would
    /// make every job unique and the cache useless.
    pub label: String,
    /// The command, as argv. Never a shell string: the same rule
    /// [`crate::differential`] already follows, for the same reason.
    pub command: Vec<String>,
    /// Exactly what the child process sees. Never inherited from this
    /// process, so a job's result cannot depend on the environment of
    /// whoever happened to run the queue.
    pub environment: BTreeMap<String, String>,
    /// Wall-clock ceiling. Exceeding it is [`Verdict::TimedOut`], which
    /// is a provider outcome and not a statement about the change.
    pub deadline: Duration,
    /// Whether this job's artifacts may be written to a shared build
    /// cache.
    ///
    /// False for untrusted and fork builds. This is the CREEP-class
    /// mitigation ("only trusted executors write the action cache")
    /// expressed as a property of the job rather than as a deployment
    /// note, because a deployment note is not enforcement.
    pub may_write_cache: bool,
}

impl Job {
    /// A job with the documented defaults: a full deadline, an empty
    /// environment, and no permission to write the shared cache.
    ///
    /// Cache-write permission is opt-in rather than opt-out on purpose.
    /// The failure mode of the safe default is a slow build; the
    /// failure mode of the other one is a poisoned artifact.
    #[must_use]
    pub fn new(subject: ContentHash, command: Vec<String>) -> Self {
        Self {
            subject,
            label: String::new(),
            command,
            environment: BTreeMap::new(),
            deadline: DEFAULT_DEADLINE,
            may_write_cache: false,
        }
    }

    /// Content address of the work, for a shared cache to key on.
    ///
    /// Covers what determines the output — the tree, the command, the
    /// environment — and deliberately not `deadline` or
    /// `may_write_cache`, which govern *how* the job may run rather
    /// than what it computes. Two jobs that differ only in how long
    /// they are allowed to take are the same question.
    #[must_use]
    pub fn cache_key(&self) -> ContentHash {
        let mut bytes = Vec::new();
        push_field(&mut bytes, self.subject.to_hex().as_bytes());
        push_len(&mut bytes, self.command.len());
        for arg in &self.command {
            push_field(&mut bytes, arg.as_bytes());
        }
        push_len(&mut bytes, self.environment.len());
        for (k, v) in &self.environment {
            push_field(&mut bytes, k.as_bytes());
            push_field(&mut bytes, v.as_bytes());
        }
        ContentHash::blake3(&bytes)
    }
}

/// Write a count into the cache-key preimage.
fn push_len(bytes: &mut Vec<u8>, len: usize) {
    bytes.extend_from_slice(&(len as u64).to_le_bytes());
}

/// Write one length-prefixed field into the cache-key preimage.
///
/// Separators are not enough here. With a `\0` written between
/// arguments, `["a", "b"]` and `["a\0b"]` produce the same preimage,
/// and a Rust `String` may contain a NUL. Two different commands
/// sharing one cache key is exactly the first-to-cache-wins poisoning
/// that [`Job::may_write_cache`] exists to bound, so the framing has to
/// be unambiguous rather than merely conventional. The counts do the
/// same job for the boundary between argv and the environment.
fn push_field(bytes: &mut Vec<u8>, field: &[u8]) {
    push_len(bytes, field.len());
    bytes.extend_from_slice(field);
}

/// What an executor found. Four cases, because the queue's response to
/// each of them differs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The command ran and exited zero.
    Passed,
    /// The command ran and exited nonzero. A statement about the change.
    Failed {
        /// Exit code, or `None` when the process ended by signal.
        exit_code: Option<i32>,
    },
    /// The provider could not produce a verdict at all. A statement
    /// about us, never about the change.
    Errored {
        /// Which provider failed, for an operator reading the report.
        provider: String,
        /// What went wrong.
        detail: String,
    },
    /// The job's deadline elapsed. Also not a statement about the
    /// change: a job may time out because the executor was oversubscribed.
    TimedOut,
}

impl Verdict {
    /// Whether this verdict may evict the change from the train.
    ///
    /// The whole reason [`Verdict`] is not a bool. Ejection is
    /// permanent for the change *and everything that transitively
    /// depends on it*, so it is reserved for the one case that is
    /// actually a statement about the change's content.
    #[must_use]
    pub fn evicts(&self) -> bool {
        matches!(self, Verdict::Failed { .. })
    }

    /// Whether the executor answered the question it was asked.
    ///
    /// `Passed` and `Failed` are answers. `Errored` and `TimedOut` are
    /// the absence of one, and a train holding either must stall rather
    /// than draw a conclusion.
    #[must_use]
    pub fn is_conclusive(&self) -> bool {
        matches!(self, Verdict::Passed | Verdict::Failed { .. })
    }
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Verdict::Passed => write!(f, "passed"),
            Verdict::Failed { exit_code: Some(c) } => write!(f, "failed (exit {c})"),
            Verdict::Failed { exit_code: None } => write!(f, "failed (killed by signal)"),
            Verdict::Errored { provider, detail } => write!(f, "{provider}: {detail}"),
            Verdict::TimedOut => write!(f, "timed out"),
        }
    }
}

/// Who a provider says it is, established once before any job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutorInfo {
    /// Provider name, for reports and for operator diagnosis.
    pub name: String,
    /// Protocol version the provider speaks. Must equal [`PROTOCOL`].
    pub protocol: u32,
}

/// Why an executor produced no verdicts at all.
///
/// Distinct from [`Verdict::Errored`], which is a per-job fault: these
/// are faults of the whole call, and none of them is evidence about any
/// change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutorError {
    /// The provider could not be reached or did not complete a
    /// handshake.
    Unavailable(String),
    /// The provider answered, but not in the shape the seam requires --
    /// a version it does not speak, or a verdict count that does not
    /// match the jobs it was given.
    Protocol(String),
}

impl std::fmt::Display for ExecutorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecutorError::Unavailable(m) => write!(f, "executor unavailable: {m}"),
            ExecutorError::Protocol(m) => write!(f, "executor protocol error: {m}"),
        }
    }
}

impl std::error::Error for ExecutorError {}

/// The CI executor seam (D18).
///
/// Implementations must pass `conformance` in
/// `choir-queue/tests/it/executor.rs`. A seam with one implementation
/// has never been tested against disagreement, which is how the trait
/// this replaces stayed plausible while being unable to carry a real
/// executor.
pub trait CiExecutor {
    /// Identity and protocol version. Called once before any job, so a
    /// mismatched provider is refused before it can return a verdict
    /// that means something else than it appears to.
    ///
    /// # Errors
    ///
    /// [`ExecutorError::Unavailable`] if the provider cannot be reached,
    /// [`ExecutorError::Protocol`] if it speaks a version we do not.
    fn info(&mut self) -> Result<ExecutorInfo, ExecutorError>;

    /// Runs one train's worth of jobs. The provider chooses its own
    /// concurrency.
    ///
    /// The returned vector is index-aligned with `jobs`, so a provider
    /// that drops or reorders a job fails a length check rather than
    /// silently attributing one change's result to another.
    ///
    /// # Errors
    ///
    /// [`ExecutorError`] when the call as a whole produced no verdicts.
    /// A per-job fault is [`Verdict::Errored`], not an error here.
    fn run(&mut self, jobs: &[Job]) -> Result<Vec<Verdict>, ExecutorError>;
}

/// An executor that answers from a caller-supplied function instead of
/// running anything.
///
/// Named rather than a blanket impl over `FnMut` on purpose. The seam
/// this replaces had exactly such a blanket impl, which meant every
/// test got a fake without ever choosing one, and the trait looked
/// exercised while no implementation had ever run a process. Reaching
/// for a fake is now a thing you can see in the source.
pub struct Synthetic {
    name: String,
    answer: Box<dyn FnMut(&Job) -> Verdict + Send>,
}

impl std::fmt::Debug for Synthetic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Synthetic")
            .field("name", &self.name)
            .finish()
    }
}

impl Synthetic {
    /// An executor whose every verdict is [`Verdict::Passed`].
    #[must_use]
    pub fn passing() -> Self {
        Self::new(|_| Verdict::Passed)
    }

    /// An executor that fails exactly the jobs whose label is listed
    /// and passes the rest.
    ///
    /// The shape most queue tests need: "change 7 breaks the build".
    #[must_use]
    pub fn failing_labels(labels: &[&str]) -> Self {
        let owned: Vec<String> = labels.iter().map(|l| (*l).to_string()).collect();
        Self::new(move |job| {
            if owned.contains(&job.label) {
                Verdict::Failed { exit_code: Some(1) }
            } else {
                Verdict::Passed
            }
        })
    }

    /// An executor answering from `answer`.
    pub fn new(answer: impl FnMut(&Job) -> Verdict + Send + 'static) -> Self {
        Self {
            name: "synthetic".to_string(),
            answer: Box::new(answer),
        }
    }
}

impl CiExecutor for Synthetic {
    fn info(&mut self) -> Result<ExecutorInfo, ExecutorError> {
        Ok(ExecutorInfo {
            name: self.name.clone(),
            protocol: PROTOCOL,
        })
    }

    fn run(&mut self, jobs: &[Job]) -> Result<Vec<Verdict>, ExecutorError> {
        Ok(jobs.iter().map(|j| (self.answer)(j)).collect())
    }
}
