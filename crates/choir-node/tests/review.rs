//! Review fan-out over the platform API: a signed RequestReview op
//! opens a review, each reviewer's pending queue is served by
//! `/api/reviews?reviewer=`, signed verdicts land, and the view
//! reports completion/approval.

use choir_identity::{ActorKey, Registry};
use choir_node::platform::hex_encode;
use choir_node::{Node, Platform};
use choir_oplog::MemLog;
use choir_view::{OpKind, Verdict, ViewOp};

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
    });
    let (code, resp) = curl(&[
        "-X", "POST", "-d", &submit_body(&author, "author", &request),
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
        "-X", "POST", "-d", &submit_body(&reviewer, "ana", &approve),
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
        "-X", "POST", "-d", &submit_body(&reviewer, "bot", &approve),
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
        "-X", "POST", "-d", &submit_body(&reviewer, "mallory", &intrude),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 400);
    assert!(resp["error"].as_str().unwrap().contains("not a reviewer"), "{resp}");

    node.unblock();
}
