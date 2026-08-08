//! L1 view/workspace model: typed operations over the op log and the
//! materialized repo state they fold into (plan.md L1, jj-style).
//!
//! The op log ([`choir_oplog`]) stores opaque payloads; this crate gives
//! them a versioned schema ([`ViewOp`]) and a deterministic fold
//! ([`View::materialize`]). Because the fold is pure, the view at *any*
//! point in history is reconstructible by replaying a prefix
//! ([`View::at`]) — that is the whole undo model, no reverse patches.
//!
//! Commits are content-addressed objects in the chunk store
//! ([`Commit`]/[`TreeEntry`]). A conflicted merge is a *valid* commit
//! ([`TreeEntry::Conflict`]): work continues on top of it and the
//! resolution is a later commit, never a blocked workspace (plan.md
//! first-class conflicts, D9).
//!
//! One-way-door rules (plan.md §E): every persisted shape here
//! ([`ViewOp`], [`Commit`]) carries `format_version`, and all identifiers
//! are self-describing [`ContentHash`] envelopes.
//!
//! # Examples
//!
//! ```
//! use choir_oplog::{MemLog, OpLog};
//! use choir_view::{View, ViewOp, OpKind, append_op};
//! use choir_hash::ContentHash;
//!
//! let mut log = MemLog::new();
//! let commit_id = ContentHash::blake3(b"pretend commit");
//! append_op(&mut log, "agent-1", ViewOp::new(OpKind::SetWorkspaceHead {
//!     workspace: "agent-1".into(),
//!     commit: commit_id.clone(),
//!     prev: None,
//! })).unwrap();
//! let view = View::materialize(&log).unwrap();
//! assert_eq!(view.workspaces.get("agent-1"), Some(&commit_id));
//! ```

use std::collections::BTreeMap;

use choir_hash::ContentHash;
use choir_oplog::{LogError, OpEntry, OpLog};
use choir_store::{ChunkStore, StoreError};
use serde::{Deserialize, Serialize};

/// Current view-op and commit wire-format version. Bump on any
/// incompatible change; additive changes keep the version (plan.md §E).
pub const FORMAT_VERSION: u16 = 1;

/// A typed operation carried in [`OpEntry::payload`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewOp {
    /// Wire-format version this op was written with; see [`FORMAT_VERSION`].
    pub format_version: u16,
    /// What the operation does to the view.
    pub kind: OpKind,
}

impl ViewOp {
    /// Wraps `kind` at the current [`FORMAT_VERSION`].
    pub fn new(kind: OpKind) -> Self {
        Self {
            format_version: FORMAT_VERSION,
            kind,
        }
    }

    /// Serializes into an [`OpEntry::payload`].
    pub fn to_payload(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("ViewOp is always serializable")
    }

    /// Decodes an [`OpEntry::payload`].
    ///
    /// # Errors
    ///
    /// Returns [`ViewError::Decode`] when the bytes are not a valid op.
    pub fn from_payload(payload: &[u8]) -> Result<Self, ViewError> {
        serde_json::from_slice(payload).map_err(|e| ViewError::Decode(e.to_string()))
    }
}

/// The view mutations. Head-moving ops carry `prev` (compare-and-set
/// against the current view) so a stale writer is rejected instead of
/// silently clobbering a concurrent advance — the same discipline the op
/// log itself applies to its head.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OpKind {
    /// Point `workspace` at `commit`; `prev` must equal its current head
    /// (`None` = workspace must not exist yet).
    SetWorkspaceHead {
        /// Workspace being moved.
        workspace: String,
        /// New head commit id.
        commit: ContentHash,
        /// Expected current head (CAS), `None` to create.
        prev: Option<ContentHash>,
    },
    /// Point named ref `name` at `commit` under the same CAS rule.
    SetRef {
        /// Ref name (e.g. `"main"`).
        name: String,
        /// New target commit id.
        commit: ContentHash,
        /// Expected current target (CAS), `None` to create.
        prev: Option<ContentHash>,
    },
    /// Remove `workspace` from the view (its commits stay in the store).
    DeleteWorkspace {
        /// Workspace being removed.
        workspace: String,
    },
}

/// Failure modes of view folding and commit storage.
#[derive(Debug)]
pub enum ViewError {
    /// CAS failure: `prev` did not match the current head/target.
    StaleHead {
        /// The workspace or ref name that was targeted.
        target: String,
        /// What the op expected the current value to be.
        expected: Option<ContentHash>,
        /// What the view actually held.
        actual: Option<ContentHash>,
    },
    /// Payload or stored commit bytes failed to decode.
    Decode(String),
    /// Underlying op-log failure.
    Log(LogError),
    /// Underlying chunk-store failure.
    Store(StoreError),
}

/// One entry in a commit's tree: a path maps to file content or to an
/// unresolved conflict (first-class: committing this is valid, D9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TreeEntry {
    /// Regular file; `blob` is a [`choir_store`] manifest address.
    File {
        /// Manifest address of the file content.
        blob: ContentHash,
    },
    /// Unresolved merge conflict for this path, all three sides kept.
    Conflict {
        /// Base-side manifest address (`None` when the file is new on
        /// both sides).
        base: Option<ContentHash>,
        /// Left-side manifest address.
        left: ContentHash,
        /// Right-side manifest address.
        right: ContentHash,
    },
}

/// A content-addressed commit: parents, a path→entry tree, metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Commit {
    /// Wire-format version; see [`FORMAT_VERSION`].
    pub format_version: u16,
    /// Parent commit ids (0 = root, 2+ = merge).
    pub parents: Vec<ContentHash>,
    /// Path → entry, sorted (BTreeMap) so serialization is canonical.
    pub tree: BTreeMap<String, TreeEntry>,
    /// Author identity string (key-backed from L8 onward).
    pub author: String,
    /// Commit message.
    pub message: String,
}

impl Commit {
    /// Whether any tree entry is an unresolved [`TreeEntry::Conflict`].
    pub fn is_conflicted(&self) -> bool {
        self.tree
            .values()
            .any(|e| matches!(e, TreeEntry::Conflict { .. }))
    }

    /// Stores this commit in `store` and returns its content address.
    ///
    /// # Errors
    ///
    /// Propagates [`ViewError::Store`] from the underlying store.
    pub fn put(&self, store: &mut dyn ChunkStore) -> Result<ContentHash, ViewError> {
        let bytes = serde_json::to_vec(self).expect("Commit is always serializable");
        store.put(&bytes).map_err(ViewError::Store)
    }

    /// Loads the commit at `id` from `store`, verifying its address.
    ///
    /// # Errors
    ///
    /// Returns [`ViewError::Store`] on lookup/verification failure and
    /// [`ViewError::Decode`] when the bytes are not a commit.
    pub fn get(store: &dyn ChunkStore, id: &ContentHash) -> Result<Self, ViewError> {
        let bytes = store.get(id).map_err(ViewError::Store)?;
        serde_json::from_slice(&bytes).map_err(|e| ViewError::Decode(e.to_string()))
    }
}

/// The materialized repo state: where every workspace and ref points.
///
/// A `View` is only ever produced by folding ops, so two replicas that
/// replay the same log prefix hold identical views.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct View {
    /// Workspace name → head commit id.
    pub workspaces: BTreeMap<String, ContentHash>,
    /// Ref name → target commit id.
    pub refs: BTreeMap<String, ContentHash>,
}

impl View {
    /// Applies one op, enforcing its CAS precondition. A rejected op
    /// leaves the view unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`ViewError::StaleHead`] when `prev` does not match.
    pub fn apply(&mut self, op: &ViewOp) -> Result<(), ViewError> {
        match &op.kind {
            OpKind::SetWorkspaceHead {
                workspace,
                commit,
                prev,
            } => {
                let actual = self.workspaces.get(workspace);
                if actual != prev.as_ref() {
                    return Err(ViewError::StaleHead {
                        target: workspace.clone(),
                        expected: prev.clone(),
                        actual: actual.cloned(),
                    });
                }
                self.workspaces.insert(workspace.clone(), commit.clone());
            }
            OpKind::SetRef { name, commit, prev } => {
                let actual = self.refs.get(name);
                if actual != prev.as_ref() {
                    return Err(ViewError::StaleHead {
                        target: name.clone(),
                        expected: prev.clone(),
                        actual: actual.cloned(),
                    });
                }
                self.refs.insert(name.clone(), commit.clone());
            }
            OpKind::DeleteWorkspace { workspace } => {
                self.workspaces.remove(workspace);
            }
        }
        Ok(())
    }

    /// Folds the whole log into a view.
    ///
    /// # Errors
    ///
    /// Propagates decode and CAS failures; a log accepted through
    /// [`append_op`] always replays cleanly.
    pub fn materialize(log: &dyn OpLog) -> Result<Self, ViewError> {
        Self::at(log, log.len())
    }

    /// Folds only the first `upto` entries — the view as it was after op
    /// `upto - 1`. This is undo/time-travel: restore by writing a new op
    /// that sets heads back to this view (history itself is append-only).
    ///
    /// # Errors
    ///
    /// Same failure modes as [`View::materialize`].
    pub fn at(log: &dyn OpLog, upto: u64) -> Result<Self, ViewError> {
        let mut view = View::default();
        for seq in 0..upto.min(log.len()) {
            let entry = log.get(seq).expect("seq < len");
            view.apply(&ViewOp::from_payload(&entry.payload)?)?;
        }
        Ok(view)
    }
}

/// Validates `op` against the log's current view, then appends it as a
/// new [`OpEntry`] — the single-writer submit path in miniature. The CAS
/// check happens *before* the append, so the log never contains an op
/// that fails to replay.
///
/// # Errors
///
/// Returns [`ViewError::StaleHead`] when the op's precondition fails and
/// [`ViewError::Log`] when the backend append fails.
pub fn append_op(
    log: &mut dyn OpLog,
    submitter: &str,
    op: ViewOp,
) -> Result<ContentHash, ViewError> {
    let mut view = View::materialize(log)?;
    view.apply(&op)?;
    let entry = OpEntry {
        format_version: choir_oplog::FORMAT_VERSION,
        parent: log.head(),
        seq: log.len(),
        workspace: submitter.to_string(),
        payload: op.to_payload(),
        witnesses: Vec::new(),
            author_sig: None,
    };
    log.append(entry).map_err(ViewError::Log)
}
