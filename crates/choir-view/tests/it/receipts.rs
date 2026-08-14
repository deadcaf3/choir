//! Read receipts as a folded operation.
//!
//! The claims worth testing are the ones the op's doc commits to: a
//! receipt records the *first* read at its fold position, a (review,
//! viewer) pair is the replay defence and is refused twice, archiving
//! drops the receipts with the rest of the bulk, and replay reproduces
//! every receipt exactly.

use choir_hash::ContentHash;
use choir_oplog::{MemLog, OpLog};
use choir_view::{append_op, OpKind, View, ViewError, ViewOp};

/// A review to hang receipts on, plus the log holding it.
fn review_log() -> MemLog {
    let mut log = MemLog::new();
    append_op(
        &mut log,
        "author",
        ViewOp::new(OpKind::RequestReview {
            id: "r1".into(),
            target: ContentHash::blake3(b"the proposal"),
            reviewers: vec!["ana".into()],
            target_ref: Some("demo.git:refs/heads/main".into()),
        }),
    )
    .expect("the review opens");
    log
}

fn viewed(viewer: &str) -> ViewOp {
    ViewOp::new(OpKind::ViewedReview {
        id: "r1".into(),
        viewer: viewer.into(),
    })
}

/// A receipt is the first read: it lands at its fold position, and a
/// replay of the log reproduces the same map exactly.
#[test]
fn receipts_fold_at_their_log_position_and_replay_exactly() {
    let mut log = review_log();
    append_op(&mut log, "ana", viewed("ana")).unwrap();
    append_op(&mut log, "bot", viewed("bot")).unwrap();

    let view = View::materialize(&log).unwrap();
    let viewed_map = &view.reviews["r1"].viewed;
    // The review op is seq 0, so the receipts occupy 1 and 2. A fold
    // position, not a clock: exactly reproducible.
    assert_eq!(viewed_map.get("ana"), Some(&1));
    assert_eq!(viewed_map.get("bot"), Some(&2));

    let again = View::at(&log, log.len()).unwrap();
    assert_eq!(view, again, "two replays of one log disagreed");
}

/// The replay defence, in the shape invariant 4 forces: `seq` and
/// `parent` are assigned after signing, so the payload's (review,
/// viewer) pair has to be its own identity and the fold has to refuse a
/// repeat of it. Refusing the repeat is also what keeps the recorded
/// position the *first* read rather than the latest.
#[test]
fn a_viewer_is_refused_twice_on_the_same_review() {
    let mut log = review_log();
    append_op(&mut log, "ana", viewed("ana")).unwrap();

    match append_op(&mut log, "ana", viewed("ana")) {
        Err(ViewError::Review(msg)) => {
            assert!(msg.contains("ana") && msg.contains("already"), "{msg}");
        }
        other => panic!("a replayed receipt was admitted: {other:?}"),
    }
    assert_eq!(View::materialize(&log).unwrap().reviews["r1"].viewed["ana"], 1);

    // Scoped to its review: the same viewer reads another review freely.
    append_op(
        &mut log,
        "author",
        ViewOp::new(OpKind::RequestReview {
            id: "r2".into(),
            target: ContentHash::blake3(b"another proposal"),
            reviewers: vec![],
            target_ref: None,
        }),
    )
    .unwrap();
    append_op(
        &mut log,
        "ana",
        ViewOp::new(OpKind::ViewedReview {
            id: "r2".into(),
            viewer: "ana".into(),
        }),
    )
    .expect("a receipt is scoped to its review");
}

/// A receipt on a review that does not exist is refused by name, and an
/// unnamed viewer is refused before anything is stored.
#[test]
fn a_receipt_needs_a_review_and_a_viewer() {
    let mut log = review_log();
    match append_op(
        &mut log,
        "ana",
        ViewOp::new(OpKind::ViewedReview {
            id: "nope".into(),
            viewer: "ana".into(),
        }),
    ) {
        Err(ViewError::Review(msg)) => assert!(msg.contains("no such review"), "{msg}"),
        other => panic!("a receipt on an absent review was admitted: {other:?}"),
    }
    assert!(matches!(
        append_op(&mut log, "ana", viewed("")),
        Err(ViewError::Review(_))
    ));
    assert!(View::materialize(&log).unwrap().reviews["r1"].viewed.is_empty());
}

/// Receipts are bulk: they grow with readers and authorize nothing, so
/// archiving drops them with the verdicts and the thread. The review
/// then accepts no further receipts -- which is what keeps the
/// first-read rule true for the whole life of the review, since an
/// emptied map can no longer prove a viewer was recorded.
#[test]
fn archiving_drops_the_receipts_and_closes_them() {
    let mut log = review_log();
    append_op(&mut log, "ana", viewed("ana")).unwrap();
    append_op(
        &mut log,
        "ana",
        ViewOp::new(OpKind::PostVerdict {
            id: "r1".into(),
            reviewer: "ana".into(),
            verdict: choir_view::Verdict::Approve,
            note: "fine".into(),
        }),
    )
    .unwrap();
    append_op(
        &mut log,
        "node",
        ViewOp::new(OpKind::ArchiveReview {
            id: "r1".into(),
            lapsed: false,
        }),
    )
    .unwrap();

    let view = View::materialize(&log).unwrap();
    let review = &view.reviews["r1"];
    assert!(review.viewed.is_empty(), "archiving kept the receipts");
    assert!(review.approved(), "archiving lost the outcome");

    // Frozen, and the refusal says archived rather than absent --
    // including for the viewer the archived map used to hold.
    match append_op(&mut log, "bot", viewed("bot")) {
        Err(ViewError::Review(msg)) => assert!(msg.contains("archived"), "{msg}"),
        other => panic!("an archived review accepted a receipt: {other:?}"),
    }
    assert!(matches!(
        append_op(&mut log, "ana", viewed("ana")),
        Err(ViewError::Review(_))
    ));
}
