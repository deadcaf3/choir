//! Platform API acceptance over real HTTP (curl): signed submit
//! admitted, unknown key and stale CAS rejected with reasons, view
//! endpoint reflects exactly the admitted ops.

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

#[test]
fn signed_submit_and_view_over_http() {
    let work = std::env::temp_dir().join(format!("choir-node-api-{}", std::process::id()));
    std::fs::create_dir_all(&work).unwrap();

    let alice = ActorKey::generate();
    let mallory = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&alice.public_key_bytes()).unwrap();

    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
    node.enable_platform(Platform::start(registry, Box::new(MemLog::new())).unwrap());
    let port = node.port();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    let api = format!("http://127.0.0.1:{port}/api");

    let commit = choir_oplog::ContentHash::blake3(b"api demo commit");
    let op = ViewOp::new(OpKind::SetRef {
        name: "main".into(),
        commit: commit.clone(),
        prev: None,
    });

    // Alice's signed op is admitted at seq 0.
    let (code, resp) = curl(&[
        "-X", "POST", "-d", &submit_body(&alice, "alice", &op),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 200, "{resp}");
    assert_eq!(resp["seq"], 0);

    // Mallory's unregistered key is rejected.
    let (code, resp) = curl(&[
        "-X", "POST", "-d", &submit_body(&mallory, "mallory", &op),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 400);
    assert!(resp["error"].as_str().unwrap().contains("bad signature"));

    // Alice replaying the same op (prev=None, but main now exists): stale.
    let (code, resp) = curl(&[
        "-X", "POST", "-d", &submit_body(&alice, "alice", &op),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 400);
    assert!(resp["error"].as_str().unwrap().contains("stale"));

    // The view shows exactly the admitted state.
    let (code, view) = curl(&[&format!("{api}/view")]);
    assert_eq!(code, 200);
    assert_eq!(view["refs"]["main"], commit.to_hex());
    assert!(view["workspaces"].as_object().unwrap().is_empty());

    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}

#[test]
fn platform_state_survives_restart() {
    let work = std::env::temp_dir().join(format!("choir-node-restart-{}", std::process::id()));
    std::fs::create_dir_all(&work).unwrap();
    let log_path = work.join("ops.jsonl");

    let alice = ActorKey::generate();
    let commit = choir_oplog::ContentHash::blake3(b"persisted commit");
    let op = ViewOp::new(OpKind::SetRef {
        name: "main".into(),
        commit: commit.clone(),
        prev: None,
    });

    // First "daemon run": admit one signed op into a FileLog.
    {
        let mut registry = Registry::new();
        registry.register(&alice.public_key_bytes()).unwrap();
        let log = choir_oplog::FileLog::open(&log_path).unwrap();
        let platform = Platform::start(registry, Box::new(log)).unwrap();
        let payload = op.to_payload();
        let sig = alice.sign_submission("alice", &payload);
        let (status, _) = platform.handle_api(
            "POST",
            "/api/submit",
            serde_json::json!({
                "workspace": "alice",
                "payload_hex": hex_encode(&payload),
                "key_id": sig.key_id,
                "signature_hex": hex_encode(&sig.signature),
            })
            .to_string()
            .as_bytes(),
        );
        assert_eq!(status, 200);
    }

    // Second run over the same file: the view is already there, and the
    // CAS state carried over (replaying the old op is stale now).
    let mut registry = Registry::new();
    registry.register(&alice.public_key_bytes()).unwrap();
    let log = choir_oplog::FileLog::open(&log_path).unwrap();
    let platform = Platform::start(registry, Box::new(log)).unwrap();
    let (status, body) = platform.handle_api("GET", "/api/view", b"");
    assert_eq!(status, 200);
    let view: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(view["refs"]["main"], commit.to_hex());

    let payload = op.to_payload();
    let sig = alice.sign_submission("alice", &payload);
    let (status, body) = platform.handle_api(
        "POST",
        "/api/submit",
        serde_json::json!({
            "workspace": "alice",
            "payload_hex": hex_encode(&payload),
            "key_id": sig.key_id,
            "signature_hex": hex_encode(&sig.signature),
        })
        .to_string()
        .as_bytes(),
    );
    assert_eq!(status, 400);
    assert!(body.contains("stale"));

    std::fs::remove_dir_all(&work).ok();
}
