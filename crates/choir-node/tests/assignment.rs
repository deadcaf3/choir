//! Reviewer assignment (D24 layer 5): a requester who names no
//! reviewers gets a node-drawn pair from the operator's pool, never
//! themselves; nobody but the node may assign; and a review that could
//! not be assigned stays visibly unassigned rather than reading as done.
//! Plus the per-ref form of the same rule: a review that says it wants
//! to land on a protected ref must go through the draw.

use choir_identity::{ActorKey, Registry};
use choir_node::platform::hex_encode;
use choir_node::{Node, Platform};
use choir_oplog::MemLog;
use choir_view::{OpKind, ViewOp};

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

fn submit_body(key: &ActorKey, workspace: &str, op: &ViewOp) -> String {
    let payload = op.to_payload();
    let sig = key.sign_submission(workspace, &payload);
    serde_json::json!({
        "workspace": workspace,
        "payload_hex": hex_encode(&payload),
        "key_id": sig.key_id,
        "signature_hex": hex_encode(&sig.signature),
    })
    .to_string()
}

fn request(id: &str, seed: &[u8]) -> ViewOp {
    ViewOp::new(OpKind::RequestReview {
        id: id.into(),
        target: choir_oplog::ContentHash::blake3(seed),
        reviewers: Vec::new(),
        target_ref: None,
    })
}

#[test]
fn the_node_assigns_reviewers_and_nobody_else_can() {
    let work = std::env::temp_dir().join(format!("choir-node-assign-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let pool_file = work.join("reviewers");

    let author = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&author.public_key_bytes()).unwrap();

    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate())
            .unwrap()
            .with_reviewer_pool(pool_file.clone()),
    );
    let port = node.port();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    let api = format!("http://127.0.0.1:{port}/api");

    // Pool file missing: the request still lands, but unassigned — and
    // an unassigned review is never complete, so "ask nobody" is not a
    // way to look approved.
    let (code, resp) = curl(&[
        "-X", "POST", "-d", &submit_body(&author, "carol", &request("r-none", b"a")),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 200, "{resp}");
    assert!(resp["assignment_error"].is_string(), "{resp}");
    let (_, view) = curl(&[&format!("{api}/view")]);
    assert_eq!(view["reviews"]["r-none"]["reviewers"], serde_json::json!([]));
    assert_eq!(view["reviews"]["r-none"]["complete"], false, "{view}");
    assert_eq!(view["reviews"]["r-none"]["approved"], false);

    // The requester cannot assign their own reviewers, even on a review
    // that is genuinely unassigned.
    let forged = ViewOp::new(OpKind::AssignReviewers {
        id: "r-none".into(),
        reviewers: vec!["carol".into()],
    });
    let (code, resp) = curl(&[
        "-X", "POST", "-d", &submit_body(&author, "carol", &forged),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 400, "{resp}");
    assert!(
        resp["error"].as_str().unwrap().contains("only the node may assign"),
        "{resp}"
    );

    // With a pool, the draw excludes the requester and takes two.
    std::fs::write(&pool_file, "# eligible reviewers\nana\nbot\ncarol\ndan\n").unwrap();
    let (code, resp) = curl(&[
        "-X", "POST", "-d", &submit_body(&author, "carol", &request("r-1", b"b")),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 200, "{resp}");
    let drawn: Vec<String> = serde_json::from_value(resp["reviewers"].clone()).expect("reviewers");
    assert_eq!(drawn.len(), 2, "{resp}");
    assert!(!drawn.contains(&"carol".to_string()), "requester drew themselves: {resp}");
    for r in &drawn {
        assert!(["ana", "bot", "dan"].contains(&r.as_str()), "{resp}");
    }

    // The assignment is in the view, and the drawn reviewers have it in
    // their pending queues.
    let (_, view) = curl(&[&format!("{api}/view")]);
    let r1 = &view["reviews"]["r-1"];
    assert_eq!(r1["reviewers"], serde_json::json!(drawn), "{view}");
    assert_eq!(r1["complete"], false);
    let (_, pending) = curl(&[&format!("{api}/reviews?reviewer={}", drawn[0])]);
    assert!(pending["pending"].get("r-1").is_some(), "{pending}");

    // Assignment happens once: a second (even node-authored) assignment
    // cannot swap the reviewers out from under a live review. Here the
    // requester tries it again and is refused on authorship first.
    let forged = ViewOp::new(OpKind::AssignReviewers {
        id: "r-1".into(),
        reviewers: vec!["ana".into()],
    });
    let (code, _) = curl(&[
        "-X", "POST", "-d", &submit_body(&author, "carol", &forged),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 400);

    // A pool containing only the requester leaves the review unassigned
    // rather than assigning them to themselves.
    std::fs::write(&pool_file, "carol\n").unwrap();
    let (code, resp) = curl(&[
        "-X", "POST", "-d", &submit_body(&author, "carol", &request("r-2", b"c")),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 200, "{resp}");
    // Exclusion is by operator now, and an unprefixed name is its own
    // operator, so a pool of only the requester still assigns nobody.
    assert!(
        resp["assignment_error"].as_str().unwrap().contains("nobody outside"),
        "{resp}"
    );

    // Draws vary: over many requests from a 3-candidate pool, more than
    // one pair must come up (a constant draw would be a fixed reviewer
    // ring, which is the thing this is meant to prevent).
    std::fs::write(&pool_file, "ana\nbot\ndan\n").unwrap();
    let mut seen = std::collections::BTreeSet::new();
    for i in 0..12 {
        let id = format!("r-var-{i}");
        let (code, resp) = curl(&[
            "-X", "POST", "-d",
            &submit_body(&author, "carol", &request(&id, id.as_bytes())),
            &format!("{api}/submit"),
        ]);
        assert_eq!(code, 200, "{resp}");
        seen.insert(resp["reviewers"].to_string());
    }
    assert!(seen.len() > 1, "assignment never varied: {seen:?}");

    node.unblock();
}

#[test]
fn required_assignment_refuses_self_named_reviewers() {
    let work = std::env::temp_dir().join(format!("choir-node-required-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let pool_file = work.join("reviewers");
    std::fs::write(&pool_file, "ana\nbot\n").unwrap();

    let author = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&author.public_key_bytes()).unwrap();

    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate())
            .unwrap()
            .with_reviewer_pool(pool_file)
            .with_required_assignment(),
    );
    let port = node.port();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    let api = format!("http://127.0.0.1:{port}/api");

    // Picking your own reviewer is refused outright, not quietly
    // overridden — the requester learns why.
    let self_named = ViewOp::new(OpKind::RequestReview {
        id: "r-self".into(),
        target: choir_oplog::ContentHash::blake3(b"x"),
        reviewers: vec!["ana".into()],
        target_ref: None,
    });
    let (code, resp) = curl(&[
        "-X", "POST", "-d", &submit_body(&author, "carol", &self_named),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 400, "{resp}");
    assert!(
        resp["error"].as_str().unwrap().contains("empty reviewer list"),
        "{resp}"
    );
    // Rejected in the policy, so nothing landed in the view.
    let (_, view) = curl(&[&format!("{api}/view")]);
    assert!(view["reviews"].get("r-self").is_none(), "{view}");

    // The assigned path still works.
    let (code, resp) = curl(&[
        "-X", "POST", "-d", &submit_body(&author, "carol", &request("r-ok", b"y")),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 200, "{resp}");
    assert_eq!(resp["reviewers"].as_array().map(Vec::len), Some(2), "{resp}");

    node.unblock();
}

/// A review that declares where it wants to land.
fn request_bound(id: &str, reviewers: &[&str], target_ref: Option<&str>) -> ViewOp {
    ViewOp::new(OpKind::RequestReview {
        id: id.into(),
        target: choir_oplog::ContentHash::blake3(id.as_bytes()),
        reviewers: reviewers.iter().map(|r| (*r).to_string()).collect(),
        target_ref: target_ref.map(str::to_string),
    })
}

#[test]
fn protected_refs_gate_self_named_reviewers_per_ref() {
    let work = std::env::temp_dir().join(format!("choir-node-protected-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let pool_file = work.join("reviewers");
    let refs_file = work.join("protected");
    std::fs::write(&pool_file, "ana\nbot\n").unwrap();
    std::fs::write(
        &refs_file,
        "# what counts as privilege-bearing here\ndemo.git:refs/heads/main\ndemo.git:refs/heads/release/*\n",
    )
    .unwrap();

    let author = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&author.public_key_bytes()).unwrap();

    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate())
            .unwrap()
            .with_reviewer_pool(pool_file)
            .with_protected_refs(refs_file.clone()),
    );
    let port = node.port();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    let api = format!("http://127.0.0.1:{port}/api");
    let post = |op: &ViewOp| {
        curl(&[
            "-X", "POST", "-d", &submit_body(&author, "carol", op),
            &format!("{api}/submit"),
        ])
    };

    // Landing on a protected ref: picking your own reviewer is refused,
    // and the error names the ref so the requester knows why this one.
    let (code, resp) = post(&request_bound(
        "p-main",
        &["ana"],
        Some("demo.git:refs/heads/main"),
    ));
    assert_eq!(code, 400, "{resp}");
    assert!(
        resp["error"].as_str().unwrap().contains("protected ref"),
        "{resp}"
    );

    // Trailing `*` is a prefix glob, so a whole release namespace is
    // covered by one line.
    let (code, resp) = post(&request_bound(
        "p-rel",
        &["ana"],
        Some("demo.git:refs/heads/release/1.0"),
    ));
    assert_eq!(code, 400, "{resp}");

    // An unprotected ref is still self-service: the gate is per-ref, not
    // node-wide (that is what `--require-assignment` is for).
    let (code, resp) = post(&request_bound(
        "p-scratch",
        &["ana"],
        Some("demo.git:refs/heads/scratch"),
    ));
    assert_eq!(code, 200, "{resp}");

    // Known, documented escape: a review that names no ref is unbound,
    // so there is nothing to match. Asserted so it stays a decision
    // rather than becoming a surprise — closing it needs SetRef itself
    // to require an approved review.
    let (code, resp) = post(&request_bound("p-unbound", &["ana"], None));
    assert_eq!(code, 200, "{resp}");

    // The assigned path works on a protected ref, and the view carries
    // the binding so a reader can see what the review is for.
    let (code, resp) = post(&request_bound("p-ok", &[], Some("demo.git:refs/heads/main")));
    assert_eq!(code, 200, "{resp}");
    assert_eq!(resp["reviewers"].as_array().map(Vec::len), Some(2), "{resp}");
    let (_, view) = curl(&[&format!("{api}/view")]);
    assert_eq!(
        view["reviews"]["p-ok"]["target_ref"], "demo.git:refs/heads/main",
        "{view}"
    );
    assert_eq!(view["reviews"]["p-unbound"]["target_ref"], serde_json::Value::Null);

    // Editing the list takes effect with no restart.
    std::fs::write(&refs_file, "demo.git:refs/heads/scratch\n").unwrap();
    let (code, resp) = post(&request_bound(
        "p-reload",
        &["ana"],
        Some("demo.git:refs/heads/scratch"),
    ));
    assert_eq!(code, 400, "{resp}");

    // Fail closed: an unreadable list refuses the review rather than
    // quietly demoting the gate to an advisory.
    std::fs::remove_file(&refs_file).unwrap();
    let (code, resp) = post(&request_bound(
        "p-gone",
        &["ana"],
        Some("demo.git:refs/heads/main"),
    ));
    assert_eq!(code, 400, "{resp}");
    assert!(
        resp["error"].as_str().unwrap().contains("unreadable"),
        "{resp}"
    );

    node.unblock();
}

#[test]
fn the_draw_excludes_the_requesters_whole_operator_not_just_their_name() {
    // The multi-operator case, which is the normal one. Excluding only
    // the requester's own name lets an operator running several agents
    // satisfy two-person integrity by themselves, and manufacture more
    // agreement by registering more agents — the Sybil move D24 says must
    // be blocked at the operator level.
    let work = std::env::temp_dir().join(format!("choir-node-operator-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let pool_file = work.join("reviewers");

    let author = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&author.public_key_bytes()).unwrap();

    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate())
            .unwrap()
            .with_reviewer_pool(pool_file.clone()),
    );
    let port = node.port();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    let api = format!("http://127.0.0.1:{port}/api");
    let post = |channel: &str, op: &ViewOp| {
        curl(&[
            "-X", "POST", "-d", &submit_body(&author, channel, op),
            &format!("{api}/submit"),
        ])
    };

    // alice runs three agents; bob and carol run one each.
    std::fs::write(
        &pool_file,
        "# operator/agent\nalice/one\nalice/two\nalice/three\nbob/one\ncarol/one\n",
    )
    .unwrap();

    // Twenty draws from alice: never a sibling, and never two from the
    // same operator. One run could pass by luck; twenty could not.
    for i in 0..20 {
        let id = format!("op-{i}");
        let (code, resp) = post("alice/two", &request(&id, id.as_bytes()));
        assert_eq!(code, 200, "{resp}");
        let drawn: Vec<String> =
            serde_json::from_value(resp["reviewers"].clone()).expect("reviewers");
        assert_eq!(drawn.len(), 2, "{resp}");
        for r in &drawn {
            assert!(
                !r.starts_with("alice/"),
                "drew the requester's own operator: {drawn:?}"
            );
        }
        let operators: std::collections::BTreeSet<&str> =
            drawn.iter().map(|r| r.split('/').next().unwrap()).collect();
        assert_eq!(operators.len(), 2, "two seats, one operator: {drawn:?}");
    }

    // Only one other operator available: draw one rather than two from
    // the same operator. Under-assigned and visible beats independent-
    // looking and false.
    std::fs::write(&pool_file, "alice/one\nalice/two\nbob/one\nbob/two\n").unwrap();
    let (code, resp) = post("alice/one", &request("op-thin", b"thin"));
    assert_eq!(code, 200, "{resp}");
    // One seat, and it is bob's — but *which* of bob's agents is a draw,
    // not a property. Asserting the sample rather than the property is
    // how a test passes on one seed and fails on the next.
    let drawn: Vec<String> = serde_json::from_value(resp["reviewers"].clone()).expect("reviewers");
    assert_eq!(drawn.len(), 1, "should not double up on bob: {resp}");
    assert!(drawn[0].starts_with("bob/"), "{resp}");

    // A pool that is entirely the requester's operator assigns nobody,
    // and says why.
    std::fs::write(&pool_file, "alice/one\nalice/two\n").unwrap();
    let (code, resp) = post("alice/one", &request("op-none", b"none"));
    assert_eq!(code, 200, "{resp}");
    assert!(
        resp["assignment_error"].as_str().unwrap().contains("alice"),
        "{resp}"
    );

    // Unprefixed names keep behaving as before: each is its own operator,
    // so a flat pool is unchanged by namespacing existing.
    std::fs::write(&pool_file, "ana\nbot\ncarol\n").unwrap();
    let (code, resp) = post("carol", &request("op-flat", b"flat"));
    assert_eq!(code, 200, "{resp}");
    let drawn: Vec<String> = serde_json::from_value(resp["reviewers"].clone()).expect("reviewers");
    assert_eq!(drawn, ["ana", "bot"], "{resp}");

    node.unblock();
}
