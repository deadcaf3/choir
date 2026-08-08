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
