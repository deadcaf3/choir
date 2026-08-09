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

use choir_hash::ContentHash;
use choir_oplog::Witness;
use choir_view::{OpKind, Verdict, View, ViewOp};

fn h(tag: &[u8]) -> ContentHash {
    ContentHash::blake3(tag)
}

fn witness() -> Witness {
    Witness {
        key_id: "owner-key".into(),
        signature: vec![1, 2, 3],
    }
}

/// Every op shape, in both admissible and inadmissible states. Hand-rolled
/// rather than generated: the repo hand-rolls its randomness elsewhere for
/// the same reason (no `rand`/`proptest` dependency), and an enumerated set
/// covering each variant's success and failure arms is more legible than a
/// generator for eight variants.
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
        }))
        .expect("setup");
        v
    };
    let empty = View::default();

    let mut out: Vec<(&'static str, View, ViewOp)> = Vec::new();
    let mut push = |name, view: &View, kind| out.push((name, view.clone(), ViewOp::new(kind)));

    // Head-moving CAS, both directions.
    push("create workspace on empty", &empty, OpKind::SetWorkspaceHead {
        workspace: "ws".into(), commit: h(b"c1"), prev: None });
    push("create workspace that exists", &populated, OpKind::SetWorkspaceHead {
        workspace: "ws".into(), commit: h(b"c2"), prev: None });
    push("advance workspace correct prev", &populated, OpKind::SetWorkspaceHead {
        workspace: "ws".into(), commit: h(b"c2"), prev: Some(h(b"c1")) });
    push("advance workspace wrong prev", &populated, OpKind::SetWorkspaceHead {
        workspace: "ws".into(), commit: h(b"c2"), prev: Some(h(b"nope")) });
    push("set ref correct prev", &populated, OpKind::SetRef {
        name: "r".into(), commit: h(b"c2"), prev: Some(h(b"c1")) });
    push("set ref wrong prev", &populated, OpKind::SetRef {
        name: "r".into(), commit: h(b"c2"), prev: None });
    push("delete ref correct prev", &populated, OpKind::DeleteRef {
        name: "r".into(), prev: Some(h(b"c1")) });
    push("delete ref wrong prev", &populated, OpKind::DeleteRef {
        name: "r".into(), prev: None });
    push("delete absent ref", &populated, OpKind::DeleteRef {
        name: "gone".into(), prev: Some(h(b"c1")) });
    // Deleting an absent workspace is legal; it states an end state.
    push("delete absent workspace", &empty, OpKind::DeleteWorkspace {
        workspace: "nobody".into() });
    push("delete present workspace", &populated, OpKind::DeleteWorkspace {
        workspace: "ws".into() });

    // Stable change creation and revision checkpoint CAS.
    push("create change", &empty, OpKind::CreateChange {
        id: "change-1".into(), owner: "operator/agent".into(), workspace: "bound-ws".into(),
        base_revision: h(b"base"), idempotency_key: "request-1".into() });
    push("duplicate change", &populated, OpKind::CreateChange {
        id: "change-1".into(), owner: "operator/agent".into(), workspace: "other-ws".into(),
        base_revision: h(b"base"), idempotency_key: "request-2".into() });
    push("duplicate idempotency key", &populated, OpKind::CreateChange {
        id: "change-2".into(), owner: "operator/agent".into(), workspace: "other-ws".into(),
        base_revision: h(b"base"), idempotency_key: "request-1".into() });
    push("create change on occupied workspace", &populated, OpKind::CreateChange {
        id: "change-2".into(), owner: "operator/agent".into(), workspace: "ws".into(),
        base_revision: h(b"base"), idempotency_key: "request-2".into() });
    push("checkpoint current revision", &populated, OpKind::CheckpointChange {
        id: "change-1".into(), workspace: "bound-ws".into(), revision: h(b"checkpoint"),
        prev_revision: h(b"base") });
    push("checkpoint stale revision", &populated, OpKind::CheckpointChange {
        id: "change-1".into(), workspace: "bound-ws".into(), revision: h(b"checkpoint"),
        prev_revision: h(b"stale") });
    push("checkpoint wrong workspace", &populated, OpKind::CheckpointChange {
        id: "change-1".into(), workspace: "ws".into(), revision: h(b"checkpoint"),
        prev_revision: h(b"base") });
    push("checkpoint unknown change", &populated, OpKind::CheckpointChange {
        id: "missing".into(), workspace: "bound-ws".into(), revision: h(b"checkpoint"),
        prev_revision: h(b"base") });
    push("checkpoint no-op revision", &populated, OpKind::CheckpointChange {
        id: "change-1".into(), workspace: "bound-ws".into(), revision: h(b"base"),
        prev_revision: h(b"base") });
    push("archive current revision", &populated, OpKind::ArchiveChange {
        id: "change-1".into(), workspace: "bound-ws".into(), prev_revision: h(b"base"),
        owner: "operator/agent".into(), owner_sig: witness() });
    push("archive stale revision", &populated, OpKind::ArchiveChange {
        id: "change-1".into(), workspace: "bound-ws".into(), prev_revision: h(b"stale"),
        owner: "operator/agent".into(), owner_sig: witness() });
    push("archive wrong workspace", &populated, OpKind::ArchiveChange {
        id: "change-1".into(), workspace: "ws".into(), prev_revision: h(b"base"),
        owner: "operator/agent".into(), owner_sig: witness() });
    push("archive wrong owner", &populated, OpKind::ArchiveChange {
        id: "change-1".into(), workspace: "bound-ws".into(), prev_revision: h(b"base"),
        owner: "other/agent".into(), owner_sig: witness() });
    push("archive unknown change", &populated, OpKind::ArchiveChange {
        id: "missing".into(), workspace: "bound-ws".into(), prev_revision: h(b"base"),
        owner: "operator/agent".into(), owner_sig: witness() });

    // Reviews.
    push("new review", &populated, OpKind::RequestReview {
        id: "fresh".into(), target: h(b"c1"), reviewers: vec!["ana".into()], target_ref: None });
    push("duplicate review", &populated, OpKind::RequestReview {
        id: "rev".into(), target: h(b"c1"), reviewers: vec!["ana".into()], target_ref: None });
    push("assign unassigned", &populated, OpKind::AssignReviewers {
        id: "unassigned".into(), reviewers: vec!["bo".into()] });
    push("assign already assigned", &populated, OpKind::AssignReviewers {
        id: "rev".into(), reviewers: vec!["bo".into()] });
    push("assign empty list", &populated, OpKind::AssignReviewers {
        id: "unassigned".into(), reviewers: Vec::new() });
    push("assign unknown review", &populated, OpKind::AssignReviewers {
        id: "ghost".into(), reviewers: vec!["bo".into()] });
    push("verdict from listed reviewer", &populated, OpKind::PostVerdict {
        id: "rev".into(), reviewer: "ana".into(), verdict: Verdict::Approve, note: String::new() });
    push("verdict from stranger", &populated, OpKind::PostVerdict {
        id: "rev".into(), reviewer: "mal".into(), verdict: Verdict::Approve, note: String::new() });
    push("verdict on unknown review", &populated, OpKind::PostVerdict {
        id: "ghost".into(), reviewer: "ana".into(), verdict: Verdict::Approve, note: String::new() });

    // Archiving, and the states around it. These preconditions moved into
    // `validate` when the archiving work merged, so they need the same
    // agreement guarantee as everything else.
    let archived = {
        let mut v = populated.clone();
        v.apply(&ViewOp::new(OpKind::PostVerdict {
            id: "rev".into(), reviewer: "ana".into(),
            verdict: Verdict::Approve, note: String::new() })).expect("setup");
        v.apply(&ViewOp::new(OpKind::ArchiveReview { id: "rev".into(), lapsed: false })).expect("setup");
        v
    };
    let complete = {
        let mut v = populated.clone();
        v.apply(&ViewOp::new(OpKind::PostVerdict {
            id: "rev".into(), reviewer: "ana".into(),
            verdict: Verdict::Approve, note: String::new() })).expect("setup");
        v
    };
    push("archive a complete review", &complete, OpKind::ArchiveReview { id: "rev".into(), lapsed: false });
    push("archive an incomplete review", &populated, OpKind::ArchiveReview { id: "rev".into(), lapsed: false });
    push("archive an unassigned review", &populated, OpKind::ArchiveReview { id: "unassigned".into(), lapsed: false });
    push("archive an unknown review", &populated, OpKind::ArchiveReview { id: "ghost".into(), lapsed: false });
    push("archive an archived review", &archived, OpKind::ArchiveReview { id: "rev".into(), lapsed: false });
    push("verdict on an archived review", &archived, OpKind::PostVerdict {
        id: "rev".into(), reviewer: "ana".into(), verdict: Verdict::Approve, note: String::new() });
    push("assign an archived review", &archived, OpKind::AssignReviewers {
        id: "rev".into(), reviewers: vec!["bo".into()] });

    // Provenance.
    push("provenance ok", &populated, OpKind::RecordProvenance {
        subject: "ws".into(), kind: "plan".into(), body: "b".into() });
    push("provenance empty subject", &populated, OpKind::RecordProvenance {
        subject: String::new(), kind: "plan".into(), body: "b".into() });
    push("provenance empty kind", &populated, OpKind::RecordProvenance {
        subject: "ws".into(), kind: String::new(), body: "b".into() });

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
