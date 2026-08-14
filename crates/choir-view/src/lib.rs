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
use choir_oplog::{LogError, OpEntry, OpLog, Witness};
use choir_store::{ChunkStore, StoreError};
use serde::{Deserialize, Serialize};

/// Current view-op and commit wire-format version. Bump on any
/// incompatible change; additive changes keep the version (plan.md §E).
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
        }
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
        }
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

/// Materialized state of one review: what is under review, who was
/// asked, who has answered what, and what was said about it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReviewState {
    /// The commit under review.
    pub target: Option<ContentHash>,
    /// Actors the review fanned out to.
    pub reviewers: Vec<String>,
    /// reviewer → (verdict, note); absent = not answered yet.
    pub verdicts: BTreeMap<String, (Verdict, String)>,
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
}

impl ReviewState {
    fn operator_is_slashed(&self, reviewer: &str) -> bool {
        let operator = reviewer_operator(reviewer);
        self.slashes
            .keys()
            .any(|slashed| reviewer_operator(slashed) == operator)
    }

    fn live_approval_weight(&self, apply_slashes: bool) -> usize {
        self.verdicts
            .iter()
            .enumerate()
            .filter(|(index, (reviewer, (verdict, _)))| {
                if *verdict != Verdict::Approve
                    || (apply_slashes && self.operator_is_slashed(reviewer))
                {
                    return false;
                }
                let operator = reviewer_operator(reviewer);
                self.verdicts
                    .iter()
                    .take(*index)
                    .all(|(prior, (prior_verdict, _))| {
                        *prior_verdict != Verdict::Approve
                            || reviewer_operator(prior) != operator
                    })
            })
            .count()
            * MAX_APPROVAL_WEIGHT_PER_OPERATOR
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
        !self.reviewers.is_empty()
            && self.reviewers.iter().all(|r| self.verdicts.contains_key(r))
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
            .is_some_and(|(verdict, _)| *verdict == Verdict::Approve)
            && eligible.all(|reviewer| {
                self.verdicts
                    .get(reviewer)
                    .is_some_and(|(verdict, _)| *verdict == Verdict::Approve)
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
            return approval_weight.saturating_sub(
                self.slashed_operator_count() * MAX_APPROVAL_WEIGHT_PER_OPERATOR,
            );
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
    /// when it is a resolution — Pijul's resolution-as-linked-change,
    /// as metadata on the existing shape (plan.md D15: never a new merge
    /// substrate). A conflict is a value (invariant 6): the link points
    /// *at* the conflicted commit, which stays in history untouched.
    /// Additive (`default` + `skip_serializing_if`), so commits written
    /// before the field existed decode as `None` and re-serialize
    /// byte-identically — invariant 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolves: Option<ContentHash>,
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
                    return Err(ViewError::Review(format!("review {id} is already assigned")));
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
                            .is_some_and(|(verdict, _)| *verdict == Verdict::Approve)
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
                    return Err(ViewError::Review(format!("review {id} is already archived")));
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
            OpKind::RecordProvenance { subject, kind, .. } => {
                if subject.is_empty() || kind.is_empty() {
                    return Err(ViewError::Provenance(
                        "provenance subject and kind must be non-empty".to_string(),
                    ));
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
                    Some(bound) if bound.operator != *operator => Err(ViewError::Identity(format!(
                        "key {} is already bound to operator {}",
                        key.to_hex(),
                        bound.operator
                    ))),
                    Some(_) => Ok(()),
                }
            }
            OpKind::RevokeKey { key, reason } => {
                if reason.is_empty() {
                    return Err(ViewError::Identity(
                        "a revocation must carry a non-empty reason".to_string(),
                    ));
                }
                let bound = self
                    .bindings
                    .get(&key.to_hex())
                    .ok_or_else(|| ViewError::Identity(format!("key {} is not bound", key.to_hex())))?;
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
            }
            OpKind::PostVerdict {
                id,
                reviewer,
                verdict,
                note,
            } => {
                self.reviews
                    .get_mut(id)
                    .expect("validate proved the review exists, is live, and lists this reviewer")
                    .verdicts
                    .insert(reviewer.clone(), (*verdict, note.clone()));
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
            OpKind::RecordProvenance { subject, kind, body } => {
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
