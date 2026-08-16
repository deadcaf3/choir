//! Drives `choir runner` against a real node.
//!
//! The library half of the seam is unit-tested, but everything it
//! decides is only worth anything if the subcommand actually wires it to
//! the node: resolves a base from the view, provisions under the derived
//! binding, checks what came back, and archives the same change. That
//! path crosses HTTP and a real workspace on disk, so it is exercised
//! here rather than asserted about.

use choir_identity::{ActorKey, Registry};
use choir_node::{Node, Platform};
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

/// Runs `choir runner <config>` with `request` on stdin.
fn runner(config: &std::path::Path, request: &serde_json::Value) -> (bool, serde_json::Value) {
    use std::io::Write;
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_choir"))
        .args(["runner", config.to_str().expect("config path")])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("choir runs");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(request.to_string().as_bytes())
        .expect("write request");
    let out = child.wait_with_output().expect("choir completes");
    let value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        panic!(
            "runner must answer JSON on stdout, got {:?}",
            String::from_utf8_lossy(&out.stdout)
        )
    });
    (out.status.success(), value)
}

#[test]
fn the_runner_drives_one_lifecycle_end_to_end() {
    let work = std::env::temp_dir().join(format!("choir-runner-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();

    let owner_key = ActorKey::from_secret_bytes(&[11; 32]);
    let key_file = work.join("owner.key");
    std::fs::write(&key_file, owner_key.secret_bytes()).unwrap();
    let mut registry = Registry::new();
    registry.register(&owner_key.public_key_bytes()).unwrap();

    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate()).unwrap(),
    );
    node.create_repo("agents/demo.git").unwrap();
    let port = node.port();
    let node = std::sync::Arc::new(node);
    std::thread::spawn(move || node.serve_forever());
    let api = format!("http://127.0.0.1:{port}");

    // A base ref only exists once a commit has been pushed, and the
    // runner resolves its base from the view rather than being told one.
    let seed = work.join("seed");
    let url = format!("{api}/agents/demo.git");
    assert!(git(&work, &["clone", "-q", &url, seed.to_str().unwrap()])
        .status
        .success());
    std::fs::write(seed.join("f.txt"), "v1\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "first"]);
    assert!(git(&seed, &["push", "-q", "origin", "HEAD:main"])
        .status
        .success());
    let head = String::from_utf8_lossy(&git(&seed, &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();

    let config_path = work.join("runner.json");
    std::fs::write(
        &config_path,
        serde_json::json!({
            "api": api,
            "repo": "agents/demo",
            "owner": "operator/agent",
            "key_file": key_file.to_str().unwrap(),
            "namespace": "sy",
            "base_ref": "agents/demo.git:refs/heads/main",
        })
        .to_string(),
    )
    .unwrap();

    let ensure = serde_json::json!({
        "protocol_version": 1,
        "operation": "ensure",
        "scheme": "from-external",
        "workspace_key": "issue-7",
        "external_id": "ISSUE-7",
        "generation": "1",
    });

    let (ok, result) = runner(&config_path, &ensure);
    assert!(ok, "ensure failed: {result}");
    assert_eq!(result["operation"], "ensure");
    assert_eq!(result["binding"]["scheme"], "from-external");
    // The base was resolved from the view, not supplied.
    assert_eq!(result["binding"]["base"], head);
    assert_eq!(result["workspace"]["created_now"], true);
    let path = result["workspace"]["path"]
        .as_str()
        .expect("a workspace path")
        .to_string();
    assert!(
        std::path::Path::new(&path).join("f.txt").exists(),
        "the workspace was not provisioned from the base revision"
    );
    let change_id = result["binding"]["change_id"]
        .as_str()
        .expect("a change id")
        .to_string();
    let workspace_id = result["binding"]["workspace_id"]
        .as_str()
        .expect("a workspace id")
        .to_string();

    // Convergence, which is the reason this scheme costs durable state:
    // the identical request is the same binding and does not provision a
    // second workspace for one unit of work.
    let (ok, again) = runner(&config_path, &ensure);
    assert!(ok, "the idempotent retry failed: {again}");
    assert_eq!(again["binding"]["change_id"], change_id.as_str());
    assert_eq!(
        again["workspace"]["created_now"], false,
        "retry re-provisioned"
    );

    // A different attempt at the same work is a different change.
    let mut second_attempt = ensure.clone();
    second_attempt["generation"] = serde_json::json!("2");
    let (ok, other) = runner(&config_path, &second_attempt);
    assert!(ok, "the second attempt failed: {other}");
    assert_ne!(other["binding"]["change_id"], change_id.as_str());

    // The change is in the view under the derived identity, which is
    // what makes the binding real rather than a string the adapter made
    // up and echoed back to itself.
    let view: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(
        &std::process::Command::new(env!("CARGO_BIN_EXE_choir"))
            .args(["view", &api])
            .output()
            .expect("choir view")
            .stdout,
    ))
    .expect("view is JSON");
    assert!(
        view["changes"][&change_id].is_object(),
        "the derived change is absent from the view: {}",
        view["changes"]
    );
    assert_eq!(
        view["changes"][&change_id]["workspace_id"],
        workspace_id.as_str()
    );

    // Archive detaches the workspace under the same binding.
    let mut archive = ensure;
    archive["operation"] = serde_json::json!("archive");
    let (ok, archived) = runner(&config_path, &archive);
    assert!(ok, "archive failed: {archived}");
    assert_eq!(archived["archive"]["change_id"], change_id.as_str());
    assert!(
        archived["archive"]["archived_path"].is_string(),
        "archive returned no recoverable path: {archived}"
    );
    assert!(
        !std::path::Path::new(&path).exists(),
        "the live workspace directory survived the archive"
    );
}

/// Metering is not a permanent refusal, and the runner has to say so.
///
/// A node's 429 body carries no typed `code`, so this outcome rests
/// entirely on unknown codes defaulting to transient. A scheduler that
/// read it as terminal would abandon work that a minute's wait would
/// have completed, and it would do that precisely when the node is
/// busiest. Pinned against a real 429 from a real limiter rather than a
/// hand-written body, because the whole risk here is what the node
/// actually emits.
#[test]
fn a_rate_limited_node_is_reported_as_worth_retrying() {
    let work = std::env::temp_dir().join(format!("choir-runner-429-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();

    let owner_key = ActorKey::from_secret_bytes(&[13; 32]);
    let key_file = work.join("owner.key");
    std::fs::write(&key_file, owner_key.secret_bytes()).unwrap();
    let mut registry = Registry::new();
    registry.register(&owner_key.public_key_bytes()).unwrap();

    // Metering only applies to an authenticated user without a node-wide
    // grant, so the node needs an auth table and no ACL.
    let mut auth = choir_node::AuthTable::new();
    auth.insert("scheduler".to_string(), "t".to_string());
    let auth_file = work.join("auth");
    std::fs::write(&auth_file, "scheduler:t\n").unwrap();

    let mut node = Node::bind_with_auth(&work.join("repos"), 0, Some(auth)).unwrap();
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate()).unwrap(),
    );
    // One API request per minute: an `ensure` spends one resolving its
    // base and is metered on the create that follows.
    node.enable_rate_limit(std::num::NonZeroU32::new(1), None);
    node.create_repo("agents/demo.git").unwrap();
    let port = node.port();
    let node = std::sync::Arc::new(node);
    std::thread::spawn(move || node.serve_forever());
    let api = format!("http://127.0.0.1:{port}");

    // Seeding is git traffic, which is a separate unmetered class here.
    let seed = work.join("seed");
    let url = format!("http://scheduler:t@127.0.0.1:{port}/agents/demo.git");
    assert!(git(&work, &["clone", "-q", &url, seed.to_str().unwrap()])
        .status
        .success());
    std::fs::write(seed.join("f.txt"), "v1\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "first"]);
    assert!(git(&seed, &["push", "-q", "origin", "HEAD:main"])
        .status
        .success());

    let config_path = work.join("runner.json");
    std::fs::write(
        &config_path,
        serde_json::json!({
            "api": api,
            "repo": "agents/demo",
            "owner": "operator/agent",
            "key_file": key_file.to_str().unwrap(),
            "namespace": "sy",
            "base_ref": "agents/demo.git:refs/heads/main",
            "auth_file": auth_file.to_str().unwrap(),
            "auth_user": "scheduler",
        })
        .to_string(),
    )
    .unwrap();

    let (ok, refused) = runner(
        &config_path,
        &serde_json::json!({
            "protocol_version": 1,
            "operation": "ensure",
            "scheme": "from-external",
            "workspace_key": "issue-9",
            "external_id": "ISSUE-9",
            "generation": "1",
        }),
    );
    assert!(
        !ok,
        "the metered request was reported as success: {refused}"
    );
    assert_eq!(
        refused["error"]["retryable"], true,
        "rate limiting was reported as permanent, which strands work that would succeed: {refused}"
    );
    assert_eq!(refused["error"]["code"], "choir_unavailable");
    assert!(
        refused["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("rate limit")),
        "the refusal did not say what actually happened: {refused}"
    );
}

/// A refused request still answers a typed result on stdout, because an
/// orchestrator branches on `retryable` and cannot parse a stack trace.
#[test]
fn a_refusal_is_machine_readable_and_says_whether_to_retry() {
    let work = std::env::temp_dir().join(format!("choir-runner-bad-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let key_file = work.join("owner.key");
    std::fs::write(&key_file, [11_u8; 32]).unwrap();

    let config_path = work.join("runner.json");
    std::fs::write(
        &config_path,
        serde_json::json!({
            // Nothing listens here; the point is the shape of the answer.
            "api": "http://127.0.0.1:1",
            "repo": "agents/demo",
            "owner": "operator/agent",
            "key_file": key_file.to_str().unwrap(),
            "namespace": "sy",
            "base_ref": "agents/demo.git:refs/heads/main",
        })
        .to_string(),
    )
    .unwrap();

    // A request the seam itself refuses, before any network call.
    let (ok, refused) = runner(
        &config_path,
        &serde_json::json!({
            "protocol_version": 1,
            "operation": "ensure",
            "scheme": "from-external",
            "workspace_key": "../escape",
            "external_id": "ISSUE-7",
            "generation": "1",
        }),
    );
    assert!(!ok, "a traversal key was accepted");
    assert_eq!(refused["protocol_version"], 1);
    assert_eq!(refused["error"]["code"], "invalid_request");
    assert_eq!(
        refused["error"]["retryable"], false,
        "an unsafe request was reported as worth retrying"
    );
}
