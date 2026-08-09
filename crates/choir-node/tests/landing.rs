//! The landing gate (D24 layer 5, the enforcement half): with
//! `--protected-refs` plus `--require-review`, a protected ref only moves
//! to a commit a review with two independent approvals already named as
//! its destination, and cannot be deleted at all. A real `git push` is
//! what gets refused — the gate lives in admission policy, so there is no
//! path around it.

use choir_identity::{ActorKey, Registry};
use choir_node::platform::hex_encode;
use choir_node::{Node, Platform};
use choir_oplog::MemLog;
use choir_view::{OpKind, Verdict, ViewOp};

fn git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args(["-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main"])
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

fn curl(args: &[&str]) -> (u16, serde_json::Value) {
    let out = std::process::Command::new("curl")
        .args(["-s", "-w", "\n%{http_code}"])
        .args(args)
        .output()
        .expect("curl runs");
    let text = String::from_utf8_lossy(&out.stdout);
    let (body, code) = text.rsplit_once('\n').expect("status line");
    (
        code.trim().parse().expect("numeric status"),
        serde_json::from_str(body).expect("json body"),
    )
}

fn submit_body(key: &ActorKey, channel: &str, op: &ViewOp) -> String {
    let payload = op.to_payload();
    let sig = key.sign_submission(channel, &payload);
    serde_json::json!({
        "workspace": channel,
        "payload_hex": hex_encode(&payload),
        "key_id": sig.key_id,
        "signature_hex": hex_encode(&sig.signature),
    })
    .to_string()
}

/// Commits `content` and returns the new HEAD oid.
fn commit(dir: &std::path::Path, content: &str, message: &str) -> String {
    std::fs::write(dir.join("f.txt"), content).unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", message]);
    String::from_utf8(git(dir, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_string()
}

#[test]
fn a_protected_ref_only_moves_to_a_commit_an_approved_review_named() {
    let work = std::env::temp_dir().join(format!("choir-landing-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let pool_file = work.join("reviewers");
    let refs_file = work.join("protected");
    std::fs::write(&pool_file, "ana\nbot\n").unwrap();
    std::fs::write(&refs_file, "agents/demo.git:refs/heads/main\n").unwrap();

    let author = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&author.public_key_bytes()).unwrap();

    // The node's own key, kept so the test can sign the ops only the node
    // is allowed to author (assignment, archiving).
    let node_secret = ActorKey::generate().secret_bytes();
    let node_key = ActorKey::from_secret_bytes(&node_secret);

    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), node_key)
            .unwrap()
            .with_reviewer_pool(pool_file)
            .with_protected_refs(refs_file)
            .with_required_review(),
    );
    let port = node.port();
    node.create_repo("agents/demo.git").unwrap();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    let url = format!("http://127.0.0.1:{port}/agents/demo.git");
    let api = format!("http://127.0.0.1:{port}/api");
    let refs = || curl(&[&format!("{api}/view")]).1["refs"].clone();

    let clone = work.join("clone");
    assert!(git(&work, &["clone", "-q", &url, clone.to_str().unwrap()])
        .status
        .success());

    // Creating a protected ref is allowed: no history exists yet to
    // hijack, and deletion is refused below, so "delete then re-create"
    // is not a way back in.
    let c1 = commit(&clone, "one\n", "first");
    let out = git(&clone, &["push", "-q", "origin", "HEAD:main"]);
    assert!(
        out.status.success(),
        "creating a protected ref should be allowed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(refs()["agents/demo.git:refs/heads/main"], format!("11-{c1}"));

    // Advancing it without an approved review is refused, and the refusal
    // is real: the ref did not move.
    let c2 = commit(&clone, "two\n", "second");
    assert!(
        !git(&clone, &["push", "-q", "origin", "HEAD:main"]).status.success(),
        "unreviewed push to a protected ref should be refused"
    );
    assert_eq!(
        refs()["agents/demo.git:refs/heads/main"],
        format!("11-{c1}"),
        "refused push must not have moved the ref"
    );

    // An unprotected ref is unaffected: the gate is per-ref.
    assert!(git(&clone, &["push", "-q", "origin", "HEAD:feature"]).status.success());

    // Open a review that says where it wants to land. No reviewers named,
    // so the node draws them — on a protected ref that is the only form
    // `--protected-refs` accepts, which is how the two halves compose.
    let review = |id: &str, oid: &str, target_ref: &str| {
        ViewOp::new(OpKind::RequestReview {
            id: id.into(),
            target: choir_oplog::ContentHash::from_git_oid(oid).expect("git oid"),
            reviewers: Vec::new(),
            target_ref: Some(target_ref.into()),
        })
    };
    let (code, resp) = curl(&[
        "-X", "POST", "-d",
        &submit_body(&author, "carol", &review("land-1", &c2, "agents/demo.git:refs/heads/main")),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 200, "{resp}");
    let drawn: Vec<String> = serde_json::from_value(resp["reviewers"].clone()).expect("reviewers");

    // A review that is open but unanswered does not authorize anything.
    assert!(
        !git(&clone, &["push", "-q", "origin", "HEAD:main"]).status.success(),
        "an unapproved review must not authorize a landing"
    );

    // Approve it. Each verdict is signed on the reviewer's own channel,
    // which is what admission policy binds it to. (One key signs for both
    // names here: name→key binding is L8 and does not exist yet — the
    // same known gap the reviewer pool has.)
    for who in &drawn {
        let verdict = ViewOp::new(OpKind::PostVerdict {
            id: "land-1".into(),
            reviewer: who.clone(),
            verdict: Verdict::Approve,
            note: "lgtm".into(),
        });
        let (code, resp) = curl(&[
            "-X", "POST", "-d", &submit_body(&author, who, &verdict),
            &format!("{api}/submit"),
        ]);
        assert_eq!(code, 200, "{resp}");
    }

    // Now the same push lands.
    let out = git(&clone, &["push", "-q", "origin", "HEAD:main"]);
    assert!(
        out.status.success(),
        "approved push should land: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(refs()["agents/demo.git:refs/heads/main"], format!("11-{c2}"));

    // The approval authorized exactly one (ref, commit) pair: it does not
    // carry forward to the next commit.
    let _c3 = commit(&clone, "three\n", "third");
    assert!(
        !git(&clone, &["push", "-q", "origin", "HEAD:main"]).status.success(),
        "an approval must not authorize later commits"
    );

    // Nor does an approval bound to a different ref authorize this one —
    // otherwise a rubber-stamped scratch branch would be a way onto main.
    let c3 = String::from_utf8(git(&clone, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_string();
    let (code, resp) = curl(&[
        "-X", "POST", "-d",
        &submit_body(&author, "carol", &review("land-2", &c3, "agents/demo.git:refs/heads/feature")),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 200, "{resp}");
    let drawn2: Vec<String> = serde_json::from_value(resp["reviewers"].clone()).expect("reviewers");
    for who in &drawn2 {
        let verdict = ViewOp::new(OpKind::PostVerdict {
            id: "land-2".into(),
            reviewer: who.clone(),
            verdict: Verdict::Approve,
            note: String::new(),
        });
        curl(&[
            "-X", "POST", "-d", &submit_body(&author, who, &verdict),
            &format!("{api}/submit"),
        ]);
    }
    assert!(
        !git(&clone, &["push", "-q", "origin", "HEAD:main"]).status.success(),
        "an approval for another ref must not authorize main"
    );
    assert_eq!(
        refs()["agents/demo.git:refs/heads/main"],
        format!("11-{c2}"),
        "main must still be where the one real approval left it"
    );

    // An ARCHIVED approval still authorizes a landing. This is the whole
    // point of keeping (target_ref, target, approved) when a review's
    // verdicts are pruned: retention must not quietly become an expiry
    // policy on approvals.
    let (code, resp) = curl(&[
        "-X", "POST", "-d",
        &submit_body(&author, "carol", &review("land-3", &c3, "agents/demo.git:refs/heads/main")),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 200, "{resp}");
    let drawn3: Vec<String> = serde_json::from_value(resp["reviewers"].clone()).expect("reviewers");
    for who in &drawn3 {
        let verdict = ViewOp::new(OpKind::PostVerdict {
            id: "land-3".into(),
            reviewer: who.clone(),
            verdict: Verdict::Approve,
            note: "ok".into(),
        });
        let (code, resp) = curl(&[
            "-X", "POST", "-d", &submit_body(&author, who, &verdict),
            &format!("{api}/submit"),
        ]);
        assert_eq!(code, 200, "{resp}");
    }

    // Nobody but the node may archive -- otherwise archiving is a way to
    // erase a RequestChanges you did not like.
    let archive = ViewOp::new(OpKind::ArchiveReview { id: "land-3".into(), lapsed: false });
    let (code, resp) = curl(&[
        "-X", "POST", "-d", &submit_body(&author, "carol", &archive),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 400, "{resp}");
    assert!(
        resp["code"] == "node_only" && resp["error"].as_str().unwrap().contains("archive"),
        "{resp}"
    );

    let node_key = ActorKey::from_secret_bytes(&node_secret);
    let (code, resp) = curl(&[
        "-X", "POST", "-d", &submit_body(&node_key, "node/archive", &archive),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 200, "{resp}");
    let (_, view) = curl(&[&format!("{api}/view")]);
    assert_eq!(view["reviews"]["land-3"]["archived"], true, "{view}");
    assert_eq!(view["reviews"]["land-3"]["approved"], true, "{view}");
    assert_eq!(view["reviews"]["land-3"]["verdicts"], serde_json::json!({}), "{view}");

    // ...and the push it authorized still lands, with the verdicts gone.
    let out = git(&clone, &["push", "-q", "origin", "HEAD:main"]);
    assert!(
        out.status.success(),
        "archived approval failed to authorize: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(refs()["agents/demo.git:refs/heads/main"], format!("11-{c3}"));

    // A protected ref cannot be deleted, reviewed or not.
    assert!(
        !git(&clone, &["push", "-q", "origin", ":main"]).status.success(),
        "a protected ref must not be deletable"
    );
    assert_eq!(refs()["agents/demo.git:refs/heads/main"], format!("11-{c3}"));
    assert!(refs()["agents/demo.git:refs/heads/main"].is_string());

    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}

#[test]
fn one_operator_cannot_supply_enough_approval_weight_to_land() {
    let work = std::env::temp_dir().join(format!("choir-landing-cap-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let pool_file = work.join("reviewers");
    let refs_file = work.join("protected");
    // Two agent names, but one operator: the draw correctly yields one
    // seat. That seat must not carry the whole protected-ref threshold.
    std::fs::write(&pool_file, "reviewer/one\nreviewer/two\n").unwrap();
    std::fs::write(&refs_file, "agents/demo.git:refs/heads/main\n").unwrap();

    let author = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&author.public_key_bytes()).unwrap();
    let node_secret = ActorKey::generate().secret_bytes();
    let node_key = ActorKey::from_secret_bytes(&node_secret);

    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), node_key)
            .unwrap()
            .with_reviewer_pool(pool_file)
            .with_protected_refs(refs_file)
            .with_required_review(),
    );
    let port = node.port();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    let api = format!("http://127.0.0.1:{port}/api");
    let submit = |key: &ActorKey, channel: &str, op: &ViewOp| {
        curl(&[
            "-X",
            "POST",
            "-d",
            &submit_body(key, channel, op),
            &format!("{api}/submit"),
        ])
    };
    let target_ref = "agents/demo.git:refs/heads/main";
    let initial = choir_oplog::ContentHash::from_git_oid(&"1".repeat(40)).unwrap();
    let candidate = choir_oplog::ContentHash::from_git_oid(&"2".repeat(40)).unwrap();

    // Creating the protected ref remains allowed.
    let create = ViewOp::new(OpKind::SetRef {
        name: target_ref.into(),
        commit: initial.clone(),
        prev: None,
    });
    assert_eq!(submit(&author, "writer/agent", &create).0, 200);

    let request = ViewOp::new(OpKind::RequestReview {
        id: "thin-review".into(),
        target: candidate.clone(),
        reviewers: Vec::new(),
        target_ref: Some(target_ref.into()),
    });
    let (code, resp) = submit(&author, "writer/agent", &request);
    assert_eq!(code, 200, "{resp}");
    let drawn: Vec<String> = serde_json::from_value(resp["reviewers"].clone()).unwrap();
    assert_eq!(
        drawn.len(),
        1,
        "one operator must get only one seat: {resp}"
    );

    let verdict = ViewOp::new(OpKind::PostVerdict {
        id: "thin-review".into(),
        reviewer: drawn[0].clone(),
        verdict: Verdict::Approve,
        note: String::new(),
    });
    assert_eq!(submit(&author, &drawn[0], &verdict).0, 200);
    let (_, view) = curl(&[&format!("{api}/view")]);
    assert_eq!(
        view["reviews"]["thin-review"]["approved"], true,
        "{view}"
    );
    assert_eq!(
        view["reviews"]["thin-review"]["approval_weight"], 1,
        "{view}"
    );

    let advance = ViewOp::new(OpKind::SetRef {
        name: target_ref.into(),
        commit: candidate.clone(),
        prev: Some(initial),
    });
    let (code, resp) = submit(&author, "writer/agent", &advance);
    assert_eq!(
        code, 400,
        "one approval must not meet a two-person threshold: {resp}"
    );
    assert_eq!(resp["code"], "review_required", "{resp}");
    assert_eq!(resp["actual"], "approval weight 1", "{resp}");
    assert!(
        resp["next"]
            .as_str()
            .unwrap()
            .contains("two distinct operators"),
        "{resp}"
    );

    // Compaction must retain the approval weight as well as the outcome;
    // otherwise dropping the reviewer list would erase the cap.
    let archive = ViewOp::new(OpKind::ArchiveReview {
        id: "thin-review".into(),
        lapsed: false,
    });
    let node_key = ActorKey::from_secret_bytes(&node_secret);
    assert_eq!(submit(&node_key, "node/archive", &archive).0, 200);
    let (code, resp) = submit(&author, "writer/agent", &advance);
    assert_eq!(
        code, 400,
        "archiving must not inflate approval weight: {resp}"
    );
    assert_eq!(resp["code"], "review_required", "{resp}");
    assert_eq!(resp["actual"], "approval weight 1", "{resp}");

    // Two weak reviews do not add together. The threshold belongs to
    // one review; summing rows would let one operator manufacture weight
    // by opening the same one-seat review twice.
    let second_request = ViewOp::new(OpKind::RequestReview {
        id: "thin-review-2".into(),
        target: candidate,
        reviewers: Vec::new(),
        target_ref: Some(target_ref.into()),
    });
    let (code, resp) = submit(&author, "writer/agent", &second_request);
    assert_eq!(code, 200, "{resp}");
    let second_drawn: Vec<String> = serde_json::from_value(resp["reviewers"].clone()).unwrap();
    assert_eq!(second_drawn.len(), 1, "{resp}");
    let second_verdict = ViewOp::new(OpKind::PostVerdict {
        id: "thin-review-2".into(),
        reviewer: second_drawn[0].clone(),
        verdict: Verdict::Approve,
        note: String::new(),
    });
    assert_eq!(submit(&author, &second_drawn[0], &second_verdict).0, 200);
    let (code, resp) = submit(&author, "writer/agent", &advance);
    assert_eq!(code, 400, "separate weak reviews must not combine: {resp}");
    assert_eq!(resp["actual"], "approval weight 1", "{resp}");

    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}
