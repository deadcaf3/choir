//! `SYNC.md`'s three checks over one served page, as a function (D17).
//!
//! Named for the contract it implements rather than for its first caller.
//! It lived in `choir-cli` while `choir log --verify` was the only reader
//! of a page; a seed is a second one, and it replicates through the
//! daemon, which must not depend on the CLI. This crate already owns
//! [`Registry`] and sees [`OpEntry`], so moving here adds no dependency
//! edge anywhere.
//!
//! Pure and node-free on purpose. The verification logic is the part
//! worth being certain about, and a check that can only be exercised
//! against a live node can only ever be exercised against a *correct*
//! one — so the cases that matter, a flipped hash and a broken parent
//! link, would never run. Mutations proved exactly that: removing the
//! recomputation and removing the continuity check both left the
//! end-to-end test green, because nothing ever handed it a bad page.
//!
//! # What each check establishes
//!
//! 1. **Continuity** — the page is a contiguous run and each entry's
//!    `parent` is the previous entry's `hash`. Catches a page that
//!    begins in the wrong place or has an entry dropped from the middle.
//! 2. **Recomputation** — the `hash` the node claims is the hash of the
//!    entry it was attached to. Needs nothing but the page.
//! 3. **Authorship** — the actor named actually signed it, *and* was
//!    still trusted at that position (D44). For an ed25519 entry this is
//!    the only check that needs things the page does not carry: the
//!    public key, and the revocation positions. An entry whose key the
//!    caller does not hold is reported **unverified**, never verified;
//!    an entry signed at or after its key's revocation is a **failure**,
//!    because that is a claim the log itself contradicts rather than one
//!    this client cannot check.
//!
//!    A passkey entry carries its own credential key (D45) and so needs
//!    nothing external — but for the same reason it establishes only
//!    half of what the ed25519 path does. See
//!    [`crate::sync::Report::integrity_only`].
//!
//! # What still cannot be verified forever
//!
//! For an ed25519 entry the log records a key **id** — `hash(pubkey)` —
//! never the public key itself. So those signatures are checkable only
//! while somebody still holds the key material, and an operator who
//! deletes a revoked key's line from the trusted-keys file makes that
//! key's history permanently `unverified`. Revocation no longer causes
//! that decay; deletion still does. Retaining revoked keys' public
//! material is an operator responsibility nothing in the code can
//! enforce.
//!
//! D45 fixed that for passkeys by a route not open to ed25519 keys: a
//! WebAuthn signature is useless without the authenticator data anyway,
//! so the entry already carried scheme-specific material and the
//! credential key joined it. An ed25519 signature carries nothing, and
//! adding the public key to every one of them would grow every entry to
//! re-state what a one-line file already says.
//!
//! What no passkey entry can establish, before or after D45, is that the
//! credential belonged to the channel. That binding lives in the
//! accounts store, which is server state, is not in the backup set, and
//! is deliberately erasable. An entry admitted on a credential now
//! withdrawn looks exactly like one admitted on a credential still
//! enrolled, and neither this client nor any other can tell them apart.
//!
//! D46 widens that split to every scheme, and it is a property given up
//! on purpose. The channel an entry names is an opaque handle, so
//! *verification* still needs nothing but the page and a key, while
//! *attribution* — which person that handle was — needs the accounts
//! store and gets no answer once the account is revoked. "These bytes
//! are genuine" is permanent; "this was alice" is deletable. A handle
//! this client cannot resolve is the expected state for a deleted
//! account, not a fault in the log.

use crate::{IdentityError, Registry};
use choir_hash::ContentHash;
use choir_oplog::{OpEntry, Witness};

/// What one page's verification found.
#[derive(Debug, Default)]
pub struct Report {
    /// Checks that failed. Non-empty means the page is not trustworthy.
    pub failures: Vec<String>,
    /// Things the caller should know that are not failures — above all,
    /// signatures nobody could check.
    pub notes: Vec<String>,
    /// Signatures actually verified against a held key.
    pub checked: usize,
    /// Passkey signatures that verified against the credential key the
    /// entry itself carries (D45): the bytes are intact and were signed
    /// by the credential named, and nothing here anchors that credential
    /// to a channel.
    ///
    /// Its own counter rather than a share of [`Report::checked`],
    /// because the two answer different questions and one number that
    /// means two things is the shape every wrong reading in this file
    /// has taken. Half a check is worth reporting; it is not worth
    /// reporting as a whole one.
    pub integrity_only: usize,
    /// Entries whose authorship could not be established either way.
    pub unverified: usize,
    /// The seq of the first entry any check failed on, if one did.
    ///
    /// A reader that only reports can use [`Report::failures`] alone; a
    /// replica that appends needs to know where to stop, because every
    /// entry before this one passed all three checks and is safe to keep.
    pub first_failure: Option<u64>,
}

/// Actor id (hex, as `author_key` carries it) → the log position its
/// binding was revoked at, from `/api/view`'s `bindings[].revoked.at`.
///
/// Supplied by the caller rather than folded from the page, because a
/// page is a window: the `RevokeKey` that matters may sit outside it, and
/// a verifier that inferred "not revoked" from "no revocation in view"
/// would be answering a question it cannot see.
pub type Revocations = std::collections::BTreeMap<String, u64>;

/// Runs the three checks over `entries`, in order.
///
/// Authorship is resolved **as of each entry's own seq** (D44): a
/// signature made before its key was revoked stays valid forever, and one
/// made at or after the revocation is a `failure`. Revocation is
/// append-only and positional, so this verdict is stable — re-running it
/// next year over the same page gives the same answer, which is the
/// property that makes the log auditable rather than merely current.
#[must_use]
pub fn page(entries: &[serde_json::Value], registry: &Registry, revoked: &Revocations) -> Report {
    let mut report = Report::default();
    let mut previous: Option<(u64, String)> = None;
    for entry in entries {
        let seq = entry["seq"].as_u64().unwrap_or_default();
        let claimed = entry["hash"].as_str().unwrap_or_default().to_string();
        let failed_before = report.failures.len();

        if let Some((last_seq, last_hash)) = &previous {
            if seq != last_seq + 1 {
                report.failures.push(format!(
                    "seq {seq}: follows {last_seq}, so an entry is missing"
                ));
            }
            if entry["parent"].as_str() != Some(last_hash.as_str()) {
                report.failures.push(format!(
                    "seq {seq}: parent is not the previous entry's hash"
                ));
            }
        }

        let rebuilt = rebuild(entry);
        match &rebuilt {
            Some(e) if e.content_hash().to_hex() == claimed => {}
            Some(_) => report
                .failures
                .push(format!("seq {seq}: does not hash to the hash it claims")),
            None => report.failures.push(format!(
                "seq {seq}: cannot be rebuilt from the fields served"
            )),
        }

        match (entry["author_key"].as_str(), &rebuilt) {
            (None, _) => {
                report.unverified += 1;
                report
                    .notes
                    .push(format!("seq {seq}: unsigned, so authorship is unverified"));
            }
            (Some(key_id), Some(e)) => match e.author_sig.as_ref() {
                None => report.unverified += 1,
                // A passkey carries its own key (D45), so this branch
                // answers a different question from the one below and
                // has to report a different answer. It also consults no
                // revocation table: `revoked` is keyed by ed25519 actor
                // id and a passkey's `key_id` is a credential id, a
                // different namespace — and withdrawing a credential is
                // a store edit that leaves no positional record, so
                // there is nothing here to look up rather than a lookup
                // that happens to miss.
                Some(sig) if sig.scheme_id() == choir_oplog::scheme::WEBAUTHN_ES256 => {
                    match crate::verify_carried_webauthn(&e.signing_hash(), sig) {
                        Ok(_) => {
                            report.integrity_only += 1;
                            report.notes.push(format!(
                                "seq {seq}: signed by credential {key_id}, whose key this \
                                 entry carries; the bytes are intact, and nothing in the log \
                                 ties that credential to a channel"
                            ));
                        }
                        Err(IdentityError::UnknownKey(_)) => {
                            report.unverified += 1;
                            report.notes.push(format!(
                                "seq {seq}: passkey entry carries no credential key, so it \
                                 was written before D45 and nothing can check it now"
                            ));
                        }
                        Err(error) => report.failures.push(format!(
                            "seq {seq}: passkey signature does not verify ({error:?})"
                        )),
                    }
                }
                Some(sig) => {
                    // The three outcomes are already distinct in the
                    // error type, and keeping them distinct is the whole
                    // honesty of this: a claim that failed, a claim
                    // never examined, and a claim checked. Collapsing
                    // the middle into either neighbour is how a verifier
                    // starts lying.
                    match registry.verify_signing_hash(&e.signing_hash(), sig) {
                        // The signature is good; whether the key was
                        // still trusted at this position is a second
                        // question, and the order matters. Asking about
                        // revocation first would report a forged
                        // signature from a revoked key as a revocation
                        // problem, which sends the reader to the wrong
                        // repair.
                        Ok(_) => match revoked.get(key_id) {
                            Some(at) if seq >= *at => report.failures.push(format!(
                                "seq {seq}: signed by {key_id}, revoked at seq {at}"
                            )),
                            _ => report.checked += 1,
                        },
                        Err(IdentityError::UnknownKey(_)) => {
                            report.unverified += 1;
                            report.notes.push(format!(
                                "seq {seq}: no key held for {key_id}, authorship unverified"
                            ));
                        }
                        Err(IdentityError::UnsupportedScheme(scheme)) => {
                            report.unverified += 1;
                            report.notes.push(format!(
                                "seq {seq}: signed with scheme {scheme}, which this client \
                                 cannot check; authorship unverified"
                            ));
                        }
                        Err(error) => report
                            .failures
                            .push(format!("seq {seq}: signature does not verify ({error:?})")),
                    }
                }
            },
            (Some(_), None) => report.unverified += 1,
        }
        if report.first_failure.is_none() && report.failures.len() > failed_before {
            report.first_failure = Some(seq);
        }
        previous = Some((seq, claimed));
    }
    report
}

/// Rebuilds the hashed form from the fields `/api/log` serves.
///
/// Every field of the canonical form is on the wire, which is what makes
/// the chain checkable by someone who does not trust the node — so
/// `None` means the node sent a page this client cannot verify, which is
/// itself the finding.
#[must_use]
pub fn rebuild(entry: &serde_json::Value) -> Option<OpEntry> {
    let author_sig = match entry["author_key"].as_str() {
        None => None,
        Some(key_id) => {
            let signature = hex_decode(entry["author_sig_hex"].as_str()?)?;
            Some(match entry["author_scheme"].as_u64() {
                None => Witness::ed25519(key_id, signature),
                Some(scheme) => Witness {
                    key_id: key_id.to_string(),
                    signature,
                    scheme: Some(u16::try_from(scheme).ok()?),
                    authenticator_data: entry["authenticator_data_hex"]
                        .as_str()
                        .and_then(hex_decode),
                    client_data_json: entry["client_data_json_hex"].as_str().and_then(hex_decode),
                    credential_key: entry["credential_key_hex"].as_str().and_then(hex_decode),
                },
            })
        }
    };
    Some(OpEntry {
        format_version: u16::try_from(entry["format_version"].as_u64()?).ok()?,
        parent: match entry["parent"].as_str() {
            None => None,
            Some(hex) => Some(hash_from_hex(hex)?),
        },
        seq: entry["seq"].as_u64()?,
        channel: entry["workspace"].as_str()?.to_string(),
        payload: hex_decode(entry["payload_hex"].as_str()?)?,
        witnesses: serde_json::from_value(entry["witnesses"].clone()).ok()?,
        author_sig,
    })
}

/// A `<codec>-<hex>` content hash, as every hash on the wire is spelled.
fn hash_from_hex(text: &str) -> Option<ContentHash> {
    let (codec, digest) = text.split_once('-')?;
    Some(ContentHash {
        codec: u8::from_str_radix(codec, 16).ok()?,
        digest: hex_decode(digest)?,
    })
}

/// Hex of either case to bytes; `None` on any bad input.
///
/// The same ten lines `choir_node::platform::hex_decode` holds. Copied
/// rather than shared: that one is the daemon's wire helper, and this
/// crate sits below the daemon, so sharing it would mean a dependency in
/// the wrong direction or a new crate for a loop.
fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::Revocations;
    use crate::{ActorKey, Registry};
    use choir_oplog::{OpEntry, FORMAT_VERSION};

    /// A page of `n` chained, signed entries, in the shape `/api/log`
    /// serves — built here rather than fetched, because the cases worth
    /// checking are the ones a correct node never produces.
    fn page(key: &ActorKey, n: u64) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        let mut parent: Option<choir_hash::ContentHash> = None;
        for seq in 0..n {
            let mut entry = OpEntry {
                format_version: FORMAT_VERSION,
                parent: parent.clone(),
                seq,
                channel: "agent".into(),
                payload: format!("op {seq}").into_bytes(),
                witnesses: Vec::new(),
                author_sig: None,
            };
            key.sign_entry(&mut entry);
            let hash = entry.content_hash();
            out.push(serde_json::json!({
                "seq": entry.seq,
                "workspace": entry.channel,
                "payload_hex": entry.payload.iter()
                    .map(|b| format!("{b:02x}")).collect::<String>(),
                "author_key": entry.author_sig.as_ref().map(|w| w.key_id.clone()),
                "author_sig_hex": entry.author_sig.as_ref()
                    .map(|w| w.signature.iter().map(|b| format!("{b:02x}")).collect::<String>()),
                "hash": hash.to_hex(),
                "parent": entry.parent.as_ref().map(choir_hash::ContentHash::to_hex),
                "format_version": entry.format_version,
                "witnesses": entry.witnesses,
            }));
            parent = Some(hash);
        }
        out
    }

    fn registry_for(key: &ActorKey) -> Registry {
        let mut registry = Registry::new();
        registry
            .register(&key.public_key_bytes())
            .expect("valid key");
        registry
    }

    /// The control. Without it every assertion below could be passing
    /// because verification refuses everything.
    #[test]
    fn a_good_page_verifies() {
        let key = ActorKey::generate();
        let report = super::page(&page(&key, 3), &registry_for(&key), &Revocations::new());
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.checked, 3);
        assert_eq!(report.unverified, 0);
    }

    /// A revocation map naming `key` as withdrawn at `at`.
    fn revoked_at(key: &ActorKey, at: u64) -> Revocations {
        let mut map = Revocations::new();
        map.insert(key.actor_id().to_hex(), at);
        map
    }

    /// The half the brief asks for by name: signing happened, then the
    /// key was withdrawn, and the old entries do not decay. Without this
    /// every key rotation would quietly unverify the history behind it.
    #[test]
    fn a_signature_made_before_its_key_was_revoked_still_verifies() {
        let key = ActorKey::generate();
        let report = super::page(&page(&key, 3), &registry_for(&key), &revoked_at(&key, 3));
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.checked, 3);
    }

    /// The other half, and the one that has to be a *failure* rather than
    /// a note. "Unverified" says this client could not check the claim;
    /// here the claim was checked and the log itself contradicts it.
    #[test]
    fn a_signature_made_after_its_key_was_revoked_is_a_failure() {
        let key = ActorKey::generate();
        let report = super::page(&page(&key, 3), &registry_for(&key), &revoked_at(&key, 1));
        assert_eq!(report.checked, 1, "only seq 0 predates the revocation");
        assert_eq!(report.unverified, 0, "these are refusals, not unknowns");
        assert_eq!(report.failures.len(), 2, "{:?}", report.failures);
        assert!(
            report.failures[0].contains("revoked at seq 1"),
            "{:?}",
            report.failures
        );
    }

    /// The boundary. A key revoked *at* seq N did not authorize the entry
    /// sitting at seq N: the revocation is sequenced before it, and the
    /// off-by-one in the other direction would license exactly one entry
    /// nobody approved.
    #[test]
    fn the_entry_at_the_revocation_seq_is_already_too_late() {
        let key = ActorKey::generate();
        let report = super::page(&page(&key, 2), &registry_for(&key), &revoked_at(&key, 1));
        assert_eq!(report.checked, 1);
        assert_eq!(report.failures.len(), 1, "{:?}", report.failures);
    }

    /// A node that lies about an entry's hash. Caught by recomputation
    /// alone, with no key and no trust in the server.
    #[test]
    fn a_hash_that_is_not_the_hash_of_its_entry_is_caught() {
        let key = ActorKey::generate();
        let mut entries = page(&key, 3);
        entries[1]["hash"] = serde_json::json!(
            "1e-0000000000000000000000000000000000000000000000000000000000000000"
        );
        let report = super::page(&entries, &registry_for(&key), &Revocations::new());
        assert!(
            report
                .failures
                .iter()
                .any(|f| f.contains("does not hash to")),
            "a forged hash passed: {report:?}"
        );
    }

    /// Where a replica must stop: the first entry any check failed on,
    /// and nothing at all on a good page.
    #[test]
    fn the_first_failure_names_the_seq_a_replica_stops_at() {
        let key = ActorKey::generate();
        let good = super::page(&page(&key, 3), &registry_for(&key), &Revocations::new());
        assert_eq!(good.first_failure, None);
        let mut entries = page(&key, 4);
        entries[2]["hash"] = entries[1]["hash"].clone();
        let report = super::page(&entries, &registry_for(&key), &Revocations::new());
        assert_eq!(report.first_failure, Some(2), "{:?}", report.failures);
    }

    /// A page with an entry dropped out of the middle: the parent link
    /// no longer joins, which is the check that makes paging safe.
    #[test]
    fn an_entry_dropped_from_the_middle_breaks_the_chain() {
        let key = ActorKey::generate();
        let mut entries = page(&key, 3);
        entries.remove(1);
        let report = super::page(&entries, &registry_for(&key), &Revocations::new());
        assert!(
            report.failures.iter().any(|f| f.contains("parent is not")),
            "a gap passed: {report:?}"
        );
        assert!(
            report
                .failures
                .iter()
                .any(|f| f.contains("an entry is missing")),
            "the sequence gap was not reported: {report:?}"
        );
    }

    /// A signature lifted from one entry onto another. The bytes are a
    /// real signature by a trusted key — only over different content.
    #[test]
    fn a_signature_moved_between_entries_does_not_verify() {
        let key = ActorKey::generate();
        let mut entries = page(&key, 3);
        let lifted = entries[0]["author_sig_hex"].clone();
        entries[2]["author_sig_hex"] = lifted;
        let report = super::page(&entries, &registry_for(&key), &Revocations::new());
        assert!(
            report
                .failures
                .iter()
                .any(|f| f.contains("does not verify")),
            "a replayed signature passed: {report:?}"
        );
    }

    /// The honesty case: no key held means unverified, and it must never
    /// be counted as checked.
    #[test]
    fn an_unheld_key_is_unverified_rather_than_verified() {
        let key = ActorKey::generate();
        let report = super::page(&page(&key, 2), &Registry::new(), &Revocations::new());
        assert!(report.failures.is_empty(), "an unheld key is not a failure");
        assert_eq!(report.checked, 0, "authorship was claimed without a key");
        assert_eq!(report.unverified, 2);
        assert!(report.notes.iter().any(|n| n.contains("no key held")));
    }

    /// Base64url, no padding — what a `clientDataJSON` challenge is
    /// spelled in. Ten lines rather than a dependency, and self-checking:
    /// get it wrong and every assertion below fails on a challenge
    /// mismatch rather than passing quietly.
    fn base64url(bytes: &[u8]) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let mut buf = [0u8; 3];
            buf[..chunk.len()].copy_from_slice(chunk);
            let n = u32::from_be_bytes([0, buf[0], buf[1], buf[2]]);
            for i in 0..chunk.len() + 1 {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 0x3f) as usize] as char);
            }
        }
        out
    }

    /// A scratch directory this test alone owns. These run on parallel
    /// threads in one process, so a shared name is two tests overwriting
    /// each other's key files.
    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("choir-verify-d45-{}-{tag}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    /// A P-256 credential: the private key file, and its public key as
    /// SubjectPublicKeyInfo DER — the exact bytes `getPublicKey()`
    /// returns and `Witness::credential_key` carries.
    fn credential(dir: &std::path::Path, name: &str) -> (std::path::PathBuf, Vec<u8>) {
        let secret = dir.join(format!("{name}.key"));
        let spki = dir.join(format!("{name}.der"));
        assert!(std::process::Command::new("openssl")
            .args([
                "ecparam",
                "-name",
                "prime256v1",
                "-genkey",
                "-noout",
                "-out"
            ])
            .arg(&secret)
            .output()
            .expect("openssl runs")
            .status
            .success());
        assert!(std::process::Command::new("openssl")
            .args(["ec", "-in"])
            .arg(&secret)
            .args(["-pubout", "-outform", "DER", "-out"])
            .arg(&spki)
            .output()
            .expect("openssl runs")
            .status
            .success());
        (secret, std::fs::read(&spki).expect("spki"))
    }

    /// One entry signed the way an authenticator signs one, served in
    /// `/api/log`'s shape.
    ///
    /// Built with real ECDSA rather than a fixture, because the claim
    /// under test is that these bytes verify against nothing but
    /// themselves — and a hand-written signature verifies against
    /// nothing at all. `carried` is the key written into the witness,
    /// which is a separate argument from the one that signs precisely so
    /// a test can make them disagree.
    fn passkey_entry(
        dir: &std::path::Path,
        secret: &std::path::Path,
        carried: Option<Vec<u8>>,
    ) -> serde_json::Value {
        let channel = "bob";
        let payload = b"an op bob approved".to_vec();
        let signing = choir_oplog::signing_hash(channel, &payload);
        let challenge = base64url(&crate::webauthn_challenge(&signing));
        let client_data =
            format!(r#"{{"type":"webauthn.get","challenge":"{challenge}","origin":"x"}}"#)
                .into_bytes();

        let cd = dir.join("cd.json");
        std::fs::write(&cd, &client_data).expect("write");
        let hashed = std::process::Command::new("openssl")
            .args(["dgst", "-sha256", "-binary"])
            .arg(&cd)
            .output()
            .expect("openssl runs");
        assert!(hashed.status.success(), "hash clientDataJSON");
        let authenticator_data = vec![0x49u8; 37];
        let mut message = authenticator_data.clone();
        message.extend_from_slice(&hashed.stdout);

        let msg = dir.join("assertion.bin");
        let der = dir.join("assertion.der");
        std::fs::write(&msg, &message).expect("write");
        assert!(std::process::Command::new("openssl")
            .args(["dgst", "-sha256", "-sign"])
            .arg(secret)
            .args(["-out"])
            .arg(&der)
            .arg(&msg)
            .output()
            .expect("openssl runs")
            .status
            .success());

        let mut sig = choir_oplog::Witness::webauthn_es256(
            "bobs-laptop",
            std::fs::read(&der).expect("signature"),
            authenticator_data,
            client_data,
        );
        sig.credential_key = carried;
        let entry = OpEntry {
            format_version: FORMAT_VERSION,
            parent: None,
            seq: 0,
            channel: channel.into(),
            payload,
            witnesses: Vec::new(),
            author_sig: Some(sig),
        };
        let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let sig = entry.author_sig.as_ref().expect("signed");
        let mut served = serde_json::json!({
            "seq": entry.seq,
            "workspace": entry.channel,
            "payload_hex": hex(&entry.payload),
            "author_key": sig.key_id,
            "author_sig_hex": hex(&sig.signature),
            "author_scheme": sig.scheme,
            "authenticator_data_hex": sig.authenticator_data.as_deref().map(hex),
            "client_data_json_hex": sig.client_data_json.as_deref().map(hex),
            "hash": entry.content_hash().to_hex(),
            "parent": serde_json::Value::Null,
            "format_version": entry.format_version,
            "witnesses": entry.witnesses,
        });
        if let Some(key) = sig.credential_key.as_deref() {
            served["credential_key_hex"] = serde_json::json!(hex(key));
        }
        served
    }

    /// D45's whole point: a passkey-signed entry checks out with no
    /// credential store, no registry and no node — which is the state a
    /// Phase-4 restore leaves a reader in, because `pull_backup.sh` takes
    /// the log and not the accounts file.
    #[test]
    fn a_passkey_entry_verifies_from_the_page_alone() {
        let dir = scratch("carried");
        let (secret, spki) = credential(&dir, "bob");
        let entries = vec![passkey_entry(&dir, &secret, Some(spki))];

        let report = super::page(&entries, &Registry::new(), &Revocations::new());
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.integrity_only, 1);
        assert_eq!(report.unverified, 0);
        // And never as a whole check: the credential arrived with the
        // entry, so nothing vouched for it, and reporting this as
        // `checked` would claim an anchor that does not exist.
        assert_eq!(
            report.checked, 0,
            "an unanchored key was counted as checked"
        );
        assert!(report
            .notes
            .iter()
            .any(|n| n.contains("nothing in the log ties that credential to a channel")));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The substitution the field invites: swap in a different, perfectly
    /// valid credential key. It cannot forge anything — it makes a good
    /// entry stop verifying — and it must be reported as a failure and
    /// not as something unverifiable.
    ///
    /// The served `hash` is rebuilt around the substitution, so the
    /// recomputation check passes and this test can only be satisfied by
    /// the signature check. Tampering without that step fails on the hash
    /// and proves nothing about D45.
    #[test]
    fn a_substituted_credential_key_is_a_failure() {
        let dir = scratch("swapped");
        let (secret, _) = credential(&dir, "bob");
        let (_, other) = credential(&dir, "someone-else");
        let mut entry = passkey_entry(&dir, &secret, Some(other));
        entry["hash"] = serde_json::json!(super::rebuild(&entry)
            .expect("rebuildable")
            .content_hash()
            .to_hex());

        let report = super::page(&[entry], &Registry::new(), &Revocations::new());
        assert!(
            report
                .failures
                .iter()
                .any(|f| f.contains("passkey signature does not verify")),
            "a swapped credential key passed: {report:?}"
        );
        assert_eq!(report.integrity_only, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Entries written before D45 carry no credential key, and there is
    /// no way to obtain one now. Say so, and count it as unverified —
    /// the failure bucket would accuse an honest node of lying about
    /// history it wrote correctly under the older format.
    #[test]
    fn a_passkey_entry_written_before_d45_stays_unverified() {
        let dir = scratch("pre-d45");
        let (secret, _) = credential(&dir, "bob");
        let entries = vec![passkey_entry(&dir, &secret, None)];

        let report = super::page(&entries, &Registry::new(), &Revocations::new());
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.unverified, 1);
        assert_eq!(report.integrity_only, 0);
        assert!(report
            .notes
            .iter()
            .any(|n| n.contains("written before D45")));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The counters do not leak into each other. An ed25519 page was
    /// `checked` before D45 and is `checked` after it, and adding a
    /// second bucket must not quietly reclassify anything.
    #[test]
    fn an_ed25519_page_is_untouched_by_the_passkey_bucket() {
        let key = ActorKey::generate();
        let report = super::page(&page(&key, 3), &registry_for(&key), &Revocations::new());
        assert_eq!(report.checked, 3);
        assert_eq!(report.integrity_only, 0);
    }
}
