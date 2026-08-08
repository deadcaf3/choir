//! L8 acceptance: sign/verify roundtrip, tamper and impersonation
//! rejection, key persistence, and pre-L8 (unsigned) entry decode.

use choir_identity::{ActorKey, IdentityError, Registry};
use choir_oplog::{OpEntry, FORMAT_VERSION};

fn entry(payload: &[u8]) -> OpEntry {
    OpEntry {
        format_version: FORMAT_VERSION,
        parent: None,
        seq: 0,
        workspace: "w".into(),
        payload: payload.to_vec(),
        witnesses: Vec::new(),
        author_sig: None,
    }
}

#[test]
fn sign_verify_roundtrip_and_tamper_detection() {
    let key = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&key.public_key_bytes()).unwrap();

    let mut e = entry(b"set head");
    key.sign_entry(&mut e);
    assert_eq!(registry.verify_entry(&e).unwrap(), key.actor_id());

    // Any post-signature mutation must invalidate it.
    let mut tampered = e.clone();
    tampered.payload = b"set head to somewhere else".to_vec();
    assert_eq!(
        registry.verify_entry(&tampered),
        Err(IdentityError::BadSignature)
    );
    let mut reseq = e.clone();
    reseq.seq = 7;
    assert_eq!(registry.verify_entry(&reseq), Err(IdentityError::BadSignature));
}

#[test]
fn impersonation_and_unknown_keys_rejected() {
    let alice = ActorKey::generate();
    let mallory = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&alice.public_key_bytes()).unwrap();

    // Mallory signs but claims Alice's key id.
    let mut e = entry(b"op");
    mallory.sign_entry(&mut e);
    e.author_sig.as_mut().unwrap().key_id = alice.actor_id().to_hex();
    assert_eq!(registry.verify_entry(&e), Err(IdentityError::BadSignature));

    // Mallory's own honest signature is merely unknown, not valid.
    let mut e2 = entry(b"op");
    mallory.sign_entry(&mut e2);
    assert!(matches!(
        registry.verify_entry(&e2),
        Err(IdentityError::UnknownKey(_))
    ));
}

#[test]
fn key_roundtrips_through_secret_bytes() {
    let key = ActorKey::generate();
    let restored = ActorKey::from_secret_bytes(&key.secret_bytes());
    assert_eq!(restored.actor_id(), key.actor_id());

    let mut e = entry(b"op");
    restored.sign_entry(&mut e);
    let mut registry = Registry::new();
    registry.register(&key.public_key_bytes()).unwrap();
    assert!(registry.verify_entry(&e).is_ok());
}

#[test]
fn pre_l8_entries_still_decode_and_report_unsigned() {
    // JSON written before author_sig existed (no such field).
    let old = r#"{"format_version":1,"parent":null,"seq":0,"workspace":"w","payload":[1,2],"witnesses":[]}"#;
    let e: OpEntry = serde_json::from_str(old).unwrap();
    assert!(e.author_sig.is_none());
    assert_eq!(
        Registry::new().verify_entry(&e),
        Err(IdentityError::Unsigned)
    );
    // And an unsigned entry serializes without the field, so the hash
    // chain of pre-L8 logs is unchanged by this upgrade.
    let json = serde_json::to_string(&e).unwrap();
    assert!(!json.contains("author_sig"));
}
