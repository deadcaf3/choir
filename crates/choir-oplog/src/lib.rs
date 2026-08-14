//! Append-only operation log: the L1 wire format and the log-backend seam.
//!
//! One-way-door rules (DECISIONS.md) enforced here:
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
//!     channel: "agent-1".into(),
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
/// changes keep the version (DECISIONS.md).
pub const FORMAT_VERSION: u16 = 1;

/// Signature scheme identifiers for [`Witness::scheme`] (D39).
///
/// These are choir-local numbers rather than multicodec entries, unlike
/// [`ContentHash`]'s codec byte, and the difference is deliberate:
/// multicodec names a *key type*, while a verifier needs the
/// *construction* — which bytes were actually signed. A WebAuthn
/// signature covers `authenticator_data ‖ SHA-256(client_data_json)`
/// rather than the message, and no curve identifier says that.
pub mod scheme {
    /// Ed25519 over the signed bytes directly. The only scheme that
    /// existed before D39, which is why an absent [`super::Witness::scheme`]
    /// means this one.
    pub const ED25519: u16 = 1;
    /// WebAuthn ES256 (D39): ECDSA P-256 with SHA-256, over
    /// `authenticator_data ‖ SHA-256(client_data_json)`, where the
    /// challenge inside `client_data_json` is the entry's
    /// [`super::OpEntry::signing_hash`].
    pub const WEBAUTHN_ES256: u16 = 2;
}

/// A cosignature: a witness cosignature (unused until Phase 2, D16) or,
/// in [`OpEntry::author_sig`], the author's own. Present in the format
/// from the first persisted byte so adding witnessing never rewrites
/// history.
///
/// The scheme and WebAuthn fields are additive (`serde(default)` plus
/// `skip_serializing_if`), so a signature written before D39 serializes
/// to exactly the bytes it always did and every entry hash containing
/// one is unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Witness {
    /// Identifier of the witness key that produced [`Witness::signature`].
    pub key_id: String,
    /// Signature over the entry's content hash.
    pub signature: Vec<u8>,
    /// Which scheme produced [`Witness::signature`], from [`scheme`].
    ///
    /// Absent means [`scheme::ED25519`]: entries written before D39
    /// carry no tag, and giving them one would change their bytes and
    /// therefore their hash. An unrecognised value decodes fine on
    /// purpose — an old reader must be able to replay a log containing
    /// a scheme it cannot verify, so the refusal belongs at
    /// verification rather than at decode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheme: Option<u16>,
    /// WebAuthn authenticator data, the first half of what a
    /// [`scheme::WEBAUTHN_ES256`] signature covers. Absent for every
    /// other scheme.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authenticator_data: Option<Vec<u8>>,
    /// WebAuthn client data JSON, whose `challenge` member carries the
    /// [`OpEntry::signing_hash`] the human approved. Absent for every
    /// other scheme.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_data_json: Option<Vec<u8>>,
}

impl Witness {
    /// An ed25519 cosignature, the shape every caller before D39 wrote
    /// as a struct literal.
    pub fn ed25519(key_id: impl Into<String>, signature: Vec<u8>) -> Self {
        Self {
            key_id: key_id.into(),
            signature,
            scheme: None,
            authenticator_data: None,
            client_data_json: None,
        }
    }

    /// A WebAuthn ES256 cosignature (D39), carrying the two byte strings
    /// a verifier needs and cannot reconstruct: the authenticator data,
    /// and the client data JSON whose `challenge` binds the signature to
    /// one [`OpEntry::signing_hash`].
    pub fn webauthn_es256(
        key_id: impl Into<String>,
        signature: Vec<u8>,
        authenticator_data: Vec<u8>,
        client_data_json: Vec<u8>,
    ) -> Self {
        Self {
            key_id: key_id.into(),
            signature,
            scheme: Some(scheme::WEBAUTHN_ES256),
            authenticator_data: Some(authenticator_data),
            client_data_json: Some(client_data_json),
        }
    }

    /// The scheme this signature claims, resolving the pre-D39 absence
    /// to [`scheme::ED25519`]. Verifiers should match on this rather
    /// than on [`Witness::scheme`] directly, so the two spellings of
    /// ed25519 never diverge.
    pub fn scheme_id(&self) -> u16 {
        self.scheme.unwrap_or(scheme::ED25519)
    }
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
    /// Signature-covered attribution channel that submitted the op.
    ///
    /// Serialized as `workspace` because that field name is frozen into
    /// format v1 and therefore into every entry hash. The Rust name is
    /// deliberately accurate: workspace ids live in `ViewOp`, while this
    /// value identifies the collaboration channel an actor spoke on.
    #[serde(rename = "workspace")]
    pub channel: String,
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

    /// What the author signs: a hash over `(channel, payload)` only.
    ///
    /// The author asserts *what* they submitted, not *where* it landed —
    /// `seq`/`parent` are assigned by the sequencer after signing (and
    /// are covered by witnesses from Phase 2). Replaying a signed op at
    /// a different position is rejected by the CAS `prev` carried inside
    /// the payload, not by the signature.
    pub fn signing_hash(&self) -> ContentHash {
        signing_hash(&self.channel, &self.payload)
    }
}

/// Hash over an author's submission content — see
/// [`OpEntry::signing_hash`]. Standalone so clients can sign before the
/// sequencer has built the entry.
pub fn signing_hash(channel: &str, payload: &[u8]) -> ContentHash {
    // This tuple has no field names, so renaming the concept from the
    // overloaded `workspace` to `channel` changes no signed bytes.
    let canonical = serde_json::to_vec(&(channel, payload)).expect("tuple always serializes");
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

/// The log-backend seam (D16). Conformance suite: `tests/it/conformance.rs`,
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

    /// Makes every prior [`OpLog::append`] durable — survives power loss,
    /// not merely process death.
    ///
    /// Until this returns `Ok`, an appended entry may exist only in the
    /// OS page cache. That matters here beyond losing a tail: the
    /// `pre-receive` hook submits a ref op *before* git applies the ref,
    /// so an unsynced log can leave git holding a ref whose authorising
    /// op does not exist. Two sources of truth then disagree, and the
    /// next push fails its CAS against a view that never saw the update.
    ///
    /// The sequencer calls this once per batch rather than once per
    /// append, and only acknowledges submitters afterwards.
    ///
    /// # Errors
    ///
    /// Backend-specific [`LogError`] on storage failure. A failure here
    /// must be treated as "the batch is not durable", never as success.
    ///
    /// Default: a no-op, correct for backends that never outlive the
    /// process (see [`MemLog`]).
    fn sync(&mut self) -> Result<(), LogError> {
        Ok(())
    }
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

    /// Nothing to do: a `MemLog` never outlives its process, so there is
    /// no weaker state for `sync` to strengthen. The default would serve;
    /// it is spelled out because "in-memory logs cannot be made durable"
    /// is the reason, not an oversight.
    fn sync(&mut self) -> Result<(), LogError> {
        Ok(())
    }
}

/// Second implementation (seam rule: feature-poor is fine, broken is not):
/// JSON-lines file, append-only, rebuilt head on open.
pub struct FileLog {
    /// Buffered so a batch of appends costs one write syscall instead of
    /// one each. Correctness rests on [`OpLog::sync`]: nothing here is
    /// durable, or even visible to another reader of the file, until the
    /// buffer is flushed.
    file: std::io::BufWriter<std::fs::File>,
    /// Separate read handle, so [`OpLog::get`] can take `&self` without
    /// disturbing the writer.
    reader: std::sync::Mutex<std::fs::File>,
    /// Byte offset where each entry starts, one `u64` per entry.
    ///
    /// This replaces holding every [`OpEntry`] in memory. That cost some
    /// hundreds of bytes per op and never shrank, so a log grew in RAM
    /// with the repo's *lifetime* rather than with load — at the Phase-1
    /// target of 5 ops/s, ~432k entries a day, forever. Eight bytes per op
    /// instead, and `get` pays a seek and a parse for what is no longer
    /// resident.
    ///
    /// Rebuilt on open and never persisted, so it adds no on-disk format
    /// and needs no `format_version` or checkpoint record. `open` already
    /// scanned the whole file to rebuild `head`; this rides along.
    offsets: Vec<u64>,
    /// Total bytes handed to the writer, including what is still sitting
    /// in the `BufWriter`.
    write_pos: u64,
    /// Entries appended since the last successful flush. They are not yet
    /// readable from the file, so [`OpLog::get`] serves them from here.
    /// Bounded by the sequencer's batch size, which syncs once per batch.
    pending: std::collections::VecDeque<OpEntry>,
    head: Option<ContentHash>,
    /// Bytes of unterminated tail discarded by [`FileLog::open`]; see
    /// [`FileLog::torn_tail_bytes`]. Zero for a cleanly closed log.
    torn_tail_bytes: u64,
}

impl FileLog {
    /// Opens (creating if absent) the log file at `path` and replays it to
    /// rebuild the in-memory index and head.
    ///
    /// # Torn tails
    ///
    /// A power cut can leave the file ending in a record that was only
    /// partly written, or — under delayed allocation — in a run of NUL
    /// bytes. Any final record with no terminating newline is treated as
    /// such a torn write and truncated away, whether or not it happens to
    /// decode: a complete record whose newline never landed would
    /// otherwise be concatenated with the next append into one line that
    /// never parses again.
    ///
    /// Discarding it cannot lose an acknowledged op. The sequencer
    /// acknowledges only after [`OpLog::sync`] returns, and that call
    /// returns only once every byte before it is on the platter, so
    /// anything in an unterminated tail was never acknowledged to anyone.
    /// The truncation is reported by [`FileLog::torn_tail_bytes`] rather
    /// than performed silently.
    ///
    /// A decode failure in a *newline-terminated* record is not a torn
    /// write — it is damage to a record that was once written whole — and
    /// still fails as [`LogError::Corrupt`]. That includes a NUL-filled
    /// gap followed by further records: refusing to start is the right
    /// answer there, because the alternative is silently dropping ops from
    /// the middle of the log.
    ///
    /// # Errors
    ///
    /// Returns [`LogError::Io`] on filesystem failure and
    /// [`LogError::Corrupt`] when a terminated line fails to decode.
    pub fn open(path: &std::path::Path) -> Result<Self, LogError> {
        use std::io::{BufRead, BufReader};
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(path)
            .map_err(LogError::Io)?;
        // Replay rebuilds `head` and the offset index together. Reading by
        // bytes rather than `.lines()` is what makes the offsets available
        // at all: a line iterator does not say where it was.
        let mut offsets = Vec::new();
        let mut head = None;
        let mut write_pos = 0u64;
        let mut torn_tail_bytes = 0u64;
        let mut reader = BufReader::new(&file);
        let mut line = Vec::new();
        loop {
            line.clear();
            let n = reader.read_until(b'\n', &mut line).map_err(LogError::Io)?;
            if n == 0 {
                break;
            }
            let Some(body) = line.strip_suffix(b"\n") else {
                // No newline means `read_until` hit EOF mid-record: a torn
                // tail. `write_pos` is already the offset of its first
                // byte, which is where the file has to end.
                torn_tail_bytes = n as u64;
                break;
            };
            let entry: OpEntry =
                serde_json::from_slice(body).map_err(|e| LogError::Corrupt(e.to_string()))?;
            head = Some(entry.content_hash());
            offsets.push(write_pos);
            write_pos += n as u64;
        }
        drop(reader);
        if torn_tail_bytes > 0 {
            // Cut it off before the writer can append behind it. `sync_all`
            // rather than `sync_data` because it is the file's *length*
            // that has to survive here.
            file.set_len(write_pos).map_err(LogError::Io)?;
            file.sync_all().map_err(LogError::Io)?;
        }
        // The file's own bytes are synced by `sync`, but a fresh file (or a
        // just-truncated one) is only reachable through its directory
        // entry, and that is a separate write. Once per open, so the cost
        // does not appear on the submit path.
        sync_parent_dir(path)?;
        let read_handle = std::fs::File::open(path).map_err(LogError::Io)?;
        Ok(Self {
            file: std::io::BufWriter::new(file),
            reader: std::sync::Mutex::new(read_handle),
            offsets,
            write_pos,
            pending: std::collections::VecDeque::new(),
            head,
            torn_tail_bytes,
        })
    }

    /// Bytes of partly written tail that [`FileLog::open`] truncated away,
    /// or 0 if the log ended on a record boundary.
    ///
    /// Non-zero means this process started after an unclean stop. The
    /// discarded bytes were never acknowledged (see [`FileLog::open`]), so
    /// this is a fact worth reporting, not a fault — but a caller that
    /// never reports it turns a crash into a silent one.
    pub fn torn_tail_bytes(&self) -> u64 {
        self.torn_tail_bytes
    }
}

/// Fsyncs the directory holding `path`, so the file's name survives power
/// loss and not just its contents.
fn sync_parent_dir(path: &std::path::Path) -> Result<(), LogError> {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        // A bare filename lives in the process's working directory.
        _ => std::path::Path::new("."),
    };
    std::fs::File::open(parent)
        .and_then(|dir| dir.sync_all())
        .map_err(LogError::Io)
}

impl Drop for FileLog {
    /// Last-resort flush. The sequencer syncs per batch, so in normal
    /// operation this finds an empty buffer; it exists so a log dropped on
    /// an error path does not silently discard buffered entries. Errors
    /// are unreportable here, hence the `ok()` — durability is the
    /// sequencer's job via [`OpLog::sync`], not this.
    fn drop(&mut self) {
        use std::io::Write;
        self.file.flush().ok();
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
        self.offsets.push(self.write_pos);
        self.write_pos += line.len() as u64;
        // Held only until the next flush makes it readable from the file.
        self.pending.push_back(entry);
        self.head = Some(hash.clone());
        Ok(hash)
    }

    fn head(&self) -> Option<ContentHash> {
        self.head.clone()
    }

    fn len(&self) -> u64 {
        self.offsets.len() as u64
    }

    /// Reads one entry back: from the pending buffer if it has not been
    /// flushed yet, otherwise from the file at its recorded offset.
    ///
    /// Returns `None` for an out-of-range `seq` and also for a stored line
    /// that fails to decode or read. The trait signature has no way to say
    /// "present but unreadable", and inventing one is a wider change than
    /// this belongs in — but a corrupt log is a real condition, and `open`
    /// does report it as [`LogError::Corrupt`], so damage is caught when
    /// the log is next opened rather than never.
    fn get(&self, seq: u64) -> Option<OpEntry> {
        let len = self.offsets.len() as u64;
        if seq >= len {
            return None;
        }
        // Entries appended since the last flush are not in the file yet.
        let pending_start = len - self.pending.len() as u64;
        if seq >= pending_start {
            return self.pending.get((seq - pending_start) as usize).cloned();
        }

        let start = self.offsets[seq as usize];
        // The next entry's offset bounds this one; for the last entry the
        // bound is everything written so far.
        let end = self
            .offsets
            .get(seq as usize + 1)
            .copied()
            .unwrap_or(self.write_pos);
        let mut buf = vec![0u8; (end - start) as usize];

        use std::io::{Read, Seek, SeekFrom};
        let mut file = self.reader.lock().ok()?;
        file.seek(SeekFrom::Start(start)).ok()?;
        file.read_exact(&mut buf).ok()?;
        let body = buf.strip_suffix(b"\n").unwrap_or(&buf);
        serde_json::from_slice(body).ok()
    }

    /// Flush the buffer to the OS, then ask the OS to put it on the
    /// platter. Both halves are required and neither substitutes for the
    /// other: `flush` alone leaves the bytes in the page cache, and
    /// `sync_data` alone would sync a buffer that was never written.
    ///
    /// `sync_data` rather than `sync_all`: the file's length and contents
    /// must survive, its mtime need not, and skipping the metadata write
    /// is the cheaper half of an fsync.
    fn sync(&mut self) -> Result<(), LogError> {
        use std::io::Write;
        self.file.flush().map_err(LogError::Io)?;
        // Only now are these readable from the file, so only now may the
        // in-memory copies go. If `flush` failed they are still the only
        // copy and must be kept.
        self.pending.clear();
        self.file.get_ref().sync_data().map_err(LogError::Io)
    }
}
