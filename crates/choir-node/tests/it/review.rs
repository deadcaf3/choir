//! Review fan-out over the platform API: a signed RequestReview op
//! opens a review, each reviewer's pending queue is served by
//! `/api/reviews?reviewer=`, signed verdicts land, and the view
//! reports completion/approval.

use choir_identity::{ActorKey, Registry};
use choir_node::{Node, Platform};
use choir_oplog::MemLog;
use choir_view::{OpKind, Verdict, ViewOp};

use crate::support::curl;

use crate::support::submit_body_legacy as submit_body;

#[test]
fn review_fan_out_over_http() {
    let work = std::env::temp_dir().join(format!("choir-node-review-{}", std::process::id()));
    std::fs::create_dir_all(&work).unwrap();

    let author = ActorKey::generate();
    let reviewer = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&author.public_key_bytes()).unwrap();
    registry.register(&reviewer.public_key_bytes()).unwrap();

    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate()).unwrap(),
    );
    let port = node.port();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    let api = format!("http://127.0.0.1:{port}/api");

    // Author opens a review fanning out to two reviewers.
    let target = choir_oplog::ContentHash::blake3(b"change under review");
    let request = ViewOp::new(OpKind::RequestReview {
        id: "r-1".into(),
        target,
        reviewers: vec!["ana".into(), "bot".into()],
        target_ref: None,
    });
    let (code, resp) = curl(&[
        "-X",
        "POST",
        "-d",
        &submit_body(&author, "author", &request),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 200, "{resp}");

    // Both reviewers see it pending; an uninvolved actor sees nothing.
    let (_, pending) = curl(&[&format!("{api}/reviews?reviewer=ana")]);
    assert!(pending["pending"].get("r-1").is_some(), "{pending}");
    let (_, pending) = curl(&[&format!("{api}/reviews?reviewer=nobody")]);
    assert!(pending["pending"].as_object().unwrap().is_empty());

    // Ana approves; her queue empties, bot's still shows it.
    let approve = ViewOp::new(OpKind::PostVerdict {
        id: "r-1".into(),
        reviewer: "ana".into(),
        verdict: Verdict::Approve,
        note: "lgtm".into(),
    });
    let (code, resp) = curl(&[
        "-X",
        "POST",
        "-d",
        &submit_body(&reviewer, "ana", &approve),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 200, "{resp}");
    let (_, pending) = curl(&[&format!("{api}/reviews?reviewer=ana")]);
    assert!(pending["pending"].as_object().unwrap().is_empty());
    let (_, pending) = curl(&[&format!("{api}/reviews?reviewer=bot")]);
    assert!(pending["pending"].get("r-1").is_some());

    // Bot approves; the view reports the review complete and approved.
    let approve = ViewOp::new(OpKind::PostVerdict {
        id: "r-1".into(),
        reviewer: "bot".into(),
        verdict: Verdict::Approve,
        note: "checks pass".into(),
    });
    let (code, _) = curl(&[
        "-X",
        "POST",
        "-d",
        &submit_body(&reviewer, "bot", &approve),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 200);
    let (_, view) = curl(&[&format!("{api}/view")]);
    let r = &view["reviews"]["r-1"];
    assert_eq!(r["complete"], true, "{view}");
    assert_eq!(r["approved"], true);
    assert_eq!(r["verdicts"]["ana"]["note"], "lgtm");

    // A non-listed reviewer's verdict is rejected by the sequencer.
    let intrude = ViewOp::new(OpKind::PostVerdict {
        id: "r-1".into(),
        reviewer: "mallory".into(),
        verdict: Verdict::Approve,
        note: String::new(),
    });
    let (code, resp) = curl(&[
        "-X",
        "POST",
        "-d",
        &submit_body(&reviewer, "mallory", &intrude),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 400);
    assert_eq!(resp["code"], "review_state", "{resp}");
    assert!(
        resp["error"].as_str().unwrap().contains("not a reviewer"),
        "{resp}"
    );

    node.unblock();
}

/// A reviewer's queue answers to the name the reviewer actually has.
///
/// Channels are `operator/agent`, so every real reviewer name carries a
/// slash, and a client that percent-escapes it — the `choir` CLI does —
/// was answered with an empty queue. `choir reviews` therefore told a
/// drawn reviewer there was nothing to do while `choir state` told the
/// same reviewer they were the only thing a change was waiting on. The
/// endpoint's other tests use single-word names, which is why a broken
/// queue read as a working one for as long as it did.
#[test]
fn a_queue_is_found_by_an_escaped_channel_name() {
    let work = std::env::temp_dir().join(format!("choir-node-review-esc-{}", std::process::id()));
    std::fs::create_dir_all(&work).unwrap();

    let author = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&author.public_key_bytes()).unwrap();

    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate()).unwrap(),
    );
    let port = node.port();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    let api = format!("http://127.0.0.1:{port}/api");

    let request = ViewOp::new(OpKind::RequestReview {
        id: "r-escaped".into(),
        target: choir_oplog::ContentHash::blake3(b"change with a real reviewer name"),
        reviewers: vec!["bea/reviewer".into()],
        target_ref: None,
    });
    let (code, resp) = curl(&[
        "-X",
        "POST",
        "-d",
        &submit_body(&author, "ada/agent", &request),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 200, "{resp}");

    for spelling in ["bea/reviewer", "bea%2Freviewer", "bea%2freviewer"] {
        let (_, pending) = curl(&[&format!("{api}/reviews?reviewer={spelling}")]);
        assert!(
            pending["pending"].get("r-escaped").is_some(),
            "queue empty for {spelling}: {pending}"
        );
    }
    // Decoding is not a wildcard: a different reviewer still sees none.
    let (_, pending) = curl(&[&format!("{api}/reviews?reviewer=cai%2Freviewer")]);
    assert!(pending["pending"].as_object().unwrap().is_empty());

    node.unblock();
}

/// Discussion over the same signed path (D38): a comment is an operation,
/// so it is admitted, sequenced and served exactly like a verdict, and
/// the two attribution rules a discussion surface needs are enforced at
/// admission rather than trusted from the payload.
#[test]
fn comments_land_on_a_review_and_carry_their_author() {
    let work =
        std::env::temp_dir().join(format!("choir-node-review-comment-{}", std::process::id()));
    std::fs::create_dir_all(&work).unwrap();

    let author = ActorKey::generate();
    let reviewer = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&author.public_key_bytes()).unwrap();
    registry.register(&reviewer.public_key_bytes()).unwrap();

    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate()).unwrap(),
    );
    let port = node.port();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    let api = format!("http://127.0.0.1:{port}/api");

    let request = ViewOp::new(OpKind::RequestReview {
        id: "r-talk".into(),
        target: choir_oplog::ContentHash::blake3(b"change under discussion"),
        reviewers: vec!["ana".into()],
        target_ref: None,
    });
    let (code, resp) = curl(&[
        "-X",
        "POST",
        "-d",
        &submit_body(&author, "author", &request),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 200, "{resp}");

    let comment = |comment: &str, who: &str, body: &str| {
        ViewOp::new(OpKind::PostComment {
            id: "r-talk".into(),
            comment: comment.into(),
            author: who.into(),
            body: body.into(),
        })
    };

    // The reviewer speaks, and so does the author -- commenting is not
    // restricted to the reviewer list, because the point of a discussion
    // is that the person being reviewed can answer.
    for (key, who, id, body) in [
        (&reviewer, "ana", "c1", "why this base?"),
        (&author, "author", "c2", "it is the merge base"),
    ] {
        let (code, resp) = curl(&[
            "-X",
            "POST",
            "-d",
            &submit_body(key, who, &comment(id, who, body)),
            &format!("{api}/submit"),
        ]);
        assert_eq!(code, 200, "{resp}");
    }

    let (_, view) = curl(&[&format!("{api}/view")]);
    let thread = view["reviews"]["r-talk"]["comments"].as_array().unwrap();
    assert_eq!(thread.len(), 2, "{view}");
    assert_eq!(thread[0]["author"], "ana", "{view}");
    assert_eq!(thread[0]["body"], "why this base?");
    assert_eq!(thread[1]["author"], "author");
    assert!(
        thread[0]["at"].as_u64().unwrap() < thread[1]["at"].as_u64().unwrap(),
        "the thread is not served in the order it was admitted: {view}"
    );

    // Words in somebody else's name: refused, and named as the mismatch
    // it is rather than as a bad signature.
    let forged = comment("c3", "ana", "I withdraw my objection");
    let (code, resp) = curl(&[
        "-X",
        "POST",
        "-d",
        &submit_body(&author, "author", &forged),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 400, "{resp}");
    assert_eq!(resp["code"], "reviewer_mismatch", "{resp}");

    // A comment cannot land twice, and the two layers answer differently
    // on purpose. Identical signed bytes are a lost-response retry: the
    // duplicate index answers 200 with the original seq, and no second
    // comment appears.
    let (code, resp) = curl(&[
        "-X",
        "POST",
        "-d",
        &submit_body(&reviewer, "ana", &comment("c1", "ana", "why this base?")),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 200, "{resp}");
    assert_eq!(resp["already_applied"], true, "{resp}");

    // Different bytes reusing the id reach the fold, which is the layer
    // that stays true after the duplicate window turns over and is the
    // only one a replaying reader has. `seq` and `parent` are assigned
    // after signing, so this is where the defence has to live.
    let (code, resp) = curl(&[
        "-X",
        "POST",
        "-d",
        &submit_body(&reviewer, "ana", &comment("c1", "ana", "and again")),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 400, "{resp}");
    assert_eq!(resp["code"], "review_state", "{resp}");
    assert!(
        resp["error"].as_str().unwrap().contains("already exists"),
        "{resp}"
    );
    let (_, view) = curl(&[&format!("{api}/view")]);
    assert_eq!(
        view["reviews"]["r-talk"]["comments"]
            .as_array()
            .unwrap()
            .len(),
        2,
        "a refused comment still reached the thread: {view}"
    );

    // And a comment on a review that does not exist says so.
    let (code, resp) = curl(&[
        "-X",
        "POST",
        "-d",
        &submit_body(
            &reviewer,
            "ana",
            &ViewOp::new(OpKind::PostComment {
                id: "no-such-review".into(),
                comment: "c1".into(),
                author: "ana".into(),
                body: "hello?".into(),
            }),
        ),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 400, "{resp}");
    assert!(
        resp["error"].as_str().unwrap().contains("no such review"),
        "{resp}"
    );

    node.unblock();
}
