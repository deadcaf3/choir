//! Conformance for the D5 speculation seam, plus the two things that
//! are true of only one backend.
//!
//! The suite is the private `conformance` function; each backend calls
//! it from its own `#[test]`, so adding a third speculator means adding
//! a call rather than a suite. What it can assert is bounded by what
//! the queue itself knows: a state is an opaque string, so nothing here
//! may look inside one. Every assertion is therefore about *relations*
//! between states — advanced or not, same or different — which is
//! exactly the set of questions [`MergeQueue`] asks.

use std::path::{Path, PathBuf};
use std::process::Command;

use choir_queue::executor::{Synthetic, Verdict};
use choir_queue::git::GitSpeculator;
use choir_queue::speculate::{Speculator, Step, TextSpeculator};
use choir_queue::{Change, MergeQueue};

/// The three lines every fixture edits, so the two backends differ in
/// how a state is spelled and in nothing else.
const BASE: &str = "alpha\nbeta\ngamma\n";
const FIRST: &str = "ALPHA\nbeta\ngamma\n";
const SECOND: &str = "alpha\nbeta\nGAMMA\n";
const RIVAL: &str = "first-line\nbeta\ngamma\n";
const REBASED: &str = "ALPHA\nbeta\nGAMMA\n";

/// What a backend must supply to be asked the shared questions.
struct Fixtures {
    /// The state the changes are authored against.
    base: String,
    /// Edits the first line.
    first: Change,
    /// Edits the last line; independent of `first`.
    second: Change,
    /// Edits the first line differently; conflicts with `first`.
    rival: Change,
    /// `first`'s edit, authored on top of `second`. The same logical
    /// change at a different position, which is the whole point of
    /// [`Speculator::identity`].
    rebased: Change,
}

fn change(id: u64, base: &str, proposed: &str) -> Change {
    Change {
        id,
        workspace: format!("agent-{id}"),
        base: base.to_string(),
        proposed: proposed.to_string(),
        depends: Vec::new(),
    }
}

fn conformance(s: &mut dyn Speculator, f: &Fixtures) {
    let name = s.name();

    // 1. A change advances the state it is put on.
    let Step::Advanced(one) = s.step(&f.first.base, &f.base, &f.first.proposed) else {
        panic!("{name}: an independent change must advance the state");
    };
    assert_ne!(one, f.base, "{name}: advancing must produce a new state");

    // 2. An independent change advances the advanced state. This is
    //    the train: the second change is merged onto the speculative
    //    future the first produced, not onto the base.
    let Step::Advanced(two) = s.step(&f.second.base, &one, &f.second.proposed) else {
        panic!("{name}: a change independent of the train must advance it");
    };
    assert_ne!(two, one, "{name}: the second change must move the state");

    // 3. Distinct states are named distinctly and each is named
    //    consistently. A subject that varied per call would report two
    //    checks about one tree; one that collided would report a
    //    verdict about the wrong tree.
    assert_eq!(
        s.subject(&one),
        s.subject(&one),
        "{name}: subject is stable"
    );
    assert_ne!(
        s.subject(&one),
        s.subject(&two),
        "{name}: distinct states need distinct subjects"
    );

    // 4. An overlapping change is a conflict, never a silent pick (D6).
    assert!(
        matches!(
            s.step(&f.rival.base, &one, &f.rival.proposed),
            Step::Conflict
        ),
        "{name}: an overlapping edit must come back as a conflict"
    );

    // 5. A conflict leaves the speculator usable. The queue evicts the
    //    conflicting change and keeps going with the rest of the train,
    //    so a backend that needs cleaning up after a conflict must do
    //    it itself rather than leaving the next change to fail in its
    //    debris.
    assert!(
        matches!(
            s.step(&f.second.base, &one, &f.second.proposed),
            Step::Advanced(_)
        ),
        "{name}: a conflict must not poison the next merge"
    );

    // 6. The same logical edit has one identity wherever it sits, and
    //    two different edits do not share one. The first half is what
    //    lets the queue refuse a resubmission the train rebased under;
    //    the second is what stops it refusing everything.
    assert_eq!(
        s.identity(&f.first),
        s.identity(&f.rebased),
        "{name}: identity must survive a rebase"
    );
    assert_ne!(
        s.identity(&f.first),
        s.identity(&f.second),
        "{name}: different changes must not share an identity"
    );
    assert_eq!(
        s.identity(&f.first),
        s.identity(&f.first),
        "{name}: identity is deterministic"
    );
}

#[test]
fn the_text_speculator_conforms() {
    let f = Fixtures {
        base: BASE.to_string(),
        first: change(1, BASE, FIRST),
        second: change(2, BASE, SECOND),
        rival: change(3, BASE, RIVAL),
        rebased: change(4, SECOND, REBASED),
    };
    conformance(&mut TextSpeculator::default(), &f);
}

#[test]
fn the_git_speculator_conforms() {
    let repo = fixture_repo("conformance");
    let f = git_fixtures(&repo);
    conformance(&mut GitSpeculator::new(repo.clone()), &f);
    let _ = std::fs::remove_dir_all(&repo);
}

/// Runs git in `dir`, panicking with its stderr.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-c")
        .arg("user.name=fixture")
        .arg("-c")
        .arg("user.email=fixture@choir.invalid")
        .arg("-c")
        .arg("commit.gpgsign=false")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A fresh repository with one commit holding [`BASE`].
///
/// Named by tag and pid: the harness runs these modules on threads of
/// one process, so two fixtures sharing a directory would race.
fn fixture_repo(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("choir-speculate-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the fixture directory is creatable");
    git(&dir, &["init", "-q", "-b", "main"]);
    commit(&dir, BASE, "base");
    dir
}

/// Writes `content` and commits it, returning the new commit id.
fn commit(dir: &Path, content: &str, message: &str) -> String {
    std::fs::write(dir.join("f.txt"), content).expect("the fixture file is writable");
    git(dir, &["add", "f.txt"]);
    git(dir, &["commit", "-q", "-m", message]);
    git(dir, &["rev-parse", "HEAD"])
}

/// The same five states as the text fixtures, spelled as commits.
fn git_fixtures(repo: &Path) -> Fixtures {
    let base = git(repo, &["rev-parse", "HEAD"]);
    let first = commit(repo, FIRST, "first");
    git(repo, &["checkout", "-q", "--detach", &base]);
    let second = commit(repo, SECOND, "second");
    git(repo, &["checkout", "-q", "--detach", &base]);
    let rival = commit(repo, RIVAL, "rival");
    // The rebased twin sits on `second`, so its parent differs and its
    // patch does not.
    git(repo, &["checkout", "-q", "--detach", &second]);
    let rebased = commit(repo, REBASED, "rebased first");
    git(repo, &["checkout", "-q", "--detach", &base]);
    Fixtures {
        base: base.clone(),
        first: change(1, &base, &first),
        second: change(2, &base, &second),
        rival: change(3, &base, &rival),
        rebased: change(4, &second, &rebased),
    }
}

#[test]
fn an_oid_we_do_not_have_is_our_fault_and_not_an_author_s() {
    let repo = fixture_repo("unknown-oid");
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let mut s = GitSpeculator::new(repo.clone());
    // Well-formed and absent, which is the case a conflict check based
    // on git's exit code alone gets wrong: both exit nonzero.
    let missing = "0".repeat(40);
    match s.step(&base, &base, &missing) {
        Step::Unavailable(why) => assert!(
            why.contains(&missing),
            "the reason must name what could not be merged, got {why}"
        ),
        other => panic!("a missing oid must not read as a verdict on anybody: {other:?}"),
    }
    // And the worktree is still usable afterwards.
    let f = git_fixtures(&repo);
    assert!(matches!(
        s.step(&f.first.base, &f.base, &f.first.proposed),
        Step::Advanced(_)
    ));
    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn a_git_state_is_named_by_its_oid_and_nothing_else() {
    let repo = fixture_repo("subject");
    let f = git_fixtures(&repo);
    let s = GitSpeculator::new(repo.clone());
    let subject = s.subject(&f.base);
    assert_eq!(
        subject.git_oid().as_deref(),
        Some(f.base.as_str()),
        "a check written down against anything but the oid is unresolvable to git"
    );
    assert_eq!(
        subject,
        choir_hash::ContentHash::from_git_oid(&f.base).expect("the base is an oid"),
        "including the codec byte: invariant 2 forbids a bare digest"
    );
    assert_ne!(
        subject,
        choir_hash::ContentHash::blake3(f.base.as_bytes()),
        "hashing the oid text would name a tree nobody can fetch"
    );
    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn a_verified_base_is_a_commit_and_an_unverified_one_says_so() {
    let repo = fixture_repo("verify");
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let s = GitSpeculator::new(repo.clone());
    assert_eq!(s.verify("HEAD").as_deref(), Ok(base.as_str()));
    assert_eq!(s.verify(&base).as_deref(), Ok(base.as_str()));
    assert!(
        s.verify("refs/heads/nope").is_err(),
        "a base that does not resolve must be refused before a queue is built on it"
    );
    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn the_queue_lands_real_commits_through_the_git_speculator() {
    let repo = fixture_repo("queue");
    let f = git_fixtures(&repo);
    let mut queue =
        MergeQueue::with_speculator(&f.base, Box::new(GitSpeculator::new(repo.clone())));
    queue.submit(f.first.clone());
    queue.submit(f.second.clone());
    queue.submit(f.rival);
    let mut ci = Synthetic::new(|_| Verdict::Passed);
    let sequencer = choir_sequencer::Sequencer::spawn(Box::new(choir_oplog::MemLog::new()));
    let report = queue.drain(&mut ci, &sequencer);
    sequencer.shutdown();

    assert_eq!(report.merged, vec![1, 2], "both independent changes land");
    assert_eq!(
        report.rejected.len(),
        1,
        "the overlapping change is evicted, not landed: {:?}",
        report.rejected
    );
    assert_eq!(report.provider_error, None);

    // The queue's final state is a commit in that repository, and its
    // tree carries both edits. This is the assertion the single-file
    // model could not make: it is about git, not about a string the
    // queue happened to pass around.
    let tip = report.final_state;
    let content = git(&repo, &["show", &format!("{tip}:f.txt")]);
    assert_eq!(
        content,
        REBASED.trim_end(),
        "both edits are in the landed tree"
    );
    let _ = std::fs::remove_dir_all(&repo);
}

/// A speculator that can never answer, for the queue-level half of the
/// [`Step::Unavailable`] contract.
struct Broken;

impl Speculator for Broken {
    fn name(&self) -> &'static str {
        "broken"
    }
    fn step(&mut self, _base: &str, _onto: &str, _proposed: &str) -> Step {
        Step::Unavailable("the disk went away".to_string())
    }
    fn identity(&self, change: &Change) -> String {
        change.id.to_string()
    }
    fn subject(&self, state: &str) -> choir_hash::ContentHash {
        choir_hash::ContentHash::blake3(state.as_bytes())
    }
}

#[test]
fn a_speculator_that_cannot_merge_evicts_nobody() {
    let mut queue = MergeQueue::with_speculator(BASE, Box::new(Broken));
    let window_before = queue.window();
    queue.submit(change(1, BASE, FIRST));
    queue.submit(change(2, BASE, SECOND));
    let mut ci = Synthetic::new(|_| Verdict::Passed);
    let sequencer = choir_sequencer::Sequencer::spawn(Box::new(choir_oplog::MemLog::new()));
    let report = queue.drain(&mut ci, &sequencer);
    let log = sequencer.shutdown();

    assert!(
        report.merged.is_empty(),
        "nothing was tested, so nothing lands"
    );
    assert!(
        report.rejected.is_empty(),
        "our own broken merge is not a verdict on an author: {:?}",
        report.rejected
    );
    let why = report.provider_error.expect("the stall says why");
    assert!(
        why.contains("broken") && why.contains("the disk went away"),
        "the reason must name the speculator and its complaint, got {why}"
    );
    assert_eq!(report.ci_runs, 0, "no job was built, so none ran");
    assert_eq!(log.len(), 0, "nothing landed, so nothing was sequenced");
    assert_eq!(queue.len(), 2, "both changes are still queued, in order");
    assert_eq!(
        queue.window(),
        window_before,
        "halving answers changes that failed, and none of these did"
    );
}
