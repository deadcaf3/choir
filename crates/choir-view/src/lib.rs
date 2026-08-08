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
    /// Open review `id` on commit `target`, fanning out to `reviewers`
    /// (additive variant, added for review fan-out; wire-format
    /// unchanged). `id` must not already exist.
    ///
    /// An **empty** `reviewers` list opens the review *unassigned*: the
    /// requester is not naming their own reviewers, and an
    /// [`OpKind::AssignReviewers`] op fills the list in later. An
    /// unassigned review is never `complete()` and never `approved()`,
    /// so "ask nobody" cannot read as a pass (D24 layer 5).
    RequestReview {
        /// Caller-chosen review id (unique per log).
        id: String,
        /// The commit under review.
        target: ContentHash,
        /// Actor names the review fans out to; empty = unassigned.
        reviewers: Vec<String>,
        /// The ref this review proposes to land on, in the view's
        /// namespaced form `<repo>:<refname>` (e.g.
        /// `"choir/choir.git:refs/heads/main"`). `None` = unbound: a
        /// review of a commit that names no destination.
        ///
        /// Additive field, and the worked example of invariant 1 for an
        /// *enum variant*: `default` + `skip_serializing_if` means logs
        /// written before this field decode as `None` **and** re-serialize
        /// byte-identically, so their entry hashes do not move.
        ///
        /// This is what per-ref policy conditions on. Without it there is
        /// no way to say "reviews landing on `main` are privilege-bearing"
        /// — a review named a commit, and a commit belongs to no branch
        /// (D24 layer 5; D23 blast-radius gating).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target_ref: Option<String>,
    },
    /// Fill in the reviewer list of an unassigned review (additive
    /// variant, wire-format unchanged). Assign-once: the review must
    /// exist with an empty list, and the new list must be non-empty.
    ///
    /// This is the view-level half of D24 layer 5 — "the requester does
    /// not choose who reviews them". *Who* may assign is admission
    /// policy (L2), not view semantics: the daemon accepts this op only
    /// from its own key, and picks from an operator-curated pool.
    AssignReviewers {
        /// The unassigned review being filled in.
        id: String,
        /// Actor names the review now fans out to (non-empty).
        reviewers: Vec<String>,
    },
    /// Record `reviewer`'s verdict on review `id` (additive variant).
    /// Only listed reviewers may post; re-posting overwrites the
    /// reviewer's own earlier verdict (re-review after changes).
    ///
    /// `reviewer` is payload data and therefore covered by the author
    /// signature; binding it to the submitting key is admission policy
    /// (L2), not view semantics.
    PostVerdict {
        /// The review being answered.
        id: String,
        /// The responding reviewer (must be in the review's list).
        reviewer: String,
        /// The verdict.
        verdict: Verdict,
        /// Free-text rationale (may be empty).
        note: String,
    },
    /// Remove named ref `name` under the same CAS rule (additive
    /// variant, added for git branch deletion; wire-format unchanged).
    DeleteRef {
        /// Ref being removed.
        name: String,
        /// Expected current target (CAS).
        prev: Option<ContentHash>,
    },
    /// Attach an intent/provenance record — task spec, plan, rationale
    /// — to `subject` (D22 substrate; additive variant, wire-format
    /// unchanged). The latest record per `(subject, kind)` wins in the
    /// view; the full history stays in the log.
    ///
    /// `subject` is deliberately not bound to the submitting channel —
    /// a shared living spec is written by many agents. Trusting *who*
    /// wrote a record means reading the log entry's author.
    RecordProvenance {
        /// What the record is about: a workspace name, or any agreed
        /// channel (e.g. a repo's shared spec).
        subject: String,
        /// Record type, e.g. `"task-spec"` or `"plan"`.
        kind: String,
        /// The record text (stored as-is; an empty body is a valid,
        /// visible "withdrawn" state, not a deletion).
        body: String,
    },
}

/// A reviewer's answer to a review request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    /// The change may land.
    Approve,
    /// The change needs work before landing.
    RequestChanges,
}

/// Materialized state of one review: what is under review, who was
/// asked, who has answered what.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReviewState {
    /// The commit under review.
    pub target: Option<ContentHash>,
    /// Actors the review fanned out to.
    pub reviewers: Vec<String>,
    /// reviewer → (verdict, note); absent = not answered yet.
    pub verdicts: BTreeMap<String, (Verdict, String)>,
    /// The ref the review proposes to land on (`<repo>:<refname>`), or
    /// `None` for a review that named no destination. Policy reads this
    /// to decide whether a review is privilege-bearing.
    pub target_ref: Option<String>,
}

impl ReviewState {
    /// Whether every listed reviewer has answered. An unassigned review
    /// (no reviewers yet) is never complete — vacuous truth must not
    /// turn "asked nobody" into a finished review.
    #[must_use]
    pub fn complete(&self) -> bool {
        !self.reviewers.is_empty()
            && self.reviewers.iter().all(|r| self.verdicts.contains_key(r))
    }

    /// Whether the review is complete with no `RequestChanges`.
    #[must_use]
    pub fn approved(&self) -> bool {
        self.complete()
            && self.verdicts.values().all(|(v, _)| *v == Verdict::Approve)
    }
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
    /// Review-op precondition failure (duplicate id, unknown review,
    /// or a reviewer not on the review's list).
    Review(String),
    /// Provenance-record precondition failure (empty subject or kind).
    Provenance(String),
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
    /// Review id → review state (fan-out and verdicts).
    pub reviews: BTreeMap<String, ReviewState>,
    /// Subject → record kind → latest body (D22 provenance records).
    pub provenance: BTreeMap<String, BTreeMap<String, String>>,
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
            OpKind::RequestReview {
                id,
                target,
                reviewers,
                target_ref,
            } => {
                if self.reviews.contains_key(id) {
                    return Err(ViewError::Review(format!("review {id} already exists")));
                }
                self.reviews.insert(
                    id.clone(),
                    ReviewState {
                        target: Some(target.clone()),
                        reviewers: reviewers.clone(),
                        verdicts: BTreeMap::new(),
                        target_ref: target_ref.clone(),
                    },
                );
            }
            OpKind::AssignReviewers { id, reviewers } => {
                if reviewers.is_empty() {
                    return Err(ViewError::Review(
                        "assignment must name at least one reviewer".to_string(),
                    ));
                }
                let review = self
                    .reviews
                    .get_mut(id)
                    .ok_or_else(|| ViewError::Review(format!("no such review {id}")))?;
                if !review.reviewers.is_empty() {
                    return Err(ViewError::Review(format!(
                        "review {id} is already assigned"
                    )));
                }
                review.reviewers = reviewers.clone();
            }
            OpKind::PostVerdict {
                id,
                reviewer,
                verdict,
                note,
            } => {
                let review = self
                    .reviews
                    .get_mut(id)
                    .ok_or_else(|| ViewError::Review(format!("no such review {id}")))?;
                if !review.reviewers.iter().any(|r| r == reviewer) {
                    return Err(ViewError::Review(format!(
                        "{reviewer} is not a reviewer of {id}"
                    )));
                }
                review
                    .verdicts
                    .insert(reviewer.clone(), (*verdict, note.clone()));
            }
            OpKind::RecordProvenance { subject, kind, body } => {
                if subject.is_empty() || kind.is_empty() {
                    return Err(ViewError::Provenance(
                        "provenance subject and kind must be non-empty".to_string(),
                    ));
                }
                self.provenance
                    .entry(subject.clone())
                    .or_default()
                    .insert(kind.clone(), body.clone());
            }
            OpKind::DeleteRef { name, prev } => {
                let actual = self.refs.get(name);
                if actual != prev.as_ref() {
                    return Err(ViewError::StaleHead {
                        target: name.clone(),
                        expected: prev.clone(),
                        actual: actual.cloned(),
                    });
                }
                self.refs.remove(name);
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
