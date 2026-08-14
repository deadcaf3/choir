//! Resolution memory keyed by the conflict triple (Pijul item 1,
//! rerere-shaped; DECISIONS.md D15: metadata on existing shapes, never a new
//! merge substrate).
//!
//! A conflict's identity already exists in the model: the
//! `(base, left, right)` manifest-address triple of a
//! [`TreeEntry::Conflict`]. The memory key is the hash of that entry's
//! canonical serialization — the exact bytes invariant 3 freezes — so
//! keying adds no second identity scheme. Resolutions are found through
//! item A's `Commit::resolves` links: a head-moving op whose commit
//! links a conflicted commit contributes one remembered resolution per
//! path that was conflicted there and is a plain file in the resolver.
//!
//! A recalled resolution is a **candidate, never a landing**: the queue
//! replays it into the train, where it runs the same CI verdict as any
//! other member. It deliberately skips the strategy safety check —
//! that check polices *strategies* (a resolution must stay inside what
//! the change proposed), while a genuine conflict resolution edits
//! beyond both sides by construction and was already committed as a
//! value by its author. What the replay skips is only the re-derivation
//! of a resolution the log already holds.

use std::collections::BTreeMap;

use choir_hash::ContentHash;
use choir_oplog::OpLog;
use choir_store::{get_blob, put_blob, ChunkStore, ChunkerParams, MemStore};
use choir_view::{Commit, OpKind, TreeEntry, ViewOp};

/// Previously seen conflict resolutions, keyed by the conflict triple.
#[derive(Debug, Default)]
pub struct ResolutionMemory {
    /// Triple key (hex of the hashed canonical `TreeEntry::Conflict`)
    /// → the resolved file content.
    map: BTreeMap<String, String>,
}

impl ResolutionMemory {
    /// An empty memory: every lookup misses, the queue behaves as before.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of remembered resolutions.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether nothing has been remembered.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// The conflict's identity: the hash of the canonical serialization
    /// of the `(base, left, right)` triple, spelled as the
    /// [`TreeEntry::Conflict`] those addresses came from. Reusing the
    /// persisted shape's bytes means the key inherits invariant 3's
    /// canonicalization for free and cannot drift from the store's own
    /// identity for the same conflict.
    ///
    /// Contents are addressed with default chunker params; a resolution
    /// recorded under different params keys differently and simply
    /// misses — the memory is an optimization, a miss re-conflicts.
    pub fn key(base: &str, left: &str, right: &str) -> String {
        let mut scratch = MemStore::new();
        let mut addr = |text: &str| {
            put_blob(&mut scratch, text.as_bytes(), ChunkerParams::default())
                .expect("MemStore writes cannot fail")
        };
        let entry = TreeEntry::Conflict {
            base: Some(addr(base)),
            left: addr(left),
            right: addr(right),
        };
        let bytes = serde_json::to_vec(&entry).expect("TreeEntry always serializes");
        ContentHash::blake3(&bytes).to_hex()
    }

    /// Builds the memory by walking `log` for head-moving ops whose
    /// commit carries a `resolves` link, and reading both sides from
    /// `store`. Anything the store cannot supply is skipped — an
    /// incomplete memory only means fewer recalls.
    pub fn from_log(log: &dyn OpLog, store: &dyn ChunkStore) -> Self {
        let mut map = BTreeMap::new();
        for seq in 0..log.len() {
            let Some(entry) = log.get(seq) else { continue };
            let Ok(op) = ViewOp::from_payload(&entry.payload) else {
                continue;
            };
            let commit_id = match &op.kind {
                OpKind::SetWorkspaceHead { commit, .. } | OpKind::SetRef { commit, .. } => commit,
                _ => continue,
            };
            let Ok(resolver) = Commit::get(store, commit_id) else {
                continue;
            };
            let Some(conflict_id) = &resolver.resolves else {
                continue;
            };
            let Ok(conflicted) = Commit::get(store, conflict_id) else {
                continue;
            };
            for (path, entry) in &conflicted.tree {
                let TreeEntry::Conflict { .. } = entry else {
                    continue;
                };
                let Some(TreeEntry::File { blob }) = resolver.tree.get(path) else {
                    continue;
                };
                let Ok(bytes) = get_blob(store, blob) else {
                    continue;
                };
                let Ok(text) = String::from_utf8(bytes) else {
                    continue;
                };
                let key_bytes =
                    serde_json::to_vec(entry).expect("TreeEntry always serializes");
                map.insert(ContentHash::blake3(&key_bytes).to_hex(), text);
            }
        }
        Self { map }
    }

    /// The remembered resolution for this triple, if any.
    pub fn recall(&self, base: &str, left: &str, right: &str) -> Option<&str> {
        self.map
            .get(&Self::key(base, left, right))
            .map(String::as_str)
    }
}
