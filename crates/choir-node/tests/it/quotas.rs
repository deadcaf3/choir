//! Per-user quotas (D37) against a real served node: the workspace
//! ceiling refuses in the node's own words, counts per user rather than
//! per node, and does not shadow the answers `provision` already gives.
//!
//! What is deliberately not here: anything that restarts a daemon or
//! reads the request log's tail. Both live in `tests/quotas.rs`, which is
//! its own binary — this harness shares a process and runs its modules on
//! parallel threads, and a restart is exactly the thing that cannot be
//! asserted under that.

use choir_identity::{ActorKey, Registry};
use choir_node::{AuthTable, Node, Platform};
use choir_oplog::MemLog;

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

/// Seeds one commit into a bare repository without going through the
/// daemon, so a test of the workspace ceiling does not also depend on the
/// push path.
fn seed_commit(work: &std::path::Path, bare: &std::path::Path) {
    let head_ref = String::from_utf8_lossy(&git(bare, &["symbolic-ref", "HEAD"]).stdout)
        .trim()
        .to_string();
    let seed = work.join("seed");
    std::fs::create_dir_all(&seed).expect("seed dir");
    assert!(git(&seed, &["init", "-q"]).status.success());
    std::fs::write(seed.join("f.txt"), "v1\n").expect("seed file");
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "first"]);
    let refspec = format!("HEAD:{head_ref}");
    let push = git(
        &seed,
        &["push", "-q", bare.to_str().expect("utf-8 path"), &refspec],
    );
    assert!(push.status.success(), "{push:?}");
}

fn api(port: u16, user: &str, body: &str) -> (u16, serde_json::Value) {
    let url = format!("http://127.0.0.1:{port}/api/workspace");
    let out = std::process::Command::new("curl")
        .args(["-s", "-w", "\n%{http_code}", "-u", user, "-X", "POST", "-d", body, &url])
        .output()
        .expect("curl runs");
    let text = String::from_utf8_lossy(&out.stdout);
    let (body, code) = text.rsplit_once('\n').expect("status line");
    (
        code.trim().parse().expect("numeric status"),
        serde_json::from_str(body).unwrap_or_else(|e| panic!("JSON response ({e}): {body:?}")),
    )
}

fn create(port: u16, user: &str, name: &str) -> (u16, serde_json::Value) {
    api(
        port,
        user,
        &format!(r#"{{"repo":"agents/demo","name":"{name}"}}"#),
    )
}

#[test]
fn the_workspace_ceiling_is_per_user_and_names_its_own_repair() {
    let work = std::env::temp_dir().join("choir-node-quotas-workspaces");
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");
    let root = work.join("repos");

    let mut auth = AuthTable::new();
    auth.insert("alice".into(), "a".into());
    auth.insert("bob".into(), "b".into());
    let mut node = Node::bind_with_auth(&root, 0, Some(auth)).expect("node binds");
    let port = node.port();
    node.create_repo("agents/demo.git").expect("repo created");
    seed_commit(&work, &root.join("agents/demo.git"));
    node.enable_quotas(None, std::num::NonZeroU32::new(2));
    node.enable_platform(
        Platform::start(Registry::new(), Box::new(MemLog::new()), ActorKey::generate())
            .expect("platform starts"),
    );
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }

    // Alice's allowance is two, and the second one is not the one refused.
    for name in ["one", "two"] {
        let (code, body) = create(port, "alice:a", name);
        assert_eq!(code, 200, "{name}: {body}");
    }

    // The third is refused, in the node's own words: a stable code, the
    // two numbers that were compared, and the action that frees room.
    let (code, body) = create(port, "alice:a", "three");
    assert_eq!(code, 403, "{body}");
    assert_eq!(body["code"], "quota_exceeded", "{body}");
    assert_eq!(body["expected"], "at most 2 workspaces", "{body}");
    assert_eq!(body["actual"], "2 workspaces", "{body}");
    assert!(
        body["next"].as_str().is_some_and(|s| s.contains("archive")),
        "the refusal must name what frees the allowance: {body}"
    );

    // The ceiling is per user, not per node: bob's own allowance is
    // untouched by alice having spent hers. A shared counter would fail
    // here, and only here.
    let (code, body) = create(port, "bob:b", "bobs-one");
    assert_eq!(code, 200, "{body}");

    // And the quota does not answer questions `provision` already
    // answers: a name that exists gets that conflict, not the quota's.
    let (code, body) = create(port, "bob:b", "bobs-one");
    assert_eq!(code, 409, "{body}");
    assert_ne!(body["code"], "quota_exceeded", "{body}");
}

#[test]
fn a_node_wide_grant_holder_is_exempt_from_the_workspace_ceiling() {
    // D33's second exemption, applied to D37 unchanged: the `@node`
    // grant is already total authority over this node, so throttling the
    // one actor who can repair it — during the incident the quota is
    // reporting — is worse than the consumption. A quota that can lock
    // the operator out is worse than no quota.
    let work = std::env::temp_dir().join("choir-node-quotas-exempt");
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");
    let root = work.join("repos");

    let mut auth = AuthTable::new();
    auth.insert("alice".into(), "a".into());
    auth.insert("carol".into(), "c".into());
    let mut node = Node::bind_with_auth(&root, 0, Some(auth)).expect("node binds");
    let port = node.port();
    node.create_repo("agents/demo.git").expect("repo created");
    seed_commit(&work, &root.join("agents/demo.git"));
    let acl = work.join("acl");
    std::fs::write(&acl, "alice   *       write\ncarol   *       write\ncarol   @node   auditor\n")
        .expect("acl file");
    node.watch_acl_file(acl).expect("acl loads");
    node.enable_quotas(None, std::num::NonZeroU32::new(1));
    node.enable_platform(
        Platform::start(Registry::new(), Box::new(MemLog::new()), ActorKey::generate())
            .expect("platform starts"),
    );
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }

    // Alice has the same grant on the repository and no `@node` grant,
    // so the ceiling is hers to hit. This is the control: without it the
    // exemption below could be a quota that never ran at all.
    assert_eq!(create(port, "alice:a", "alice-one").0, 200);
    assert_eq!(create(port, "alice:a", "alice-two").0, 403);

    // Carol holds `@node`, so the same ceiling does not apply to her.
    for name in ["carol-one", "carol-two", "carol-three"] {
        let (code, body) = create(port, "carol:c", name);
        assert_eq!(code, 200, "the @node grant holder was refused: {name}: {body}");
    }
}

#[test]
fn an_unset_workspace_ceiling_limits_nothing() {
    // The mirror of the D33 finding that a class whose ceiling was unset
    // returned before the map was consulted, so a test believed it had
    // proved separation when it had proved nothing. Here the unset case
    // is asserted on purpose rather than relied on by accident.
    let work = std::env::temp_dir().join("choir-node-quotas-unset");
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");
    let root = work.join("repos");

    let mut auth = AuthTable::new();
    auth.insert("alice".into(), "a".into());
    let mut node = Node::bind_with_auth(&root, 0, Some(auth)).expect("node binds");
    let port = node.port();
    node.create_repo("agents/demo.git").expect("repo created");
    seed_commit(&work, &root.join("agents/demo.git"));
    node.enable_quotas(None, None);
    node.enable_platform(
        Platform::start(Registry::new(), Box::new(MemLog::new()), ActorKey::generate())
            .expect("platform starts"),
    );
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }

    for name in ["one", "two", "three", "four", "five"] {
        let (code, body) = create(port, "alice:a", name);
        assert_eq!(code, 200, "{name}: {body}");
    }
}
