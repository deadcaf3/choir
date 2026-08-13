//! Instant workspace provisioning acceptance (`POST /api/workspace`):
//! a CoW workspace appears on disk, registers in the view, is
//! independent of its siblings, and pushes ride the sequenced HTTP
//! path. Includes a small p50 measurement (printed with --nocapture).

use choir_identity::{ActorKey, Registry};
use choir_node::platform::hex_encode;
use choir_node::{Node, Platform};
use choir_oplog::MemLog;
use choir_view::{ArchiveAuthorization, CreateAuthorization};

fn git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false", "-c", "init.defaultBranch=main"])
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

fn api(port: u16, method: &str, path: &str, body: Option<&str>) -> (u16, serde_json::Value) {
    let url = format!("http://127.0.0.1:{port}{path}");
    let mut args = vec!["-s", "-w", "\n%{http_code}", "-X", method];
    if let Some(b) = body {
        args.extend(["-d", b]);
    }
    args.push(&url);
    let out = std::process::Command::new("curl").args(&args).output().expect("curl runs");
    let text = String::from_utf8_lossy(&out.stdout);
    let (body, code) = text.rsplit_once('\n').expect("status line");
    (code.trim().parse().expect("numeric status"), serde_json::from_str(body).expect("json"))
}

fn signed_create_body(
    key: &ActorKey,
    repo: &str,
    name: &str,
    base: &str,
    owner: &str,
    change: &str,
    idempotency_key: &str,
) -> String {
    let authorization = CreateAuthorization::new(
        change.into(),
        owner.into(),
        format!("{repo}/{name}"),
        choir_oplog::ContentHash::from_git_oid(base).unwrap(),
        idempotency_key.into(),
    )
    .to_payload();
    let signature = key.sign_submission(owner, &authorization);
    serde_json::json!({
        "repo": repo, "name": name, "base": base, "owner": owner,
        "change": change, "idempotency_key": idempotency_key,
        "channel": owner, "payload_hex": hex_encode(&authorization),
        "key_id": signature.key_id, "signature_hex": hex_encode(&signature.signature),
    })
    .to_string()
}

#[test]
// The thread-spawn collect below is load-bearing and clippy's
// needless_collect is wrong about it: it forces every request to be in
// flight before any is joined. Consumed lazily the requests would go out
// one at a time and the race this test exists to provoke -- six unique
// names plus one duplicate racer -- would never happen.
#[allow(clippy::needless_collect)]
fn workspace_provisioning_end_to_end() {
    let work = std::env::temp_dir().join(format!("choir-node-ws-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let root = work.join("repos");

    let owner_key = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&owner_key.public_key_bytes()).unwrap();
    let mut node = Node::bind(&root, 0).unwrap();
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate()).unwrap(),
    );
    let port = node.port();
    node.create_repo("agents/demo.git").unwrap();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }

    // Empty repo: provisioning must refuse, not create a broken dir.
    let (code, resp) = api(port, "POST", "/api/workspace",
        Some(r#"{"repo":"agents/demo","name":"too-early"}"#));
    assert_eq!(code, 400, "{resp}");

    // Seed a commit through the daemon (the sequenced path).
    let url = format!("http://127.0.0.1:{port}/agents/demo.git");
    let seed = work.join("seed");
    assert!(git(&work, &["clone", "-q", &url, seed.to_str().unwrap()]).status.success());
    std::fs::write(seed.join("f.txt"), "v1\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "first"]);
    assert!(git(&seed, &["push", "-q", "origin", "HEAD:main"]).status.success());
    let head = String::from_utf8_lossy(&git(&seed, &["rev-parse", "HEAD"]).stdout).trim().to_string();

    // Two workspaces provision and are independent.
    let (code, ws1) = api(port, "POST", "/api/workspace",
        Some(r#"{"repo":"agents/demo","name":"agent-1"}"#));
    assert_eq!(code, 200, "{ws1}");
    assert_eq!(ws1["head"].as_str().unwrap(), head);
    let (code, ws2) = api(port, "POST", "/api/workspace",
        Some(r#"{"repo":"agents/demo","name":"agent-2"}"#));
    assert_eq!(code, 200, "{ws2}");

    let p1 = std::path::PathBuf::from(ws1["path"].as_str().unwrap());
    let p2 = std::path::PathBuf::from(ws2["path"].as_str().unwrap());
    assert_eq!(std::fs::read_to_string(p1.join("f.txt")).unwrap(), "v1\n");
    std::fs::write(p1.join("f.txt"), "agent-1 edit\n").unwrap();
    assert_eq!(std::fs::read_to_string(p2.join("f.txt")).unwrap(), "v1\n",
        "workspaces must not share mutable state");

    // Both registered in the view under repo/name.
    let (_, view) = api(port, "GET", "/api/view", None);
    assert!(view["workspaces"].get("agents/demo/agent-1").is_some(), "{view}");
    assert!(view["workspaces"].get("agents/demo/agent-2").is_some());

    // Workspace origin is the daemon URL, not the on-disk bare path.
    let origin = String::from_utf8_lossy(&git(&p1, &["remote", "get-url", "origin"]).stdout)
        .trim()
        .to_string();
    assert_eq!(origin, url);
    // And a push from the workspace rides the sequenced path.
    git(&p1, &["add", "."]);
    git(&p1, &["commit", "-q", "-m", "ws edit"]);
    // Workspaces start on a detached HEAD, so pushes name the full ref.
    let push = git(&p1, &["push", "-q", "origin", "HEAD:refs/heads/agent-1-work"]);
    assert!(push.status.success(), "{}", String::from_utf8_lossy(&push.stderr));
    let (_, view) = api(port, "GET", "/api/view", None);
    assert!(view["refs"].get("agents/demo.git:refs/heads/agent-1-work").is_some(), "{view}");

    // Guard rails: duplicate name, traversal, missing repo.
    let (code, _) = api(port, "POST", "/api/workspace",
        Some(r#"{"repo":"agents/demo","name":"agent-1"}"#));
    assert_eq!(code, 409);
    let (code, _) = api(port, "POST", "/api/workspace",
        Some(r#"{"repo":"agents/demo","name":"../escape"}"#));
    assert_eq!(code, 400);
    let (code, _) = api(port, "POST", "/api/workspace",
        Some(r#"{"repo":"agents/nope","name":"x"}"#));
    assert_eq!(code, 404);

    // Adapter-grade creation is pinned to an exact base even after the
    // repository advances. The stable change binding makes retries
    // idempotent and exposes change/revision identity in the view.
    std::fs::write(seed.join("f.txt"), "v2\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "second"]);
    assert!(git(&seed, &["push", "-q", "origin", "HEAD:main"]).status.success());
    let head2 = String::from_utf8_lossy(&git(&seed, &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();
    let advanced = signed_create_body(
        &owner_key,
        "agents/demo",
        "adapter-1",
        &head,
        "operator/agent",
        "change-1",
        "request-1",
    );
    let forged = signed_create_body(
        &ActorKey::generate(),
        "agents/demo",
        "forged",
        &head,
        "operator/agent",
        "change-forged",
        "request-forged",
    );
    let (code, problem) = api(port, "POST", "/api/workspace", Some(&forged));
    assert_eq!(code, 409, "{problem}");
    assert_eq!(problem["code"], "unknown_key", "{problem}");
    assert!(!root.join(".choir/workspaces/agents/demo/forged").exists());
    let (code, created) = api(port, "POST", "/api/workspace", Some(&advanced));
    assert_eq!(code, 200, "{created}");
    assert_eq!(created["head"], head);
    assert_eq!(created["created"], true);
    let adapter_path = std::path::PathBuf::from(created["path"].as_str().unwrap());
    assert_eq!(std::fs::read_to_string(adapter_path.join("f.txt")).unwrap(), "v1\n");
    let (code, reused) = api(port, "POST", "/api/workspace", Some(&advanced));
    assert_eq!(code, 200, "{reused}");
    assert_eq!(reused["created"], false);
    assert_eq!(reused["reused"], true);
    assert_eq!(reused["path"], created["path"]);
    assert_eq!(reused["operation"]["seq"], created["operation"]["seq"]);
    assert_eq!(reused["operation"]["hash"], created["operation"]["hash"]);
    assert_eq!(reused["operation"]["already_applied"], true);

    let (_, view) = api(port, "GET", "/api/view", None);
    assert_eq!(view["changes"]["change-1"]["owner"], "operator/agent");
    assert_eq!(
        view["changes"]["change-1"]["active_workspace"],
        "agents/demo/adapter-1"
    );
    assert_eq!(
        view["changes"]["change-1"]["base_revision"],
        format!("11-{head}")
    );
    assert_eq!(
        view["workspaces"]["agents/demo/adapter-1"],
        format!("11-{head}")
    );

    for (field, value) in [
        ("base", head2.as_str()),
        ("owner", "other/agent"),
        ("change", "change-2"),
        ("idempotency_key", "request-2"),
    ] {
        let mut mismatch: serde_json::Value = serde_json::from_str(&advanced).unwrap();
        mismatch[field] = serde_json::json!(value);
        let (code, problem) = api(
            port,
            "POST",
            "/api/workspace",
            Some(&mismatch.to_string()),
        );
        assert_eq!(code, 409, "{field}: {problem}");
        assert_eq!(problem["code"], "workspace_state", "{field}: {problem}");
    }
    let invalid = serde_json::json!({
        "repo": "agents/demo", "name": "invalid-base", "base": "abc",
        "owner": "operator/agent", "change": "invalid-change",
        "idempotency_key": "invalid-request",
    })
    .to_string();
    let (code, problem) = api(port, "POST", "/api/workspace", Some(&invalid));
    assert_eq!(code, 400, "{problem}");
    assert!(!root.join(".choir/workspaces/agents/demo/invalid-base").exists());

    // Archive retains dirty and unpushed workspace data, removes only the
    // active view row, and is idempotent under a lost response.
    std::fs::write(adapter_path.join("unpublished.txt"), "keep me\n").unwrap();
    let archive_payload = ArchiveAuthorization::new(
        "change-1".into(),
        "agents/demo/adapter-1".into(),
        choir_oplog::ContentHash::from_git_oid(&head).unwrap(),
    )
    .to_payload();
    let wrong_signature = ActorKey::generate().sign_submission("operator/agent", &archive_payload);
    let wrong_archive = serde_json::json!({
        "repo": "agents/demo", "name": "adapter-1", "change": "change-1",
        "idempotency_key": "request-1", "channel": "operator/agent",
        "payload_hex": hex_encode(&archive_payload), "key_id": wrong_signature.key_id,
        "signature_hex": hex_encode(&wrong_signature.signature),
    })
    .to_string();
    let (code, problem) = api(
        port,
        "POST",
        "/api/workspace/archive",
        Some(&wrong_archive),
    );
    assert_eq!(code, 409, "{problem}");
    assert_eq!(problem["code"], "unknown_key", "{problem}");
    assert!(adapter_path.exists());

    let archive_signature = owner_key.sign_submission("operator/agent", &archive_payload);
    let archive = serde_json::json!({
        "repo": "agents/demo", "name": "adapter-1", "change": "change-1",
        "idempotency_key": "request-1", "channel": "operator/agent",
        "payload_hex": hex_encode(&archive_payload), "key_id": archive_signature.key_id,
        "signature_hex": hex_encode(&archive_signature.signature),
    })
    .to_string();
    let (code, archived) = api(port, "POST", "/api/workspace/archive", Some(&archive));
    assert_eq!(code, 200, "{archived}");
    assert_eq!(archived["already_archived"], false);
    let archived_path = std::path::PathBuf::from(archived["archived_path"].as_str().unwrap());
    assert!(!adapter_path.exists());
    assert_eq!(
        std::fs::read_to_string(archived_path.join("unpublished.txt")).unwrap(),
        "keep me\n"
    );
    let (_, view) = api(port, "GET", "/api/view", None);
    assert!(view["workspaces"].get("agents/demo/adapter-1").is_none());
    assert!(view["changes"]["change-1"]["active_workspace"].is_null());
    let (code, archived_again) = api(port, "POST", "/api/workspace/archive", Some(&archive));
    assert_eq!(code, 200, "{archived_again}");
    assert_eq!(archived_again["already_archived"], true);
    assert_eq!(
        archived_again["operation"]["seq"],
        archived["operation"]["seq"]
    );
    assert_eq!(
        archived_again["operation"]["hash"],
        archived["operation"]["hash"]
    );
    assert_eq!(archived_again["operation"]["already_applied"], true);

    // Simulate the original single-generation archive layout. Reopening
    // below must migrate it into the versioned directory without losing
    // data before archiving the successor generation.
    let archive_root = archived_path.parent().unwrap().to_path_buf();
    let legacy_staging = archive_root
        .parent()
        .unwrap()
        .join("adapter-1-legacy-staging");
    std::fs::rename(&archived_path, &legacy_staging).unwrap();
    std::fs::remove_dir(&archive_root).unwrap();
    std::fs::rename(&legacy_staging, &archive_root).unwrap();
    assert!(archive_root.join(".git").exists());

    // A later logical generation may reuse Symphony's deterministic
    // issue workspace name. Each generation gets a distinct recoverable
    // archive path, so the second terminal cleanup cannot collide with
    // the first one's retained dirty state.
    let reopened_create = signed_create_body(
        &owner_key,
        "agents/demo",
        "adapter-1",
        &head2,
        "operator/agent",
        "change-1-reopened",
        "request-1-reopened",
    );
    let (code, reopened) = api(port, "POST", "/api/workspace", Some(&reopened_create));
    assert_eq!(code, 200, "{reopened}");
    assert_eq!(reopened["created"], true);
    let reopened_path = std::path::PathBuf::from(reopened["path"].as_str().unwrap());
    assert!(reopened_path.exists());
    let reopened_authorization = ArchiveAuthorization::new(
        "change-1-reopened".into(),
        "agents/demo/adapter-1".into(),
        choir_oplog::ContentHash::from_git_oid(&head2).unwrap(),
    )
    .to_payload();
    let reopened_signature = owner_key.sign_submission("operator/agent", &reopened_authorization);
    let reopened_archive = serde_json::json!({
        "repo": "agents/demo", "name": "adapter-1", "change": "change-1-reopened",
        "idempotency_key": "request-1-reopened", "channel": "operator/agent",
        "payload_hex": hex_encode(&reopened_authorization),
        "key_id": reopened_signature.key_id,
        "signature_hex": hex_encode(&reopened_signature.signature),
    })
    .to_string();
    let (code, reopened_archived) = api(
        port,
        "POST",
        "/api/workspace/archive",
        Some(&reopened_archive),
    );
    assert_eq!(code, 200, "{reopened_archived}");
    let reopened_archived_path =
        std::path::PathBuf::from(reopened_archived["archived_path"].as_str().unwrap());
    assert_ne!(reopened_archived_path, archived_path);
    assert!(archived_path.exists());
    assert!(reopened_archived_path.exists());

    // Identical concurrent lifecycle requests converge on one durable
    // change and one physical workspace.
    let idem_body = signed_create_body(
        &owner_key,
        "agents/demo",
        "adapter-racer",
        &head2,
        "operator/racer",
        "change-racer",
        "request-racer",
    );
    let lifecycle_handles: Vec<_> = (0..8)
        .map(|_| {
            let body = idem_body.clone();
            std::thread::spawn(move || api(port, "POST", "/api/workspace", Some(&body)))
        })
        .collect();
    let lifecycle_results: Vec<_> = lifecycle_handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    assert!(
        lifecycle_results.iter().all(|(code, _)| *code == 200),
        "{lifecycle_results:?}"
    );
    assert_eq!(
        lifecycle_results
            .iter()
            .filter(|(_, body)| body["created"] == true)
            .count(),
        1,
        "{lifecycle_results:?}"
    );
    assert_eq!(
        lifecycle_results
            .iter()
            .filter(|(_, body)| body["reused"] == true)
            .count(),
        7,
        "{lifecycle_results:?}"
    );

    // Concurrent provisioning: 8 parallel requests on one repo — the
    // per-repo lock must serialize template refresh, so every workspace
    // comes out whole. Two of them race for the same name: exactly one
    // may win.
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let name = if i < 2 { "racer".to_string() } else { format!("conc-{i}") };
            std::thread::spawn(move || {
                let body = format!(r#"{{"repo":"agents/demo","name":"{name}"}}"#);
                api(port, "POST", "/api/workspace", Some(&body))
            })
        })
        .collect();
    let results: Vec<(u16, serde_json::Value)> =
        handles.into_iter().map(|h| h.join().unwrap()).collect();
    let ok: Vec<_> = results.iter().filter(|(code, _)| *code == 200).collect();
    assert_eq!(ok.len(), 7, "6 unique + 1 racer must win: {results:?}");
    assert_eq!(results.iter().filter(|(code, _)| *code == 409).count(), 1);
    for (_, resp) in &ok {
        let p = std::path::PathBuf::from(resp["path"].as_str().unwrap());
        assert_eq!(std::fs::read_to_string(p.join("f.txt")).unwrap(), "v2\n",
            "no torn copies under concurrency");
    }

    // Small provisioning sample: p50 copy time over 9 more workspaces.
    let mut times: Vec<f64> = (0..9)
        .map(|i| {
            let body = format!(r#"{{"repo":"agents/demo","name":"bench-{i}"}}"#);
            let (code, resp) = api(port, "POST", "/api/workspace", Some(&body));
            assert_eq!(code, 200, "{resp}");
            resp["copy_ms"].as_f64().unwrap()
        })
        .collect();
    times.sort_by(f64::total_cmp);
    println!("workspace copy p50: {:.2} ms (n=9)", times[4]);

    // The template must have git's own housekeeping switched off. It is
    // the directory `cp` walks, and auto-gc or `maintenance run --auto`
    // deleting a lock file mid-walk is a hard `cp` error and a spurious
    // `500` for whoever asked for the workspace. Observed once as a
    // flake; the config is what stops it happening again.
    let template = root.join(".choir/checkouts/agents/demo");
    for (key, want) in [("gc.auto", "0"), ("maintenance.auto", "false")] {
        let got = String::from_utf8_lossy(
            &git(&template, &["config", "--local", "--get", key]).stdout,
        )
        .trim()
        .to_string();
        assert_eq!(got, want, "template has {key} unset, so housekeeping can race the copy");
    }

    // ...and a template created before that config existed must still
    // provision, because the protection also rides on the command line
    // of every git this module runs against a template. Stripping the
    // persisted keys is exactly what an older template looks like.
    for key in ["gc.auto", "maintenance.auto"] {
        assert!(
            git(&template, &["config", "--local", "--unset", key]).status.success(),
            "could not strip {key} to simulate an older template"
        );
    }
    let (code, resp) = api(port, "POST", "/api/workspace",
        Some(r#"{"repo":"agents/demo","name":"legacy-template"}"#));
    assert_eq!(code, 200, "a template without the persisted config failed to provision: {resp}");

    node.unblock();
}
