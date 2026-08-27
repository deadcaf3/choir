//! Proof of work, for the one unauthenticated write on this node (D72).
//!
//! `POST /api/access` is the only route that lets somebody who holds no
//! credential put a row on this node's disk. Two things bound it. The
//! store caps the queue, so the worst case is a full queue rather than a
//! full disk. This module makes each row cost something to create, so
//! filling that queue takes work rather than sixty-four HTTP requests.
//!
//! **A cost, not a test.** No puzzle here asks whether the caller is a
//! person: the honest version of that question needs a third party
//! watching the reader, which is exactly what D28 and D59 spent effort
//! removing. What this asks is whether the caller was willing to spend
//! about [`WORK_BITS`] bits of hashing, which a person waiting a couple
//! of seconds pays without noticing and a script wanting the whole queue
//! pays sixty-four times over.
//!
//! The challenge is issued by [`crate::session::Sessions`] and spent
//! there, so a solved stamp works exactly once. Without that, one solve
//! would buy every row.
//!
//! SHA-256 rather than the BLAKE3 the rest of this node hashes with, for
//! one reason: the other half of this runs in a browser, and
//! `crypto.subtle.digest` offers SHA-256 and not BLAKE3. Shipping a
//! BLAKE3 implementation in JavaScript to avoid naming a second hash
//! function would be a hash implementation with no tests in this
//! workspace, which is the trade `choir-identity` already refused when it
//! took `serde_json` rather than hand-read a JSON object.

use sha2::{Digest, Sha256};

/// How many leading zero bits a stamp must show.
///
/// Tuned by feel and meant to be tuned again: high enough that a script
/// filling [`crate::accounts::MAX_PENDING_REQUESTS`] slots does real
/// work, low enough that a phone finishes while its owner is still
/// reading the page. Expected hashes is `2^WORK_BITS`. Nothing derives
/// from this number, so raising it is a one-line change that costs no
/// migration -- the challenge carries it to the browser on every issue.
pub(crate) const WORK_BITS: u32 = 18;

/// The bytes a stamp is taken over: the challenge, a colon, the nonce.
///
/// Written once here rather than formatted at each end, because the two
/// ends are in different languages and a disagreement about the
/// separator would look exactly like a browser that cannot compute.
fn preimage(challenge: &str, nonce: &str) -> String {
    format!("{challenge}:{nonce}")
}

/// Whether `nonce` solves `challenge` at [`WORK_BITS`].
///
/// The nonce is checked for shape before it is hashed: it is a decimal
/// counter, and anything else is a caller sending something this node
/// did not ask for. Bounded, too, so a megabyte of digits is refused
/// rather than hashed.
pub(crate) fn solved(challenge: &str, nonce: &str) -> bool {
    if nonce.is_empty() || nonce.len() > 32 || !nonce.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let digest = Sha256::digest(preimage(challenge, nonce).as_bytes());
    leading_zero_bits(&digest) >= WORK_BITS
}

/// How many zero bits a digest starts with.
///
/// Stops at the first byte that is not zero, which is what makes this
/// cheap: the answer is decided within the first three bytes for every
/// input that is not already a solution.
fn leading_zero_bits(digest: &[u8]) -> u32 {
    let mut bits = 0;
    for byte in digest {
        if *byte == 0 {
            bits += 8;
            continue;
        }
        return bits + byte.leading_zeros();
    }
    bits
}

#[cfg(test)]
mod tests {
    /// Found by the same search a browser runs, then written down: the
    /// test is that this exact pair still verifies, so a change to the
    /// preimage spelling or the bit count is caught by a value rather
    /// than by a property that would move with it.
    #[test]
    fn a_known_stamp_verifies_and_its_neighbours_do_not() {
        let challenge = "b3-0000000000000000000000000000000000000000000000000000000000000000";
        let nonce = (0..)
            .map(|n| n.to_string())
            .find(|n| super::solved(challenge, n))
            .expect("a solution exists within the search space");
        assert!(super::solved(challenge, &nonce));
        // The nonce is bound to the challenge: the same work does not
        // pay for a second row.
        assert!(!super::solved("b3-1", &nonce));
    }

    #[test]
    fn a_nonce_that_is_not_a_counter_is_refused_before_it_is_hashed() {
        for bad in ["", "-1", "1e6", " 1", "0x10", &"9".repeat(33)] {
            assert!(!super::solved("b3-0", bad), "accepted {bad:?}");
        }
    }

    #[test]
    fn leading_zero_bits_counts_what_it_says() {
        assert_eq!(super::leading_zero_bits(&[0xff]), 0);
        assert_eq!(super::leading_zero_bits(&[0x7f]), 1);
        assert_eq!(super::leading_zero_bits(&[0x00, 0x3f]), 10);
        assert_eq!(super::leading_zero_bits(&[0x00, 0x00]), 16);
    }
}
