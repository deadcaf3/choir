//! Review retention is an emitter, not an in-memory deletion: the node
//! signs `ArchiveReview` ops, the sequencer orders them, and replay owns
//! the compacted result. Count chooses when completed review detail goes;
//! wall-clock age is consulted only under an explicit incomplete-review
//! lapse policy.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use choir_hash::ContentHash;
use choir_identity::{ActorKey, Registry};
use choir_node::platform::{hex_encode, ReviewRetention};
use choir_node::Platform;
use choir_oplog::{LogError, MemLog, OpEntry, OpLog};
use choir_view::{append_op, OpKind, Verdict, ViewOp};

fn submit_body(key: &ActorKey, channel: &str, op: &ViewOp) -> Vec<u8> {
    let payload = op.to_payload();
    let sig = key.sign_submission(channel, &payload);
    serde_json::json!({
        "workspace": channel,
        "payload_hex": hex_encode(&payload),
        "key_id": sig.key_id,
        "signature_hex": hex_encode(&sig.signature),
    })
    .to_string()
    .into_bytes()
}

fn submit(platform: &Platform, key: &ActorKey, channel: &str, op: ViewOp) -> serde_json::Value {
    let body = submit_body(key, channel, &op);
    let (status, response) = platform.handle_api("POST", "/api/submit", &body);
    assert_eq!(status, 200, "submission failed: {response}");
    serde_json::from_str(&response).expect("JSON response")
}

fn request(id: &str) -> ViewOp {
    ViewOp::new(OpKind::RequestReview {
        id: id.into(),
        target: ContentHash::blake3(id.as_bytes()),
        reviewers: vec!["ana".into()],
        target_ref: None,
    })
}

fn approve(id: &str) -> ViewOp {
    ViewOp::new(OpKind::PostVerdict {
        id: id.into(),
        reviewer: "ana".into(),
        verdict: Verdict::Approve,
        note: format!("detail for {id}"),
    })
}

fn view(platform: &Platform) -> serde_json::Value {
    let (status, response) = platform.handle_api("GET", "/api/view", &[]);
    assert_eq!(status, 200, "view failed: {response}");
    serde_json::from_str(&response).expect("JSON view")
}

fn platform(retention: ReviewRetention) -> (ActorKey, Platform) {
    let key = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&key.public_key_bytes()).unwrap();
    let platform = Platform::start_with_review_retention(
        registry,
        Box::new(MemLog::new()),
        ActorKey::generate(),
        retention,
    )
    .expect("platform starts");
    (key, platform)
}

#[test]
fn count_archives_the_oldest_complete_review_by_request_sequence() {
    let (key, platform) = platform(ReviewRetention::keep(2));

    // Deliberately not lexical order: FIFO must come from op sequence,
    // not View.reviews' BTreeMap key order.
    for id in ["z-oldest", "a-middle"] {
        submit(&platform, &key, "author", request(id));
        submit(&platform, &key, "ana", approve(id));
    }
    let response = submit(&platform, &key, "author", request("m-newest"));

    assert_eq!(
        response["archived_reviews"],
        serde_json::json!(["z-oldest"]),
        "the emitter should report the maintenance op it durably ordered"
    );
    let reviews = view(&platform)["reviews"].clone();
    assert_eq!(reviews["z-oldest"]["archived"], true, "{reviews}");
    assert_eq!(reviews["z-oldest"]["approved"], true, "{reviews}");
    assert_eq!(reviews["z-oldest"]["verdicts"], serde_json::json!({}));
    assert_eq!(reviews["a-middle"]["archived"], false, "{reviews}");
    assert_eq!(reviews["m-newest"]["archived"], false, "{reviews}");
}

#[test]
fn incomplete_reviews_never_lapse_without_an_explicit_age() {
    let (key, platform) = platform(ReviewRetention::keep(1));

    submit(&platform, &key, "author", request("old-incomplete"));
    let response = submit(&platform, &key, "author", request("new-incomplete"));

    assert!(
        response.get("archived_reviews").is_none(),
        "count alone must not invent an abandonment policy: {response}"
    );
    let reviews = view(&platform)["reviews"].clone();
    assert_eq!(reviews["old-incomplete"]["archived"], false, "{reviews}");
    assert_eq!(reviews["new-incomplete"]["archived"], false, "{reviews}");
}

#[test]
fn an_explicit_lapse_age_settles_an_over_limit_incomplete_review() {
    // Zero is an explicit deterministic boundary for the test; no sleep or
    // wall-clock assertion is involved.
    let retention = ReviewRetention::keep(1).lapse_incomplete_after(Duration::ZERO);
    let (key, platform) = platform(retention);

    submit(&platform, &key, "author", request("old-incomplete"));
    let response = submit(&platform, &key, "author", request("new-incomplete"));

    assert_eq!(
        response["archived_reviews"],
        serde_json::json!(["old-incomplete"]),
        "{response}"
    );
    let reviews = view(&platform)["reviews"].clone();
    assert_eq!(reviews["old-incomplete"]["archived"], true, "{reviews}");
    assert_eq!(
        reviews["old-incomplete"]["approved"], false,
        "lapsing must not invent approval: {reviews}"
    );
    assert_eq!(reviews["new-incomplete"]["archived"], false, "{reviews}");
}

struct CountingLog {
    inner: MemLog,
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

    fn sync(&mut self) -> Result<(), LogError> {
        self.syncs.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

#[test]
fn startup_recovers_fifo_order_and_emits_archive_ops_as_a_batch() {
    const REVIEWS: usize = 40;

    // Pre-existing reviews exercise restart replay rather than only the
    // live accepted() tracker. Descending ids make key order the opposite
    // of request order.
    let mut log = MemLog::new();
    for i in (0..REVIEWS).rev() {
        let id = format!("r-{i:02}");
        append_op(&mut log, "author", request(&id)).unwrap();
        append_op(&mut log, "ana", approve(&id)).unwrap();
    }

    let key = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&key.public_key_bytes()).unwrap();
    let syncs = Arc::new(AtomicUsize::new(0));
    let platform = Platform::start_with_review_retention(
        registry,
        Box::new(CountingLog {
            inner: log,
            syncs: syncs.clone(),
        }),
        ActorKey::generate(),
        ReviewRetention::keep(1),
    )
    .expect("platform replays");

    let startup_barriers = syncs.load(Ordering::Relaxed);
    let reviews = view(&platform)["reviews"].clone();
    assert_eq!(
        reviews["r-39"]["archived"], true,
        "the first request should be archived despite its largest id: {reviews}"
    );
    assert_eq!(
        reviews["r-00"]["archived"], false,
        "the last request should be the retained live review: {reviews}"
    );

    // One grouped startup pass emits 39 archive submissions. A serial
    // emitter would report 39 barriers. This asserts the count, never
    // elapsed time.
    assert!(
        startup_barriers < REVIEWS / 4,
        "{} startup archives cost {startup_barriers} barriers; serial emission would cost {}",
        REVIEWS - 1,
        REVIEWS - 1
    );

    // The tracker removed every archived id. A later write must not emit
    // them again, and should therefore add exactly its own barrier.
    let trigger = ViewOp::new(OpKind::RecordProvenance {
        subject: "retention".into(),
        kind: "trigger".into(),
        body: "run".into(),
    });
    let response = submit(&platform, &key, "author", trigger);
    assert!(
        response.get("archived_reviews").is_none(),
        "startup maintenance should not repeat: {response}"
    );
    assert_eq!(
        syncs.load(Ordering::Relaxed),
        startup_barriers + 1,
        "the later write should cost only its own barrier"
    );
}
