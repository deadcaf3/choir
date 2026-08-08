//! Append-only operation log: the L1 wire format and the log-backend seam.
//!
//! One-way-door rules (plan.md §E) enforced here:
//! - every persisted entry carries `format_version`
//! - hashes are self-describing (codec byte + digest), so the hash function
//!   can change under the same envelope (D6)
//! - `witnesses` exists from day 1, empty until Phase 2, so the append-only →
//!   witnessed swap (D16) is additive, not a migration
//!
//! # Examples
//!
//! ```
//! use choir_oplog::{MemLog, OpEntry, OpLog, FORMAT_VERSION};
//!
//! let mut log = MemLog::new();
//! let genesis = OpEntry {
//!     format_version: FORMAT_VERSION,
//!     parent: None,
//!     seq: 0,
//!     workspace: "agent-1".into(),
//!     payload: b"first op".to_vec(),
//!     witnesses: Vec::new(),
//!     author_sig: None,
//! };
//! let head = log.append(genesis).unwrap();
//! assert_eq!(log.head(), Some(head));
//! assert_eq!(log.len(), 1);
//! ```

use serde::{Deserialize, Serialize};

pub use choir_hash::ContentHash;

/// Current wire-format version. Bump on any incompatible change; additive
/// changes keep the version (plan.md §E evolution policy).
pub const FORMAT_VERSION: u16 = 1;

/// A witness cosignature. Unused until Phase 2 (D16); present in the format
/// from the first persisted byte so adding witnessing never rewrites history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Witness {
    /// Identifier of the witness key that produced [`Witness::signature`].
    pub key_id: String,
    /// Signature over the entry's content hash.
    pub signature: Vec<u8>,
}

/// One operation in the log. Payload semantics live above this layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpEntry {
    /// Wire-format version this entry was written with; see [`FORMAT_VERSION`].
    pub format_version: u16,
    /// Hash of the previous entry; `None` only for the genesis entry.
    pub parent: Option<ContentHash>,
    /// Sequence number assigned by the single-writer sequencer.
    pub seq: u64,
    /// Workspace (agent) that submitted the op.
    pub workspace: String,
    /// Opaque operation body; interpreted by the layers above L1.
    pub payload: Vec<u8>,
    /// Witness cosignatures; empty until Phase 2 (D16).
    pub witnesses: Vec<Witness>,
    /// Author signature over [`OpEntry::signing_hash`] (L8). Additive
    /// field (`serde(default)`): entries written before L8 decode with
    /// `None`, keeping [`FORMAT_VERSION`] at 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_sig: Option<Witness>,
}

impl OpEntry {
    /// Content address of this entry (its canonical serialization, hashed).
    pub fn content_hash(&self) -> ContentHash {
        let bytes = serde_json::to_vec(self).expect("OpEntry is always serializable");
        ContentHash::blake3(&bytes)
    }

    /// What the author signs: a hash over `(workspace, payload)` only.
    ///
    /// The author asserts *what* they submitted, not *where* it landed —
    /// `seq`/`parent` are assigned by the sequencer after signing (and
    /// are covered by witnesses from Phase 2). Replaying a signed op at
    /// a different position is rejected by the CAS `prev` carried inside
    /// the payload, not by the signature.
    pub fn signing_hash(&self) -> ContentHash {
        signing_hash(&self.workspace, &self.payload)
    }
}

/// Hash over an author's submission content — see
/// [`OpEntry::signing_hash`]. Standalone so clients can sign before the
/// sequencer has built the entry.
pub fn signing_hash(workspace: &str, payload: &[u8]) -> ContentHash {
    let canonical = serde_json::to_vec(&(workspace, payload)).expect("tuple always serializes");
    ContentHash::blake3(&canonical)
}

/// Failure modes of an [`OpLog`] backend.
#[derive(Debug)]
pub enum LogError {
    /// Parent hash of the appended entry does not match the current head.
    HeadMismatch,
    /// Underlying storage I/O failure.
    Io(std::io::Error),
    /// Stored data could not be decoded as valid entries.
    Corrupt(String),
}

/// The log-backend seam (D16). Conformance suite: `tests/conformance.rs`,
/// run against every implementation.
///
/// Implementations must reject appends whose `parent` is not the current
/// head, and a rejected append must not mutate the log.
pub trait OpLog: Send {
    /// Appends `entry` and returns its content hash (the new head).
    ///
    /// # Errors
    ///
    /// Returns [`LogError::HeadMismatch`] when `entry.parent` is not the
    /// current head, or a backend-specific [`LogError`] on storage failure.
    fn append(&mut self, entry: OpEntry) -> Result<ContentHash, LogError>;

    /// Content hash of the newest entry, or `None` for an empty log.
    fn head(&self) -> Option<ContentHash>;

    /// Number of entries in the log.
    fn len(&self) -> u64;

    /// Whether the log has no entries.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Entry at sequence number `seq`, or `None` if out of range.
    fn get(&self, seq: u64) -> Option<OpEntry>;
}

/// Primary in-memory implementation (also the dev/test runtime).
#[derive(Default)]
pub struct MemLog {
    entries: Vec<OpEntry>,
    head: Option<ContentHash>,
}

impl MemLog {
    /// Creates an empty in-memory log.
    pub fn new() -> Self {
        Self::default()
    }
}

impl OpLog for MemLog {
    fn append(&mut self, entry: OpEntry) -> Result<ContentHash, LogError> {
        if entry.parent != self.head {
            return Err(LogError::HeadMismatch);
        }
        let hash = entry.content_hash();
        self.entries.push(entry);
        self.head = Some(hash.clone());
        Ok(hash)
    }

    fn head(&self) -> Option<ContentHash> {
        self.head.clone()
    }

    fn len(&self) -> u64 {
        self.entries.len() as u64
    }

    fn get(&self, seq: u64) -> Option<OpEntry> {
        self.entries.get(seq as usize).cloned()
    }
}

/// Second implementation (seam rule: feature-poor is fine, broken is not):
/// JSON-lines file, append-only, rebuilt head on open.
pub struct FileLog {
    file: std::fs::File,
    entries: Vec<OpEntry>,
    head: Option<ContentHash>,
}

impl FileLog {
    /// Opens (creating if absent) the log file at `path` and replays it to
    /// rebuild the in-memory index and head.
    ///
    /// # Errors
    ///
    /// Returns [`LogError::Io`] on filesystem failure and
    /// [`LogError::Corrupt`] when an existing line fails to decode.
    pub fn open(path: &std::path::Path) -> Result<Self, LogError> {
        use std::io::{BufRead, BufReader};
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(path)
            .map_err(LogError::Io)?;
        let mut entries = Vec::new();
        let mut head = None;
        for line in BufReader::new(&file).lines() {
            let line = line.map_err(LogError::Io)?;
            let entry: OpEntry =
                serde_json::from_str(&line).map_err(|e| LogError::Corrupt(e.to_string()))?;
            head = Some(entry.content_hash());
            entries.push(entry);
        }
        Ok(Self { file, entries, head })
    }
}

impl OpLog for FileLog {
    fn append(&mut self, entry: OpEntry) -> Result<ContentHash, LogError> {
        use std::io::Write;
        if entry.parent != self.head {
            return Err(LogError::HeadMismatch);
        }
        let mut line = serde_json::to_vec(&entry).map_err(|e| LogError::Corrupt(e.to_string()))?;
        line.push(b'\n');
        self.file.write_all(&line).map_err(LogError::Io)?;
        let hash = entry.content_hash();
        self.entries.push(entry);
        self.head = Some(hash.clone());
        Ok(hash)
    }

    fn head(&self) -> Option<ContentHash> {
        self.head.clone()
    }

    fn len(&self) -> u64 {
        self.entries.len() as u64
    }

    fn get(&self, seq: u64) -> Option<OpEntry> {
        self.entries.get(seq as usize).cloned()
    }
}
