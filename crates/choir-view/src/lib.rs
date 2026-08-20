//! L1 view/workspace model: typed operations over the op log and the
//! materialized repo state they fold into (DECISIONS.md jj-style).
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
//! resolution is a later commit, never a blocked workspace (DECISIONS.md
//! first-class conflicts, D9).
//!
//! One-way-door rules (DECISIONS.md): every persisted shape here
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
use choir_oplog::{LogError, OpEntry, OpLog, Witness};
use choir_store::{ChunkStore, StoreError};
use serde::{Deserialize, Serialize};

/// Current view-op and commit wire-format version. Bump on any
/// incompatible change; additive changes keep the version (DECISIONS.md).
pub const FORMAT_VERSION: u16 = 1;

/// Where and when an op is admissible: the author's own statement of
/// which log they are submitting into and which head they observed.
///
/// This is an admission precondition, exactly like [`OpKind`]'s `prev`,
/// and it lives in the payload for the same reason `prev` does — the
/// payload is what the author signs. A signature over `(channel,
/// payload)` is otherwise position-independent, log-independent and
/// occurrence-independent, so a captured op replays onto any node that
/// trusts the key, and replays again on the node it came from as soon
/// as CAS state returns to what it expected (ABA). `prev` cannot close
/// that: it asks whether the state matches, not whether the op has run.
///
/// A head hash can occur at exactly one position in exactly one chain,
/// which is what makes it a usable freshness token without a clock: the
/// house rule is that elapsed time is not a thing this system measures.
///
/// The admission rule is in `choir-node`'s policy, not in [`View`]: the
/// view is pure state and knows nothing about nodes or log windows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpScope {
    /// Actor id of the node whose log this op was signed for.
    pub node: ContentHash,
    /// A log head the author had observed when they signed. `None` says
    /// the author read an empty log — admissible only while the node has
    /// evicted nothing, which is the span its duplicate index still
    /// covers in full.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<ContentHash>,
}

/// Which path authored an op, when it was not the default author-signed
/// submission (D41).
///
/// `None` on [`ViewOp::provenance`] is the default class: an actor
/// built, signed, and submitted the op under its own key. The variants
/// label the ops the node signs *on behalf of* a git pusher, whose key
/// never touches the payload — a materially different provenance that
/// the channel prefix (`key/`, `git/`) previously carried only by
/// convention. The label sits inside the signed payload, and the node's
/// admission policy refuses any labeled op not signed by the node's own
/// key, so the class can neither be claimed by an ordinary author nor
/// stripped by whoever relays the bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Provenance {
    /// A git push whose push certificate verified: the channel names
    /// the pusher's own key, but the node signed the op.
    PushCertified,
    /// A git push with no verified certificate: the channel names only
    /// the transport user the push arrived as.
    PushTransport,
}

/// A typed operation carried in [`OpEntry::payload`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewOp {
    /// Wire-format version this op was written with; see [`FORMAT_VERSION`].
    pub format_version: u16,
    /// What the operation does to the view.
    pub kind: OpKind,
    /// The log and head this op was signed for, when the author bound it
    /// to one. Additive (`default` + `skip_serializing_if`), so ops
    /// written before scopes existed decode as `None` and re-serialize
    /// byte-identically — invariant 1, and the reason adding a replay
    /// defence is not a log migration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<OpScope>,
    /// How this op was authored, when not the default author-signed
    /// class; see [`Provenance`]. Additive under the same rule as
    /// `scope`, so every existing op decodes as `None` and re-serializes
    /// byte-identically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,
    /// Explicit change dependencies: content hashes of
    /// the changes this op declares it builds on. Declared-only — the
    /// platform never infers dependencies from file overlap; inference
    /// is a separate decision. Additive under the same rule as `scope`
    /// (an empty list is not serialized), and because it sits inside
    /// the payload it is covered by the author's `(channel, payload)`
    /// signature (invariant 4) with no change to the signing scheme:
    /// ops written before the field existed re-serialize
    /// byte-identically, so their signatures still verify.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends: Vec<ContentHash>,
}

impl ViewOp {
    /// Wraps `kind` at the current [`FORMAT_VERSION`], unscoped, in the
    /// default author-signed provenance class.
    pub fn new(kind: OpKind) -> Self {
        Self {
            format_version: FORMAT_VERSION,
            kind,
            scope: None,
            provenance: None,
            depends: Vec::new(),
        }
    }

    /// Declares the changes this op builds on. The list rides inside
    /// the signed payload, so a relay can neither strip nor extend it.
    #[must_use]
    pub fn with_depends(mut self, depends: Vec<ContentHash>) -> Self {
        self.depends = depends;
        self
    }

    /// Labels this op with a non-default provenance class. Only the
    /// node's push path does this; admission refuses the label under
    /// any other signer, so calling it from an ordinary author buys a
    /// rejection, not a classification.
    #[must_use]
    pub fn with_provenance(mut self, provenance: Provenance) -> Self {
        self.provenance = Some(provenance);
        self
    }

    /// Binds this op to one log and one observed head. The scope is
    /// inside the payload, so it is covered by the author's signature
    /// and cannot be stripped or rewritten by whoever relays the bytes.
    #[must_use]
    pub fn in_scope(mut self, node: ContentHash, head: Option<ContentHash>) -> Self {
        self.scope = Some(OpScope { node, head });
        self
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

/// Owner-signed authorization carried into a node-authored physical
/// workspace creation operation.
///
/// This is separate from [`ViewOp`]: submitting the authorization to the
/// raw operation endpoint cannot create a directory or a change. The
/// workspace endpoint first verifies and materializes the exact binding,
/// then the node wraps it in [`OpKind::CreateChange`] under its own key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateAuthorization {
    /// Wire-format version; see [`FORMAT_VERSION`].
    pub format_version: u16,
    /// Stable logical contribution id.
    pub id: String,
    /// Owner channel that must match the signature attribution.
    pub owner: String,
    /// Workspace being created.
    pub workspace: String,
    /// Exact immutable starting revision.
    pub base_revision: ContentHash,
    /// Owner-scoped retry identity.
    pub idempotency_key: String,
    /// Directory prefixes the owner declares this change works within;
    /// empty means the whole tree.
    ///
    /// Covered by the owner's signature on purpose. The cone lands in
    /// the log as part of an op the *node* authors, so if it were not
    /// signed here the node could attach a scope its owner never
    /// declared — and a declaration nobody signed is worth no more than
    /// the node-side config this was meant to replace. Additive under
    /// invariant 1: an empty cone is not serialized, so every
    /// authorization signed before this field existed still verifies
    /// byte-for-byte.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cone: Vec<String>,
}

impl CreateAuthorization {
    /// Creates an authorization at the current wire-format version.
    pub fn new(
        id: String,
        owner: String,
        workspace: String,
        base_revision: ContentHash,
        idempotency_key: String,
    ) -> Self {
        Self {
            format_version: FORMAT_VERSION,
            id,
            owner,
            workspace,
            base_revision,
            idempotency_key,
            cone: Vec::new(),
        }
    }

    /// The same authorization, scoped to `cone`.
    ///
    /// A builder rather than a sixth parameter on [`Self::new`], so the
    /// unscoped call sites -- which are most of them, and every one
    /// written before cones existed -- keep reading as the plain thing
    /// they are.
    #[must_use]
    pub fn with_cone(mut self, cone: Vec<String>) -> Self {
        self.cone = cone;
        self
    }

    /// Canonical bytes covered by the owner's submission signature.
    pub fn to_payload(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("CreateAuthorization is always serializable")
    }

    /// Decodes signed authorization bytes.
    pub fn from_payload(payload: &[u8]) -> Result<Self, ViewError> {
        serde_json::from_slice(payload).map_err(|error| ViewError::Decode(error.to_string()))
    }
}

/// Owner-signed authorization carried into a node-authored physical
/// workspace archive operation.
///
/// This is separate from [`ViewOp`]: submitting the authorization to the
/// raw operation endpoint cannot detach a workspace. The archive endpoint
/// first moves the filesystem, then the node wraps this proof in
/// [`OpKind::ArchiveChange`] and submits that operation under its own key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveAuthorization {
    /// Wire-format version; see [`FORMAT_VERSION`].
    pub format_version: u16,
    /// Stable logical contribution id.
    pub id: String,
    /// Currently active workspace.
    pub workspace: String,
    /// Expected current change/workspace revision.
    pub prev_revision: ContentHash,
}

impl ArchiveAuthorization {
    /// Creates an authorization at the current wire-format version.
    pub fn new(id: String, workspace: String, prev_revision: ContentHash) -> Self {
        Self {
            format_version: FORMAT_VERSION,
            id,
            workspace,
            prev_revision,
        }
    }

    /// Canonical bytes covered by the owner's submission signature.
    pub fn to_payload(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("ArchiveAuthorization is always serializable")
    }

    /// Decodes signed authorization bytes.
    pub fn from_payload(payload: &[u8]) -> Result<Self, ViewError> {
        serde_json::from_slice(payload).map_err(|error| ViewError::Decode(error.to_string()))
    }
}

/// Why a [`OpKind::Submit`] was allowed to land (D43).
///
/// The gate that admits a landing runs at apply time inside the node's
/// submission policy, so without this the log records *that* a ref moved
/// and never *why*. A later additive field cannot repair that: entries
/// written before it stay blank, and that window is permanently
/// unauditable. So it ships with the first `Submit` ever accepted.
///
/// The record is **checked, not trusted**. Everything here except the
/// ACL's own grants is derivable from the fold, and [`View::validate`]
/// rederives it and refuses a mismatch — the [`OpKind::RecordRefSnapshot`]
/// discipline, for the same reason: a claim every replayer verifies is
/// worth more than one only the admitting node could have checked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Authorization {
    /// Wire-format version; see [`FORMAT_VERSION`].
    pub format_version: u16,
    /// The rule that admitted the landing.
    pub basis: Basis,
    /// Actor ids whose standing approvals the basis rested on, in the
    /// review's verdict order.
    ///
    /// **Actor ids, never channel names.** An id is `hash(pubkey)` and is
    /// the trust root (D9); a channel name is mutable, and an audit
    /// record that reads differently later than it read when written is
    /// not an audit record. Resolved through the log's own
    /// [`OpKind::BindKey`] records by [`View::bound_actor_at`], so the
    /// join is replayable — and an approval whose channel the log binds
    /// to no key, or to more than one, cannot land through this op at
    /// all.
    ///
    /// **What an id here proves, exactly.** It is the key the log bound
    /// to the approving channel *as of the verdict's own position*
    /// ([`VerdictState::at`]), not proof that this key cast the verdict:
    /// the fold is handed only the op, never the entry, so
    /// [`ReviewState::verdicts`] is keyed by channel and no review can
    /// record a signer. Resolving at the verdict's position rather than
    /// the landing's is what keeps the claim true across a key rotation
    /// (D44) — the replacement key never saw the review. That is also
    /// why freezing the id here is worth doing: [`KeyBinding::channel`]
    /// is the one field a later re-binding may change, so the answer is
    /// only stable once written down.
    ///
    /// **Empty is a distinct value from absent.** A basis that requires
    /// no approvals records `[]`, and a reader can tell that from a log
    /// predating the field, because such a log holds no `Submit`.
    pub approvers: Vec<ContentHash>,
}

impl Authorization {
    /// Creates an authorization at the current wire-format version.
    #[must_use]
    pub fn new(basis: Basis, approvers: Vec<ContentHash>) -> Self {
        Self {
            format_version: FORMAT_VERSION,
            basis,
            approvers,
        }
    }
}

/// The rule that admitted a landing, as the evaluation that admitted it
/// computed it (D43).
///
/// Three variants because there are three ways a protected-ref landing
/// is currently allowed, and an approver list alone distinguishes none of
/// them: under D42 a landing can be authorized with **zero** approvals,
/// so an empty list would equally mean "an owner landed it" and "the
/// policy required nobody".
///
/// There is deliberately **no variant for an ungated ref.** A `Submit`
/// whose ref is unprotected, or whose node is not running the review
/// gate, is refused rather than recorded — see [`OpKind::Submit`]. The
/// record therefore means exactly one thing, and cannot be used to dress
/// an unexamined ref move as an authorized one.
///
/// Where a variant names a principal it names it in the namespace the
/// rule actually read. The ownership rule reads the ACL's subject
/// column, which holds usernames and has no actor id to offer, so
/// `owner` is a username. Recording the rule's real input beats
/// recording a prettier identity the rule never saw.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Basis {
    /// `owner` holds `own` on the repository and performed the landing
    /// themselves (D42). Performing the landing is assent, which is why
    /// no approval is recorded.
    OwnerLanded {
        /// The ACL subject the node resolved the acting identity to.
        owner: String,
    },
    /// `owner` holds `own` on the repository and approved a review
    /// naming this exact `(ref, commit)` pair (D42).
    OwnerApproved {
        /// The approving owner's reviewer channel, which is also its ACL
        /// subject. The fold checks it against the review's standing
        /// approvals; it cannot check the grant, which lives in a file.
        owner: String,
    },
    /// No owner is granted on the repository, so the weight rule applied
    /// and was met.
    ApprovalWeight {
        /// The threshold in force when the landing was admitted.
        ///
        /// This is the part of the "the rule itself is not pinned"
        /// residual that could be closed cheaply: for this basis an
        /// auditor no longer has to reconstruct the threshold from
        /// operator-side config history. The ACL grants behind the two
        /// owner variants stay unpinned, and still need a tripwire
        /// rather than taste.
        required: u32,
        /// The review's approval weight at admission.
        met: u32,
    },
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
    /// Settle review `id` and drop its bulk (additive variant,
    /// wire-format unchanged).
    ///
    /// A review's verdicts, notes and reviewer list are the part that
    /// grows without bound; the landing gate reads only
    /// `(target_ref, target, approved)`. Archiving keeps that triple and
    /// discards the rest, so retention stops being an authorization
    /// decision — an approval never silently expires.
    ///
    /// **Archiving is freezing, not deleting.** `PostVerdict` overwrites
    /// a reviewer's earlier verdict, so approval is *not* monotonic and a
    /// review can go approved then not. Once the verdicts are gone that
    /// transition can no longer be computed, so an archived review
    /// accepts no further verdicts — and says so, rather than reporting
    /// itself absent.
    ///
    /// *Who* may archive is admission policy (L2), like
    /// [`OpKind::AssignReviewers`]: the daemon accepts it only from its
    /// own key, because otherwise archiving would be a way to erase a
    /// `RequestChanges` you did not like.
    ArchiveReview {
        /// The review being settled.
        id: String,
        /// Settle an **incomplete** review as not approved, rather than
        /// refusing because it never reached an outcome.
        ///
        /// An unanswered review is exactly the kind that accumulates, and
        /// it is incomplete by definition — so a pruner that can only
        /// archive complete reviews reclaims the ones least likely to
        /// pile up. Lapsing is the answer, and it deliberately invents
        /// **no new outcome**: a review nobody answered never got
        /// approval, so `Archived { approved: false }` is the whole truth.
        /// The landing gate is unchanged, and there is no new persisted
        /// enum to version.
        ///
        /// **When** is not a view question. A pure fold of the log has no
        /// clock, so the view cannot decide "abandoned"; the node decides
        /// on wall time and this op records the decision, exactly as the
        /// reviewer draw is decided node-side and recorded by
        /// [`OpKind::AssignReviewers`]. Replay reproduces the lapse from
        /// the op and never from re-running a timer.
        ///
        /// Additive: absent in a payload written before this field
        /// existed, which decodes as `false` — the previous strict
        /// behaviour exactly.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        lapsed: bool,
    },
    /// Invalidate one reviewer's approval after a policy or trust finding
    /// (additive variant, wire-format unchanged).
    ///
    /// The operation is append-only: it never moves a ref back and never
    /// erases the original verdict. A review with any slash visibly needs
    /// re-review, and its affected operator no longer contributes approval
    /// weight to future protected-ref authorization.
    ///
    /// Live rows verify that `reviewer` currently has an approval. Archived
    /// rows deliberately no longer retain reviewer detail, so admission
    /// accepts this operation only from the node key; the signed operation
    /// is the compact authority attestation and replay input.
    SlashApproval {
        /// The review whose approval is invalidated.
        id: String,
        /// Reviewer channel whose approval is invalidated.
        reviewer: String,
        /// Operator-visible reason for requiring re-review (non-empty).
        reason: String,
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
    /// Append one comment to the discussion on review `id` (D38).
    ///
    /// This is the half D34 deferred: a review page could show what was
    /// proposed and what was decided, and had nowhere to put the
    /// conversation that produced the decision. Discussion is a persisted
    /// operation like any other, so it is sequenced, replayable and
    /// append-only rather than a mutable side table.
    ///
    /// Three properties live in the fold, and each is load-bearing:
    ///
    /// 1. **Order is the log's order.** The thread is a `Vec` appended in
    ///    fold order, not a map keyed by id, because a total order over a
    ///    conversation is the one thing a single-writer sequencer offers
    ///    that a mutable comment table cannot.
    /// 2. **`comment` is the replay defence.** `seq` and `parent` are
    ///    assigned after signing (invariant 4), so nothing positional is
    ///    covered by the author's signature; the payload carries its own
    ///    identity instead and the fold refuses an id the review already
    ///    holds. That is the CAS `prev` in the only shape a comment can
    ///    take, since a comment moves no head. The ABA hole `prev` leaves
    ///    for refs does not open here: an id is never released, because
    ///    an archived review accepts no comments at all.
    /// 3. **Nothing is ever edited.** There is no edit and no delete;
    ///    a correction is a later comment. Removing one is a question
    ///    about persisted history rather than about presentation, and it
    ///    needs its own decision row (D38's tripwire).
    ///
    /// `author` is payload data and therefore covered by the author
    /// signature; binding it to the submitting channel is admission
    /// policy (L2), exactly as for [`OpKind::PostVerdict`]'s `reviewer`.
    /// Without that binding the log's author attribution and the view's
    /// comment attribution could disagree, which on a discussion surface
    /// means words in somebody else's name.
    PostComment {
        /// The review being discussed.
        id: String,
        /// Caller-chosen comment id, unique within that review. It is the
        /// author's retry identity — resubmitting the same comment is
        /// refused rather than duplicated — and the anchor a later reply
        /// or reaction would name.
        comment: String,
        /// The channel making the statement (must be the submitting
        /// channel, enforced at admission).
        author: String,
        /// The comment text (non-empty).
        body: String,
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
    /// Create stable logical change `id` and bind its first active
    /// workspace at an immutable base revision (additive variant;
    /// existing operation bytes are unchanged).
    ///
    /// The change id survives later checkpoints and workspace archival.
    /// `idempotency_key` is scoped to `owner`; the view rejects a second
    /// change using the same pair so a lost create response cannot fork
    /// one logical request into two changes.
    CreateChange {
        /// Stable logical contribution id.
        id: String,
        /// Channel allowed to checkpoint the change (enforced by L2
        /// admission because signatures are not part of the pure view).
        owner: String,
        /// Exclusively bound active workspace.
        workspace: String,
        /// Exact immutable revision the workspace starts from.
        base_revision: ContentHash,
        /// Retry identity, unique within `owner`.
        idempotency_key: String,
        /// Owner signature over the matching [`CreateAuthorization`].
        /// Absent only on operations accepted before creation
        /// authorization was introduced.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        owner_sig: Option<Witness>,
        /// Directory prefixes this change declares it works within, in
        /// git's cone spelling (`"services/api"`). Empty = the whole
        /// tree, which is what every change written before this field
        /// existed decodes to and is exactly the previous behaviour
        /// (invariant 1).
        ///
        /// **Declared, not enforced here.** The fold does not police
        /// which paths a commit touches; a cone is a statement about
        /// intent that the node can serve a matching partial clone from
        /// and that [`conflicts_for_cone`] narrows a conflict report
        /// against. Making it a hard boundary would need path
        /// enforcement in the merge layer, which is a separate decision
        /// and a much larger one.
        ///
        /// It sits in the signed payload rather than in node-side
        /// config for the reason D49 puts a check there: a replayer can
        /// then reproduce what the change said it was scoped to, and an
        /// operator cannot rewrite it after the fact.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        cone: Vec<String>,
    },
    /// Publish an immutable revision of an existing change and advance
    /// its bound workspace under compare-and-set.
    CheckpointChange {
        /// Stable logical contribution id.
        id: String,
        /// The change's currently bound workspace.
        workspace: String,
        /// Newly published immutable revision.
        revision: ContentHash,
        /// Expected current change/workspace revision.
        prev_revision: ContentHash,
    },
    /// Archive a stable change's active workspace under revision CAS.
    /// The revision remains addressable on the change after the mutable
    /// filesystem surface is detached.
    ArchiveChange {
        /// Stable logical contribution id.
        id: String,
        /// The change's currently bound workspace.
        workspace: String,
        /// Expected current change/workspace revision.
        prev_revision: ContentHash,
        /// Owner channel that signed the matching
        /// [`ArchiveAuthorization`].
        owner: String,
        /// Signature over the canonical authorization bytes. Admission
        /// verifies it before this node-authored operation may land.
        owner_sig: Witness,
    },
    /// Bind actor key `key` to the durable operator identity `operator`
    /// (additive variant, wire-format unchanged).
    ///
    /// Until now an operator existed only as a prefix convention on a
    /// channel name ([`reviewer_operator`]) plus a line in a mutable,
    /// operator-owned keys file. Both are rewritable without a trace, so
    /// "these two keys are the same operator" was an assertion no replay
    /// could reproduce. This op puts the assertion *in* the log, where it
    /// is sequenced, append-only, and recoverable at any prefix via
    /// [`View::at`].
    ///
    /// Two properties make it load-bearing, and both live in the fold:
    ///
    /// 1. **First binding wins the clock.** [`KeyBinding::bound_at`] is
    ///    the sequence of the op that *first* bound the key, and never
    ///    moves again — so re-binding to adjust a channel cannot reset
    ///    accumulated standing.
    /// 2. **One key, one operator, for the life of the key.** Re-binding
    ///    a key to a different operator is refused, so whatever position
    ///    a key accumulates cannot be handed to somebody else.
    ///
    /// **What this does not establish.** *Who* may author a binding is
    /// admission policy (L2), exactly as for [`OpKind::AssignReviewers`]
    /// and [`OpKind::SlashApproval`]. With no admission rule wired, any
    /// key can bind any other key under any operator name, and `operator`
    /// is a self-chosen label rather than a verified identity. The fold
    /// proves *sequence and immutability*; it never proves authority.
    BindKey {
        /// The durable operator identity the key is bound to. Non-empty,
        /// and free of `/` so it cannot alias a `operator/agent` channel
        /// prefix and read as two different operators.
        operator: String,
        /// The actor key being bound: the content address of its public
        /// key, as `choir_identity::ActorKey::actor_id` produces it.
        key: ContentHash,
        /// The channel name the operator asserts this key speaks as, when
        /// it asserts one — the sequenced form of the keys file's
        /// `<name> <hex>` line.
        ///
        /// Constrained so the two available operator answers cannot
        /// disagree: [`reviewer_operator`] of this channel must equal
        /// `operator`, i.e. the channel is `operator` itself or
        /// `operator/<agent>`. Without that rule a durable record reading
        /// "bob" and a channel prefix reading "alice" would both be live,
        /// which is worse than a single wrong answer.
        ///
        /// Additive per invariant 1: a payload written before this field
        /// existed decodes as `None` **and** re-serializes byte-identically,
        /// so entry hashes do not move.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channel: Option<String>,
    },
    /// Withdraw `key`'s binding (additive variant, wire-format unchanged).
    ///
    /// Append-only and terminal. The binding row stays, so the operator
    /// attribution for everything the key already did survives; the key
    /// itself can never be bound again. Allowing a rebind would make
    /// revocation a formality — revoke, rebind, carry on — so the remedy
    /// is a fresh key, the same shape as [`OpKind::SlashApproval`]'s
    /// "open a new review".
    ///
    /// This is the record a revocation cascade replays over, and the slot
    /// a later vouch or bond withdrawal hangs off. It moves no ref and
    /// undoes no landed change; like a slash, it constrains what comes
    /// next rather than rewriting what came before.
    ///
    /// *Who* may revoke is admission policy (L2), as for
    /// [`OpKind::BindKey`].
    RevokeKey {
        /// The bound key whose binding is withdrawn.
        key: ContentHash,
        /// Operator-visible reason for the withdrawal (non-empty).
        reason: String,
    },
    /// Record a signed attestation of the **complete** ref-state at one
    /// log position (D25; additive variant, wire-format unchanged).
    ///
    /// This is the object that closes the gap the attestation section of
    /// the design notes states: every existing check proves the chain a
    /// reader *was shown* is consistent and authentically authored, none
    /// proves another reader was shown the same chain. A snapshot is the
    /// unit two readers compare, the record that makes a mirror bundle
    /// checkable against the log, the checkpoint truncation needs, and —
    /// when D16's gate opens — the thing a witness cosigns. One object,
    /// because those are one question: "what was the whole ref-state at
    /// seq N?".
    ///
    /// The fold *verifies* the claim rather than storing it: admission
    /// compares [`RefSnapshot::refs`] against the view's refs and
    /// [`RefSnapshot::at_seq`] against the fold position, so a snapshot
    /// that lies about the log it sits in is refused by every replayer,
    /// not just by the node that admitted it. The chain rule
    /// (`prev_snapshot` must name the latest admitted snapshot) makes
    /// replaying an old snapshot a chain violation even where the ref
    /// map recurs — the ABA shape D26 measured, answered here the same
    /// way `prev` answers it for refs.
    ///
    /// *Who* may record one is admission policy (L2), as for
    /// [`OpKind::AssignReviewers`]: the daemon accepts it only from its
    /// own key. The view enforces truth, not authority.
    RecordRefSnapshot {
        /// The snapshot; its detached file projection is these exact
        /// canonical bytes, never a second schema.
        snapshot: RefSnapshot,
    },
    /// Record that `viewer` read review `id` (a read receipt; additive
    /// variant, wire-format unchanged; the backlog).
    ///
    /// The receipt is what lets an author distinguish "reviewed and
    /// ignored" from "nobody has looked yet". The fact recorded is the
    /// *first* read per viewer -- when this review first got that
    /// reader's attention -- so the fold refuses a viewer the review
    /// already holds, and that refusal doubles as the replay defence,
    /// exactly as a comment id does for [`OpKind::PostComment`]: nothing
    /// positional is signed, so the payload's (review, viewer) pair is
    /// its own retry identity.
    ///
    /// A receipt moves no ref, changes no verdict and carries no
    /// authorization weight; it is bulk in [`OpKind::ArchiveReview`]'s
    /// sense and is dropped with the rest of it.
    ///
    /// `viewer` is payload data covered by the author signature; binding
    /// it to the submitting channel is admission policy (L2), exactly as
    /// for [`OpKind::PostVerdict`]'s `reviewer`.
    ViewedReview {
        /// The review that was read.
        id: String,
        /// The channel that read it (must be the submitting channel,
        /// enforced at admission).
        viewer: String,
    },
    /// Record one automated check's outcome on a commit (D49; additive
    /// variant, wire-format unchanged).
    ///
    /// **The node does not run the check.** Executing workflows is
    /// containers, secrets, caches and artifacts — the largest surface
    /// on the platform and the least differentiated part of it, since
    /// every forge already has one. What no forge has is a check result
    /// that is *ordered against the ref it attests* and replayable by
    /// someone who trusts none of the parties. So this op carries the
    /// verdict and nothing else: any runner, or a person, reports by
    /// signing one.
    ///
    /// That inversion is what makes it stronger than a status field on a
    /// merge gate. A field is mutable and is read at merge time, so
    /// "was this green when it landed" decays into "is it green now". A
    /// signed op is append-only and sits at a known sequence, so the
    /// question stays decidable forever, and `choir log --verify`
    /// already recomputes the hash and checks the signature.
    ///
    /// Re-reporting overwrites the reporter's own earlier result for the
    /// same `(subject, name)`, exactly as [`OpKind::PostVerdict`] lets a
    /// reviewer re-review. A check that flaps is a check that flaps; the
    /// log keeps every report and the view keeps the latest.
    ///
    /// `reporter` is payload data covered by the author signature.
    /// Binding it to the submitting channel is admission policy (L2),
    /// the same split as `reviewer` and `viewer` above.
    RecordCheck {
        /// The commit the check ran against.
        subject: ContentHash,
        /// Check name, e.g. `"ci/build"` (non-empty).
        name: String,
        /// What the check found.
        status: CheckStatus,
        /// Where a human can read the run: a URL, a run id, or empty.
        evidence: String,
        /// The channel reporting it (must be the submitting channel,
        /// enforced at admission).
        reporter: String,
        /// The ref this check's subject is proposed to land on, in the
        /// view's namespaced form `<repo>:<refname>`. `None` = unbound.
        ///
        /// Present for the same reason [`OpKind::RequestReview`] carries
        /// one, and it is load-bearing twice: per-ref policy conditions
        /// on it, and it is the only thing that lets the ACL narrow a
        /// check to a repository. A commit id names no repository, so a
        /// check without this field is visible to node-wide readers
        /// only — correct, and useless to the repository it belongs to.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target_ref: Option<String>,
    },
    /// Land a reviewed commit on a ref **and record why it was allowed**
    /// (D43; additive variant, wire-format unchanged).
    ///
    /// [`OpKind::SetRef`] can already move a ref, and the landing gate
    /// already runs before it. What the log does not keep is the reason:
    /// the gate is evaluated at apply time inside the node's submission
    /// policy against files that are not in the log, so a `SetRef` on a
    /// protected ref records that a merge happened and nothing about what
    /// permitted it. This op is the same move with the answer attached.
    ///
    /// Three properties live in the fold:
    ///
    /// 1. **The named review must actually say what the landing claims.**
    ///    It must exist, be live, and name exactly this `(name, commit)`
    ///    pair. An authorization citing a review about something else is
    ///    the failure mode worth refusing.
    /// 2. **The authorization is rederived, never trusted.** Approvers,
    ///    approval weight, and the approving owner's standing verdict are
    ///    all computed from the view and compared; a mismatch is a
    ///    rejection. The ACL grant behind an owner basis is the one part
    ///    no replayer can check, because it lives in an operator file.
    /// 3. **It is the sole reason the record outlives the review.**
    ///    [`OpKind::ArchiveReview`] discards verdicts, so after archiving
    ///    the entry bytes are the only surviving answer to "who approved
    ///    this". Replay still verifies, because validation runs at this
    ///    op's own position — before the archive that comes later.
    ///
    /// **This op narrows the gate on purpose.** The node's weight check
    /// takes the maximum over *every* review naming `(ref, commit)`; a
    /// `Submit` names one review and is judged on that one, so two
    /// half-approved reviews of the same commit cannot pool weight
    /// through this path.
    ///
    /// **It is refused on a ref no rule gates.** A node not running the
    /// review gate, or a ref outside its protected set, takes an
    /// ordinary [`OpKind::SetRef`]. Admitting a `Submit` there would
    /// mint an authorization record for a decision nothing examined,
    /// which is worse than no record — so [`Basis`] has no variant for
    /// it and admission says so.
    ///
    /// *Who* may submit one is admission policy (L2). Unlike
    /// [`OpKind::AssignReviewers`] this one is **author-signed**: the
    /// basis is a node determination, but pressing merge is a human act,
    /// and the signature is the only place the log can keep who wanted
    /// it. Admission rederives the authorization and refuses any
    /// mismatch, so a client that writes its own basis buys a rejection
    /// rather than a claim.
    Submit {
        /// The review being landed.
        review: String,
        /// Ref name, in the view's namespaced form (`<repo>:<refname>`).
        name: String,
        /// The commit to land, which must be the review's target.
        commit: ContentHash,
        /// Expected current target (CAS), `None` to create.
        prev: Option<ContentHash>,
        /// Why this was allowed.
        authorization: Authorization,
    },
}

/// A signed attestation that the complete ref-state at log position
/// [`RefSnapshot::at_seq`] was exactly [`RefSnapshot::refs`] (D25).
///
/// Carried inside [`OpKind::RecordRefSnapshot`], and written detached —
/// byte-identical — beside mirror bundles so a bundle is checkable
/// against something other than itself.
///
/// The chain pointer lives **inside** this struct rather than being
/// inherited from `OpEntry.parent` because `seq`/`parent` are assigned
/// after signing: a snapshot copied out of the log would otherwise carry
/// no chain of its own, and an old one could be served forever. Same
/// discipline as the CAS `prev` inside a payload.
///
/// Ref names are map keys, and a git ref name may legally carry `"` and
/// any non-ASCII byte (git forbids control bytes, space, `~^:?*[\`, but
/// not quotes or high bytes) — and an API-submitted name is not bound by
/// git's grammar at all. Canonical serialization of hostile keys is
/// therefore pinned by this struct's golden vector and property tests,
/// not assumed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefSnapshot {
    /// Wire-format version; see [`FORMAT_VERSION`].
    pub format_version: u16,
    /// The complete ref map at `at_seq`, in the view's namespaced form
    /// (`<repo>:<refname>`). Every ref the view holds — a selection
    /// would let an equivocating node attest only the refs it is honest
    /// about.
    pub refs: BTreeMap<String, ContentHash>,
    /// The fold position the state was read at: the number of ops
    /// applied before this one. Admission requires it to equal the
    /// view's position, so it is also the sequence this op itself
    /// occupies in the log.
    pub at_seq: u64,
    /// Content address ([`RefSnapshot::id`]) of the previous snapshot on
    /// this log; `None` only for a log's first snapshot. Additive per
    /// invariant 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev_snapshot: Option<ContentHash>,
}

impl RefSnapshot {
    /// The canonical bytes: what is hashed, what the author signs over
    /// (inside the op payload), and what the detached file contains.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("RefSnapshot is always serializable")
    }

    /// Content address of the canonical bytes — the identity the next
    /// snapshot's `prev_snapshot` names.
    #[must_use]
    pub fn id(&self) -> ContentHash {
        ContentHash::blake3(&self.canonical_bytes())
    }
}

/// One actor key's durable binding to an operator identity, as the fold
/// sees it after replaying [`OpKind::BindKey`] and [`OpKind::RevokeKey`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyBinding {
    /// The operator identity this key belongs to. Fixed at first binding.
    pub operator: String,
    /// The channel the operator asserts this key speaks as, if any. The
    /// only field a later re-binding may change.
    pub channel: Option<String>,
    /// Log sequence of the op that *first* bound this key.
    ///
    /// This is the age primitive: it is assigned once and never moves, so
    /// it orders keys by standing in a way replay reproduces exactly. It
    /// counts **sequenced ops, not elapsed time** — see
    /// [`View::ops_since_binding`] for what that can and cannot answer.
    pub bound_at: u64,
    /// The withdrawal record, once revoked; never cleared.
    pub revoked: Option<Revocation>,
}

impl KeyBinding {
    /// Whether this binding has been withdrawn. Authorization must ask;
    /// attribution must not (see [`View::operator_of`]).
    #[must_use]
    pub fn is_revoked(&self) -> bool {
        self.revoked.is_some()
    }
}

/// The append-only record that a binding was withdrawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revocation {
    /// Log sequence of the [`OpKind::RevokeKey`] op that withdrew it.
    pub at: u64,
    /// Operator-visible reason for the withdrawal.
    pub reason: String,
}

/// A reviewer's answer to a review request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    /// The change may land.
    Approve,
    /// The change needs work before landing.
    RequestChanges,
}

/// What an automated check found about a commit (D49).
///
/// Three states and not two. "Not finished" is a real answer and the one
/// a caller most needs to distinguish, because the action it implies —
/// wait — differs from both pass and fail. Collapsing it into failure
/// makes every in-flight check look like a broken build; collapsing it
/// into success is worse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckStatus {
    /// The check ran and was satisfied.
    Passed,
    /// The check ran and was not satisfied.
    Failed,
    /// The check has started and has not reported an outcome.
    Running,
}

impl CheckStatus {
    /// The wire spelling, which is also what the CLI accepts.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            CheckStatus::Passed => "passed",
            CheckStatus::Failed => "failed",
            CheckStatus::Running => "running",
        }
    }

    /// Parses the CLI spelling, or `None` for anything else.
    ///
    /// Deliberately not a `FromStr` impl taking arbitrary case: a check
    /// reported as `"PASSED"` by a runner that upcased its output should
    /// be refused loudly rather than accepted into a signed op, because
    /// the op is what a later audit reads.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "passed" => Some(CheckStatus::Passed),
            "failed" => Some(CheckStatus::Failed),
            "running" => Some(CheckStatus::Running),
            _ => None,
        }
    }
}

/// The latest report for one `(subject, check name)` pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckState {
    /// What the check found.
    pub status: CheckStatus,
    /// Where a human can read the run; may be empty.
    pub evidence: String,
    /// Channel that reported it.
    pub reporter: String,
    /// Ref the subject is proposed to land on, when the report named
    /// one. This is what the node's ACL narrows a check on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_ref: Option<String>,
}

/// Whether a review is still accepting verdicts, or has been settled and
/// had its bulk dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReviewStatus {
    /// Accepting verdicts; `reviewers` and `verdicts` are authoritative.
    #[default]
    Live,
    /// Settled: `reviewers` and `verdicts` have been dropped, and the
    /// outcome they produced is recorded here instead. Distinguishable
    /// from a review that never existed, which is the point — a reviewer
    /// told "no such review" would go hunting for a typo.
    Archived {
        /// The verdict the review had reached when it was archived.
        approved: bool,
        /// Approval weight at settlement time. Reviewer details are
        /// dropped, so this compact scalar preserves the per-operator
        /// cap for later protected-ref authorization.
        approval_weight: usize,
    },
}

/// Maximum approval weight contributed by one operator, regardless of
/// how many agent channels that operator controls.
pub const MAX_APPROVAL_WEIGHT_PER_OPERATOR: usize = 1;

/// The operator a review channel belongs to: the part before the first
/// `/` in `operator/agent`, or the whole name when no prefix is present.
///
/// Unprefixed names remain distinct operators, preserving the original
/// flat-name behavior. Nodes that rely on this boundary must bind channel
/// names to trusted keys; otherwise the prefix is only a self-assertion.
#[must_use]
pub fn reviewer_operator(name: &str) -> &str {
    name.split_once('/').map_or(name, |(operator, _)| operator)
}

/// One comment on a review, as the fold sees it after replaying
/// [`OpKind::PostComment`] (D38).
///
/// There is no timestamp. A pure fold has no clock, so `at` counts
/// sequenced ops exactly as [`KeyBinding::bound_at`] does: it orders the
/// thread, it is monotonic, and replay reproduces it from the log alone.
/// Anything phrased in minutes or days needs a durable timestamp this
/// crate does not have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentState {
    /// The author's chosen id, unique within the review.
    pub id: String,
    /// Channel that made the statement.
    pub author: String,
    /// The comment text.
    pub body: String,
    /// Fold position the comment was applied at, which is the log
    /// sequence its op occupies.
    pub at: u64,
}

/// One reviewer's answer, as the fold recorded it.
///
/// The `at` field is D44's addition and the reason this is a struct
/// rather than the `(Verdict, String)` pair it used to be. A verdict is
/// keyed by **channel**, and a channel's key rotates: revoke a key and
/// bind a fresh one and the channel now names a key that never cast this
/// verdict. Without the position, "which key approved this" has no answer
/// the log can reproduce, and an authorization record naming the current
/// key would be asserting something false.
///
/// This is derived state, not wire format — nothing here is hashed or
/// persisted, so recording it moves no entry hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerdictState {
    /// The answer itself.
    pub verdict: Verdict,
    /// The note the reviewer attached; empty when they left none.
    pub note: String,
    /// Fold position the verdict was applied at, which is the log
    /// sequence its op occupies — the same clock as
    /// [`CommentState::at`] and [`KeyBinding::bound_at`].
    pub at: u64,
}

/// Materialized state of one review: what is under review, who was
/// asked, who has answered what, and what was said about it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReviewState {
    /// The commit under review.
    pub target: Option<ContentHash>,
    /// Actors the review fanned out to.
    pub reviewers: Vec<String>,
    /// reviewer → their answer; absent = not answered yet.
    pub verdicts: BTreeMap<String, VerdictState>,
    /// reviewer → node-recorded reason for retroactively invalidating
    /// that approval. Kept separately from verdict bulk so archived rows
    /// remain compact while the append-only decision stays visible.
    pub slashes: BTreeMap<String, String>,
    /// The ref the review proposes to land on (`<repo>:<refname>`), or
    /// `None` for a review that named no destination. Policy reads this
    /// to decide whether a review is privilege-bearing.
    pub target_ref: Option<String>,
    /// The discussion, in the order the sequencer admitted it (D38).
    /// Emptied by [`OpKind::ArchiveReview`] with the rest of the bulk.
    pub comments: Vec<CommentState>,
    /// viewer → fold position of that viewer's first recorded read
    ///. Emptied by [`OpKind::ArchiveReview`]
    /// with the rest of the bulk.
    pub viewed: BTreeMap<String, u64>,
    /// Live, or settled with its outcome retained. Defaults to
    /// [`ReviewStatus::Live`], so replaying a log written before
    /// archiving existed yields exactly the previous behaviour.
    pub status: ReviewStatus,
}

/// Materialized identity and current revision of one logical change.
///
/// Revision history remains in the append-only op log. The view keeps the
/// latest exact revision needed for CAS and review selection, plus the
/// create binding needed to make workspace retries deterministic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeState {
    /// Channel allowed to checkpoint this change.
    pub owner: String,
    /// Workspace identity originally bound to the change. Retained after
    /// archival so a delayed retry cannot target a later workspace that
    /// reused the same name.
    pub workspace_id: String,
    /// Active mutable workspace, or `None` after archival.
    pub active_workspace: Option<String>,
    /// Immutable revision from which the change began.
    pub base_revision: ContentHash,
    /// Latest immutable revision published for the change. This equals
    /// `base_revision` until the first checkpoint.
    pub revision_id: ContentHash,
    /// Owner-scoped identity of the create request.
    pub idempotency_key: String,
    /// Directory prefixes the change declared it works within; empty
    /// means the whole tree.
    pub cone: Vec<String>,
}

impl ReviewState {
    fn operator_is_slashed(&self, reviewer: &str) -> bool {
        let operator = reviewer_operator(reviewer);
        self.slashes
            .keys()
            .any(|slashed| reviewer_operator(slashed) == operator)
    }

    fn live_approval_weight(&self, apply_slashes: bool) -> usize {
        self.counted_approver_channels(apply_slashes).len() * MAX_APPROVAL_WEIGHT_PER_OPERATOR
    }

    fn counted_approver_channels(&self, apply_slashes: bool) -> Vec<(&str, u64)> {
        self.verdicts
            .iter()
            .enumerate()
            .filter(|(index, (reviewer, answer))| {
                if answer.verdict != Verdict::Approve
                    || (apply_slashes && self.operator_is_slashed(reviewer))
                {
                    return false;
                }
                let operator = reviewer_operator(reviewer);
                self.verdicts
                    .iter()
                    .take(*index)
                    .all(|(prior, prior_answer)| {
                        prior_answer.verdict != Verdict::Approve
                            || reviewer_operator(prior) != operator
                    })
            })
            .map(|(_, (reviewer, answer))| (reviewer.as_str(), answer.at))
            .collect()
    }

    /// The reviewer channels [`ReviewState::approval_weight`] actually
    /// counts: the first standing `Approve` from each distinct operator,
    /// in verdict order, slashes applied.
    ///
    /// This exists so a [`OpKind::Submit`] can name the approvals its
    /// weight rested on rather than "everyone who clicked approve". The
    /// weight is *defined* as this list's length times
    /// [`MAX_APPROVAL_WEIGHT_PER_OPERATOR`], so the number in an
    /// authorization record and the names beside it cannot drift.
    ///
    /// Each entry pairs the channel with the fold position of the verdict
    /// being counted, because that position is what resolves the channel
    /// to a key (see [`View::bound_actor_at`]). Returning the channel
    /// alone would leave every caller to look the position up again, and
    /// the one that forgot would silently credit whatever key holds the
    /// channel today.
    ///
    /// Meaningless on an archived review, whose verdicts are gone: the
    /// list is empty while [`ReviewState::approval_weight`] still answers
    /// from the stored total.
    #[must_use]
    pub fn counted_approvers(&self) -> Vec<(&str, u64)> {
        self.counted_approver_channels(true)
    }

    /// Whether `reviewer` has an `Approve` verdict that no slash has
    /// invalidated — the individual half of [`ReviewState::counted_approvers`],
    /// used where one named approval has to be checked rather than a set.
    #[must_use]
    pub fn approval_stands(&self, reviewer: &str) -> bool {
        self.standing_approval_at(reviewer).is_some()
    }

    /// The fold position of `reviewer`'s standing approval, or `None` if
    /// they have none — [`ReviewState::approval_stands`] plus the one
    /// fact a caller needs to resolve it to a key
    /// ([`View::bound_actor_at`]).
    ///
    /// The predicate is defined as this returning `Some`, so the two can
    /// never disagree about what "standing" means.
    #[must_use]
    pub fn standing_approval_at(&self, reviewer: &str) -> Option<u64> {
        self.verdicts
            .get(reviewer)
            .filter(|answer| answer.verdict == Verdict::Approve)
            .filter(|_| !self.operator_is_slashed(reviewer))
            .map(|answer| answer.at)
    }

    fn slashed_operator_count(&self) -> usize {
        self.slashes
            .keys()
            .enumerate()
            .filter(|(index, reviewer)| {
                let operator = reviewer_operator(reviewer);
                self.slashes
                    .keys()
                    .take(*index)
                    .all(|prior| reviewer_operator(prior) != operator)
            })
            .count()
    }

    /// Whether every listed reviewer has answered. An unassigned review
    /// (no reviewers yet) is never complete — vacuous truth must not
    /// turn "asked nobody" into a finished review.
    #[must_use]
    pub fn complete(&self) -> bool {
        if matches!(self.status, ReviewStatus::Archived { .. }) {
            // Archiving requires completeness, and the reviewer list it
            // was computed from is gone. Recomputing here would read the
            // emptied list and answer "not complete", silently unfinishing
            // every settled review.
            return true;
        }
        !self.reviewers.is_empty() && self.reviewers.iter().all(|r| self.verdicts.contains_key(r))
    }

    /// Whether the review is complete with no `RequestChanges`.
    #[must_use]
    pub fn approved(&self) -> bool {
        if let ReviewStatus::Archived { approved, .. } = self.status {
            return approved && self.approval_weight() > 0;
        }
        if !self.complete() {
            return false;
        }
        let mut eligible = self
            .reviewers
            .iter()
            .filter(|reviewer| !self.slashes.contains_key(*reviewer));
        let Some(first) = eligible.next() else {
            return false;
        };
        self.verdicts
            .get(first)
            .is_some_and(|answer| answer.verdict == Verdict::Approve)
            && eligible.all(|reviewer| {
                self.verdicts
                    .get(reviewer)
                    .is_some_and(|answer| answer.verdict == Verdict::Approve)
            })
    }

    /// Approval weight after capping every operator at one unit.
    ///
    /// Multiple agent channels under one `operator/agent` prefix never
    /// manufacture additional approval weight. Archived reviews return
    /// the compact weight captured before their reviewer detail was
    /// dropped.
    #[must_use]
    pub fn approval_weight(&self) -> usize {
        if let ReviewStatus::Archived {
            approval_weight, ..
        } = self.status
        {
            return approval_weight
                .saturating_sub(self.slashed_operator_count() * MAX_APPROVAL_WEIGHT_PER_OPERATOR);
        }
        self.live_approval_weight(true)
    }

    /// Whether a retroactive invalidation requires a fresh review before
    /// the same `(ref, commit)` can authorize another protected landing.
    #[must_use]
    pub fn re_review_required(&self) -> bool {
        !self.slashes.is_empty()
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
    /// Check-report precondition failure: an empty name or reporter, or
    /// a name carrying the key separator.
    Check(String),
    /// Change lifecycle precondition failure (duplicate identity,
    /// invalid binding, unknown/archived change, or no-op checkpoint).
    Change(String),
    /// Key-binding precondition failure: an empty or `/`-bearing
    /// operator, a channel that reads as a different operator, a key
    /// already bound elsewhere, or a revoked/unbound key.
    Identity(String),
    /// Ref-snapshot precondition failure: the claimed ref map does not
    /// match the view, the claimed position is not the fold position, or
    /// the chain pointer does not name the latest admitted snapshot.
    Snapshot(String),
    /// Resolution-link precondition failure: [`Commit::resolves`] names
    /// a commit the store does not hold, or one with nothing to resolve.
    Resolution(String),
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
    /// The commit whose [`TreeEntry::Conflict`] this commit resolves,
    /// when it is a resolution — resolution-as-linked-change,
    /// as metadata on the existing shape (DECISIONS.md D15: never a new merge
    /// substrate). A conflict is a value (invariant 6): the link points
    /// *at* the conflicted commit, which stays in history untouched.
    /// Additive (`default` + `skip_serializing_if`), so commits written
    /// before the field existed decode as `None` and re-serialize
    /// byte-identically — invariant 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolves: Option<ContentHash>,
}

/// Conflicts in one commit, split by whether the reader's cone covers
/// them (D50).
///
/// The split is the point. `inside` names paths whose content the reader
/// may fetch; `outside` names paths and **nothing else** -- no base,
/// left or right address, no size, no message. The type is the
/// enforcement: `outside` is a list of strings, so there is no field a
/// later change could accidentally start populating with content.
///
/// This is a capability the partial-clone story alone does not have.
/// Withholding content is ordinary; git does it, and so does every
/// system with a sparse checkout. What is unusual is being able to say
/// *that a collision happened* at a path the reader cannot read, which
/// choir can do only because the op log is separate from the content it
/// orders. A reader who never receives `docs/guide.md` still learns
/// that their merge collided there, and can go ask the person who owns
/// it. Elsewhere that collision is simply invisible until someone else
/// trips over it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConflictReport {
    /// Conflicted paths the cone covers, readable in full.
    pub inside: Vec<String>,
    /// Conflicted paths the cone does not cover. Paths only.
    pub outside: Vec<String>,
}

impl ConflictReport {
    /// Whether the commit conflicts anywhere, in or out of the cone.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inside.is_empty() && self.outside.is_empty()
    }
}

/// Whether `cone` covers `path`, in git's cone spelling.
///
/// An empty cone covers everything, which is what a change that
/// declared no scope means and what every change written before cones
/// existed decodes to. A prefix matches on a directory boundary, so
/// `services/api` covers `services/api/main.rs` and does **not** cover
/// `services/apiary/main.rs` -- the bug a bare `starts_with` would
/// introduce, and the reason this is a named function with a test
/// rather than an inline call.
#[must_use]
pub fn cone_covers(cone: &[String], path: &str) -> bool {
    if cone.is_empty() {
        return true;
    }
    cone.iter().any(|prefix| {
        let prefix = prefix.trim_end_matches('/');
        prefix.is_empty()
            || path == prefix
            || path
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// Splits `commit`'s conflicts into what `cone` covers and what it does
/// not.
///
/// Pure, and deliberately takes the commit rather than a store handle:
/// redaction that needed I/O would be redaction that can fail open.
#[must_use]
pub fn conflicts_for_cone(commit: &Commit, cone: &[String]) -> ConflictReport {
    let mut report = ConflictReport::default();
    for (path, entry) in &commit.tree {
        if !matches!(entry, TreeEntry::Conflict { .. }) {
            continue;
        }
        if cone_covers(cone, path) {
            report.inside.push(path.clone());
        } else {
            report.outside.push(path.clone());
        }
    }
    report
}

impl Commit {
    /// Whether any tree entry is an unresolved [`TreeEntry::Conflict`].
    pub fn is_conflicted(&self) -> bool {
        self.tree
            .values()
            .any(|e| matches!(e, TreeEntry::Conflict { .. }))
    }

    /// Validates this commit's `resolves` link against `store`.
    ///
    /// A `Some` link must name a commit the store holds whose tree still
    /// carries an unresolved [`TreeEntry::Conflict`] — anything else is
    /// refused, never silently dropped: a dangling link admitted once
    /// would replay forever as a claim about a conflict nobody can load.
    /// `None` validates trivially (most commits resolve nothing).
    ///
    /// # Errors
    ///
    /// Returns [`ViewError::Resolution`] when the link is dangling or the
    /// referenced commit has no conflict to resolve.
    pub fn validate_resolves(&self, store: &dyn ChunkStore) -> Result<(), ViewError> {
        let Some(target) = &self.resolves else {
            return Ok(());
        };
        let resolved = Commit::get(store, target).map_err(|e| {
            ViewError::Resolution(format!(
                "resolves names {} which the store cannot supply: {e:?}",
                target.to_hex()
            ))
        })?;
        if !resolved.is_conflicted() {
            return Err(ViewError::Resolution(format!(
                "resolves names {} which holds no unresolved conflict",
                target.to_hex()
            )));
        }
        Ok(())
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
    /// Stable logical change id → owner, workspace and exact revision.
    pub changes: BTreeMap<String, ChangeState>,
    /// Ref name → target commit id.
    pub refs: BTreeMap<String, ContentHash>,
    /// Review id → review state (fan-out and verdicts).
    pub reviews: BTreeMap<String, ReviewState>,
    /// Subject → record kind → latest body (D22 provenance records).
    pub provenance: BTreeMap<String, BTreeMap<String, String>>,
    /// `<subject-hex>:<check-name>` → the latest report for it (D49).
    ///
    /// One flat map rather than subject → name → state, because every
    /// consumer of a view section wants the same two things: the ACL
    /// narrows section rows one at a time, and the node's paging bounds
    /// them one at a time. A nested map would need both to learn a
    /// second shape, and "all checks on commit X" is still a prefix
    /// scan. The separator is `:` for the same reason `refs` uses it,
    /// and it cannot collide: a hex subject contains no `:`.
    pub checks: BTreeMap<String, CheckState>,
    /// Actor key id → its durable operator binding (D24 T1/T3 substrate).
    ///
    /// Keyed by [`ContentHash::to_hex`] rather than by the hash itself so
    /// it joins directly against the `key_id` a [`choir_oplog::Witness`]
    /// carries, which is how an entry names its author.
    pub bindings: BTreeMap<String, KeyBinding>,
    /// Number of ops folded so far, which is the log sequence the next
    /// applied op will occupy.
    ///
    /// It is a fold counter rather than a value read off the log, because
    /// [`View::apply`] is handed only the op — a signature this crate is
    /// deliberately not changing. The counter is correct because every
    /// path that builds a view applies exactly the log's ops, in order,
    /// once: [`View::at`] replays a prefix, and a node's live view starts
    /// from such a replay and then applies each entry the sequencer
    /// admits. A caller holding both can assert `view.next_seq ==
    /// entry.seq` before applying; nothing inside the fold can check it
    /// on their behalf.
    pub next_seq: u64,
    /// The most recent admitted [`RefSnapshot`], whole rather than by
    /// id: readers ask a view "what is the latest attestation?" and the
    /// chain check needs its identity, which [`RefSnapshot::id`] derives
    /// from the value.
    pub latest_snapshot: Option<RefSnapshot>,
}

impl View {
    /// Rederives a [`OpKind::Submit`]'s authorization and refuses a
    /// mismatch (D43).
    ///
    /// The node's gate decides *whether* a landing is allowed, reading an
    /// ACL and a protected-ref list that are not in the log. This checks
    /// everything about that decision which **is** in the log: that the
    /// cited review exists, is live, and proposes exactly this landing;
    /// that a named approving owner really has a standing approval on it;
    /// that a claimed approval weight is the weight this review actually
    /// carries; and that the approver ids are the ones the log's own
    /// bindings produce.
    ///
    /// So the ACL grant is the single unverifiable element, and it is
    /// named rather than implied. Everything else is refused on
    /// disagreement by every replayer, not just by the node that admitted
    /// it — the [`OpKind::RecordRefSnapshot`] discipline.
    fn validate_submit(
        &self,
        review: &str,
        name: &str,
        commit: &ContentHash,
        authorization: &Authorization,
    ) -> Result<(), ViewError> {
        let state = self
            .reviews
            .get(review)
            .ok_or_else(|| ViewError::Review(format!("no such review {review}")))?;
        if matches!(state.status, ReviewStatus::Archived { .. }) {
            // Archiving drops the verdicts, so nothing here could be
            // rederived. Refusing keeps "the record was checked" true
            // without exception, rather than true except when it is not.
            return Err(ViewError::Review(format!(
                "review {review} is archived and its verdicts are gone, so the \
                 authorization this landing claims cannot be checked"
            )));
        }
        if state.target_ref.as_deref() != Some(name) {
            return Err(ViewError::Review(format!(
                "review {review} proposes to land on {}, not {name}",
                state.target_ref.as_deref().unwrap_or("no ref")
            )));
        }
        if state.target.as_ref() != Some(commit) {
            return Err(ViewError::Review(format!(
                "review {review} names commit {}, not {}",
                state
                    .target
                    .as_ref()
                    .map_or_else(|| "none".to_string(), ContentHash::to_hex),
                commit.to_hex()
            )));
        }
        let expected: Vec<ContentHash> = match &authorization.basis {
            Basis::OwnerLanded { owner } => {
                if owner.is_empty() {
                    return Err(ViewError::Review(
                        "an owner-landed authorization must name the owner".to_string(),
                    ));
                }
                Vec::new()
            }
            Basis::OwnerApproved { owner } => {
                let at = state.standing_approval_at(owner).ok_or_else(|| {
                    ViewError::Review(format!(
                        "{owner} has no standing approval on review {review}"
                    ))
                })?;
                vec![self.bound_actor_at(owner, at).map_err(ViewError::Review)?]
            }
            Basis::ApprovalWeight { required, met } => {
                if *required == 0 {
                    return Err(ViewError::Review(
                        "an approval-weight authorization must state a nonzero threshold"
                            .to_string(),
                    ));
                }
                let actual = u32::try_from(state.approval_weight()).unwrap_or(u32::MAX);
                if *met != actual {
                    return Err(ViewError::Review(format!(
                        "review {review} carries approval weight {actual}, not the claimed {met}"
                    )));
                }
                if met < required {
                    return Err(ViewError::Review(format!(
                        "approval weight {met} is below the {required} this authorization claims \
                         to satisfy"
                    )));
                }
                state
                    .counted_approvers()
                    .into_iter()
                    .map(|(channel, at)| self.bound_actor_at(channel, at))
                    .collect::<Result<Vec<_>, String>>()
                    .map_err(ViewError::Review)?
            }
        };
        if authorization.approvers != expected {
            return Err(ViewError::Review(format!(
                "authorization on review {review} lists {} approvers where the log's own \
                 bindings produce {}",
                authorization.approvers.len(),
                expected.len()
            )));
        }
        Ok(())
    }

    /// Whether `op` would apply cleanly, without changing anything.
    ///
    /// Every precondition in this model is a read — a CAS comparison, a
    /// key lookup, or a non-empty check — so admission can be decided
    /// against a shared `&View` rather than against a private copy of it.
    /// That is what lets the single-writer admission path stop cloning the
    /// whole view per submission.
    ///
    /// [`View::apply`] calls this first and mutates only on `Ok`, so the
    /// two can never disagree about what is admissible. Keeping them as
    /// one code path is the point: a separate fast-path predicate that
    /// drifts from the real one is how "checked in check()" turns into a
    /// panic on the writer thread.
    ///
    /// # Errors
    ///
    /// The same failures [`View::apply`] would return for `op`.
    pub fn validate(&self, op: &ViewOp) -> Result<(), ViewError> {
        /// CAS comparison shared by the three head-moving ops.
        fn cas(
            actual: Option<&ContentHash>,
            expected: &Option<ContentHash>,
            target: &str,
        ) -> Result<(), ViewError> {
            if actual != expected.as_ref() {
                return Err(ViewError::StaleHead {
                    target: target.to_string(),
                    expected: expected.clone(),
                    actual: actual.cloned(),
                });
            }
            Ok(())
        }

        match &op.kind {
            OpKind::SetWorkspaceHead {
                workspace, prev, ..
            } => cas(self.workspaces.get(workspace), prev, workspace),
            OpKind::SetRef { name, prev, .. } | OpKind::DeleteRef { name, prev } => {
                cas(self.refs.get(name), prev, name)
            }
            OpKind::Submit {
                review,
                name,
                commit,
                prev,
                authorization,
            } => {
                cas(self.refs.get(name), prev, name)?;
                self.validate_submit(review, name, commit, authorization)
            }
            // Removing an absent workspace is not an error: the op is a
            // statement about the end state, not about the transition.
            OpKind::DeleteWorkspace { .. } => Ok(()),
            OpKind::RequestReview { id, .. } => {
                if self.reviews.contains_key(id) {
                    return Err(ViewError::Review(format!("review {id} already exists")));
                }
                Ok(())
            }
            OpKind::AssignReviewers { id, reviewers } => {
                if reviewers.is_empty() {
                    return Err(ViewError::Review(
                        "assignment must name at least one reviewer".to_string(),
                    ));
                }
                let review = self
                    .reviews
                    .get(id)
                    .ok_or_else(|| ViewError::Review(format!("no such review {id}")))?;
                if matches!(review.status, ReviewStatus::Archived { .. }) {
                    // Its reviewer list is empty because it was emptied,
                    // not because it is unassigned.
                    return Err(ViewError::Review(format!("review {id} is archived")));
                }
                if !review.reviewers.is_empty() {
                    return Err(ViewError::Review(format!(
                        "review {id} is already assigned"
                    )));
                }
                Ok(())
            }
            OpKind::PostVerdict { id, reviewer, .. } => {
                let review = self
                    .reviews
                    .get(id)
                    .ok_or_else(|| ViewError::Review(format!("no such review {id}")))?;
                if matches!(review.status, ReviewStatus::Archived { .. }) {
                    return Err(ViewError::Review(format!(
                        "review {id} is archived and accepts no further verdicts"
                    )));
                }
                if !review.reviewers.iter().any(|r| r == reviewer) {
                    return Err(ViewError::Review(format!(
                        "{reviewer} is not a reviewer of {id}"
                    )));
                }
                if review.slashes.contains_key(reviewer) {
                    return Err(ViewError::Review(format!(
                        "{reviewer}'s approval on {id} was slashed; open a new review"
                    )));
                }
                Ok(())
            }
            OpKind::SlashApproval {
                id,
                reviewer,
                reason,
            } => {
                if reviewer.is_empty() || reason.is_empty() {
                    return Err(ViewError::Review(
                        "a slash must name a reviewer and a non-empty reason".to_string(),
                    ));
                }
                let review = self
                    .reviews
                    .get(id)
                    .ok_or_else(|| ViewError::Review(format!("no such review {id}")))?;
                if review.slashes.contains_key(reviewer) {
                    return Err(ViewError::Review(format!(
                        "{reviewer}'s approval on {id} is already slashed"
                    )));
                }
                match review.status {
                    ReviewStatus::Live => {
                        if !review.reviewers.iter().any(|listed| listed == reviewer) {
                            return Err(ViewError::Review(format!(
                                "{reviewer} is not a reviewer of {id}"
                            )));
                        }
                        if !review
                            .verdicts
                            .get(reviewer)
                            .is_some_and(|answer| answer.verdict == Verdict::Approve)
                        {
                            return Err(ViewError::Review(format!(
                                "{reviewer} has no approval to slash on {id}"
                            )));
                        }
                    }
                    ReviewStatus::Archived { approved, .. } => {
                        if !approved || review.approval_weight() == 0 {
                            return Err(ViewError::Review(format!(
                                "archived review {id} carries no approval to slash"
                            )));
                        }
                    }
                }
                Ok(())
            }
            OpKind::ArchiveReview { id, lapsed } => {
                let review = self
                    .reviews
                    .get(id)
                    .ok_or_else(|| ViewError::Review(format!("no such review {id}")))?;
                if matches!(review.status, ReviewStatus::Archived { .. }) {
                    return Err(ViewError::Review(format!(
                        "review {id} is already archived"
                    )));
                }
                if review.complete() && *lapsed {
                    // It reached an outcome; lapsing would discard it.
                    return Err(ViewError::Review(format!(
                        "review {id} is complete and cannot be lapsed"
                    )));
                }
                if !review.complete() && !*lapsed {
                    // Freezing an unfinished review would strand it: the
                    // outcome is not decided and no further verdict can
                    // decide it. Lapsing is the deliberate way to say
                    // "this one is abandoned", so it must be asked for.
                    return Err(ViewError::Review(format!(
                        "review {id} is not complete; archive it with lapsed to settle it as \
                         unapproved"
                    )));
                }
                Ok(())
            }
            OpKind::PostComment {
                id,
                comment,
                author,
                body,
            } => {
                if comment.is_empty() || author.is_empty() || body.is_empty() {
                    return Err(ViewError::Review(
                        "a comment must carry an id, an author and a body".to_string(),
                    ));
                }
                let review = self
                    .reviews
                    .get(id)
                    .ok_or_else(|| ViewError::Review(format!("no such review {id}")))?;
                if matches!(review.status, ReviewStatus::Archived { .. }) {
                    // Archiving dropped the thread, so an id that was
                    // taken no longer looks taken. Refusing every comment
                    // on an archived review is what keeps the uniqueness
                    // rule -- and with it the replay defence -- true for
                    // the whole life of the review.
                    return Err(ViewError::Review(format!(
                        "review {id} is archived and accepts no further comments"
                    )));
                }
                if review.comments.iter().any(|held| held.id == *comment) {
                    return Err(ViewError::Review(format!(
                        "comment {comment} already exists on review {id}"
                    )));
                }
                Ok(())
            }
            OpKind::ViewedReview { id, viewer } => {
                if viewer.is_empty() {
                    return Err(ViewError::Review(
                        "a read receipt must name its viewer".to_string(),
                    ));
                }
                let review = self
                    .reviews
                    .get(id)
                    .ok_or_else(|| ViewError::Review(format!("no such review {id}")))?;
                if matches!(review.status, ReviewStatus::Archived { .. }) {
                    // Archiving dropped the receipts, so a viewer that was
                    // recorded no longer looks recorded; refusing receipts
                    // on an archived review keeps the first-read rule true
                    // for the whole life of the review, as for comments.
                    return Err(ViewError::Review(format!(
                        "review {id} is archived and accepts no further receipts"
                    )));
                }
                if review.viewed.contains_key(viewer) {
                    return Err(ViewError::Review(format!(
                        "viewer {viewer} already holds a receipt on review {id}"
                    )));
                }
                Ok(())
            }
            OpKind::RecordProvenance { subject, kind, .. } => {
                if subject.is_empty() || kind.is_empty() {
                    return Err(ViewError::Provenance(
                        "provenance subject and kind must be non-empty".to_string(),
                    ));
                }
                Ok(())
            }
            OpKind::RecordCheck { name, reporter, .. } => {
                if name.is_empty() || reporter.is_empty() {
                    return Err(ViewError::Check(
                        "a check must name itself and its reporter".to_string(),
                    ));
                }
                // The key is built by joining on `:`, so a name carrying
                // one could address a row belonging to another subject.
                // Refused here rather than escaped, because the view is
                // a map and an escaping scheme is a second encoding of a
                // hashed structure (invariant 3's failure mode).
                if name.contains(':') {
                    return Err(ViewError::Check(format!(
                        "check name {name} may not contain ':'"
                    )));
                }
                Ok(())
            }
            OpKind::CreateChange {
                id,
                owner,
                workspace,
                idempotency_key,
                ..
            } => {
                if id.is_empty()
                    || owner.is_empty()
                    || workspace.is_empty()
                    || idempotency_key.is_empty()
                {
                    return Err(ViewError::Change(
                        "change id, owner, workspace and idempotency key must be non-empty"
                            .to_string(),
                    ));
                }
                if self.changes.contains_key(id) {
                    return Err(ViewError::Change(format!("change {id} already exists")));
                }
                if self.workspaces.contains_key(workspace) {
                    return Err(ViewError::Change(format!(
                        "workspace {workspace} already exists"
                    )));
                }
                if let Some((existing, _)) = self.changes.iter().find(|(_, change)| {
                    change.workspace_id == *workspace
                        && change.active_workspace.as_deref() == Some(workspace)
                }) {
                    return Err(ViewError::Change(format!(
                        "workspace {workspace} is active on change {existing}"
                    )));
                }
                if let Some((existing, _)) = self.changes.iter().find(|(_, change)| {
                    change.owner == *owner && change.idempotency_key == *idempotency_key
                }) {
                    return Err(ViewError::Change(format!(
                        "idempotency key already belongs to change {existing}"
                    )));
                }
                Ok(())
            }
            OpKind::CheckpointChange {
                id,
                workspace,
                revision,
                prev_revision,
            } => {
                if id.is_empty() || workspace.is_empty() {
                    return Err(ViewError::Change(
                        "change id and workspace must be non-empty".to_string(),
                    ));
                }
                let change = self
                    .changes
                    .get(id)
                    .ok_or_else(|| ViewError::Change(format!("no such change {id}")))?;
                if change.active_workspace.as_deref() != Some(workspace) {
                    return Err(ViewError::Change(format!(
                        "change {id} is not active in workspace {workspace}"
                    )));
                }
                cas(
                    Some(&change.revision_id),
                    &Some(prev_revision.clone()),
                    &format!("change {id}"),
                )?;
                cas(
                    self.workspaces.get(workspace),
                    &Some(prev_revision.clone()),
                    workspace,
                )?;
                if revision == prev_revision {
                    return Err(ViewError::Change(format!(
                        "checkpoint for change {id} must advance to a different revision"
                    )));
                }
                Ok(())
            }
            OpKind::ArchiveChange {
                id,
                workspace,
                prev_revision,
                owner,
                ..
            } => {
                if id.is_empty() || workspace.is_empty() || owner.is_empty() {
                    return Err(ViewError::Change(
                        "change id, workspace and owner must be non-empty".to_string(),
                    ));
                }
                let change = self
                    .changes
                    .get(id)
                    .ok_or_else(|| ViewError::Change(format!("no such change {id}")))?;
                if change.active_workspace.as_deref() != Some(workspace) {
                    return Err(ViewError::Change(format!(
                        "change {id} is not active in workspace {workspace}"
                    )));
                }
                if change.owner != *owner {
                    return Err(ViewError::Change(format!(
                        "change {id} is owned by a different channel"
                    )));
                }
                cas(
                    Some(&change.revision_id),
                    &Some(prev_revision.clone()),
                    &format!("change {id}"),
                )?;
                cas(
                    self.workspaces.get(workspace),
                    &Some(prev_revision.clone()),
                    workspace,
                )
            }
            OpKind::BindKey {
                operator,
                key,
                channel,
            } => {
                if operator.is_empty() {
                    return Err(ViewError::Identity(
                        "a binding must name an operator".to_string(),
                    ));
                }
                if operator.contains('/') {
                    // A `/` would let one operator identity read as a
                    // different one through the channel-prefix rule.
                    return Err(ViewError::Identity(format!(
                        "operator {operator} must not contain '/'"
                    )));
                }
                if let Some(channel) = channel {
                    if channel.is_empty() {
                        return Err(ViewError::Identity(
                            "a bound channel must be non-empty; omit it instead".to_string(),
                        ));
                    }
                    let reads_as = reviewer_operator(channel);
                    if reads_as != operator {
                        return Err(ViewError::Identity(format!(
                            "channel {channel} reads as operator {reads_as}, not {operator}"
                        )));
                    }
                }
                match self.bindings.get(&key.to_hex()) {
                    None => Ok(()),
                    // Terminal by design: a rebindable revocation is no
                    // revocation at all. The remedy is a fresh key.
                    Some(bound) if bound.is_revoked() => Err(ViewError::Identity(format!(
                        "key {} is revoked; bind a fresh key",
                        key.to_hex()
                    ))),
                    // Re-binding to the *same* operator is how a channel
                    // is corrected, and it keeps `bound_at`. Re-binding
                    // elsewhere would transfer accumulated standing.
                    Some(bound) if bound.operator != *operator => {
                        Err(ViewError::Identity(format!(
                            "key {} is already bound to operator {}",
                            key.to_hex(),
                            bound.operator
                        )))
                    }
                    Some(_) => Ok(()),
                }
            }
            OpKind::RevokeKey { key, reason } => {
                if reason.is_empty() {
                    return Err(ViewError::Identity(
                        "a revocation must carry a non-empty reason".to_string(),
                    ));
                }
                let bound = self.bindings.get(&key.to_hex()).ok_or_else(|| {
                    ViewError::Identity(format!("key {} is not bound", key.to_hex()))
                })?;
                if bound.is_revoked() {
                    return Err(ViewError::Identity(format!(
                        "key {} is already revoked",
                        key.to_hex()
                    )));
                }
                Ok(())
            }
            OpKind::RecordRefSnapshot { snapshot } => {
                // Truth first: an attestation the fold cannot reproduce
                // is refused by every replayer, not archived as a claim.
                if snapshot.refs != self.refs {
                    return Err(ViewError::Snapshot(
                        "snapshot does not match the ref-state it claims to attest".to_string(),
                    ));
                }
                if snapshot.at_seq != self.next_seq {
                    return Err(ViewError::Snapshot(format!(
                        "snapshot was taken at position {}, the view is at {}",
                        snapshot.at_seq, self.next_seq
                    )));
                }
                // The chain rule is what makes an *old* snapshot
                // inadmissible even when the ref map recurs (the ABA
                // shape): its `prev_snapshot` no longer names the latest.
                let expected = self.latest_snapshot.as_ref().map(RefSnapshot::id);
                if snapshot.prev_snapshot != expected {
                    return Err(ViewError::Snapshot(format!(
                        "snapshot chain expected prev {:?}, op names {:?}",
                        expected.as_ref().map(ContentHash::to_hex),
                        snapshot.prev_snapshot.as_ref().map(ContentHash::to_hex)
                    )));
                }
                Ok(())
            }
        }
    }

    /// The [`View::checks`] key for one `(subject, check name)` pair.
    #[must_use]
    pub fn check_key(subject: &ContentHash, name: &str) -> String {
        format!("{}:{name}", subject.to_hex())
    }

    /// Every check reported against `subject`, as `(name, state)` in
    /// name order.
    #[must_use]
    pub fn checks_for(&self, subject: &ContentHash) -> Vec<(&str, &CheckState)> {
        let prefix = format!("{}:", subject.to_hex());
        self.checks
            .range(prefix.clone()..)
            .take_while(|(key, _)| key.starts_with(&prefix))
            .filter_map(|(key, state)| key.split_once(':').map(|(_, name)| (name, state)))
            .collect()
    }

    /// The one answer for `subject`, or `None` when nothing reported.
    ///
    /// A failure outranks a run still in flight. Both are "not green",
    /// but only one of them can still become green, and a caller
    /// deciding whether to wait needs that distinction to point the
    /// right way: told `Running` while a sibling check has already
    /// failed, it waits for an outcome that cannot arrive.
    #[must_use]
    pub fn checks_verdict(&self, subject: &ContentHash) -> Option<CheckStatus> {
        let states = self.checks_for(subject);
        if states.is_empty() {
            return None;
        }
        if states.iter().any(|(_, s)| s.status == CheckStatus::Failed) {
            return Some(CheckStatus::Failed);
        }
        if states.iter().any(|(_, s)| s.status == CheckStatus::Running) {
            return Some(CheckStatus::Running);
        }
        Some(CheckStatus::Passed)
    }

    /// Applies one op, enforcing its CAS precondition. A rejected op
    /// leaves the view unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`ViewError::StaleHead`] when `prev` does not match.
    pub fn apply(&mut self, op: &ViewOp) -> Result<(), ViewError> {
        // Preconditions live in `validate` and nowhere else, so admission
        // and application cannot drift apart.
        self.validate(op)?;
        match &op.kind {
            OpKind::SetWorkspaceHead {
                workspace, commit, ..
            } => {
                // A legacy move cannot leave a stale logical-change label
                // attached to a revision it did not checkpoint.
                for change in self.changes.values_mut() {
                    if change.active_workspace.as_deref() == Some(workspace) {
                        change.active_workspace = None;
                    }
                }
                self.workspaces.insert(workspace.clone(), commit.clone());
            }
            OpKind::SetRef { name, commit, .. } => {
                self.refs.insert(name.clone(), commit.clone());
            }
            // The ref move is all of it. The authorization is not
            // projected anywhere: it is a statement about one admission
            // decision, and the entry bytes are where a statement about
            // a past decision belongs. Copying it into the view would
            // create a second, mutable-looking home for an immutable
            // fact, and the review page can already tell a landed review
            // by comparing the ref to the target.
            OpKind::Submit { name, commit, .. } => {
                self.refs.insert(name.clone(), commit.clone());
            }
            OpKind::DeleteWorkspace { workspace } => {
                self.workspaces.remove(workspace);
                for change in self.changes.values_mut() {
                    if change.active_workspace.as_deref() == Some(workspace) {
                        change.active_workspace = None;
                    }
                }
            }
            OpKind::RequestReview {
                id,
                target,
                reviewers,
                target_ref,
            } => {
                self.reviews.insert(
                    id.clone(),
                    ReviewState {
                        target: Some(target.clone()),
                        reviewers: reviewers.clone(),
                        verdicts: BTreeMap::new(),
                        slashes: BTreeMap::new(),
                        target_ref: target_ref.clone(),
                        comments: Vec::new(),
                        viewed: BTreeMap::new(),
                        status: ReviewStatus::Live,
                    },
                );
            }
            OpKind::AssignReviewers { id, reviewers } => {
                self.reviews
                    .get_mut(id)
                    .expect("validate proved the review exists and is live")
                    .reviewers = reviewers.clone();
            }
            OpKind::ArchiveReview { id, lapsed } => {
                let review = self
                    .reviews
                    .get_mut(id)
                    .expect("validate proved the review exists and is settleable");
                let approval_weight = if *lapsed {
                    0
                } else {
                    // Store the pre-slash baseline. Archived reads apply
                    // the durable slash map, so capturing the already-
                    // discounted live value here would subtract a slash
                    // twice after compaction.
                    review.live_approval_weight(false)
                };
                review.status = ReviewStatus::Archived {
                    // A lapsed review was never answered, so it never got
                    // approval. Reading it off `approved()` would work
                    // today, but only because an incomplete review is
                    // never approved -- stating the outcome directly
                    // means a later change to `approved()` cannot quietly
                    // turn abandoned reviews into approvals.
                    approved: !*lapsed && review.approved(),
                    approval_weight,
                };
                // The bulk goes; the gate's compact authorization row
                // (target_ref, target, outcome, approval weight) stays.
                // Discussion is bulk by the same measure -- it is the
                // part that grows without bound -- and it informs no
                // authorization decision, so it goes with the verdicts.
                review.reviewers = Vec::new();
                review.verdicts = BTreeMap::new();
                review.comments = Vec::new();
                review.viewed = BTreeMap::new();
            }
            OpKind::PostVerdict {
                id,
                reviewer,
                verdict,
                note,
            } => {
                // Read before the mutable borrow, and the same clock
                // `bound_at` and `Revocation::at` use: this position is
                // what later resolves the channel to the key that was
                // live when the verdict was cast (D44).
                let at = self.next_seq;
                self.reviews
                    .get_mut(id)
                    .expect("validate proved the review exists, is live, and lists this reviewer")
                    .verdicts
                    .insert(
                        reviewer.clone(),
                        VerdictState {
                            verdict: *verdict,
                            note: note.clone(),
                            at,
                        },
                    );
            }
            OpKind::SlashApproval {
                id,
                reviewer,
                reason,
            } => {
                self.reviews
                    .get_mut(id)
                    .expect("validate proved the review has an approval to slash")
                    .slashes
                    .insert(reviewer.clone(), reason.clone());
            }
            OpKind::PostComment {
                id,
                comment,
                author,
                body,
            } => {
                let at = self.next_seq;
                self.reviews
                    .get_mut(id)
                    .expect("validate proved the review is live and free of this comment id")
                    .comments
                    .push(CommentState {
                        id: comment.clone(),
                        author: author.clone(),
                        body: body.clone(),
                        at,
                    });
            }
            OpKind::ViewedReview { id, viewer } => {
                let at = self.next_seq;
                self.reviews
                    .get_mut(id)
                    .expect("validate proved the review is live and new to this viewer")
                    .viewed
                    .insert(viewer.clone(), at);
            }
            OpKind::RecordCheck {
                subject,
                name,
                status,
                evidence,
                reporter,
                target_ref,
            } => {
                self.checks.insert(
                    Self::check_key(subject, name),
                    CheckState {
                        status: *status,
                        evidence: evidence.clone(),
                        reporter: reporter.clone(),
                        target_ref: target_ref.clone(),
                    },
                );
            }
            OpKind::RecordProvenance {
                subject,
                kind,
                body,
            } => {
                self.provenance
                    .entry(subject.clone())
                    .or_default()
                    .insert(kind.clone(), body.clone());
            }
            OpKind::DeleteRef { name, .. } => {
                self.refs.remove(name);
            }
            OpKind::CreateChange {
                id,
                owner,
                workspace,
                base_revision,
                idempotency_key,
                cone,
                ..
            } => {
                self.workspaces
                    .insert(workspace.clone(), base_revision.clone());
                self.changes.insert(
                    id.clone(),
                    ChangeState {
                        owner: owner.clone(),
                        workspace_id: workspace.clone(),
                        active_workspace: Some(workspace.clone()),
                        base_revision: base_revision.clone(),
                        revision_id: base_revision.clone(),
                        idempotency_key: idempotency_key.clone(),
                        cone: cone.clone(),
                    },
                );
            }
            OpKind::CheckpointChange {
                id,
                workspace,
                revision,
                ..
            } => {
                self.workspaces.insert(workspace.clone(), revision.clone());
                self.changes
                    .get_mut(id)
                    .expect("validate proved the change exists and is active")
                    .revision_id = revision.clone();
            }
            OpKind::ArchiveChange { id, workspace, .. } => {
                self.workspaces.remove(workspace);
                self.changes
                    .get_mut(id)
                    .expect("validate proved the change exists and is active")
                    .active_workspace = None;
            }
            OpKind::BindKey {
                operator,
                key,
                channel,
            } => {
                let bound_at = self.next_seq;
                self.bindings
                    .entry(key.to_hex())
                    // A re-binding may correct the channel and nothing
                    // else. `bound_at` is deliberately untouched here:
                    // that omission is the age clock.
                    .and_modify(|bound| bound.channel = channel.clone())
                    .or_insert_with(|| KeyBinding {
                        operator: operator.clone(),
                        channel: channel.clone(),
                        bound_at,
                        revoked: None,
                    });
            }
            OpKind::RevokeKey { key, reason } => {
                let at = self.next_seq;
                self.bindings
                    .get_mut(&key.to_hex())
                    .expect("validate proved the key is bound and not yet revoked")
                    .revoked = Some(Revocation {
                    at,
                    reason: reason.clone(),
                });
            }
            OpKind::RecordRefSnapshot { snapshot } => {
                self.latest_snapshot = Some(snapshot.clone());
            }
        }
        // Only a successful apply advances the fold position, so the
        // counter counts ops that are actually in the log. `validate`
        // returned above on every rejection, leaving it untouched.
        self.next_seq += 1;
        Ok(())
    }

    /// The snapshot attesting this view's current ref-state, chained to
    /// the latest admitted one — the value [`OpKind::RecordRefSnapshot`]
    /// admits as long as nothing lands in between (its `at_seq` is the
    /// CAS: any interleaved op moves the fold position and the emitter
    /// re-takes rather than attesting a state it did not read).
    #[must_use]
    pub fn snapshot(&self) -> RefSnapshot {
        RefSnapshot {
            format_version: FORMAT_VERSION,
            refs: self.refs.clone(),
            at_seq: self.next_seq,
            prev_snapshot: self.latest_snapshot.as_ref().map(RefSnapshot::id),
        }
    }

    /// The actor key the log binds to reviewer channel `channel`.
    ///
    /// The inverse of [`KeyBinding::channel`], and the join that lets an
    /// authorization record name approvers as actor ids when a review can
    /// only name channels (see [`Authorization::approvers`]).
    ///
    /// Resolved **as of fold position `at`** (D44), which for an approver
    /// is the position of the verdict being credited
    /// ([`VerdictState::at`]) rather than the position of the landing.
    ///
    /// Asking "now" is the bug this replaced. A channel's key rotates:
    /// `BindKey` tells the holder of a revoked key to bind a fresh one,
    /// and the fresh one necessarily claims the same channel, because the
    /// channel is the operator's own name. So resolving at landing time
    /// had two failures at once — with the withdrawn row still counted it
    /// left the channel permanently ambiguous and refused every later
    /// landing citing that reviewer, and with the withdrawn row ignored it
    /// would name the fresh key as the approver of a verdict that key
    /// never cast. Neither is a record worth keeping. The verdict's own
    /// position has one answer and it is the true one.
    ///
    /// Contrast [`View::operator_of`], which answers for a revoked key
    /// unconditionally: that is attribution, and attribution must not go
    /// blind the moment a key is withdrawn.
    ///
    /// **The liveness half is exact; the channel half is the latest one.**
    /// `bound_at` and [`Revocation::at`] are both recorded, so whether a
    /// key was live at `at` replays exactly. Which channel it claimed is
    /// read from the current binding, because a re-binding overwrites
    /// [`KeyBinding::channel`] in place and the fold keeps no history of
    /// it. A re-binding cannot change the operator (that is refused) and
    /// the channel must read as the operator, so the drift this permits is
    /// `ana` → `ana/laptop` within one operator, never across two. An
    /// exact answer would need [`View::at`] re-folded to `at`, which this
    /// method deliberately does not do: it is called from `validate`,
    /// which has no log.
    ///
    /// # Errors
    ///
    /// Returns a caller-facing reason when the log binds no key to
    /// `channel`, none that was live at `at`, or more than one that was.
    /// **Ambiguity is refused, not resolved.** Two keys may legitimately
    /// be live on one channel at once, and nothing in a review says which
    /// of them cast the verdict, so picking one would put a specific key
    /// in a durable record on the strength of a tiebreak. A record that
    /// says nothing is recoverable; one that says the wrong thing
    /// confidently is not.
    ///
    /// "Nothing live at `at`" is its own message rather than folding into
    /// "no binding": the repairs differ. One needs the operator to bind a
    /// key; the other needs a fresh verdict from a fresh key, because the
    /// approval on file was cast by a key nobody trusts now.
    pub fn bound_actor_at(&self, channel: &str, at: u64) -> Result<ContentHash, String> {
        let claims = |bound: &KeyBinding| bound.channel.as_deref() == Some(channel);
        let live_then = |bound: &KeyBinding| {
            bound.bound_at <= at && bound.revoked.as_ref().is_none_or(|gone| gone.at > at)
        };
        let mut live = self
            .bindings
            .iter()
            .filter(|(_, bound)| claims(bound) && live_then(bound));
        let key_id = match live.next() {
            Some((key_id, _)) => key_id,
            None if self.bindings.values().any(claims) => {
                return Err(format!(
                    "no key the log binds to {channel} was live at seq {at}; that reviewer \
                     needs a fresh key and a fresh verdict"
                ))
            }
            None => return Err(format!("the log binds no key to {channel}")),
        };
        if live.next().is_some() {
            return Err(format!(
                "the log binds more than one live key to {channel} at seq {at}"
            ));
        }
        ContentHash::from_hex(key_id)
            .ok_or_else(|| format!("binding for {channel} holds an unreadable key id {key_id}"))
    }

    /// The operator `key` is bound to, if the log ever bound it.
    ///
    /// Deliberately still answers for a **revoked** key. Revocation
    /// withdraws authority going forward; it does not un-attribute what
    /// the key already did, and an audit that lost the operator the
    /// moment a key was revoked would go blind exactly when it matters.
    /// Authorization must therefore also consult
    /// [`KeyBinding::is_revoked`]; attribution must not.
    #[must_use]
    pub fn operator_of(&self, key: &ContentHash) -> Option<&str> {
        self.bindings
            .get(&key.to_hex())
            .map(|bound| bound.operator.as_str())
    }

    /// Ops sequenced since `key` was **first** bound, measured at this
    /// view's fold position.
    ///
    /// Named for what it counts. This is an ordering and activity
    /// primitive: it says a key has been bound across N sequenced ops,
    /// and it is monotonic, replayable and unresettable. It is **not** a
    /// wall clock, and it must not be relabelled as one — ops are not
    /// uniformly spaced in time, so a question phrased in days or weeks
    /// (D24 T1's `<2 weeks` branch) still needs a durable timestamp this
    /// crate does not have. A pure fold has no clock, the same reason
    /// [`OpKind::ArchiveReview`]'s lapse decision is made node-side and
    /// merely recorded here.
    #[must_use]
    pub fn ops_since_binding(&self, key: &ContentHash) -> Option<u64> {
        self.bindings
            .get(&key.to_hex())
            .map(|bound| self.next_seq.saturating_sub(bound.bound_at))
    }

    /// Every key currently bound to `operator`, revoked ones included, in
    /// key-id order.
    ///
    /// Revoked keys stay in the count because the question T3 asks — how
    /// concentrated is control — is not answered by a number an operator
    /// can lower by revoking keys it no longer needs. Callers wanting
    /// only live keys filter on [`KeyBinding::is_revoked`].
    pub fn operator_keys<'a>(
        &'a self,
        operator: &'a str,
    ) -> impl Iterator<Item = (&'a str, &'a KeyBinding)> + 'a {
        self.bindings
            .iter()
            .filter(move |(_, bound)| bound.operator == operator)
            .map(|(key_id, bound)| (key_id.as_str(), bound))
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
        channel: submitter.to_string(),
        payload: op.to_payload(),
        witnesses: Vec::new(),
        author_sig: None,
    };
    log.append(entry).map_err(ViewError::Log)
}

/// [`append_op`], plus the resolution-link check a store makes possible:
/// a head-moving op whose commit the store holds is refused when that
/// commit's [`Commit::resolves`] link is dangling or names a commit with
/// no conflict to resolve.
///
/// This is admission policy, not part of the pure fold — the same split
/// as the node's scope and provenance checks: [`View::apply`] stays a
/// store-free function so replicas can replay a log with no store at
/// hand, and every op admitted here still replays cleanly through it.
/// A commit the store cannot supply is skipped, not refused: refs on the
/// git-compat path carry git oids that never enter the chunk store
/// (invariant 2), and a resolves link cannot exist on a commit that
/// cannot be loaded.
///
/// # Errors
///
/// [`ViewError::Resolution`] for an invalid link, plus everything
/// [`append_op`] returns.
pub fn append_op_with_store(
    log: &mut dyn OpLog,
    store: &dyn ChunkStore,
    submitter: &str,
    op: ViewOp,
) -> Result<ContentHash, ViewError> {
    let named = match &op.kind {
        OpKind::SetWorkspaceHead { commit, .. } | OpKind::SetRef { commit, .. } => Some(commit),
        _ => None,
    };
    if let Some(id) = named {
        if let Ok(commit) = Commit::get(store, id) {
            commit.validate_resolves(store)?;
        }
    }
    append_op(log, submitter, op)
}
