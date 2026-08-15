//! `SYNC.md`'s three checks over one served page, as a function (D17).
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
//!    still trusted at that position (D44). This is the only check that
//!    needs things the page does not carry: the public key, and the
//!    revocation positions. An entry whose key the caller does not hold
//!    is reported **unverified**, never verified; an entry signed at or
//!    after its key's revocation is a **failure**, because that is a
//!    claim the log itself contradicts rather than one this client
//!    cannot check.
//!
//! # What still cannot be verified forever
//!
//! The log records a key **id** — `hash(pubkey)` — never the public key
//! itself. So a page's signatures are checkable only while somebody still
//! holds the key material, and an operator who deletes a revoked key's
//! line from the trusted-keys file makes that key's history permanently
//! `unverified`. Revocation no longer causes that decay; deletion still
//! does. Retaining revoked keys' public material is an operator
//! responsibility nothing in the code can enforce.

use choir_hash::ContentHash;
use choir_identity::{IdentityError, Registry};
use choir_node::platform::hex_decode;
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
    /// Entries whose authorship could not be established either way.
    pub unverified: usize,
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
pub fn page(
    entries: &[serde_json::Value],
    registry: &Registry,
    revoked: &Revocations,
) -> Report {
    let mut report = Report::default();
    let mut previous: Option<(u64, String)> = None;
    for entry in entries {
        let seq = entry["seq"].as_u64().unwrap_or_default();
        let claimed = entry["hash"].as_str().unwrap_or_default().to_string();

        if let Some((last_seq, last_hash)) = &previous {
            if seq != last_seq + 1 {
                report
                    .failures
                    .push(format!("seq {seq}: follows {last_seq}, so an entry is missing"));
            }
            if entry["parent"].as_str() != Some(last_hash.as_str()) {
                report
                    .failures
                    .push(format!("seq {seq}: parent is not the previous entry's hash"));
            }
        }

        let rebuilt = rebuild(entry);
        match &rebuilt {
            Some(e) if e.content_hash().to_hex() == claimed => {}
            Some(_) => report
                .failures
                .push(format!("seq {seq}: does not hash to the hash it claims")),
            None => report
                .failures
                .push(format!("seq {seq}: cannot be rebuilt from the fields served")),
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

#[cfg(test)]
mod tests {
    use super::Revocations;
    use choir_identity::{ActorKey, Registry};
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
        registry.register(&key.public_key_bytes()).expect("valid key");
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
        assert!(report.failures[0].contains("revoked at seq 1"), "{:?}", report.failures);
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
            report.failures.iter().any(|f| f.contains("does not hash to")),
            "a forged hash passed: {report:?}"
        );
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
            report.failures.iter().any(|f| f.contains("an entry is missing")),
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
            report.failures.iter().any(|f| f.contains("does not verify")),
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
}
