//! Conformance for the D18 CI executor seam.
//!
//! The house rule is that a seam is real when a conformance suite plus a
//! second implementation both pass. The trait this replaced had neither,
//! which is how it stayed plausible for a year while being unable to
//! carry a real executor.
//!
//! Two backends run the suite, and they prove different amounts. Read
//! that honestly:
//!
//! - [`LocalRunner`] runs real child processes, so its pass is evidence
//!   about exit codes, deadlines, and start failures.
//! - [`Synthetic`] returns what it is told, so its pass proves only the
//!   *protocol* surface: the handshake, the empty batch, and index
//!   alignment. It cannot corroborate the verdict mapping, and saying so
//!   here is cheaper than someone later reading "2 backends pass" as
//!   more than it is.
//!
//! A microVM executor is the third call to `conformance`, and the reason
//! this file exists before it does.

use choir_hash::ContentHash;
use choir_queue::executor::{CiExecutor, ExecutorError, Job, Synthetic, Verdict, PROTOCOL};
use choir_queue::local::LocalRunner;
use std::time::Duration;

/// Jobs meaningful to one backend. A backend supplies its own, because
/// "a command that exits 3" is spelled differently in a VM than in a
/// subprocess -- which is exactly what the seam is abstracting.
struct Fixtures {
    passing: Job,
    failing: Job,
    /// A job the provider cannot start at all.
    erroring: Job,
    /// A job that outlives its deadline.
    slow: Job,
}

fn tree(name: &str) -> ContentHash {
    ContentHash::blake3(name.as_bytes())
}

/// Every assertion both backends must satisfy.
fn conformance(ci: &mut dyn CiExecutor, f: Fixtures) {
    // 1. The handshake precedes work and pins the protocol.
    let info = ci.info().expect("a reachable executor introduces itself");
    assert_eq!(
        info.protocol, PROTOCOL,
        "executor `{}` speaks protocol {} and we speak {PROTOCOL}",
        info.name, info.protocol
    );
    assert!(!info.name.is_empty(), "an executor must name itself");

    // 2. An empty batch is an empty answer, not an error. The train can
    //    legitimately be empty and that must not read as a fault.
    assert_eq!(ci.run(&[]).expect("empty batch"), Vec::<Verdict>::new());

    // 3. A command that succeeds passes.
    assert_eq!(
        ci.run(std::slice::from_ref(&f.passing))
            .expect("passing job"),
        vec![Verdict::Passed]
    );

    // 4. A command that exits nonzero is `Failed` -- about the change --
    //    and is the only verdict permitted to evict.
    let failed = ci
        .run(std::slice::from_ref(&f.failing))
        .expect("failing job");
    assert!(
        matches!(failed[0], Verdict::Failed { .. }),
        "a nonzero exit must be Failed, got {:?}",
        failed[0]
    );
    assert!(failed[0].evicts(), "a test failure must be able to evict");
    assert!(failed[0].is_conclusive());

    // 5. A provider that cannot start the job is `Errored` -- about us --
    //    and must NOT evict. This is the distinction the whole seam
    //    exists for, so it is asserted rather than assumed.
    let errored = ci
        .run(std::slice::from_ref(&f.erroring))
        .expect("erroring job");
    assert!(
        matches!(errored[0], Verdict::Errored { .. }),
        "a provider fault must be Errored, not a verdict about the change, got {:?}",
        errored[0]
    );
    assert!(
        !errored[0].evicts(),
        "a provider fault must never evict a change"
    );
    assert!(!errored[0].is_conclusive());

    // 6. A job past its deadline is `TimedOut`, and also does not evict:
    //    a job can time out because the executor was oversubscribed.
    let timed = ci.run(std::slice::from_ref(&f.slow)).expect("slow job");
    assert_eq!(
        timed[0],
        Verdict::TimedOut,
        "a job past its deadline must be TimedOut, got {:?}",
        timed[0]
    );
    assert!(!timed[0].evicts(), "a timeout must never evict a change");

    // 7. Index alignment. A provider that reorders or drops has
    //    attributed one change's result to another, and the batch call
    //    is worthless without this.
    let batch = vec![f.passing.clone(), f.failing.clone(), f.passing];
    let out = ci.run(&batch).expect("mixed batch");
    assert_eq!(out.len(), batch.len(), "one verdict per job");
    assert_eq!(out[0], Verdict::Passed);
    assert!(matches!(out[1], Verdict::Failed { .. }));
    assert_eq!(out[2], Verdict::Passed);
}

fn shell(subject: &str, script: &str) -> Job {
    Job::new(
        tree(subject),
        vec!["/bin/sh".into(), "-c".into(), script.into()],
    )
}

#[test]
fn the_local_runner_conforms() {
    let mut slow = shell("slow", "sleep 30");
    slow.deadline = Duration::from_millis(100);
    conformance(
        &mut LocalRunner::new(),
        Fixtures {
            passing: shell("pass", "exit 0"),
            failing: shell("fail", "exit 3"),
            // A path that cannot exist, so the failure is at spawn.
            erroring: Job::new(
                tree("error"),
                vec!["/nonexistent/choir-not-a-command".into()],
            ),
            slow,
        },
    );
}

#[test]
fn the_synthetic_executor_conforms() {
    let answer = |job: &Job| match job.label.as_str() {
        "fail" => Verdict::Failed { exit_code: Some(3) },
        "error" => Verdict::Errored {
            provider: "synthetic".into(),
            detail: "asked to fail".into(),
        },
        "slow" => Verdict::TimedOut,
        _ => Verdict::Passed,
    };
    let labelled = |name: &str| {
        let mut j = Job::new(tree(name), vec!["irrelevant".into()]);
        j.label = name.to_string();
        j
    };
    conformance(
        &mut Synthetic::new(answer),
        Fixtures {
            passing: labelled("pass"),
            failing: labelled("fail"),
            erroring: labelled("error"),
            slow: labelled("slow"),
        },
    );
}

/// The child sees exactly `Job.environment` and nothing this process
/// inherited. Local-runner-specific: a synthetic executor has no child
/// to inspect.
///
/// This is the property that makes a job's result reproducible, and it
/// is asserted by having the child fail unless the environment is
/// exactly right.
///
/// `HOME` is the probe rather than a variable this test sets, because
/// this module shares a process with every other one in the harness and
/// `set_var` would be a process-global mutation. `HOME` is set for the
/// test runner and is not one a shell synthesizes when absent, unlike
/// `PATH`.
#[test]
fn the_child_sees_only_the_job_environment() {
    assert!(
        std::env::var_os("HOME").is_some(),
        "this test probes leakage with HOME, which the runner does not have"
    );
    let mut job = shell("env", "[ -z \"$HOME\" ] && [ \"$WANTED\" = yes ]");
    job.environment.insert("WANTED".into(), "yes".into());
    let out = LocalRunner::new().run(&[job]).expect("env job");
    assert_eq!(
        out[0],
        Verdict::Passed,
        "the child saw an inherited variable, or did not see its own: {:?}",
        out[0]
    );
}

/// Verdicts come back in job order even when the provider runs them
/// concurrently, which a serial implementation could pass by accident --
/// so the jobs are given deliberately inverted durations.
#[test]
fn concurrency_does_not_reorder_verdicts() {
    let jobs = vec![
        shell("a", "sleep 0.3; exit 0"),
        shell("b", "exit 4"),
        shell("c", "sleep 0.1; exit 0"),
    ];
    let out = LocalRunner::new().run(&jobs).expect("concurrent batch");
    assert_eq!(out[0], Verdict::Passed);
    assert_eq!(out[1], Verdict::Failed { exit_code: Some(4) });
    assert_eq!(out[2], Verdict::Passed);
}

/// A job with no command is refused, loudly, as a provider fault.
///
/// The default [`choir_queue::JobTemplate`] has no command, so this is
/// the verdict a queue gets when nobody configured what "test" means.
/// It must not read as a change that failed.
#[test]
fn a_job_with_no_command_errors_rather_than_failing() {
    let out = LocalRunner::new()
        .run(&[Job::new(tree("empty"), Vec::new())])
        .expect("empty command");
    assert!(
        matches!(out[0], Verdict::Errored { .. }),
        "an unconfigured job must be a provider fault, got {:?}",
        out[0]
    );
    assert!(!out[0].evicts());
}

/// `ExecutorError` is the whole-call failure, distinct from a per-job
/// `Errored`, and it must say which kind it was.
#[test]
fn the_two_executor_errors_read_differently() {
    assert!(ExecutorError::Unavailable("no route".into())
        .to_string()
        .contains("unavailable"));
    assert!(ExecutorError::Protocol("version 9".into())
        .to_string()
        .contains("protocol"));
}

/// The batch really runs concurrently, proved without asserting on any
/// duration.
///
/// Two jobs each wait for the other's marker file. Run concurrently both
/// finish; run one at a time the first blocks forever on a marker the
/// second has not been started to write, and the deadline turns it into
/// `TimedOut`. So the verdicts alone distinguish the two, and this
/// module can live in the shared harness where wall-clock assertions are
/// banned.
///
/// This is the test that fails if the `collect` in `LocalRunner::run` is
/// removed -- a change clippy actively suggests, and one no ordering
/// assertion can detect.
#[test]
fn the_runner_runs_a_batch_concurrently() {
    let dir = std::env::temp_dir().join("choir-queue-executor-rendezvous");
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).expect("rendezvous dir");
    let wait_for = |mine: &str, theirs: &str| {
        let mine = dir.join(mine);
        let theirs = dir.join(theirs);
        let mut job = shell(
            mine.to_str().unwrap(),
            &format!(
                "touch {m}; while [ ! -e {t} ]; do sleep 0.02; done",
                m = mine.display(),
                t = theirs.display()
            ),
        );
        job.deadline = Duration::from_secs(20);
        job
    };
    let out = LocalRunner::new()
        .run(&[wait_for("a", "b"), wait_for("b", "a")])
        .expect("rendezvous batch");
    std::fs::remove_dir_all(&dir).ok();

    assert_eq!(
        out,
        vec![Verdict::Passed, Verdict::Passed],
        "the two jobs never saw each other, so the batch ran serially"
    );
}

/// The cache key must cover what determines the output and nothing
/// else. Both halves of that are load-bearing and neither was asserted
/// until this test: a key that moves too readily is a cache that never
/// hits, and a key that moves too little is a cache that serves one
/// job's artifacts for another's question.
#[test]
fn the_cache_key_covers_the_work_and_not_the_bookkeeping() {
    let tree = ContentHash::blake3(b"tree");
    let base = Job::new(tree, vec!["cargo".into(), "test".into()]);

    // Bookkeeping: none of these change what the command computes.
    let mut relabelled = base.clone();
    relabelled.label = "change 41".into();
    let mut other_label = base.clone();
    other_label.label = "change 42".into();
    assert_eq!(
        relabelled.cache_key(),
        other_label.cache_key(),
        "the same work for two changes must be one cache entry, or the \
         cache never hits on the case it exists for"
    );

    let mut hurried = base.clone();
    hurried.deadline = Duration::from_secs(1);
    let mut trusted = base.clone();
    trusted.may_write_cache = true;
    assert_eq!(base.cache_key(), hurried.cache_key());
    assert_eq!(base.cache_key(), trusted.cache_key());

    // The work itself: each of these is a different question.
    let mut elsewhere = base.clone();
    elsewhere.subject = ContentHash::blake3(b"another tree");
    let mut other_command = base.clone();
    other_command.command = vec!["cargo".into(), "bench".into()];
    let mut with_env = base.clone();
    with_env.environment.insert("RUSTFLAGS".into(), "-O".into());
    for (what, job) in [
        ("subject", elsewhere),
        ("command", other_command),
        ("environment", with_env),
    ] {
        assert_ne!(
            base.cache_key(),
            job.cache_key(),
            "{what} does not reach the cache key, so two different \
             questions share one answer"
        );
    }
}

/// Where one field ends and the next begins is part of the key.
///
/// A NUL written *between* arguments rather than a length written
/// before each one makes `["a", "b"]` and `["a\0b"]` the same preimage,
/// and a Rust `String` may contain a NUL. That collision is
/// first-to-cache-wins poisoning (CVE-2025-36852's class) reachable by
/// anyone who can choose an argument -- inside the very function whose
/// `may_write_cache` sibling exists to bound it. It was live until this
/// test was written.
#[test]
fn an_argument_boundary_cannot_be_forged_with_a_separator() {
    let tree = ContentHash::blake3(b"tree");
    let two_args = Job::new(tree.clone(), vec!["a".into(), "b".into()]);
    let one_smuggled = Job::new(tree.clone(), vec!["a\0b".into()]);
    assert_ne!(
        two_args.cache_key(),
        one_smuggled.cache_key(),
        "an argument containing a NUL forged another command's cache key"
    );

    // The same forgery through the environment: one pair {ab: c} must
    // not key the same as {a: bc} for any separator choice.
    let mut split = Job::new(tree.clone(), vec![]);
    split.environment.insert("a".into(), "bc".into());
    let mut joined = Job::new(tree.clone(), vec![]);
    joined.environment.insert("ab".into(), "c".into());
    assert_ne!(split.cache_key(), joined.cache_key());

    // And the argv/environment boundary itself.
    let mut arg_side = Job::new(tree.clone(), vec!["x".into()]);
    arg_side.environment.clear();
    let mut env_side = Job::new(tree, vec![]);
    env_side.environment.insert("x".into(), String::new());
    assert_ne!(arg_side.cache_key(), env_side.cache_key());
}
