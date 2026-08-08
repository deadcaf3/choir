//! L1 acceptance: deterministic replay, CAS rejection, prefix-replay
//! undo, and first-class conflict commits that work continues on top of.

use std::collections::BTreeMap;

use choir_hash::ContentHash;
use choir_oplog::MemLog;
use choir_store::{put_blob, ChunkerParams, MemStore};
use choir_view::{append_op, Commit, OpKind, TreeEntry, View, ViewError, ViewOp};

fn set_head(ws: &str, commit: &ContentHash, prev: Option<&ContentHash>) -> ViewOp {
    ViewOp::new(OpKind::SetWorkspaceHead {
        workspace: ws.into(),
        commit: commit.clone(),
        prev: prev.cloned(),
    })
}

fn commit(
    store: &mut MemStore,
    parents: &[&ContentHash],
    files: &[(&str, &str)],
    message: &str,
) -> ContentHash {
    let mut tree = BTreeMap::new();
    for (path, content) in files {
        let blob = put_blob(store, content.as_bytes(), ChunkerParams::default()).unwrap();
        tree.insert(path.to_string(), TreeEntry::File { blob });
    }
    Commit {
        format_version: choir_view::FORMAT_VERSION,
        parents: parents.iter().map(|p| (*p).clone()).collect(),
        tree,
        author: "test".into(),
        message: message.into(),
    }
    .put(store)
    .unwrap()
}

#[test]
fn replay_is_deterministic_and_ordered() {
    let mut store = MemStore::new();
    let mut log = MemLog::new();

    let c1 = commit(&mut store, &[], &[("a.txt", "one\n")], "c1");
    let c2 = commit(&mut store, &[&c1], &[("a.txt", "two\n")], "c2");

    append_op(&mut log, "w1", set_head("w1", &c1, None)).unwrap();
    append_op(&mut log, "w2", set_head("w2", &c1, None)).unwrap();
    append_op(&mut log, "w1", set_head("w1", &c2, Some(&c1))).unwrap();

    let v1 = View::materialize(&log).unwrap();
    let v2 = View::materialize(&log).unwrap();
    assert_eq!(v1, v2, "same log must fold to the same view");
    assert_eq!(v1.workspaces.get("w1"), Some(&c2));
    assert_eq!(v1.workspaces.get("w2"), Some(&c1));
}

#[test]
fn stale_cas_is_rejected_and_log_unchanged() {
    let mut store = MemStore::new();
    let mut log = MemLog::new();

    let c1 = commit(&mut store, &[], &[("a.txt", "one\n")], "c1");
    let c2 = commit(&mut store, &[&c1], &[("a.txt", "two\n")], "c2");
    let c3 = commit(&mut store, &[&c1], &[("a.txt", "three\n")], "c3");

    append_op(&mut log, "w1", set_head("w1", &c1, None)).unwrap();
    append_op(&mut log, "w1", set_head("w1", &c2, Some(&c1))).unwrap();

    // A second writer still believing the head is c1 must be rejected.
    let stale = append_op(&mut log, "w1", set_head("w1", &c3, Some(&c1)));
    assert!(matches!(stale, Err(ViewError::StaleHead { .. })));
    assert_eq!(log_len(&log), 2, "rejected op must not reach the log");
    assert_eq!(
        View::materialize(&log).unwrap().workspaces.get("w1"),
        Some(&c2)
    );
}

fn log_len(log: &MemLog) -> u64 {
    use choir_oplog::OpLog;
    log.len()
}

#[test]
fn prefix_replay_is_undo() {
    let mut store = MemStore::new();
    let mut log = MemLog::new();

    let c1 = commit(&mut store, &[], &[("a.txt", "one\n")], "c1");
    let c2 = commit(&mut store, &[&c1], &[("a.txt", "two\n")], "c2");

    append_op(&mut log, "w1", set_head("w1", &c1, None)).unwrap();
    let before = View::materialize(&log).unwrap();
    append_op(&mut log, "w1", set_head("w1", &c2, Some(&c1))).unwrap();

    let undone = View::at(&log, 1).unwrap();
    assert_eq!(undone, before, "view at op 1 must equal the pre-op view");
    assert_ne!(undone, View::materialize(&log).unwrap());
}

#[test]
fn conflicted_commit_is_valid_and_buildable_upon() {
    let mut store = MemStore::new();
    let mut log = MemLog::new();

    let base = put_blob(&mut store, b"base\n", ChunkerParams::default()).unwrap();
    let left = put_blob(&mut store, b"left\n", ChunkerParams::default()).unwrap();
    let right = put_blob(&mut store, b"right\n", ChunkerParams::default()).unwrap();

    let mut tree = BTreeMap::new();
    tree.insert(
        "hot.txt".to_string(),
        TreeEntry::Conflict {
            base: Some(base),
            left,
            right,
        },
    );
    let merge = Commit {
        format_version: choir_view::FORMAT_VERSION,
        parents: vec![],
        tree,
        author: "test".into(),
        message: "conflicted merge".into(),
    };
    assert!(merge.is_conflicted());
    let merge_id = merge.put(&mut store).unwrap();

    // The conflicted commit becomes a workspace head — not an error state.
    append_op(&mut log, "w1", set_head("w1", &merge_id, None)).unwrap();

    // Work continues on top: a child commit resolves the path.
    let resolved = commit(&mut store, &[&merge_id], &[("hot.txt", "resolved\n")], "fix");
    append_op(&mut log, "w1", set_head("w1", &resolved, Some(&merge_id))).unwrap();

    let view = View::materialize(&log).unwrap();
    let head = Commit::get(&store, view.workspaces.get("w1").unwrap()).unwrap();
    assert!(!head.is_conflicted());
    assert_eq!(head.parents, vec![merge_id]);
}

#[test]
fn commit_roundtrips_through_store() {
    let mut store = MemStore::new();
    let c1 = commit(&mut store, &[], &[("a.txt", "one\n"), ("b.txt", "two\n")], "c1");
    let loaded = Commit::get(&store, &c1).unwrap();
    assert_eq!(loaded.tree.len(), 2);
    assert_eq!(loaded.message, "c1");
    assert_eq!(loaded.put(&mut store).unwrap(), c1, "re-store is stable");
}

#[test]
fn review_fan_out_semantics() {
    let mut log = MemLog::new();
    let target = ContentHash::blake3(b"change under review");
    let request = ViewOp::new(OpKind::RequestReview {
        id: "r1".into(),
        target: target.clone(),
        reviewers: vec!["ana".into(), "bot-reviewer".into()],
        target_ref: None,
    });
    append_op(&mut log, "author", request.clone()).unwrap();

    // Duplicate id is rejected; unknown review and non-reviewer too.
    assert!(matches!(
        append_op(&mut log, "author", request),
        Err(ViewError::Review(_))
    ));
    let verdict = |id: &str, who: &str, v: choir_view::Verdict, note: &str| {
        ViewOp::new(OpKind::PostVerdict {
            id: id.into(),
            reviewer: who.into(),
            verdict: v,
            note: note.into(),
        })
    };
    assert!(matches!(
        append_op(&mut log, "ana", verdict("nope", "ana", choir_view::Verdict::Approve, "")),
        Err(ViewError::Review(_))
    ));
    assert!(matches!(
        append_op(&mut log, "mallory", verdict("r1", "mallory", choir_view::Verdict::Approve, "")),
        Err(ViewError::Review(_))
    ));

    // First verdict: incomplete. RequestChanges: complete but not approved.
    append_op(&mut log, "ana", verdict("r1", "ana", choir_view::Verdict::Approve, "lgtm")).unwrap();
    let view = View::materialize(&log).unwrap();
    let r = view.reviews.get("r1").unwrap();
    assert!(!r.complete() && !r.approved());

    append_op(
        &mut log,
        "bot-reviewer",
        verdict("r1", "bot-reviewer", choir_view::Verdict::RequestChanges, "missing test"),
    )
    .unwrap();
    let view = View::materialize(&log).unwrap();
    let r = view.reviews.get("r1").unwrap();
    assert!(r.complete() && !r.approved());

    // Re-review overwrites the reviewer's own verdict: now approved.
    append_op(
        &mut log,
        "bot-reviewer",
        verdict("r1", "bot-reviewer", choir_view::Verdict::Approve, "test added"),
    )
    .unwrap();
    let view = View::materialize(&log).unwrap();
    let r = view.reviews.get("r1").unwrap();
    assert!(r.approved());
    assert_eq!(r.target.as_ref(), Some(&target));
    assert_eq!(r.verdicts.len(), 2);
}

#[test]
fn unassigned_reviews_are_never_complete_and_assign_once() {
    let mut log = MemLog::new();
    append_op(
        &mut log,
        "author",
        ViewOp::new(OpKind::RequestReview {
            id: "r1".into(),
            target: ContentHash::blake3(b"change"),
            reviewers: Vec::new(),
            target_ref: None,
        }),
    )
    .unwrap();
    let assign = |id: &str, who: &[&str]| {
        ViewOp::new(OpKind::AssignReviewers {
            id: id.into(),
            reviewers: who.iter().map(|s| (*s).to_string()).collect(),
        })
    };

    // A review nobody was asked to do is not a passed review.
    let view = View::materialize(&log).unwrap();
    let r = view.reviews.get("r1").unwrap();
    assert!(!r.complete() && !r.approved(), "vacuous approval");

    // Empty assignment and unknown review are both rejected.
    assert!(matches!(
        append_op(&mut log, "node", assign("r1", &[])),
        Err(ViewError::Review(_))
    ));
    assert!(matches!(
        append_op(&mut log, "node", assign("nope", &["ana"])),
        Err(ViewError::Review(_))
    ));

    append_op(&mut log, "node", assign("r1", &["ana"])).unwrap();
    let view = View::materialize(&log).unwrap();
    assert_eq!(view.reviews.get("r1").unwrap().reviewers, vec!["ana"]);

    // Assign-once: reviewers cannot be swapped out mid-review, so a
    // hostile re-draw cannot replace a reviewer who would say no.
    assert!(matches!(
        append_op(&mut log, "node", assign("r1", &["friend"])),
        Err(ViewError::Review(_))
    ));

    append_op(
        &mut log,
        "ana",
        ViewOp::new(OpKind::PostVerdict {
            id: "r1".into(),
            reviewer: "ana".into(),
            verdict: choir_view::Verdict::Approve,
            note: String::new(),
        }),
    )
    .unwrap();
    assert!(View::materialize(&log).unwrap().reviews.get("r1").unwrap().approved());
}

#[test]
fn provenance_records_latest_wins() {
    let mut log = MemLog::new();
    let record = |subject: &str, kind: &str, body: &str| {
        ViewOp::new(OpKind::RecordProvenance {
            subject: subject.into(),
            kind: kind.into(),
            body: body.into(),
        })
    };

    // Empty subject or kind is rejected; the log stays clean.
    assert!(matches!(
        append_op(&mut log, "agent-1", record("", "task-spec", "x")),
        Err(ViewError::Provenance(_))
    ));
    assert!(matches!(
        append_op(&mut log, "agent-1", record("agent-1", "", "x")),
        Err(ViewError::Provenance(_))
    ));
    assert_eq!(choir_oplog::OpLog::len(&log), 0);

    // Records accumulate per (subject, kind); latest body wins.
    append_op(&mut log, "agent-1", record("agent-1", "task-spec", "add auth")).unwrap();
    append_op(&mut log, "agent-1", record("agent-1", "plan", "1. schema 2. api")).unwrap();
    append_op(&mut log, "agent-2", record("repo/shared", "task-spec", "living spec v1")).unwrap();
    append_op(&mut log, "agent-1", record("agent-1", "task-spec", "add auth + rate limit")).unwrap();

    let view = View::materialize(&log).unwrap();
    assert_eq!(view.provenance["agent-1"]["task-spec"], "add auth + rate limit");
    assert_eq!(view.provenance["agent-1"]["plan"], "1. schema 2. api");
    assert_eq!(view.provenance["repo/shared"]["task-spec"], "living spec v1");

    // An empty body is a visible withdrawn state, not a deletion.
    append_op(&mut log, "agent-2", record("repo/shared", "task-spec", "")).unwrap();
    let view = View::materialize(&log).unwrap();
    assert_eq!(view.provenance["repo/shared"]["task-spec"], "");

    // Prefix replay shows the earlier spec — history is in the log.
    let earlier = View::at(&log, 3).unwrap();
    assert_eq!(earlier.provenance["agent-1"]["task-spec"], "add auth");
}

#[test]
fn target_ref_is_additive_and_old_payloads_hash_the_same() {
    // Invariant 1, for an enum variant: a log written before
    // `target_ref` existed must decode *and* re-serialize to the same
    // bytes, or every entry hash in every existing log moves and the
    // chain no longer verifies. `#[serde(default, skip_serializing_if)]`
    // is what buys that, and this test is what proves it stayed.
    let target = ContentHash::blake3(b"under review");
    let target_json = serde_json::to_string(&target).unwrap();
    let old = format!(
        r#"{{"format_version":1,"kind":{{"RequestReview":{{"id":"r1","target":{target_json},"reviewers":["ana"]}}}}}}"#
    );
    let op = ViewOp::from_payload(old.as_bytes()).expect("old payload still decodes");
    assert_eq!(
        String::from_utf8(op.to_payload()).unwrap(),
        old,
        "re-serializing an old payload changed its bytes, so its hash moved"
    );
    assert!(
        matches!(&op.kind, OpKind::RequestReview { target_ref: None, .. }),
        "absent field must read as unbound, not as some default ref"
    );

    // And a bound review round-trips, carrying the ref into the view.
    let bound = ViewOp::new(OpKind::RequestReview {
        id: "r2".into(),
        target: target.clone(),
        reviewers: vec!["ana".into()],
        target_ref: Some("choir/choir.git:refs/heads/main".into()),
    });
    let wire = bound.to_payload();
    assert_eq!(ViewOp::from_payload(&wire).unwrap(), bound, "bound review round-trips");

    let mut log = MemLog::new();
    append_op(&mut log, "author", op).unwrap();
    append_op(&mut log, "author", bound).unwrap();
    let view = View::materialize(&log).unwrap();
    assert_eq!(view.reviews["r1"].target_ref, None);
    assert_eq!(
        view.reviews["r2"].target_ref.as_deref(),
        Some("choir/choir.git:refs/heads/main")
    );
}

#[test]
fn archiving_freezes_a_review_and_keeps_its_outcome() {
    // Retention must not become an authorization decision: the landing
    // gate reads (target_ref, target, approved), so archiving keeps that
    // triple and drops the bulk. The trap is that approval is NOT
    // monotonic -- PostVerdict overwrites -- so once verdicts are gone
    // the outcome can no longer be recomputed and must be stored.
    let mut log = MemLog::new();
    let target = ContentHash::blake3(b"change");
    append_op(
        &mut log,
        "author",
        ViewOp::new(OpKind::RequestReview {
            id: "r1".into(),
            target: target.clone(),
            reviewers: vec!["ana".into(), "bot".into()],
            target_ref: Some("demo.git:refs/heads/main".into()),
        }),
    )
    .unwrap();
    let verdict = |who: &str, v: choir_view::Verdict| {
        ViewOp::new(OpKind::PostVerdict {
            id: "r1".into(),
            reviewer: who.into(),
            verdict: v,
            note: "reasoning that takes space".into(),
        })
    };
    let archive = ViewOp::new(OpKind::ArchiveReview { id: "r1".into() });

    // Incomplete: archiving would strand it, since no further verdict
    // could ever decide the outcome.
    append_op(&mut log, "ana", verdict("ana", choir_view::Verdict::Approve)).unwrap();
    assert!(matches!(
        append_op(&mut log, "node", archive.clone()),
        Err(ViewError::Review(_))
    ));

    append_op(&mut log, "bot", verdict("bot", choir_view::Verdict::Approve)).unwrap();
    let view = View::materialize(&log).unwrap();
    assert!(view.reviews["r1"].approved());

    append_op(&mut log, "node", archive.clone()).unwrap();
    let view = View::materialize(&log).unwrap();
    let r = &view.reviews["r1"];

    // Outcome survives; bulk does not.
    assert!(r.approved(), "archived approval must not evaporate");
    assert!(r.complete(), "archived reviews are settled, not unfinished");
    assert!(r.verdicts.is_empty(), "verdicts should have been dropped");
    assert!(r.reviewers.is_empty(), "reviewer list should have been dropped");
    // The gate's other two fields are untouched.
    assert_eq!(r.target.as_ref(), Some(&target));
    assert_eq!(r.target_ref.as_deref(), Some("demo.git:refs/heads/main"));
    assert!(matches!(
        r.status,
        choir_view::ReviewStatus::Archived { approved: true }
    ));

    // Frozen: no further verdicts, and the refusal says archived rather
    // than absent, or a reviewer goes hunting for a typo.
    let err = append_op(&mut log, "ana", verdict("ana", choir_view::Verdict::RequestChanges));
    match err {
        Err(ViewError::Review(msg)) => assert!(msg.contains("archived"), "{msg}"),
        other => panic!("expected an archived refusal, got {other:?}"),
    }
    // Double archive, and assignment onto an emptied list, both refused.
    assert!(matches!(
        append_op(&mut log, "node", archive),
        Err(ViewError::Review(_))
    ));
    assert!(matches!(
        append_op(
            &mut log,
            "node",
            ViewOp::new(OpKind::AssignReviewers {
                id: "r1".into(),
                reviewers: vec!["carol".into()],
            })
        ),
        Err(ViewError::Review(_))
    ));

    // A rejected verdict must not have disturbed the frozen outcome.
    let view = View::materialize(&log).unwrap();
    assert!(view.reviews["r1"].approved());
}

#[test]
fn archiving_preserves_a_rejection_too() {
    // The dangerous direction: if archiving lost the outcome and fell
    // back to recomputing from an emptied verdict map, a RequestChanges
    // review would silently read as approved -- vacuously, since "all of
    // no verdicts are approvals".
    let mut log = MemLog::new();
    append_op(
        &mut log,
        "author",
        ViewOp::new(OpKind::RequestReview {
            id: "r2".into(),
            target: ContentHash::blake3(b"bad"),
            reviewers: vec!["ana".into()],
            target_ref: None,
        }),
    )
    .unwrap();
    append_op(
        &mut log,
        "ana",
        ViewOp::new(OpKind::PostVerdict {
            id: "r2".into(),
            reviewer: "ana".into(),
            verdict: choir_view::Verdict::RequestChanges,
            note: "no".into(),
        }),
    )
    .unwrap();
    append_op(&mut log, "node", ViewOp::new(OpKind::ArchiveReview { id: "r2".into() })).unwrap();

    let view = View::materialize(&log).unwrap();
    assert!(
        !view.reviews["r2"].approved(),
        "an archived rejection must not read as approved"
    );
}
