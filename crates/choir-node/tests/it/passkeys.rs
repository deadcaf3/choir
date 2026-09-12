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
    let invite = json(&body)["invite"]
        .as_str()
        .expect("invite pair")
        .to_string();
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
        .args([
            "ecparam",
            "-name",
            "prime256v1",
            "-genkey",
            "-noout",
            "-out",
        ])
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

/// Signs one submission the way an authenticator does: ECDSA P-256 over
/// `authenticator_data ‖ SHA-256(client_data_json)`, with the op's
/// `signing_hash` as the challenge inside the client data.
///
/// Returns `(authenticator_data, client_data_json, signature)` — the
/// three values a submission carries. `tag` names this assertion's
/// scratch files: two assertions in one test would otherwise overwrite
/// each other's, and the loser signs the winner's bytes.
fn assertion(
    work: &std::path::Path,
    secret: &std::path::Path,
    tag: &str,
    origin: &str,
    channel: &str,
    payload: &[u8],
) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let signing = choir_oplog::signing_hash(channel, payload);
    let challenge = base64url(&choir_identity::webauthn_challenge(&signing));
    assertion_over(work, secret, tag, origin, &challenge)
}

/// The same ceremony over a challenge somebody else chose.
///
/// Signing in has no operation to commit to, so its challenge is a nonce
/// the node minted rather than a `signing_hash`. Everything after that
/// point is identical, which is the property worth keeping visible: one
/// assertion format, one verifier, two sources of bytes.
fn assertion_over(
    work: &std::path::Path,
    secret: &std::path::Path,
    tag: &str,
    origin: &str,
    challenge: &str,
) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let client_data =
        format!(r#"{{"type":"webauthn.get","challenge":"{challenge}","origin":"{origin}"}}"#)
            .into_bytes();
    let cd = work.join(format!("{tag}-cd.json"));
    std::fs::write(&cd, &client_data).expect("write");
    let hashed = std::process::Command::new("openssl")
        .args(["dgst", "-sha256", "-binary"])
        .arg(&cd)
        .output()
        .expect("openssl runs");
    assert!(hashed.status.success(), "hash clientDataJSON");
    let auth_data = vec![0x49u8; 37];
    let mut message = auth_data.clone();
    message.extend_from_slice(&hashed.stdout);
    let msg = work.join(format!("{tag}-msg.bin"));
    let der = work.join(format!("{tag}-sig.der"));
    std::fs::write(&msg, &message).expect("write");
    assert!(std::process::Command::new("openssl")
        .args(["dgst", "-sha256", "-sign"])
        .arg(secret)
        .arg("-out")
        .arg(&der)
        .arg(&msg)
        .output()
        .expect("openssl runs")
        .status
        .success());
    (
        auth_data,
        client_data,
        std::fs::read(&der).expect("signature"),
    )
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
        (
            "standard alphabet, not url",
            format!("{}+", &good[..good.len() - 1]),
        ),
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
    assert_eq!(
        status, 409,
        "a duplicate credential id was accepted: {body}"
    );

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
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
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
    let old =
        Accounts::open(older, None, BTreeSet::new()).expect("a pre-D39 store must still load");
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
    node.enable_passkeys();
    node.enable_accounts(work.join("accounts.json"), None, None)
        .expect("accounts enable");
    node.enable_platform(
        Platform::start(
            Registry::new(),
            Box::new(MemLog::new()),
            ActorKey::generate(),
        )
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
        &format!("{base}/api/accounts/invite",),
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
    assert_eq!(
        status, 404,
        "removing an absent credential must not report success"
    );

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
    node.enable_passkeys();
    node.enable_accounts(work.join("accounts.json"), None, None)
        .expect("accounts enable");
    node.enable_platform(
        Platform::start(
            Registry::new(),
            Box::new(MemLog::new()),
            ActorKey::generate(),
        )
        .expect("platform starts"),
    );
    std::thread::spawn(move || node.serve_forever());
    let base = format!("http://127.0.0.1:{port}");

    // Bob gets an account, then enrols the authenticator he will sign
    // with.
    let (_, invite) = curl(&[
        "-u",
        "alice:a",
        "-X",
        "POST",
        "--data-binary",
        r#"{"user":"bob","grants":["agents/demo write"]}"#,
        &format!("{base}/api/accounts/invite"),
    ]);
    let pair = invite["invite"].as_str().expect("invite").to_string();
    let (_, redeemed) = curl(&[
        "-u",
        &pair,
        "-X",
        "POST",
        "--data-binary",
        "{}",
        &format!("{base}/api/accounts/redeem"),
    ]);
    let token = redeemed["token"].as_str().expect("token").to_string();
    let bob = format!("bob:{token}");

    let (public_key, secret) = credential(&work, "submit");
    let (status, body) = curl(&[
        "-u",
        &bob,
        "-X",
        "POST",
        "--data-binary",
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
    let (auth_data, client_data, signature) =
        assertion(&work, &secret, "submit", &base, "bob", &payload);

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
        "-u",
        &bob,
        "-X",
        "POST",
        "-d",
        &forged,
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
        "-u",
        &bob,
        "-X",
        "POST",
        "-d",
        &submit("alice", 2),
        &format!("{base}/api/submit"),
    ]);
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["code"], "unknown_key", "{body}");

    // A scheme the node does not implement is named rather than
    // reinterpreted as ed25519 and reported as a bad signature.
    let (status, body) = curl(&[
        "-u",
        &bob,
        "-X",
        "POST",
        "-d",
        &submit("bob", 999),
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
        "-u",
        &bob,
        "-X",
        "POST",
        "-d",
        &submit("bob", 2),
        &format!("{base}/api/submit"),
    ]);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["seq"], 0, "{body}");

    // And the entry it produced is still verifiable by a third party.
    //
    // D39 put three fields inside `Witness` and therefore inside the
    // hashed form, and `/api/log` did not serve them: a client
    // rebuilding the canonical bytes from the served fields computed a
    // different hash, so a legitimate passkey-signed entry was
    // indistinguishable from a node lying about its log. Found by
    // building the verifier that would have reported it, not by any
    // test — `sync_contract.rs` exercises the recipe only over ed25519
    // entries, which are unaffected.
    // Read as alice: the op log is a node-wide read (D29), which bob's
    // repository grant does not carry.
    let (status, page) = curl(&["-u", "alice:a", &format!("{base}/api/log?from=0")]);
    assert_eq!(status, 200, "{page}");
    let entry = page["entries"]
        .as_array()
        .and_then(|entries| {
            entries
                .iter()
                .find(|e| e["author_scheme"].as_u64() == Some(2))
        })
        .unwrap_or_else(|| panic!("no passkey-signed entry in the page: {page}"));
    for field in [
        "authenticator_data_hex",
        "client_data_json_hex",
        // D45's field, and the reason this loop is the tripwire: the
        // node stamps the credential key into the witness, so an
        // `/api/log` that does not serve it puts the entry back in the
        // state described above — a legitimate entry that recomputes to
        // the wrong hash.
        "credential_key_hex",
    ] {
        assert!(
            entry[field].as_str().is_some_and(|hex| !hex.is_empty()),
            "the served entry omits `{field}`, so its hash cannot be recomputed: {entry}"
        );
    }
    // Which key, not merely that there is one. "Some bytes are present"
    // is satisfied by a node stamping anything at all, and the whole
    // claim of D45 is that the bytes are *the enrolled credential's*.
    let served_key =
        choir_node::platform::hex_decode(entry["credential_key_hex"].as_str().expect("key"))
            .expect("hex");
    assert_eq!(
        base64url(&served_key),
        public_key,
        "the node stamped a key that is not the one bob enrolled"
    );
    // The whole point, checked the way a third party would: rebuild the
    // signature from what was served and confirm the hash the node
    // claims is the hash of what it sent.
    let rebuilt = choir_oplog::Witness::webauthn_es256(
        entry["author_key"].as_str().expect("author key"),
        choir_node::platform::hex_decode(entry["author_sig_hex"].as_str().expect("sig"))
            .expect("hex"),
        choir_node::platform::hex_decode(entry["authenticator_data_hex"].as_str().expect("auth"))
            .expect("hex"),
        choir_node::platform::hex_decode(entry["client_data_json_hex"].as_str().expect("client"))
            .expect("hex"),
    )
    .with_credential_key(
        choir_node::platform::hex_decode(entry["credential_key_hex"].as_str().expect("key"))
            .expect("hex"),
    );
    let recomputed = choir_oplog::OpEntry {
        format_version: entry["format_version"].as_u64().expect("version") as u16,
        parent: entry["parent"].as_str().and_then(|hex| {
            let (codec, digest) = hex.split_once('-')?;
            Some(choir_oplog::ContentHash {
                codec: u8::from_str_radix(codec, 16).ok()?,
                digest: choir_node::platform::hex_decode(digest)?,
            })
        }),
        seq: entry["seq"].as_u64().expect("seq"),
        channel: entry["workspace"].as_str().expect("channel").to_string(),
        payload: choir_node::platform::hex_decode(entry["payload_hex"].as_str().expect("payload"))
            .expect("hex"),
        witnesses: serde_json::from_value(entry["witnesses"].clone()).expect("witnesses"),
        author_sig: Some(rebuilt),
    }
    .content_hash();
    assert_eq!(
        recomputed.to_hex(),
        entry["hash"].as_str().expect("hash"),
        "a passkey-signed entry does not hash to what the node claims: {entry}"
    );

    // And D45's property, asked the way the future asks it: check the
    // signature using only what the entry carries. No store, no
    // registry, no node — the state a Phase-4 restore leaves a reader
    // in, because `pull_backup.sh` takes the log and not `accounts.json`.
    let carried = choir_oplog::Witness::webauthn_es256(
        entry["author_key"].as_str().expect("author key"),
        choir_node::platform::hex_decode(entry["author_sig_hex"].as_str().expect("sig"))
            .expect("hex"),
        choir_node::platform::hex_decode(entry["authenticator_data_hex"].as_str().expect("auth"))
            .expect("hex"),
        choir_node::platform::hex_decode(entry["client_data_json_hex"].as_str().expect("client"))
            .expect("hex"),
    )
    .with_credential_key(served_key);
    choir_identity::verify_carried_webauthn(&choir_oplog::signing_hash("bob", &payload), &carried)
        .expect("a passkey entry must verify from the page alone");

    // The batch endpoint is a second door to the same admission, and
    // stamping had to be wired into both. Skipping it here would leave
    // the failure silent in the worst way available: the op lands, the
    // response says 200, and the entry is uncheckable forever with
    // nothing at submission time to notice.
    let second = ViewOp::new(OpKind::SetRef {
        name: "agents/demo.git:refs/heads/next".into(),
        commit: choir_oplog::ContentHash::blake3(b"a second commit"),
        prev: None,
    });
    let batched = second.to_payload();
    let (auth2, client2, sig2) = assertion(&work, &secret, "batch", &base, "bob", &batched);
    let (status, body) = curl(&[
        "-u",
        &bob,
        "-X",
        "POST",
        "--data-binary",
        &serde_json::json!({"ops": [{
            "channel": "bob",
            "payload_hex": hex_encode(&batched),
            "key_id": "bobs-laptop",
            "signature_hex": hex_encode(&sig2),
            "scheme": 2,
            "authenticator_data_hex": hex_encode(&auth2),
            "client_data_json_hex": hex_encode(&client2),
        }]})
        .to_string(),
        &format!("{base}/api/submit-batch"),
    ]);
    assert_eq!(status, 200, "{body}");

    let (status, page) = curl(&["-u", "alice:a", &format!("{base}/api/log?from=0")]);
    assert_eq!(status, 200, "{page}");
    let batched_entry = page["entries"]
        .as_array()
        .and_then(|entries| entries.iter().find(|e| e["seq"].as_u64() == Some(1)))
        .unwrap_or_else(|| panic!("the batched op did not land: {page}"));
    assert_eq!(
        batched_entry["credential_key_hex"]
            .as_str()
            .map(|hex| base64url(&choir_node::platform::hex_decode(hex).expect("hex"))),
        Some(public_key),
        "the batch path admitted a passkey op without stamping its key: {batched_entry}"
    );

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
        "-u",
        "alice:a",
        "-X",
        "POST",
        "-d",
        &crate::support::submit_body(&author, "alice", &request),
        &format!("{base}/api/submit"),
    ]);
    assert_eq!(status, 200, "{body}");

    let page = |who: &str| {
        let out = std::process::Command::new("curl")
            .args([
                "-s",
                "-u",
                who,
                &format!("{base}/r/agents/demo/review/r-passkey"),
            ])
            .output()
            .expect("curl runs");
        String::from_utf8_lossy(&out.stdout).to_string()
    };

    // Bob was asked, so bob is offered the control — and the one file
    // that makes it work is pulled in, from this node and nowhere else.
    let bobs = page("bob:b");
    assert!(
        bobs.contains("Your verdict"),
        "the reviewer was offered nothing: {bobs}"
    );
    assert!(bobs.contains("button class=\"verdict"), "no verdict button");
    assert!(
        bobs.contains("src=\"/static/webauthn.js\""),
        "the ceremony is missing"
    );
    // Two, since D81: the ceremony and the command palette, both off
    // this origin and both named here, so a third would fail this.
    assert_eq!(
        bobs.matches("src=").count(),
        2,
        "the review page fetches something else"
    );
    assert!(
        bobs.contains("src=\"/static/palette.js\""),
        "the palette is missing from a page that draws the bar"
    );
    assert!(bobs.contains("<noscript>"), "no scripting-off fallback");
    assert!(
        bobs.contains("data-user=\"bob\""),
        "the channel is not the caller"
    );

    // Alice can read the review and has every grant on the node, but she
    // was not asked, so there is nothing here for her to press. Authority
    // is not the same question as being a reviewer.
    let alices = page("alice:a");
    assert!(
        !alices.contains("Your verdict"),
        "a non-reviewer was offered a verdict"
    );
    assert!(
        !alices.contains("button class=\"verdict"),
        "a non-reviewer got a verdict button"
    );
    // She does get the comment box, and that is the distinction rather
    // than an exception to it: judgement belongs to the people asked for
    // it, discussion to anyone who may write here. This assertion used
    // to read "no script at all" and was correct until the comment box
    // landed; the workspace gate caught it, the crate-scoped run did not.
    assert!(
        alices.contains("Say something"),
        "a writer was offered no way to discuss"
    );

    // A page offering a control must not also claim it takes no writes.
    // Found by reading the rendered page rather than by any assertion:
    // the browse footer said "Read-only" directly beneath Approve,
    // Request changes and Sign and post. Every assertion passed, because
    // each one asked whether the right words were present and none could
    // ask what else was.
    for page in [&bobs, &alices] {
        assert!(
            !page.contains("Read-only"),
            "a page offering a write control still claims to be read-only: {page}"
        );
    }
    assert!(
        bobs.contains("signed operation"),
        "the footer lost the half that is true"
    );

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
    node.enable_passkeys();
    node.enable_accounts(work.join("accounts.json"), None, None)
        .expect("accounts enable");
    node.enable_platform(
        Platform::start(
            Registry::new(),
            Box::new(MemLog::new()),
            ActorKey::generate(),
        )
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
    // ...and told what they can do. A page that says only what somebody
    // cannot do is a dead end, which is what this was until it was read
    // rather than asserted on.
    assert!(
        operators.contains("write path is the CLI"),
        "the operator was left with no way forward: {operators}"
    );

    let (_, invite) = curl(&[
        "-u",
        "alice:a",
        "-X",
        "POST",
        "--data-binary",
        r#"{"user":"bob","grants":["agents/demo read"]}"#,
        &format!("{base}/api/accounts/invite"),
    ]);
    let pair = invite["invite"].as_str().expect("invite").to_string();
    let (_, redeemed) = curl(&[
        "-u",
        &pair,
        "-X",
        "POST",
        "--data-binary",
        "{}",
        &format!("{base}/api/accounts/redeem"),
    ]);
    let token = redeemed["token"].as_str().expect("token").to_string();
    let bob = format!("bob:{token}");

    // Bob has an account and no passkeys: the ceremony is offered, with
    // the scripting-off fallback beside it.
    let empty = page(&bob);
    assert!(empty.contains("None yet"), "no empty state: {empty}");
    assert!(empty.contains("src=\"/static/webauthn.js\""), "no ceremony");
    assert!(empty.contains("Add a passkey"), "no control");
    assert!(empty.contains("<noscript>"), "no scripting-off fallback");
    // Two, since D81: the ceremony and the palette this page draws the
    // bar for. Both named, so a third fails here.
    assert_eq!(
        empty.matches("src=").count(),
        2,
        "the account page fetches something else"
    );
    assert!(
        empty.contains("src=\"/static/palette.js\""),
        "the account page draws the bar and lost its key"
    );

    // After enrolling, the page lists it under the name he gave it — and
    // lists it for him only.
    let (public_key, _secret) = credential(&work, "account");
    let (status, body) = curl(&[
        "-u",
        &bob,
        "-X",
        "POST",
        "--data-binary",
        &format!(
            r#"{{"credential_id":"work-laptop-cred","public_key":"{public_key}","label":"work laptop"}}"#
        ),
        &format!("{base}/api/accounts/passkey"),
    ]);
    assert_eq!(status, 200, "{body}");

    let listed = page(&bob);
    assert!(
        listed.contains("work laptop"),
        "the label is missing: {listed}"
    );
    assert!(
        listed.contains("work-laptop-cred"),
        "the credential is missing"
    );
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
    node.enable_passkeys();
    node.enable_accounts(work.join("accounts.json"), None, None)
        .expect("accounts enable");
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate())
            .expect("platform starts"),
    );
    std::thread::spawn(move || node.serve_forever());
    let base = format!("http://127.0.0.1:{port}");

    let (_, invite) = curl(&[
        "-u",
        "alice:a",
        "-X",
        "POST",
        "--data-binary",
        r#"{"user":"bob","grants":["agents/demo write"]}"#,
        &format!("{base}/api/accounts/invite"),
    ]);
    let pair = invite["invite"].as_str().expect("invite").to_string();
    let (_, redeemed) = curl(&[
        "-u",
        &pair,
        "-X",
        "POST",
        "--data-binary",
        "{}",
        &format!("{base}/api/accounts/redeem"),
    ]);
    let bob = format!("bob:{}", redeemed["token"].as_str().expect("token"));

    let (public_key, secret) = credential(&work, "commenter");
    assert_eq!(
        curl(&[
            "-u",
            &bob,
            "-X",
            "POST",
            "--data-binary",
            &format!(r#"{{"credential_id":"bobs-key","public_key":"{public_key}"}}"#),
            &format!("{base}/api/accounts/passkey"),
        ])
        .0,
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
            "-u",
            "alice:a",
            "-X",
            "POST",
            "-d",
            &crate::support::submit_body(&author, "alice", &request),
            &format!("{base}/api/submit"),
        ])
        .0,
        200
    );

    // Prepare. The body names alice; the node must build bob's comment,
    // because the author is who authenticated and never who asked.
    let (status, prepared) = curl(&[
        "-u",
        &bob,
        "-X",
        "POST",
        "--data-binary",
        r#"{"kind":"comment","id":"r-talk","body":"the diff reads fine","author":"alice"}"#,
        &format!("{base}/api/prepare"),
    ]);
    assert_eq!(status, 200, "{prepared}");
    assert_eq!(
        prepared["channel"], "bob",
        "the body chose the author: {prepared}"
    );

    let payload =
        choir_node::platform::hex_decode(prepared["payload_hex"].as_str().expect("payload"))
            .expect("hex");
    match choir_view::ViewOp::from_payload(&payload)
        .expect("a ViewOp")
        .kind
    {
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
        "-u",
        &bob,
        "-X",
        "POST",
        "-d",
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
        .args([
            "-s",
            "-u",
            &bob,
            &format!("{base}/r/agents/demo/review/r-talk"),
        ])
        .output()
        .expect("curl runs");
    let page = String::from_utf8_lossy(&out.stdout);
    assert!(
        page.contains("the diff reads fine"),
        "the comment is missing: {page}"
    );
    assert!(page.contains("Say something"), "the box is missing");

    // An over-long comment is refused at prepare, where the person can
    // still edit it, rather than at admission where they cannot.
    let long = "x".repeat(5000);
    let (status, body) = curl(&[
        "-u",
        &bob,
        "-X",
        "POST",
        "--data-binary",
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
    node.enable_passkeys();
    node.enable_accounts(work.join("accounts.json"), None, None)
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
            "-u",
            "alice:a",
            "-X",
            "POST",
            "-d",
            &crate::support::submit_body(&author, "alice", &request),
            &format!("{base}/api/submit"),
        ])
        .0,
        200
    );

    let header_at = |path: &str, name: &str| header_of(&base, path, name);
    let csp = |path: &str| header_at(path, "content-security-policy");

    // The two pages that carry a ceremony. `'self'` and not a digest:
    // the script is one same-origin file now, so the header names a
    // source. What must never appear is `unsafe-inline` — that is the
    // fallback the digest path could reach on a host without `openssl`,
    // and it licenses every inline script on the page including one an
    // escaping miss put there.
    // `/signin` is here because it shipped without either half: no
    // script tag, and the read surface's policy that would have blocked
    // one. What a person saw was a page explaining passkeys with no
    // button, because the control starts hidden and the script is what
    // reveals it. This loop is where that should have been caught, and it
    // only checks pages it is told about.
    for path in ["/r/agents/demo/review/r-csp", "/account", "/signin"] {
        let header = csp(path).unwrap_or_else(|| panic!("{path} sends no CSP"));
        // The whole directive, compared whole. Probing it for a
        // forbidden substring is how this test used to be written, and
        // it passed against `script-src 'self' 'unsafe-inline'`: every
        // spelling somebody thought of was absent and the one that
        // matters was there. `style-src 'unsafe-inline'` is legitimate
        // and next door, so "no unsafe-inline anywhere" is not the rule
        // either — the rule is that this directive says exactly one
        // thing.
        assert_eq!(
            directive(&header, "script-src").as_deref(),
            Some("'self'"),
            "{path} runs script it should not: {header}"
        );
        assert_eq!(
            directive(&header, "connect-src").as_deref(),
            Some("'self'"),
            "{header}"
        );
        assert_eq!(
            directive(&header, "default-src").as_deref(),
            Some("'none'"),
            "{header}"
        );
    }

    // Every page that draws the bar draws it under the policy that runs
    // the palette (D81). This loop used to assert the opposite -- that
    // the read path runs nothing at all -- and that property is the one
    // D81 spent on purpose. What replaces it is the narrower true claim:
    // the directive says exactly `'self'`, so the widening is to this
    // node and to nothing else, and `default-src 'none'` still refuses
    // every fetch the two named directives do not cover.
    for path in ["/", "/r/", "/r/agents/demo", "/r/agents/demo/reviews"] {
        let header = csp(path).unwrap_or_else(|| panic!("{path} sends no CSP"));
        assert_eq!(
            directive(&header, "script-src").as_deref(),
            Some("'self'"),
            "{path} licenses script from somewhere other than this node: {header}"
        );
        assert_eq!(
            directive(&header, "connect-src").as_deref(),
            Some("'self'"),
            "{path}: {header}"
        );
        assert!(header.contains("default-src 'none'"), "{path}: {header}");
    }

    // And the pages that draw no bar keep what the loop above gave up.
    // The invite page is the one that matters: its address is a live
    // credential (D36), so a script on it would hold somebody's secret
    // in `location`. It builds its own document, renders no bar, and
    // must still be served `UNSCRIPTED_PAGE_CSP`.
    let header = csp("/join").expect("/join sends no CSP");
    assert!(
        !header.contains("script-src"),
        "/join may run script, and it draws no bar that would need it: {header}"
    );

    // The two policies are two hand-written constants, and the strict
    // one is meant to be the surface's minus two directives. Read off
    // the wire because that is where a divergence would show: drop
    // `frame-ancestors` from one of them and every page still looks
    // right, on a node that can now be framed.
    let read = csp("/join").expect("the invite door sends a CSP");
    let scripted = csp("/account").expect("the account page sends a CSP");
    // Every page that carries the tag must also be served under the policy
    // that lets it run. Read off the wire, both halves together, because a
    // page with one and not the other renders perfectly and does nothing.
    //
    // `/account` is not in this list: for an `--auth-file` operator it
    // returns before the ceremony, since that credential has no account
    // record to enrol against, and a page with nothing to drive correctly
    // loads nothing. `/signin` has no such case -- it offers the ceremony
    // or says the node does not have one.
    let body = std::process::Command::new("curl")
        .args(["-s", &format!("{base}/signin")])
        .output()
        .expect("curl runs");
    let body = String::from_utf8_lossy(&body.stdout);
    assert!(
        body.contains("/static/webauthn.js"),
        "/signin is served under a scripted policy but loads no script"
    );
    assert!(
        body.contains("signin-go"),
        "/signin renders the control the script reveals"
    );
    assert!(
        scripted.starts_with(&read),
        "the scripted policy no longer extends the read one:\n  {read}\n  {scripted}"
    );

    // `script-src 'self'` licenses a *source*, not bytes: any
    // same-origin URL a `<script src>` can name is now a candidate. What
    // stops one being loaded as code is `nosniff`, which makes a browser
    // refuse anything that is not typed as JavaScript — so it has to be
    // on everything, including git's own CGI output, which is a pusher's
    // bytes with a content type git chose.
    for path in [
        "/",
        "/r/agents/demo",
        "/r/agents/demo/review/r-csp",
        "/account",
        "/api/view",
        "/agents/demo.git/info/refs?service=git-upload-pack",
        "/no/such/page",
    ] {
        assert_eq!(
            header_at(path, "x-content-type-options").as_deref(),
            Some("nosniff"),
            "{path} may be sniffed into a script"
        );
    }

    std::fs::remove_dir_all(&work).ok();
}

/// One CSP directive's value, whole, or `None` if the policy does not
/// carry that directive at all.
fn directive(header: &str, name: &str) -> Option<String> {
    header
        .split(';')
        .map(str::trim)
        .find_map(|d| d.strip_prefix(name).map(|rest| rest.trim().to_string()))
}

/// One header off the wire, by name, lowercased for comparison.
fn header_of(base: &str, path: &str, name: &str) -> Option<String> {
    let out = std::process::Command::new("curl")
        .args([
            "-s",
            "-D",
            "-",
            "-o",
            "/dev/null",
            "-u",
            "alice:a",
            &format!("{base}{path}"),
        ])
        .output()
        .expect("curl runs");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with(&format!("{name}:")))
        .map(|line| {
            line[line.find(':').expect("a header colon") + 1..]
                .trim()
                .to_string()
        })
}

/// The text inside each `<script>` element, which is what a
/// `script-src 'self'` header refuses to run.
///
/// A page that grew one would not be a style problem: the control would
/// simply stop working in a browser, and every test here drives the node
/// with `curl`, which enforces nothing.
fn inline_script_bodies(html: &str) -> Vec<String> {
    let mut bodies = Vec::new();
    let mut rest = html;
    while let Some(at) = rest.find("<script") {
        rest = &rest[at..];
        let open = match rest.find('>') {
            Some(i) => i + 1,
            None => break,
        };
        let end = rest.find("</script>").unwrap_or(rest.len());
        if end > open {
            bodies.push(rest[open..end].to_string());
        }
        rest = &rest[end.min(rest.len())..];
        rest = rest.strip_prefix("</script>").unwrap_or(rest);
    }
    bodies
}

/// Every attribute in `html` whose name starts with `on` — the other way
/// code reaches a page, and one `script-src` says nothing about.
fn handler_attributes(html: &str) -> Vec<String> {
    let mut found = Vec::new();
    for chunk in html.split('<').skip(1) {
        for word in chunk.split('>').next().unwrap_or("").split_whitespace() {
            let name = word.split('=').next().unwrap_or("");
            if word.contains('=')
                && name.len() > 2
                && name.starts_with("on")
                && name[2..].chars().all(|c| c.is_ascii_alphabetic())
            {
                found.push(word.to_string());
            }
        }
    }
    found
}

/// Phase 5's whole claim, read off the wire: the pages carry no code,
/// the code is one file, and that file is the only JavaScript the node
/// will ever hand a browser.
#[test]
fn the_ceremony_pages_carry_no_code_and_fetch_one_file() {
    use choir_view::{OpKind, ViewOp};

    let work = std::env::temp_dir().join("choir-node-passkeys-static");
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
    node.enable_passkeys();
    node.enable_accounts(work.join("accounts.json"), None, None)
        .expect("accounts enable");
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate())
            .expect("platform starts"),
    );
    std::thread::spawn(move || node.serve_forever());
    let base = format!("http://127.0.0.1:{port}");

    // A review alice is asked to judge, so the page renders both
    // ceremonies rather than the reader's version of itself.
    let request = ViewOp::new(OpKind::RequestReview {
        id: "r-static".into(),
        target: choir_oplog::ContentHash::blake3(b"a proposal"),
        reviewers: vec!["alice".into()],
        target_ref: Some("agents/demo.git:refs/heads/main".into()),
    });
    assert_eq!(
        curl(&[
            "-u",
            "alice:a",
            "-X",
            "POST",
            "-d",
            &crate::support::submit_body(&author, "alice", &request),
            &format!("{base}/api/submit"),
        ])
        .0,
        200
    );

    let body = |who: &str, path: &str| {
        let out = std::process::Command::new("curl")
            .args(["-s", "-u", who, &format!("{base}{path}")])
            .output()
            .expect("curl runs");
        String::from_utf8_lossy(&out.stdout).to_string()
    };

    // The account page only offers enrolment to an issued account, so
    // one is issued: alice's credential comes from the operator's auth
    // file and cannot hold a passkey.
    let (_, invite) = curl(&[
        "-u",
        "alice:a",
        "-X",
        "POST",
        "--data-binary",
        r#"{"user":"bob","grants":["agents/demo read"]}"#,
        &format!("{base}/api/accounts/invite"),
    ]);
    let pair = invite["invite"].as_str().expect("invite").to_string();
    let (_, redeemed) = curl(&[
        "-u",
        &pair,
        "-X",
        "POST",
        "--data-binary",
        "{}",
        &format!("{base}/api/accounts/redeem"),
    ]);
    let bob = format!("bob:{}", redeemed["token"].as_str().expect("token"));

    let review = body("alice:a", "/r/agents/demo/review/r-static");
    let account = body(&bob, "/account");
    for (what, page) in [("review", &review), ("account", &account)] {
        // The ceremony is on the page at all — otherwise the two
        // assertions below hold for a page with no write path on it,
        // which is the shape of a test that checks nothing.
        assert!(
            page.contains("<script"),
            "{what}: no ceremony rendered: {page}"
        );
        assert_eq!(
            inline_script_bodies(page),
            Vec::<String>::new(),
            "{what} carries inline script"
        );
        assert_eq!(
            handler_attributes(page),
            Vec::<String>::new(),
            "{what} carries an event handler"
        );
        // Two files, both this node's and both named: the ceremony
        // that writes and the palette that reads (D81). The count is
        // what makes this a test rather than a description -- a third
        // script, from anywhere, fails here.
        assert_eq!(
            page.matches("<script").count(),
            2,
            "{what} pulls in more than the two files"
        );
        assert!(
            page.contains("src=\"/static/webauthn.js\""),
            "{what} fetches something else: {page}"
        );
        assert!(
            page.contains("src=\"/static/palette.js\""),
            "{what} lost the palette: {page}"
        );
    }

    // A reader with nothing to sign gets no ceremony: the read
    // surface's guarantee, kept by construction. Since D81 the page
    // does carry a script, so the claim is about *which* -- the palette
    // reads and the ceremony writes, and a page with nothing to sign
    // must not be handed the one that signs.
    let listing = body("alice:a", "/r/agents/demo/reviews");
    assert!(
        !listing.contains("/static/webauthn.js"),
        "a read page fetched the ceremony: {listing}"
    );
    assert_eq!(
        listing.matches("<script").count(),
        1,
        "a read page fetched a script that is not the palette: {listing}"
    );

    // The file itself: typed as JavaScript, since under `script-src
    // 'self'` a browser with `nosniff` runs nothing that is not.
    assert_eq!(
        header_of(&base, "/static/webauthn.js", "content-type").as_deref(),
        Some("text/javascript; charset=utf-8")
    );
    assert_eq!(
        header_of(&base, "/static/webauthn.js", "x-content-type-options").as_deref(),
        Some("nosniff")
    );
    let script = body("alice:a", "/static/webauthn.js");
    for ceremony in ["'verdict'", "'comment'", "'enrol'"] {
        assert!(
            script.contains(ceremony),
            "the served file has lost {ceremony}"
        );
    }
    assert!(!script.contains("<script"), "a script file carrying markup");

    // Served without a credential, deliberately: a browser that is sent
    // a 401 for a subresource and does not retry leaves the ceremony
    // hidden and the `<noscript>` sentence suppressed, which is an
    // absent control rather than a visible failure. There is nothing in
    // this file to protect — it is the same constant on every node.
    let out = std::process::Command::new("curl")
        .args([
            "-s",
            "-w",
            "\n%{http_code}",
            &format!("{base}/static/webauthn.js"),
        ])
        .output()
        .expect("curl runs");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let (anon, code) = text.rsplit_once('\n').expect("a status code");
    assert_eq!(
        code.trim(),
        "200",
        "the ceremony needs a credential to load"
    );
    assert_eq!(
        anon, script,
        "an anonymous reader is served different bytes"
    );

    // And it revalidates, so a page load costs a round trip and no
    // bytes rather than the file again.
    let tag = header_of(&base, "/static/webauthn.js", "etag").expect("an ETag");
    let out = std::process::Command::new("curl")
        .args([
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "-H",
            &format!("If-None-Match: {tag}"),
            &format!("{base}/static/webauthn.js"),
        ])
        .output()
        .expect("curl runs");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "304",
        "no revalidation"
    );

    std::fs::remove_dir_all(&work).ok();
}

/// D39's tripwire applied to the page D39 added, on the node shape
/// `noscript.rs` cannot build.
///
/// That module holds the rule for every read page and now includes
/// `/account`, but its node runs no `--accounts-file`, so what it can
/// check is the "no store" sentence. The case worth checking is the other
/// one: an account page with credentials on it. If the passkey table were
/// ever filled in from script rather than rendered server-side, that page
/// would still return 200, still contain `<script`, and still look right
/// in a browser with scripting on — and a reader with it off would be
/// told they have no passkeys when they have two.
///
/// Lives here rather than in `noscript.rs` because the helper that mints
/// a credential does, and because the rule reads better beside the page
/// it constrains — the same reason each page keeps its own rule about
/// what it renders, while `ui.rs` holds the one rule about the script
/// they share.
#[test]
fn the_account_page_still_lists_your_passkeys_with_scripting_disabled() {
    let work = std::env::temp_dir().join("choir-node-passkeys-noscript");
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");
    std::fs::write(work.join("acl"), "alice @node write\nalice * write\n").expect("acl");

    let mut table = AuthTable::new();
    table.insert("alice".into(), "a".into());
    let mut node = Node::bind_with_auth(&work.join("repos"), 0, Some(table)).expect("node binds");
    let port = node.port();
    node.watch_acl_file(work.join("acl")).expect("acl loads");
    node.enable_passkeys();
    node.enable_accounts(work.join("accounts.json"), None, None)
        .expect("accounts enable");
    node.enable_platform(
        Platform::start(
            Registry::new(),
            Box::new(MemLog::new()),
            ActorKey::generate(),
        )
        .expect("platform starts"),
    );
    std::thread::spawn(move || node.serve_forever());
    let base = format!("http://127.0.0.1:{port}");

    let (_, invite) = curl(&[
        "-u",
        "alice:a",
        "-X",
        "POST",
        "--data-binary",
        r#"{"user":"bob","grants":["agents/demo read"]}"#,
        &format!("{base}/api/accounts/invite"),
    ]);
    let pair = invite["invite"].as_str().expect("invite").to_string();
    let (_, redeemed) = curl(&[
        "-u",
        &pair,
        "-X",
        "POST",
        "--data-binary",
        "{}",
        &format!("{base}/api/accounts/redeem"),
    ]);
    let bob = format!("bob:{}", redeemed["token"].as_str().expect("token"));

    let (public_key, _secret) = credential(&work, "noscript");
    assert_eq!(
        curl(&[
            "-u", &bob, "-X", "POST", "--data-binary",
            &format!(
                r#"{{"credential_id":"visible-without-js","public_key":"{public_key}","label":"my phone"}}"#
            ),
            &format!("{base}/api/accounts/passkey"),
        ]).0,
        200
    );

    let out = std::process::Command::new("curl")
        .args(["-s", "-u", &bob, &format!("{base}/account")])
        .output()
        .expect("curl runs");
    let page = String::from_utf8_lossy(&out.stdout).to_string();

    // The document a browser with scripting disabled actually renders.
    let mut readable = String::with_capacity(page.len());
    let mut rest = page.as_str();
    while let Some(open) = rest.find("<script") {
        readable.push_str(&rest[..open]);
        rest = match rest[open..].find("</script>") {
            Some(close) => &rest[open + close + "</script>".len()..],
            None => "",
        };
    }
    readable.push_str(rest);

    assert!(
        !readable.contains("<script"),
        "the stripper left script behind"
    );
    // The credential id is shown truncated to sixteen characters, which
    // is a real browser's id cut to something a roster can hold. The
    // label is what a person identifies a key by; the prefix is there to
    // tell two keys apart, and asserting the prefix rather than the whole
    // id is what keeps this test about visibility instead of layout.
    for wanted in ["my phone", "visible-without-", "Passkeys"] {
        assert!(
            readable.contains(wanted),
            "a reader with scripting off cannot see `{wanted}`: {readable}"
        );
    }
    // And the way out is still named, so the page is not a dead end for
    // them either.
    assert!(
        readable.contains("/api/accounts/passkey"),
        "the scripting-off reader is told nothing about how to enrol"
    );

    std::fs::remove_dir_all(&work).ok();
}

/// The switch that lets a node offer self-service credentials without
/// offering browser signing with them (D39).
///
/// Enrolment, the write path and the enrolment page all live behind the
/// accounts store, so before this switch existed turning on
/// `--accounts-file` turned on passkeys in the same move. The private
/// beta's manifest says it offers accounts and not passkeys; this is the
/// test that the sentence is true of a running node rather than only of
/// the document.
#[test]
fn accounts_without_passkeys_offers_neither_the_ceremony_nor_the_page() {
    let work = std::env::temp_dir().join("choir-node-passkeys-switch");
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
    // Accounts on, passkeys deliberately not.
    node.enable_accounts(work.join("accounts.json"), None, None)
        .expect("accounts enable");
    node.enable_platform(
        Platform::start(
            Registry::new(),
            Box::new(MemLog::new()),
            ActorKey::generate(),
        )
        .expect("platform starts"),
    );
    std::thread::spawn(move || node.serve_forever());
    let base = format!("http://127.0.0.1:{port}");

    // Self-service itself is on: this is what separates the two switches
    // from one switch, and without it the test would pass on a node with
    // accounts off for the wrong reason.
    let (status, body) = curl(&[
        "-u",
        "alice:a",
        "-X",
        "POST",
        "--data-binary",
        r#"{"user":"bob","grants":["owner/repo.git write"]}"#,
        &format!("{base}/api/accounts/invite"),
    ]);
    assert_eq!(
        status, 200,
        "accounts must be enabled for this test: {body}"
    );

    let (public_key, _secret) = credential(&work, "cred");
    let (status, body) = curl(&[
        "-u",
        "alice:a",
        "-X",
        "POST",
        "--data-binary",
        &format!(r#"{{"credential_id":"c","public_key":"{public_key}"}}"#),
        &format!("{base}/api/accounts/passkey"),
    ]);
    assert_eq!(status, 503, "enrolment must be refused: {body}");
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|e| e.contains("passkeys are not enabled")),
        "the refusal must name the switch: {body}"
    );

    let out = std::process::Command::new("curl")
        .args(["-s", "-u", "alice:a", &format!("{base}/account")])
        .output()
        .expect("curl runs");
    let page = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        page.contains("Passkeys are not enabled"),
        "the enrolment page must refuse rather than offer a button that 503s: {page}"
    );

    std::fs::remove_dir_all(&work).ok();
}

/// A credential id is enrolled at most once across the whole node, not
/// merely once per account (D71).
///
/// Sign-in looks the account up *from* the credential id, because an
/// assertion arrives before anyone has said who they are and the id is
/// the only name in it. Neither the id nor the public key is a secret --
/// the operator's roster prints both -- so with the check scoped to one
/// account, anybody holding an account could enrol somebody else's pair
/// and their next sign-in would verify correctly against the wrong
/// account and open a session there.
#[test]
fn one_credential_id_belongs_to_one_account_across_the_node() {
    let (store, _path, work) = store_with_bob("global-unique");
    let (status, body) = store.invite(
        "alice",
        &json(r#"{"user":"carol","grants":["agents/demo read"]}"#),
    );
    assert_eq!(status, 200, "{body}");
    let invite = json(&body)["invite"]
        .as_str()
        .expect("invite pair")
        .to_string();
    let id = invite.split(':').next().expect("invite id").to_string();
    let (status, body) = store.redeem(&id, &json("{}"));
    assert_eq!(status, 200, "{body}");

    let (public_key, _secret) = credential(&work, "shared");
    let enrol = |user: &str| {
        store.enroll_passkey(
            user,
            &json(&format!(
                r#"{{"credential_id":"the-same-id","public_key":"{public_key}"}}"#
            )),
        )
    };

    let (status, body) = enrol("bob");
    assert_eq!(status, 200, "{body}");
    let (status, body) = enrol("carol");
    assert_eq!(
        status, 409,
        "a credential already enrolled elsewhere must be refused: {body}"
    );

    // And the lookup sign-in depends on answers with the one account that
    // actually holds it.
    let (owner, _spki) = store
        .account_for_credential("the-same-id")
        .expect("the credential resolves to its account");
    assert_eq!(owner, "bob");

    std::fs::remove_dir_all(&work).ok();
}

/// The whole sign-in ceremony against a running node (D71): the node
/// mints a challenge, an authenticator signs it, and what comes back is a
/// session cookie that reaches pages a credential used to be needed for.
///
/// Also the properties that make it a credential rather than a
/// formality: the challenge is spent on use, so replaying an assertion
/// opens nothing; a signature over bytes this node never issued is
/// refused; and signing out stops the cookie working.
#[test]
fn a_passkey_opens_a_browser_session_and_the_challenge_is_spent() {
    let work = std::env::temp_dir().join("choir-node-passkeys-signin");
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");
    let acl_path = work.join("acl");
    std::fs::write(&acl_path, "alice @node write\nalice * write\nbob * read\n").expect("acl file");

    let mut table = AuthTable::new();
    table.insert("alice".into(), "a".into());
    let root = work.join("repos");
    let mut node = Node::bind_with_auth(&root, 0, Some(table)).expect("node binds");
    let port = node.port();
    node.create_repo("agents/demo.git").expect("repo created");
    node.watch_acl_file(acl_path).expect("acl loads");
    node.enable_passkeys();
    node.enable_accounts(work.join("accounts.json"), None, None)
        .expect("accounts enable");
    node.enable_platform(
        Platform::start(
            Registry::new(),
            Box::new(MemLog::new()),
            ActorKey::generate(),
        )
        .expect("platform starts"),
    );
    std::thread::spawn(move || node.serve_forever());
    let base = format!("http://127.0.0.1:{port}");

    let (_, invite) = curl(&[
        "-u",
        "alice:a",
        "-X",
        "POST",
        "--data-binary",
        r#"{"user":"bob","grants":["agents/demo.git read"]}"#,
        &format!("{base}/api/accounts/invite"),
    ]);
    let pair = invite["invite"].as_str().expect("invite").to_string();
    let (_, redeemed) = curl(&[
        "-u",
        &pair,
        "-X",
        "POST",
        "--data-binary",
        "{}",
        &format!("{base}/api/accounts/redeem"),
    ]);
    let token = redeemed["token"].as_str().expect("token").to_string();
    let bob = format!("bob:{token}");

    let (public_key, secret) = credential(&work, "signin");
    let (status, body) = curl(&[
        "-u",
        &bob,
        "-X",
        "POST",
        "--data-binary",
        &format!(r#"{{"credential_id":"bobs-key","public_key":"{public_key}"}}"#),
        &format!("{base}/api/accounts/passkey"),
    ]);
    assert_eq!(status, 200, "{body}");

    // The ceremony proper. No credential is presented at any point after
    // this line: the assertion is the whole claim.
    let challenge_of = || {
        let (status, issued) = curl(&["-X", "POST", &format!("{base}/api/signin/challenge")]);
        assert_eq!(status, 200, "{issued}");
        issued["challenge"]
            .as_str()
            .expect("a challenge")
            .to_string()
    };
    use choir_node::platform::hex_encode;
    let sign = |tag: &str, challenge: &str| {
        let (auth_data, client_data, signature) =
            assertion_over(&work, &secret, tag, &base, challenge);
        serde_json::json!({
            "key_id": "bobs-key",
            "scheme": 2,
            "signature_hex": hex_encode(&signature),
            "authenticator_data_hex": hex_encode(&auth_data),
            "client_data_json_hex": hex_encode(&client_data),
        })
        .to_string()
    };

    let challenge = challenge_of();
    let jar = work.join("jar");
    let signed_in = std::process::Command::new("curl")
        .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", "-c"])
        .arg(&jar)
        .args([
            "-X",
            "POST",
            "--data-binary",
            &sign("one", &challenge),
            &format!("{base}/api/signin"),
        ])
        .output()
        .expect("curl runs");
    assert_eq!(
        String::from_utf8_lossy(&signed_in.stdout),
        "200",
        "the ceremony must open a session"
    );

    // The cookie now reaches a page that refuses an anonymous caller.
    let with_jar = |path: &str| {
        let out = std::process::Command::new("curl")
            .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", "-b"])
            .arg(&jar)
            .arg(format!("{base}{path}"))
            .output()
            .expect("curl runs");
        String::from_utf8_lossy(&out.stdout).to_string()
    };
    assert_eq!(
        with_jar("/api/view"),
        "200",
        "the session must authenticate"
    );

    // Replaying the same assertion opens nothing: the challenge it names
    // was spent by the sign-in above.
    let replay = curl(&[
        "-X",
        "POST",
        "--data-binary",
        &sign("one", &challenge),
        &format!("{base}/api/signin"),
    ]);
    assert_eq!(replay.0, 401, "a spent challenge must not sign in again");

    // Bytes this node never issued are refused even though the signature
    // over them is perfectly good.
    let forged = curl(&[
        "-X",
        "POST",
        "--data-binary",
        &sign(
            "two",
            &base64url(b"1e-0000000000000000000000000000000000000000000000000000000000000000"),
        ),
        &format!("{base}/api/signin"),
    ]);
    assert_eq!(forged.0, 401, "a challenge we never minted must be refused");

    // The control is on the page, so signing out is something a person
    // can do rather than only something an endpoint supports.
    let account = std::process::Command::new("curl")
        .args(["-s", "-b"])
        .arg(&jar)
        .arg(format!("{base}/account"))
        .output()
        .expect("curl runs");
    let account = String::from_utf8_lossy(&account.stdout);
    assert!(
        account.contains("action=\"/api/signout\""),
        "a session must be offered a way out of itself: {account}"
    );

    // Signing out forgets the token, and nothing else honours it.
    let out = std::process::Command::new("curl")
        .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", "-b"])
        .arg(&jar)
        .args(["-X", "POST", &format!("{base}/api/signout")])
        .output()
        .expect("curl runs");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "303");
    assert_eq!(
        with_jar("/api/view"),
        "401",
        "a closed session must stop authenticating"
    );

    std::fs::remove_dir_all(&work).ok();
}

/// The bootstrap: a person with no passkey can still get one (D71, D74).
///
/// Enrolment acts on an authenticated caller, and the sign-in page
/// replaced the browser's own credential dialog. Taken together that
/// closed the only door: the ceremony needs a passkey to reach the page
/// that enrols a passkey. D71 propped it open with a link back to the
/// dialog, which is the flow the screenshots of the ugly route were; the
/// page carries its own username and password form now, and this bounds
/// what that change did to the wall every other client meets.
#[test]
fn a_person_with_no_passkey_can_still_reach_the_page_that_enrols_one() {
    let work = std::env::temp_dir().join("choir-node-passkeys-bootstrap");
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");
    std::fs::write(work.join("acl"), "alice @node write\nalice * write\n").expect("acl");

    let mut table = AuthTable::new();
    table.insert("alice".into(), "a".into());
    let mut node = Node::bind_with_auth(&work.join("repos"), 0, Some(table)).expect("node binds");
    let port = node.port();
    node.watch_acl_file(work.join("acl")).expect("acl loads");
    node.enable_passkeys();
    node.enable_accounts(work.join("accounts.json"), None, None)
        .expect("accounts enable");
    std::thread::spawn(move || node.serve_forever());
    let base = format!("http://127.0.0.1:{port}");

    let headers = |args: &[&str]| {
        let out = std::process::Command::new("curl")
            .args(["-s", "-D-", "-o", "/dev/null"])
            .args(args)
            .output()
            .expect("curl runs");
        String::from_utf8_lossy(&out.stdout).to_lowercase()
    };

    // An ordinary browser route answers with the page and no challenge:
    // that is the change this test exists to bound.
    let ordinary = headers(&["-H", "Accept: text/html", &format!("{base}/account")]);
    assert!(
        !ordinary.contains("www-authenticate"),
        "a browser route must not raise the dialog any more: {ordinary}"
    );

    // There is no route left that hands a browser back to the dialog.
    // The one that used to is gone, and asking for it now is a `404`
    // like any other path this node does not serve -- not a `401` with a
    // challenge on it.
    let removed = headers(&[
        "-H",
        "Accept: text/html",
        &format!("{base}/signin/credential"),
    ]);
    assert!(
        !removed.contains("www-authenticate"),
        "the route back to browser chrome is still there: {removed}"
    );

    // A client that does not ask for HTML still gets the wall it speaks.
    // Every API client, every script and git itself authenticate this
    // way, and a page would be an unparseable answer to all of them.
    let api = headers(&[&format!("{base}/api/view")]);
    assert!(
        api.contains("www-authenticate"),
        "an API client was shown a page: {api}"
    );

    std::fs::remove_dir_all(&work).ok();
}

/// D73. The private beta runs `--read-only-browser`, and that used to
/// take the whole ceremony file with it: the flag withheld
/// `/static/webauthn.js`, so every passkey control on the node was a
/// button that loaded nothing. The manifest said `passkeys=enabled`
/// beside a posture under which no browser could enrol one.
///
/// The flag now withholds authorship. What must still work is getting a
/// credential and signing in with it; what must not is putting an
/// operation in the log from a page, which
/// `browse::a_read_only_browser_renders_no_mutation_control_anywhere`
/// holds.
#[test]
fn the_enrolment_page_works_under_a_read_only_browser() {
    let work = std::env::temp_dir().join("choir-node-passkeys-readonly");
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");
    let acl_path = work.join("acl");
    std::fs::write(&acl_path, "alice @node write\nalice * write\n").expect("acl file");

    let mut table = AuthTable::new();
    table.insert("alice".into(), "a".into());
    let mut node = Node::bind_with_auth(&work.join("repos"), 0, Some(table)).expect("node binds");
    let port = node.port();
    node.watch_acl_file(acl_path).expect("acl loads");
    node.enable_passkeys();
    node.enable_accounts(work.join("accounts.json"), None, None)
        .expect("accounts enable");
    node.disable_browser_writes();
    node.enable_platform(
        Platform::start(
            Registry::new(),
            Box::new(MemLog::new()),
            ActorKey::generate(),
        )
        .expect("platform starts"),
    );
    std::thread::spawn(move || node.serve_forever());
    let base = format!("http://127.0.0.1:{port}");

    let fetch = |path: &str, args: &[&str]| -> (u16, String) {
        let out = std::process::Command::new("curl")
            .args(["-s", "-w", "\n%{http_code}"])
            .args(args)
            .arg(format!("{base}{path}"))
            .output()
            .expect("curl runs");
        let text = String::from_utf8_lossy(&out.stdout);
        let (body, code) = text.rsplit_once('\n').expect("status line");
        (
            code.trim().parse().expect("numeric status"),
            body.to_string(),
        )
    };

    // The file the ceremonies are written in.
    let (status, script) = fetch("/static/webauthn.js", &[]);
    assert_eq!(status, 200, "the ceremony file is withheld: {script}");
    assert!(script.contains("navigator.credentials"), "{script}");

    // The page that enrols one, seen by somebody who can hold one. The
    // operator's own credential is written into the auth file rather
    // than issued, so it is the wrong reader for this: it has no account
    // record to enrol against, whatever the posture.
    let (status, minted) = crate::support::curl(&[
        "-u",
        "alice:a",
        "-X",
        "POST",
        "-d",
        r#"{"user":"bea","grants":["agents/demo.git read"]}"#,
        &format!("{base}/api/accounts/invite"),
    ]);
    assert_eq!(status, 200, "{minted}");
    let pair = minted["invite"]
        .as_str()
        .expect("an invite pair")
        .to_string();
    let (status, redeemed) = crate::support::curl(&[
        "-u",
        &pair,
        "-X",
        "POST",
        "-d",
        "{}",
        &format!("{base}/api/accounts/redeem"),
    ]);
    assert_eq!(status, 200, "{redeemed}");
    let token = redeemed["token"].as_str().expect("a token").to_string();

    let (status, page) = fetch("/account", &["-u", &format!("bea:{token}")]);
    assert_eq!(status, 200, "the enrolment page is refused: {page}");
    assert!(page.contains("/static/webauthn.js"), "no ceremony: {page}");
    assert!(
        page.contains("/api/accounts/passkey"),
        "no enrolment: {page}"
    );

    // And the sign-in page a person with a passkey lands on (D71). It
    // answers `401`: it is the unauthorized page rendered instead of the
    // browser's own credential dialog, not a successful one.
    let (status, signin) = fetch("/signin", &[]);
    assert_eq!(status, 401, "{signin}");
    assert!(signin.contains("/static/webauthn.js"), "{signin}");

    // What the flag still withholds: the endpoint that prepares an
    // operation for a browser to sign.
    let (status, prepared) = fetch("/api/prepare", &["-u", "alice:a", "-X", "POST", "-d", "{}"]);
    assert_eq!(
        status, 403,
        "browser authorship survived --read-only-browser: {prepared}"
    );

    std::fs::remove_dir_all(&work).ok();
}

/// D74. The first sign-in on a node -- the one that happens to every
/// single person, before they have a passkey to sign in with -- used to
/// be the browser's own credential dialog, reached through a link on the
/// sign-in page. Cancelling it left the reader on the word
/// `unauthorized`.
///
/// It is a form on our own page now, and this drives the whole route a
/// newcomer takes: refused with no credential, shown the page, signing
/// in with what they were issued, and landing on the page that enrols
/// the passkey they will use instead from then on.
#[test]
fn a_person_signs_in_with_a_password_on_our_page_and_lands_where_they_enrol_a_passkey() {
    let work = std::env::temp_dir().join("choir-node-passkeys-form");
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");
    let acl_path = work.join("acl");
    std::fs::write(&acl_path, "alice @node write\nalice * write\n").expect("acl file");

    let mut table = AuthTable::new();
    table.insert("alice".into(), "a".into());
    let mut node = Node::bind_with_auth(&work.join("repos"), 0, Some(table)).expect("node binds");
    let port = node.port();
    node.create_repo("agents/demo.git").expect("repo created");
    node.watch_acl_file(acl_path).expect("acl loads");
    node.enable_passkeys();
    node.enable_accounts(work.join("accounts.json"), None, None)
        .expect("accounts enable");
    node.enable_platform(
        Platform::start(
            Registry::new(),
            Box::new(MemLog::new()),
            ActorKey::generate(),
        )
        .expect("platform starts"),
    );
    std::thread::spawn(move || node.serve_forever());
    let base = format!("http://127.0.0.1:{port}");

    let head = |path: &str, args: &[&str]| -> (u16, String, String) {
        let out = std::process::Command::new("curl")
            .args(["-s", "-i", "-w", "\n%{http_code}"])
            .args(args)
            .arg(format!("{base}{path}"))
            .output()
            .expect("curl runs");
        let text = String::from_utf8_lossy(&out.stdout);
        let (rest, code) = text.rsplit_once('\n').expect("status line");
        let (headers, body) = rest.split_once("\r\n\r\n").unwrap_or((rest, ""));
        (
            code.trim().parse().expect("numeric status"),
            headers.to_string(),
            body.to_string(),
        )
    };
    let header_of = |headers: &str, name: &str| -> Option<String> {
        headers.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name)
                .then(|| value.trim().to_string())
        })
    };

    // An account to be, since a passkey is enrolled on an issued one.
    let (status, minted) = crate::support::curl(&[
        "-u",
        "alice:a",
        "-X",
        "POST",
        "-d",
        r#"{"user":"bea","grants":["agents/demo.git write"]}"#,
        &format!("{base}/api/accounts/invite"),
    ]);
    assert_eq!(status, 200, "{minted}");
    let pair = minted["invite"].as_str().expect("a pair").to_string();
    let (status, redeemed) = crate::support::curl(&[
        "-u",
        &pair,
        "-X",
        "POST",
        "-d",
        "{}",
        &format!("{base}/api/accounts/redeem"),
    ]);
    assert_eq!(status, 200, "{redeemed}");
    let token = redeemed["token"].as_str().expect("a token").to_string();

    // A browser with no credential is shown the page, not the dialog,
    // and the page carries a form rather than a link back to the dialog.
    let (status, _, page) = head("/r/", &["-H", "Accept: text/html"]);
    assert_eq!(status, 401);
    assert!(
        page.contains("name=\"secret\""),
        "no form on the page: {page}"
    );
    assert!(
        !page.contains("/signin/credential"),
        "the page still hands the reader back to browser chrome: {page}"
    );
    // And it remembers where they were going.
    assert!(page.contains("value=\"/r/\""), "{page}");

    // A wrong password is the same page again, saying one thing, and no
    // session is opened.
    let (status, headers, page) = head(
        "/signin",
        &["-X", "POST", "-d", "user=bea&secret=wrong&next=/r/"],
    );
    assert_eq!(status, 401, "{page}");
    assert!(page.contains("did not match"), "{page}");
    assert!(
        header_of(&headers, "Set-Cookie").is_none(),
        "a failed sign-in opened a session: {headers}"
    );

    // The right one opens a session and lands on the page that enrols a
    // passkey, because this account has none.
    let (status, headers, _) = head(
        "/signin",
        &[
            "-X",
            "POST",
            "-d",
            &format!("user=bea&secret={token}&next=/r/"),
        ],
    );
    assert_eq!(status, 303, "{headers}");
    assert_eq!(
        header_of(&headers, "Location").as_deref(),
        Some("/account"),
        "a newcomer was not shown where the passkey is: {headers}"
    );
    let cookie = header_of(&headers, "Set-Cookie").expect("a session cookie");
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    assert!(cookie.contains("SameSite=Lax"), "{cookie}");
    let jar: String = cookie.split(';').next().expect("cookie pair").to_string();

    // The session works: the page they were refused now renders, with no
    // credential presented at all.
    let (status, _, _) = head("/r/", &["-H", &format!("Cookie: {jar}")]);
    assert_eq!(status, 200, "the session does not authenticate");

    // The operator's own credential, which lives in the auth file and
    // never in the store, signs in on the same form. Without that the
    // one person who must get in before anybody else could not.
    let (status, headers, _) = head(
        "/signin",
        &["-X", "POST", "-d", "user=alice&secret=a&next=/people"],
    );
    assert_eq!(status, 303, "{headers}");
    assert_eq!(
        header_of(&headers, "Location").as_deref(),
        Some("/people"),
        "an operator has no account record, so nothing to enrol: {headers}"
    );

    // A form from another site cannot spend a cached credential here.
    let (status, _, _) = head(
        "/signin",
        &[
            "-X",
            "POST",
            "-H",
            "Origin: https://evil.example",
            "-d",
            &format!("user=bea&secret={token}"),
        ],
    );
    assert_eq!(status, 403);

    // And `next` cannot be turned into somewhere else entirely.
    let (status, headers, _) = head(
        "/signin",
        &[
            "-X",
            "POST",
            "-d",
            "user=alice&secret=a&next=//evil.example/",
        ],
    );
    assert_eq!(status, 303);
    assert_eq!(
        header_of(&headers, "Location").as_deref(),
        Some("/"),
        "an open redirect: {headers}"
    );

    // Git still meets the wall it speaks, because it does not ask for
    // HTML and its URLs name a repository.
    let (status, headers, _) = head("/agents/demo.git/info/refs?service=git-upload-pack", &[]);
    assert_eq!(status, 401);
    assert!(
        header_of(&headers, "WWW-Authenticate").is_some(),
        "git was shown a sign-in page: {headers}"
    );

    std::fs::remove_dir_all(&work).ok();
}

/// D75, end to end: a stranger with a link becomes an account with a
/// name they chose and a passkey, and no password is created anywhere.
///
/// The two halves this asserts are the two the user asked for. The
/// username is the redeemer's -- the node no longer picks one, and the
/// operator never typed one -- and the credential is an authenticator,
/// so nothing on the route hands over a secret to keep.
///
/// The ceremony itself runs in a browser, which no test here has. What
/// is driven instead is the `POST` the ceremony makes, with a real P-256
/// key from `openssl` standing in for the authenticator's, which is the
/// same substitution every other passkey test in this module makes.
#[test]
fn an_invite_becomes_an_account_with_a_chosen_name_and_no_password() {
    let work = std::env::temp_dir().join("choir-node-passkeys-passwordless");
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");
    std::fs::write(work.join("acl"), "alice @node write\nalice * write\n").expect("acl");

    let mut table = AuthTable::new();
    table.insert("alice".into(), "a".into());
    let mut node = Node::bind_with_auth(&work.join("repos"), 0, Some(table)).expect("node binds");
    let port = node.port();
    node.create_repo("agents/demo.git").expect("repo created");
    node.watch_acl_file(work.join("acl")).expect("acl loads");
    node.enable_passkeys();
    node.enable_accounts(work.join("accounts.json"), None, None)
        .expect("accounts enable");
    node.enable_platform(
        Platform::start(
            Registry::new(),
            Box::new(MemLog::new()),
            ActorKey::generate(),
        )
        .expect("platform starts"),
    );
    std::thread::spawn(move || node.serve_forever());
    let base = format!("http://127.0.0.1:{port}");

    let form = |path: &str, args: &[&str]| -> (u16, String, String) {
        let out = std::process::Command::new("curl")
            .args(["-s", "-i", "-w", "\n%{http_code}"])
            .args(args)
            .arg(format!("{base}{path}"))
            .output()
            .expect("curl runs");
        let text = String::from_utf8_lossy(&out.stdout);
        let (rest, code) = text.rsplit_once('\n').expect("status line");
        let (headers, body) = rest.split_once("\r\n\r\n").unwrap_or((rest, ""));
        (
            code.trim().parse().expect("numeric status"),
            headers.to_string(),
            body.to_string(),
        )
    };
    let header_of = |headers: &str, name: &str| -> Option<String> {
        headers.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name)
                .then(|| value.trim().to_string())
        })
    };

    // The operator mints a link and names nobody.
    let (status, issued) = crate::support::curl(&[
        "-u",
        "alice:a",
        "-X",
        "POST",
        "-d",
        r#"{"display_name":"Ada Lovelace","grants":["agents/demo.git write"]}"#,
        &format!("{base}/api/accounts/invite"),
    ]);
    assert_eq!(status, 200, "{issued}");
    assert!(
        issued["user"].is_null(),
        "the operator was handed a principal to give away: {issued}"
    );
    let pair = issued["invite"].as_str().expect("a pair").to_string();
    let (id, secret) = pair.split_once(':').expect("id:secret");

    // The page asks for a name, offers the ceremony, and does not
    // promise a password.
    let (status, _, page) = form(&format!("/join?i={id}&k={secret}"), &[]);
    assert_eq!(status, 200, "{page}");
    assert!(page.contains("Pick your username"), "{page}");
    assert!(page.contains("claim-go"), "no ceremony offered: {page}");
    assert!(
        !page.contains("shows you a password"),
        "the page still promises a password: {page}"
    );

    // What the ceremony posts back.
    let (public_key, _key_file) = credential(&work, "claim");
    let body = format!(
        "i={id}&k={secret}&user=ada&credential_id=cred-ada&public_key={public_key}&label=laptop"
    );
    let (status, headers, welcome) = form("/join", &["-X", "POST", "-d", &body]);
    assert_eq!(status, 200, "{welcome}");
    assert!(welcome.contains("No password"), "{welcome}");
    assert!(
        !welcome.contains("Copy this now"),
        "a password was handed over anyway: {welcome}"
    );
    // Signed in already: there is no credential for them to present, so
    // a page ending in "now log in" would be a dead end.
    let cookie = header_of(&headers, "Set-Cookie").expect("a session cookie");
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    let jar: String = cookie.split(';').next().expect("cookie pair").to_string();

    let (status, _, _) = form("/r/", &["-H", &format!("Cookie: {jar}")]);
    assert_eq!(status, 200, "the redemption did not sign them in");

    // The name is theirs, and the readable one the operator wrote is
    // beside it rather than in it.
    let (status, roster) =
        crate::support::curl(&["-u", "alice:a", &format!("{base}/api/accounts")]);
    assert_eq!(status, 200, "{roster}");
    let account = roster["accounts"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["user"] == "ada"))
        .unwrap_or_else(|| panic!("no account named `ada`: {roster}"));
    assert_eq!(account["display_name"], "Ada Lovelace");
    assert_eq!(account["passkeys"].as_array().map(Vec::len), Some(1));

    // No password exists. Not "an unknown one" -- none: basic auth has
    // no answer for this account whatever is presented.
    let (status, _, _) = form("/r/", &["-u", "ada:"]);
    assert_eq!(status, 401);
    let (status, _, _) = form("/r/", &["-u", "ada:anything"]);
    assert_eq!(status, 401);

    // And git still needs one, so it is minted on request and once.
    let (status, headers, minted) = form(
        "/account/token",
        &["-X", "POST", "-H", &format!("Cookie: {jar}")],
    );
    assert_eq!(status, 200, "{headers}{minted}");
    let token = minted
        .split("<pre class=\"cmd\">")
        .nth(1)
        .and_then(|rest| rest.split("</pre>").next())
        .expect("a token on the page")
        .to_string();
    assert!(!token.is_empty(), "{minted}");
    let (status, _, _) = form("/r/", &["-u", &format!("ada:{token}")]);
    assert_eq!(status, 200, "the minted token does not authenticate");

    std::fs::remove_dir_all(&work).ok();
}
