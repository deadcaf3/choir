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
//!     workspace: "agent-1".into(),
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

    /// Signs a submission's content — `(workspace, payload)` — before
    /// the sequencer assigns it a position. See
    /// [`choir_oplog::signing_hash`] for what is and isn't covered.
    pub fn sign_submission(&self, workspace: &str, payload: &[u8]) -> Witness {
        let hash = choir_oplog::signing_hash(workspace, payload);
        let sig = self.signing.sign(hash.to_hex().as_bytes());
        Witness {
            key_id: self.actor_id().to_hex(),
            signature: sig.to_bytes().to_vec(),
        }
    }

    /// Signs `entry` in place: sets `author_sig` over the entry's
    /// [`OpEntry::signing_hash`]. Any existing signature is replaced.
    pub fn sign_entry(&self, entry: &mut OpEntry) {
        entry.author_sig = Some(self.sign_submission(&entry.workspace, &entry.payload));
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
        self.verify_submission(&entry.workspace, &entry.payload, sig)
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
        workspace: &str,
        payload: &[u8],
        sig: &Witness,
    ) -> Result<ContentHash, IdentityError> {
        let key = self
            .keys
            .get(&sig.key_id)
            .ok_or_else(|| IdentityError::UnknownKey(sig.key_id.clone()))?;
        let signature = Signature::from_slice(&sig.signature)
            .map_err(|_| IdentityError::BadSignature)?;
        let hash = choir_oplog::signing_hash(workspace, payload);
        key.verify(hash.to_hex().as_bytes(), &signature)
            .map_err(|_| IdentityError::BadSignature)?;
        Ok(ContentHash::blake3(&key.to_bytes()))
    }
}
