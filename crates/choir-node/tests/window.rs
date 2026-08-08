//! The `/api/log` sliding window: eviction bookkeeping and the trim that
//! `with_log_window_cap` owes a platform started over an existing log.

use choir_hash::ContentHash;
use choir_identity::{ActorKey, Registry};
use choir_node::platform::hex_encode;
use choir_node::Platform;
use choir_oplog::{FileLog, OpLog};
use choir_view::{OpKind, ViewOp};

/// Submits `n` ops and returns the platform, so a second platform can be
/// started over the same log to exercise the startup path.
fn seed_log(dir: &std::path::Path, n: usize) {
    let key = ActorKey::generate();
    let mut registry = Registry::new();
    registry
        .register(&key.public_key_bytes())
        .expect("generated key is valid");
    let log = FileLog::open(&dir.join("ops.jsonl")).expect("open");
    let platform =
        Platform::start(registry, Box::new(log), ActorKey::generate()).expect("platform starts");

    let mut prev: Option<ContentHash> = None;
    for i in 0..n {
        let commit = ContentHash::blake3(format!("c{i}").as_bytes());
        let op = ViewOp::new(OpKind::SetWorkspaceHead {
            workspace: "ws".to_string(),
            commit: commit.clone(),
            prev: prev.replace(commit),
        });
        let payload = op.to_payload();
        let sig = key.sign_submission("ws", &payload);
        let body = serde_json::json!({
            "workspace": "ws",
            "payload_hex": hex_encode(&payload),
            "key_id": sig.key_id,
            "signature_hex": hex_encode(&sig.signature),
        })
        .to_string();
        let (status, out) = platform.handle_api("POST", "/api/submit", body.as_bytes());
        assert_eq!(status, 200, "seed op rejected: {out}");
    }
}

/// Shrinking the window must take effect immediately, not at the next
/// push. A platform restarted over an existing log takes no submissions
/// until a client sends one, so a deferred trim leaves `/api/log` serving
/// entries from below its own reported `window_base` for an unbounded
/// time — a reader cannot then tell what the window actually holds.
#[test]
fn shrinking_the_window_trims_it_at_once() {
    let dir = std::env::temp_dir().join(format!("choir-window-trim-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    seed_log(&dir, 20);

    // Restart over the same log, then shrink the window without
    // submitting anything.
    let log = FileLog::open(&dir.join("ops.jsonl")).expect("reopen");
    assert_eq!(log.len(), 20, "seeded log survived");
    let registry = Registry::new();
    let platform = Platform::start(registry, Box::new(log), ActorKey::generate())
        .expect("platform starts")
        .with_log_window_cap(5);

    let (status, out) = platform.handle_api("GET", "/api/log?from=15", b"");
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(status, 200, "{out}");
    let parsed: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert_eq!(
        parsed["window_base"].as_u64(),
        Some(15),
        "a 5-entry window over a 20-entry log must start at 15, got {out}"
    );
    assert_eq!(
        parsed["entries"].as_array().expect("entries").len(),
        5,
        "the window must hold exactly its cap"
    );
}

/// Startup fills the window from the tail. With a log shorter than the
/// cap the whole log is held and `base` is 0 — the case every existing
/// test exercises, pinned here so the tail-fill arithmetic cannot drift.
#[test]
fn a_short_log_fills_the_window_from_zero() {
    let dir = std::env::temp_dir().join(format!("choir-window-short-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    seed_log(&dir, 7);

    let log = FileLog::open(&dir.join("ops.jsonl")).expect("reopen");
    let platform = Platform::start(Registry::new(), Box::new(log), ActorKey::generate())
        .expect("platform starts");

    let (status, out) = platform.handle_api("GET", "/api/log?from=0", b"");
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(status, 200, "{out}");
    let parsed: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert_eq!(parsed["window_base"].as_u64(), Some(0));
    assert_eq!(parsed["entries"].as_array().expect("entries").len(), 7);
    assert_eq!(
        parsed["source"].as_str(),
        Some("window"),
        "a short log is served from memory, not the resync path"
    );
}
