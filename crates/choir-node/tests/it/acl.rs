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

/// Rewrites the ACL and forces a distinct mtime, so the reload cannot be
/// missed because two writes landed inside one timestamp tick.
fn rewrite(path: &std::path::Path, text: &str) {
    std::fs::write(path, text).expect("acl rewrite");
    let file = std::fs::File::options().write(true).open(path).expect("reopen acl");
    let ahead = std::time::SystemTime::now() + std::time::Duration::from_secs(1);
    file.set_times(std::fs::FileTimes::new().set_modified(ahead))
        .expect("stamp acl mtime");
}

/// Seeds `repo` with one commit, using a credential that may push, and
/// returns the clone directory.
fn seed(work: &std::path::Path, base: &str, creds: &str, repo: &str) -> std::path::PathBuf {
    let url = format!("http://{creds}@{}/{repo}", base.trim_start_matches("http://"));
    let dir = work.join("seed");
    assert!(
        git(work, &["clone", "-q", &url, dir.to_str().unwrap()]).status.success(),
        "seeding clone failed"
    );
    std::fs::write(dir.join("f.txt"), "seed\n").unwrap();
    assert!(git(&dir, &["add", "."]).status.success());
    assert!(git(&dir, &["commit", "-q", "-m", "seed"]).status.success());
    let push = git(&dir, &["push", "-q", "origin", "HEAD:main"]);
    assert!(push.status.success(), "{}", String::from_utf8_lossy(&push.stderr));
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
    assert!(git(&work, &["clone", "-q", &alice, dir.to_str().unwrap()]).status.success());
    std::fs::write(dir.join("g.txt"), "alice\n").unwrap();
    assert!(git(&dir, &["add", "."]).status.success());
    assert!(git(&dir, &["commit", "-q", "-m", "alice"]).status.success());
    assert!(git(&dir, &["push", "-q", "origin", "HEAD:main"]).status.success());

    // Read grant: clone lands, push is refused.
    let bob = format!("http://bob:b@{host}/agents/one.git");
    let bobdir = work.join("bob");
    assert!(git(&work, &["clone", "-q", &bob, bobdir.to_str().unwrap()]).status.success());
    std::fs::write(bobdir.join("h.txt"), "bob\n").unwrap();
    assert!(git(&bobdir, &["add", "."]).status.success());
    assert!(git(&bobdir, &["commit", "-q", "-m", "bob"]).status.success());
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
    assert!(!stderr.contains("403"), "a denial confirmed the repo exists: {stderr}");

    // The wildcard reaches every repository at its level, and no further.
    let carol = format!("http://carol:c@{host}/agents/two.git");
    let out = git(&work, &["clone", "-q", &carol, "carol"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
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
    assert_eq!(status, 404, "a write grant on one repo moved another one's ref");

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
    let still_granted = git(&work, &["clone", "-q", &format!("http://alice:a@{host}/agents/one.git"), "alice-after-break"]);
    assert!(
        still_granted.status.success(),
        "a malformed edit dropped a grant that was already in force"
    );
    let still_denied = git(&work, &["clone", "-q", &bob_url, "bob-after-break"]);
    assert!(!still_denied.status.success(), "a malformed edit opened the node");
}

/// The pre-receive hook's callback submits a ref op under any user's
/// name, spending authorization the git route already checked. Reaching
/// it with a user credential would forge a ref update on any repository
/// and skip every check above.
#[test]
fn the_hook_callback_is_not_reachable_with_a_user_credential() {
    let (base, _, _work, _) = served("internal", "alice  agents/one  write\n", &["agents/one.git"]);
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
