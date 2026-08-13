//! D25 fold semantics: a `RecordRefSnapshot` is admitted only when the
//! fold itself can reproduce the claim, and the chain rule makes an old
//! snapshot inadmissible even when the ref map it attests recurs — the
//! ABA shape D26 measured for refs, answered the same way for
//! attestations.

use choir_hash::ContentHash;
use choir_view::{OpKind, RefSnapshot, View, ViewError, ViewOp};

fn h(tag: &[u8]) -> ContentHash {
    ContentHash::blake3(tag)
}

fn set_ref(name: &str, commit: &ContentHash, prev: Option<&ContentHash>) -> ViewOp {
    ViewOp::new(OpKind::SetRef {
        name: name.into(),
        commit: commit.clone(),
        prev: prev.cloned(),
    })
}

fn record(snapshot: RefSnapshot) -> ViewOp {
    ViewOp::new(OpKind::RecordRefSnapshot { snapshot })
}

/// The through-line: `View::snapshot` is always admissible immediately,
/// each admission advances the chain, and the view serves the latest.
#[test]
fn snapshots_chain_and_the_view_serves_the_latest() {
    let mut view = View::default();

    // A log's first snapshot: empty ref map, position 0, no predecessor.
    let genesis = view.snapshot();
    assert_eq!(genesis.at_seq, 0);
    assert_eq!(genesis.prev_snapshot, None);
    assert!(genesis.refs.is_empty());
    view.apply(&record(genesis.clone())).unwrap();
    assert_eq!(view.latest_snapshot.as_ref(), Some(&genesis));

    let main = h(b"main head");
    view.apply(&set_ref("repo.git:refs/heads/main", &main, None))
        .unwrap();
    let second = view.snapshot();
    assert_eq!(second.at_seq, 2, "genesis snapshot + set-ref were applied");
    assert_eq!(
        second.prev_snapshot,
        Some(genesis.id()),
        "the chain pointer names the latest admitted snapshot"
    );
    assert_eq!(second.refs.get("repo.git:refs/heads/main"), Some(&main));
    view.apply(&record(second.clone())).unwrap();
    assert_eq!(view.latest_snapshot.as_ref(), Some(&second));
}

/// Truth: a snapshot claiming a ref-state the fold cannot reproduce is
/// refused by every replayer, whichever field carries the lie.
#[test]
fn a_snapshot_the_fold_cannot_reproduce_is_refused() {
    let mut view = View::default();
    view.apply(&set_ref("repo.git:refs/heads/main", &h(b"head"), None))
        .unwrap();

    // Wrong map: one extra ref the view does not hold.
    let mut lying = view.snapshot();
    lying
        .refs
        .insert("repo.git:refs/heads/ghost".into(), h(b"ghost"));
    let err = view.validate(&record(lying)).unwrap_err();
    assert!(
        matches!(&err, ViewError::Snapshot(s) if s.contains("does not match")),
        "{err:?}"
    );

    // Wrong position: the state was read, then something else landed.
    // The interleaved op moves no ref, so this isolates the position
    // check from the map check.
    let stale = view.snapshot();
    view.apply(&ViewOp::new(OpKind::RecordProvenance {
        subject: "spec".into(),
        kind: "plan".into(),
        body: "landed between read and attest".into(),
    }))
    .unwrap();
    let err = view.validate(&record(stale)).unwrap_err();
    assert!(
        matches!(&err, ViewError::Snapshot(s) if s.contains("position")),
        "{err:?}"
    );

    // A refusal leaves the chain untouched.
    assert_eq!(view.latest_snapshot, None);
}

/// The ABA case this op exists to close: refs return to an attested
/// value, and the old attestation still does not re-admit, because its
/// chain pointer no longer names the latest snapshot.
#[test]
fn an_old_snapshot_is_refused_even_when_the_ref_map_recurs() {
    let mut view = View::default();
    let x = h(b"commit x");
    let y = h(b"commit y");
    let name = "repo.git:refs/heads/main";

    view.apply(&set_ref(name, &x, None)).unwrap();
    let first = view.snapshot();
    view.apply(&record(first.clone())).unwrap();

    // The ref leaves x and comes back to it.
    view.apply(&set_ref(name, &y, Some(&x))).unwrap();
    view.apply(&set_ref(name, &x, Some(&y))).unwrap();

    // A fresh snapshot of the recurred state is admissible...
    let fresh = view.snapshot();
    assert_eq!(fresh.refs, first.refs, "the ref map genuinely recurred");
    view.apply(&record(fresh.clone())).unwrap();

    // ...but one carrying the recurred map with the *old* chain pointer
    // is not, even at the right position.
    let replayed = RefSnapshot {
        prev_snapshot: first.prev_snapshot.clone(),
        at_seq: view.next_seq,
        ..first
    };
    let err = view.validate(&record(replayed)).unwrap_err();
    assert!(
        matches!(&err, ViewError::Snapshot(s) if s.contains("chain")),
        "{err:?}"
    );
    assert_eq!(view.latest_snapshot.as_ref(), Some(&fresh));
}
