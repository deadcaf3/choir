//! Append-only operation log: the L1 wire format and the log-backend seam.
//!
//! One-way-door rules (plan.md §E) enforced here:
//! - every persisted entry carries `format_version`
//! - hashes are self-describing (codec byte + digest), so the hash function
//!   can change under the same envelope (D6)
//! - `witnesses` exists from day 1, empty until Phase 2, so the append-only →
//!   witnessed swap (D16) is additive, not a migration

use serde::{Deserialize, Serialize};

/// Bump on any incompatible change; additive changes keep the version.
pub const FORMAT_VERSION: u16 = 1;

/// Self-describing content address (multihash-style envelope, D6).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContentHash {
    /// 0x1e = BLAKE3-256, following the multicodec table.
    pub codec: u8,
    pub digest: Vec<u8>,
}

impl ContentHash {
    pub fn blake3(data: &[u8]) -> Self {
        Self {
            codec: 0x1e,
            digest: blake3::hash(data).as_bytes().to_vec(),
        }
    }
}

/// A witness cosignature. Unused until Phase 2 (D16); present in the format
/// from the first persisted byte so adding witnessing never rewrites history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Witness {
    pub key_id: String,
    pub signature: Vec<u8>,
}

/// One operation in the log. Payload semantics live above this layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpEntry {
    pub format_version: u16,
    /// Hash of the previous entry; None only for the genesis entry.
    pub parent: Option<ContentHash>,
    /// Sequence number assigned by the single-writer sequencer.
    pub seq: u64,
    /// Workspace (agent) that submitted the op.
    pub workspace: String,
    pub payload: Vec<u8>,
    pub witnesses: Vec<Witness>,
}

impl OpEntry {
    pub fn content_hash(&self) -> ContentHash {
        let bytes = serde_json::to_vec(self).expect("OpEntry is always serializable");
        ContentHash::blake3(&bytes)
    }
}

#[derive(Debug)]
pub enum LogError {
    /// Parent hash of the appended entry does not match the current head.
    HeadMismatch,
    Io(std::io::Error),
    Corrupt(String),
}

/// The log-backend seam (D16). Conformance suite: `tests/conformance.rs`,
/// run against every implementation.
pub trait OpLog: Send {
    fn append(&mut self, entry: OpEntry) -> Result<ContentHash, LogError>;
    fn head(&self) -> Option<ContentHash>;
    fn len(&self) -> u64;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn get(&self, seq: u64) -> Option<OpEntry>;
}

/// Primary in-memory implementation (also the dev/test runtime).
#[derive(Default)]
pub struct MemLog {
    entries: Vec<OpEntry>,
    head: Option<ContentHash>,
}

impl MemLog {
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
