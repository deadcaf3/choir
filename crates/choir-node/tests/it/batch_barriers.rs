//! `/api/submit-batch` must cost one durability barrier per batch, not one
//! per op.
//!
//! This is the property, separate from the speed. The endpoint previously
//! called the single-op path in a loop, and because that path blocks until
//! its own reply, the writer's queue was empty every time it looked: every
//! op became its own batch and paid its own fsync. The speed-up is a
//! consequence; the barrier count is the thing that must not regress.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use choir_hash::ContentHash;
use choir_identity::{ActorKey, Registry};
use choir_node::platform::hex_encode;
use choir_node::Platform;
use choir_oplog::{FileLog, LogError, OpEntry, OpLog};
use choir_view::{OpKind, ViewOp};

/// Counts durability barriers, delegating storage to a real `FileLog` so
/// the barrier being counted is a real one.
struct CountingLog {
    inner: FileLog,
    syncs: Arc<AtomicUsize>,
}

impl OpLog for CountingLog {
    fn append(&mut self, entry: OpEntry) -> Result<ContentHash, LogError> {
        self.inner.append(entry)
    }
    fn head(&self) -> Option<ContentHash> {
        self.inner.head()
    }
    fn len(&self) -> u64 {
        self.inner.len()
    }
    fn get(&self, seq: u64) -> Option<OpEntry> {
        self.inner.get(seq)
    }

    fn last(&self) -> Option<&OpEntry> {
        self.inner.last()
    }
    fn sync(&mut self) -> Result<(), LogError> {
        self.syncs.fetch_add(1, Ordering::Relaxed);
        self.inner.sync()
    }
}

#[test]
fn a_batch_request_shares_its_durability_barriers() {
    const OPS: usize = 300;

    let dir = std::env::temp_dir().join(format!("choir-batch-bar-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");

    let key = ActorKey::generate();
    let mut registry = Registry::new();
    registry
        .register(&key.public_key_bytes())
        .expect("generated key is valid");
    let syncs = Arc::new(AtomicUsize::new(0));
    let log = CountingLog {
        inner: FileLog::open(&dir.join("ops.jsonl")).expect("open"),
        syncs: syncs.clone(),
    };
    let platform =
        Platform::start(registry, Box::new(log), ActorKey::generate()).expect("platform starts");

    // One workspace advancing its own head, so every op is admissible and
    // the CAS chain is sequential.
    let mut prev: Option<ContentHash> = None;
    let bodies: Vec<String> = (0..OPS)
        .map(|i| {
            let commit = ContentHash::blake3(format!("c{i}").as_bytes());
            let op = ViewOp::new(OpKind::SetWorkspaceHead {
                workspace: "ws".to_string(),
                commit: commit.clone(),
                prev: prev.replace(commit),
            });
            let payload = op.to_payload();
            let sig = key.sign_submission("ws", &payload);
            serde_json::json!({
                "workspace": "ws",
                "payload_hex": hex_encode(&payload),
                "key_id": sig.key_id,
                "signature_hex": hex_encode(&sig.signature),
            })
            .to_string()
        })
        .collect();

    let body = format!(r#"{{"ops":[{}]}}"#, bodies.join(","));
    let (status, out) = platform.handle_api("POST", "/api/submit-batch", body.as_bytes());
    assert_eq!(status, 200, "batch rejected: {out}");

    let parsed: serde_json::Value = serde_json::from_str(&out).expect("reply is json");
    assert_eq!(
        parsed["accepted"].as_u64().expect("accepted count"),
        OPS as u64,
        "every op in the batch admitted"
    );
    assert_eq!(
        parsed["results"].as_array().expect("results array").len(),
        OPS,
        "one result per requested op, in request order"
    );

    let barriers = syncs.load(Ordering::Relaxed);
    std::fs::remove_dir_all(&dir).ok();

    // The load-bearing assertion. One barrier per op is the old shape and
    // would give `barriers >= OPS`. A generous ceiling is used rather than
    // an exact count because the writer may legitimately close a batch
    // early -- MAX_BATCH, or simply finding the queue momentarily empty --
    // and pinning the exact number would make this a timing test.
    assert!(
        barriers < OPS / 10,
        "batch of {OPS} ops cost {barriers} durability barriers; \
         one per op means the endpoint is submitting serially again"
    );
    println!("batch of {OPS} ops cost {barriers} durability barriers");
}

/// Malformed ops must not take a slot from the sequencer, and must still
/// occupy their slot in the results array — otherwise a caller cannot line
/// results up with what it sent.
#[test]
fn a_malformed_op_keeps_its_place_in_the_results() {
    let dir = std::env::temp_dir().join(format!("choir-batch-bad-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");

    let key = ActorKey::generate();
    let mut registry = Registry::new();
    registry
        .register(&key.public_key_bytes())
        .expect("generated key is valid");
    let log = FileLog::open(&dir.join("ops.jsonl")).expect("open");
    let platform =
        Platform::start(registry, Box::new(log), ActorKey::generate()).expect("platform starts");

    let good = |i: usize, prev: Option<ContentHash>| {
        let commit = ContentHash::blake3(format!("c{i}").as_bytes());
        let op = ViewOp::new(OpKind::SetWorkspaceHead {
            workspace: "ws".to_string(),
            commit,
            prev,
        });
        let payload = op.to_payload();
        let sig = key.sign_submission("ws", &payload);
        serde_json::json!({
            "workspace": "ws",
            "payload_hex": hex_encode(&payload),
            "key_id": sig.key_id,
            "signature_hex": hex_encode(&sig.signature),
        })
        .to_string()
    };

    // good, malformed (missing fields), good — the second good op chains
    // off the first, so the malformed one must not consume a position.
    let first = ContentHash::blake3(b"c0");
    let body = format!(
        r#"{{"ops":[{},{{"workspace":"ws"}},{}]}}"#,
        good(0, None),
        good(1, Some(first))
    );
    let (status, out) = platform.handle_api("POST", "/api/submit-batch", body.as_bytes());
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(status, 200, "a malformed element must not fail the request");

    let parsed: serde_json::Value = serde_json::from_str(&out).expect("reply is json");
    let results = parsed["results"].as_array().expect("results array");
    assert_eq!(results.len(), 3, "one result per requested op");
    assert_eq!(parsed["accepted"].as_u64(), Some(2));
    assert_eq!(parsed["rejected"].as_u64(), Some(1));
    assert!(
        results[0].get("seq").is_some(),
        "first op should be admitted, got {}",
        results[0]
    );
    assert!(
        results[1].get("error").is_some(),
        "the malformed op's slot must carry its error, got {}",
        results[1]
    );
    assert!(
        results[2].get("seq").is_some(),
        "the op after a malformed one must still be admitted, got {}",
        results[2]
    );
}
