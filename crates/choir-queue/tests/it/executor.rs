//! Conformance for the D18 CI executor seam.
//!
//! The house rule is that a seam is real when a conformance suite plus a
//! second implementation both pass. The trait this replaced had neither,
//! which is how it stayed plausible for a year while being unable to
//! carry a real executor.
//!
//! Four backends run the suite, and they prove different amounts.
//! Read that honestly:
//!
//! - [`LocalRunner`] runs real child processes, so its pass is evidence
//!   about exit codes, deadlines, and start failures.
//! - [`Synthetic`] returns what it is told, so its pass proves only the
//!   *protocol* surface: the handshake, the empty batch, and index
//!   alignment. It cannot corroborate the verdict mapping, and saying so
//!   here is cheaper than someone later reading "3 backends pass" as
//!   more than it is.
//! - `ProtocolRunner` against `choir-ci-local` is `LocalRunner` behind a
//!   pipe, so it proves the two agree across a process boundary. It is
//!   the strongest of the four for that one question and says nothing
//!   the others do not about the rest.
//! - `WorktreeRunner` materializes each job's subject as a git worktree
//!   before running it, so its pass is evidence that provisioning per
//!   job keeps the alignment and the four verdicts intact -- including
//!   the case only it has, a subject that cannot be checked out, which
//!   is `Errored` and evicts nobody.
//!
//! A microVM executor is a fourth helper behind the same protocol, and
//! cannot be built here: Firecracker and Cloud Hypervisor both need
//! KVM, which is Linux-only, so it would be the untested single
//! implementation this suite exists to forbid. The process boundary is
//! the part of it that *can* be gated on this machine, and it is the
//! part that carries the risk -- serialization, alignment, and a far
//! side that stops talking.

use choir_hash::ContentHash;
use choir_queue::conform::{conform, Fixtures, Outcome};
use choir_queue::executor::{CiExecutor, ExecutorError, Job, Synthetic, Verdict, PROTOCOL};
use choir_queue::local::LocalRunner;
use choir_queue::worktree::WorktreeRunner;
use std::time::Duration;

fn tree(name: &str) -> ContentHash {
    ContentHash::blake3(name.as_bytes())
}

/// Every assertion both backends must satisfy, which now live in
/// [`choir_queue::conform`] so that a helper this workspace cannot
/// build -- the microVM driver the seam exists for -- is gated by the
/// same list rather than by a copy of it.
fn conformance(ci: &mut dyn CiExecutor, f: Fixtures) {
    let checks = conform(ci, f);
    let failed: Vec<String> = checks
        .iter()
        .filter(|c| matches!(c.outcome, Outcome::Failed(_)))
        .map(ToString::to_string)
        .collect();
    assert!(failed.is_empty(), "{}", failed.join("\n"));
    // A backend that skipped everything would otherwise pass silently.
    assert!(
        checks
            .iter()
            .any(|c| matches!(c.outcome, Outcome::Passed) && c.name == "handshake"),
        "the handshake must be established, got {checks:?}"
    );
}

/// A directory holding the marker `probe` looks for, distinct per
/// caller because the three conformance runs share a process.
fn probe_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("choir-ci-directory-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the probe directory");
    std::fs::write(dir.join("choir-directory-marker"), b"here").expect("write the marker");
    dir
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
            in_directory: Some(in_dir(probe_dir("local"))),
        },
    );
}

/// The probe: exits zero only from a directory holding the marker, so
/// the verdict *is* the assertion about where the job ran.
fn in_dir(dir: std::path::PathBuf) -> Job {
    let mut job = shell("directory", "test -e choir-directory-marker");
    job.directory = Some(dir);
    job
}

/// A repository with two commits, each holding a different `marker`.
///
/// The marker is what makes a verdict evidence about *which* tree ran:
/// a job that only exits zero can pass in anybody's checkout.
fn fixture_repo(tag: &str) -> (std::path::PathBuf, String, String) {
    let dir = std::env::temp_dir().join(format!("choir-ci-worktree-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the fixture repository");
    let run = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-c")
            .arg("user.name=fixture")
            .arg("-c")
            .arg("user.email=fixture@choir.invalid")
            .arg("-c")
            .arg("commit.gpgsign=false")
            .args(args)
            .current_dir(&dir)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    run(&["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("marker"), "one").expect("write the first marker");
    run(&["add", "marker"]);
    run(&["commit", "-q", "-m", "one"]);
    let one = run(&["rev-parse", "HEAD"]);
    std::fs::write(dir.join("marker"), "two").expect("write the second marker");
    run(&["commit", "-q", "-am", "two"]);
    let two = run(&["rev-parse", "HEAD"]);
    (dir, one, two)
}

/// A job addressed by a commit id rather than by a name.
fn at_commit(oid: &str, script: &str) -> Job {
    Job::new(
        ContentHash::from_git_oid(oid).expect("the fixture oid is an oid"),
        vec!["/bin/sh".into(), "-c".into(), script.into()],
    )
}

#[test]
fn the_worktree_runner_conforms() {
    let (repo, one, _two) = fixture_repo("conformance");
    let root = std::env::temp_dir().join(format!(
        "choir-ci-checkouts-conformance-{}",
        std::process::id()
    ));
    let mut slow = at_commit(&one, "sleep 30");
    slow.deadline = Duration::from_millis(100);
    conformance(
        &mut WorktreeRunner::new(repo.clone(), root.clone()),
        Fixtures {
            passing: at_commit(&one, "exit 0"),
            failing: at_commit(&one, "exit 3"),
            // Well-formed and absent, so the failure is at checkout --
            // which is the fault shape this backend adds and the one it
            // must not report as a verdict about the change.
            erroring: at_commit(&"f".repeat(40), "exit 0"),
            slow,
            in_directory: Some(in_dir(probe_dir("worktree"))),
        },
    );
    let _ = std::fs::remove_dir_all(&repo);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn every_job_in_a_batch_is_tested_against_its_own_tree() {
    // The defect this backend exists to prevent: one checkout shared by
    // a batch tests the last state N times and answers under N
    // different subjects, well-formed and wrong.
    let (repo, one, two) = fixture_repo("per-job");
    let root =
        std::env::temp_dir().join(format!("choir-ci-checkouts-per-job-{}", std::process::id()));
    let mut ci = WorktreeRunner::new(repo.clone(), root.clone());
    let verdicts = ci
        .run(&[
            at_commit(&one, "test \"$(cat marker)\" = one"),
            at_commit(&two, "test \"$(cat marker)\" = two"),
        ])
        .expect("the batch runs");
    assert_eq!(
        verdicts,
        vec![Verdict::Passed, Verdict::Passed],
        "each job must see the tree its own subject names"
    );

    // And the checkouts do not accumulate: a bridge polling a forge
    // runs this every minute forever.
    let left: Vec<String> = std::fs::read_dir(&root)
        .map(|d| {
            d.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        left.is_empty(),
        "every checkout is removed when its batch finishes, left: {left:?}"
    );
    let _ = std::fs::remove_dir_all(&repo);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_checkout_a_crashed_run_left_behind_does_not_poison_the_next() {
    // `worktree add` refuses a path that exists, so debris from a run
    // that died between the checkout and the cleanup would fault every
    // later batch at provisioning -- reported as an outage forever,
    // until somebody cleaned up by hand.
    let (repo, one, _two) = fixture_repo("debris");
    let root =
        std::env::temp_dir().join(format!("choir-ci-checkouts-debris-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    // The name is the contract between `provision` and this test: a
    // batch of one puts its checkout in slot 0.
    let stale = root.join(format!("{one}-0"));
    std::fs::create_dir_all(&stale).expect("create the debris");
    std::fs::write(stale.join("half-written"), b"from a run that died").expect("write the debris");

    let mut ci = WorktreeRunner::new(repo.clone(), root.clone());
    assert_eq!(
        ci.run(std::slice::from_ref(&at_commit(&one, "test -e marker")))
            .expect("the batch runs"),
        vec![Verdict::Passed],
        "the debris must be cleared, not reported as an outage"
    );
    let _ = std::fs::remove_dir_all(&repo);
    let _ = std::fs::remove_dir_all(&root);
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
            // A synthetic executor runs nothing, so there is no
            // directory for it to honor and nothing to learn from
            // asking. Skipping is honest; a fixture answering from the
            // label table would be the suite testing itself.
            in_directory: None,
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
    let mut env_side = Job::new(tree.clone(), vec![]);
    env_side.environment.insert("x".into(), String::new());
    assert_ne!(arg_side.cache_key(), env_side.cache_key());

    // The working directory is observable to the command, so it is in
    // the key. Three cases, because the interesting one is the third:
    // "no directory" must not collide with "the empty directory", which
    // is what a plain length-prefixed field would have done.
    let mut here = Job::new(tree.clone(), vec!["build".into()]);
    here.directory = Some(std::path::PathBuf::from("/a"));
    let mut there = Job::new(tree.clone(), vec!["build".into()]);
    there.directory = Some(std::path::PathBuf::from("/b"));
    let anywhere = Job::new(tree.clone(), vec!["build".into()]);
    let mut empty = Job::new(tree, vec!["build".into()]);
    empty.directory = Some(std::path::PathBuf::new());
    assert_ne!(
        here.cache_key(),
        there.cache_key(),
        "two checkouts of the same tree at different paths share a cache key"
    );
    assert_ne!(
        anywhere.cache_key(),
        empty.cache_key(),
        "`None` and an empty path are the same preimage"
    );
}

/// The third backend, and the one that carries the seam somewhere the
/// other two cannot go: the same `LocalRunner`, behind a pipe.
///
/// Every assertion in `conformance` is about the executor's answers,
/// and none of them mentions processes, so running them through
/// `choir-ci-local` compares in-process execution against
/// out-of-process execution directly. A difference is a defect in the
/// wire format, not a story about how pipes are different.
#[test]
fn the_protocol_runner_conforms() {
    let mut slow = shell("slow", "sleep 30");
    slow.deadline = Duration::from_millis(100);
    conformance(
        &mut helper(),
        Fixtures {
            passing: shell("pass", "exit 0"),
            failing: shell("fail", "exit 3"),
            erroring: Job::new(
                tree("error"),
                vec!["/nonexistent/choir-not-a-command".into()],
            ),
            slow,
            in_directory: Some(in_dir(probe_dir("remote"))),
        },
    );
}

/// A runner pointed at the reference helper.
fn helper() -> choir_queue::remote::ProtocolRunner {
    choir_queue::remote::ProtocolRunner::new(vec![env!("CARGO_BIN_EXE_choir-ci-local").to_string()])
}

/// One `sh -c` helper that writes canned lines and exits, for the
/// protocol failures a well-behaved helper never produces.
/// The protocol number is pinned to a literal, which is the one place
/// in this suite where that is right rather than a change-detector.
///
/// Every other assertion computes from the constant, so moving it is
/// invisible: a mutation setting it back to 1 changed nothing any test
/// could see. But the number is the whole compatibility claim -- it is
/// what a helper somewhere else in the world has compiled in, and the
/// handshake refuses on it. Renumbering silently is the failure this
/// catches. When the wire format really changes, bump both.
#[test]
fn the_protocol_number_is_what_helpers_were_built_against() {
    assert_eq!(
        PROTOCOL, 2,
        "the protocol number moved; a helper built against the old one \
         will now be refused, so change this only when the wire format \
         changed and say so in the D18 row"
    );
}

/// A well-formed greeting at whatever `PROTOCOL` is today, for the
/// canned helpers below. Computed rather than pasted, because a stale
/// literal turns each of those into a handshake-refusal test that still
/// passes under its own name.
fn hello(name: &str) -> String {
    format!(r#"{{"name":"{name}","protocol":{PROTOCOL}}}"#)
}

fn canned(script: &str) -> choir_queue::remote::ProtocolRunner {
    choir_queue::remote::ProtocolRunner::new(vec![
        "/bin/sh".into(),
        "-c".into(),
        // Read and discard the host's lines so it is never the writer
        // that fails first, which would test the wrong thing.
        format!("{script}; cat >/dev/null"),
    ])
}

/// Everything a job carries has to survive the wire, or a remote
/// executor silently tests something other than what was asked and
/// returns an aligned, believed verdict about it.
#[test]
fn a_job_survives_the_round_trip_intact() {
    let mut job = Job::new(tree("round trip"), vec!["cargo".into(), "test".into()]);
    job.label = "change 7".into();
    job.environment.insert("RUSTFLAGS".into(), "-O".into());
    job.environment.insert("EMPTY".into(), String::new());
    job.directory = Some(std::path::PathBuf::from("/tmp/choir-train"));
    job.deadline = Duration::from_millis(1234);
    job.may_write_cache = true;

    let wire = choir_queue::remote::job_to_json(&job).expect("the job serializes");
    let back = choir_queue::remote::job_from_json(&wire).expect("the line parses");
    assert_eq!(back, job, "a field was lost or changed crossing the wire");

    // And the cache key survives it, which is the property that decides
    // whether a remote cache hits on work this process already did.
    assert_eq!(back.cache_key(), job.cache_key());
}

/// Same for verdicts, including the payloads that make two of them
/// distinguishable at all.
#[test]
fn every_verdict_survives_the_round_trip() {
    for verdict in [
        Verdict::Passed,
        Verdict::Failed { exit_code: Some(3) },
        Verdict::Failed { exit_code: None },
        Verdict::Errored {
            provider: "firecracker".into(),
            detail: "the vm did not boot".into(),
        },
        Verdict::TimedOut,
    ] {
        let wire = choir_queue::remote::verdict_to_json(&verdict);
        let back = choir_queue::remote::verdict_from_json(&wire).expect("the line parses");
        assert_eq!(back, verdict);
        assert_eq!(
            back.evicts(),
            verdict.evicts(),
            "the wire changed whether this verdict may eject a change"
        );
    }
}

/// A verdict this build does not understand is refused, never coerced.
///
/// Both neighbours are wrong in a way that matters: `Passed` lands
/// untested work, and `Failed` ejects a change -- permanently, with its
/// dependents -- on account of our own inability to read a line.
#[test]
fn an_unknown_verdict_is_refused_rather_than_guessed() {
    let unknown = serde_json::json!({ "verdict": "probably_fine" });
    let why = choir_queue::remote::verdict_from_json(&unknown).expect_err("must not parse");
    assert!(why.contains("probably_fine"), "unhelpful refusal: {why}");

    let mut ci = canned(&format!(
        r#"printf '{}\n{{"verdict":"probably_fine}}\n'"#,
        hello("x")
    ));
    let out = ci.run(&[shell("pass", "exit 0")]);
    assert!(
        matches!(out, Err(ExecutorError::Protocol(_))),
        "an unreadable verdict was not a protocol error: {out:?}"
    );
}

/// A helper speaking another version is refused at the handshake,
/// before it is handed a batch it would answer wrongly.
#[test]
fn a_version_mismatch_is_refused_at_the_handshake() {
    let mut ci = canned(r#"printf '{"name":"future","protocol":99}\n'"#);
    match ci.info() {
        Err(ExecutorError::Protocol(why)) => {
            assert!(
                why.contains("99"),
                "the refusal must name the version: {why}"
            );
        }
        other => panic!("a version mismatch was not refused: {other:?}"),
    }
}

/// A helper that never speaks is unavailable, not a protocol fault.
///
/// The distinction is the one the queue acts on: both stall the train,
/// but only one of them is worth reporting as our outage rather than
/// as a build we cannot talk to.
#[test]
fn a_silent_helper_is_unavailable() {
    let mut ci = canned("exit 0");
    assert!(
        matches!(ci.info(), Err(ExecutorError::Unavailable(_))),
        "a helper that said nothing was not reported as unavailable"
    );

    let mut missing =
        choir_queue::remote::ProtocolRunner::new(vec!["/nonexistent/choir-helper".into()]);
    assert!(matches!(missing.info(), Err(ExecutorError::Unavailable(_))));
}

/// The failure only a process boundary has: the far side dies with the
/// batch half answered.
///
/// The answers it did give are about the jobs it gave them for, and
/// throwing them away would waste real work -- so the tail is filled
/// with `Errored`, which keeps the indices aligned and is true. The
/// alternative, a short list, is refused wholesale by the queue.
#[test]
fn a_helper_that_dies_mid_batch_errors_only_the_jobs_it_left() {
    let mut ci = canned(&format!(
        r#"printf '{}\n{{"verdict":"passed"}}\n'"#,
        hello("flaky")
    ));
    let jobs = [
        shell("a", "exit 0"),
        shell("b", "exit 0"),
        shell("c", "exit 0"),
    ];
    let out = ci.run(&jobs).expect("a short answer is not an error");

    assert_eq!(out.len(), jobs.len(), "the batch lost its alignment");
    assert_eq!(out[0], Verdict::Passed, "a real answer was discarded");
    for verdict in &out[1..] {
        assert!(
            matches!(verdict, Verdict::Errored { .. }),
            "an unanswered job got a verdict about the commit: {verdict:?}"
        );
        assert!(!verdict.evicts(), "a dead helper ejected a change");
    }
}

/// The whole point, restated as a queue outcome: a helper that dies
/// halfway lands the work it approved and ejects nobody.
#[test]
fn a_half_dead_helper_lands_the_prefix_and_rejects_nothing() {
    let mut ci = canned(&format!(
        r#"printf '{}\n{{"verdict":"passed"}}\n'"#,
        hello("flaky")
    ));
    // 30 lines of base so three changes edit disjoint regions and the
    // merge itself is never what stops the train.
    let base: String = (0..30).map(|i| format!("line {i}\n")).collect();
    let changes: Vec<choir_queue::Change> = (0..3)
        .map(|id: u64| {
            let mut lines: Vec<String> = base.lines().map(String::from).collect();
            lines[(3 * id) as usize] = format!("edited by {id}");
            choir_queue::Change {
                id,
                workspace: format!("ws-{id}"),
                base: base.clone(),
                proposed: lines.join("\n") + "\n",
                depends: vec![],
            }
        })
        .collect();
    let (report, _) = choir_queue::run_batch(&base, changes, &mut ci);

    assert_eq!(report.merged, vec![0], "the approved change did not land");
    assert!(
        report.rejected.is_empty(),
        "a dead helper rejected work: {:?}",
        report.rejected
    );
    assert!(report.provider_error.is_some(), "the stall said nothing");
}
