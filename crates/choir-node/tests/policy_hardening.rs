//! L2 admission-policy hardening: trusted-keys hot-reload (register a
//! key by appending a line, no restart) and reviewer↔channel binding
//! (a verdict's claimed reviewer must be the signed submission channel).

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
fn keys_hot_reload_and_reviewer_binding() {
    let work = std::env::temp_dir().join(format!("choir-node-policy-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let keys_file = work.join("keys.txt");
    std::fs::write(&keys_file, "").unwrap();

    let late = ActorKey::generate();
    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
    node.enable_platform(
        Platform::start_reloading(
            Registry::new(),
            Box::new(MemLog::new()),
            ActorKey::generate(),
            Some(keys_file.clone()),
        )
        .unwrap(),
    );
    let port = node.port();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    let api = format!("http://127.0.0.1:{port}/api");

    let target = choir_oplog::ContentHash::blake3(b"hardening target");
    let request = ViewOp::new(OpKind::RequestReview {
        id: "h-1".into(),
        target,
        reviewers: vec!["late".into()],
        target_ref: None,
    });

    // Unregistered key: rejected.
    let (code, resp) = curl(&[
        "-X", "POST", "-d", &submit_body(&late, "late", &request),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 400);
    assert_eq!(resp["code"], "unknown_key", "{resp}");

    // Operator appends the key line; mtime must move for the reload
    // check, so nudge it past filesystem timestamp granularity.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(&keys_file, format!("{}\n", hex_encode(&late.public_key_bytes()))).unwrap();

    // Same submission now admitted — no restart happened.
    let (code, resp) = curl(&[
        "-X", "POST", "-d", &submit_body(&late, "late", &request),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 200, "{resp}");

    // Reviewer binding: signed by a registered key, but the op claims a
    // different reviewer than the submission channel — rejected before
    // it reaches view semantics.
    let spoof = ViewOp::new(OpKind::PostVerdict {
        id: "h-1".into(),
        reviewer: "late".into(),
        verdict: Verdict::Approve,
        note: String::new(),
    });
    let (code, resp) = curl(&[
        "-X", "POST", "-d", &submit_body(&late, "someone-else", &spoof),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 400);
    assert!(
        resp["code"] == "reviewer_mismatch",
        "{resp}"
    );

    // The honest verdict (channel == reviewer) is admitted.
    let (code, resp) = curl(&[
        "-X", "POST", "-d", &submit_body(&late, "late", &spoof),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 200, "{resp}");
    let (_, view) = curl(&[&format!("{api}/view")]);
    assert_eq!(view["reviews"]["h-1"]["approved"], true, "{view}");

    // Revocation is a tightening too. Removing a trusted key must take
    // effect on that key's very next submission, without waiting for an
    // unrelated bad signature to make the policy refresh its registry.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(&keys_file, "").unwrap();
    let revoked_request = ViewOp::new(OpKind::RequestReview {
        id: "h-revoked".into(),
        target: choir_oplog::ContentHash::blake3(b"revoked target"),
        reviewers: vec!["late".into()],
        target_ref: None,
    });
    let (code, resp) = curl(&[
        "-X", "POST", "-d", &submit_body(&late, "late", &revoked_request),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 400, "{resp}");
    assert_eq!(resp["code"], "unknown_key", "{resp}");
    let (_, view) = curl(&[&format!("{api}/view")]);
    assert!(view["reviews"].get("h-revoked").is_none(), "{view}");

    node.unblock();
}
