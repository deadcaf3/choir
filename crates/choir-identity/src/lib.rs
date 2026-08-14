//! L8 identity: one ed25519 key per actor, signatures over op entries
//! (plan.md L8, D13 radicle-style key-per-agent).
//!
//! An actor's id is the self-describing [`ContentHash`] of its public
//! key, so ids survive a future signature-scheme change the same way
//! content addresses survive a hash change (D6). The signature covers
//! [`choir_oplog::OpEntry::signing_hash`] — the entry with the signature
//! field blanked — and lands in the entry's additive `author_sig` field,
//! so pre-L8 logs remain valid and verification is opt-in per deployment
//! until the daemon leaves localhost.
//!
//! Trust policy (who may touch which workspace/ref) is a later layer;
//! this crate only answers "is this entry really from that key".
//!
//! # Examples
//!
//! ```
//! use choir_identity::{ActorKey, Registry};
//! use choir_oplog::{OpEntry, FORMAT_VERSION};
//!
//! let key = ActorKey::generate();
//! let mut registry = Registry::new();
//! registry.register(&key.public_key_bytes()).unwrap();
//!
//! let mut entry = OpEntry {
//!     format_version: FORMAT_VERSION,
//!     parent: None,
//!     seq: 0,
//!     channel: "agent-1".into(),
//!     payload: b"op".to_vec(),
//!     witnesses: Vec::new(),
//!     author_sig: None,
//! };
//! key.sign_entry(&mut entry);
//! assert_eq!(registry.verify_entry(&entry).unwrap(), key.actor_id());
//! ```

use choir_hash::ContentHash;
use choir_oplog::{OpEntry, Witness};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

/// Failure modes of signing and verification.
#[derive(Debug, PartialEq, Eq)]
pub enum IdentityError {
    /// Entry has no `author_sig`.
    Unsigned,
    /// Signing key id is not in the registry.
    UnknownKey(String),
    /// Signature bytes are malformed or do not verify.
    BadSignature,
    /// Public key bytes are not a valid ed25519 key.
    BadKey,
    /// The external verifier could not be run at all, with the reason.
    ///
    /// Kept apart from [`IdentityError::BadSignature`] for the same reason
    /// [`IdentityError::UnknownKey`] is: the repairs are opposite. A
    /// signature that does not verify is evidence about the request; a
    /// verifier that will not start is evidence about the host, and
    /// answering the second with the first would report an operational
    /// fault as an attack.
    Verifier(String),
}

/// An actor's signing keypair.
pub struct ActorKey {
    signing: SigningKey,
}

impl ActorKey {
    /// Generates a fresh keypair from the OS RNG.
    pub fn generate() -> Self {
        Self {
            signing: SigningKey::generate(&mut rand_core::OsRng),
        }
    }

    /// Restores a keypair from its 32 secret bytes (e.g. loaded from an
    /// on-disk key file).
    pub fn from_secret_bytes(bytes: &[u8; 32]) -> Self {
        Self {
            signing: SigningKey::from_bytes(bytes),
        }
    }

    /// The 32 secret bytes; callers own keeping them off disk or 0600.
    pub fn secret_bytes(&self) -> [u8; 32] {
        self.signing.to_bytes()
    }

    /// Public key bytes to publish/register.
    pub fn public_key_bytes(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }

    /// This actor's id: the content address of its public key.
    pub fn actor_id(&self) -> ContentHash {
        ContentHash::blake3(&self.public_key_bytes())
    }

    /// Signs a submission's content — `(channel, payload)` — before
    /// the sequencer assigns it a position. See
    /// [`choir_oplog::signing_hash`] for what is and isn't covered.
    pub fn sign_submission(&self, channel: &str, payload: &[u8]) -> Witness {
        let hash = choir_oplog::signing_hash(channel, payload);
        let sig = self.signing.sign(hash.to_hex().as_bytes());
        Witness {
            key_id: self.actor_id().to_hex(),
            signature: sig.to_bytes().to_vec(),
        }
    }

    /// Signs `entry` in place: sets `author_sig` over the entry's
    /// [`OpEntry::signing_hash`]. Any existing signature is replaced.
    pub fn sign_entry(&self, entry: &mut OpEntry) {
        entry.author_sig = Some(self.sign_submission(&entry.channel, &entry.payload));
    }
}

/// Known public keys, indexed by actor id. This is the verification
/// side's whole world: an unregistered key is an unknown author.
#[derive(Default)]
pub struct Registry {
    keys: std::collections::HashMap<String, VerifyingKey>,
}

impl Registry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a public key and returns the actor id it now answers to.
    ///
    /// # Errors
    ///
    /// Returns [`IdentityError::BadKey`] when the bytes are not a valid
    /// ed25519 public key.
    pub fn register(&mut self, public_key_bytes: &[u8; 32]) -> Result<ContentHash, IdentityError> {
        let key = VerifyingKey::from_bytes(public_key_bytes).map_err(|_| IdentityError::BadKey)?;
        let id = ContentHash::blake3(public_key_bytes);
        self.keys.insert(id.to_hex(), key);
        Ok(id)
    }

    /// Verifies `entry`'s author signature and returns the author's id.
    ///
    /// # Errors
    ///
    /// [`IdentityError::Unsigned`] for a missing signature,
    /// [`IdentityError::UnknownKey`] for an unregistered author, and
    /// [`IdentityError::BadSignature`] when the signature does not match
    /// the entry (tampered entry or wrong key).
    pub fn verify_entry(&self, entry: &OpEntry) -> Result<ContentHash, IdentityError> {
        let sig = entry.author_sig.as_ref().ok_or(IdentityError::Unsigned)?;
        self.verify_submission(&entry.channel, &entry.payload, sig)
    }

    /// Verifies a signature over submission content — the sequencer-side
    /// check before a position is assigned. Returns the author's id.
    ///
    /// # Errors
    ///
    /// Same failure modes as [`Registry::verify_entry`], minus
    /// [`IdentityError::Unsigned`].
    pub fn verify_submission(
        &self,
        channel: &str,
        payload: &[u8],
        sig: &Witness,
    ) -> Result<ContentHash, IdentityError> {
        self.verify_signing_hash(&choir_oplog::signing_hash(channel, payload), sig)
    }

    /// Same check, for a caller that has already computed the submission's
    /// [`choir_oplog::signing_hash`] — an admission policy that indexes
    /// submissions by it, for instance. Hashing the same bytes twice per
    /// op is measurable on the write path, and the node's allocation
    /// budget is the test that says so.
    ///
    /// # Errors
    ///
    /// Same failure modes as [`Registry::verify_submission`].
    pub fn verify_signing_hash(
        &self,
        signing: &ContentHash,
        sig: &Witness,
    ) -> Result<ContentHash, IdentityError> {
        let key = self
            .keys
            .get(&sig.key_id)
            .ok_or_else(|| IdentityError::UnknownKey(sig.key_id.clone()))?;
        let signature = Signature::from_slice(&sig.signature)
            .map_err(|_| IdentityError::BadSignature)?;
        key.verify(signing.to_hex().as_bytes(), &signature)
            .map_err(|_| IdentityError::BadSignature)?;
        Ok(ContentHash::blake3(&key.to_bytes()))
    }
}

/// Prefix that turns a raw P-256 point into SubjectPublicKeyInfo DER.
///
/// A WebAuthn authenticator hands over coordinates, not a key file. This
/// is the constant `SEQUENCE { AlgorithmIdentifier { id-ecPublicKey,
/// prime256v1 }, BIT STRING }` header that precedes the uncompressed
/// `04 ‖ X(32) ‖ Y(32)` point, so building a usable key is concatenation
/// rather than a DER writer. Measured against `openssl`'s own output
/// before it was relied on (D39).
pub const P256_SPKI_PREFIX: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08,
    0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];

/// Wraps a raw uncompressed P-256 point as SubjectPublicKeyInfo DER.
///
/// # Errors
///
/// [`IdentityError::BadKey`] unless `point` is exactly the 65 bytes of an
/// uncompressed point beginning with `0x04`. Compressed points are refused
/// rather than expanded: an authenticator that sends one is doing something
/// this path has never seen, and guessing is the wrong response.
pub fn p256_point_to_spki(point: &[u8]) -> Result<Vec<u8>, IdentityError> {
    if point.len() != 65 || point[0] != 0x04 {
        return Err(IdentityError::BadKey);
    }
    let mut spki = Vec::with_capacity(P256_SPKI_PREFIX.len() + point.len());
    spki.extend_from_slice(&P256_SPKI_PREFIX);
    spki.extend_from_slice(point);
    Ok(spki)
}

/// Verifies an ECDSA-P-256/SHA-256 signature, the scheme WebAuthn calls
/// ES256 (COSE algorithm -7).
///
/// `spki_der` is the credential public key in SubjectPublicKeyInfo DER —
/// what `getPublicKey()` returns for a registration, and what
/// [`p256_point_to_spki`] builds from raw coordinates. `message` is the
/// bytes the authenticator signed, which for an assertion is
/// `authenticatorData ‖ SHA-256(clientDataJSON)`. `signature_der` is the
/// ASN.1 `(r, s)` pair, which is already the encoding `openssl` expects,
/// so nothing is reshaped in between.
///
/// Verification runs in an `openssl` subprocess rather than through a
/// P-256 crate. That is the trade `choir-bridge` already makes for RS256:
/// one more dependency against one more process, and this workspace has
/// consistently chosen the process (D39).
///
/// **This is the primitive only.** It answers "did this key sign these
/// bytes" and deliberately not "is this assertion bound to the operation
/// the caller has in mind". Binding the challenge to
/// `signing_hash(channel, payload)` belongs to the caller, and D39 carries
/// a tripwire for it because an assertion that verifies against the wrong
/// challenge is a signature attesting to something nobody agreed to.
///
/// # Errors
///
/// [`IdentityError::BadSignature`] when the signature does not verify or
/// the key is unusable, and [`IdentityError::Verifier`] when `openssl`
/// could not be run.
pub fn verify_es256(
    spki_der: &[u8],
    message: &[u8],
    signature_der: &[u8],
) -> Result<(), IdentityError> {
    use std::sync::atomic::{AtomicU64, Ordering};
    // Unique per call, not per process: verification runs on request
    // threads, and two of them sharing a path would let one delete the
    // other's key between write and read.
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let work = std::env::temp_dir().join(format!(
        "choir-es256-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let io = |e: std::io::Error| IdentityError::Verifier(e.to_string());
    std::fs::create_dir_all(&work).map_err(io)?;
    let result = verify_es256_in(&work, spki_der, message, signature_der);
    std::fs::remove_dir_all(&work).ok();
    result
}

fn verify_es256_in(
    work: &std::path::Path,
    spki_der: &[u8],
    message: &[u8],
    signature_der: &[u8],
) -> Result<(), IdentityError> {
    let io = |e: std::io::Error| IdentityError::Verifier(e.to_string());
    let key_der = work.join("key.der");
    let key_pem = work.join("key.pem");
    let msg = work.join("message.bin");
    let sig = work.join("signature.der");
    std::fs::write(&key_der, spki_der).map_err(io)?;
    std::fs::write(&msg, message).map_err(io)?;
    std::fs::write(&sig, signature_der).map_err(io)?;

    // DER in, PEM out: `dgst -verify` wants a PEM public key, and doing
    // the conversion here keeps the caller free to pass whatever the
    // browser handed it.
    let converted = std::process::Command::new("openssl")
        .args(["pkey", "-pubin", "-inform", "DER", "-in"])
        .arg(&key_der)
        .arg("-out")
        .arg(&key_pem)
        .output()
        .map_err(io)?;
    if !converted.status.success() {
        // A key openssl will not parse is a bad key, not a broken host.
        return Err(IdentityError::BadSignature);
    }

    let verified = std::process::Command::new("openssl")
        .args(["dgst", "-sha256", "-verify"])
        .arg(&key_pem)
        .arg("-signature")
        .arg(&sig)
        .arg(&msg)
        .output()
        .map_err(io)?;
    if verified.status.success() {
        Ok(())
    } else {
        Err(IdentityError::BadSignature)
    }
}
