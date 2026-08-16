//! Admission for the durable operator record: only the node may bind or
//! revoke an operator key.
//!
//! `check` is a series of per-variant guards falling through to
//! `View::validate`, i.e. admit-by-default. A binding is the record that
//! T3 attribution and T1's ordering primitive read, so without a guard
//! naming these variants any trusted key could mint attribution evidence
//! about itself — a tripwire whose evidence is forgeable by its subjects,
//! which reads as sequenced proof and is therefore worse than no record.
//!
//! The op and the rule about who may author it ship together for that
//! reason: there is no commit in this history where the gap is live.

use choir_identity::{ActorKey, Registry};
use choir_node::platform::hex_decode;
use choir_node::{Node, Platform};
use choir_oplog::{ContentHash, MemLog};
use choir_view::{OpKind, View, ViewOp};

use crate::support::curl;

use crate::support::submit_body_legacy as submit_body;

fn bind(operator: &str, key: &ContentHash, channel: Option<&str>) -> ViewOp {
    ViewOp::new(OpKind::BindKey {
        operator: operator.into(),
        key: key.clone(),
        channel: channel.map(Into::into),
    })
}

/// `bound_at` is a fold counter, and the view tests prove the counter
/// tracks the log. Neither fact is the claim the field's name makes to a
/// *client*: that the number equals the sequence the node reported when
/// it accepted the binding. Those are two different layers, and only the
/// second is the contract anyone outside this process can use.
///
/// This is a contract test, not a novel-defect detector, and the mutation
/// receipt says so. Skewing the sequencer's assigned seq leaves the whole
/// view layer green (the fold stays self-consistent) and does break other
/// node tests — but on a CAS rejection, which names nothing about
/// `bound_at`. This is the assertion that names the invariant that broke,
/// so the next person reads a sentence instead of a 400.
///
/// It asserts the round trip a reader actually performs: submit through
/// the API, keep the seq the node reports, then rebuild the view from the
/// node's own `/api/log` bytes and require the recomputed `bound_at` to
/// be that same number.
#[test]
fn a_replayed_binding_carries_the_seq_the_node_reported() {
    let node_key = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&node_key.public_key_bytes()).unwrap();
    let platform = Platform::start(
        registry,
        Box::new(MemLog::new()),
        ActorKey::from_secret_bytes(&node_key.secret_bytes()),
    )
    .unwrap();

    let submit = |op: &ViewOp| -> u64 {
        let (code, body) = platform.handle_api(
            "POST",
            "/api/submit",
            submit_body(&node_key, "node", op).as_bytes(),
        );
        let resp: serde_json::Value = serde_json::from_str(&body).expect("json body");
        assert_eq!(code, 200, "{resp}");
        resp["seq"].as_u64().expect("accepted ops report a seq")
    };

    // Interleave bindings with unrelated ops so a `bound_at` that counted
    // bindings rather than log positions would diverge here. Binding the
    // *last* key well past zero is the point: an off-by-any-amount bug
    // survives a log whose first op is the binding under test.
    let keys: Vec<ContentHash> = (0..3)
        .map(|i| ContentHash::blake3(format!("agent key {i}").as_bytes()))
        .collect();
    let mut reported = Vec::new();
    for (i, key) in keys.iter().enumerate() {
        submit(&ViewOp::new(OpKind::RecordProvenance {
            subject: "ws".into(),
            kind: format!("note-{i}"),
            body: String::new(),
        }));
        reported.push(submit(&bind(&format!("op{i}"), key, None)));
    }
    let revoked_at = submit(&ViewOp::new(OpKind::RevokeKey {
        key: keys[0].clone(),
        reason: "key material rotated".into(),
    }));
    assert_eq!(
        reported,
        vec![1, 3, 5],
        "the node's own seqs moved; the rest of this test reads them, so pin them"
    );

    // Rebuild from the transport bytes, not from the node's in-process
    // view: a client only ever has these.
    let (code, body) = platform.handle_api("GET", "/api/log?from=0", b"");
    assert_eq!(code, 200, "{body}");
    let page: serde_json::Value = serde_json::from_str(&body).expect("json body");
    let entries = page["entries"].as_array().expect("entries array");
    let mut replayed = View::default();
    for (offset, entry) in entries.iter().enumerate() {
        assert_eq!(
            entry["seq"].as_u64(),
            Some(offset as u64),
            "the page must be gapless and zero-based for the replay below to mean anything"
        );
        let payload = hex_decode(entry["payload_hex"].as_str().expect("payload_hex")).expect("hex");
        replayed
            .apply(&ViewOp::from_payload(&payload).expect("decodes"))
            .expect("the node admitted it, so a replay must too");
    }

    for (key, seq) in keys.iter().zip(&reported) {
        let bound = &replayed.bindings[&key.to_hex()];
        assert_eq!(
            bound.bound_at, *seq,
            "replayed bound_at must equal the seq the node reported for that binding"
        );
    }
    assert_eq!(
        replayed.bindings[&keys[0].to_hex()]
            .revoked
            .as_ref()
            .expect("revoked")
            .at,
        revoked_at,
        "a revocation's recorded position must survive the same round trip"
    );
    assert_eq!(replayed.next_seq, entries.len() as u64);
}

#[test]
fn only_the_node_may_bind_or_revoke_operator_keys() {
    let work = std::env::temp_dir().join(format!("choir-node-binding-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();

    // Both keys are trusted. The difference under test is *which* of them
    // the node treats as its own, not whether the author is known.
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

    let subject = ContentHash::blake3(b"some agent key");

    // A trusted key that is not the node's may not mint a binding, and in
    // particular may not bind a key to an operator name of its choosing.
    let (code, resp) = curl(&[
        "-X",
        "POST",
        "-d",
        &submit_body(&author, "carol", &bind("carol", &subject, None)),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 400, "{resp}");
    assert_eq!(resp["code"], "node_only", "{resp}");
    assert!(
        resp["error"].as_str().expect("error text").contains("bind"),
        "{resp}"
    );

    // Revocation is guarded by the same rule: otherwise a defecting key
    // could revoke the binding that attributes its own past work.
    let revoke = ViewOp::new(OpKind::RevokeKey {
        key: subject.clone(),
        reason: "not yours to withdraw".into(),
    });
    let (code, resp) = curl(&[
        "-X",
        "POST",
        "-d",
        &submit_body(&author, "carol", &revoke),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 400, "{resp}");
    assert_eq!(resp["code"], "node_only", "{resp}");

    // The node's own key is the one path that works.
    let (code, resp) = curl(&[
        "-X",
        "POST",
        "-d",
        &submit_body(
            &node_key,
            "node",
            &bind("carol", &subject, Some("carol/agent")),
        ),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 200, "{resp}");

    // Channel correction stays open to the node, and is the reason strict
    // assign-once was not adopted: a typo must not burn a key forever.
    let (code, resp) = curl(&[
        "-X",
        "POST",
        "-d",
        &submit_body(
            &node_key,
            "node",
            &bind("carol", &subject, Some("carol/other")),
        ),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 200, "{resp}");

    // Moving the key to a different operator is refused by the fold even
    // for the node, and it surfaces as the identity code rather than as
    // `unclassified` -- the mapping only exists because this path is now
    // reachable at all.
    let (code, resp) = curl(&[
        "-X",
        "POST",
        "-d",
        &submit_body(&node_key, "node", &bind("mallory", &subject, None)),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 400, "{resp}");
    assert_eq!(resp["code"], "identity_state", "{resp}");

    // And revocation is terminal: the node may revoke once, never twice.
    let node_revoke = ViewOp::new(OpKind::RevokeKey {
        key: subject.clone(),
        reason: "key material rotated".into(),
    });
    let (code, resp) = curl(&[
        "-X",
        "POST",
        "-d",
        &submit_body(&node_key, "node", &node_revoke),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 200, "{resp}");

    // Re-sending the *identical* signed bytes is a lost-response retry,
    // not a second revocation, and the node answers 200 with
    // `already_applied` from its retry index. Worth pinning: it means a
    // replayed revocation cannot double-count, and it is why the genuine
    // double-revoke below has to carry different bytes to be a new op.
    let (code, resp) = curl(&[
        "-X",
        "POST",
        "-d",
        &submit_body(&node_key, "node", &node_revoke),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 200, "{resp}");
    assert_eq!(resp["already_applied"], true, "{resp}");

    let second_revoke = ViewOp::new(OpKind::RevokeKey {
        key: subject,
        reason: "a genuinely different second attempt".into(),
    });
    let (code, resp) = curl(&[
        "-X",
        "POST",
        "-d",
        &submit_body(&node_key, "node", &second_revoke),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 400, "{resp}");
    assert_eq!(resp["code"], "identity_state", "{resp}");
}
