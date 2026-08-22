//! The witness half of D16, folded (D67).
//!
//! A witness cosigns D25's ref-state attestation and nothing else. The
//! reason it is a separate op rather than an entry's `witnesses` field
//! is structural: `OpEntry::content_hash` covers that field, so a
//! cosignature added after the entry was hashed would rewrite the entry
//! and orphan every descendant. In-entry witnessing is therefore
//! synchronous by construction — signatures collected before the
//! append, on the sequencer's critical path — which is exactly what
//! D16's tripwire exists to avoid. This is the async branch that row
//! already names as its alternative.
//!
//! Three properties make a witness count worth reading, and each has a
//! test here:
//!
//! 1. **A witness must be a bound operator.** The same Sybil floor a
//!    vouch has (D65), in the fold for the same reason: a replayer must
//!    reach the same verdict as the node that admitted the op.
//! 2. **Only the current attestation is witnessable.** Refs can return
//!    to a prior value, so a cosignature over an older snapshot would
//!    read as a statement about now. That is the ABA shape D25 names,
//!    and it is refused rather than recorded.
//! 3. **One row per witness**, so the section is bounded by the witness
//!    population rather than by how many snapshots the node has taken —
//!    the D64 growth property, stated for this section.
//!
//! What none of it establishes is *authorship*: the fold is handed an
//! op, never its signer, so `witness` here is a claim. Binding it to the
//! signing channel, and refusing the node's own signature, is the
//! daemon's job — `choir-node/tests/it/witness.rs` tests that.

use choir_hash::ContentHash;
use choir_view::{OpKind, View, ViewError, ViewOp};

fn h(tag: &[u8]) -> ContentHash {
    ContentHash::blake3(tag)
}

/// A view where `names` are each an operator with one live key.
fn bound(names: &[&str]) -> View {
    let mut view = View::default();
    for name in names {
        view.apply(&ViewOp::new(OpKind::BindKey {
            operator: (*name).into(),
            key: h(name.as_bytes()),
            channel: Some((*name).into()),
        }))
        .expect("a fresh key binds");
    }
    view
}

/// Takes the view's own snapshot and records it, returning its id.
fn attest(view: &mut View) -> ContentHash {
    let snapshot = view.snapshot();
    let id = snapshot.id();
    view.apply(&ViewOp::new(OpKind::RecordRefSnapshot { snapshot }))
        .expect("the view's own snapshot is admissible");
    id
}

fn countersign(witness: &str, snapshot: &ContentHash) -> ViewOp {
    ViewOp::new(OpKind::CountersignSnapshot {
        witness: witness.into(),
        snapshot: snapshot.clone(),
    })
}

fn set_ref(name: &str, commit: &ContentHash) -> ViewOp {
    ViewOp::new(OpKind::SetRef {
        name: name.into(),
        commit: commit.clone(),
        prev: None,
    })
}

/// Property 1, both halves: an unregistered witness cannot witness, and
/// neither can one whose last key was revoked.
#[test]
fn a_witness_needs_a_live_key_binding() {
    let mut view = bound(&["observer"]);
    let id = attest(&mut view);

    let stranger = view.validate(&countersign("stranger", &id));
    assert!(
        matches!(stranger, Err(ViewError::Witness(ref m)) if m.contains("no live key binding")),
        "an unregistered witness was admitted: {stranger:?}"
    );

    view.apply(&countersign("observer", &id))
        .expect("a bound operator witnesses");

    // Revoke the only key and the standing row remains — it is a record
    // of something that was true — but nothing further may be signed.
    view.apply(&ViewOp::new(OpKind::RevokeKey {
        key: h(b"observer"),
        reason: "rotated".into(),
    }))
    .expect("a bound key revokes");
    let after = attest(&mut view);
    let revoked = view.validate(&countersign("observer", &after));
    assert!(
        matches!(revoked, Err(ViewError::Witness(_))),
        "a revoked operator kept witnessing: {revoked:?}"
    );
    assert!(
        view.witnessed.contains_key("observer"),
        "revocation erased what the operator had already attested"
    );
}

/// Property 2, and the reason it is not merely tidiness: the ref map is
/// driven back to a value it already held, so the *old* snapshot is once
/// again a true statement about the refs — and still refused, because it
/// is not a true statement about now.
#[test]
fn only_the_latest_attestation_is_witnessable_even_when_the_refs_return() {
    let mut view = bound(&["observer"]);
    let a = h(b"commit a");
    let b = h(b"commit b");

    view.apply(&set_ref("demo.git:refs/heads/main", &a))
        .expect("first ref");
    let first = attest(&mut view);

    view.apply(&ViewOp::new(OpKind::SetRef {
        name: "demo.git:refs/heads/main".into(),
        commit: b.clone(),
        prev: Some(a.clone()),
    }))
    .expect("second ref");
    let second = attest(&mut view);

    // Back to exactly the state `first` attested.
    view.apply(&ViewOp::new(OpKind::SetRef {
        name: "demo.git:refs/heads/main".into(),
        commit: a,
        prev: Some(b),
    }))
    .expect("third ref");
    let third = attest(&mut view);

    for (stale, why) in [(&first, "the first"), (&second, "the second")] {
        let refused = view.validate(&countersign("observer", stale));
        assert!(
            matches!(refused, Err(ViewError::Witness(ref m)) if m.contains("latest")),
            "{why} attestation was witnessable after it stopped being current: {refused:?}"
        );
    }
    assert_ne!(
        first, third,
        "the chain pointer must make two snapshots of the same refs distinct, \
         or this test proves nothing"
    );
    view.apply(&countersign("observer", &third))
        .expect("the current attestation is witnessable");
}

/// Saying it twice is refused, and a new attestation is a new statement
/// the same witness may make — the row moves rather than accumulating.
#[test]
fn a_witness_says_it_once_per_attestation() {
    let mut view = bound(&["observer"]);
    let first = attest(&mut view);
    view.apply(&countersign("observer", &first))
        .expect("first witness");
    let at_first = view.witnessed["observer"].at;

    let again = view.validate(&countersign("observer", &first));
    assert!(
        matches!(again, Err(ViewError::Witness(ref m)) if m.contains("already")),
        "a repeated cosignature was admitted: {again:?}"
    );

    let second = attest(&mut view);
    view.apply(&countersign("observer", &second))
        .expect("a new attestation is a new statement");
    assert_eq!(
        view.witnessed.len(),
        1,
        "the row accumulated rather than moved"
    );
    assert_eq!(view.witnessed["observer"].snapshot, second);
    assert!(
        view.witnessed["observer"].at > at_first,
        "the position did not move with the statement"
    );
}

/// Property 3: witnessing churn does not grow the view. Three witnesses
/// over fifty attestations leave three rows, and the serialized section
/// is byte-identical to the one three witnesses leave after one.
#[test]
fn the_section_is_bounded_by_the_witness_population_not_by_uptime() {
    let names = ["one", "two", "three"];
    let mut view = bound(&names);
    let first = attest(&mut view);
    for name in names {
        view.apply(&countersign(name, &first)).expect("first round");
    }
    let after_one_round = serde_json::to_string(&view.witnessed).expect("serializes");

    for round in 0..50u32 {
        view.apply(&set_ref(
            &format!("demo.git:refs/heads/r{round}"),
            &h(&round.to_le_bytes()),
        ))
        .expect("a ref moves so the next snapshot differs");
        let id = attest(&mut view);
        for name in names {
            view.apply(&countersign(name, &id)).expect("later round");
        }
    }

    assert_eq!(view.witnessed.len(), 3, "one row per witness, and no more");
    let after_fifty = serde_json::to_string(&view.witnessed).expect("serializes");
    assert_ne!(
        after_one_round, after_fifty,
        "the rows must carry which attestation was witnessed, or they say nothing"
    );

    // The shape is what is bounded, not the contents: same keys, same
    // field names, same order, whatever the churn.
    let keys = |text: &str| -> Vec<String> {
        let value: serde_json::Value = serde_json::from_str(text).expect("json");
        value
            .as_object()
            .expect("object")
            .iter()
            .map(|(k, v)| format!("{k}:{}", v.as_object().expect("row").len()))
            .collect()
    };
    assert_eq!(keys(&after_one_round), keys(&after_fifty));
}

/// A log with no attestation yet has nothing to witness, and says so in
/// its own words rather than by failing a hash comparison.
#[test]
fn there_is_nothing_to_witness_before_the_first_attestation() {
    let view = bound(&["observer"]);
    let refused = view.validate(&countersign("observer", &h(b"invented")));
    assert!(
        matches!(refused, Err(ViewError::Witness(ref m)) if m.contains("no ref-state")),
        "witnessing an empty log was not refused clearly: {refused:?}"
    );
}
