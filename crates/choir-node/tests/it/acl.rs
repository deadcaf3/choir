//! Per-repository authorization (D29), driven the way a second person
//! drives it: a real git client against a real ACL file, and edits to
//! that file while the node keeps serving.
//!
//! The grammar has unit tests beside the parser. What only a served node
//! can show is the part that matters — that a credential granted one
//! repository cannot reach another one, that read cannot push, and that
//! the hook callback is not a way around either.

use choir_identity::{ActorKey, Registry};
use choir_node::{AuthTable, Node, Platform};
use choir_oplog::MemLog;
use choir_view::{OpKind, ViewOp};

use crate::support::{curl, submit_body};

fn git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
            "-c",
            "credential.helper=",
            "-c",
            "credential.interactive=false",
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

/// A served node with four credentials, an ACL file, and the repos the
/// test needs. Returns the base URL, the node's key, the work directory
/// and the ACL path, which tests rewrite to exercise the reload.
///
/// `tag` keeps the temp directory distinct: this harness shares one
/// process, so pids no longer separate modules or tests.
fn served(
    tag: &str,
    acl: &str,
    repos: &[&str],
) -> (String, ActorKey, std::path::PathBuf, std::path::PathBuf) {
    let work = std::env::temp_dir().join(format!("choir-node-acl-{tag}"));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");
    let acl_path = work.join("acl");
    std::fs::write(&acl_path, acl).expect("acl file");

    let node_key = ActorKey::generate();
    let platform = Platform::start(
        Registry::new(),
        Box::new(MemLog::new()),
        ActorKey::from_secret_bytes(&node_key.secret_bytes()),
    )
    .expect("platform starts");

    let mut table = AuthTable::new();
    for (user, token) in [("alice", "a"), ("bob", "b"), ("carol", "c"), ("dave", "d")] {
        table.insert(user.into(), token.into());
    }

    let mut node =
        Node::bind_with_auth(&work.join("repos"), 0, Some(table)).expect("node binds free port");
    let port = node.port();
    for repo in repos {
        node.create_repo(repo).expect("repo created");
    }
    node.watch_acl_file(acl_path.clone()).expect("acl loads");
    node.enable_platform(platform);
    std::thread::spawn(move || node.serve_forever());

    (format!("http://127.0.0.1:{port}"), node_key, work, acl_path)
}

/// One request for the browser page, as `(status, headers, body)`.
///
/// `support::curl` parses JSON, and the page deliberately is not JSON;
/// the phase-B tests also need the `ETag`, which that helper drops.
fn page_get(url: &str, args: &[&str]) -> (u16, String, String) {
    let out = std::process::Command::new("curl")
        .args(["-s", "-i", "-w", "\n%{http_code}"])
        .args(args)
        .arg(url)
        .output()
        .expect("curl runs");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let (rest, code) = text.rsplit_once('\n').unwrap_or((text.as_str(), "0"));
    let (headers, body) = match rest.split_once("\r\n\r\n") {
        Some((h, b)) => (h.to_string(), b.to_string()),
        None => (rest.to_string(), String::new()),
    };
    (code.trim().parse().unwrap_or(0), headers, body)
}

/// Rewrites the ACL and forces a distinct mtime, so the reload cannot be
/// missed because two writes landed inside one timestamp tick.
fn rewrite(path: &std::path::Path, text: &str) {
    std::fs::write(path, text).expect("acl rewrite");
    let file = std::fs::File::options()
        .write(true)
        .open(path)
        .expect("reopen acl");
    let ahead = std::time::SystemTime::now() + std::time::Duration::from_secs(1);
    file.set_times(std::fs::FileTimes::new().set_modified(ahead))
        .expect("stamp acl mtime");
}

/// Seeds `repo` with one commit, using a credential that may push, and
/// returns the clone directory.
fn seed(work: &std::path::Path, base: &str, creds: &str, repo: &str) -> std::path::PathBuf {
    let url = format!(
        "http://{creds}@{}/{repo}",
        base.trim_start_matches("http://")
    );
    let dir = work.join("seed");
    assert!(
        git(work, &["clone", "-q", &url, dir.to_str().unwrap()])
            .status
            .success(),
        "seeding clone failed"
    );
    std::fs::write(dir.join("f.txt"), "seed\n").unwrap();
    assert!(git(&dir, &["add", "."]).status.success());
    assert!(git(&dir, &["commit", "-q", "-m", "seed"]).status.success());
    let push = git(&dir, &["push", "-q", "origin", "HEAD:main"]);
    assert!(
        push.status.success(),
        "{}",
        String::from_utf8_lossy(&push.stderr)
    );
    dir
}

/// The whole point, as one matrix: a grant on one repository is not a
/// grant on the other, `read` cannot push, and a credential with no row
/// at all is told nothing exists.
#[test]
fn a_grant_on_one_repository_is_not_a_grant_on_the_node() {
    let (base, _, work, _) = served(
        "matrix",
        "alice  agents/one  write\n\
         bob    agents/one  read\n\
         carol  *           read\n",
        &["agents/one.git", "agents/two.git"],
    );
    let host = base.trim_start_matches("http://").to_string();
    seed(&work, &base, "alice:a", "agents/one.git");

    // Write grant: clone and push both land.
    let alice = format!("http://alice:a@{host}/agents/one.git");
    let dir = work.join("alice");
    assert!(git(&work, &["clone", "-q", &alice, dir.to_str().unwrap()])
        .status
        .success());
    std::fs::write(dir.join("g.txt"), "alice\n").unwrap();
    assert!(git(&dir, &["add", "."]).status.success());
    assert!(git(&dir, &["commit", "-q", "-m", "alice"]).status.success());
    assert!(git(&dir, &["push", "-q", "origin", "HEAD:main"])
        .status
        .success());

    // Read grant: clone lands, push is refused.
    let bob = format!("http://bob:b@{host}/agents/one.git");
    let bobdir = work.join("bob");
    assert!(git(&work, &["clone", "-q", &bob, bobdir.to_str().unwrap()])
        .status
        .success());
    std::fs::write(bobdir.join("h.txt"), "bob\n").unwrap();
    assert!(git(&bobdir, &["add", "."]).status.success());
    assert!(git(&bobdir, &["commit", "-q", "-m", "bob"])
        .status
        .success());
    let refused = git(&bobdir, &["push", "origin", "HEAD:main"]);
    assert!(!refused.status.success(), "a read grant pushed");
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("403"), "not a forbidden: {stderr}");

    // No row at all: the repository does not exist as far as dave is
    // told, and a 404 rather than a 403 is the point.
    let dave = format!("http://dave:d@{host}/agents/one.git");
    let out = git(&work, &["clone", "-q", &dave, "dave"]);
    assert!(!out.status.success(), "an ungranted credential cloned");
    // git renders the 404 as "not found" rather than echoing the status.
    // What must not appear is a 403, which would confirm the name.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not found") || stderr.contains("404"),
        "expected a not-found: {stderr}"
    );
    assert!(
        !stderr.contains("403"),
        "a denial confirmed the repo exists: {stderr}"
    );

    // The wildcard reaches every repository at its level, and no further.
    let carol = format!("http://carol:c@{host}/agents/two.git");
    let out = git(&work, &["clone", "-q", &carol, "carol"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A submitted op is authorized against the repository it names, which
/// it carries in the view's `<repo>:<refname>` ref key. Without this the
/// git-route check is decoration: `/api/submit` moves the same refs.
#[test]
fn a_submitted_ref_op_is_authorized_against_the_repository_it_names() {
    let (base, key, _work, _) = served(
        "submit",
        "alice  agents/one  write\n",
        &["agents/one.git", "agents/two.git"],
    );
    let url = format!("{base}/api/submit");

    let granted = ViewOp::new(OpKind::SetRef {
        name: "agents/one.git:refs/heads/topic".into(),
        commit: choir_oplog::ContentHash::blake3(b"granted"),
        prev: None,
    });
    let (status, body) = curl(&[
        "-u",
        "alice:a",
        "-d",
        &submit_body(&key, "node/test", &granted),
        &url,
    ]);
    assert_eq!(status, 200, "a granted repository was refused: {body}");

    let ungranted = ViewOp::new(OpKind::SetRef {
        name: "agents/two.git:refs/heads/topic".into(),
        commit: choir_oplog::ContentHash::blake3(b"ungranted"),
        prev: None,
    });
    let (status, _) = curl(&[
        "-u",
        "alice:a",
        "-d",
        &submit_body(&key, "node/test", &ungranted),
        &url,
    ]);
    assert_eq!(
        status, 404,
        "a write grant on one repo moved another one's ref"
    );

    // Repository-less ops fail closed. `BindKey` is the sharp case: with
    // no admission rule wired, any key can bind any other, so reaching it
    // must take a deliberate node-wide grant rather than a repo grant.
    let node_scoped = ViewOp::new(OpKind::BindKey {
        operator: "someone".into(),
        key: choir_oplog::ContentHash::blake3(b"k"),
        channel: None,
    });
    let (status, _) = curl(&[
        "-u",
        "alice:a",
        "-d",
        &submit_body(&key, "node/test", &node_scoped),
        &url,
    ]);
    assert_eq!(status, 403, "a repository grant reached a node-scoped op");
}

/// The op log and the ref-state attestation describe the whole node, so
/// they are gated rather than filtered — and the gate is a grant nobody
/// holds by accident.
#[test]
fn the_whole_node_reads_need_the_node_wide_grant() {
    let (base, _, _work, _) = served(
        "auditor",
        "alice  agents/one  write\n\
         carol  @node       auditor\n",
        &["agents/one.git"],
    );

    for path in ["/api/log", "/api/ref-agreement"] {
        let url = format!("{base}{path}");
        let (status, _) = curl(&["-u", "carol:c", &url]);
        assert_eq!(status, 200, "the auditor was refused {path}");
        let (status, _) = curl(&["-u", "alice:a", &url]);
        assert_eq!(status, 403, "{path} was readable without a node-wide grant");
    }
}

/// Granting access is "append a line", and revoking it is "remove one".
/// The second half is the mutation test: a guard checked only against
/// the file this test wrote would pass while enforcing nothing.
#[test]
fn editing_the_file_takes_effect_without_a_restart_in_both_directions() {
    let (base, _, work, acl) = served("reload", "alice  agents/one  write\n", &["agents/one.git"]);
    let host = base.trim_start_matches("http://").to_string();
    seed(&work, &base, "alice:a", "agents/one.git");

    let bob_url = format!("http://bob:b@{host}/agents/one.git");
    let denied = git(&work, &["clone", "-q", &bob_url, "bob-before"]);
    assert!(!denied.status.success(), "bob cloned before being granted");

    rewrite(&acl, "alice  agents/one  write\nbob  agents/one  read\n");
    let allowed = git(&work, &["clone", "-q", &bob_url, "bob-after"]);
    assert!(
        allowed.status.success(),
        "an appended grant did not take effect: {}",
        String::from_utf8_lossy(&allowed.stderr)
    );

    // Revoke it again. This direction is the one that has to work for
    // the ACL to be worth anything.
    rewrite(&acl, "alice  agents/one  write\n");
    let revoked = git(&work, &["clone", "-q", &bob_url, "bob-revoked"]);
    assert!(!revoked.status.success(), "a removed grant still cloned");

    // A broken file keeps the last good table rather than locking
    // everyone out or, worse, letting everyone in.
    rewrite(&acl, "alice agents/one sideways\n");
    let still_granted = git(
        &work,
        &[
            "clone",
            "-q",
            &format!("http://alice:a@{host}/agents/one.git"),
            "alice-after-break",
        ],
    );
    assert!(
        still_granted.status.success(),
        "a malformed edit dropped a grant that was already in force"
    );
    let still_denied = git(&work, &["clone", "-q", &bob_url, "bob-after-break"]);
    assert!(
        !still_denied.status.success(),
        "a malformed edit opened the node"
    );
}

/// The pre-receive hook's callback submits a ref op under any user's
/// name, spending authorization the git route already checked. Reaching
/// it with a user credential would forge a ref update on any repository
/// and skip every check above.
#[test]
fn the_hook_callback_is_not_reachable_with_a_user_credential() {
    let (base, _, _work, _) = served(
        "internal",
        "alice  agents/one  write\n",
        &["agents/one.git"],
    );
    let body = serde_json::json!({
        "repo": "agents/one.git",
        "refname": "refs/heads/main",
        "old": "0000000000000000000000000000000000000000",
        "new": "1111111111111111111111111111111111111111",
        "user": "alice",
    })
    .to_string();
    let (status, _) = curl(&[
        "-u",
        "alice:a",
        "-d",
        &body,
        &format!("{base}/api/git-update"),
    ]);
    assert_eq!(status, 403, "a user credential reached the hook callback");
}

/// A traversal segment is authorized against the repository named first
/// and then resolved by `git http-backend` against whatever it points
/// at. The node refuses it before either half can happen, so the answer
/// does not depend on what that resolution would have done.
#[test]
fn a_traversal_path_cannot_reach_a_repository_the_grant_does_not_cover() {
    let (base, _, _work, _) = served(
        "traversal",
        "alice  agents/one  read\n",
        &["agents/one.git", "agents/two.git"],
    );
    let url = format!(
        "{base}/agents/one.git/objects/../../agents/two.git/info/refs?service=git-upload-pack"
    );
    let out = std::process::Command::new("curl")
        .args([
            // Without this curl collapses the traversal itself and the
            // test would prove nothing about the node.
            "--path-as-is",
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "-u",
            "alice:a",
            &url,
        ])
        .output()
        .expect("curl runs");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "404",
        "a traversal path was not refused"
    );
}

/// Provisioning writes into a repository, so it is a write on that
/// repository — and the check runs before any directory is created.
#[test]
fn provisioning_a_workspace_needs_write_on_that_repository() {
    let (base, _, work, _) = served(
        "provision",
        "alice  agents/one  write\nbob  agents/one  read\n",
        &["agents/one.git"],
    );
    seed(&work, &base, "alice:a", "agents/one.git");
    let url = format!("{base}/api/workspace");
    let body = serde_json::json!({ "repo": "agents/one", "name": "ws" }).to_string();

    let (status, _) = curl(&["-u", "bob:b", "-d", &body, &url]);
    assert_eq!(status, 403, "a read grant provisioned a workspace");
    assert!(
        !work.join("repos/.choir/workspaces/agents/one/ws").exists(),
        "a refused caller still got a directory"
    );

    let (status, response) = curl(&["-u", "alice:a", "-d", &body, &url]);
    assert_eq!(status, 200, "a write grant was refused: {response}");
}

/// Phase B: contents were gated in phase A, and this is the inventory.
/// A reader granted one repository must not be able to enumerate the
/// other one's refs through the aggregate view or the page rendered from
/// it — which is the whole difference between "cannot clone it" and
/// "does not know it is there".
#[test]
fn the_view_and_the_page_show_only_the_repositories_a_reader_holds() {
    let (base, _, work, acl) = served(
        "view-filter",
        "alice  agents/one  write\n\
         bob    agents/two  write\n\
         carol  *           read\n",
        &["agents/one.git", "agents/two.git"],
    );
    // Both repositories are seeded over HTTP, so both refs reach the view
    // through the pre-receive hook the way every real ref does. Pushing
    // straight at a bare directory leaves the view empty, and the filter
    // would then pass by having nothing to hide — which is how this test
    // first went green against a leak it could not have seen.
    seed(&work, &base, "alice:a", "agents/one.git");
    std::fs::remove_dir_all(work.join("seed")).expect("clear the first seed clone");
    seed(&work, &base, "bob:b", "agents/two.git");

    let view = |creds: &str| -> serde_json::Value {
        let (status, body) = curl(&["-u", creds, &format!("{base}/api/view")]);
        assert_eq!(status, 200, "view refused for {creds}: {body}");
        body
    };

    let alice = view("alice:a");
    let refs = alice["refs"].as_object().expect("refs object");
    assert!(
        refs.keys().any(|k| k.starts_with("agents/one.git:")),
        "a granted repository's refs went missing: {refs:?}"
    );
    assert!(
        !refs.keys().any(|k| k.starts_with("agents/two.git:")),
        "an ungranted repository's refs were served: {refs:?}"
    );
    // The node-wide sections are gated, not narrowed.
    for section in ["bindings", "concentration", "view_growth", "sequencer_lag"] {
        assert!(
            alice.get(section).is_none(),
            "{section} reached a reader with no node-wide grant"
        );
    }
    // ...and what a writer needs to keep submitting survives.
    assert!(
        alice["log"]["node"].is_string(),
        "the log scope was withheld from a writer"
    );

    // The wildcard sees both, which is what makes the assertion above a
    // statement about the grant rather than about the seeding.
    let carol = view("carol:c");
    let carol_refs = carol["refs"].as_object().expect("refs object");
    assert!(
        carol_refs.keys().any(|k| k.starts_with("agents/two.git:")),
        "the wildcard grant could not see the second repository: {carol_refs:?}"
    );

    // The page is rendered from the same filtered payload, so the name
    // must not survive in the HTML either.
    let (status, _, page) = page_get(&format!("{base}/status"), &["-u", "alice:a"]);
    assert_eq!(status, 200, "the page was refused");
    assert!(
        page.contains("agents/one"),
        "the granted repository is missing from the page"
    );
    assert!(
        !page.contains("agents/two"),
        "the page named a repository its reader cannot read"
    );

    // The mutation: move the grant to the other repository and the same
    // request must flip. A filter checked only against the file this test
    // wrote first would pass while filtering nothing.
    rewrite(&acl, "alice  agents/two  read\ncarol  *  read\n");
    let flipped = view("alice:a");
    let refs = flipped["refs"].as_object().expect("refs object");
    assert!(
        refs.keys().any(|k| k.starts_with("agents/two.git:")),
        "the moved grant did not take effect: {refs:?}"
    );
    assert!(
        !refs.keys().any(|k| k.starts_with("agents/one.git:")),
        "the removed grant still served its repository: {refs:?}"
    );
    let (_, _, page) = page_get(&format!("{base}/status"), &["-u", "alice:a"]);
    assert!(
        page.contains("agents/two") && !page.contains("agents/one"),
        "the cached page outlived the grant that built it"
    );
}

/// The page cache is keyed per reader, so one reader's `ETag` must not
/// hand another reader a `304` for a page they were never shown — and a
/// reader's own repeat visit must still cost nothing.
#[test]
fn a_conditional_request_is_answered_per_reader() {
    let (base, _, work, _) = served(
        "view-etag",
        "alice  agents/one  write\ncarol  *  read\n",
        &["agents/one.git", "agents/two.git"],
    );
    seed(&work, &base, "alice:a", "agents/one.git");

    let tag_of = |creds: &str| -> String {
        let (status, headers, _) = page_get(&format!("{base}/status"), &["-u", creds]);
        assert_eq!(status, 200, "the page was refused for {creds}");
        headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.trim()
                    .eq_ignore_ascii_case("ETag")
                    .then(|| value.trim().to_string())
            })
            .expect("the page carries an ETag")
    };

    let alice = tag_of("alice:a");
    let carol = tag_of("carol:c");
    assert_ne!(
        alice, carol,
        "two readers of different pages share one ETag"
    );

    // The reader's own tag still short-circuits.
    let (status, _, _) = page_get(
        &format!("{base}/status"),
        &["-u", "alice:a", "-H", &format!("If-None-Match: {alice}")],
    );
    assert_eq!(status, 304, "a reader's own ETag did not produce a 304");

    // Somebody else's tag must not.
    let (status, _, _) = page_get(
        &format!("{base}/status"),
        &["-u", "alice:a", "-H", &format!("If-None-Match: {carol}")],
    );
    assert_eq!(status, 200, "another reader's ETag produced a 304");
}

/// Every section a real node serves has a disclosure rule.
///
/// The filter used to carry a hand-maintained list of section names, and
/// a section absent from it was served to everybody. That is not
/// hypothetical: `changes` was added to the view and reached readers
/// holding no grant on the repositories those changes belonged to.
/// `filter_response` now fails closed instead, which fixes the
/// disclosure but would hide the mistake — an unclassified section just
/// quietly stops being served. This is the half that stays loud.
///
/// It reads the view an auditor is served rather than a sample written
/// here, because a sample can only contain the sections whoever wrote it
/// remembered. A node-wide reader is the right vantage point: the
/// fail-closed retain does not run for them, so an unclassified section
/// still appears and is caught here rather than silently dropped.
#[test]
fn every_section_the_view_serves_is_classified() {
    let (base, _, work, _) = served(
        "section-coverage",
        "alice  agents/one  write\n\
         dave   @node      auditor\n",
        &["agents/one.git"],
    );
    // Seeded so the per-repository sections are populated rather than
    // absent: a section that is never emitted cannot be checked, and an
    // empty view would let this pass by having nothing to classify.
    seed(&work, &base, "alice:a", "agents/one.git");

    let (status, view) = curl(&["-u", "dave:d", &format!("{base}/api/view")]);
    assert_eq!(status, 200, "the auditor's view was refused: {view}");
    // Both endpoints the table covers, unioned. `pending` and its
    // omission mark come only from `/api/reviews`, and exempting them by
    // name here — as this test used to do for `pending` — is how a
    // section can be classified for an endpoint nobody checks. Asking
    // the second endpoint costs one request and removes the exemption.
    let (status, queue) = curl(&["-u", "dave:d", &format!("{base}/api/reviews?reviewer=dave")]);
    assert_eq!(
        status, 200,
        "the auditor's review queue was refused: {queue}"
    );
    let mut served_sections = view.as_object().expect("the view is a JSON object").clone();
    served_sections.extend(
        queue
            .as_object()
            .expect("the queue is a JSON object")
            .clone(),
    );

    let unclassified: Vec<&String> = served_sections
        .keys()
        .filter(|name| choir_node::acl::disclosure(name).is_none())
        .collect();
    assert!(
        unclassified.is_empty(),
        "these sections are served but have no disclosure rule, so every reader \
         without a node-wide grant silently stops seeing them: {unclassified:?}. \
         Add a row to acl::SECTIONS saying whether each is Public, NodeWide or \
         PerRepo, and give any PerRepo one a narrowing in filter_response."
    );

    // The reverse direction: a row for a section nothing serves is a
    // rule guarding nothing, and it hides that the real one was renamed.
    let missing: Vec<&str> = choir_node::acl::SECTIONS
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| !served_sections.contains_key(*name))
        .collect();
    assert!(
        missing.is_empty(),
        "acl::SECTIONS classifies sections the view does not serve: {missing:?}"
    );
}

/// Answering a review is a read-level act (D55).
///
/// A reviewer is drawn onto somebody else's change, and the whole point
/// of drawing them is that they are not the person with push rights. So
/// a verdict, a comment and a viewing receipt authorize against the
/// repository under review at `read`, while everything that moves a ref
/// still needs `write` — and the attribution binding at admission is
/// what makes that safe: those three ops are refused unless the name
/// they claim is the channel that signed them.
///
/// Before this, every `/api/submit` op needed `write`, so a node that
/// asked an outsider to review had to hand them push access first.
#[test]
fn a_drawn_reviewer_answers_with_read_and_still_cannot_push() {
    let (base, key, _work, _) = served(
        "reviewer-read",
        "alice  agents/one  write\n\
         bob    agents/one  read\n",
        &["agents/one.git"],
    );
    let url = format!("{base}/api/submit");

    // Alice, who may push, asks bob to look at a commit on her ref.
    let request = ViewOp::new(OpKind::RequestReview {
        id: "r-read".into(),
        target: choir_oplog::ContentHash::blake3(b"a change bob did not write"),
        reviewers: vec!["bob/reviewer".into()],
        target_ref: Some("agents/one.git:refs/heads/topic".into()),
    });
    let (status, body) = curl(&[
        "-u",
        "alice:a",
        "-d",
        &submit_body(&key, "alice/agent", &request),
        &url,
    ]);
    assert_eq!(status, 200, "the request for review was refused: {body}");

    // Bob holds read and nothing else. He can say what he thinks.
    //
    // Every submission here signs with the one key `served` registered:
    // what is under test is the grant behind the HTTP credential, and a
    // second registered key would only restate the identity layer that
    // `key_names` already covers.
    let comment = ViewOp::new(OpKind::PostComment {
        id: "r-read".into(),
        comment: "c1".into(),
        author: "bob/reviewer".into(),
        body: "the greeting reads well".into(),
    });
    let (status, body) = curl(&[
        "-u",
        "bob:b",
        "-d",
        &submit_body(&key, "bob/reviewer", &comment),
        &url,
    ]);
    assert_eq!(status, 200, "a read grant could not comment: {body}");

    let verdict = ViewOp::new(OpKind::PostVerdict {
        id: "r-read".into(),
        reviewer: "bob/reviewer".into(),
        verdict: choir_view::Verdict::Approve,
        note: "approved".into(),
    });
    let (status, body) = curl(&[
        "-u",
        "bob:b",
        "-d",
        &submit_body(&key, "bob/reviewer", &verdict),
        &url,
    ]);
    assert_eq!(status, 200, "a read grant could not answer: {body}");

    // ...and that is the whole of what read bought him. The same
    // credential moving the ref he just approved is still refused.
    let push = ViewOp::new(OpKind::SetRef {
        name: "agents/one.git:refs/heads/topic".into(),
        commit: choir_oplog::ContentHash::blake3(b"bob's own commit"),
        prev: None,
    });
    let (status, _) = curl(&[
        "-u",
        "bob:b",
        "-d",
        &submit_body(&key, "bob/reviewer", &push),
        &url,
    ]);
    assert_eq!(status, 403, "a read grant moved a ref");

    // Nor is read on one repository a licence to review on the node: a
    // credential with no row at all is told the repository is not there.
    let intrude = ViewOp::new(OpKind::PostComment {
        id: "r-read".into(),
        comment: "c2".into(),
        author: "dave/agent".into(),
        body: "hello".into(),
    });
    let (status, _) = curl(&[
        "-u",
        "dave:d",
        "-d",
        &submit_body(&key, "dave/agent", &intrude),
        &url,
    ]);
    assert_eq!(status, 404, "an ungranted credential reached a review");
}
