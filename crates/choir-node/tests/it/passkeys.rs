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
