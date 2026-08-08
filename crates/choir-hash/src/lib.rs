//! Self-describing content addressing (plan.md §E one-way door D6).
//!
//! The codec byte names the hash function, so a future hash migration adds a
//! codec instead of rewriting stored identifiers. Shared by L0 (store) and
//! L1 (op log).
//!
//! # Examples
//!
//! ```
//! use choir_hash::ContentHash;
//!
//! let h = ContentHash::blake3(b"hello");
//! assert_eq!(h.codec, 0x1e); // BLAKE3-256 per the multicodec table
//! assert_eq!(h.digest.len(), 32);
//! assert_eq!(h, ContentHash::blake3(b"hello"));
//! ```

use serde::{Deserialize, Serialize};

/// Self-describing content address (multihash-style envelope).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContentHash {
    /// Hash-function identifier; `0x1e` = BLAKE3-256, following the
    /// multicodec table.
    pub codec: u8,
    /// Raw digest bytes for `codec`.
    pub digest: Vec<u8>,
}

impl ContentHash {
    /// Hashes `data` with BLAKE3-256 and wraps it in the envelope.
    pub fn blake3(data: &[u8]) -> Self {
        Self {
            codec: 0x1e,
            digest: blake3::hash(data).as_bytes().to_vec(),
        }
    }

    /// Lowercase hex of the digest, prefixed with the codec byte
    /// (e.g. `1e-ab12…`); used for filesystem sharding and display.
    pub fn to_hex(&self) -> String {
        let mut s = format!("{:02x}-", self.codec);
        for b in &self.digest {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }
}
