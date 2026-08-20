//! Signed check reports end to end, and the narrowing that a commit id
//! alone could not provide (D49).
//!
//! Three things are worth proving at daemon level rather than in the
//! fold. A report reaches the view through the ordinary signed-op
//! endpoint, so it inherits the sequencer and needs no surface of its
//! own. A report bound to a ref is narrowed to that repository, so a
//! reader granted one repository does not learn what is failing in
//! another. And a report bound to no ref is node-wide, which is the
//! fail-closed direction and is easy to get backwards.

use choir_identity::{ActorKey, Registry};
use choir_node::{AuthTable, Node, Platform};
use choir_oplog::MemLog;
use choir_view::{CheckStatus, OpKind, ViewOp};

use crate::support::{curl, submit_body};

fn subject() -> choir_hash::ContentHash {
    choir_hash::ContentHash::from_git_oid("1111111111111111111111111111111111111111")
        .expect("a valid git oid")
}

fn report(name: &str, status: CheckStatus, target_ref: Option<&str>) -> ViewOp {
    ViewOp::new(OpKind::RecordCheck {
        subject: subject(),
        name: name.into(),
        status,
        evidence: "run-17".into(),
        reporter: "ci/runner".into(),
        target_ref: target_ref.map(str::to_string),
    })
}

/// A signed report lands through `/api/submit` and comes back on
/// `/api/view` with its reporter and evidence intact.
#[test]
fn a_signed_report_lands_and_reads_back() {
    let work = std::env::temp_dir().join("choir-node-checks-roundtrip");
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");

    let author = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&author.public_key_bytes()).expect("key");
    let platform =
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate()).expect("platform");

    let mut node = Node::bind(&work.join("repos"), 0).expect("node binds");
    node.create_repo("agents/demo.git").expect("repo");
    let port = node.port();
    node.enable_platform(platform);
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    let api = format!("http://127.0.0.1:{port}/api");

    let op = report(
        "ci/build",
        CheckStatus::Running,
        Some("agents/demo.git:refs/heads/main"),
    );
    let (code, body) = curl(&[
        "-X",
        "POST",
        "-d",
        &submit_body(&author, "ci/runner", &op),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 200, "{body}");

    let key = format!("{}:ci/build", subject().to_hex());
    let (code, view) = curl(&[&format!("{api}/view")]);
    assert_eq!(code, 200, "{view}");
    assert_eq!(view["checks"][&key]["status"], "Running", "{view}");
    assert_eq!(view["checks"][&key]["reporter"], "ci/runner");
    assert_eq!(view["checks"][&key]["evidence"], "run-17");

    // The later report replaces it rather than accumulating, which is
    // the fold's rule seen from outside.
    let op = report(
        "ci/build",
        CheckStatus::Passed,
        Some("agents/demo.git:refs/heads/main"),
    );
    let (code, body) = curl(&[
        "-X",
        "POST",
        "-d",
        &submit_body(&author, "ci/runner", &op),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 200, "{body}");
    let (_, view) = curl(&[&format!("{api}/view")]);
    assert_eq!(view["checks"][&key]["status"], "Passed", "{view}");
    assert_eq!(
        view["checks"].as_object().expect("checks is a map").len(),
        1,
        "re-reporting grew the map: {view}"
    );

    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}

/// A report bound to a ref is narrowed to that repository, and one bound
/// to nothing needs the node-wide grant.
///
/// The second half is the one that would fail silently if the section
/// were classified `PerRepo` and the fallback forgotten: an unbound
/// check names no repository, so a per-repo reader must not see it.
#[test]
fn reports_narrow_to_the_repository_their_ref_names() {
    let work = std::env::temp_dir().join("choir-node-checks-acl");
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");

    let acl_path = work.join("acl");
    std::fs::write(
        &acl_path,
        "alice  agents/mine   read\n\
         carol  @node         auditor\n\
         carol  *             read\n\
         ci     *             write\n\
         ci     @node         write\n",
    )
    .expect("acl file");

    let author = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&author.public_key_bytes()).expect("key");
    let platform =
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate()).expect("platform");

    let mut table = AuthTable::new();
    table.insert("alice".into(), "a".into());
    table.insert("carol".into(), "c".into());
    table.insert("ci".into(), "z".into());

    let mut node =
        Node::bind_with_auth(&work.join("repos"), 0, Some(table)).expect("node binds free port");
    let port = node.port();
    node.create_repo("agents/mine.git").expect("repo");
    node.create_repo("agents/theirs.git").expect("repo");
    node.watch_acl_file(acl_path).expect("acl loads");
    node.enable_platform(platform);
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    let base = format!("http://127.0.0.1:{port}");

    // Submitted by a credential that may write everywhere, so the ACL
    // on the *write* path is not what this test is measuring. `ci` needs
    // both rows: `*` is a statement about repositories and deliberately
    // never covers `@node`, which is what the unbound report falls to.
    for op in [
        report(
            "mine",
            CheckStatus::Passed,
            Some("agents/mine.git:refs/heads/main"),
        ),
        report(
            "theirs",
            CheckStatus::Failed,
            Some("agents/theirs.git:refs/heads/main"),
        ),
        report("unbound", CheckStatus::Failed, None),
    ] {
        let (code, body) = curl(&[
            "-u",
            "ci:z",
            "-X",
            "POST",
            "-d",
            &submit_body(&author, "ci/runner", &op),
            &format!("{base}/api/submit"),
        ]);
        assert_eq!(code, 200, "{body}");
    }

    let hex = subject().to_hex();
    let (code, view) = curl(&["-u", "alice:a", &format!("{base}/api/view")]);
    assert_eq!(code, 200, "{view}");
    let checks = view["checks"].as_object().expect("checks is a map");
    assert!(
        checks.contains_key(&format!("{hex}:mine")),
        "alice lost the check on her own repository: {view}"
    );
    assert!(
        !checks.contains_key(&format!("{hex}:theirs")),
        "a repository-scoped reader saw another repository's check: {view}"
    );
    assert!(
        !checks.contains_key(&format!("{hex}:unbound")),
        "an unbound check reached a reader with no node-wide grant: {view}"
    );

    // The reader who holds both grants sees all three, which is what
    // makes the two absences above narrowing rather than the rows never
    // existing. Both grants, because they are answering different
    // questions: `*` is every repository and `@node` is the node
    // itself, and a check bound to no ref belongs to the second.
    let (code, view) = curl(&["-u", "carol:c", &format!("{base}/api/view")]);
    assert_eq!(code, 200, "{view}");
    let checks = view["checks"].as_object().expect("checks is a map");
    for name in ["mine", "theirs", "unbound"] {
        assert!(
            checks.contains_key(&format!("{hex}:{name}")),
            "the auditor is missing {name}: {view}"
        );
    }

    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}
