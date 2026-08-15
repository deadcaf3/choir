//! The authorization a `Submit` carries, as the fold checks it (D43).
//!
//! The node's gate decides whether a landing is allowed, from files that
//! are not in the log. Everything about that decision which *is* in the
//! log gets rederived here, by every replayer rather than only by the
//! node that admitted it. So these tests are about the half a compromised
//! or buggy node cannot talk its way past.

use choir_hash::ContentHash;
use choir_oplog::MemLog;
use choir_view::{append_op, Authorization, Basis, OpKind, Verdict, View, ViewError, ViewOp};

const REF: &str = "demo.git:refs/heads/main";

fn target() -> ContentHash {
    ContentHash::blake3(b"the proposal")
}

fn actor(seed: &[u8]) -> ContentHash {
    ContentHash::blake3(seed)
}

/// A log with one live review of [`target`] landing on [`REF`], two
/// reviewers from distinct operators, and a binding for each — the
/// smallest world in which an authorization can be checked at all.
fn approved_log() -> MemLog {
    approved_log_with(&[])
}

/// [`approved_log`], plus `extra` bindings placed **before** any verdict
/// is cast. The position matters: an approver is resolved as of the
/// verdict's own fold position (D44), so a binding appended afterwards
/// cannot change who that verdict credits.
fn approved_log_with(extra: &[(&str, &[u8])]) -> MemLog {
    let mut log = MemLog::new();
    for (who, seed) in [("ana", &b"ana"[..]), ("bo", &b"bo"[..])] {
        append_op(
            &mut log,
            "node",
            ViewOp::new(OpKind::BindKey {
                operator: who.into(),
                key: actor(seed),
                channel: Some(who.into()),
            }),
        )
        .expect("the binding lands");
    }
    for (who, seed) in extra {
        append_op(
            &mut log,
            "node",
            ViewOp::new(OpKind::BindKey {
                operator: (*who).into(),
                key: actor(seed),
                channel: Some((*who).into()),
            }),
        )
        .expect("a second key on one channel is a legal binding");
    }
    append_op(
        &mut log,
        "author",
        ViewOp::new(OpKind::SetRef {
            name: REF.into(),
            commit: ContentHash::blake3(b"base"),
            prev: None,
        }),
    )
    .expect("the ref exists");
    append_op(
        &mut log,
        "author",
        ViewOp::new(OpKind::RequestReview {
            id: "r1".into(),
            target: target(),
            reviewers: vec!["ana".into(), "bo".into()],
            target_ref: Some(REF.into()),
        }),
    )
    .expect("the review opens");
    for who in ["ana", "bo"] {
        append_op(
            &mut log,
            who,
            ViewOp::new(OpKind::PostVerdict {
                id: "r1".into(),
                reviewer: who.into(),
                verdict: Verdict::Approve,
                note: String::new(),
            }),
        )
        .expect("the verdict lands");
    }
    log
}

fn submit(authorization: Authorization) -> ViewOp {
    ViewOp::new(OpKind::Submit {
        review: "r1".into(),
        name: REF.into(),
        commit: target(),
        prev: Some(ContentHash::blake3(b"base")),
        authorization,
    })
}

fn weight(met: u32) -> Authorization {
    Authorization::new(
        Basis::ApprovalWeight { required: 2, met },
        vec![actor(b"ana"), actor(b"bo")],
    )
}

fn view_of(log: &MemLog) -> View {
    View::materialize(log).expect("the log replays")
}

fn refused(view: &View, op: &ViewOp) -> String {
    match view.validate(op) {
        Err(ViewError::Review(reason)) => reason,
        other => panic!("the fold must refuse this: {other:?}"),
    }
}

/// The honest case, so every refusal below is known to be about the thing
/// it names rather than about the fixture.
#[test]
fn an_authorization_the_log_agrees_with_applies_and_moves_the_ref() {
    let log = approved_log();
    let mut view = view_of(&log);
    view.apply(&submit(weight(2))).expect("the landing applies");
    assert_eq!(view.refs.get(REF), Some(&target()));
}

/// A weight nobody has to believe. The number in the record is the
/// review's own, and a replayer that recomputes it is what stops a node
/// from writing a landing as better-supported than it was.
#[test]
fn a_claimed_weight_the_review_does_not_carry_is_refused() {
    let log = approved_log();
    let view = view_of(&log);
    let reason = refused(&view, &submit(weight(4)));
    assert!(
        reason.contains("approval weight 2, not the claimed 4"),
        "{reason}"
    );
}

/// The approver list is derived, not accepted. Naming somebody who never
/// approved is the forgery this field exists to make impossible, and it
/// is refused even though the *weight* is entirely truthful.
#[test]
fn an_approver_the_review_never_had_is_refused() {
    let log = approved_log();
    let view = view_of(&log);
    let forged = Authorization::new(
        Basis::ApprovalWeight {
            required: 2,
            met: 2,
        },
        vec![actor(b"ana"), actor(b"someone-else")],
    );
    let reason = refused(&view, &submit(forged));
    assert!(reason.contains("bindings produce"), "{reason}");
}

/// An owner basis is checked against the review too. The fold cannot see
/// the ACL and so cannot know `ana` owns anything — but it can know
/// whether `ana` approved, and refusing here means the unverifiable part
/// of the record is exactly one field wide.
#[test]
fn an_owner_approval_nobody_cast_is_refused() {
    let log = approved_log();
    let view = view_of(&log);
    let reason = refused(
        &view,
        &submit(Authorization::new(
            Basis::OwnerApproved {
                owner: "carol".into(),
            },
            vec![actor(b"ana")],
        )),
    );
    assert!(
        reason.contains("carol has no standing approval"),
        "{reason}"
    );
}

/// A slashed approval is not a standing one. `SlashApproval` exists to
/// invalidate a verdict retroactively, and a landing record that could
/// still cite it would be the one place the slash did not reach.
#[test]
fn a_slashed_owner_approval_can_no_longer_authorize_a_landing() {
    let mut log = approved_log();
    append_op(
        &mut log,
        "node",
        ViewOp::new(OpKind::SlashApproval {
            id: "r1".into(),
            reviewer: "ana".into(),
            reason: "key found on a shared host".into(),
        }),
    )
    .expect("the slash lands");
    let view = view_of(&log);
    let reason = refused(
        &view,
        &submit(Authorization::new(
            Basis::OwnerApproved {
                owner: "ana".into(),
            },
            vec![actor(b"ana")],
        )),
    );
    assert!(reason.contains("ana has no standing approval"), "{reason}");
}

/// Citing a review that proposes something else. The pair a review names
/// is the whole basis for reading its approvals as approval *of this
/// landing*, so a mismatch is refused rather than treated as a stale
/// pointer.
#[test]
fn a_review_that_proposes_a_different_commit_cannot_authorize_this_one() {
    let log = approved_log();
    let view = view_of(&log);
    let elsewhere = ViewOp::new(OpKind::Submit {
        review: "r1".into(),
        name: REF.into(),
        commit: ContentHash::blake3(b"something else entirely"),
        prev: Some(ContentHash::blake3(b"base")),
        authorization: weight(2),
    });
    let reason = refused(&view, &elsewhere);
    assert!(reason.contains("names commit"), "{reason}");
}

/// An archived review has no verdicts left, so nothing here could be
/// rederived. Refusing keeps "the record was checked" true without
/// exception — the alternative is a record that is verified except when
/// it happens not to be, which is the same as unverified.
#[test]
fn a_landing_cannot_cite_a_review_whose_verdicts_are_gone() {
    let mut log = approved_log();
    append_op(
        &mut log,
        "node",
        ViewOp::new(OpKind::ArchiveReview {
            id: "r1".into(),
            lapsed: false,
        }),
    )
    .expect("the archive lands");
    let view = view_of(&log);
    let reason = refused(&view, &submit(weight(2)));
    assert!(reason.contains("archived"), "{reason}");
}

/// An approver whose key the log never bound cannot be named, so the
/// landing is refused rather than recorded with a gap. This is the fold's
/// half of the node's own refusal, and it is why a replayer reaches the
/// same verdict without the node's key files.
#[test]
fn an_approval_the_log_binds_no_key_to_cannot_be_recorded() {
    let mut log = MemLog::new();
    append_op(
        &mut log,
        "author",
        ViewOp::new(OpKind::SetRef {
            name: REF.into(),
            commit: ContentHash::blake3(b"base"),
            prev: None,
        }),
    )
    .expect("the ref exists");
    append_op(
        &mut log,
        "author",
        ViewOp::new(OpKind::RequestReview {
            id: "r1".into(),
            target: target(),
            reviewers: vec!["ana".into()],
            target_ref: Some(REF.into()),
        }),
    )
    .expect("the review opens");
    append_op(
        &mut log,
        "ana",
        ViewOp::new(OpKind::PostVerdict {
            id: "r1".into(),
            reviewer: "ana".into(),
            verdict: Verdict::Approve,
            note: String::new(),
        }),
    )
    .expect("the verdict lands");
    let view = view_of(&log);
    let reason = refused(
        &view,
        &submit(Authorization::new(
            Basis::OwnerApproved {
                owner: "ana".into(),
            },
            vec![actor(b"ana")],
        )),
    );
    assert!(reason.contains("binds no key to ana"), "{reason}");
}

/// Two *live* keys on one channel is refused rather than resolved.
/// Nothing in a review says which key cast the verdict, so a tiebreak
/// would put a specific key in a durable record on the strength of a
/// guess. The "live" qualifier is D44's: a revoked rival is not a rival,
/// or the documented revoke-and-rebind repair would break this path.
#[test]
fn a_channel_the_log_binds_twice_is_refused_rather_than_guessed() {
    let log = approved_log_with(&[("ana", &b"ana's second key"[..])]);
    let view = view_of(&log);
    let reason = refused(&view, &submit(weight(2)));
    assert!(reason.contains("more than one live key to ana"), "{reason}");
}

/// The lifecycle the code itself prescribes, end to end: a key is lost,
/// revoked, and replaced by a fresh one on the same channel — which is
/// forced, because a channel must read as its operator's name.
///
/// This is the regression test for the defect D44 was opened on. The
/// landing must still work, and the record must still name the key that
/// actually approved rather than the replacement, which never saw the
/// review. Before as-of resolution, the two halves were unreachable
/// together: counting the withdrawn row refused the landing outright,
/// and ignoring it credited the fresh key.
#[test]
fn a_revoked_key_replaced_by_a_fresh_one_still_credits_the_key_that_approved() {
    let mut log = approved_log();
    append_op(
        &mut log,
        "node",
        ViewOp::new(OpKind::RevokeKey {
            key: actor(b"ana"),
            reason: "laptop lost".into(),
        }),
    )
    .expect("the revocation lands");
    append_op(
        &mut log,
        "node",
        ViewOp::new(OpKind::BindKey {
            operator: "ana".into(),
            key: actor(b"ana's fresh key"),
            channel: Some("ana".into()),
        }),
    )
    .expect("the remedy BindKey itself prescribes");
    let mut view = view_of(&log);
    // `weight(2)` names actor(b"ana") -- the revoked key. That is the
    // assertion: the fresh key is bound, live, and on the same channel,
    // and it is still not the one credited.
    view.apply(&submit(weight(2)))
        .expect("the landing survives the rotation");
    assert_eq!(view.refs.get(REF), Some(&target()));
}

/// The other direction, so the rule is not just "revocation is ignored":
/// an approval cast by a channel whose only key was already revoked has
/// no key to credit, and the landing is refused rather than attributed to
/// nobody.
#[test]
fn an_approval_cast_after_the_key_was_revoked_cannot_be_credited() {
    let mut log = MemLog::new();
    append_op(
        &mut log,
        "node",
        ViewOp::new(OpKind::BindKey {
            operator: "ana".into(),
            key: actor(b"ana"),
            channel: Some("ana".into()),
        }),
    )
    .expect("the binding lands");
    append_op(
        &mut log,
        "node",
        ViewOp::new(OpKind::RevokeKey {
            key: actor(b"ana"),
            reason: "left the project".into(),
        }),
    )
    .expect("the revocation lands");
    append_op(
        &mut log,
        "author",
        ViewOp::new(OpKind::SetRef {
            name: REF.into(),
            commit: ContentHash::blake3(b"base"),
            prev: None,
        }),
    )
    .expect("the ref exists");
    append_op(
        &mut log,
        "author",
        ViewOp::new(OpKind::RequestReview {
            id: "r1".into(),
            target: target(),
            reviewers: vec!["ana".into()],
            target_ref: Some(REF.into()),
        }),
    )
    .expect("the review opens");
    append_op(
        &mut log,
        "ana",
        ViewOp::new(OpKind::PostVerdict {
            id: "r1".into(),
            reviewer: "ana".into(),
            verdict: Verdict::Approve,
            note: String::new(),
        }),
    )
    .expect("the fold does not gate verdicts on bindings");
    let view = view_of(&log);
    let reason = refused(
        &view,
        &submit(Authorization::new(
            Basis::OwnerApproved {
                owner: "ana".into(),
            },
            vec![actor(b"ana")],
        )),
    );
    assert!(reason.contains("was live at seq"), "{reason}");
}

/// The same two keys, in the other order relative to the verdict, and the
/// landing succeeds. A key bound *after* an approval was cast could not
/// have cast it, so it is not a rival for the credit — which is what
/// stops an operator adding a key and retroactively making every open
/// approval of theirs unresolvable.
#[test]
fn a_key_bound_after_the_verdict_does_not_make_it_ambiguous() {
    let mut log = approved_log();
    append_op(
        &mut log,
        "node",
        ViewOp::new(OpKind::BindKey {
            operator: "ana".into(),
            key: actor(b"ana's second key"),
            channel: Some("ana".into()),
        }),
    )
    .expect("a second key on one channel is a legal binding");
    let mut view = view_of(&log);
    view.apply(&submit(weight(2))).expect("the landing stands");
    assert_eq!(view.refs.get(REF), Some(&target()));
}
