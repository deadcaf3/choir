//! Stable change identity across rebase: a change is
//! identified by the hash of its position-independent content — the
//! normalized diff from [`choir_merge::normalized_diff`] — so the same
//! logical edit authored against different bases (before and after the
//! train rewrote the tip under it) carries the same identity. The queue
//! uses it to recognize a resubmission of an already-landed change and
//! refuse it instead of re-merging or duplicating it.

use choir_hash::ContentHash;

use crate::Change;

/// The change's stable identity: hex digest of its normalized diff.
///
/// Position-independent by construction — hunk offsets and context are
/// stripped before hashing — and content-sensitive: any differing added
/// or removed line is a different change.
pub fn change_identity(change: &Change) -> String {
    let normalized = choir_merge::normalized_diff(&change.base, &change.proposed);
    ContentHash::blake3(normalized.as_bytes()).to_hex()
}
