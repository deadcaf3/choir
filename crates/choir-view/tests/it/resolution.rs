//! Resolution-as-linked-change (Pijul item 3, metadata-only per D15):
//! `Commit::resolves` links a resolution to the conflicted commit it
//! resolves, admission refuses an invalid link, and the field is
//! additive — a pre-change commit decodes, re-serializes byte-identically
//! and keeps its hash.

use std::collections::BTreeMap;

use choir_hash::ContentHash;
use choir_oplog::{MemLog, OpLog};
use choir_store::{put_blob, ChunkerParams, MemStore};
use choir_view::{append_op_with_store, Commit, OpKind, TreeEntry, View, ViewError, ViewOp};

fn set_head(ws: &str, commit: &ContentHash, prev: Option<&ContentHash>) -> ViewOp {
    ViewOp::new(OpKind::SetWorkspaceHead {
        workspace: ws.into(),
        commit: commit.clone(),
        prev: prev.cloned(),
    })
}

/// Stores a conflicted commit on `path` and returns its id.
fn conflicted_commit(store: &mut MemStore, path: &str) -> ContentHash {
    let base = put_blob(store, b"base\n", ChunkerParams::default()).unwrap();
    let left = put_blob(store, b"left\n", ChunkerParams::default()).unwrap();
    let right = put_blob(store, b"right\n", ChunkerParams::default()).unwrap();
    let mut tree = BTreeMap::new();
    tree.insert(
        path.to_string(),
        TreeEntry::Conflict {
            base: Some(base),
            left,
            right,
        },
    );
    Commit {
        format_version: choir_view::FORMAT_VERSION,
        parents: vec![],
        tree,
        author: "test".into(),
        message: "conflicted merge".into(),
        resolves: None,
    }
    .put(store)
    .unwrap()
}

/// Stores a resolution of `path` on top of `parent`, linked (or not) via
/// `resolves`.
fn resolution_commit(
    store: &mut MemStore,
    parent: &ContentHash,
    path: &str,
    resolves: Option<ContentHash>,
) -> ContentHash {
    let blob = put_blob(store, b"resolved\n", ChunkerParams::default()).unwrap();
    let mut tree = BTreeMap::new();
    tree.insert(path.to_string(), TreeEntry::File { blob });
    Commit {
        format_version: choir_view::FORMAT_VERSION,
        parents: vec![parent.clone()],
        tree,
        author: "test".into(),
        message: "resolve the conflict".into(),
        resolves,
    }
    .put(store)
    .unwrap()
}

/// The pass/fail case for item A: a committed conflict, a resolution
/// whose `resolves` names it, both admitted; the link is readable back;
/// and a dangling link is refused rather than silently dropped.
#[test]
fn resolution_links_its_conflict_and_a_dangling_link_is_refused() {
    let mut store = MemStore::new();
    let mut log = MemLog::new();

    let conflict = conflicted_commit(&mut store, "hot.txt");
    append_op_with_store(&mut log, &store, "w1", set_head("w1", &conflict, None)).unwrap();

    let fix = resolution_commit(&mut store, &conflict, "hot.txt", Some(conflict.clone()));
    append_op_with_store(&mut log, &store, "w1", set_head("w1", &fix, Some(&conflict))).unwrap();

    // The link holds in both directions: the head names the conflict it
    // resolves, and the conflicted commit is still a value in history
    // (invariant 6) — resolving rewrote nothing.
    let view = View::materialize(&log).unwrap();
    let head = Commit::get(&store, view.workspaces.get("w1").unwrap()).unwrap();
    assert_eq!(head.resolves, Some(conflict.clone()));
    assert!(!head.is_conflicted());
    let still_there = Commit::get(&store, &conflict).unwrap();
    assert!(still_there.is_conflicted());

    // A dangling link — the store has never seen the named commit — is
    // refused at admission and never reaches the log.
    let dangling = ContentHash::blake3(b"no such commit");
    let bad = resolution_commit(&mut store, &fix, "hot.txt", Some(dangling));
    let before = log.len();
    let refused = append_op_with_store(&mut log, &store, "w1", set_head("w1", &bad, Some(&fix)));
    assert!(matches!(refused, Err(ViewError::Resolution(_))));
    assert_eq!(log.len(), before, "a refused op must not reach the log");
}

/// `resolves` may only name a commit that has something to resolve: a
/// link to a conflict-free commit is refused, not admitted vacuously.
#[test]
fn resolves_link_to_an_unconflicted_commit_is_refused() {
    let mut store = MemStore::new();
    let mut log = MemLog::new();

    let plain = {
        let blob = put_blob(&mut store, b"calm\n", ChunkerParams::default()).unwrap();
        let mut tree = BTreeMap::new();
        tree.insert("calm.txt".to_string(), TreeEntry::File { blob });
        Commit {
            format_version: choir_view::FORMAT_VERSION,
            parents: vec![],
            tree,
            author: "test".into(),
            message: "nothing to resolve".into(),
            resolves: None,
        }
        .put(&mut store)
        .unwrap()
    };
    append_op_with_store(&mut log, &store, "w1", set_head("w1", &plain, None)).unwrap();

    let bad = resolution_commit(&mut store, &plain, "calm.txt", Some(plain.clone()));
    let refused = append_op_with_store(&mut log, &store, "w1", set_head("w1", &bad, Some(&plain)));
    assert!(matches!(refused, Err(ViewError::Resolution(_))));
}

/// Invariant 1 for the new field, proven against pre-change bytes built
/// in this test: a commit serialized before `resolves` existed decodes
/// with `resolves: None`, re-serializes byte-identically, and keeps its
/// content hash. A linked commit, by contrast, does carry the field.
#[test]
fn pre_change_commit_bytes_decode_and_hash_identically() {
    // The exact field set Commit had before this change, as raw JSON —
    // not produced by today's serializer, so the round-trip below cannot
    // pass by construction.
    let blob = serde_json::to_string(&ContentHash::blake3(b"old blob")).unwrap();
    let old_bytes = format!(
        r#"{{"format_version":1,"parents":[],"tree":{{"a.txt":{{"File":{{"blob":{blob}}}}}}},"author":"old-agent","message":"written before the link field existed"}}"#
    )
    .into_bytes();
    assert!(
        !String::from_utf8(old_bytes.clone()).unwrap().contains("\"resolves\":"),
        "the fixture must predate the field"
    );

    let decoded: Commit = serde_json::from_slice(&old_bytes).unwrap();
    assert_eq!(decoded.resolves, None);
    let re_bytes = serde_json::to_vec(&decoded).unwrap();
    assert_eq!(re_bytes, old_bytes, "old commits must re-serialize byte-identically");
    assert_eq!(
        ContentHash::blake3(&re_bytes),
        ContentHash::blake3(&old_bytes),
        "old commit hashes must not move"
    );

    // And the field does appear when set, so the link is persisted.
    let mut linked = decoded;
    linked.resolves = Some(ContentHash::blake3(b"the conflict"));
    let linked_bytes = serde_json::to_vec(&linked).unwrap();
    assert!(String::from_utf8(linked_bytes).unwrap().contains("resolves"));
}
