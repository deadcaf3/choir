//! Admission for the D25 ref-state attestation: only the node may record
//! one.
//!
//! The fold already refuses a snapshot it cannot reproduce, so this guard
//! is purely about authorship. `check` is admit-by-default (per-variant
//! guards falling through to `View::validate`), which means a variant
//! that does not name itself in the policy is writable by any trusted
//! key — and a snapshot signed by an arbitrary key attests nothing about
//! the node while reading as though it did. The op and the rule about
//! who may author it ship together, as `key_binding.rs` says of its own
//! variants: there is no commit where the gap is live.

use choir_identity::{ActorKey, Registry};
use choir_node::{Node, Platform};
use choir_oplog::MemLog;
use choir_view::{OpKind, RefSnapshot, View, ViewOp};

use crate::support::curl;
use crate::support::submit_body_legacy as submit_body;

#[test]
fn only_the_node_may_record_a_ref_snapshot() {
    let work = std::env::temp_dir().join(format!("choir-node-snapshot-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();

    // Both keys are trusted; the difference under test is which one the
    // node treats as its own.
    let author = ActorKey::generate();
    let node_key = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&author.public_key_bytes()).unwrap();
    registry.register(&node_key.public_key_bytes()).unwrap();
    let node_key_for_platform = ActorKey::from_secret_bytes(&node_key.secret_bytes());

    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), node_key_for_platform).unwrap(),
    );
    let port = node.port();
    let node = std::sync::Arc::new(node);
    std::thread::spawn(move || node.serve_forever());
    let api = format!("http://127.0.0.1:{port}/api");

    // The log is empty, so the admissible snapshot is exactly what a
    // fresh view emits: no refs, position 0, no predecessor.
    let admissible = View::default().snapshot();

    // A trusted non-node key is refused on authorship, not on truth: the
    // snapshot it offers is the correct one.
    let (code, resp) = curl(&[
        "-X",
        "POST",
        "-d",
        &submit_body(
            &author,
            "carol",
            &ViewOp::new(OpKind::RecordRefSnapshot {
                snapshot: admissible.clone(),
            }),
        ),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 400, "{resp}");
    assert_eq!(resp["code"], "node_only", "{resp}");
    assert!(
        resp["error"]
            .as_str()
            .expect("error text")
            .contains("snapshot"),
        "{resp}"
    );

    // The node's own key is the one path that works, and the refusal
    // above consumed no sequence: the snapshot taken at position 0 still
    // admits.
    let (code, resp) = curl(&[
        "-X",
        "POST",
        "-d",
        &submit_body(
            &node_key,
            "node",
            &ViewOp::new(OpKind::RecordRefSnapshot {
                snapshot: admissible,
            }),
        ),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 200, "{resp}");
    assert_eq!(resp["seq"], 0, "{resp}");

    // Even the node cannot re-record the same snapshot: the chain moved.
    let stale = RefSnapshot {
        at_seq: 1,
        ..View::default().snapshot()
    };
    let (code, resp) = curl(&[
        "-X",
        "POST",
        "-d",
        &submit_body(
            &node_key,
            "node",
            &ViewOp::new(OpKind::RecordRefSnapshot { snapshot: stale }),
        ),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 400, "{resp}");
    assert!(
        resp["error"]
            .as_str()
            .expect("error text")
            .contains("chain"),
        "{resp}"
    );

    std::fs::remove_dir_all(work).ok();
}
