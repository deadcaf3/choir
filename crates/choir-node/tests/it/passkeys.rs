//! Passkey enrolment (D39), the piece that makes the verifier reachable.
//!
//! `choir-identity` already proves that a WebAuthn assertion verifies and
//! that one bound to a different `signing_hash` is refused. What it
//! cannot prove is that the key a verifier is handed is the key its
//! holder enrolled, because it has no store. That join is what this
//! module tests, and the end-to-end case drives a real `openssl`-minted
//! P-256 credential through enrolment and back out into
//! `verify_webauthn_assertion` rather than asserting on a stored string.
//!
//! Most of these drive the store directly. `passkey_spki` is the whole
//! point of enrolment and has no HTTP spelling, so testing enrolment only
//! through `curl` would assert that a key was accepted and never that it
//! comes back usable.

use std::collections::BTreeSet;

use choir_identity::{ActorKey, Registry};
use choir_node::accounts::Accounts;
use choir_node::{AuthTable, Node, Platform};
use choir_oplog::MemLog;

use crate::support::curl;

fn json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).expect("test JSON parses")
}

/// A store on disk with one redeemed account, `bob`, and the path it
/// lives at so a test can reopen it.
fn store_with_bob(tag: &str) -> (Accounts, std::path::PathBuf, std::path::PathBuf) {
    let work = std::env::temp_dir().join(format!("choir-node-passkeys-{tag}"));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");
    let path = work.join("accounts.json");
    let store = Accounts::open(path.clone(), None, BTreeSet::new()).expect("store opens");

    let (status, body) = store.invite(
        "alice",
        &json(r#"{"user":"bob","grants":["agents/demo read"]}"#),
    );
    assert_eq!(status, 200, "{body}");
    let invite = json(&body)["invite"].as_str().expect("invite pair").to_string();
    let id = invite.split(':').next().expect("invite id").to_string();
    let (status, body) = store.redeem(&id, &json("{}"));
    assert_eq!(status, 200, "{body}");
    (store, path, work)
}

/// Mints a P-256 credential with `openssl` and returns its
/// SubjectPublicKeyInfo DER, base64url, plus the secret key's path — the
/// same shape `getPublicKey()` hands a browser.
fn credential(work: &std::path::Path, name: &str) -> (String, std::path::PathBuf) {
    let secret = work.join(format!("{name}.key"));
    let der = work.join(format!("{name}.der"));
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
        .arg(&der)
        .output()
        .expect("openssl runs");
    assert!(ok.status.success(), "extract SPKI");
    (base64url(&std::fs::read(&der).expect("spki")), secret)
}

/// Base64url without padding, via `openssl` rather than through the
/// node's own encoder, so a test of the node's decoding does not depend
/// on the node's encoding being right.
fn base64url(bytes: &[u8]) -> String {
    // Unique per call. These modules share a process and run on parallel
    // threads, so a name built from the pid and the input length collides
    // between two tests encoding two 91-byte keys — and the loser reads
    // an empty file rather than failing, which is the worst shape a test
    // helper can fail in.
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "choir-passkey-b64-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let raw = dir.join("in.bin");
    std::fs::write(&raw, bytes).expect("write");
    let out = std::process::Command::new("openssl")
        .args(["base64", "-A", "-in"])
        .arg(&raw)
        .output()
        .expect("openssl runs");
    std::fs::remove_dir_all(&dir).ok();
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .chars()
        .filter(|c| *c != '=')
        .map(|c| match c {
            '+' => '-',
            '/' => '_',
            other => other,
        })
        .collect()
}

/// The join D39's two halves need: a key that went in through enrolment
/// comes back out as the bytes a verifier can use, and a genuine
/// assertion by that credential verifies against the operation it
/// approved.
///
/// Asserting on the stored string instead would prove the store echoes
/// what it was told. This asserts it stored something a verifier accepts,
/// which is a different claim and the one enrolment exists to make.
#[test]
fn an_enrolled_credential_comes_back_as_a_key_that_verifies() {
    let (store, _path, work) = store_with_bob("verifies");
    let (public_key, secret) = credential(&work, "cred");

    let (status, body) = store.enroll_passkey(
        "bob",
        &json(&format!(
            r#"{{"credential_id":"cred-one","public_key":"{public_key}","label":"laptop"}}"#
        )),
    );
    assert_eq!(status, 200, "{body}");

    let spki = store
        .passkey_spki("bob", "cred-one")
        .expect("the enrolled key must come back");

    // Sign the assertion the way an authenticator does, over a challenge
    // that is this operation's `signing_hash`.
    let signing = choir_oplog::signing_hash("git/bob", b"the op bob approved");
    let challenge = base64url(&choir_identity::webauthn_challenge(&signing));
    let client_data =
        format!(r#"{{"type":"webauthn.get","challenge":"{challenge}","origin":"https://node"}}"#)
            .into_bytes();
    let cd_path = work.join("cd.json");
    std::fs::write(&cd_path, &client_data).expect("write");
    let hashed = std::process::Command::new("openssl")
        .args(["dgst", "-sha256", "-binary"])
        .arg(&cd_path)
        .output()
        .expect("openssl runs");
    let auth_data = vec![0x49u8; 37];
    let mut message = auth_data.clone();
    message.extend_from_slice(&hashed.stdout);
    let msg_path = work.join("msg.bin");
    let sig_path = work.join("sig.der");
    std::fs::write(&msg_path, &message).expect("write");
    let ok = std::process::Command::new("openssl")
        .args(["dgst", "-sha256", "-sign"])
        .arg(&secret)
        .arg("-out")
        .arg(&sig_path)
        .arg(&msg_path)
        .output()
        .expect("openssl runs");
    assert!(ok.status.success(), "sign");

    let sig = choir_oplog::Witness::webauthn_es256(
        "cred-one",
        std::fs::read(&sig_path).expect("sig"),
        auth_data,
        client_data,
    );
    assert_eq!(
        choir_identity::verify_webauthn_assertion(&spki, &signing, &sig),
        Ok(()),
        "an assertion by the enrolled credential must verify"
    );

    // And the lookup is scoped to the account, not global: the same
    // credential id under another name is not this key. Without the
    // `user` half of the key, one account's assertion would spend as
    // another's.
    assert_eq!(store.passkey_spki("alice", "cred-one"), None);

    std::fs::remove_dir_all(&work).ok();
}

/// A public key is checked for shape at enrolment rather than at first
/// use, because the two fail in places the holder can and cannot see.
/// Every case here is a key a browser could plausibly send if something
/// upstream were wrong.
#[test]
fn a_key_that_is_not_a_p256_spki_is_refused_at_enrolment() {
    let (store, _path, work) = store_with_bob("bad-keys");
    let (good, _secret) = credential(&work, "cred");

    let cases: Vec<(&str, String)> = vec![
        ("not base64url at all", "not base64!".to_string()),
        // Deliberately crafted rather than translated: a base64url
        // string need not contain `-` or `_` at all, so `replace` left
        // some keys byte-identical and the case passed vacuously. It was
        // written that way first and enrolled successfully, which is how
        // this comment came to exist.
        ("standard alphabet, not url", format!("{}+", &good[..good.len() - 1])),
        ("truncated", good[..good.len() - 8].to_string()),
        // Right length, wrong prefix: an RSA or ed25519 SPKI would land
        // here, and a length-only check would accept it.
        ("right length, wrong prefix", {
            let mut bytes = base64url_decode_for_test(&good);
            bytes[3] ^= 0xff;
            base64url(&bytes)
        }),
        // Right prefix, compressed point: the one `p256_point_to_spki`
        // refuses rather than expands.
        ("compressed point", {
            let mut bytes = base64url_decode_for_test(&good);
            let at = choir_identity::P256_SPKI_PREFIX.len();
            bytes[at] = 0x02;
            base64url(&bytes)
        }),
    ];
    for (name, key) in cases {
        let (status, body) = store.enroll_passkey(
            "bob",
            &json(&format!(
                r#"{{"credential_id":"c","public_key":"{key}","label":"x"}}"#
            )),
        );
        assert_eq!(status, 400, "{name} was accepted: {body}");
    }

    // The control. Without it every assertion above could be passing
    // because enrolment refuses everything.
    let (status, body) = store.enroll_passkey(
        "bob",
        &json(&format!(
            r#"{{"credential_id":"c","public_key":"{good}","label":"x"}}"#
        )),
    );
    assert_eq!(status, 200, "a real key must still enrol: {body}");

    // The same credential id twice is a conflict, not a second entry. A
    // duplicate would make removal ambiguous: one `remove` call would
    // leave a credential that still verifies under an id the holder
    // believes they withdrew.
    let (status, body) = store.enroll_passkey(
        "bob",
        &json(&format!(
            r#"{{"credential_id":"c","public_key":"{good}","label":"again"}}"#
        )),
    );
    assert_eq!(status, 409, "a duplicate credential id was accepted: {body}");

    // And the ceiling holds. One account already has one key, so the
    // next `MAX_PASSKEYS - 1` fit and the one after that does not.
    for n in 1..choir_node::accounts::MAX_PASSKEYS {
        let (status, body) = store.enroll_passkey(
            "bob",
            &json(&format!(
                r#"{{"credential_id":"c{n}","public_key":"{good}","label":"x"}}"#
            )),
        );
        assert_eq!(status, 200, "key {n} was refused early: {body}");
    }
    let (status, body) = store.enroll_passkey(
        "bob",
        &json(&format!(
            r#"{{"credential_id":"one-too-many","public_key":"{good}","label":"x"}}"#
        )),
    );
    assert_eq!(status, 409, "the ceiling did not hold: {body}");

    std::fs::remove_dir_all(&work).ok();
}

/// Base64url decode, for building deliberately broken keys. Independent
/// of the node's decoder, which is what these cases are testing.
fn base64url_decode_for_test(input: &str) -> Vec<u8> {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut rev = [255u8; 256];
    for (i, &c) in ALPHABET.iter().enumerate() {
        rev[c as usize] = i as u8;
    }
    let mut out = Vec::new();
    let (mut buf, mut bits) = (0u32, 0u32);
    for c in input.bytes() {
        let v = rev[c as usize];
        assert_ne!(v, 255, "test input must be base64url");
        buf = (buf << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    out
}

/// Revocation's contract is that it forgets, and a passkey is a thing
/// that must be forgotten with the account rather than outliving it.
/// Asserted against the file on disk as well as the live store, because
/// "not returned by a lookup" and "not written down" are different
/// promises and only the second survives a restart.
#[test]
fn revocation_forgets_the_passkeys_with_the_account() {
    let (store, path, work) = store_with_bob("revoke");
    let (public_key, _secret) = credential(&work, "cred");
    let (status, body) = store.enroll_passkey(
        "bob",
        &json(&format!(
            r#"{{"credential_id":"gone-after","public_key":"{public_key}","label":"x"}}"#
        )),
    );
    assert_eq!(status, 200, "{body}");
    assert!(std::fs::read_to_string(&path)
        .expect("store readable")
        .contains("gone-after"));

    let (status, body) = store.revoke(&json(r#"{"user":"bob"}"#));
    assert_eq!(status, 200, "{body}");

    assert_eq!(store.passkey_spki("bob", "gone-after"), None);
    let on_disk = std::fs::read_to_string(&path).expect("store readable");
    assert!(
        !on_disk.contains("gone-after") && !on_disk.contains(&public_key),
        "a revoked account left its credential on disk: {on_disk}"
    );

    std::fs::remove_dir_all(&work).ok();
}

/// Enrolment is persisted, and a store written before D39 still loads.
/// The second half is the additive-field claim (invariant 1) checked
/// against a file this binary did not write, which is the only input the
/// claim is about.
#[test]
fn enrolment_survives_a_reopen_and_a_pre_d39_store_still_loads() {
    let (store, path, work) = store_with_bob("reopen");
    let (public_key, _secret) = credential(&work, "cred");
    assert_eq!(
        store
            .enroll_passkey(
                "bob",
                &json(&format!(
                    r#"{{"credential_id":"kept","public_key":"{public_key}","label":"phone"}}"#
                )),
            )
            .0,
        200
    );
    drop(store);

    let reopened = Accounts::open(path, None, BTreeSet::new()).expect("store reopens");
    assert_eq!(
        reopened.passkey_spki("bob", "kept").map(|k| k.len()),
        Some(choir_identity::P256_SPKI_PREFIX.len() + 65),
        "an enrolled key must survive a reopen"
    );
    drop(reopened);

    // A store as an older binary would have written it: no `passkeys`
    // member anywhere.
    let older = work.join("older.json");
    std::fs::write(
        &older,
        r#"{"format_version":1,"accounts":[{"user":"bob","token_hash":"1e-ab","grants":[],"ssh_keys":[],"created_at":1}],"invites":[],"retired":[]}"#,
    )
    .expect("write older store");
    let old = Accounts::open(older, None, BTreeSet::new()).expect("a pre-D39 store must still load");
    assert_eq!(old.len(), 1, "the account survived the load");
    assert_eq!(
        old.passkey_spki("bob", "anything"),
        None,
        "an account with no passkeys has none, rather than failing to load"
    );

    std::fs::remove_dir_all(&work).ok();
}

/// The route, the ACL arm, and the two principals that must not reach it.
///
/// Enrolment takes the account from the authenticated caller and never
/// from the body, so this drives the one case where that matters: a body
/// naming somebody else. It must enrol on the caller regardless.
#[test]
fn enrolment_acts_on_the_caller_and_refuses_the_principals_that_have_no_account() {
    let work = std::env::temp_dir().join("choir-node-passkeys-http");
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");
    let acl_path = work.join("acl");
    std::fs::write(&acl_path, "alice @node write\nalice * write\n").expect("acl file");

    let mut table = AuthTable::new();
    table.insert("alice".into(), "a".into());
    let root = work.join("repos");
    let mut node = Node::bind_with_auth(&root, 0, Some(table)).expect("node binds");
    let port = node.port();
    node.watch_acl_file(acl_path).expect("acl loads");
    node.enable_accounts(work.join("accounts.json"), None)
        .expect("accounts enable");
    node.enable_platform(
        Platform::start(Registry::new(), Box::new(MemLog::new()), ActorKey::generate())
            .expect("platform starts"),
    );
    std::thread::spawn(move || node.serve_forever());
    let base = format!("http://127.0.0.1:{port}");

    let (public_key, _secret) = credential(&work, "cred");

    // The operator holds a hand-written credential and therefore no
    // account record. Refused, and the refusal says why rather than
    // reporting a missing endpoint.
    let (status, body) = curl(&[
        "-u",
        "alice:a",
        "-X",
        "POST",
        "--data-binary",
        &format!(r#"{{"credential_id":"c","public_key":"{public_key}"}}"#),
        &format!("{base}/api/accounts/passkey"),
    ]);
    assert_eq!(status, 404, "{body}");
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|e| e.contains("auth file")),
        "the refusal must name the reason: {body}"
    );

    // Mint and redeem an account for bob.
    let (status, invite) = curl(&[
        "-u",
        "alice:a",
        "-X",
        "POST",
        "--data-binary",
        r#"{"user":"bob","grants":["agents/demo read"]}"#,
        &format!("{base}/api/accounts/invite", ),
    ]);
    assert_eq!(status, 200, "{invite}");
    let pair = invite["invite"].as_str().expect("invite pair").to_string();

    // An unredeemed invite may only be redeemed — it must not reach
    // enrolment, which requires no grant and would otherwise be open to
    // it.
    let (status, _) = curl(&[
        "-u",
        &pair,
        "-X",
        "POST",
        "--data-binary",
        &format!(r#"{{"credential_id":"c","public_key":"{public_key}"}}"#),
        &format!("{base}/api/accounts/passkey"),
    ]);
    assert_eq!(status, 403, "an invite must not enrol a passkey");

    let (status, redeemed) = curl(&[
        "-u",
        &pair,
        "-X",
        "POST",
        "--data-binary",
        "{}",
        &format!("{base}/api/accounts/redeem"),
    ]);
    assert_eq!(status, 200, "{redeemed}");
    let token = redeemed["token"].as_str().expect("token").to_string();

    // Bob enrols, with a body that names alice. The account enrolled on
    // is bob's, because the caller is bob.
    let (status, body) = curl(&[
        "-u",
        &format!("bob:{token}"),
        "-X",
        "POST",
        "--data-binary",
        &format!(r#"{{"user":"alice","credential_id":"bobs","public_key":"{public_key}"}}"#),
        &format!("{base}/api/accounts/passkey"),
    ]);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["user"], "bob", "the body must not choose the account");

    // The roster shows it under bob and nowhere else.
    let (status, roster) = curl(&["-u", "alice:a", &format!("{base}/api/accounts")]);
    assert_eq!(status, 200, "{roster}");
    let accounts = roster["accounts"].as_array().expect("accounts array");
    let bob = accounts
        .iter()
        .find(|a| a["user"] == "bob")
        .expect("bob is listed");
    assert_eq!(bob["passkeys"][0]["credential_id"], "bobs");
    assert_eq!(bob["passkeys"][0]["label"], "passkey", "the default label");

    // Removal is the mirror, and removing twice is a 404 rather than a
    // silent success — a credential that "removes" when it was never
    // there hides the case where the id was mistyped.
    let remove = format!("{base}/api/accounts/passkey/remove");
    let args = [
        "-u",
        &format!("bob:{token}"),
        "-X",
        "POST",
        "--data-binary",
        r#"{"credential_id":"bobs"}"#,
        &remove,
    ];
    let (status, body) = curl(&args);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["remaining"], 0);
    let (status, _) = curl(&args);
    assert_eq!(status, 404, "removing an absent credential must not report success");

    std::fs::remove_dir_all(&work).ok();
}

/// The whole D39 write path, end to end: a human's enrolled authenticator
/// signs one operation and the node admits it into the log.
///
/// This is the test that makes `verify_webauthn_assertion` reachable.
/// Until it existed the primitive was verified in isolation and the store
/// held keys nothing consulted, which is two green halves and no path.
#[test]
fn an_op_signed_by_an_enrolled_passkey_is_admitted() {
    use choir_node::platform::hex_encode;
    use choir_view::{OpKind, ViewOp};

    let work = std::env::temp_dir().join("choir-node-passkeys-submit");
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");
    std::fs::write(work.join("acl"), "alice @node write\nalice * write\n").expect("acl");

    let mut table = AuthTable::new();
    table.insert("alice".into(), "a".into());
    let mut node = Node::bind_with_auth(&work.join("repos"), 0, Some(table)).expect("node binds");
    let port = node.port();
    node.watch_acl_file(work.join("acl")).expect("acl loads");
    node.enable_accounts(work.join("accounts.json"), None)
        .expect("accounts enable");
    node.enable_platform(
        Platform::start(Registry::new(), Box::new(MemLog::new()), ActorKey::generate())
            .expect("platform starts"),
    );
    std::thread::spawn(move || node.serve_forever());
    let base = format!("http://127.0.0.1:{port}");

    // Bob gets an account, then enrols the authenticator he will sign
    // with.
    let (_, invite) = curl(&[
        "-u", "alice:a", "-X", "POST", "--data-binary",
        r#"{"user":"bob","grants":["agents/demo write"]}"#,
        &format!("{base}/api/accounts/invite"),
    ]);
    let pair = invite["invite"].as_str().expect("invite").to_string();
    let (_, redeemed) = curl(&[
        "-u", &pair, "-X", "POST", "--data-binary", "{}",
        &format!("{base}/api/accounts/redeem"),
    ]);
    let token = redeemed["token"].as_str().expect("token").to_string();
    let bob = format!("bob:{token}");

    let (public_key, secret) = credential(&work, "submit");
    let (status, body) = curl(&[
        "-u", &bob, "-X", "POST", "--data-binary",
        &format!(r#"{{"credential_id":"bobs-laptop","public_key":"{public_key}"}}"#),
        &format!("{base}/api/accounts/passkey"),
    ]);
    assert_eq!(status, 200, "{body}");

    // The operation, and the assertion over exactly its `signing_hash`.
    // Repository-qualified, so the op authorizes against the grant bob
    // was issued rather than falling to the node-wide default.
    let op = ViewOp::new(OpKind::SetRef {
        name: "agents/demo.git:refs/heads/main".into(),
        commit: choir_oplog::ContentHash::blake3(b"a commit bob approved"),
        prev: None,
    });
    let payload = op.to_payload();
    let signing = choir_oplog::signing_hash("bob", &payload);
    let challenge = base64url(&choir_identity::webauthn_challenge(&signing));
    let client_data =
        format!(r#"{{"type":"webauthn.get","challenge":"{challenge}","origin":"{base}"}}"#)
            .into_bytes();
    let cd = work.join("submit-cd.json");
    std::fs::write(&cd, &client_data).expect("write");
    let hashed = std::process::Command::new("openssl")
        .args(["dgst", "-sha256", "-binary"])
        .arg(&cd)
        .output()
        .expect("openssl runs");
    let auth_data = vec![0x49u8; 37];
    let mut message = auth_data.clone();
    message.extend_from_slice(&hashed.stdout);
    let msg = work.join("submit-msg.bin");
    let der = work.join("submit-sig.der");
    std::fs::write(&msg, &message).expect("write");
    assert!(std::process::Command::new("openssl")
        .args(["dgst", "-sha256", "-sign"])
        .arg(&secret)
        .arg("-out")
        .arg(&der)
        .arg(&msg)
        .output()
        .expect("openssl runs")
        .status
        .success());
    let signature = std::fs::read(&der).expect("signature");

    let submit = |channel: &str, scheme: u64| {
        serde_json::json!({
            "channel": channel,
            "payload_hex": hex_encode(&payload),
            "key_id": "bobs-laptop",
            "signature_hex": hex_encode(&signature),
            "scheme": scheme,
            "authenticator_data_hex": hex_encode(&auth_data),
            "client_data_json_hex": hex_encode(&client_data),
        })
        .to_string()
    };

    // The attack the account-keyed lookup actually stops: bob holds his
    // own authenticator, so he can mint a *fresh* assertion over the
    // signing hash of alice's channel. The challenge then matches
    // perfectly and the signature is genuine. What refuses it is that
    // `bobs-laptop` is enrolled on bob and the lookup asks alice.
    //
    // Written after a mutation showed the case above failing for the
    // wrong reason: unscoping the lookup still produced a refusal, so
    // that assertion could not have been testing what it claimed.
    let alice_signing = choir_oplog::signing_hash("alice", &payload);
    let alice_challenge = base64url(&choir_identity::webauthn_challenge(&alice_signing));
    let alice_client =
        format!(r#"{{"type":"webauthn.get","challenge":"{alice_challenge}","origin":"{base}"}}"#)
            .into_bytes();
    let acd = work.join("alice-cd.json");
    std::fs::write(&acd, &alice_client).expect("write");
    let ahashed = std::process::Command::new("openssl")
        .args(["dgst", "-sha256", "-binary"])
        .arg(&acd)
        .output()
        .expect("openssl runs");
    let mut amessage = auth_data.clone();
    amessage.extend_from_slice(&ahashed.stdout);
    let amsg = work.join("alice-msg.bin");
    let ader = work.join("alice-sig.der");
    std::fs::write(&amsg, &amessage).expect("write");
    assert!(std::process::Command::new("openssl")
        .args(["dgst", "-sha256", "-sign"])
        .arg(&secret)
        .arg("-out")
        .arg(&ader)
        .arg(&amsg)
        .output()
        .expect("openssl runs")
        .status
        .success());
    let forged = serde_json::json!({
        "channel": "alice",
        "payload_hex": hex_encode(&payload),
        "key_id": "bobs-laptop",
        "signature_hex": hex_encode(&std::fs::read(&ader).expect("sig")),
        "scheme": 2,
        "authenticator_data_hex": hex_encode(&auth_data),
        "client_data_json_hex": hex_encode(&alice_client),
    })
    .to_string();
    let (status, body) = curl(&[
        "-u", &bob, "-X", "POST", "-d", &forged,
        &format!("{base}/api/submit"),
    ]);
    assert_eq!(status, 400, "{body}");
    assert_eq!(
        body["code"], "unknown_key",
        "a genuine assertion by bob's credential was accepted on alice's channel: {body}"
    );

    // Replaying this assertion on somebody else's channel is refused at
    // the lookup, which runs first. Kept because it is the cheap attack,
    // but it does not isolate the lookup: a mutation that unscoped the
    // lookup still refused this, at the challenge instead, because
    // `signing_hash` covers the channel. The next case is the one that
    // isolates it.
    let (status, body) = curl(&[
        "-u", &bob, "-X", "POST", "-d", &submit("alice", 2),
        &format!("{base}/api/submit"),
    ]);
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["code"], "unknown_key", "{body}");

    // A scheme the node does not implement is named rather than
    // reinterpreted as ed25519 and reported as a bad signature.
    let (status, body) = curl(&[
        "-u", &bob, "-X", "POST", "-d", &submit("bob", 999),
        &format!("{base}/api/submit"),
    ]);
    assert_eq!(status, 400, "{body}");
    assert!(
        body["error"]
            .as_str()
            .or_else(|| body["detail"].as_str())
            .unwrap_or_default()
            .contains("999"),
        "the refusal must name the scheme it rejected: {body}"
    );

    // And the real thing: bob's authenticator signs, and the op lands.
    let (status, body) = curl(&[
        "-u", &bob, "-X", "POST", "-d", &submit("bob", 2),
        &format!("{base}/api/submit"),
    ]);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["seq"], 0, "{body}");

    // It is in the log as bob's, signed by the credential he enrolled.
    let (status, view) = curl(&["-u", "alice:a", &format!("{base}/api/view")]);
    assert_eq!(status, 200, "{view}");
    assert!(
        view["refs"]["agents/demo.git:refs/heads/main"] != serde_json::Value::Null,
        "the ref the passkey set is missing: {view}"
    );

    std::fs::remove_dir_all(&work).ok();
}

/// The human-facing half, served: a reviewer opening a review page is
/// offered a passkey verdict, a reader is not, and the read surface below
/// is identical either way.
///
/// The unit tests in `browse.rs` cover what `verdict_buttons` emits. This
/// covers that the page actually calls it, with the authenticated user,
/// through the real request path — the join a unit test of the renderer
/// cannot make.
#[test]
fn a_reviewer_is_offered_a_passkey_verdict_and_a_reader_is_not() {
    use choir_view::{OpKind, ViewOp};

    let work = std::env::temp_dir().join("choir-node-passkeys-page");
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");
    std::fs::write(
        work.join("acl"),
        "alice @node write\nalice * write\nbob agents/demo write\n",
    )
    .expect("acl");

    let mut table = AuthTable::new();
    table.insert("alice".into(), "a".into());
    table.insert("bob".into(), "b".into());
    let author = ActorKey::generate();
    let mut registry = Registry::new();
    registry
        .register(&author.public_key_bytes())
        .expect("valid key");

    let root = work.join("repos");
    let mut node = Node::bind_with_auth(&root, 0, Some(table)).expect("node binds");
    let port = node.port();
    node.create_repo("agents/demo.git").expect("repo created");
    node.watch_acl_file(work.join("acl")).expect("acl loads");
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate())
            .expect("platform starts"),
    );
    std::thread::spawn(move || node.serve_forever());
    let base = format!("http://127.0.0.1:{port}");

    // A review that asks bob. The target is a BLAKE3 hash rather than a
    // git oid, which the page renders as "names no commit" — this test
    // is about the verdict controls, and a diff would only add setup.
    let request = ViewOp::new(OpKind::RequestReview {
        id: "r-passkey".into(),
        target: choir_oplog::ContentHash::blake3(b"a proposal"),
        reviewers: vec!["bob".into()],
        target_ref: Some("agents/demo.git:refs/heads/main".into()),
    });
    let (status, body) = curl(&[
        "-u", "alice:a", "-X", "POST", "-d",
        &crate::support::submit_body(&author, "alice", &request),
        &format!("{base}/api/submit"),
    ]);
    assert_eq!(status, 200, "{body}");

    let page = |who: &str| {
        let out = std::process::Command::new("curl")
            .args(["-s", "-u", who, &format!("{base}/r/agents/demo/review/r-passkey")])
            .output()
            .expect("curl runs");
        String::from_utf8_lossy(&out.stdout).to_string()
    };

    // Bob was asked, so bob is offered the control — and the script that
    // makes it work is inline, on the page, with no `src`.
    let bobs = page("bob:b");
    assert!(bobs.contains("Your verdict"), "the reviewer was offered nothing: {bobs}");
    assert!(bobs.contains("button class=\"verdict"), "no verdict button");
    assert!(bobs.contains("navigator.credentials.get"), "the script is missing");
    assert!(!bobs.contains("src="), "the review page fetches something");
    assert!(bobs.contains("<noscript>"), "no scripting-off fallback");
    assert!(bobs.contains("data-user=\"bob\""), "the channel is not the caller");

    // Alice can read the review and has every grant on the node, but she
    // was not asked, so there is nothing here for her to press. Authority
    // is not the same question as being a reviewer.
    let alices = page("alice:a");
    assert!(!alices.contains("Your verdict"), "a non-reviewer was offered a verdict");
    assert!(!alices.contains("button class=\"verdict"), "a non-reviewer got a verdict button");
    // She does get the comment box, and that is the distinction rather
    // than an exception to it: judgement belongs to the people asked for
    // it, discussion to anyone who may write here. This assertion used
    // to read "no script at all" and was correct until the comment box
    // landed; the workspace gate caught it, the crate-scoped run did not.
    assert!(alices.contains("Say something"), "a writer was offered no way to discuss");

    // And the read surface is the same page for both: the enhancement
    // added a section, it did not change what was already there.
    for marker in ["Proposal", "Reviewers", "Discussion", "Changes"] {
        assert!(bobs.contains(marker), "reviewer's page lost {marker}");
        assert!(alices.contains(marker), "reader's page lost {marker}");
    }

    std::fs::remove_dir_all(&work).ok();
}

/// The door D39 was missing: a human with a browser can reach a page that
/// enrols a passkey, and it shows them the ones they already have.
///
/// Served, not rendered in isolation, because the thing that was actually
/// absent was a *route* — the endpoint and the ceremony both existed in
/// pieces and nothing joined them to a URL a person could open.
#[test]
fn a_person_can_reach_a_page_that_enrols_a_passkey() {
    let work = std::env::temp_dir().join("choir-node-passkeys-account");
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");
    std::fs::write(work.join("acl"), "alice @node write\nalice * write\n").expect("acl");

    let mut table = AuthTable::new();
    table.insert("alice".into(), "a".into());
    let mut node = Node::bind_with_auth(&work.join("repos"), 0, Some(table)).expect("node binds");
    let port = node.port();
    node.watch_acl_file(work.join("acl")).expect("acl loads");
    node.enable_accounts(work.join("accounts.json"), None)
        .expect("accounts enable");
    node.enable_platform(
        Platform::start(Registry::new(), Box::new(MemLog::new()), ActorKey::generate())
            .expect("platform starts"),
    );
    std::thread::spawn(move || node.serve_forever());
    let base = format!("http://127.0.0.1:{port}");

    let page = |who: &str| {
        let out = std::process::Command::new("curl")
            .args(["-s", "-u", who, &format!("{base}/account")])
            .output()
            .expect("curl runs");
        String::from_utf8_lossy(&out.stdout).to_string()
    };

    // The operator's credential comes from the auth file, so it has no
    // account record and cannot hold a passkey. The page says that
    // instead of offering an enrolment that would 404.
    let operators = page("alice:a");
    assert!(
        operators.contains("cannot hold a passkey"),
        "the operator was not told why: {operators}"
    );

    let (_, invite) = curl(&[
        "-u", "alice:a", "-X", "POST", "--data-binary",
        r#"{"user":"bob","grants":["agents/demo read"]}"#,
        &format!("{base}/api/accounts/invite"),
    ]);
    let pair = invite["invite"].as_str().expect("invite").to_string();
    let (_, redeemed) = curl(&[
        "-u", &pair, "-X", "POST", "--data-binary", "{}",
        &format!("{base}/api/accounts/redeem"),
    ]);
    let token = redeemed["token"].as_str().expect("token").to_string();
    let bob = format!("bob:{token}");

    // Bob has an account and no passkeys: the ceremony is offered, with
    // the scripting-off fallback beside it.
    let empty = page(&bob);
    assert!(empty.contains("None yet"), "no empty state: {empty}");
    assert!(empty.contains("navigator.credentials.create"), "no ceremony");
    assert!(empty.contains("Add a passkey"), "no control");
    assert!(empty.contains("<noscript>"), "no scripting-off fallback");
    assert!(!empty.contains("src="), "the account page fetches something");

    // After enrolling, the page lists it under the name he gave it — and
    // lists it for him only.
    let (public_key, _secret) = credential(&work, "account");
    let (status, body) = curl(&[
        "-u", &bob, "-X", "POST", "--data-binary",
        &format!(
            r#"{{"credential_id":"work-laptop-cred","public_key":"{public_key}","label":"work laptop"}}"#
        ),
        &format!("{base}/api/accounts/passkey"),
    ]);
    assert_eq!(status, 200, "{body}");

    let listed = page(&bob);
    assert!(listed.contains("work laptop"), "the label is missing: {listed}");
    assert!(listed.contains("work-laptop-cred"), "the credential is missing");
    assert!(
        !page("alice:a").contains("work-laptop-cred"),
        "one account's credential is shown on another's page"
    );

    std::fs::remove_dir_all(&work).ok();
}

/// The comment half of D39's row, served and signed: prepare, sign,
/// submit, and the comment is in the log with the author the node
/// authenticated rather than the one the body asked for.
#[test]
fn a_comment_is_prepared_by_the_node_and_signed_by_a_passkey() {
    use choir_node::platform::hex_encode;
    use choir_view::{OpKind, ViewOp};

    let work = std::env::temp_dir().join("choir-node-passkeys-comment");
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");
    std::fs::write(
        work.join("acl"),
        "alice @node write\nalice * write\nbob agents/demo write\n",
    )
    .expect("acl");

    let mut table = AuthTable::new();
    table.insert("alice".into(), "a".into());
    let author = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&author.public_key_bytes()).expect("key");

    let mut node = Node::bind_with_auth(&work.join("repos"), 0, Some(table)).expect("node binds");
    let port = node.port();
    node.create_repo("agents/demo.git").expect("repo created");
    node.watch_acl_file(work.join("acl")).expect("acl loads");
    node.enable_accounts(work.join("accounts.json"), None)
        .expect("accounts enable");
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate())
            .expect("platform starts"),
    );
    std::thread::spawn(move || node.serve_forever());
    let base = format!("http://127.0.0.1:{port}");

    let (_, invite) = curl(&[
        "-u", "alice:a", "-X", "POST", "--data-binary",
        r#"{"user":"bob","grants":["agents/demo write"]}"#,
        &format!("{base}/api/accounts/invite"),
    ]);
    let pair = invite["invite"].as_str().expect("invite").to_string();
    let (_, redeemed) = curl(&[
        "-u", &pair, "-X", "POST", "--data-binary", "{}",
        &format!("{base}/api/accounts/redeem"),
    ]);
    let bob = format!("bob:{}", redeemed["token"].as_str().expect("token"));

    let (public_key, secret) = credential(&work, "commenter");
    assert_eq!(
        curl(&[
            "-u", &bob, "-X", "POST", "--data-binary",
            &format!(r#"{{"credential_id":"bobs-key","public_key":"{public_key}"}}"#),
            &format!("{base}/api/accounts/passkey"),
        ]).0,
        200
    );

    let request = ViewOp::new(OpKind::RequestReview {
        id: "r-talk".into(),
        target: choir_oplog::ContentHash::blake3(b"a proposal"),
        reviewers: vec!["alice".into()],
        target_ref: Some("agents/demo.git:refs/heads/main".into()),
    });
    assert_eq!(
        curl(&[
            "-u", "alice:a", "-X", "POST", "-d",
            &crate::support::submit_body(&author, "alice", &request),
            &format!("{base}/api/submit"),
        ]).0,
        200
    );

    // Prepare. The body names alice; the node must build bob's comment,
    // because the author is who authenticated and never who asked.
    let (status, prepared) = curl(&[
        "-u", &bob, "-X", "POST", "--data-binary",
        r#"{"kind":"comment","id":"r-talk","body":"the diff reads fine","author":"alice"}"#,
        &format!("{base}/api/prepare"),
    ]);
    assert_eq!(status, 200, "{prepared}");
    assert_eq!(prepared["channel"], "bob", "the body chose the author: {prepared}");

    let payload = choir_node::platform::hex_decode(
        prepared["payload_hex"].as_str().expect("payload"),
    )
    .expect("hex");
    match choir_view::ViewOp::from_payload(&payload).expect("a ViewOp").kind {
        OpKind::PostComment { author, body, .. } => {
            assert_eq!(author, "bob", "the prepared op names the wrong author");
            assert_eq!(body, "the diff reads fine");
        }
        other => panic!("wrong op kind: {other:?}"),
    }

    // Sign the challenge the node handed back, exactly as the page does.
    let challenge = prepared["challenge"].as_str().expect("challenge");
    let client_data =
        format!(r#"{{"type":"webauthn.get","challenge":"{challenge}","origin":"{base}"}}"#)
            .into_bytes();
    let cd = work.join("cd.json");
    std::fs::write(&cd, &client_data).expect("write");
    let hashed = std::process::Command::new("openssl")
        .args(["dgst", "-sha256", "-binary"])
        .arg(&cd)
        .output()
        .expect("openssl runs");
    let auth_data = vec![0x49u8; 37];
    let mut message = auth_data.clone();
    message.extend_from_slice(&hashed.stdout);
    let msg = work.join("msg.bin");
    let der = work.join("sig.der");
    std::fs::write(&msg, &message).expect("write");
    assert!(std::process::Command::new("openssl")
        .args(["dgst", "-sha256", "-sign"])
        .arg(&secret)
        .arg("-out")
        .arg(&der)
        .arg(&msg)
        .output()
        .expect("openssl runs")
        .status
        .success());

    let (status, body) = curl(&[
        "-u", &bob, "-X", "POST", "-d",
        &serde_json::json!({
            "channel": prepared["channel"],
            "payload_hex": prepared["payload_hex"],
            "key_id": "bobs-key",
            "scheme": 2,
            "signature_hex": hex_encode(&std::fs::read(&der).expect("sig")),
            "authenticator_data_hex": hex_encode(&auth_data),
            "client_data_json_hex": hex_encode(&client_data),
        })
        .to_string(),
        &format!("{base}/api/submit"),
    ]);
    assert_eq!(status, 200, "{body}");

    // And it is on the page, attributed to bob.
    let out = std::process::Command::new("curl")
        .args(["-s", "-u", &bob, &format!("{base}/r/agents/demo/review/r-talk")])
        .output()
        .expect("curl runs");
    let page = String::from_utf8_lossy(&out.stdout);
    assert!(page.contains("the diff reads fine"), "the comment is missing: {page}");
    assert!(page.contains("Say something"), "the box is missing");

    // An over-long comment is refused at prepare, where the person can
    // still edit it, rather than at admission where they cannot.
    let long = "x".repeat(5000);
    let (status, body) = curl(&[
        "-u", &bob, "-X", "POST", "--data-binary",
        &serde_json::json!({ "kind": "comment", "id": "r-talk", "body": long }).to_string(),
        &format!("{base}/api/prepare"),
    ]);
    assert_eq!(status, 400, "{body}");

    std::fs::remove_dir_all(&work).ok();
}

/// The header that decides whether any of D39's write path works in a
/// real browser, read off the wire rather than inferred from routing.
///
/// Every test in this file drives the node with `curl`, which does not
/// enforce CSP — so all of them passed while the scripts were blocked in
/// every actual browser. That is the gap this closes, and the reason it
/// asserts on the served header rather than on the constant.
#[test]
fn only_the_pages_that_carry_script_are_allowed_to_run_it() {
    use choir_view::{OpKind, ViewOp};

    let work = std::env::temp_dir().join("choir-node-passkeys-csp");
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");
    std::fs::write(work.join("acl"), "alice @node write\nalice * write\n").expect("acl");

    let mut table = AuthTable::new();
    table.insert("alice".into(), "a".into());
    let author = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&author.public_key_bytes()).expect("key");

    let mut node = Node::bind_with_auth(&work.join("repos"), 0, Some(table)).expect("node binds");
    let port = node.port();
    node.create_repo("agents/demo.git").expect("repo created");
    node.watch_acl_file(work.join("acl")).expect("acl loads");
    node.enable_accounts(work.join("accounts.json"), None)
        .expect("accounts enable");
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate())
            .expect("platform starts"),
    );
    std::thread::spawn(move || node.serve_forever());
    let base = format!("http://127.0.0.1:{port}");

    let request = ViewOp::new(OpKind::RequestReview {
        id: "r-csp".into(),
        target: choir_oplog::ContentHash::blake3(b"a proposal"),
        reviewers: vec!["alice".into()],
        target_ref: Some("agents/demo.git:refs/heads/main".into()),
    });
    assert_eq!(
        curl(&[
            "-u", "alice:a", "-X", "POST", "-d",
            &crate::support::submit_body(&author, "alice", &request),
            &format!("{base}/api/submit"),
        ]).0,
        200
    );

    // The `Content-Security-Policy` a URL actually answers with.
    let csp = |path: &str| {
        let out = std::process::Command::new("curl")
            .args(["-s", "-D", "-", "-o", "/dev/null", "-u", "alice:a", &format!("{base}{path}")])
            .output()
            .expect("curl runs");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .find(|line| line.to_ascii_lowercase().starts_with("content-security-policy:"))
            .map(|line| line[line.find(':').expect("a header colon") + 1..].trim().to_string())
    };

    // The two script-bearing pages, and the hashes they must carry.
    // Computed here from the served scripts would be the same mechanism
    // the node uses; these constants come from an independent SHA-256
    // (Python's hashlib) so the two derivations have to agree.
    let verdict = "'sha256-7EGlzCX77Fq0Dv7W6kFGtGfAh/m7JGJL8Ko9o8yEuo0='";
    let comment = "'sha256-p6kALqNE2wazCGuVLbD3pSO+Ul4HNDFhIidwQ3PaGak='";
    let enrol = "'sha256-+eHBwQ+sINOtDs9/x5LGhej3ZINumisaDBHssA+AA6I='";

    let review = csp("/r/agents/demo/review/r-csp").expect("the review page sends a CSP");
    assert!(review.contains(verdict), "no verdict hash: {review}");
    assert!(review.contains(comment), "no comment hash: {review}");
    assert!(review.contains("connect-src 'self'"), "fetch is blocked: {review}");
    assert!(
        !review.contains("'unsafe-inline'; script-src") && !review.contains("script-src 'unsafe"),
        "the review page permits any inline script: {review}"
    );

    let account = csp("/account").expect("the account page sends a CSP");
    assert!(account.contains(enrol), "no enrolment hash: {account}");
    assert!(account.contains("connect-src 'self'"), "fetch is blocked: {account}");
    // Each page carries only its own script. A node-wide header would
    // license the review scripts here and the enrolment script there,
    // on pages that must never run them.
    assert!(!account.contains(verdict), "the account page licenses a review script");
    assert!(!review.contains(enrol), "the review page licenses the enrolment script");

    // And every other browser surface still runs nothing at all. This is
    // the property the read path has always had and the one a single
    // shared header would have quietly spent.
    for path in ["/", "/r/", "/r/agents/demo", "/r/agents/demo/reviews"] {
        let header = csp(path).unwrap_or_else(|| panic!("{path} sends no CSP"));
        assert!(
            !header.contains("script-src"),
            "{path} may run script: {header}"
        );
        assert!(header.contains("default-src 'none'"), "{path}: {header}");
    }

    std::fs::remove_dir_all(&work).ok();
}
