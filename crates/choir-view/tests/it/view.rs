//! L1 acceptance: deterministic replay, CAS rejection, prefix-replay
//! undo, and first-class conflict commits that work continues on top of.

use std::collections::BTreeMap;

use choir_hash::ContentHash;
use choir_oplog::{MemLog, Witness};
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
        resolves: None,
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

#[test]
fn stable_change_survives_checkpoints_and_workspace_archive() {
    let mut log = MemLog::new();
    let base = ContentHash::blake3(b"base");
    let checkpoint = ContentHash::blake3(b"checkpoint");
    let create = ViewOp::new(OpKind::CreateChange {
        id: "change-1".into(),
        owner: "operator/agent".into(),
        workspace: "repo/agent".into(),
        base_revision: base.clone(),
        idempotency_key: "request-1".into(),
        owner_sig: None,
        cone: Vec::new(),
    });
    append_op(&mut log, "node", create).unwrap();

    let created = View::materialize(&log).unwrap();
    let change = created.changes.get("change-1").unwrap();
    assert_eq!(change.revision_id, base);
    assert_eq!(change.active_workspace.as_deref(), Some("repo/agent"));
    assert_eq!(created.workspaces.get("repo/agent"), Some(&base));

    append_op(
        &mut log,
        "operator/agent",
        ViewOp::new(OpKind::CheckpointChange {
            id: "change-1".into(),
            workspace: "repo/agent".into(),
            revision: checkpoint.clone(),
            prev_revision: base,
        }),
    )
    .unwrap();
    let checkpointed = View::materialize(&log).unwrap();
    assert_eq!(checkpointed.changes["change-1"].revision_id, checkpoint);
    assert_eq!(checkpointed.workspaces["repo/agent"], checkpoint);

    append_op(
        &mut log,
        "node",
        ViewOp::new(OpKind::ArchiveChange {
            id: "change-1".into(),
            workspace: "repo/agent".into(),
            prev_revision: checkpoint.clone(),
            owner: "operator/agent".into(),
            owner_sig: Witness::ed25519("owner-key", vec![1, 2, 3]),
        }),
    )
    .unwrap();
    let archived = View::materialize(&log).unwrap();
    assert!(!archived.workspaces.contains_key("repo/agent"));
    assert_eq!(archived.changes["change-1"].active_workspace, None);
    assert_eq!(archived.changes["change-1"].revision_id, checkpoint);

    let mut reopened = archived;
    reopened
        .apply(&ViewOp::new(OpKind::CreateChange {
            id: "change-2".into(),
            owner: "operator/agent".into(),
            workspace: "repo/agent".into(),
            base_revision: checkpoint,
            idempotency_key: "request-2".into(),
            owner_sig: None,
            cone: Vec::new(),
        }))
        .unwrap();
    assert_eq!(
        reopened.changes["change-2"].active_workspace.as_deref(),
        Some("repo/agent")
    );
}

#[test]
fn change_checkpoint_rejects_stale_revision_without_mutation() {
    let base = ContentHash::blake3(b"base");
    let mut view = View::default();
    view.apply(&ViewOp::new(OpKind::CreateChange {
        id: "change-1".into(),
        owner: "operator/agent".into(),
        workspace: "repo/agent".into(),
        base_revision: base,
        idempotency_key: "request-1".into(),
        owner_sig: None,
        cone: Vec::new(),
    }))
    .unwrap();
    let before = view.clone();
    let result = view.apply(&ViewOp::new(OpKind::CheckpointChange {
        id: "change-1".into(),
        workspace: "repo/agent".into(),
        revision: ContentHash::blake3(b"checkpoint"),
        prev_revision: ContentHash::blake3(b"stale"),
    }));
    assert!(matches!(result, Err(ViewError::StaleHead { .. })));
    assert_eq!(view, before);
}

#[test]
fn legacy_workspace_move_detaches_change_identity() {
    let base = ContentHash::blake3(b"base");
    let moved = ContentHash::blake3(b"legacy move");
    let mut view = View::default();
    view.apply(&ViewOp::new(OpKind::CreateChange {
        id: "change-1".into(),
        owner: "operator/agent".into(),
        workspace: "repo/agent".into(),
        base_revision: base.clone(),
        idempotency_key: "request-1".into(),
        owner_sig: None,
        cone: Vec::new(),
    }))
    .unwrap();
    view.apply(&set_head("repo/agent", &moved, Some(&base)))
        .unwrap();
    assert_eq!(view.workspaces["repo/agent"], moved);
    assert_eq!(view.changes["change-1"].active_workspace, None);
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
        resolves: None,
    };
    assert!(merge.is_conflicted());
    let merge_id = merge.put(&mut store).unwrap();

    // The conflicted commit becomes a workspace head — not an error state.
    append_op(&mut log, "w1", set_head("w1", &merge_id, None)).unwrap();

    // Work continues on top: a child commit resolves the path.
    let resolved = commit(
        &mut store,
        &[&merge_id],
        &[("hot.txt", "resolved\n")],
        "fix",
    );
    append_op(&mut log, "w1", set_head("w1", &resolved, Some(&merge_id))).unwrap();

    let view = View::materialize(&log).unwrap();
    let head = Commit::get(&store, view.workspaces.get("w1").unwrap()).unwrap();
    assert!(!head.is_conflicted());
    assert_eq!(head.parents, vec![merge_id]);
}

#[test]
fn commit_roundtrips_through_store() {
    let mut store = MemStore::new();
    let c1 = commit(
        &mut store,
        &[],
        &[("a.txt", "one\n"), ("b.txt", "two\n")],
        "c1",
    );
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
        append_op(
            &mut log,
            "ana",
            verdict("nope", "ana", choir_view::Verdict::Approve, "")
        ),
        Err(ViewError::Review(_))
    ));
    assert!(matches!(
        append_op(
            &mut log,
            "mallory",
            verdict("r1", "mallory", choir_view::Verdict::Approve, "")
        ),
        Err(ViewError::Review(_))
    ));

    // First verdict: incomplete. RequestChanges: complete but not approved.
    append_op(
        &mut log,
        "ana",
        verdict("r1", "ana", choir_view::Verdict::Approve, "lgtm"),
    )
    .unwrap();
    let view = View::materialize(&log).unwrap();
    let r = view.reviews.get("r1").unwrap();
    assert!(!r.complete() && !r.approved());

    append_op(
        &mut log,
        "bot-reviewer",
        verdict(
            "r1",
            "bot-reviewer",
            choir_view::Verdict::RequestChanges,
            "missing test",
        ),
    )
    .unwrap();
    let view = View::materialize(&log).unwrap();
    let r = view.reviews.get("r1").unwrap();
    assert!(r.complete() && !r.approved());

    // Re-review overwrites the reviewer's own verdict: now approved.
    append_op(
        &mut log,
        "bot-reviewer",
        verdict(
            "r1",
            "bot-reviewer",
            choir_view::Verdict::Approve,
            "test added",
        ),
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
    assert!(View::materialize(&log)
        .unwrap()
        .reviews
        .get("r1")
        .unwrap()
        .approved());
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
    append_op(
        &mut log,
        "agent-1",
        record("agent-1", "task-spec", "add auth"),
    )
    .unwrap();
    append_op(
        &mut log,
        "agent-1",
        record("agent-1", "plan", "1. schema 2. api"),
    )
    .unwrap();
    append_op(
        &mut log,
        "agent-2",
        record("repo/shared", "task-spec", "living spec v1"),
    )
    .unwrap();
    append_op(
        &mut log,
        "agent-1",
        record("agent-1", "task-spec", "add auth + rate limit"),
    )
    .unwrap();

    let view = View::materialize(&log).unwrap();
    assert_eq!(
        view.provenance["agent-1"]["task-spec"],
        "add auth + rate limit"
    );
    assert_eq!(view.provenance["agent-1"]["plan"], "1. schema 2. api");
    assert_eq!(
        view.provenance["repo/shared"]["task-spec"],
        "living spec v1"
    );

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
        matches!(
            &op.kind,
            OpKind::RequestReview {
                target_ref: None,
                ..
            }
        ),
        "absent field must read as unbound, not as some default ref"
    );

    // And a bound review round-trips, carrying the ref into the view.
    let bound = ViewOp::new(OpKind::RequestReview {
        id: "r2".into(),
        target,
        reviewers: vec!["ana".into()],
        target_ref: Some("choir/choir.git:refs/heads/main".into()),
    });
    let wire = bound.to_payload();
    assert_eq!(
        ViewOp::from_payload(&wire).unwrap(),
        bound,
        "bound review round-trips"
    );

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
    let archive = ViewOp::new(OpKind::ArchiveReview {
        id: "r1".into(),
        lapsed: false,
    });

    // Incomplete: archiving would strand it, since no further verdict
    // could ever decide the outcome.
    append_op(
        &mut log,
        "ana",
        verdict("ana", choir_view::Verdict::Approve),
    )
    .unwrap();
    assert!(matches!(
        append_op(&mut log, "node", archive.clone()),
        Err(ViewError::Review(_))
    ));

    append_op(
        &mut log,
        "bot",
        verdict("bot", choir_view::Verdict::Approve),
    )
    .unwrap();
    let view = View::materialize(&log).unwrap();
    assert!(view.reviews["r1"].approved());

    append_op(&mut log, "node", archive.clone()).unwrap();
    let view = View::materialize(&log).unwrap();
    let r = &view.reviews["r1"];

    // Outcome survives; bulk does not.
    assert!(r.approved(), "archived approval must not evaporate");
    assert!(r.complete(), "archived reviews are settled, not unfinished");
    assert!(r.verdicts.is_empty(), "verdicts should have been dropped");
    assert!(
        r.reviewers.is_empty(),
        "reviewer list should have been dropped"
    );
    // The gate's other two fields are untouched.
    assert_eq!(r.target.as_ref(), Some(&target));
    assert_eq!(r.target_ref.as_deref(), Some("demo.git:refs/heads/main"));
    assert!(matches!(
        r.status,
        choir_view::ReviewStatus::Archived {
            approved: true,
            approval_weight: 2,
        }
    ));

    // Frozen: no further verdicts, and the refusal says archived rather
    // than absent, or a reviewer goes hunting for a typo.
    let err = append_op(
        &mut log,
        "ana",
        verdict("ana", choir_view::Verdict::RequestChanges),
    );
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
fn approval_weight_caps_sibling_channels_and_survives_archiving() {
    let mut log = MemLog::new();
    append_op(
        &mut log,
        "author/agent",
        ViewOp::new(OpKind::RequestReview {
            id: "weighted".into(),
            target: ContentHash::blake3(b"weighted change"),
            reviewers: vec![
                "reviewer/one".into(),
                "reviewer/two".into(),
                "peer/one".into(),
            ],
            target_ref: Some("demo.git:refs/heads/main".into()),
        }),
    )
    .unwrap();
    for reviewer in ["reviewer/one", "reviewer/two", "peer/one"] {
        append_op(
            &mut log,
            reviewer,
            ViewOp::new(OpKind::PostVerdict {
                id: "weighted".into(),
                reviewer: reviewer.into(),
                verdict: choir_view::Verdict::Approve,
                note: String::new(),
            }),
        )
        .unwrap();
    }

    let view = View::materialize(&log).unwrap();
    assert!(view.reviews["weighted"].approved());
    assert_eq!(
        view.reviews["weighted"].approval_weight(),
        2,
        "two sibling channels must contribute only one operator's weight"
    );

    append_op(
        &mut log,
        "node/archive",
        ViewOp::new(OpKind::ArchiveReview {
            id: "weighted".into(),
            lapsed: false,
        }),
    )
    .unwrap();
    let view = View::materialize(&log).unwrap();
    assert_eq!(
        view.reviews["weighted"].approval_weight(),
        2,
        "archiving must retain capped weight after dropping reviewer detail"
    );
}

#[test]
fn slashing_a_live_approval_removes_only_that_operators_weight() {
    let mut log = MemLog::new();
    append_op(
        &mut log,
        "author/agent",
        ViewOp::new(OpKind::RequestReview {
            id: "slashed-live".into(),
            target: ContentHash::blake3(b"live slash"),
            reviewers: vec![
                "reviewer/one".into(),
                "reviewer/two".into(),
                "peer/one".into(),
            ],
            target_ref: Some("demo.git:refs/heads/main".into()),
        }),
    )
    .unwrap();
    for reviewer in ["reviewer/one", "reviewer/two", "peer/one"] {
        append_op(
            &mut log,
            reviewer,
            ViewOp::new(OpKind::PostVerdict {
                id: "slashed-live".into(),
                reviewer: reviewer.into(),
                verdict: choir_view::Verdict::Approve,
                note: String::new(),
            }),
        )
        .unwrap();
    }

    append_op(
        &mut log,
        "node/slash",
        ViewOp::new(OpKind::SlashApproval {
            id: "slashed-live".into(),
            reviewer: "reviewer/one".into(),
            reason: "reviewer key compromised".into(),
        }),
    )
    .unwrap();
    let view = View::materialize(&log).unwrap();
    let review = &view.reviews["slashed-live"];
    assert!(review.re_review_required());
    assert_eq!(
        review.approval_weight(),
        1,
        "a slash removes the affected operator's capped seat"
    );
    assert!(review.approved());
    assert_eq!(review.slashes["reviewer/one"], "reviewer key compromised");

    // A sibling channel from the same operator was already capped into
    // the same seat. Slashing it too must not subtract that seat twice.
    append_op(
        &mut log,
        "node/slash",
        ViewOp::new(OpKind::SlashApproval {
            id: "slashed-live".into(),
            reviewer: "reviewer/two".into(),
            reason: "same operator".into(),
        }),
    )
    .unwrap();
    let view = View::materialize(&log).unwrap();
    assert_eq!(view.reviews["slashed-live"].approval_weight(), 1);

    // Slashing is final for this review. A later verdict cannot silently
    // restore the invalidated approval, and the same slash is not replayed.
    assert!(matches!(
        append_op(
            &mut log,
            "reviewer/one",
            ViewOp::new(OpKind::PostVerdict {
                id: "slashed-live".into(),
                reviewer: "reviewer/one".into(),
                verdict: choir_view::Verdict::Approve,
                note: "try again".into(),
            }),
        ),
        Err(ViewError::Review(_))
    ));
    assert!(matches!(
        append_op(
            &mut log,
            "node/slash",
            ViewOp::new(OpKind::SlashApproval {
                id: "slashed-live".into(),
                reviewer: "reviewer/one".into(),
                reason: "duplicate".into(),
            }),
        ),
        Err(ViewError::Review(_))
    ));

    // Archiving after a live slash must not subtract it twice. The
    // archived scalar captures the original capped weight; replay then
    // applies the append-only slash records exactly once.
    append_op(
        &mut log,
        "node/archive",
        ViewOp::new(OpKind::ArchiveReview {
            id: "slashed-live".into(),
            lapsed: false,
        }),
    )
    .unwrap();
    let view = View::materialize(&log).unwrap();
    assert_eq!(view.reviews["slashed-live"].approval_weight(), 1);
}

#[test]
fn slashing_an_archived_approval_keeps_the_row_compact_and_invalidates_it() {
    let mut log = MemLog::new();
    append_op(
        &mut log,
        "author/agent",
        ViewOp::new(OpKind::RequestReview {
            id: "slashed-archive".into(),
            target: ContentHash::blake3(b"archived slash"),
            reviewers: vec!["reviewer/one".into(), "peer/one".into()],
            target_ref: Some("demo.git:refs/heads/main".into()),
        }),
    )
    .unwrap();
    for reviewer in ["reviewer/one", "peer/one"] {
        append_op(
            &mut log,
            reviewer,
            ViewOp::new(OpKind::PostVerdict {
                id: "slashed-archive".into(),
                reviewer: reviewer.into(),
                verdict: choir_view::Verdict::Approve,
                note: "bulk".into(),
            }),
        )
        .unwrap();
    }
    append_op(
        &mut log,
        "node/archive",
        ViewOp::new(OpKind::ArchiveReview {
            id: "slashed-archive".into(),
            lapsed: false,
        }),
    )
    .unwrap();

    for reviewer in ["reviewer/one", "peer/one"] {
        append_op(
            &mut log,
            "node/slash",
            ViewOp::new(OpKind::SlashApproval {
                id: "slashed-archive".into(),
                reviewer: reviewer.into(),
                reason: "retroactive policy finding".into(),
            }),
        )
        .unwrap();
    }
    let view = View::materialize(&log).unwrap();
    let review = &view.reviews["slashed-archive"];
    assert!(review.reviewers.is_empty() && review.verdicts.is_empty());
    assert!(review.re_review_required());
    assert_eq!(review.approval_weight(), 0);
    assert!(!review.approved());
    assert_eq!(review.slashes.len(), 2);
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
    append_op(
        &mut log,
        "node",
        ViewOp::new(OpKind::ArchiveReview {
            id: "r2".into(),
            lapsed: false,
        }),
    )
    .unwrap();

    let view = View::materialize(&log).unwrap();
    assert!(
        !view.reviews["r2"].approved(),
        "an archived rejection must not read as approved"
    );
}

#[test]
fn lapsing_settles_an_abandoned_review_without_inventing_an_outcome() {
    // An unanswered review is exactly the kind that accumulates, and it
    // is incomplete by definition -- so a pruner that can only archive
    // complete reviews reclaims the ones least likely to pile up. Lapsing
    // closes that, and the design point is that it needs no new outcome:
    // a review nobody answered never got approval.
    let mut log = MemLog::new();
    append_op(
        &mut log,
        "author",
        ViewOp::new(OpKind::RequestReview {
            id: "abandoned".into(),
            target: ContentHash::blake3(b"nobody looked"),
            reviewers: vec!["ana".into(), "bot".into()],
            target_ref: Some("demo.git:refs/heads/main".into()),
        }),
    )
    .unwrap();

    let archive = |lapsed: bool| {
        ViewOp::new(OpKind::ArchiveReview {
            id: "abandoned".into(),
            lapsed,
        })
    };

    // Plain archiving still refuses, and now says how to settle it.
    match append_op(&mut log, "node", archive(false)) {
        Err(ViewError::Review(msg)) => assert!(msg.contains("lapsed"), "{msg}"),
        other => panic!("expected a refusal naming the repair, got {other:?}"),
    }

    append_op(&mut log, "node", archive(true)).unwrap();
    let view = View::materialize(&log).unwrap();
    let r = &view.reviews["abandoned"];

    assert!(
        !r.approved(),
        "an abandoned review must never read as approved"
    );
    assert!(r.complete(), "it is settled, not still waiting");
    assert!(
        r.verdicts.is_empty() && r.reviewers.is_empty(),
        "bulk should be gone"
    );
    assert_eq!(r.target_ref.as_deref(), Some("demo.git:refs/heads/main"));
    assert!(matches!(
        r.status,
        choir_view::ReviewStatus::Archived {
            approved: false,
            approval_weight: 0,
        }
    ));

    // And it is frozen like any other archived review.
    let verdict = ViewOp::new(OpKind::PostVerdict {
        id: "abandoned".into(),
        reviewer: "ana".into(),
        verdict: choir_view::Verdict::Approve,
        note: String::new(),
    });
    assert!(matches!(
        append_op(&mut log, "ana", verdict),
        Err(ViewError::Review(_))
    ));
}

#[test]
fn a_complete_review_cannot_be_lapsed_out_of_its_outcome() {
    // Lapsing an answered review would discard a real verdict, which is
    // the direction that turns retention into censorship.
    let mut log = MemLog::new();
    append_op(
        &mut log,
        "author",
        ViewOp::new(OpKind::RequestReview {
            id: "answered".into(),
            target: ContentHash::blake3(b"reviewed"),
            reviewers: vec!["ana".into()],
            target_ref: None,
        }),
    )
    .unwrap();
    append_op(
        &mut log,
        "ana",
        ViewOp::new(OpKind::PostVerdict {
            id: "answered".into(),
            reviewer: "ana".into(),
            verdict: choir_view::Verdict::Approve,
            note: "lgtm".into(),
        }),
    )
    .unwrap();

    match append_op(
        &mut log,
        "node",
        ViewOp::new(OpKind::ArchiveReview {
            id: "answered".into(),
            lapsed: true,
        }),
    ) {
        Err(ViewError::Review(msg)) => assert!(msg.contains("cannot be lapsed"), "{msg}"),
        other => panic!("expected a refusal, got {other:?}"),
    }

    // Archived normally, the approval survives.
    append_op(
        &mut log,
        "node",
        ViewOp::new(OpKind::ArchiveReview {
            id: "answered".into(),
            lapsed: false,
        }),
    )
    .unwrap();
    assert!(View::materialize(&log).unwrap().reviews["answered"].approved());
}

#[test]
fn lapsed_is_additive_so_old_payloads_still_hash_the_same() {
    // Same invariant-1 discipline as `target_ref`: a payload written
    // before the field existed must decode AND re-serialize to identical
    // bytes, or every entry hash in every existing log moves.
    let old = r#"{"format_version":1,"kind":{"ArchiveReview":{"id":"r1"}}}"#;
    let op = ViewOp::from_payload(old.as_bytes()).expect("old payload decodes");
    assert_eq!(
        String::from_utf8(op.to_payload()).unwrap(),
        old,
        "re-serializing an old payload changed its bytes"
    );
    assert!(
        matches!(&op.kind, OpKind::ArchiveReview { lapsed: false, .. }),
        "absent must mean the previous strict behaviour"
    );
    // And a lapse round-trips, carrying the flag.
    let lapsing = ViewOp::new(OpKind::ArchiveReview {
        id: "r2".into(),
        lapsed: true,
    });
    assert_eq!(
        ViewOp::from_payload(&lapsing.to_payload()).unwrap(),
        lapsing
    );
}
