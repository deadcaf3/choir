//! `/api/log` catch-up for readers behind the in-memory window: the
//! evicted entries come off the persisted log, the page is
//! indistinguishable from a windowed one, and a node with no persisted
//! log says so instead of serving a page with a hole in it.

use choir_identity::{ActorKey, Registry};
use choir_node::platform::hex_encode;
use choir_node::{Node, Platform};
use choir_oplog::{FileLog, MemLog};
use choir_view::{OpKind, ViewOp};

use crate::support::curl;

/// Submits `n` provenance records (any op that always applies) as the
/// signed channel `author`.
fn fill(api: &str, key: &ActorKey, n: usize) {
    for i in 0..n {
        let op = ViewOp::new(OpKind::RecordProvenance {
            subject: "s".into(),
            kind: format!("k{i}"),
            body: format!("body {i}"),
        });
        let payload = op.to_payload();
        let sig = key.sign_submission("author", &payload);
        let body = serde_json::json!({
            "workspace": "author",
            "payload_hex": hex_encode(&payload),
            "key_id": sig.key_id,
            "signature_hex": hex_encode(&sig.signature),
        })
        .to_string();
        let (code, resp) = curl(&["-X", "POST", "-d", &body, &format!("{api}/submit")]);
        assert_eq!(code, 200, "{resp}");
    }
}

fn serve(node: Node) -> (std::sync::Arc<Node>, String) {
    let api = format!("http://127.0.0.1:{}/api", node.port());
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    (node, api)
}

#[test]
fn readers_behind_the_window_resync_from_the_persisted_log() {
    let work = std::env::temp_dir().join(format!("choir-node-resync-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let log_path = work.join("ops.jsonl");

    let key = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&key.public_key_bytes()).unwrap();

    // Window of 4 entries over a persisted log, so eviction happens
    // after a handful of ops instead of 100k.
    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
    node.enable_platform(
        Platform::start(
            registry,
            Box::new(FileLog::open(&log_path).unwrap()),
            ActorKey::generate(),
        )
        .unwrap()
        .with_log_path(log_path.clone())
        .with_log_window_cap(4),
    );
    let (node, api) = serve(node);

    fill(&api, &key, 10);

    // Inside the window: served from memory, starting exactly at `from`.
    let (code, page) = curl(&[&format!("{api}/log?from=8")]);
    assert_eq!(code, 200, "{page}");
    assert_eq!(page["source"], "window", "{page}");
    assert_eq!(page["window_base"], 6);
    assert_eq!(page["entries"][0]["seq"], 8, "{page}");

    // Behind the window: served from the persisted log, and — the point
    // of the fix — it starts at the entry that was asked for, not at
    // the window base. Before this, `from=0` silently returned seq 6.
    let (code, resync) = curl(&[&format!("{api}/log?from=0")]);
    assert_eq!(code, 200, "{resync}");
    assert_eq!(resync["source"], "log", "{resync}");
    assert_eq!(resync["entries"][0]["seq"], 0, "silent gap: {resync}");
    assert_eq!(resync["entries"].as_array().unwrap().len(), 10);

    // The two sources are indistinguishable: an entry inside the window
    // has the same JSON however it was fetched.
    let (_, from_log) = curl(&[&format!("{api}/log?from=0")]);
    assert_eq!(from_log["entries"][8], page["entries"][0], "{from_log}");

    // A reader can walk forward across the boundary with no special
    // case and see every seq exactly once, in order.
    let mut seen: Vec<u64> = Vec::new();
    let mut from = 0;
    while from < 10 {
        let (_, page) = curl(&[&format!("{api}/log?from={from}")]);
        let rows = page["entries"].as_array().unwrap().clone();
        assert!(!rows.is_empty(), "no progress at from={from}");
        for r in rows {
            seen.push(r["seq"].as_u64().unwrap());
        }
        from = seen.len();
    }
    assert_eq!(seen, (0..10).collect::<Vec<u64>>());

    node.unblock();
}

#[test]
fn an_in_memory_log_reports_the_gap_instead_of_hiding_it() {
    let work = std::env::temp_dir().join(format!("choir-node-resync-mem-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();

    let key = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&key.public_key_bytes()).unwrap();

    // No log path configured: evicted entries are simply gone.
    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate())
            .unwrap()
            .with_log_window_cap(2),
    );
    let (node, api) = serve(node);

    fill(&api, &key, 5);

    let (code, resp) = curl(&[&format!("{api}/log?from=0")]);
    assert_eq!(
        code, 409,
        "a gap must be an error, not a short page: {resp}"
    );
    assert_eq!(resp["window_base"], 3, "{resp}");
    assert_eq!(resp["code"], "log_evicted", "{resp}");
    assert!(
        resp["next"].as_str().unwrap().contains("resync from seq"),
        "{resp}"
    );

    // Still inside the window: normal 200.
    let (code, page) = curl(&[&format!("{api}/log?from=3")]);
    assert_eq!(code, 200, "{page}");
    assert_eq!(page["entries"][0]["seq"], 3);

    node.unblock();
}
