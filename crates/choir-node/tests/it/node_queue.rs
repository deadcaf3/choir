//! The node's own merge queue (D68), end to end.
//!
//! Two proposals are pushed with nothing but `git`, the round is
//! drained through the node's real sequencer, and the branch moves
//! because an op said so. That is the whole claim: on a node the
//! landings are real, they are in the log, and `View::materialize`
//! folds them.

use choir_identity::{ActorKey, Registry};
use choir_node::{Node, Platform};
use choir_oplog::MemLog;
use choir_queue::executor::{Job, Synthetic, Verdict};

fn git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git runs")
}

struct Fixture {
    work: std::path::PathBuf,
    node: std::sync::Arc<Node>,
    port: u16,
    clone: std::path::PathBuf,
    /// A worktree of the served bare repo, which is what the queue must
    /// speculate in so its merge commits land in the node's own object
    /// store.
    workdir: std::path::PathBuf,
}

const REPO: &str = "agents/demo.git";

fn fixture(tag: &str) -> Fixture {
    let work = std::env::temp_dir().join(format!("choir-node-queue-{tag}-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();

    let root = work.join("repos");
    let mut node = Node::bind(&root, 0).unwrap();
    node.enable_platform(
        Platform::start(
            Registry::new(),
            Box::new(MemLog::new()),
            ActorKey::generate(),
        )
        .unwrap(),
    );
    let port = node.port();
    node.create_repo(REPO).unwrap();
    let node = std::sync::Arc::new(node);
    let serving = std::sync::Arc::clone(&node);
    std::thread::spawn(move || serving.serve_forever());

    let url = format!("http://127.0.0.1:{port}/{REPO}");
    let clone = work.join("clone");
    assert!(git(&work, &["clone", "-q", &url, clone.to_str().unwrap()])
        .status
        .success());
    std::fs::write(clone.join("base.txt"), "v1\n").unwrap();
    git(&clone, &["add", "."]);
    git(&clone, &["commit", "-q", "-m", "first"]);
    assert!(git(&clone, &["push", "-q", "origin", "HEAD:main"])
        .status
        .success());

    // The queue's tree is a worktree of the bare repo, not a clone of
    // it: a clone's merge commits would be invisible to the repository
    // the landing names them in.
    let workdir = work.join("queue-tree");
    let bare = root.join(REPO);
    let added = git(
        &bare,
        &[
            "worktree",
            "add",
            "--detach",
            workdir.to_str().unwrap(),
            "main",
        ],
    );
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );

    Fixture {
        work,
        node,
        port,
        clone,
        workdir,
    }
}

/// Pushes a proposal touching only `file`, so two of them merge cleanly.
fn propose(f: &Fixture, topic: &str, file: &str, body: &str) -> String {
    propose_to(f, "main", topic, file, body)
}

/// The same, aimed at `branch`.
fn propose_to(f: &Fixture, branch: &str, topic: &str, file: &str, body: &str) -> String {
    git(&f.clone, &["checkout", "-q", "main"]);
    std::fs::write(f.clone.join(file), body).unwrap();
    git(&f.clone, &["add", "."]);
    git(&f.clone, &["commit", "-q", "-m", topic]);
    let head = String::from_utf8_lossy(&git(&f.clone, &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();
    let pushed = git(
        &f.clone,
        &[
            "push",
            "-q",
            "origin",
            &format!("HEAD:refs/for/{branch}/alice/{topic}"),
        ],
    );
    assert!(
        pushed.status.success(),
        "{}",
        String::from_utf8_lossy(&pushed.stderr)
    );
    head
}

fn view(port: u16) -> serde_json::Value {
    let out = std::process::Command::new("curl")
        .args(["-s", &format!("http://127.0.0.1:{port}/api/view")])
        .output()
        .expect("curl runs");
    serde_json::from_slice(&out.stdout).expect("view json")
}

fn always_green() -> Synthetic {
    Synthetic::new(|_: &Job| Verdict::Passed)
}

fn template() -> choir_queue::JobTemplate {
    choir_queue::JobTemplate {
        command: vec!["/bin/sh".into(), "-c".into(), "true".into()],
        ..Default::default()
    }
}

/// The round reads what git wrote: two proposals, in refname order,
/// against the branch they were aimed at.
#[test]
fn a_round_is_the_proposals_in_the_view() {
    let f = fixture("round");
    let one = propose(&f, "one", "a.txt", "a\n");
    let two = propose(&f, "two", "b.txt", "b\n");
    let platform = f.node.platform().expect("the platform is enabled");

    let round = platform
        .proposal_round(REPO, "main")
        .expect("main exists, so there is a round");
    assert_eq!(
        round
            .proposals
            .iter()
            .map(|p| (p.id, p.refname.as_str(), p.head.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (1, "agents/demo.git:refs/for/main/alice/one", one.as_str()),
            (2, "agents/demo.git:refs/for/main/alice/two", two.as_str()),
        ],
        "the round is not the two proposals in refname order"
    );
    assert_eq!(round.target_ref(), "agents/demo.git:refs/heads/main");
    // Every change is aimed at the branch as it stands, not at whatever
    // the proposer happened to be on.
    assert!(round.changes().iter().all(|c| c.base == round.base));

    // A proposal aimed somewhere else is not in this round. Without
    // this the branch segment of `refs/for/<branch>/...` could be
    // ignored entirely and every assertion above would still hold,
    // while the queue merged work nobody proposed here.
    assert!(git(&f.clone, &["push", "-q", "origin", "main:release"])
        .status
        .success());
    let elsewhere = propose_to(&f, "release", "three", "c.txt", "c\n");
    let round = platform
        .proposal_round(REPO, "main")
        .expect("still a round");
    assert_eq!(
        round.proposals.len(),
        2,
        "a proposal aimed at another branch was swept into this round: {:?}",
        round.proposals
    );
    let other = platform
        .proposal_round(REPO, "release")
        .expect("release exists too");
    assert_eq!(
        other
            .proposals
            .iter()
            .map(|p| p.head.as_str())
            .collect::<Vec<_>>(),
        vec![elsewhere.as_str()],
        "the other branch's round is not its own proposal"
    );

    // A branch nobody has is not an empty round: it is no round at all.
    assert!(platform.proposal_round(REPO, "nope").is_none());
    std::fs::remove_dir_all(&f.work).ok();
}

/// A green round lands both proposals, the branch moves because an op
/// moved it, and the merged tree really holds both changes.
#[test]
fn a_green_round_moves_the_branch_through_the_log() {
    let f = fixture("green");
    propose(&f, "one", "a.txt", "a\n");
    propose(&f, "two", "b.txt", "b\n");
    let platform = f.node.platform().expect("the platform is enabled");
    let before = platform.proposal_round(REPO, "main").unwrap().base;

    let report = platform
        .run_proposal_queue(REPO, "main", &f.workdir, &mut always_green(), template())
        .expect("the round runs");
    assert_eq!(report.merged, vec![1, 2], "both proposals should land");
    assert_eq!(report.provider_error, None);

    // The branch moved, and it moved in the view -- which is only true
    // if the landing was an op the sequencer accepted.
    let after = platform.proposal_round(REPO, "main").unwrap().base;
    assert_ne!(after, before, "the branch did not move");
    assert_eq!(after, report.final_state, "the log and the round disagree");

    // And the commit it names is a merge holding both proposals, which
    // is the thing an outcome list cannot show.
    let listing = git(&f.workdir, &["ls-tree", "--name-only", "-r", &after]);
    let names = String::from_utf8_lossy(&listing.stdout);
    assert!(names.contains("a.txt"), "proposal one is missing: {names}");
    assert!(names.contains("b.txt"), "proposal two is missing: {names}");

    // The proposal refs survive their own landing: sweeping them would
    // be deciding on the author's behalf that a landed change is done.
    let round = platform.proposal_round(REPO, "main").unwrap();
    assert_eq!(round.proposals.len(), 2, "the proposals were swept");
    std::fs::remove_dir_all(&f.work).ok();
}

/// The landing is CAS'd on the base the round started from, so a push
/// that beat the round makes it stall instead of clobbering.
///
/// Asserted by landing the same round twice from one reading: the
/// second `RefLanding` still carries the original base as its `prev`,
/// which is exactly the state a queue that read the view before
/// somebody else's push is in.
#[test]
fn a_landing_that_lost_the_race_stalls_the_round() {
    let f = fixture("cas");
    propose(&f, "one", "a.txt", "a\n");
    let platform = f.node.platform().expect("the platform is enabled");
    let stale = platform.proposal_round(REPO, "main").unwrap();

    // Round one lands and moves the branch.
    let first = platform
        .run_proposal_queue(REPO, "main", &f.workdir, &mut always_green(), template())
        .expect("the round runs");
    assert_eq!(first.merged, vec![1]);
    let landed = platform.proposal_round(REPO, "main").unwrap().base;

    // Now land against the base the branch had *before* that, which is
    // what a queue holding a stale reading would do.
    let mut queue = choir_queue::MergeQueue::with_speculator(
        &stale.base,
        Box::new(choir_queue::git::GitSpeculator::new(f.workdir.clone())),
    );
    queue.set_job_template(template());
    queue.set_landing(Box::new(
        platform.ref_landing(stale.target_ref(), &stale.base),
    ));
    for change in stale.changes() {
        queue.submit(change);
    }
    let report = platform.drain_queue(&mut queue, &mut always_green());

    assert!(
        report.merged.is_empty(),
        "a landing against a stale base was allowed to move the branch"
    );
    let why = report
        .provider_error
        .expect("the round says why it stopped");
    assert!(
        why.contains("landing refused"),
        "the stall does not name the landing: {why}"
    );
    assert_eq!(
        platform.proposal_round(REPO, "main").unwrap().base,
        landed,
        "the branch moved anyway"
    );
    std::fs::remove_dir_all(&f.work).ok();
}

/// The round's verdicts reach the log as checks the node signed (D49).
///
/// This is the reporter's first production caller. Asserted through the
/// served view rather than through the report, because a reporter that
/// built perfect ops and had every one of them refused would leave a
/// report that looks exactly like this one -- which is what
/// `unreported_checks` exists to say, and why it is checked first.
#[test]
fn the_round_reports_its_checks_under_the_node_key() {
    let f = fixture("checks");
    propose(&f, "one", "a.txt", "a\n");
    propose(&f, "two", "b.txt", "b\n");
    let platform = f.node.platform().expect("the platform is enabled");

    let report = platform
        .run_proposal_queue(REPO, "main", &f.workdir, &mut always_green(), template())
        .expect("the round runs");
    assert_eq!(
        report.unreported_checks,
        Vec::<String>::new(),
        "the node refused its own reports"
    );

    let view = view(f.port);
    let checks = view["checks"]
        .as_object()
        .expect("the view has a checks map");
    // The view flattens checks to `<subject>:<name>`, so the name is
    // read off the key rather than out of the value.
    let ours: Vec<_> = checks
        .iter()
        .filter_map(|(key, check)| {
            let (subject, name) = key.rsplit_once(':')?;
            (name == "ci/queue").then_some((subject, check))
        })
        .collect();
    assert_eq!(
        ours.len(),
        2,
        "one check per change was expected, got: {:?}",
        checks.keys().collect::<Vec<_>>()
    );
    for (subject, check) in ours {
        assert_eq!(check["status"], "Passed", "{subject}: {check}");
        assert_eq!(
            check["target_ref"], "agents/demo.git:refs/heads/main",
            "a check nobody can find the repository of: {subject}"
        );
        assert_eq!(
            check["reporter"], "node/queue",
            "the check was reported under somebody else's name: {check}"
        );
        // The subject is the speculative commit CI ran against, and this
        // repository has it: a check about a tree nobody can resolve is
        // a check nobody can act on. A `ContentHash` is codec-prefixed,
        // and it is the oid after the prefix that git answers about.
        let oid = subject.rsplit('-').next().expect("a codec-prefixed hash");
        let kind = git(&f.workdir, &["cat-file", "-t", oid]);
        assert_eq!(
            String::from_utf8_lossy(&kind.stdout).trim(),
            "commit",
            "the check subject {subject} is not a commit this node holds"
        );
    }
    std::fs::remove_dir_all(&f.work).ok();
}
