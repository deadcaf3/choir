//! Browser sessions established by a passkey (D39, D71).
//!
//! Until this existed the node had exactly one way to authenticate a
//! browser: `WWW-Authenticate: Basic`, which is chrome the page cannot
//! style, cannot explain, and cannot offer a passkey through. Passkeys
//! were already the node's *write* credential, so a person could approve
//! an operation with a fingerprint and still be asked for a password to
//! look at the result.
//!
//! Two pieces of short-lived state make the ceremony work, and both live
//! here rather than in a file:
//!
//! **Challenges.** WebAuthn's replay defence is that the relying party
//! chooses the bytes being signed. Elsewhere in this node those bytes are
//! the [`choir_oplog::signing_hash`] of the operation being approved
//! ([`choir_identity::webauthn_challenge`]), so an assertion always
//! commits to *what was approved*. A sign-in approves nothing, so it needs
//! a nonce instead: issued here, single use, and short lived. The nonce is
//! shaped as a [`ContentHash`] so the same verifier checks both kinds of
//! assertion rather than a second one growing beside it.
//!
//! **Live sessions.** A cookie holding an opaque token, with the mapping
//! kept in memory. Deliberately not a signed cookie carrying the user
//! name: a token that means nothing outside this process can be revoked
//! by forgetting it, cannot be forged by anyone who learns how it is
//! constructed, and takes every session with it when the node restarts.
//! Sessions surviving a restart is not a property worth a persisted secret
//! and a rotation story.
//!
//! Both stores are capped and swept, because both are reachable before
//! authentication: a challenge is issued to anybody who asks, which is
//! what makes it the one pre-auth allocation in the node that an
//! unauthenticated caller can repeat.

use std::sync::Mutex;

use choir_hash::ContentHash;

use crate::accounts::now_secs;

/// How long an issued challenge stays usable. Long enough for a person to
/// notice the prompt and touch a sensor, short enough that the store stays
/// small under a caller doing nothing but asking for challenges.
const CHALLENGE_TTL_SECS: u64 = 120;

/// How long a session lasts before the ceremony must be repeated.
const SESSION_TTL_SECS: u64 = 12 * 60 * 60;

/// The most outstanding challenges kept at once. Reached only by a caller
/// asking for them in bulk, and the oldest are dropped rather than the
/// request refused: a person mid-ceremony loses nothing, because their
/// challenge is seconds old and the ones evicted are the stale end.
const MAX_CHALLENGES: usize = 1024;

/// The most live sessions kept at once, for the same reason.
const MAX_SESSIONS: usize = 4096;

/// The name of the cookie a signed-in browser carries.
pub(crate) const COOKIE: &str = "choir_session";

/// Short-lived state for the passkey sign-in ceremony.
#[derive(Default)]
pub(crate) struct Sessions {
    /// Issued, unspent challenges: the hash a browser was asked to sign,
    /// and when it stops being accepted.
    challenges: Mutex<Vec<(ContentHash, u64)>>,
    /// Live sessions: opaque token, the account it stands for, and when it
    /// stops being accepted.
    live: Mutex<Vec<(String, String, u64)>>,
}

impl Sessions {
    /// Mints a challenge and remembers it until it is spent or expires.
    ///
    /// The bytes are the hash of a fresh key's actor id, which is how the
    /// rest of this crate asks for unpredictable bytes without taking a
    /// dependency on a random-number generator.
    pub(crate) fn issue_challenge(&self) -> ContentHash {
        let seed = choir_identity::ActorKey::generate().actor_id().to_hex();
        let challenge = ContentHash::blake3(seed.as_bytes());
        let now = now_secs();
        let mut held = self.challenges.lock().expect("challenge lock");
        held.retain(|(_, expires)| *expires > now);
        if held.len() >= MAX_CHALLENGES {
            let overflow = held.len() + 1 - MAX_CHALLENGES;
            held.drain(..overflow);
        }
        held.push((challenge.clone(), now + CHALLENGE_TTL_SECS));
        challenge
    }

    /// Spends a challenge, answering whether it was one this node issued
    /// and had not already been spent.
    ///
    /// Removing it here is the replay defence: an assertion is valid
    /// arbitrarily often against the same bytes, so the bytes must stop
    /// being acceptable the first time they are used.
    pub(crate) fn spend_challenge(&self, challenge: &ContentHash) -> bool {
        let now = now_secs();
        let mut held = self.challenges.lock().expect("challenge lock");
        held.retain(|(_, expires)| *expires > now);
        match held.iter().position(|(held, _)| held == challenge) {
            Some(at) => {
                held.remove(at);
                true
            }
            None => false,
        }
    }

    /// Opens a session for `user` and returns the token that names it.
    pub(crate) fn open(&self, user: &str) -> String {
        let token = choir_identity::ActorKey::generate().actor_id().to_hex();
        let now = now_secs();
        let mut live = self.live.lock().expect("session lock");
        live.retain(|(_, _, expires)| *expires > now);
        if live.len() >= MAX_SESSIONS {
            let overflow = live.len() + 1 - MAX_SESSIONS;
            live.drain(..overflow);
        }
        live.push((token.clone(), user.to_string(), now + SESSION_TTL_SECS));
        token
    }

    /// The account a session token stands for, if it is live.
    pub(crate) fn user(&self, token: &str) -> Option<String> {
        let now = now_secs();
        let mut live = self.live.lock().expect("session lock");
        live.retain(|(_, _, expires)| *expires > now);
        live.iter().find_map(|(held, user, _)| {
            if crate::constant_time_eq(held.as_bytes(), token.as_bytes()) {
                Some(user.clone())
            } else {
                None
            }
        })
    }

    /// Ends a session. Signing out forgets the token, which is the whole
    /// mechanism: there is nothing else anywhere that would still honour
    /// it.
    pub(crate) fn close(&self, token: &str) {
        let mut live = self.live.lock().expect("session lock");
        live.retain(|(held, _, _)| !crate::constant_time_eq(held.as_bytes(), token.as_bytes()));
    }
}

/// The value of `name` in a `Cookie:` header, if present.
///
/// Hand-parsed for the same reason the node hand-rolls base64 and its ssh
/// key format: one header, one grammar, and a dependency avoided. Splits
/// on `;` and takes the first `=`, since a cookie value may itself contain
/// one.
pub(crate) fn cookie(header: Option<&str>, name: &str) -> Option<String> {
    let header = header?;
    header.split(';').find_map(|pair| {
        let pair = pair.trim();
        let (key, value) = pair.split_once('=')?;
        if key == name {
            Some(value.to_string())
        } else {
            None
        }
    })
}
