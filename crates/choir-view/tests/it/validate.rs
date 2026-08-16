//! `View::validate` must agree with `View::apply`, exactly, and must not
//! mutate.
//!
//! Admission stopped deep-cloning the view and now asks `validate` against
//! the shared one. That is only sound while two properties hold:
//!
//! 1. **Agreement.** `validate(op).is_ok() == apply(op).is_ok()`. If
//!    validate were laxer, the sequencer would order an op that then fails
//!    to fold, and `accepted`'s `.expect("checked in check()")` becomes a
//!    panic on the writer thread — which, with a `std::sync::Mutex`,
//!    poisons the view for every later request.
//! 2. **Purity.** A `validate` call leaves the view byte-identical, since
//!    it now runs against live shared state rather than a private copy.
//!
//! `apply` calls `validate` internally, so these hold by construction
//! today. These tests exist so that a future edit which reintroduces a
//! precondition into `apply` alone gets caught.

use std::collections::BTreeMap;

use choir_hash::ContentHash;
use choir_oplog::Witness;
use choir_view::{OpKind, RefSnapshot, Verdict, View, ViewOp};

fn h(tag: &[u8]) -> ContentHash {
    ContentHash::blake3(tag)
}

fn witness() -> Witness {
    Witness::ed25519("owner-key", vec![1, 2, 3])
}

/// Every op shape, in both admissible and inadmissible states. Hand-rolled
/// rather than generated: the repo hand-rolls its randomness elsewhere for
/// the same reason (no `rand`/`proptest` dependency), and an enumerated set
/// covering each variant's success and failure arms is more legible than a
/// generator for ten variants.
fn cases() -> Vec<(&'static str, View, ViewOp)> {
    // A view with one workspace, one ref, one assigned review.
    let populated = {
        let mut v = View::default();
        v.apply(&ViewOp::new(OpKind::SetWorkspaceHead {
            workspace: "ws".into(),
            commit: h(b"c1"),
            prev: None,
        }))
        .expect("setup");
        v.apply(&ViewOp::new(OpKind::SetRef {
            name: "r".into(),
            commit: h(b"c1"),
            prev: None,
        }))
        .expect("setup");
        v.apply(&ViewOp::new(OpKind::RequestReview {
            id: "rev".into(),
            target: h(b"c1"),
            reviewers: vec!["ana".into()],
            target_ref: None,
        }))
        .expect("setup");
        v.apply(&ViewOp::new(OpKind::RequestReview {
            id: "unassigned".into(),
            target: h(b"c1"),
            reviewers: Vec::new(),
            target_ref: None,
        }))
        .expect("setup");
        v.apply(&ViewOp::new(OpKind::CreateChange {
            id: "change-1".into(),
            owner: "operator/agent".into(),
            workspace: "bound-ws".into(),
            base_revision: h(b"base"),
            idempotency_key: "request-1".into(),
            owner_sig: None,
        }))
        .expect("setup");
        v
    };
    let empty = View::default();

    let mut out: Vec<(&'static str, View, ViewOp)> = Vec::new();
    let mut push = |name, view: &View, kind| out.push((name, view.clone(), ViewOp::new(kind)));

    // Head-moving CAS, both directions.
    push(
        "create workspace on empty",
        &empty,
        OpKind::SetWorkspaceHead {
            workspace: "ws".into(),
            commit: h(b"c1"),
            prev: None,
        },
    );
    push(
        "create workspace that exists",
        &populated,
        OpKind::SetWorkspaceHead {
            workspace: "ws".into(),
            commit: h(b"c2"),
            prev: None,
        },
    );
    push(
        "advance workspace correct prev",
        &populated,
        OpKind::SetWorkspaceHead {
            workspace: "ws".into(),
            commit: h(b"c2"),
            prev: Some(h(b"c1")),
        },
    );
    push(
        "advance workspace wrong prev",
        &populated,
        OpKind::SetWorkspaceHead {
            workspace: "ws".into(),
            commit: h(b"c2"),
            prev: Some(h(b"nope")),
        },
    );
    push(
        "set ref correct prev",
        &populated,
        OpKind::SetRef {
            name: "r".into(),
            commit: h(b"c2"),
            prev: Some(h(b"c1")),
        },
    );
    push(
        "set ref wrong prev",
        &populated,
        OpKind::SetRef {
            name: "r".into(),
            commit: h(b"c2"),
            prev: None,
        },
    );
    push(
        "delete ref correct prev",
        &populated,
        OpKind::DeleteRef {
            name: "r".into(),
            prev: Some(h(b"c1")),
        },
    );
    push(
        "delete ref wrong prev",
        &populated,
        OpKind::DeleteRef {
            name: "r".into(),
            prev: None,
        },
    );
    push(
        "delete absent ref",
        &populated,
        OpKind::DeleteRef {
            name: "gone".into(),
            prev: Some(h(b"c1")),
        },
    );
    // Deleting an absent workspace is legal; it states an end state.
    push(
        "delete absent workspace",
        &empty,
        OpKind::DeleteWorkspace {
            workspace: "nobody".into(),
        },
    );
    push(
        "delete present workspace",
        &populated,
        OpKind::DeleteWorkspace {
            workspace: "ws".into(),
        },
    );

    // Stable change creation and revision checkpoint CAS.
    push(
        "create change",
        &empty,
        OpKind::CreateChange {
            id: "change-1".into(),
            owner: "operator/agent".into(),
            workspace: "bound-ws".into(),
            base_revision: h(b"base"),
            idempotency_key: "request-1".into(),
            owner_sig: None,
        },
    );
    push(
        "duplicate change",
        &populated,
        OpKind::CreateChange {
            id: "change-1".into(),
            owner: "operator/agent".into(),
            workspace: "other-ws".into(),
            base_revision: h(b"base"),
            idempotency_key: "request-2".into(),
            owner_sig: None,
        },
    );
    push(
        "duplicate idempotency key",
        &populated,
        OpKind::CreateChange {
            id: "change-2".into(),
            owner: "operator/agent".into(),
            workspace: "other-ws".into(),
            base_revision: h(b"base"),
            idempotency_key: "request-1".into(),
            owner_sig: None,
        },
    );
    push(
        "create change on occupied workspace",
        &populated,
        OpKind::CreateChange {
            id: "change-2".into(),
            owner: "operator/agent".into(),
            workspace: "ws".into(),
            base_revision: h(b"base"),
            idempotency_key: "request-2".into(),
            owner_sig: None,
        },
    );
    push(
        "checkpoint current revision",
        &populated,
        OpKind::CheckpointChange {
            id: "change-1".into(),
            workspace: "bound-ws".into(),
            revision: h(b"checkpoint"),
            prev_revision: h(b"base"),
        },
    );
    push(
        "checkpoint stale revision",
        &populated,
        OpKind::CheckpointChange {
            id: "change-1".into(),
            workspace: "bound-ws".into(),
            revision: h(b"checkpoint"),
            prev_revision: h(b"stale"),
        },
    );
    push(
        "checkpoint wrong workspace",
        &populated,
        OpKind::CheckpointChange {
            id: "change-1".into(),
            workspace: "ws".into(),
            revision: h(b"checkpoint"),
            prev_revision: h(b"base"),
        },
    );
    push(
        "checkpoint unknown change",
        &populated,
        OpKind::CheckpointChange {
            id: "missing".into(),
            workspace: "bound-ws".into(),
            revision: h(b"checkpoint"),
            prev_revision: h(b"base"),
        },
    );
    push(
        "checkpoint no-op revision",
        &populated,
        OpKind::CheckpointChange {
            id: "change-1".into(),
            workspace: "bound-ws".into(),
            revision: h(b"base"),
            prev_revision: h(b"base"),
        },
    );
    push(
        "archive current revision",
        &populated,
        OpKind::ArchiveChange {
            id: "change-1".into(),
            workspace: "bound-ws".into(),
            prev_revision: h(b"base"),
            owner: "operator/agent".into(),
            owner_sig: witness(),
        },
    );
    push(
        "archive stale revision",
        &populated,
        OpKind::ArchiveChange {
            id: "change-1".into(),
            workspace: "bound-ws".into(),
            prev_revision: h(b"stale"),
            owner: "operator/agent".into(),
            owner_sig: witness(),
        },
    );
    push(
        "archive wrong workspace",
        &populated,
        OpKind::ArchiveChange {
            id: "change-1".into(),
            workspace: "ws".into(),
            prev_revision: h(b"base"),
            owner: "operator/agent".into(),
            owner_sig: witness(),
        },
    );
    push(
        "archive wrong owner",
        &populated,
        OpKind::ArchiveChange {
            id: "change-1".into(),
            workspace: "bound-ws".into(),
            prev_revision: h(b"base"),
            owner: "other/agent".into(),
            owner_sig: witness(),
        },
    );
    push(
        "archive unknown change",
        &populated,
        OpKind::ArchiveChange {
            id: "missing".into(),
            workspace: "bound-ws".into(),
            prev_revision: h(b"base"),
            owner: "operator/agent".into(),
            owner_sig: witness(),
        },
    );

    // Reviews.
    push(
        "new review",
        &populated,
        OpKind::RequestReview {
            id: "fresh".into(),
            target: h(b"c1"),
            reviewers: vec!["ana".into()],
            target_ref: None,
        },
    );
    push(
        "duplicate review",
        &populated,
        OpKind::RequestReview {
            id: "rev".into(),
            target: h(b"c1"),
            reviewers: vec!["ana".into()],
            target_ref: None,
        },
    );
    push(
        "assign unassigned",
        &populated,
        OpKind::AssignReviewers {
            id: "unassigned".into(),
            reviewers: vec!["bo".into()],
        },
    );
    push(
        "assign already assigned",
        &populated,
        OpKind::AssignReviewers {
            id: "rev".into(),
            reviewers: vec!["bo".into()],
        },
    );
    push(
        "assign empty list",
        &populated,
        OpKind::AssignReviewers {
            id: "unassigned".into(),
            reviewers: Vec::new(),
        },
    );
    push(
        "assign unknown review",
        &populated,
        OpKind::AssignReviewers {
            id: "ghost".into(),
            reviewers: vec!["bo".into()],
        },
    );
    push(
        "verdict from listed reviewer",
        &populated,
        OpKind::PostVerdict {
            id: "rev".into(),
            reviewer: "ana".into(),
            verdict: Verdict::Approve,
            note: String::new(),
        },
    );
    push(
        "verdict from stranger",
        &populated,
        OpKind::PostVerdict {
            id: "rev".into(),
            reviewer: "mal".into(),
            verdict: Verdict::Approve,
            note: String::new(),
        },
    );
    push(
        "verdict on unknown review",
        &populated,
        OpKind::PostVerdict {
            id: "ghost".into(),
            reviewer: "ana".into(),
            verdict: Verdict::Approve,
            note: String::new(),
        },
    );

    // Archiving, and the states around it. These preconditions moved into
    // `validate` when the archiving work merged, so they need the same
    // agreement guarantee as everything else.
    let archived = {
        let mut v = populated.clone();
        v.apply(&ViewOp::new(OpKind::PostVerdict {
            id: "rev".into(),
            reviewer: "ana".into(),
            verdict: Verdict::Approve,
            note: String::new(),
        }))
        .expect("setup");
        v.apply(&ViewOp::new(OpKind::ArchiveReview {
            id: "rev".into(),
            lapsed: false,
        }))
        .expect("setup");
        v
    };
    let complete = {
        let mut v = populated.clone();
        v.apply(&ViewOp::new(OpKind::PostVerdict {
            id: "rev".into(),
            reviewer: "ana".into(),
            verdict: Verdict::Approve,
            note: String::new(),
        }))
        .expect("setup");
        v
    };
    let slashed = {
        let mut v = complete.clone();
        v.apply(&ViewOp::new(OpKind::SlashApproval {
            id: "rev".into(),
            reviewer: "ana".into(),
            reason: "policy finding".into(),
        }))
        .expect("setup");
        v
    };
    push(
        "archive a complete review",
        &complete,
        OpKind::ArchiveReview {
            id: "rev".into(),
            lapsed: false,
        },
    );
    push(
        "archive an incomplete review",
        &populated,
        OpKind::ArchiveReview {
            id: "rev".into(),
            lapsed: false,
        },
    );
    push(
        "archive an unassigned review",
        &populated,
        OpKind::ArchiveReview {
            id: "unassigned".into(),
            lapsed: false,
        },
    );
    push(
        "archive an unknown review",
        &populated,
        OpKind::ArchiveReview {
            id: "ghost".into(),
            lapsed: false,
        },
    );
    push(
        "archive an archived review",
        &archived,
        OpKind::ArchiveReview {
            id: "rev".into(),
            lapsed: false,
        },
    );
    push(
        "verdict on an archived review",
        &archived,
        OpKind::PostVerdict {
            id: "rev".into(),
            reviewer: "ana".into(),
            verdict: Verdict::Approve,
            note: String::new(),
        },
    );
    push(
        "assign an archived review",
        &archived,
        OpKind::AssignReviewers {
            id: "rev".into(),
            reviewers: vec!["bo".into()],
        },
    );
    push(
        "slash a live approval",
        &complete,
        OpKind::SlashApproval {
            id: "rev".into(),
            reviewer: "ana".into(),
            reason: "policy finding".into(),
        },
    );
    push(
        "slash an archived approval",
        &archived,
        OpKind::SlashApproval {
            id: "rev".into(),
            reviewer: "ana".into(),
            reason: "policy finding".into(),
        },
    );
    push(
        "slash without an approval",
        &populated,
        OpKind::SlashApproval {
            id: "rev".into(),
            reviewer: "ana".into(),
            reason: "policy finding".into(),
        },
    );
    push(
        "slash an unlisted reviewer",
        &complete,
        OpKind::SlashApproval {
            id: "rev".into(),
            reviewer: "bo".into(),
            reason: "policy finding".into(),
        },
    );
    push(
        "slash an unknown review",
        &complete,
        OpKind::SlashApproval {
            id: "ghost".into(),
            reviewer: "ana".into(),
            reason: "policy finding".into(),
        },
    );
    push(
        "slash without a reason",
        &complete,
        OpKind::SlashApproval {
            id: "rev".into(),
            reviewer: "ana".into(),
            reason: String::new(),
        },
    );
    push(
        "slash twice",
        &slashed,
        OpKind::SlashApproval {
            id: "rev".into(),
            reviewer: "ana".into(),
            reason: "again".into(),
        },
    );
    push(
        "verdict after slash",
        &slashed,
        OpKind::PostVerdict {
            id: "rev".into(),
            reviewer: "ana".into(),
            verdict: Verdict::Approve,
            note: String::new(),
        },
    );

    // Comments (D38). `discussed` holds one so the duplicate-id arm --
    // which is the replay defence -- has a view that reaches it.
    let discussed = {
        let mut v = populated.clone();
        v.apply(&ViewOp::new(OpKind::PostComment {
            id: "rev".into(),
            comment: "c1".into(),
            author: "ana".into(),
            body: "said once".into(),
        }))
        .expect("setup");
        v
    };
    push(
        "comment on a live review",
        &populated,
        OpKind::PostComment {
            id: "rev".into(),
            comment: "c1".into(),
            author: "ana".into(),
            body: "b".into(),
        },
    );
    push(
        "comment from a non-reviewer",
        &populated,
        OpKind::PostComment {
            id: "rev".into(),
            comment: "c2".into(),
            author: "mal".into(),
            body: "b".into(),
        },
    );
    push(
        "comment with a taken id",
        &discussed,
        OpKind::PostComment {
            id: "rev".into(),
            comment: "c1".into(),
            author: "ana".into(),
            body: "again".into(),
        },
    );
    push(
        "comment on an unknown review",
        &populated,
        OpKind::PostComment {
            id: "ghost".into(),
            comment: "c1".into(),
            author: "ana".into(),
            body: "b".into(),
        },
    );
    push(
        "comment on an archived review",
        &archived,
        OpKind::PostComment {
            id: "rev".into(),
            comment: "c9".into(),
            author: "ana".into(),
            body: "b".into(),
        },
    );
    push(
        "comment with no id",
        &populated,
        OpKind::PostComment {
            id: "rev".into(),
            comment: String::new(),
            author: "ana".into(),
            body: "b".into(),
        },
    );
    push(
        "comment with no author",
        &populated,
        OpKind::PostComment {
            id: "rev".into(),
            comment: "c1".into(),
            author: String::new(),
            body: "b".into(),
        },
    );
    push(
        "comment with no body",
        &populated,
        OpKind::PostComment {
            id: "rev".into(),
            comment: "c1".into(),
            author: "ana".into(),
            body: String::new(),
        },
    );

    // Provenance.
    push(
        "provenance ok",
        &populated,
        OpKind::RecordProvenance {
            subject: "ws".into(),
            kind: "plan".into(),
            body: "b".into(),
        },
    );
    push(
        "provenance empty subject",
        &populated,
        OpKind::RecordProvenance {
            subject: String::new(),
            kind: "plan".into(),
            body: "b".into(),
        },
    );
    push(
        "provenance empty kind",
        &populated,
        OpKind::RecordProvenance {
            subject: "ws".into(),
            kind: String::new(),
            body: "b".into(),
        },
    );

    // Key bindings. `bound` holds one live binding and one revoked one,
    // so every arm of both variants has a view that reaches it.
    let bound = {
        let mut v = populated.clone();
        v.apply(&ViewOp::new(OpKind::BindKey {
            operator: "ana".into(),
            key: h(b"k-live"),
            channel: Some("ana/agent".into()),
        }))
        .expect("setup");
        v.apply(&ViewOp::new(OpKind::BindKey {
            operator: "bo".into(),
            key: h(b"k-dead"),
            channel: None,
        }))
        .expect("setup");
        v.apply(&ViewOp::new(OpKind::RevokeKey {
            key: h(b"k-dead"),
            reason: "rotated".into(),
        }))
        .expect("setup");
        v
    };
    push(
        "bind a fresh key",
        &populated,
        OpKind::BindKey {
            operator: "ana".into(),
            key: h(b"k-live"),
            channel: None,
        },
    );
    push(
        "bind with a bare operator channel",
        &populated,
        OpKind::BindKey {
            operator: "ana".into(),
            key: h(b"k-live"),
            channel: Some("ana".into()),
        },
    );
    push(
        "rebind same operator, new channel",
        &bound,
        OpKind::BindKey {
            operator: "ana".into(),
            key: h(b"k-live"),
            channel: Some("ana/other".into()),
        },
    );
    push(
        "rebind to a different operator",
        &bound,
        OpKind::BindKey {
            operator: "mal".into(),
            key: h(b"k-live"),
            channel: None,
        },
    );
    push(
        "bind a revoked key",
        &bound,
        OpKind::BindKey {
            operator: "bo".into(),
            key: h(b"k-dead"),
            channel: None,
        },
    );
    push(
        "bind with an empty operator",
        &populated,
        OpKind::BindKey {
            operator: String::new(),
            key: h(b"k-live"),
            channel: None,
        },
    );
    push(
        "bind with a slashed operator",
        &populated,
        OpKind::BindKey {
            operator: "ana/agent".into(),
            key: h(b"k-live"),
            channel: None,
        },
    );
    push(
        "bind with a mismatched channel",
        &populated,
        OpKind::BindKey {
            operator: "ana".into(),
            key: h(b"k-live"),
            channel: Some("mal/agent".into()),
        },
    );
    push(
        "bind with an empty channel",
        &populated,
        OpKind::BindKey {
            operator: "ana".into(),
            key: h(b"k-live"),
            channel: Some(String::new()),
        },
    );
    push(
        "revoke a live binding",
        &bound,
        OpKind::RevokeKey {
            key: h(b"k-live"),
            reason: "compromised".into(),
        },
    );
    push(
        "revoke twice",
        &bound,
        OpKind::RevokeKey {
            key: h(b"k-dead"),
            reason: "again".into(),
        },
    );
    push(
        "revoke an unbound key",
        &bound,
        OpKind::RevokeKey {
            key: h(b"k-stranger"),
            reason: "compromised".into(),
        },
    );
    push(
        "revoke without a reason",
        &bound,
        OpKind::RevokeKey {
            key: h(b"k-live"),
            reason: String::new(),
        },
    );

    // Ref snapshots (D25). `attested` holds one admitted snapshot so the
    // chain arm has a view that reaches it.
    let attested = {
        let mut v = populated.clone();
        let snapshot = v.snapshot();
        v.apply(&ViewOp::new(OpKind::RecordRefSnapshot { snapshot }))
            .expect("setup");
        v
    };
    push(
        "snapshot of the current state",
        &populated,
        OpKind::RecordRefSnapshot {
            snapshot: populated.snapshot(),
        },
    );
    push(
        "snapshot chained onto the latest",
        &attested,
        OpKind::RecordRefSnapshot {
            snapshot: attested.snapshot(),
        },
    );
    push(
        "snapshot with a lying ref map",
        &populated,
        OpKind::RecordRefSnapshot {
            snapshot: RefSnapshot {
                refs: BTreeMap::new(),
                ..populated.snapshot()
            },
        },
    );
    push(
        "snapshot at a stale position",
        &attested,
        OpKind::RecordRefSnapshot {
            snapshot: RefSnapshot {
                at_seq: 0,
                ..attested.snapshot()
            },
        },
    );
    push(
        "snapshot with a broken chain",
        &attested,
        OpKind::RecordRefSnapshot {
            snapshot: RefSnapshot {
                prev_snapshot: None,
                ..attested.snapshot()
            },
        },
    );

    out
}

#[test]
fn validate_agrees_with_apply_on_every_op_shape() {
    for (name, view, op) in cases() {
        let validated = view.validate(&op);
        let mut applied_to = view.clone();
        let applied = applied_to.apply(&op);
        assert_eq!(
            validated.is_ok(),
            applied.is_ok(),
            "{name}: validate said {:?} but apply said {:?}",
            validated.map(|()| "ok"),
            applied.map(|()| "ok"),
        );
    }
}

#[test]
fn validate_never_mutates() {
    for (name, view, op) in cases() {
        let probe = view.clone();
        let _ = probe.validate(&op);
        assert_eq!(probe, view, "{name}: validate mutated the view");
    }
}

/// A rejected `apply` must leave the view untouched. This was already true
/// and is the reason the trial clone was never buying atomicity, only
/// "do not commit yet" — which `validate` gives without the copy.
#[test]
fn a_rejected_apply_changes_nothing() {
    for (name, view, op) in cases() {
        let mut probe = view.clone();
        if probe.apply(&op).is_err() {
            assert_eq!(probe, view, "{name}: a rejected apply mutated the view");
        }
    }
}
