//! L8 acceptance: sign/verify roundtrip, tamper and impersonation
//! rejection, key persistence, and pre-L8 (unsigned) entry decode.

use choir_identity::{ActorKey, IdentityError, Registry};
use choir_oplog::{OpEntry, FORMAT_VERSION};

fn entry(payload: &[u8]) -> OpEntry {
    OpEntry {
        format_version: FORMAT_VERSION,
        parent: None,
        seq: 0,
        channel: "w".into(),
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
    let mut rews = e.clone();
    rews.channel = "someone-else".into();
    assert_eq!(registry.verify_entry(&rews), Err(IdentityError::BadSignature));

    // seq/parent are deliberately NOT covered: the sequencer assigns
    // them after signing (witnesses cover placement from Phase 2).
    let mut reseq = e.clone();
    reseq.seq = 7;
    assert!(registry.verify_entry(&reseq).is_ok());
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

/// ES256 verification, against a real assertion rather than a fixture.
///
/// The key and the signature come from `openssl` at test time, the way
/// `choir-bridge`'s JWT test mints a throwaway RSA key, because a checked-in
/// vector would prove only that the bytes were copied correctly once. The
/// message has the exact shape a WebAuthn assertion signs:
/// `authenticatorData ‖ SHA-256(clientDataJSON)`.
#[test]
fn an_es256_assertion_verifies_and_a_tampered_one_does_not() {
    let work = std::env::temp_dir().join(format!("choir-es256-test-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let secret = work.join("signer.key");
    let spki = work.join("signer.der");

    let ok = std::process::Command::new("openssl")
        .args(["ecparam", "-name", "prime256v1", "-genkey", "-noout", "-out"])
        .arg(&secret)
        .output()
        .expect("openssl runs");
    assert!(ok.status.success(), "generate P-256 key");
    let ok = std::process::Command::new("openssl")
        .args(["ec", "-in"])
        .arg(&secret)
        .args(["-pubout", "-outform", "DER", "-out"])
        .arg(&spki)
        .output()
        .expect("openssl runs");
    assert!(ok.status.success(), "extract SPKI");

    // clientDataJSON, then the message the authenticator would sign.
    let client_data = br#"{"type":"webauthn.get","challenge":"Y2hhbGxlbmdl","origin":"https://x"}"#;
    let cd = work.join("clientData.json");
    std::fs::write(&cd, client_data).unwrap();
    let hashed = std::process::Command::new("openssl")
        .args(["dgst", "-sha256", "-binary"])
        .arg(&cd)
        .output()
        .expect("openssl runs");
    assert!(hashed.status.success(), "hash clientDataJSON");
    let mut message = vec![0x49u8; 37]; // stand-in authenticatorData
    message.extend_from_slice(&hashed.stdout);

    let msg_path = work.join("message.bin");
    let sig_path = work.join("signature.der");
    std::fs::write(&msg_path, &message).unwrap();
    let ok = std::process::Command::new("openssl")
        .args(["dgst", "-sha256", "-sign"])
        .arg(&secret)
        .arg("-out")
        .arg(&sig_path)
        .arg(&msg_path)
        .output()
        .expect("openssl runs");
    assert!(ok.status.success(), "sign the assertion");

    let spki_der = std::fs::read(&spki).unwrap();
    let signature = std::fs::read(&sig_path).unwrap();
    assert_eq!(
        choir_identity::verify_es256(&spki_der, &message, &signature),
        Ok(()),
        "a real assertion verifies"
    );

    // One flipped byte anywhere in the signed message must break it. This
    // is the assertion that would still pass if `verify_es256` returned
    // `Ok` unconditionally, so it is the one worth having.
    let mut tampered = message.clone();
    tampered[0] ^= 0x01;
    assert_eq!(
        choir_identity::verify_es256(&spki_der, &tampered, &signature),
        Err(IdentityError::BadSignature),
        "a tampered message is refused"
    );

    // The COSE path: an authenticator sends coordinates, not a key file.
    // Rebuilding from the raw point must produce a key that verifies the
    // same signature, or enrolment cannot use what the browser sends.
    let point = &spki_der[choir_identity::P256_SPKI_PREFIX.len()..];
    assert_eq!(point.len(), 65, "uncompressed point is 65 bytes");
    let rebuilt = choir_identity::p256_point_to_spki(point).expect("a valid point");
    assert_eq!(rebuilt, spki_der, "the prefix is exactly what openssl emits");
    assert_eq!(
        choir_identity::verify_es256(&rebuilt, &message, &signature),
        Ok(()),
        "a key rebuilt from raw coordinates verifies"
    );

    // A compressed point is refused rather than expanded: an authenticator
    // sending one is doing something this path has never seen.
    assert_eq!(
        choir_identity::p256_point_to_spki(&[0x02; 33]),
        Err(IdentityError::BadKey)
    );

    std::fs::remove_dir_all(&work).ok();
}
