//! Durable operator identity: the properties the fold has to guarantee
//! before anything upstream is allowed to treat a binding as evidence.
//!
//! The point of moving operator identity into the log is that replay
//! reproduces it. Three things have to be true for that to be worth
//! anything, and each has a test here:
//!
//! 1. **The first-binding sequence is immovable.** It is the only age
//!    primitive in the system, so any path that lets it move — re-binding,
//!    channel correction, revoke-then-rebind — turns standing into
//!    something an operator resets at will.
//! 2. **A key belongs to one operator forever.** Otherwise accumulated
//!    standing is transferable, and the age clock measures the key rather
//!    than the operator holding it.
//! 3. **Revocation withdraws authority without erasing attribution.** A
//!    revocation cascade replays over these rows; one that forgot who a
//!    revoked key belonged to could not cascade anywhere.
//!
//! What none of this establishes is *authority*: with no admission rule
//! wired, any key may author any binding. These tests are deliberately
//! written against `View` alone, so nothing here should be read as
//! evidence that a binding is authorized — only that it is sequenced,
//! immutable in the right places, and replayable.

use choir_hash::ContentHash;
use choir_oplog::{MemLog, OpLog};
use choir_view::{append_op, OpKind, View, ViewError, ViewOp};

fn h(tag: &[u8]) -> ContentHash {
    ContentHash::blake3(tag)
}

fn bind(operator: &str, key: &ContentHash, channel: Option<&str>) -> ViewOp {
    ViewOp::new(OpKind::BindKey {
        operator: operator.into(),
        key: key.clone(),
        channel: channel.map(Into::into),
    })
}

fn revoke(key: &ContentHash, reason: &str) -> ViewOp {
    ViewOp::new(OpKind::RevokeKey {
        key: key.clone(),
        reason: reason.into(),
    })
}

/// Filler that advances the log without touching bindings, so "ops
/// elapsed" is measurably different from "bindings elapsed".
fn noise(n: usize) -> Vec<ViewOp> {
    (0..n)
        .map(|i| {
            ViewOp::new(OpKind::RecordProvenance {
                subject: "ws".into(),
                kind: format!("note-{i}"),
                body: String::new(),
            })
        })
        .collect()
}

/// Property 1. The sequence a key was first bound at is the clock, so it
/// must survive every later op that touches the same key.
#[test]
fn first_binding_sequence_never_moves() {
    let mut log = MemLog::new();
    let key = h(b"agent key");

    for op in noise(3) {
        append_op(&mut log, "ana", op).expect("filler applies");
    }
    // The binding therefore lands at seq 3, not seq 0.
    append_op(&mut log, "ana", bind("ana", &key, None)).expect("fresh key binds");

    let view = View::materialize(&log).expect("replays");
    let bound_at = view.bindings[&key.to_hex()].bound_at;
    assert_eq!(bound_at, 3, "bound_at is the log seq of the binding op");
    assert_eq!(view.next_seq, 4);
    assert_eq!(view.ops_since_binding(&key), Some(1));

    // A correction to the channel is the one re-binding the fold allows,
    // and it is exactly the move that must not reset standing.
    for op in noise(5) {
        append_op(&mut log, "ana", op).expect("filler applies");
    }
    append_op(&mut log, "ana", bind("ana", &key, Some("ana/agent")))
        .expect("same operator may correct the channel");

    let view = View::materialize(&log).expect("replays");
    assert_eq!(
        view.bindings[&key.to_hex()].bound_at,
        bound_at,
        "re-binding reset the age clock"
    );
    assert_eq!(
        view.bindings[&key.to_hex()].channel.as_deref(),
        Some("ana/agent"),
        "re-binding must still apply the channel it carries"
    );
    // 3 filler + the binding + 5 filler + the re-binding = 10 ops, and
    // the binding still sits at seq 3.
    assert_eq!(view.next_seq, 10);
    assert_eq!(view.ops_since_binding(&key), Some(7));
}

/// The clock counts sequenced ops, not bindings — the distinction the
/// name `ops_since_binding` is carrying. A binding-ordinal would report 1
/// here; the log position reports 6.
#[test]
fn the_clock_counts_log_position_not_bindings() {
    let mut log = MemLog::new();
    let first = h(b"first key");
    let second = h(b"second key");

    append_op(&mut log, "ana", bind("ana", &first, None)).expect("binds");
    for op in noise(4) {
        append_op(&mut log, "ana", op).expect("filler applies");
    }
    append_op(&mut log, "bo", bind("bo", &second, None)).expect("binds");

    let view = View::materialize(&log).expect("replays");
    assert_eq!(view.ops_since_binding(&first), Some(6));
    assert_eq!(view.ops_since_binding(&second), Some(1));
    assert_eq!(view.ops_since_binding(&h(b"never bound")), None);
}

/// Property 2. Standing is not transferable: a key answers to the
/// operator that first bound it, and to no other.
#[test]
fn a_key_belongs_to_one_operator_forever() {
    let mut view = View::default();
    let key = h(b"agent key");
    view.apply(&bind("ana", &key, None))
        .expect("fresh key binds");

    let stolen = view.apply(&bind("mal", &key, None));
    assert!(
        matches!(stolen, Err(ViewError::Identity(_))),
        "re-binding to another operator must be refused, got {stolen:?}"
    );
    assert_eq!(view.operator_of(&key), Some("ana"));
    assert_eq!(
        view.next_seq, 1,
        "a refused op must not advance the fold position"
    );
}

/// Property 3. Revocation is terminal and append-only: authority stops,
/// the attribution row stays, and the key cannot be brought back.
#[test]
fn revocation_is_terminal_and_keeps_attribution() {
    let mut view = View::default();
    let key = h(b"agent key");
    view.apply(&bind("ana", &key, Some("ana/agent")))
        .expect("binds");
    view.apply(&revoke(&key, "key material leaked"))
        .expect("a live binding revokes");

    let bound = &view.bindings[&key.to_hex()];
    assert!(bound.is_revoked());
    assert_eq!(bound.revoked.as_ref().expect("revoked").at, 1);
    assert_eq!(
        bound.revoked.as_ref().expect("revoked").reason,
        "key material leaked"
    );
    // The row survives, so everything this key authored stays attributable.
    assert_eq!(
        view.operator_of(&key),
        Some("ana"),
        "revocation must not un-attribute past work"
    );
    assert_eq!(bound.bound_at, 0, "revocation must not disturb the clock");

    let rebound = view.apply(&bind("ana", &key, None));
    assert!(
        matches!(rebound, Err(ViewError::Identity(_))),
        "a revoked key must not be rebindable, got {rebound:?}"
    );
    let twice = view.apply(&revoke(&key, "again"));
    assert!(
        matches!(twice, Err(ViewError::Identity(_))),
        "double revocation must be refused, got {twice:?}"
    );
}

/// The two available answers to "which operator is this" must not be able
/// to disagree: a bound channel has to read as its own operator under
/// `reviewer_operator`, which is what every pre-existing caller uses.
#[test]
fn a_bound_channel_must_read_as_its_own_operator() {
    let mut view = View::default();

    for (name, channel) in [
        ("bare operator name", "ana"),
        ("operator-prefixed agent", "ana/agent"),
        ("deeper agent path", "ana/team/agent"),
    ] {
        let mut probe = view.clone();
        assert!(
            probe
                .apply(&bind("ana", &h(name.as_bytes()), Some(channel)))
                .is_ok(),
            "{name}: {channel} reads as ana and must be accepted"
        );
    }

    for (name, operator, channel) in [
        ("foreign prefix", "ana", "mal/agent"),
        ("bare foreign name", "ana", "mal"),
        ("prefix is a strict extension", "ana", "anastasia/agent"),
        ("operator carries a slash", "ana/agent", "ana/agent"),
    ] {
        let refused = view.apply(&bind(operator, &h(name.as_bytes()), Some(channel)));
        assert!(
            matches!(refused, Err(ViewError::Identity(_))),
            "{name}: {operator} + {channel} must be refused, got {refused:?}"
        );
    }
    assert!(view.bindings.is_empty(), "refused binds left state behind");
}

/// The concentration input T3 wants: keys per operator, counted off the
/// log rather than off a rewritable file, and not shrinkable by revoking.
#[test]
fn operator_keys_counts_every_key_including_revoked_ones() {
    let mut view = View::default();
    for (i, operator) in ["ana", "ana", "ana", "bo"].iter().enumerate() {
        view.apply(&bind(operator, &h(format!("k{i}").as_bytes()), None))
            .expect("distinct keys bind");
    }
    view.apply(&revoke(&h(b"k0"), "rotated")).expect("revokes");

    assert_eq!(
        view.operator_keys("ana").count(),
        3,
        "revoking must not lower the count"
    );
    assert_eq!(view.operator_keys("bo").count(), 1);
    assert_eq!(view.operator_keys("nobody").count(), 0);
    assert_eq!(
        view.operator_keys("ana")
            .filter(|(_, bound)| !bound.is_revoked())
            .count(),
        2,
        "callers wanting live-only must still be able to get it"
    );
}

/// The fold is pure, so bindings replay identically and a prefix replay
/// shows the state before the binding existed — the same undo model the
/// rest of the view has.
#[test]
fn bindings_replay_deterministically_and_undo_by_prefix() {
    let mut log = MemLog::new();
    let key = h(b"agent key");
    append_op(&mut log, "ana", noise(1).remove(0)).expect("filler applies");
    append_op(&mut log, "ana", bind("ana", &key, None)).expect("binds");
    append_op(&mut log, "ana", revoke(&key, "rotated")).expect("revokes");

    assert_eq!(
        View::materialize(&log).expect("replays"),
        View::materialize(&log).expect("replays"),
        "the same log must fold to the same bindings"
    );

    let before_revoke = View::at(&log, 2).expect("replays");
    assert!(!before_revoke.bindings[&key.to_hex()].is_revoked());
    let before_bind = View::at(&log, 1).expect("replays");
    assert!(before_bind.bindings.is_empty());
    assert_eq!(before_bind.operator_of(&key), None);
}

/// `next_seq` is a fold counter, not a value read off the log, so the two
/// ways a view gets built must not be able to disagree about it. A node
/// replays at startup and then applies each admitted entry; if that path
/// drifted from a full replay, every `bound_at` written afterwards would
/// be silently wrong.
#[test]
fn the_fold_position_agrees_across_both_construction_paths() {
    let mut log = MemLog::new();
    let key = h(b"agent key");
    let ops = [
        bind("ana", &key, None),
        noise(1).remove(0),
        bind("ana", &key, Some("ana/agent")),
        revoke(&key, "rotated"),
    ];

    let mut incremental = View::default();
    for (i, op) in ops.into_iter().enumerate() {
        assert_eq!(
            incremental.next_seq, i as u64,
            "the fold position must be the seq this op is about to take"
        );
        incremental.apply(&op).expect("applies");
        append_op(&mut log, "ana", op).expect("applies");
        assert_eq!(
            incremental.next_seq,
            log.len(),
            "incremental application drifted from the log length"
        );
    }

    let replayed = View::materialize(&log).expect("replays");
    assert_eq!(
        replayed, incremental,
        "replaying the log must reproduce the incrementally built view"
    );
    assert_eq!(replayed.bindings[&key.to_hex()].bound_at, 0);
    assert_eq!(
        replayed.bindings[&key.to_hex()]
            .revoked
            .as_ref()
            .expect("revoked")
            .at,
        3
    );
}

/// Invariant 1 for the new variant: a payload written before `channel`
/// existed decodes as `None` and re-serializes to the identical bytes, so
/// its entry hash does not move.
#[test]
fn a_binding_without_a_channel_round_trips_byte_identically() {
    let stored = r#"{"format_version":1,"kind":{"BindKey":{"operator":"ana","key":{"codec":30,"digest":[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31]}}}}"#;
    let decoded = ViewOp::from_payload(stored.as_bytes()).expect("old payload decodes");
    assert!(
        matches!(&decoded.kind, OpKind::BindKey { channel: None, operator, .. } if operator == "ana"),
        "the absent field must decode as None, got {decoded:?}"
    );
    assert_eq!(
        String::from_utf8(decoded.to_payload()).expect("utf-8"),
        stored,
        "re-serializing an old payload moved its bytes, and so its hash"
    );
}
