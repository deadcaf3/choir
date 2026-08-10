//! An enabled retention bound must not tax the submit path.
//!
//! The pruning pass runs after every accepted submission. Its scan is
//! proportional to the number of live reviews, so a node holding more
//! reviews than its bound — with none of them archivable — paid that scan
//! on every unrelated write, forever. That is not an exotic case: it is
//! the default shape, because an incomplete review never lapses unless an
//! age is explicitly configured, so the reviews nobody answered are
//! exactly the ones that accumulate and exactly the ones a pass cannot
//! clear. Measured at **4.39x** on the submit path (88.4 -> 387.9 us/op)
//! before `worth_a_pass` existed.
//!
//! This asserts a ratio rather than a duration. Wall-clock figures on this
//! machine carry a documented thermal spread of several times (see
//! PHASE0.md), which would make an absolute threshold either flaky or
//! useless; two phases measured back-to-back in one process share their
//! thermal state, and the ceiling is loose enough to only ever fire on a
//! structural regression.

use choir_hash::ContentHash;
use choir_identity::{ActorKey, Registry};
use choir_node::platform::{hex_encode, ReviewRetention};
use choir_node::Platform;
use choir_oplog::MemLog;
use choir_view::{OpKind, ViewOp};

/// Live reviews held over the bound, none of them archivable.
const STUCK: usize = 1_000;
/// Unrelated writes timed on each side.
const OPS: usize = 2_000;
/// Generous: the fix measures ~1.0x, the regression measured 4.4x.
const CEILING: f64 = 2.0;

fn submit(platform: &Platform, key: &ActorKey, channel: &str, op: ViewOp) {
    let payload = op.to_payload();
    let sig = key.sign_submission(channel, &payload);
    let body = serde_json::json!({
        "workspace": channel,
        "payload_hex": hex_encode(&payload),
        "key_id": sig.key_id,
        "signature_hex": hex_encode(&sig.signature),
    })
    .to_string();
    let (status, out) = platform.handle_api("POST", "/api/submit", body.as_bytes());
    assert_eq!(status, 200, "{out}");
}

fn build(retention: Option<ReviewRetention>) -> (ActorKey, Platform) {
    let key = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&key.public_key_bytes()).expect("valid key");
    let platform = match retention {
        Some(r) => Platform::start_with_review_retention(
            registry,
            Box::new(MemLog::new()),
            ActorKey::generate(),
            r,
        ),
        None => Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate()),
    }
    .expect("platform starts");
    (key, platform)
}

/// Reviews nobody will ever answer: incomplete, so no count-only bound can
/// archive them.
fn fill_incomplete(platform: &Platform, key: &ActorKey, n: usize) {
    for i in 0..n {
        submit(
            platform,
            key,
            "author",
            ViewOp::new(OpKind::RequestReview {
                id: format!("r{i}"),
                target: ContentHash::blake3(format!("t{i}").as_bytes()),
                target_ref: None,
                reviewers: vec!["ana".into()],
            }),
        );
    }
}

/// Writes that have nothing to do with reviews — the traffic that must
/// stay unaffected by an enabled bound.
fn time_unrelated_writes(platform: &Platform, key: &ActorKey, n: usize) -> std::time::Duration {
    let start = std::time::Instant::now();
    for i in 0..n {
        submit(
            platform,
            key,
            "author",
            ViewOp::new(OpKind::RecordProvenance {
                subject: "s".into(),
                kind: format!("k{i}"),
                body: "b".into(),
            }),
        );
    }
    start.elapsed()
}

/// Best of three, per side.
///
/// The question here is whether an enabled retention bound adds per-op cost,
/// and the cleanest estimate of that is the *fastest* observation: a run can
/// be slowed by the machine but never speeded up by it, so noise only ever
/// inflates a sample. Taking a single sample instead let contention answer
/// the question — this assertion failed once at 2.87x inside a parallel
/// `cargo test --workspace`, while ten isolated runs measured 0.97x to
/// 1.03x. The ceiling was never the problem; the estimator was.
fn best_of_three(
    platform: &choir_node::Platform,
    key: &choir_identity::ActorKey,
    n: usize,
) -> std::time::Duration {
    (0..3)
        .map(|_| time_unrelated_writes(platform, key, n))
        .min()
        .expect("three samples")
}

#[test]
fn an_unprunable_backlog_does_not_slow_unrelated_writes() {
    let (key, off) = build(None);
    fill_incomplete(&off, &key, STUCK);
    let baseline = best_of_three(&off, &key, OPS);

    let (key, on) = build(Some(ReviewRetention::keep(10)));
    fill_incomplete(&on, &key, STUCK);
    let enabled = best_of_three(&on, &key, OPS);

    let ratio = enabled.as_secs_f64() / baseline.as_secs_f64();
    println!(
        "retention off {:.1} us/op, on {:.1} us/op, ratio {ratio:.2}x",
        baseline.as_secs_f64() * 1e6 / OPS as f64,
        enabled.as_secs_f64() * 1e6 / OPS as f64,
    );
    assert!(
        ratio < CEILING,
        "an enabled retention bound cost {ratio:.2}x on writes that touch no review \
         (ceiling {CEILING}x); the pruning pass is scanning when nothing changed"
    );
}
