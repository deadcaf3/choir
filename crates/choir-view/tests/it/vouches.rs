//! The vouch graph: the properties the fold must guarantee before
//! anything upstream may treat an edge as evidence (D65).
//!
//! A vouch is the only thing in the log that is one identity's opinion
//! of another, which makes it the only thing an attacker gains by
//! manufacturing identities. Four properties are what stop that, and
//! each has a test here:
//!
//! 1. **Both ends must be bound operators.** This is the Sybil floor,
//!    and it lives in the fold rather than in the daemon so a replayer
//!    reaches the same verdict as the node that admitted the op.
//! 2. **Nothing vouches for itself.** Self-assertion is the one
//!    statement a manufactured identity can always make.
//! 3. **One edge per ordered pair**, so the pair is a retry identity and
//!    a resubmission cannot silently redate an edge that already stands.
//! 4. **Withdrawal leaves no residue in the view.** Vouch churn must not
//!    grow the view, which is the D64 property stated for this section.
//!
//! What none of it establishes is *authorship*: the fold is handed an
//! op, never its signer, so `voucher` here is a claim. Binding it to the
//! signing channel is the daemon's job, and
//! `choir-node/tests/it/vouches.rs` is where that is tested.

use choir_hash::ContentHash;
use choir_view::{OpKind, View, ViewError, ViewOp};

fn h(tag: &[u8]) -> ContentHash {
    ContentHash::blake3(tag)
}

/// A view where `names` are each an operator with one live key, which is
/// the precondition every vouch below needs.
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

fn vouch(voucher: &str, subject: &str, note: &str) -> ViewOp {
    ViewOp::new(OpKind::Vouch {
        voucher: voucher.into(),
        subject: subject.into(),
        note: note.into(),
    })
}

fn withdraw(voucher: &str, subject: &str) -> ViewOp {
    ViewOp::new(OpKind::WithdrawVouch {
        voucher: voucher.into(),
        subject: subject.into(),
        reason: "changed my mind".into(),
    })
}

#[test]
fn an_edge_needs_a_bound_operator_at_both_ends() {
    let mut view = bound(&["ana"]);

    // Neither direction is admissible while `bob` is nobody: an
    // unbound name on either end is refused, and the message says
    // which end, because the repair differs (bind bob's key vs. bind
    // your own).
    let from_nobody = view.apply(&vouch("ghost", "ana", ""));
    assert!(
        matches!(&from_nobody, Err(ViewError::Vouch(m)) if m.contains("voucher ghost")),
        "{from_nobody:?}"
    );
    let for_nobody = view.apply(&vouch("ana", "ghost", ""));
    assert!(
        matches!(&for_nobody, Err(ViewError::Vouch(m)) if m.contains("subject ghost")),
        "{for_nobody:?}"
    );

    // And bind `bob`, and the same op lands. Without this the test
    // would pass on a fold that refused every vouch ever offered.
    view.apply(&ViewOp::new(OpKind::BindKey {
        operator: "bob".into(),
        key: h(b"bob"),
        channel: None,
    }))
    .expect("a fresh key binds");
    view.apply(&vouch("bob", "ana", "worked with them"))
        .expect("both ends are bound now");
    assert!(view.vouch_stands("bob", "ana"));
}

#[test]
fn a_revoked_operator_can_no_longer_vouch_but_can_still_withdraw() {
    let mut view = bound(&["ana", "bob"]);
    view.apply(&vouch("bob", "ana", "")).expect("admissible");
    view.apply(&ViewOp::new(OpKind::RevokeKey {
        key: h(b"bob"),
        reason: "laptop stolen".into(),
    }))
    .expect("a bound key revokes");

    // The floor is "an unrevoked binding", so a revoked operator is
    // back to being nobody for the purpose of making new statements.
    let refused = view.apply(&vouch("bob", "ana", "second thoughts"));
    assert!(
        matches!(&refused, Err(ViewError::Vouch(m)) if m.contains("unrevoked")),
        "{refused:?}"
    );

    // Withdrawal is deliberately not held to the same floor. Whoever
    // holds the key after a compromise needs the edge gone, and that is
    // exactly the moment the operator has no live key left.
    view.apply(&withdraw("bob", "ana"))
        .expect("an edge can always be taken back");
    assert!(!view.vouch_stands("bob", "ana"));
}

#[test]
fn nothing_vouches_for_itself_under_any_spelling() {
    let mut view = bound(&["ana"]);
    let itself = view.apply(&vouch("ana", "ana", ""));
    assert!(
        matches!(&itself, Err(ViewError::Vouch(m)) if m.contains("itself")),
        "{itself:?}"
    );

    // The channel spelling is refused before it can become a second
    // identity: `ana/agent` reads as operator `ana` everywhere else, so
    // admitting it here would give one operator two rows and let it
    // reach itself in two hops.
    let via_channel = view.apply(&vouch("ana/agent", "ana", ""));
    assert!(
        matches!(&via_channel, Err(ViewError::Vouch(m)) if m.contains("operator identity")),
        "{via_channel:?}"
    );
}

#[test]
fn the_pair_is_the_retry_identity_and_a_resubmission_never_redates_it() {
    let mut view = bound(&["ana", "bob"]);
    view.apply(&vouch("bob", "ana", "first"))
        .expect("admissible");
    let placed_at = view.vouches["ana"]["bob"].at;

    // Ten ops later the same vouch is offered again -- the shape a
    // client retrying a lost response produces. Accepting it would move
    // the edge's date forward by ten, silently reporting a fresh
    // endorsement where there is one old one.
    for i in 0..10 {
        view.apply(&ViewOp::new(OpKind::RecordProvenance {
            subject: "ws".into(),
            kind: format!("note-{i}"),
            body: String::new(),
        }))
        .expect("filler");
    }
    let again = view.apply(&vouch("bob", "ana", "first"));
    assert!(
        matches!(&again, Err(ViewError::Vouch(m)) if m.contains("already vouches")),
        "{again:?}"
    );
    assert_eq!(view.vouches["ana"]["bob"].at, placed_at);

    // Withdrawing and vouching again *does* move it, because that is a
    // second statement rather than a retry of the first.
    view.apply(&withdraw("bob", "ana")).expect("stands");
    view.apply(&vouch("bob", "ana", "second")).expect("gone");
    assert!(view.vouches["ana"]["bob"].at > placed_at);
    assert_eq!(view.vouches["ana"]["bob"].note, "second");
}

#[test]
fn withdrawing_a_vouch_leaves_nothing_behind_in_the_view() {
    let mut view = bound(&["ana", "bob"]);
    let empty = serde_json::to_vec(&view.vouches).expect("serializable");

    // 50 rounds of vouch-and-withdraw. The log grows by 100 ops; the
    // section this writes has to end byte-identical to where it started,
    // or vouch churn is a way to grow a view without adding data (D64).
    for _ in 0..50 {
        view.apply(&vouch("bob", "ana", "a note of some length"))
            .expect("admissible");
        view.apply(&withdraw("bob", "ana")).expect("stands");
    }
    assert_eq!(view.next_seq, 102, "two bindings and a hundred vouch ops");
    assert_eq!(
        serde_json::to_vec(&view.vouches).expect("serializable"),
        empty,
        "vouch churn left residue in the view"
    );
    assert!(view.vouches.is_empty(), "an emptied subject row survived");

    // And a withdrawal of an edge that is not there is refused rather
    // than treated as a no-op, so a client cannot learn the graph by
    // watching which withdrawals succeed.
    let absent = view.apply(&withdraw("bob", "ana"));
    assert!(
        matches!(&absent, Err(ViewError::Vouch(m)) if m.contains("does not vouch")),
        "{absent:?}"
    );
}

#[test]
fn a_withdrawal_states_a_reason_and_the_graph_replays_identically() {
    let mut view = bound(&["ana", "bob"]);
    view.apply(&vouch("bob", "ana", "")).expect("admissible");
    let no_reason = view.apply(&ViewOp::new(OpKind::WithdrawVouch {
        voucher: "bob".into(),
        subject: "ana".into(),
        reason: String::new(),
    }));
    assert!(
        matches!(&no_reason, Err(ViewError::Vouch(m)) if m.contains("non-empty reason")),
        "{no_reason:?}"
    );

    // Replay is the whole claim a sequenced record makes, so it is
    // asserted rather than assumed: the same ops in the same order
    // produce the same graph, including the positions inside it.
    let ops = [
        ViewOp::new(OpKind::BindKey {
            operator: "cy".into(),
            key: h(b"cy"),
            channel: None,
        }),
        vouch("cy", "ana", "mutual"),
        vouch("ana", "cy", "mutual"),
    ];
    let mut replayed = view.clone();
    for op in &ops {
        view.apply(op).expect("admissible");
        replayed.apply(op).expect("admissible");
    }
    assert_eq!(view.vouches, replayed.vouches);
    assert!(view.vouch_stands("cy", "ana") && view.vouch_stands("ana", "cy"));
}
