//! Explicit change dependencies (Pijul item 2): `ViewOp.depends` is
//! additive inside the signed payload — an op written before the field
//! existed re-serializes byte-identically, so its author signature
//! still verifies (invariant 4), and a declared list rides inside the
//! bytes the signature covers.

use choir_hash::ContentHash;
use choir_identity::{ActorKey, Registry};
use choir_view::{OpKind, ViewOp};

fn set_ref_kind() -> OpKind {
    OpKind::SetRef {
        name: "main".into(),
        commit: ContentHash::blake3(b"tip"),
        prev: None,
    }
}

/// The signature-scope proof, against pre-change bytes built here: a
/// payload serialized before `depends` existed, signed then, verifies
/// after a round-trip through today's decoder — because the decoder
/// re-emits the exact bytes the author signed.
#[test]
fn an_old_signed_payload_still_verifies_after_round_trip() {
    // The exact field set ViewOp had before `depends`, as raw JSON in
    // declaration order — not produced by today's serializer.
    let commit = serde_json::to_string(&ContentHash::blake3(b"tip")).unwrap();
    let old_payload = format!(
        r#"{{"format_version":1,"kind":{{"SetRef":{{"name":"main","commit":{commit},"prev":null}}}}}}"#
    )
    .into_bytes();
    assert!(
        !String::from_utf8(old_payload.clone()).unwrap().contains("\"depends\":"),
        "the fixture must predate the field"
    );

    // Signed as it would have been at write time (invariant 4: the
    // signature covers `(channel, payload)` and nothing else).
    let key = ActorKey::generate();
    let sig = key.sign_submission("agent-1", &old_payload);
    let mut registry = Registry::new();
    registry.register(&key.public_key_bytes()).unwrap();

    // Round-trip through today's ViewOp: decodes with no dependencies,
    // re-serializes byte-identically, and the old signature verifies
    // against the re-emitted bytes.
    let decoded = ViewOp::from_payload(&old_payload).unwrap();
    assert!(decoded.depends.is_empty());
    let re_emitted = decoded.to_payload();
    assert_eq!(re_emitted, old_payload, "old payloads must not move");
    registry
        .verify_submission("agent-1", &re_emitted, &sig)
        .expect("an old unsigned-field payload must still verify");
}

/// A declared list is inside the signed bytes: stripping it (or adding
/// one) changes the payload the signature covers, so verification fails.
#[test]
fn depends_is_covered_by_the_signature() {
    let key = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&key.public_key_bytes()).unwrap();

    let declared = ViewOp::new(set_ref_kind())
        .with_depends(vec![ContentHash::blake3(b"prerequisite change")]);
    let signed_bytes = declared.to_payload();
    let sig = key.sign_submission("agent-1", &signed_bytes);
    registry
        .verify_submission("agent-1", &signed_bytes, &sig)
        .expect("the declared form verifies as signed");

    // A relay that strips the declaration produces different bytes, and
    // the signature no longer covers them.
    let stripped = ViewOp::new(set_ref_kind()).to_payload();
    assert_ne!(stripped, signed_bytes);
    assert!(
        registry.verify_submission("agent-1", &stripped, &sig).is_err(),
        "stripping depends must break the signature"
    );
}

/// The additive rule in both directions: an empty list emits no field,
/// a declared one does.
#[test]
fn empty_depends_is_not_serialized() {
    let plain = serde_json::to_string(&ViewOp::new(set_ref_kind())).unwrap();
    assert!(!plain.contains("\"depends\":"));
    let declared = serde_json::to_string(
        &ViewOp::new(set_ref_kind()).with_depends(vec![ContentHash::blake3(b"dep")]),
    )
    .unwrap();
    assert!(declared.contains("\"depends\":"));
}
