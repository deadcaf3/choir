//! Review discussion as a folded operation (D38).
//!
//! The claims worth testing are the ones the register row commits to: the
//! thread is the log's order, a comment id is the replay defence and is
//! refused twice, archiving drops the discussion with the rest of the
//! bulk, and replaying the log from scratch reproduces every comment
//! exactly, including its position.

use choir_hash::ContentHash;
use choir_oplog::{MemLog, OpLog};
use choir_view::{append_op, OpKind, View, ViewError, ViewOp};

/// A review to hang comments on, plus the log holding it.
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

fn comment(id: &str, author: &str, body: &str) -> ViewOp {
    ViewOp::new(OpKind::PostComment {
        id: "r1".into(),
        comment: id.into(),
        author: author.into(),
        body: body.into(),
    })
}

/// The thread is the sequencer's order, and `at` is where each comment
/// sits in the log rather than when it was written.
#[test]
fn comments_fold_into_the_review_in_log_order() {
    let mut log = review_log();
    append_op(&mut log, "ana", comment("c1", "ana", "why this base?")).unwrap();
    append_op(&mut log, "author", comment("c2", "author", "it is the merge base")).unwrap();
    append_op(&mut log, "ana", comment("c3", "ana", "then I am happy")).unwrap();

    let view = View::materialize(&log).unwrap();
    let thread = &view.reviews["r1"].comments;
    assert_eq!(
        thread.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
        ["c1", "c2", "c3"],
        "the thread is not in the order the sequencer admitted it"
    );
    assert_eq!(
        thread.iter().map(|c| c.author.as_str()).collect::<Vec<_>>(),
        ["ana", "author", "ana"]
    );
    assert_eq!(thread[1].body, "it is the merge base");
    // The review op is seq 0, so the comments occupy 1, 2 and 3. This is
    // a fold position, not a clock: it is exactly reproducible.
    assert_eq!(thread.iter().map(|c| c.at).collect::<Vec<_>>(), [1, 2, 3]);
}

/// Replay is the whole product claim. A prefix reproduces the thread as
/// it stood at that point, and the full replay reproduces it entirely --
/// from the ops alone, with nothing carried over from the writer.
#[test]
fn replaying_the_log_reproduces_the_thread_exactly() {
    let mut log = review_log();
    append_op(&mut log, "ana", comment("c1", "ana", "first")).unwrap();
    append_op(&mut log, "bot", comment("c2", "bot", "second")).unwrap();

    let whole = View::materialize(&log).unwrap();
    let again = View::at(&log, log.len()).unwrap();
    assert_eq!(whole, again, "two replays of one log disagreed");

    let midway = View::at(&log, 2).unwrap();
    assert_eq!(
        midway.reviews["r1"]
            .comments
            .iter()
            .map(|c| c.id.as_str())
            .collect::<Vec<_>>(),
        ["c1"],
        "a prefix replay did not stop where the prefix does"
    );
    assert_eq!(whole.reviews["r1"].comments.len(), 2);
}

/// The replay defence, in the shape invariant 4 forces: `seq` and
/// `parent` are assigned after signing, so the payload has to carry its
/// own identity and the fold has to refuse a repeat of it.
#[test]
fn a_comment_id_is_refused_twice_on_the_same_review() {
    let mut log = review_log();
    append_op(&mut log, "ana", comment("c1", "ana", "said once")).unwrap();

    // The identical signed payload, replayed. Same bytes, same signature
    // if one were attached: only the fold can refuse this.
    match append_op(&mut log, "ana", comment("c1", "ana", "said once")) {
        Err(ViewError::Review(msg)) => {
            assert!(msg.contains("c1") && msg.contains("already exists"), "{msg}");
        }
        other => panic!("a replayed comment was admitted: {other:?}"),
    }
    // Different text under the same id is refused too: the id is the
    // identity of the statement, not a hash of it.
    assert!(matches!(
        append_op(&mut log, "ana", comment("c1", "ana", "said differently")),
        Err(ViewError::Review(_))
    ));
    assert_eq!(View::materialize(&log).unwrap().reviews["r1"].comments.len(), 1);

    // The id is unique within its review, not across the log: two
    // reviews may each hold a `c1`, and neither knows about the other.
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
        ViewOp::new(OpKind::PostComment {
            id: "r2".into(),
            comment: "c1".into(),
            author: "ana".into(),
            body: "on the other review".into(),
        }),
    )
    .expect("a comment id is scoped to its review");
}

/// A comment on a review that does not exist is refused by name, and the
/// three empty-field cases are refused before anything is stored.
#[test]
fn a_comment_needs_a_review_an_id_an_author_and_a_body() {
    let mut log = review_log();
    match append_op(
        &mut log,
        "ana",
        ViewOp::new(OpKind::PostComment {
            id: "nope".into(),
            comment: "c1".into(),
            author: "ana".into(),
            body: "into the void".into(),
        }),
    ) {
        Err(ViewError::Review(msg)) => assert!(msg.contains("no such review"), "{msg}"),
        other => panic!("a comment on an absent review was admitted: {other:?}"),
    }

    for (id, author, body) in [("", "ana", "text"), ("c1", "", "text"), ("c1", "ana", "")] {
        assert!(
            matches!(
                append_op(&mut log, "ana", comment(id, author, body)),
                Err(ViewError::Review(_))
            ),
            "an empty field was admitted: id={id:?} author={author:?} body={body:?}"
        );
    }
    assert!(View::materialize(&log).unwrap().reviews["r1"].comments.is_empty());
}

/// Archiving drops the discussion with the verdicts, because discussion
/// is the part that grows without bound and it authorizes nothing. The
/// review then accepts no further comments -- which is also what keeps
/// the id-uniqueness rule true for the whole life of the review, since
/// an emptied thread can no longer prove an id was taken.
#[test]
fn archiving_drops_the_thread_and_closes_it() {
    let mut log = review_log();
    append_op(&mut log, "ana", comment("c1", "ana", "before settling")).unwrap();
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
    assert!(review.comments.is_empty(), "archiving kept the discussion");
    assert!(review.approved(), "archiving lost the outcome");

    // Frozen, and the refusal says archived rather than absent.
    match append_op(&mut log, "ana", comment("c2", "ana", "after settling")) {
        Err(ViewError::Review(msg)) => assert!(msg.contains("archived"), "{msg}"),
        other => panic!("an archived review accepted a comment: {other:?}"),
    }
    // Including the id the archived thread used to hold: an id cannot be
    // reissued, so a captured comment op has no window to replay into.
    assert!(matches!(
        append_op(&mut log, "ana", comment("c1", "ana", "before settling")),
        Err(ViewError::Review(_))
    ));
}
