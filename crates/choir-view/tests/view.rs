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
