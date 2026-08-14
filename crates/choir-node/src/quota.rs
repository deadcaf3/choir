//! Per-user quotas (D37): how much of the node one credential may hold.
//!
//! D33 gave the node a record of who did what ([`crate::limits::Access`])
//! and a bound on how *often* anyone did it
//! ([`crate::limits::RateLimiter`]). Neither bounds how *large* one
//! request may be or how much durable state one user may accumulate, and
//! a rate limit does not imply either: one push a minute is still an
//! unbounded pack, and one workspace a minute is still unbounded disk.
//!
//! Two ceilings, sharing only the identity they key on — the
//! authenticated username string, exactly as
//! [`crate::limits::RateLimiter::check`] keys on it. Nothing here reads
//! the auth file or asks any user store whether a name exists: the string
//! arrives from the request's credentials and is used as an opaque key.
//!
//! # Where the push ceiling is enforced, and why it is there
//!
//! [`read_bounded`] runs on the request thread *before* `git
//! http-backend` is spawned, which is the whole design rather than an
//! implementation detail. Git applies no ref until the `pre-receive` hook
//! exits zero, so a size check inside that hook would already have
//! submitted ops for the refs it did reach — ops for refs git will never
//! create — and would have to drive the compensating retraction pass
//! `Node::create_repo` documents. Refusing before the CGI starts means no
//! hook ran, no op was submitted, and the retraction path is not entered
//! at all. The bound is on the transfer; the sequencer never learns the
//! push was attempted.
//!
//! The cost is stated rather than hidden: an over-limit body is drained
//! to a sink before the refusal is written, so the client always reads a
//! `413` instead of a broken connection, and so `tiny_http`'s own reader
//! does not try to swallow the remainder in one allocation on drop. The
//! bytes still cross the network. What the ceiling buys is that they
//! never reach memory beyond the ceiling, never reach `git`, and never
//! reach the log.
//!
//! # Where the workspace ceiling gets its count, and why that survives a
//! restart
//!
//! [`WorkspaceTally`] is not a new persisted file. It is a projection
//! folded out of the op log during the replay the platform already
//! performs at startup, keyed on the attribution channel each
//! workspace-creating entry already carries in its signature-covered
//! `channel` field. `PHASE0.md` left this item out of D33 calling a
//! surviving tally "a persisted-state question"; it is one, and the
//! answer is that the persisted state already exists and needed a reader
//! rather than a writer. A restart rebuilds the tally from the same log
//! that rebuilds the view, with no new format, no new file and no second
//! durability barrier.
//!
//! # Examples
//!
//! ```
//! use choir_node::quota::{channel_for, Quotas};
//!
//! // The tally's key is derived from the authenticated username and
//! // nothing else.
//! assert_eq!(channel_for("alice"), "git/alice");
//!
//! // Both ceilings are off unless the operator sets them.
//! let none = Quotas::default();
//! assert!(none.push_bytes.is_none() && none.workspaces.is_none());
//! assert!(!none.is_active());
//! ```

use std::collections::BTreeMap;
use std::io::Read;
use std::num::{NonZeroU32, NonZeroU64};

use choir_oplog::OpEntry;
use choir_view::{OpKind, ViewOp};

/// The attribution channel a request-authenticated user's operations are
/// submitted under.
///
/// This must agree with the `attribution` that `crate::provision` stamps
/// on a workspace operation, because that string is what
/// [`WorkspaceTally`] counts. The agreement is pinned end to end rather
/// than by a shared constant: `tests/it/quotas.rs` creates a workspace as
/// a named user through the real API and asserts the tally attributes it
/// to that user, so a change to either spelling fails a test rather than
/// silently zeroing everyone's count.
#[must_use]
pub fn channel_for(user: &str) -> String {
    format!("git/{user}")
}

/// The operator's per-user ceilings. Either may be left unset, and unset
/// means unlimited — the same shape as the D33 rate-limit flags.
#[derive(Debug, Clone, Copy, Default)]
pub struct Quotas {
    /// Maximum bytes in the body of one git smart-HTTP request.
    ///
    /// A per-request ceiling rather than a budget over time, because the
    /// resource being bounded is the transfer and the unpack it feeds,
    /// both of which are paid per request. That is also why this half
    /// needs no persisted state, and why only the workspace half forced
    /// a register row.
    pub push_bytes: Option<NonZeroU64>,
    /// Maximum workspaces one user may hold at once.
    pub workspaces: Option<NonZeroU32>,
}

impl Quotas {
    /// Whether either ceiling is set at all.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.push_bytes.is_some() || self.workspaces.is_some()
    }
}

/// What [`read_bounded`] found in a request body.
#[derive(Debug)]
pub enum Body {
    /// The whole body, which was within the ceiling (or there was none).
    Complete(Vec<u8>),
    /// The body was larger than the ceiling. Nothing was handed on; the
    /// remainder was drained so the client can read the refusal.
    OverLimit {
        /// The ceiling that was exceeded, in bytes.
        limit: u64,
        /// How large the body actually turned out to be, counted through
        /// the drain so the refusal can name the real number rather than
        /// "more than the limit".
        size: u64,
    },
}

/// Reads a request body, stopping at `limit` bytes.
///
/// `None` reads the whole body, which is what a node with no push ceiling
/// does. With a ceiling, at most `limit + 1` bytes are ever held: the one
/// extra byte is how "exactly at the ceiling" is told from "over it".
///
/// # Errors
///
/// Propagates a read failure from the socket. A failure *while draining*
/// an over-limit body is deliberately not propagated — the client has
/// already lost, and answering it beats reporting how it hung up.
pub fn read_bounded(reader: &mut dyn Read, limit: Option<NonZeroU64>) -> std::io::Result<Body> {
    let Some(limit) = limit.map(NonZeroU64::get) else {
        let mut body = Vec::new();
        reader.read_to_end(&mut body)?;
        return Ok(Body::Complete(body));
    };
    let mut body = Vec::new();
    // `take` bounds the allocation as well as the read: a lying
    // Content-Length cannot make this reserve more than the ceiling.
    reader.take(limit + 1).read_to_end(&mut body)?;
    if body.len() as u64 <= limit {
        return Ok(Body::Complete(body));
    }
    // Drain rather than drop. Dropping `tiny_http`'s content-length
    // reader makes *it* swallow the remainder, in a single allocation the
    // size of what is left, which is the allocation this ceiling exists
    // to prevent. Copying to a sink uses a fixed buffer and counts what
    // it discards, so the refusal can name the real size.
    let drained = std::io::copy(reader, &mut std::io::sink()).unwrap_or(0);
    Ok(Body::OverLimit {
        limit,
        size: body.len() as u64 + drained,
    })
}

/// Which channel holds which workspace, folded from the op log.
///
/// # Why a map rather than a counter
///
/// A per-channel counter incremented and decremented alongside the view
/// is a second copy of the same fact, and the two can drift — the failure
/// this repository has hit before, where a guard passed while enforcing
/// nothing. Holding only "workspace → the channel that created it" makes
/// the count derived, so it cannot disagree with itself, and makes the
/// stronger invariant checkable: this map's keys are exactly
/// `View::workspaces`' keys, because the five operations that move one
/// move the other.
#[derive(Debug, Default)]
pub struct WorkspaceTally {
    owner_of: BTreeMap<String, String>,
}

impl WorkspaceTally {
    /// Folds one admitted entry. Called on the writer thread for a live
    /// operation and in the replay loop at startup — the same call in
    /// both places, which is what makes a restart reproduce the tally
    /// rather than approximate it.
    pub fn observe(&mut self, entry: &OpEntry, op: &ViewOp) {
        match &op.kind {
            // The three that put a workspace into the view. `or_insert`
            // rather than `insert`: a later checkpoint or a legacy head
            // move does not transfer ownership to whoever moved it.
            OpKind::SetWorkspaceHead { workspace, .. }
            | OpKind::CreateChange { workspace, .. }
            | OpKind::CheckpointChange { workspace, .. } => {
                self.owner_of
                    .entry(workspace.clone())
                    .or_insert_with(|| entry.channel.clone());
            }
            // The two that take one out.
            OpKind::DeleteWorkspace { workspace }
            | OpKind::ArchiveChange { workspace, .. } => {
                self.owner_of.remove(workspace);
            }
            _ => {}
        }
    }

    /// How many workspaces `channel` currently holds.
    #[must_use]
    pub fn held_by(&self, channel: &str) -> usize {
        self.owner_of.values().filter(|owner| *owner == channel).count()
    }

    /// Every workspace the tally is tracking, for the test that pins it
    /// against `View::workspaces`.
    pub fn workspaces(&self) -> impl Iterator<Item = &str> {
        self.owner_of.keys().map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use choir_hash::ContentHash;

    fn entry(channel: &str) -> OpEntry {
        OpEntry {
            format_version: 1,
            parent: None,
            seq: 0,
            channel: channel.to_string(),
            payload: Vec::new(),
            witnesses: Vec::new(),
            author_sig: None,
        }
    }

    fn head(workspace: &str) -> ViewOp {
        ViewOp::new(OpKind::SetWorkspaceHead {
            workspace: workspace.to_string(),
            commit: ContentHash::blake3(workspace.as_bytes()),
            prev: None,
        })
    }

    #[test]
    fn a_workspace_is_counted_against_the_channel_that_created_it() {
        let mut tally = WorkspaceTally::default();
        tally.observe(&entry("git/alice"), &head("o/r/one"));
        tally.observe(&entry("git/alice"), &head("o/r/two"));
        tally.observe(&entry("git/bob"), &head("o/r/three"));
        assert_eq!(tally.held_by("git/alice"), 2);
        assert_eq!(tally.held_by("git/bob"), 1);
        assert_eq!(tally.held_by("git/carol"), 0);
    }

    #[test]
    fn a_later_head_move_does_not_transfer_the_workspace() {
        // Otherwise a user could park their workspaces on someone else's
        // count by touching them, and the ceiling would bound nobody.
        let mut tally = WorkspaceTally::default();
        tally.observe(&entry("git/alice"), &head("o/r/one"));
        tally.observe(&entry("git/bob"), &head("o/r/one"));
        assert_eq!(tally.held_by("git/alice"), 1);
        assert_eq!(tally.held_by("git/bob"), 0);
    }

    #[test]
    fn deleting_a_workspace_gives_the_allowance_back() {
        let mut tally = WorkspaceTally::default();
        tally.observe(&entry("git/alice"), &head("o/r/one"));
        assert_eq!(tally.held_by("git/alice"), 1);
        tally.observe(
            &entry("git/alice"),
            &ViewOp::new(OpKind::DeleteWorkspace {
                workspace: "o/r/one".to_string(),
            }),
        );
        assert_eq!(tally.held_by("git/alice"), 0);
        assert_eq!(tally.workspaces().count(), 0);
    }

    #[test]
    fn a_body_at_the_ceiling_passes_and_one_byte_over_does_not() {
        let limit = NonZeroU64::new(8);
        match read_bounded(&mut &b"12345678"[..], limit).expect("reads") {
            Body::Complete(body) => assert_eq!(body.len(), 8),
            other => panic!("exactly at the ceiling is not over it: {other:?}"),
        }
        match read_bounded(&mut &b"123456789"[..], limit).expect("reads") {
            Body::OverLimit { limit, size } => {
                assert_eq!(limit, 8);
                // Counted through the drain, so the refusal names the
                // real size rather than "more than eight".
                assert_eq!(size, 9);
            }
            other => panic!("nine bytes is over a ceiling of eight: {other:?}"),
        }
    }

    #[test]
    fn no_ceiling_reads_the_whole_body() {
        match read_bounded(&mut &b"12345678"[..], None).expect("reads") {
            Body::Complete(body) => assert_eq!(body.len(), 8),
            other => panic!("an unset ceiling limits nothing: {other:?}"),
        }
    }
}
