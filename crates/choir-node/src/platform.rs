//! The platform API: signed op submission and view queries over HTTP.
//!
//! This is the production composition the sequencer's `policy.rs` test
//! proved: signature verification (choir-identity) + cached-view CAS
//! (choir-view) running inside the single-writer thread. The daemon fronts
//! it with signed single and batch submission, materialized-view and log
//! reads, workspace provisioning, and review-queue endpoints. The two core
//! operation paths are:
//!
//! - `POST /api/submit` — body `{"channel", "payload_hex",
//!   "key_id", "signature_hex"}`; `workspace` remains accepted as the
//!   legacy v1 alias for `channel`. Payload bytes are a serialized
//!   [`ViewOp`]. Admitted ops answer `{"seq", "hash"}`; rejections are
//!   HTTP 400 with the policy's reason.
//! - `GET /api/view` — the current materialized view as
//!   `{"workspaces": {name: id}, "refs": {name: id}}`.
//!
//! Hex (not JSON-embedding) carries the payload because the signature
//! covers the exact bytes the author serialized; re-encoding through a
//! JSON tree could legally reorder/respace them and break verification.

use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use choir_identity::{ActorKey, Registry};
use choir_oplog::{ContentHash, OpEntry, OpLog, Witness};
use choir_sequencer::{Sequencer, SequencerHandle, Submission, SubmitPolicy};
use choir_view::{
    reviewer_operator, ArchiveAuthorization, ChangeState, OpKind, View, ViewOp,
};

use crate::reject::{Code, Rejection};

/// Operator-selected bound for live review detail retained in memory.
///
/// Complete reviews older than `max_live` are archived immediately. An
/// incomplete review is never archived unless `lapse_after` is explicitly
/// set, because choosing when an unanswered review is abandoned is policy,
/// not a harmless memory optimization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReviewRetention {
    max_live: usize,
    lapse_after: Option<Duration>,
}

impl ReviewRetention {
    /// Retains at most `max_live` live reviews when enough reviews are
    /// complete and therefore safe to archive. Incomplete reviews never
    /// lapse under this configuration.
    #[must_use]
    pub const fn keep(max_live: usize) -> Self {
        Self {
            max_live,
            lapse_after: None,
        }
    }

    /// Allows an over-limit incomplete review to lapse after `age` since
    /// this node observed its request.
    ///
    /// The op log has no timestamp, so reviews replayed at startup begin a
    /// fresh grace period. Restarting can delay a lapse, never make one
    /// happen early. The emitted `ArchiveReview { lapsed: true }` op is
    /// persisted, so replicas replay the decision rather than their clocks.
    #[must_use]
    pub const fn lapse_incomplete_after(mut self, age: Duration) -> Self {
        self.lapse_after = Some(age);
        self
    }
}

/// One live review in request-sequence order. `observed_at` is consulted
/// only for the explicitly configured incomplete-review lapse policy.
struct TrackedReview {
    id: String,
    observed_at: Option<Instant>,
}

/// Runtime state that exists only when retention was explicitly enabled.
/// Archived ids are removed, so the tracker grows with live review detail,
/// not with the repository's lifetime.
struct ReviewRetentionState {
    config: ReviewRetention,
    live: VecDeque<TrackedReview>,
    /// Set when an observed op may have changed what is prunable, cleared
    /// by a pass. Without it the submit path rescans the whole tracker on
    /// every write for as long as the node holds more live reviews than
    /// the bound and none of them is archivable — which is the *default*
    /// shape, because incomplete reviews never lapse unless an age is
    /// configured. Measured at 4.4x on the submit path before this
    /// existed (1,000 unanswered reviews, `--review-retention 10`).
    prunable_changed: bool,
}

impl ReviewRetentionState {
    fn new(config: ReviewRetention) -> Self {
        Self {
            config,
            live: VecDeque::new(),
            prunable_changed: false,
        }
    }

    fn observe(&mut self, op: &ViewOp, observed_at: Option<Instant>) {
        match &op.kind {
            OpKind::RequestReview { id, .. } => {
                self.live.push_back(TrackedReview {
                    id: id.clone(),
                    observed_at,
                });
                self.prunable_changed = true;
            }
            OpKind::ArchiveReview { id, .. } => {
                self.live.retain(|review| review.id != *id);
                self.prunable_changed = true;
            }
            // The ops that cannot move a review's count or completeness,
            // named as an exclusion rather than listing the review ops
            // positively: a review op added later then defaults to arming
            // the flag — a wasted scan, never silently stopped pruning.
            OpKind::SetWorkspaceHead { .. }
            | OpKind::SetRef { .. }
            | OpKind::DeleteRef { .. }
            | OpKind::DeleteWorkspace { .. }
            | OpKind::RecordProvenance { .. }
            | OpKind::CreateChange { .. }
            | OpKind::CheckpointChange { .. }
            | OpKind::ArchiveChange { .. } => {}
            _ => self.prunable_changed = true,
        }
    }

    /// Whether a pass could possibly find work. Deliberately conservative:
    /// it may say yes when the answer turns out to be no, never no when
    /// the answer is yes.
    fn worth_a_pass(&self) -> bool {
        // Tracked ids are dropped on archive, so this is an upper bound on
        // the live count: at or under the bound, no pass can find work.
        if self.live.len() <= self.config.max_live {
            return false;
        }
        if self.prunable_changed {
            return true;
        }
        // Under a lapse policy the clock alone can make the oldest review
        // eligible with no op arriving. The deque is in request order, so
        // the front carries the earliest deadline and one comparison
        // settles it. Count-only retention still reads no clock.
        match (self.config.lapse_after, self.live.front()) {
            (Some(age), Some(oldest)) => oldest
                .observed_at
                .is_some_and(|at| Instant::now().saturating_duration_since(at) >= age),
            _ => false,
        }
    }
}

/// Result of maintenance emitted after an already-successful user request.
/// A retention failure cannot roll that request back, so it is reported as
/// an additive response field rather than changing the request's status.
#[derive(Default)]
struct ReviewPruneOutcome {
    archived: Vec<String>,
    errors: Vec<serde_json::Value>,
}

/// Sliding window over recent admitted entries: `base` is the seq of
/// the first held entry, so `/api/log?from=` keeps absolute semantics
/// after old entries are dropped (full history lives in the op log).
pub struct LogWindow {
    base: u64,
    /// A `VecDeque`, not a `Vec`: eviction pops from the front, which is
    /// O(1) here and an O(n) memmove of up to `cap` entries there. At the
    /// 100k cap that cost was paid on every push once full.
    entries: std::collections::VecDeque<OpEntry>,
    /// Entries kept before the oldest is dropped. A field rather than a
    /// constant so a test can drive eviction without writing 100k ops.
    cap: usize,
    /// `signing_hash(channel, payload)` → the `(seq, hash)` it landed
    /// as, for the entries currently in the window.
    ///
    /// Lets a resubmission be told "this already landed, here is where"
    /// instead of the CAS failure it would otherwise see — the two are
    /// indistinguishable today, and an agent that cannot tell them apart
    /// either retries a completed write or abandons a successful one.
    ///
    /// **Bounded by the window, deliberately.** An index over the whole
    /// log would grow with the repository's lifetime, which is the exact
    /// growth profile `FileLog`'s offset index was just built to remove.
    /// A window is enough because duplicate submissions come from client
    /// retries — seconds apart, not months — so anything old enough to
    /// have fallen out of the window is not a retry.
    ///
    /// A `HashMap`, and that is safe here specifically because this is
    /// runtime state that is never serialized or hashed. Invariant 3 —
    /// "maps in hashed structs are `BTreeMap` so serialization stays
    /// canonical" — applies to persisted structures; an in-memory index
    /// has no canonical form to break. Do not "fix" this to a `BTreeMap`
    /// for consistency: `ContentHash` is `Hash` but not `Ord`.
    /// Stores the **seq only**. The entry hash is recoverable from the
    /// window when the rare already-applied path needs it, and computing
    /// it here would re-serialize every entry on the write path — the
    /// allocation budget caught exactly that.
    by_signing: std::collections::HashMap<ContentHash, u64>,
}

/// Entries retained in memory for `/api/log`; older reads are served from
/// the persisted op log when the node has one.
const LOG_WINDOW_CAP: usize = 100_000;

/// Entries per `/api/log` page, whichever source served them.
const LOG_PAGE: usize = 500;

/// Approval weight required to move a protected ref. The assignment draw
/// targets the same number of independent operators, but a thin pool may
/// return fewer; under-assignment stays visible and cannot lower this
/// landing threshold (D24 layer 5).
const REQUIRED_APPROVAL_WEIGHT: usize = 2;

/// Nanosecond clock reading, as a nonzero xorshift seed.
fn seed_from_clock() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0x9e37_79b9_7f4a_7c15, |d| d.as_nanos() as u64)
        | 1
}

/// FNV-1a, so two reviews drawn in the same nanosecond still diverge.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100_0000_01b3)
    })
}

impl LogWindow {
    fn push(&mut self, entry: OpEntry) {
        // `signing_hash` covers only (channel, payload); `content_hash`
        // would serialize the whole entry, and this runs per admitted op.
        self.by_signing.insert(entry.signing_hash(), entry.seq);
        self.entries.push_back(entry);
        self.trim();
    }

    /// The `(seq, hash)` an identical submission already landed as.
    ///
    /// Keyed on the **signing** hash, not the entry hash: an entry's hash
    /// covers `seq` and `parent`, which the sequencer assigns after the
    /// author signs, so a resubmitted identical op hashes differently
    /// every time and could never match. `signing_hash(channel,
    /// payload)` is position-independent by construction, which is
    /// exactly the identity "the same submission" needs.
    fn already_applied(&self, channel: &str, payload: &[u8]) -> Option<(u64, ContentHash)> {
        let seq = *self
            .by_signing
            .get(&choir_oplog::signing_hash(channel, payload))?;
        // Hash the entry only on a hit, which is a client retry rather
        // than the common path.
        let entry = self.entries.get(seq.checked_sub(self.base)? as usize)?;
        Some((seq, entry.content_hash()))
    }

    /// Drops the oldest entries until the window fits its cap, advancing
    /// `base` so `/api/log?from=` keeps absolute sequence semantics.
    fn trim(&mut self) {
        while self.entries.len() > self.cap {
            if let Some(dropped) = self.entries.pop_front() {
                // Evict from the index with the entry, or the map becomes
                // the unbounded thing the window exists to avoid.
                self.by_signing.remove(&dropped.signing_hash());
            }
            self.base += 1;
        }
    }
}

/// Replays once while recovering the request order needed by retention.
/// The ordinary, retention-disabled startup keeps using `View::materialize`
/// and pays for no tracker or extra work.
fn materialize_with_review_retention(
    log: &dyn OpLog,
    config: ReviewRetention,
) -> Result<(View, ReviewRetentionState), choir_view::ViewError> {
    let mut view = View::default();
    let mut retention = ReviewRetentionState::new(config);
    // Stored entries have no timestamp. Giving every pre-existing live
    // review `now` starts a fresh grace period after restart, which can
    // delay an incomplete-review lapse but can never trigger one early.
    let observed_at = config.lapse_after.map(|_| Instant::now());
    for seq in 0..log.len() {
        let entry = log.get(seq).expect("seq < len");
        let op = ViewOp::from_payload(&entry.payload)?;
        view.apply(&op)?;
        retention.observe(&op, observed_at);
    }
    Ok((view, retention))
}

/// Verify author signature, then CAS against the shared view. Runs on
/// the sequencer's writer thread; API readers share the view mutex.
struct ChoirPolicy {
    registry: Registry,
    view: Arc<Mutex<View>>,
    entries: Arc<Mutex<LogWindow>>,
    /// When set, the trusted-keys file is re-read after a failed
    /// signature check if its mtime moved — registering a key becomes
    /// "append a line", no daemon restart.
    keys_file: Option<std::path::PathBuf>,
    keys_mtime: Option<std::time::SystemTime>,
    node_pub: Vec<u8>,
    /// The node key's actor id: the only author allowed to assign reviewers.
    node_id: ContentHash,
    /// When set, a `RequestReview` may not name its own reviewers —
    /// every review must go through the node's draw (D24 layer 5).
    /// Shared with [`Platform`] so the switch is one value, not two.
    require_assignment: Arc<std::sync::atomic::AtomicBool>,
    /// Operator's protected-ref list: the same switch, but conditioned on
    /// where the review proposes to land. Shared with [`Platform`].
    protected_refs: Arc<Mutex<Option<std::path::PathBuf>>>,
    /// When set, a protected ref only moves to a commit some approved
    /// review already named — the landing half of the gate.
    require_review: Arc<std::sync::atomic::AtomicBool>,
    /// Actor id → the channel name that key is bound to,
    /// for keys whose trusted-keys line carries a name. Keys absent from
    /// this map are unconstrained, which is what every key was before the
    /// name column existed.
    ///
    /// Shared with [`Platform`], because a *tightening* must not wait for
    /// an unrelated event: the accept loop refreshes it on mtime change,
    /// while the failed-signature path below refreshes it too.
    key_names: Arc<Mutex<std::collections::HashMap<ContentHash, String>>>,
    /// Present only when the operator enabled review retention. Updated
    /// after the view fold on the same writer thread, so its FIFO order
    /// matches the sequencer order exactly.
    review_retention: Option<Arc<Mutex<ReviewRetentionState>>>,
}

impl ChoirPolicy {
    /// Rebuilds the registry from the keys file iff its mtime changed
    /// since the last (re)load. Returns whether a reload happened.
    fn reload_keys(&mut self) -> bool {
        let Some(path) = &self.keys_file else {
            return false;
        };
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        if mtime.is_none() || mtime == self.keys_mtime {
            return false;
        }
        // Same parser startup uses, so the two cannot drift — and a
        // malformed file keeps the previous registry *and* the previous
        // name bindings rather than half-applying either.
        let Ok(signers) = crate::parse_keys_file(path) else {
            self.keys_mtime = mtime;
            return false;
        };
        let mut registry = Registry::new();
        if let Ok(node_pub) = <[u8; 32]>::try_from(self.node_pub.as_slice()) {
            registry.register(&node_pub).ok();
        }
        let mut key_names = std::collections::HashMap::new();
        for signer in &signers {
            if let (Ok(actor_id), Some(name)) = (registry.register(&signer.key), &signer.name) {
                key_names.insert(actor_id, name.clone());
            }
        }
        self.registry = registry;
        *self.key_names.lock().expect("key names lock") = key_names;
        self.keys_mtime = mtime;
        true
    }

    /// Enforces the name a key is bound to, for ops whose submission
    /// channel is an identity claim rather than a workspace name.
    ///
    /// Scope is deliberately narrow. `sub.channel` is the
    /// signature-covered attribution/identity channel, not a workspace id.
    /// The v1 wire field was named `workspace`; the daemon also uses
    /// synthetic push-attribution channels
    /// (`git/<user>`, `key/<principal>`). Binding every channel would
    /// break workspace provisioning and every git-derived op.
    ///
    /// A key with no bound name is unconstrained — exactly its behaviour
    /// before the name column existed — so this cannot break a running
    /// node, and the operator opts in one line at a time.
    fn channel_is_owned(&self, actor_id: &ContentHash, channel: &str) -> Result<(), String> {
        let names = self.key_names.lock().expect("key names lock");
        match names.get(actor_id) {
            Some(bound) if bound != channel => Err(Rejection::new(
                Code::ChannelNotOwned,
                "this key is bound to a different channel",
                "submit on the channel your key is bound to, or ask the operator to bind a \
                 key to the channel you want",
            )
            .with_states(Some(bound.clone()), Some(channel.to_string()))
            .encode()),
            _ => Ok(()),
        }
    }

    /// Whether `name` matches the operator's protected-ref list. One
    /// pattern per line, `#` comments allowed, exact match or a single
    /// trailing `*` prefix glob (`repo.git:refs/heads/release/*`).
    ///
    /// Read per call rather than cached, so editing the list takes effect
    /// with no restart. A `RequestReview` always reaches here; a
    /// `SetRef`/`DeleteRef` only under `--require-review`, which is the
    /// mode that also puts a file read on the git-push path. If push
    /// throughput ever notices, an mtime-stat cache is the fix — measure
    /// before adding one.
    ///
    /// Fails **closed**: an unreadable list refuses the op instead of
    /// quietly demoting a gate to an advisory.
    fn ref_is_protected(&self, name: &str) -> Result<bool, String> {
        let guard = self.protected_refs.lock().expect("protected refs lock");
        let Some(path) = guard.as_ref() else {
            return Ok(false);
        };
        let text = std::fs::read_to_string(path)
            .map_err(|e| {
                Rejection::new(
                    Code::PolicyUnavailable,
                    format!("protected-ref list unreadable: {e}"),
                    "this is an operator problem, not a client one: the gate fails closed \
                     rather than guessing. Retry after the operator restores the file",
                )
                .encode()
            })?;
        Ok(text.lines().map(str::trim).any(|p| {
            if p.is_empty() || p.starts_with('#') {
                return false;
            }
            match p.strip_suffix('*') {
                Some(prefix) => name.starts_with(prefix),
                None => name == p,
            }
        }))
    }

    /// The greatest capped approval weight of any approved review that
    /// named exactly this `(ref, commit)` pair as where it wanted to land.
    ///
    /// Both halves matter. Matching only the commit would let an approval
    /// for a scratch branch land the same commit on `main`; matching only
    /// the ref would let any approved review authorize any later commit.
    fn approval_weight_for(&self, name: &str, commit: &ContentHash) -> usize {
        self.view
            .lock()
            .expect("view lock")
            .reviews
            .values()
            .filter(|r| {
                r.target_ref.as_deref() == Some(name)
                    && r.target.as_ref() == Some(commit)
                    && r.approved()
            })
            .map(choir_view::ReviewState::approval_weight)
            .max()
            .unwrap_or(0)
    }
}

impl SubmitPolicy for ChoirPolicy {
    fn check(&mut self, sub: &Submission) -> Result<(), String> {
        let sig = sub.author_sig.as_ref().ok_or("unsigned submission")?;
        let mut verified_actor = self
            .registry
            .verify_submission(&sub.channel, &sub.payload, sig);
        // Unknown/failed key: maybe the operator just registered it.
        if verified_actor.is_err() && self.reload_keys() {
            verified_actor = self
                .registry
                .verify_submission(&sub.channel, &sub.payload, sig);
        }
        let actor_id = verified_actor.map_err(|e| {
            Rejection::new(
                Code::UnknownKey,
                format!("signature check failed: {e:?}"),
                "ask the operator to add your public key to the node's trusted-keys file                  (`choir key <file> <you>` prints the line); it takes effect on the next request",
            )
            .encode()
        })?;
        let op = ViewOp::from_payload(&sub.payload).map_err(|e| {
            Rejection::new(
                Code::MalformedOp,
                format!("payload did not decode as a ViewOp: {e:?}"),
                "sign the bytes of a serialized ViewOp; `choir submit` does this correctly",
            )
            .encode()
        })?;
        // A verdict's claimed reviewer must be the signature-covered
        // submission channel: the log's author attribution and the
        // view's verdict attribution can never diverge.
        if let OpKind::PostVerdict { reviewer, .. } = &op.kind {
            if *reviewer != sub.channel {
                return Err(Rejection::new(
                    Code::ReviewerMismatch,
                    "a verdict's reviewer must be the channel it was signed on",
                    "resubmit on your own channel: `choir verdict` signs on the reviewer name                      by construction",
                )
                .with_states(Some(sub.channel.clone()), Some(reviewer.clone()))
                .encode());
            }
        }
        // ...and the channel itself must belong to the signing key, or
        // the check above only proves a claim is self-consistent, not
        // that it is true. Review ops only: see `channel_is_owned`.
        if matches!(
            op.kind,
            OpKind::PostVerdict { .. } | OpKind::RequestReview { .. }
        ) {
            self.channel_is_owned(&actor_id, &sub.channel)?;
        }
        // D24 layer 5: the requester does not choose who reviews them.
        // Only the daemon's own key may fill in a reviewer list; every
        // other author gets a rejection, so an accepted assignment in
        // the log always came from the node's pool draw.
        if matches!(op.kind, OpKind::AssignReviewers { .. }) && actor_id != self.node_id {
            return Err(Rejection::new(
                Code::NodeOnly,
                "only the node may assign reviewers",
                "request a review with an empty reviewer list and the node will draw them",
            )
            .encode());
        }
        // Archiving drops a review's verdicts, so an unguarded one is a
        // way to erase a RequestChanges you did not like. Node key only,
        // same reasoning as assignment: it is retention, not review.
        if matches!(op.kind, OpKind::ArchiveReview { .. }) && actor_id != self.node_id {
            return Err(Rejection::new(
                Code::NodeOnly,
                "only the node may archive reviews",
                "nothing to do: archiving is retention, performed by the node",
            )
            .encode());
        }
        // Stable change creation is coupled to physical workspace
        // provisioning. Only the node may record it, after the exact base
        // was verified and the checkout was materialized.
        if matches!(
            op.kind,
            OpKind::CreateChange { .. } | OpKind::ArchiveChange { .. }
        ) && actor_id != self.node_id
        {
            return Err(Rejection::new(
                Code::NodeOnly,
                "only the node may author physical workspace lifecycle operations",
                "call POST /api/workspace to create, or POST /api/workspace/archive with an owner-signed authorization to archive",
            )
            .encode());
        }
        if let OpKind::CheckpointChange { id, .. } = &op.kind {
            let owner = self
                .view
                .lock()
                .expect("view lock")
                .changes
                .get(id)
                .map(|change| change.owner.clone());
            if let Some(owner) = owner {
                if owner != sub.channel {
                    return Err(Rejection::new(
                        Code::ChannelNotOwned,
                        format!("change {id} is owned by a different channel"),
                        "sign the change operation on the owner channel reported in GET /api/view, or \
                         create a separate change",
                    )
                    .with_states(Some(owner), Some(sub.channel.clone()))
                    .encode());
                }
                self.channel_is_owned(&actor_id, &sub.channel)?;
            }
        }
        if let OpKind::ArchiveChange {
            id,
            workspace,
            prev_revision,
            owner,
            owner_sig,
        } = &op.kind
        {
            let authorization = ArchiveAuthorization::new(
                id.clone(),
                workspace.clone(),
                prev_revision.clone(),
            )
            .to_payload();
            let mut verified_owner = self
                .registry
                .verify_submission(owner, &authorization, owner_sig);
            if verified_owner.is_err() && self.reload_keys() {
                verified_owner = self
                    .registry
                    .verify_submission(owner, &authorization, owner_sig);
            }
            let owner_actor = verified_owner.map_err(|error| {
                Rejection::new(
                    Code::UnknownKey,
                    format!("archive owner signature check failed: {error:?}"),
                    "register the owner key, then retry the same signed archive authorization",
                )
                .encode()
            })?;
            self.channel_is_owned(&owner_actor, owner)?;
        }
        // Legacy workspace moves and deletes remain compatible for
        // legacy workspaces. Once a workspace is enrolled in a stable
        // change, non-node callers must use CheckpointChange and the
        // recoverable archive endpoint so identity cannot be bypassed.
        let bound_workspace = match &op.kind {
            OpKind::SetWorkspaceHead { workspace, .. }
            | OpKind::DeleteWorkspace { workspace } => self
                .view
                .lock()
                .expect("view lock")
                .changes
                .values()
                .any(|change| change.active_workspace.as_deref() == Some(workspace)),
            _ => false,
        };
        if bound_workspace && actor_id != self.node_id {
            return Err(Rejection::new(
                Code::WorkspaceState,
                "a change-bound workspace cannot be moved or removed through a legacy op",
                "use choir checkpoint to publish a revision, or choir workspace-archive to \
                 archive the bound workspace",
            )
            .encode());
        }
        // Required-assignment closes the other half of the same loop:
        // naming your own reviewers is refused, so the node's draw is the
        // only way a review gets reviewers. Two ways to switch it on —
        // node-wide, or per-ref once the review says where it wants to
        // land. Node-wide wins because it is the stricter of the two.
        if let OpKind::RequestReview {
            reviewers,
            target_ref,
            ..
        } = &op.kind
        {
            if !reviewers.is_empty() {
                if self
                    .require_assignment
                    .load(std::sync::atomic::Ordering::Relaxed)
                {
                    return Err(Rejection::new(
                        Code::AssignmentRequired,
                        "this node assigns reviewers",
                        "resubmit the same review with an empty reviewer list; the node draws \
                         them and returns the names in the response",
                    )
                    .encode());
                }
                if let Some(name) = target_ref {
                    if self.ref_is_protected(name)? {
                        return Err(Rejection::new(
                            Code::ProtectedRef,
                            format!("{name} is a protected ref"),
                            "resubmit with an empty reviewer list; on a protected ref only a \
                             node-drawn reviewer list is accepted",
                        )
                        .encode());
                    }
                }
            }
        }
        // The landing half of the gate: a protected ref only moves to a
        // commit that some independently approved review already named
        // as its destination. This is what turns `--protected-refs` from an
        // advisory into enforcement — without it a requester escapes by
        // simply omitting `target_ref`.
        //
        // No exemption for the node's own key. Every git push arrives
        // here as a node-signed `SetRef`, so exempting the node would
        // exempt every push, which is the whole population being gated.
        if self
            .require_review
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            match &op.kind {
                OpKind::SetRef { name, commit, prev } if self.ref_is_protected(name)? => {
                    // Creating a protected ref is allowed: there is no
                    // history to hijack yet, and deletion is refused
                    // below, so "delete then re-create" is not a way in.
                    let approval_weight = self.approval_weight_for(name, commit);
                    if prev.is_some() && approval_weight < REQUIRED_APPROVAL_WEIGHT {
                        return Err(Rejection::new(
                            Code::ReviewRequired,
                            format!(
                                "{name} is protected and this commit has approval weight \
                                 {approval_weight}, below the required {REQUIRED_APPROVAL_WEIGHT}"
                            ),
                            "open a review naming this ref and commit (`choir review ... --ref \
                             <repo:ref>`), obtain approvals from two distinct operators, then \
                             push again",
                        )
                        .with_states(
                            Some(format!(
                                "approval weight {REQUIRED_APPROVAL_WEIGHT} for {}",
                                commit.to_hex()
                            )),
                            Some(format!("approval weight {approval_weight}")),
                        )
                        .encode());
                    }
                }
                OpKind::DeleteRef { name, .. } if self.ref_is_protected(name)? => {
                    return Err(Rejection::new(
                        Code::RefUndeletable,
                        format!("{name} is protected and cannot be deleted"),
                        "delete a different ref, or ask the operator to remove this one from \
                         the protected-ref list",
                    )
                    .encode());
                }
                _ => {}
            }
        }
        // Admission is a read. Every precondition `View::apply` enforces
        // is a CAS comparison or a key lookup, so this asks the shared
        // view directly instead of deep-cloning it -- four nested
        // BTreeMaps per submission, O(total state), which grew with the
        // repo's lifetime rather than with the size of the op.
        //
        // `View::apply` calls the same `validate`, so admission and
        // application cannot disagree; that shared path is what keeps
        // `accepted`'s "checked in check()" honest.
        self.view
            .lock()
            .expect("view lock")
            .validate(&op)
            .map_err(|e| crate::reject::from_view_error(&e).encode())
    }

    fn accepted(&mut self, entry: &OpEntry) {
        let op = ViewOp::from_payload(&entry.payload).expect("checked in check()");
        self.view
            .lock()
            .expect("view lock")
            .apply(&op)
            .expect("checked in check()");
        if let Some(retention) = &self.review_retention {
            let mut retention = retention.lock().expect("review retention lock");
            let observed_at = retention.config.lapse_after.map(|_| Instant::now());
            retention.observe(&op, observed_at);
        }
        self.entries
            .lock()
            .expect("entries lock")
            .push(entry.clone());
    }
}

/// A running platform: the sequencer plus the shared view it maintains.
pub struct Platform {
    handle: SequencerHandle,
    view: Arc<Mutex<View>>,
    /// The daemon's own key: signs ops it derives from authenticated git
    /// pushes. Attribution: a verified push certificate names the
    /// pusher's key (`key/<principal>`); otherwise the basic-auth user
    /// (`git/<user>`).
    node_key: ActorKey,
    entries: Arc<Mutex<LogWindow>>,
    /// Operator-curated file of eligible reviewer names, one per line.
    /// Read fresh on every draw, so editing it takes effect at once.
    /// `None` = no pool, and unassigned reviews stay unassigned.
    reviewer_pool: Option<std::path::PathBuf>,
    /// The persisted op log, for readers that have fallen behind the
    /// in-memory window. `None` (an in-memory log) means such a reader
    /// gets a loud gap error instead of a resync.
    log_path: Option<std::path::PathBuf>,
    /// Shared with the policy: when set, self-named reviewers are
    /// refused and every review goes through the node's draw.
    require_assignment: Arc<std::sync::atomic::AtomicBool>,
    /// Shared with the policy: the same refusal, but only for reviews
    /// that propose to land on a ref the operator marked protected.
    protected_refs: Arc<Mutex<Option<std::path::PathBuf>>>,
    /// Shared with the policy: when set, a protected ref only moves to a
    /// commit an approved review already named.
    require_review: Arc<std::sync::atomic::AtomicBool>,
    /// Shared with the policy: actor id → bound channel name.
    key_names: Arc<Mutex<std::collections::HashMap<ContentHash, String>>>,
    /// Sequence-ordered live reviews, allocated only under an explicit
    /// retention configuration.
    review_retention: Option<Arc<Mutex<ReviewRetentionState>>>,
    /// Serializes concurrent maintenance passes. User submissions still
    /// race normally through the sequencer; only duplicate pruning scans
    /// and archive batches are coalesced.
    review_prune_lock: Mutex<()>,
    // Kept alive for the daemon's lifetime; the writer thread exits with
    // the process.
    _sequencer: Sequencer,
}

impl Platform {
    /// Replays `log` into a view and starts the admission sequencer over
    /// it with `registry` as the trusted key set plus the daemon's own
    /// `node_key` (registered automatically, for git-derived ops).
    ///
    /// # Errors
    ///
    /// Returns a description of any replay failure (a log written
    /// through this platform always replays cleanly).
    pub fn start(
        registry: Registry,
        log: Box<dyn OpLog>,
        node_key: ActorKey,
    ) -> Result<Self, String> {
        Self::start_inner(registry, log, node_key, None, None)
    }

    /// [`Platform::start`] with explicit live-review retention.
    ///
    /// Retention is a startup choice because recovering FIFO request order
    /// belongs in the same replay pass that builds the view. The ordinary
    /// constructor allocates no tracker and performs no retention checks.
    ///
    /// # Errors
    ///
    /// Same as [`Platform::start`].
    pub fn start_with_review_retention(
        registry: Registry,
        log: Box<dyn OpLog>,
        node_key: ActorKey,
        retention: ReviewRetention,
    ) -> Result<Self, String> {
        Self::start_inner(registry, log, node_key, None, Some(retention))
    }

    /// [`Platform::start`] with a trusted-keys file that is hot-reloaded
    /// (on mtime change) whenever a signature check fails: registering a
    /// key is appending a line, no restart. The file's contents replace
    /// the whole registry on reload, so key *removal* also takes effect.
    ///
    /// # Errors
    ///
    /// Same as [`Platform::start`].
    pub fn start_reloading(
        registry: Registry,
        log: Box<dyn OpLog>,
        node_key: ActorKey,
        keys_file: Option<std::path::PathBuf>,
    ) -> Result<Self, String> {
        Self::start_inner(registry, log, node_key, keys_file, None)
    }

    /// [`Platform::start_reloading`] with explicit live-review retention.
    ///
    /// # Errors
    ///
    /// Same as [`Platform::start`].
    pub fn start_reloading_with_review_retention(
        registry: Registry,
        log: Box<dyn OpLog>,
        node_key: ActorKey,
        keys_file: Option<std::path::PathBuf>,
        retention: ReviewRetention,
    ) -> Result<Self, String> {
        Self::start_inner(registry, log, node_key, keys_file, Some(retention))
    }

    fn start_inner(
        mut registry: Registry,
        log: Box<dyn OpLog>,
        node_key: ActorKey,
        keys_file: Option<std::path::PathBuf>,
        retention: Option<ReviewRetention>,
    ) -> Result<Self, String> {
        registry
            .register(&node_key.public_key_bytes())
            .map_err(|e| format!("register node key: {e:?}"))?;
        let (view, review_retention) = match retention {
            Some(config) => {
                let (view, state) = materialize_with_review_retention(log.as_ref(), config)
                    .map_err(|e| format!("replay: {e:?}"))?;
                (view, Some(Arc::new(Mutex::new(state))))
            }
            None => (
                View::materialize(log.as_ref()).map_err(|e| format!("replay: {e:?}"))?,
                None,
            ),
        };
        let view = Arc::new(Mutex::new(view));
        // Fill the window from the tail only. Materialising the whole log
        // into a Vec and pushing each entry through the window cloned
        // every entry twice at startup and held a second full copy of the
        // log in memory alongside the log itself -- O(total ops) for a
        // window that keeps at most `cap`.
        let len = log.len();
        let start = len.saturating_sub(LOG_WINDOW_CAP as u64);
        let mut window = LogWindow {
            base: start,
            entries: std::collections::VecDeque::with_capacity(
                (len - start).min(LOG_WINDOW_CAP as u64) as usize,
            ),
            cap: LOG_WINDOW_CAP,
            by_signing: std::collections::HashMap::new(),
        };
        for i in start..len {
            if let Some(e) = log.get(i) {
                window.push(e);
            }
        }
        let entries = Arc::new(Mutex::new(window));
        let keys_mtime = keys_file
            .as_ref()
            .and_then(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
        // Name bindings are read from the same file at startup; a
        // malformed file here is not fatal because `start_reloading`
        // already accepted the caller's registry.
        let key_names = Arc::new(Mutex::new(
            keys_file
                .as_ref()
                .and_then(|p| crate::parse_keys_file(p).ok())
                .map(|signers| {
                    signers
                        .into_iter()
                        .filter_map(|s| {
                            s.name
                                .map(|name| (ContentHash::blake3(&s.key), name))
                        })
                        .collect()
                })
                .unwrap_or_default(),
        ));
        let require_assignment = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let protected_refs = Arc::new(Mutex::new(None));
        let require_review = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sequencer = Sequencer::spawn_with_policy(
            log,
            Box::new(ChoirPolicy {
                require_assignment: require_assignment.clone(),
                protected_refs: protected_refs.clone(),
                require_review: require_review.clone(),
                registry,
                view: view.clone(),
                entries: entries.clone(),
                keys_file,
                keys_mtime,
                node_pub: node_key.public_key_bytes().to_vec(),
                node_id: node_key.actor_id(),
                key_names: key_names.clone(),
                review_retention: review_retention.clone(),
            }),
        );
        let platform = Self {
            handle: sequencer.handle(),
            view,
            node_key,
            entries,
            reviewer_pool: None,
            log_path: None,
            require_assignment,
            protected_refs,
            require_review,
            key_names,
            review_retention,
            review_prune_lock: Mutex::new(()),
            _sequencer: sequencer,
        };
        // Enabling a bound applies it at startup, not only after some
        // unrelated client happens to write. There are no external handles
        // yet, so any refusal here is a real maintenance/startup failure.
        if platform.review_retention.is_some() {
            let outcome = platform.prune_reviews();
            if !outcome.errors.is_empty() {
                return Err(format!(
                    "review retention failed during startup: {}",
                    serde_json::Value::Array(outcome.errors)
                ));
            }
        }
        Ok(platform)
    }

    /// Refuses any `RequestReview` that names its own reviewers, so the
    /// node's draw is the only path to a reviewer list (D24 layer 5,
    /// "the requester does not choose who reviews them" — enforced
    /// rather than merely offered).
    ///
    /// Requires a reviewer pool: without one no review can ever be
    /// assigned, so every request would stall unassigned.
    ///
    /// Scope: node-wide, the blunt instrument. For "only reviews landing
    /// somewhere that matters", see [`Platform::with_protected_refs`],
    /// which conditions on `RequestReview`'s `target_ref`.
    #[must_use]
    pub fn with_required_assignment(self) -> Self {
        self.require_assignment
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self
    }

    /// Points the platform at an operator-curated list of protected refs
    /// (one `<repo>:<refname>` pattern per line, `#` comments allowed, a
    /// single trailing `*` acting as a prefix glob). A `RequestReview`
    /// whose `target_ref` matches must go through the node's draw; one
    /// naming an unprotected ref, or naming no ref at all, may still pick
    /// its own reviewers.
    ///
    /// This is the per-review half of D24 layer 5 — "privilege-bearing"
    /// finally has a definition the code can read, rather than the
    /// node-wide approximation of [`Platform::with_required_assignment`].
    ///
    /// Requires a reviewer pool, for the same reason.
    ///
    /// On its own this only binds *reviews*: a requester who omits
    /// `target_ref` still escapes it. [`Platform::with_required_review`]
    /// is the other half, and closes that.
    #[must_use]
    pub fn with_protected_refs(self, path: std::path::PathBuf) -> Self {
        *self.protected_refs.lock().expect("protected refs lock") = Some(path);
        self
    }

    /// A protected ref only moves to a commit that an **approved** review
    /// with weight from two distinct operators already named as its
    /// destination, and can never be deleted.
    /// This is the landing half of the gate: with it, omitting
    /// `target_ref` stops being an escape and becomes a refusal, because
    /// the push itself is what gets checked.
    ///
    /// Requires [`Platform::with_protected_refs`] — with no list nothing
    /// is protected and the flag would do nothing.
    ///
    /// **No exemption for the node's own key.** Every git push reaches the
    /// sequencer as a node-signed `SetRef`, so exempting the node would
    /// exempt every push. The consequence is deliberate and operational:
    /// switching this on means this daemon's *own* repository can only be
    /// advanced through a review, `choirctl sync` included.
    ///
    /// Creating a protected ref is allowed (`prev == None`): there is no
    /// history to hijack yet, and since deletion is refused, "delete then
    /// re-create" is not a way back in.
    ///
    /// Not covered, and not silently implied: force-pushes and
    /// non-fast-forward updates are only constrained by the CAS `prev`
    /// git itself supplies. A sufficiently weighted approval of commit X
    /// authorizes landing X, whether or not X is a descendant of the
    /// current tip.
    #[must_use]
    pub fn with_required_review(self) -> Self {
        self.require_review
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self
    }

    /// Points the platform at the JSON-lines file its op log persists to,
    /// so `/api/log?from=` can serve entries that have already been
    /// evicted from the in-memory window. Without it, a reader that has
    /// fallen further behind than the window is told so and cannot
    /// resync.
    ///
    /// The file is append-only, so reading a prefix while the writer
    /// thread appends is safe: already-written lines never change.
    #[must_use]
    pub fn with_log_path(mut self, path: std::path::PathBuf) -> Self {
        self.log_path = Some(path);
        self
    }

    /// Replaces the actor-id → bound-name map from a freshly parsed
    /// trusted-keys file.
    ///
    /// Called from the daemon's accept loop when the file's mtime moves,
    /// so binding a name to an already-trusted key takes effect on the
    /// next request rather than waiting for some later signature failure.
    /// That matters because a binding is a *tightening*: a gate that
    /// applies at an unpredictable future moment is not a gate.
    pub fn set_key_names(&self, signers: &[crate::TrustedKey]) {
        let map = signers
            .iter()
            .filter_map(|s| {
                s.name
                    .clone()
                    .map(|name| (ContentHash::blake3(&s.key), name))
            })
            .collect();
        *self.key_names.lock().expect("key names lock") = map;
    }

    /// Shrinks the in-memory `/api/log` window. Exists so tests can
    /// exercise eviction and the resync path without writing 100k ops.
    #[must_use]
    pub fn with_log_window_cap(self, cap: usize) -> Self {
        let mut window = self.entries.lock().expect("entries lock");
        window.cap = cap.max(1);
        // Trim to the new cap now rather than waiting for the next push.
        // Without this a platform started over an existing log stays
        // over-full until something is submitted, so `/api/log` would
        // serve entries from below `base` and a reader could not tell the
        // window had shrunk.
        window.trim();
        drop(window);
        self
    }

    /// Points the platform at an operator-curated pool of eligible
    /// reviewer names (one per line, `#` comments allowed). With a pool
    /// set, a `RequestReview` carrying an empty reviewer list is
    /// answered by a node-signed [`OpKind::AssignReviewers`] drawn from
    /// the pool, excluding the requester — D24 layer 5, so a requester
    /// cannot pick a friendly reviewer.
    #[must_use]
    pub fn with_reviewer_pool(mut self, path: std::path::PathBuf) -> Self {
        self.reviewer_pool = Some(path);
        self
    }

    /// Routes one git ref update (from a repo's `update` hook) through
    /// the sequencer: CAS against the view, node-signed, totally ordered
    /// with API ops. Refs are namespaced `<repo>:<refname>`; git oids
    /// enter the envelope with their own codec ([`ContentHash::from_git_oid`]).
    ///
    /// # Errors
    ///
    /// The policy's rejection reason (stale CAS = concurrent update git
    /// itself would also have refused).
    pub fn git_update(
        &self,
        repo: &str,
        refname: &str,
        old_hex: &str,
        new_hex: &str,
        user: &str,
        cert: Option<(&str, &str)>,
    ) -> Result<(), String> {
        const ZERO: [char; 2] = ['0', '0'];
        let is_zero = |h: &str| !h.is_empty() && h.chars().all(|c| c == ZERO[0]);
        let name = format!("{repo}:{refname}");
        let prev = if is_zero(old_hex) {
            None
        } else {
            Some(ContentHash::from_git_oid(old_hex).ok_or("bad old oid")?)
        };
        let kind = if is_zero(new_hex) {
            OpKind::DeleteRef { name, prev }
        } else {
            OpKind::SetRef {
                name,
                commit: ContentHash::from_git_oid(new_hex).ok_or("bad new oid")?,
                prev,
            }
        };
        let payload = ViewOp::new(kind).to_payload();
        // Verified push certificate ("G" = good signature) attributes
        // the op to the pusher's own key; otherwise the transport user.
        let workspace = match cert {
            Some(("G", signer)) if !signer.is_empty() => format!("key/{signer}"),
            _ => format!("git/{user}"),
        };
        let sig = self.node_key.sign_submission(&workspace, &payload);
        self.handle
            .try_submit(&workspace, payload, Some(sig))
            .map(|_| ())
    }

    /// Points `workspace` at git oid `head_hex` with a node-signed op,
    /// using the view's current head as the CAS `prev` (a lost race is
    /// a sequencer rejection, not a clobber). `attribution` is the
    /// submission channel (e.g. `git/<user>`), as for git-derived ops.
    ///
    /// # Errors
    ///
    /// Bad oid, or the policy's rejection reason.
    pub fn set_workspace_head(
        &self,
        workspace: &str,
        head_hex: &str,
        attribution: &str,
    ) -> Result<(), String> {
        let commit = ContentHash::from_git_oid(head_hex).ok_or("bad head oid")?;
        let prev = self
            .view
            .lock()
            .expect("view lock")
            .workspaces
            .get(workspace)
            .cloned();
        let payload = ViewOp::new(OpKind::SetWorkspaceHead {
            workspace: workspace.to_string(),
            commit,
            prev,
        })
        .to_payload();
        let sig = self.node_key.sign_submission(attribution, &payload);
        self.handle.try_submit(attribution, payload, Some(sig)).map(|_| ())
    }

    /// Atomically creates a stable change and registers its workspace at
    /// an exact Git revision with a node-signed operation.
    pub fn create_change(
        &self,
        id: &str,
        owner: &str,
        workspace: &str,
        base_hex: &str,
        idempotency_key: &str,
        attribution: &str,
    ) -> Result<choir_sequencer::Accepted, String> {
        let base_revision = ContentHash::from_git_oid(base_hex).ok_or("bad base revision")?;
        let payload = ViewOp::new(OpKind::CreateChange {
            id: id.to_string(),
            owner: owner.to_string(),
            workspace: workspace.to_string(),
            base_revision,
            idempotency_key: idempotency_key.to_string(),
        })
        .to_payload();
        let sig = self.node_key.sign_submission(attribution, &payload);
        self.handle.try_submit(attribution, payload, Some(sig))
    }

    /// Original operation identity for a recently landed identical
    /// change-create request. The index is rebuilt from the durable log
    /// window on restart, so ordinary response-loss retries recover the
    /// same receipt without appending another operation.
    #[must_use]
    pub fn create_change_receipt(
        &self,
        id: &str,
        owner: &str,
        workspace: &str,
        base_hex: &str,
        idempotency_key: &str,
        attribution: &str,
    ) -> Option<(u64, ContentHash)> {
        let base_revision = ContentHash::from_git_oid(base_hex)?;
        let payload = ViewOp::new(OpKind::CreateChange {
            id: id.to_string(),
            owner: owner.to_string(),
            workspace: workspace.to_string(),
            base_revision,
            idempotency_key: idempotency_key.to_string(),
        })
        .to_payload();
        self.entries
            .lock()
            .expect("entries lock")
            .already_applied(attribution, &payload)
    }

    /// Submits a node-authored [`OpKind::ArchiveChange`] carrying the
    /// owner's signed authorization, after verifying that the signed
    /// payload names exactly the resource the endpoint already moved.
    pub fn submit_archive_change(
        &self,
        request: &serde_json::Value,
        expected_id: &str,
        expected_workspace: &str,
        expected_revision: &ContentHash,
        attribution: &str,
    ) -> Result<choir_sequencer::Accepted, String> {
        let sub = decode_archive_submission(
            request,
            expected_id,
            expected_workspace,
            expected_revision,
        )?;
        let payload = ViewOp::new(OpKind::ArchiveChange {
            id: expected_id.to_string(),
            workspace: expected_workspace.to_string(),
            prev_revision: expected_revision.clone(),
            owner: sub.channel,
            owner_sig: sub
                .author_sig
                .expect("decode_submission always returns a signature"),
        })
        .to_payload();
        let signature = self.node_key.sign_submission(attribution, &payload);
        self.handle
            .try_submit(attribution, payload, Some(signature))
    }

    /// Checks the archive request's signed payload shape before the
    /// filesystem is renamed. Signature and current-state admission still
    /// happen on the sequencer after the rename, with rollback on refusal.
    pub fn validate_archive_change_request(
        &self,
        request: &serde_json::Value,
        expected_id: &str,
        expected_workspace: &str,
        expected_revision: &ContentHash,
    ) -> Result<(), String> {
        decode_archive_submission(
            request,
            expected_id,
            expected_workspace,
            expected_revision,
        )
        .map(|_| ())
    }

    /// Original operation identity for an identical completed archive
    /// request still present in the durable log window.
    #[must_use]
    pub fn archive_change_receipt(
        &self,
        request: &serde_json::Value,
        expected_id: &str,
        expected_workspace: &str,
        expected_revision: &ContentHash,
        attribution: &str,
    ) -> Option<(u64, ContentHash)> {
        let sub = decode_archive_submission(
            request,
            expected_id,
            expected_workspace,
            expected_revision,
        )
        .ok()?;
        let payload = ViewOp::new(OpKind::ArchiveChange {
            id: expected_id.to_string(),
            workspace: expected_workspace.to_string(),
            prev_revision: expected_revision.clone(),
            owner: sub.channel,
            owner_sig: sub.author_sig?,
        })
        .to_payload();
        self.entries
            .lock()
            .expect("entries lock")
            .already_applied(attribution, &payload)
    }

    /// Current materialized state for one stable change.
    #[must_use]
    pub fn change_state(&self, id: &str) -> Option<ChangeState> {
        self.view
            .lock()
            .expect("view lock")
            .changes
            .get(id)
            .cloned()
    }

    /// Finds the change created by one owner-scoped idempotency key.
    #[must_use]
    pub fn change_for_idempotency(&self, owner: &str, key: &str) -> Option<(String, ChangeState)> {
        self.view
            .lock()
            .expect("view lock")
            .changes
            .iter()
            .find(|(_, change)| change.owner == owner && change.idempotency_key == key)
            .map(|(id, change)| (id.clone(), change.clone()))
    }

    /// Current exact head of an active workspace.
    #[must_use]
    pub fn workspace_head(&self, workspace: &str) -> Option<ContentHash> {
        self.view
            .lock()
            .expect("view lock")
            .workspaces
            .get(workspace)
            .cloned()
    }

    /// Draws reviewers for unassigned review `id` and records them with
    /// a node-signed op.
    ///
    /// Candidates are the pool minus everyone sharing the requester's
    /// **operator**, and the draw takes at most one reviewer per operator
    /// so [`REQUIRED_APPROVAL_WEIGHT`] reviewers means that many *independent*
    /// ones.
    ///
    /// Excluding only the requester's own name was the original rule and
    /// it does not survive the multi-operator case, which is the normal
    /// one: an operator running three agents in the pool satisfies
    /// two-person integrity by themselves, and can manufacture more
    /// agreement by registering more agents. That is the Sybil move D24
    /// says must be blocked at the operator level, so the exclusion has
    /// to be at that level too.
    ///
    /// A pool that cannot supply [`REQUIRED_APPROVAL_WEIGHT`] distinct operators
    /// draws fewer rather than doubling up — a visibly under-assigned
    /// review beats one that looks independent and is not.
    ///
    /// # Errors
    ///
    /// No pool configured, an unreadable pool, no candidate from another
    /// operator, or the sequencer's rejection reason (e.g. the review was
    /// assigned by a concurrent request).
    pub fn assign_reviewers(&self, id: &str, requester: &str) -> Result<Vec<String>, String> {
        let path = self.reviewer_pool.as_ref().ok_or("no reviewer pool configured")?;
        let text = std::fs::read_to_string(path).map_err(|e| format!("read reviewer pool: {e}"))?;
        let mine = reviewer_operator(requester);
        let mut pool: Vec<String> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#') && reviewer_operator(l) != mine)
            .map(String::from)
            .collect();
        if pool.is_empty() {
            return Err(format!(
                "reviewer pool has nobody outside {mine:?}, the requester's operator"
            ));
        }
        // Partial Fisher-Yates with a hand-rolled xorshift (no rand
        // dep). The draw is node-side and recorded in the log, so
        // replay reproduces it from the op, not from this seed.
        let mut state = seed_from_clock() ^ fnv1a(id.as_bytes());
        let mut drawn: Vec<String> = Vec::new();
        // Owned, not borrowed: the shuffle mutates `pool` under it.
        let mut seen_operators: BTreeSet<String> = BTreeSet::new();
        seen_operators.insert(mine.to_string());
        for i in 0..pool.len() {
            if drawn.len() == REQUIRED_APPROVAL_WEIGHT {
                break;
            }
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let j = i + (state as usize) % (pool.len() - i);
            pool.swap(i, j);
            // One seat per operator: a second agent from an operator
            // already drawn would add a name, not a second opinion.
            if seen_operators.insert(reviewer_operator(&pool[i]).to_string()) {
                drawn.push(pool[i].clone());
            }
        }
        let mut pool = drawn;
        pool.sort();

        let payload = ViewOp::new(OpKind::AssignReviewers {
            id: id.to_string(),
            reviewers: pool.clone(),
        })
        .to_payload();
        let channel = "node/assign";
        let sig = self.node_key.sign_submission(channel, &payload);
        self.handle.try_submit(channel, payload, Some(sig))?;
        Ok(pool)
    }

    /// Emits enough FIFO `ArchiveReview` ops to bring live review detail
    /// back to the configured count when eligible reviews exist.
    ///
    /// Complete reviews are eligible without a clock. Incomplete reviews
    /// are eligible only after the operator-selected lapse age. Multiple
    /// archives are offered together so they share durability barriers.
    fn prune_reviews(&self) -> ReviewPruneOutcome {
        let Some(retention) = &self.review_retention else {
            return ReviewPruneOutcome::default();
        };
        let _pass = self.review_prune_lock.lock().expect("review prune lock");

        let candidates: Vec<(String, bool)> = {
            // Same lock order as `ChoirPolicy::accepted`: view first,
            // tracker second. Holding both makes the count and eligibility
            // one writer-consistent snapshot; they are released before any
            // submission is sent back to the sequencer.
            let view = self.view.lock().expect("view lock");
            let mut retention = retention.lock().expect("review retention lock");
            if !retention.worth_a_pass() {
                return ReviewPruneOutcome::default();
            }
            // Cleared while the lock is still held, so an op observed from
            // here on re-arms the flag instead of being swallowed by the
            // pass that did not see it.
            retention.prunable_changed = false;
            let live_count = retention
                .live
                .iter()
                .filter(|tracked| {
                    view.reviews.get(&tracked.id).is_some_and(|review| {
                        matches!(review.status, choir_view::ReviewStatus::Live)
                    })
                })
                .count();
            let mut needed = live_count.saturating_sub(retention.config.max_live);
            // Count-only retention never reads a clock. A timestamp is
            // created and consulted only under the explicit lapse policy.
            let now = retention.config.lapse_after.map(|_| Instant::now());
            let mut candidates = Vec::with_capacity(needed);
            for tracked in &retention.live {
                if needed == 0 {
                    break;
                }
                let Some(review) = view.reviews.get(&tracked.id) else {
                    continue;
                };
                if !matches!(review.status, choir_view::ReviewStatus::Live) {
                    continue;
                }
                let lapsed = if review.complete() {
                    false
                } else if retention.config.lapse_after.is_some_and(|age| {
                    now.zip(tracked.observed_at)
                        .is_some_and(|(now, observed_at)| {
                            now.saturating_duration_since(observed_at) >= age
                        })
                }) {
                    true
                } else {
                    continue;
                };
                candidates.push((tracked.id.clone(), lapsed));
                needed -= 1;
            }
            candidates
        };

        if candidates.is_empty() {
            return ReviewPruneOutcome::default();
        }

        let channel = "node/archive";
        let submissions: Vec<Submission> = candidates
            .iter()
            .map(|(id, lapsed)| {
                let payload = ViewOp::new(OpKind::ArchiveReview {
                    id: id.clone(),
                    lapsed: *lapsed,
                })
                .to_payload();
                Submission {
                    channel: channel.to_string(),
                    author_sig: Some(self.node_key.sign_submission(channel, &payload)),
                    payload,
                }
            })
            .collect();
        let results = self.handle.try_submit_many(submissions);
        let mut outcome = ReviewPruneOutcome::default();
        for ((id, _), result) in candidates.into_iter().zip(results) {
            match result {
                Ok(_) => outcome.archived.push(id),
                Err(reason) => {
                    let mut error = Rejection::decode(&reason).to_json();
                    error["review"] = serde_json::json!(id);
                    outcome.errors.push(error);
                }
            }
        }
        outcome
    }

    fn add_retention_outcome(&self, response: &mut serde_json::Value) {
        let outcome = self.prune_reviews();
        if !outcome.archived.is_empty() {
            response["archived_reviews"] = serde_json::json!(outcome.archived);
        }
        if !outcome.errors.is_empty() {
            response["retention_errors"] = serde_json::json!(outcome.errors);
        }
    }

    /// Whether the sequencer has failed a durability barrier and stopped
    /// accepting.
    ///
    /// The daemon's accept loop polls this so a node that can no longer
    /// persist exits rather than staying up refusing everything. Process
    /// supervision only restarts a process that *exits*, so without this
    /// a transient fsync error is permanent downtime that looks like
    /// uptime.
    #[must_use]
    pub fn durability_failed(&self) -> bool {
        self.handle.durability_failed()
    }

    /// Handles one `/api/...` request, returning `(status, json_body)`.
    pub fn handle_api(&self, method: &str, path: &str, body: &[u8]) -> (u16, String) {
        match (method, path) {
            ("GET", "/api/view") => {
                let view = self.view.lock().expect("view lock");
                let ws: std::collections::BTreeMap<_, _> = view
                    .workspaces
                    .iter()
                    .map(|(k, v)| (k.clone(), v.to_hex()))
                    .collect();
                let refs: std::collections::BTreeMap<_, _> =
                    view.refs.iter().map(|(k, v)| (k.clone(), v.to_hex())).collect();
                let reviews: std::collections::BTreeMap<_, _> = view
                    .reviews
                    .iter()
                    .map(|(id, r)| (id.clone(), review_json(r)))
                    .collect();
                let changes: std::collections::BTreeMap<_, _> = view
                    .changes
                    .iter()
                    .map(|(id, change)| {
                        (
                            id.clone(),
                            serde_json::json!({
                                "owner": change.owner,
                                "workspace_id": change.workspace_id,
                                "active_workspace": change.active_workspace,
                                "base_revision": change.base_revision.to_hex(),
                                "revision_id": change.revision_id.to_hex(),
                            }),
                        )
                    })
                    .collect();
                let body = serde_json::json!({
                    "workspaces": ws,
                    "changes": changes,
                    "refs": refs,
                    "reviews": reviews,
                    "provenance": view.provenance,
                });
                (200, body.to_string())
            }
            // Pending queue for one reviewer: reviews that fanned out to
            // them and are still unanswered by them.
            ("GET", path) if path.starts_with("/api/reviews") => {
                let reviewer = path
                    .split_once("reviewer=")
                    .map(|(_, v)| v.split('&').next().unwrap_or(v))
                    .unwrap_or("");
                let view = self.view.lock().expect("view lock");
                let pending: std::collections::BTreeMap<_, _> = view
                    .reviews
                    .iter()
                    .filter(|(_, r)| {
                        r.reviewers.iter().any(|x| x == reviewer)
                            && !r.verdicts.contains_key(reviewer)
                    })
                    .map(|(id, r)| (id.clone(), review_json(r)))
                    .collect();
                (200, serde_json::json!({ "pending": pending }).to_string())
            }
            ("POST", "/api/submit") => self.submit(body),
            ("POST", "/api/submit-batch") => {
                let req: serde_json::Value = match serde_json::from_slice(body) {
                    Ok(v) => v,
                    Err(e) => return (400, Rejection::new(
                            Code::MalformedRequest,
                            format!("request body is not valid JSON: {e}"),
                            "send a JSON object; GET /llms.txt lists the fields each endpoint wants",
                        )
                        .body()),
                };
                let Some(ops) = req.get("ops").and_then(|v| v.as_array()) else {
                    return (400, r#"{"error":"need ops array"}"#.to_string());
                };
                // Ops are admitted in array order; each result is
                // independent (a rejection does not abort the batch).
                //
                // Every op is decoded from the already-parsed request,
                // offered to the sequencer, and only then waited on. The
                // previous shape called the single-op path in a loop,
                // which re-serialised each op to a string, re-parsed it,
                // blocked for its reply, and re-parsed the reply. Blocking
                // per op was the expensive part: it left the writer's
                // queue empty every time it looked, so a 500-op body paid
                // 500 durability barriers instead of ceil(500/MAX_BATCH).
                let mut decoded = Vec::with_capacity(ops.len());
                for op in ops {
                    decoded.push(decode_submission(op));
                }
                // Malformed ops never reach the sequencer. Well-formed
                // ones are pushed in request order, so pulling one
                // outcome per `Ok` below keeps results aligned with the
                // request array without any index bookkeeping.
                let subs: Vec<Submission> = decoded
                    .iter()
                    .filter_map(|d| d.as_ref().ok())
                    .map(|sub| Submission {
                        channel: sub.channel.clone(),
                        payload: sub.payload.clone(),
                        author_sig: sub.author_sig.clone(),
                    })
                    .collect();
                let mut outcomes = self.handle.try_submit_many(subs).into_iter();

                let mut accepted = 0u64;
                let mut rejected = 0u64;
                let mut results: Vec<serde_json::Value> = Vec::with_capacity(decoded.len());
                for d in &decoded {
                    let value = match d {
                        Err(reason) => {
                            rejected += 1;
                            serde_json::json!({ "error": reason })
                        }
                        Ok(sub) => match outcomes.next().expect("one outcome per submitted op") {
                            Ok(acc) => {
                                accepted += 1;
                                self.batch_result(acc, sub)
                            }
                            Err(reason) => {
                                rejected += 1;
                                serde_json::json!({ "error": reason })
                            }
                        },
                    };
                    results.push(value);
                }
                let mut response = serde_json::json!({
                    "accepted": accepted,
                    "rejected": rejected,
                    "results": results,
                });
                if accepted != 0 {
                    // Once per request, after every assignment response
                    // has been produced. Calling this from `batch_result`
                    // would rescan and emit once per element, undoing the
                    // durability-barrier win of the batch endpoint.
                    self.add_retention_outcome(&mut response);
                }
                (200, response.to_string())
            }
            ("POST", "/api/git-update") => {
                let req: serde_json::Value = match serde_json::from_slice(body) {
                    Ok(v) => v,
                    Err(e) => return (400, Rejection::new(
                            Code::MalformedRequest,
                            format!("request body is not valid JSON: {e}"),
                            "send a JSON object; GET /llms.txt lists the fields each endpoint wants",
                        )
                        .body()),
                };
                let f = |k: &str| req.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
                let (status, signer) = (f("cert_status"), f("signer"));
                match self.git_update(
                    &f("repo"),
                    &f("refname"),
                    &f("old"),
                    &f("new"),
                    &f("user"),
                    Some((status.as_str(), signer.as_str())),
                ) {
                    Ok(()) => (200, r#"{"ok":true}"#.to_string()),
                    Err(reason) => (400, Rejection::decode(&reason).body()),
                }
            }
            ("GET", path) if path.starts_with("/api/log") => {
                let from: usize = path
                    .split_once("from=")
                    .and_then(|(_, v)| v.split('&').next()?.parse().ok())
                    .unwrap_or(0);
                let window = self.entries.lock().expect("entries lock");
                let base = window.base as usize;
                // Behind the window: the entries the reader still needs
                // are no longer in memory. Serving from `base` here
                // would hand back a normal-looking page with a silent
                // hole in it, so take the persisted log instead — and
                // if there is none, say so loudly rather than lie.
                if from < base {
                    drop(window);
                    let Some(path) = &self.log_path else {
                        // The brief's reference example: a refusal that
                        // already refused correctly (a page with a hole is
                        // worse than an error), now carrying the same
                        // reason code and named action as every other.
                        let mut body = Rejection::new(
                            Code::LogEvicted,
                            "requested entries have been evicted and no persisted log is configured",
                            format!(
                                "resync from seq {base} instead; entries before it are gone from \
                                 this node"
                            ),
                        )
                        .with_states(Some(from.to_string()), Some(format!("oldest available {base}")))
                        .to_json();
                        body["window_base"] = serde_json::json!(base);
                        return (409, body.to_string());
                    };
                    return match replay_from_disk(path, from, LOG_PAGE) {
                        Ok(rows) => (
                            200,
                            serde_json::json!({
                                "entries": rows,
                                "window_base": base,
                                "source": "log",
                            })
                            .to_string(),
                        ),
                        Err(e) => (
                            500,
                            Rejection::new(
                                Code::Unclassified,
                                format!("resync from the persisted log failed: {e}"),
                                "retry; this is a node-side read failure, not something the \
                                 submission can fix",
                            )
                            .body(),
                        ),
                    };
                }
                let rows: Vec<serde_json::Value> = window
                    .entries
                    .iter()
                    .skip(from - base)
                    .take(LOG_PAGE)
                    .map(entry_json)
                    .collect();
                (
                    200,
                    serde_json::json!({
                        "entries": rows,
                        "window_base": window.base,
                        "source": "window",
                    })
                    .to_string(),
                )
            }
            _ => (404, r#"{"error":"no such endpoint"}"#.to_string()),
        }
    }

    fn submit(&self, body: &[u8]) -> (u16, String) {
        let req: serde_json::Value = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(e) => return (400, Rejection::new(
                            Code::MalformedRequest,
                            format!("request body is not valid JSON: {e}"),
                            "send a JSON object; GET /llms.txt lists the fields each endpoint wants",
                        )
                        .body()),
        };
        let sub = match decode_submission(&req) {
            Ok(sub) => sub,
            Err(reason) => return (400, Rejection::decode(&reason).body()),
        };
        match self.handle.try_submit(
            &sub.channel,
            sub.payload.clone(),
            sub.author_sig.clone(),
        ) {
            Ok(acc) => {
                let mut response = self.batch_result(acc, &sub);
                self.add_retention_outcome(&mut response);
                (200, response.to_string())
            }
            Err(reason) => {
                // A retry of an operation that already landed fails CAS
                // in exactly the same way as a genuine conflict. They
                // call for opposite actions -- read back and proceed,
                // versus re-read and rebase -- so an agent that cannot
                // tell them apart either retries a completed write or
                // abandons a successful one.
                if let Some((seq, hash)) = self
                    .entries
                    .lock()
                    .expect("entries lock")
                    .already_applied(&sub.channel, &sub.payload)
                {
                    return (
                        200,
                        serde_json::json!({
                            "seq": seq,
                            "hash": hash.to_hex(),
                            "already_applied": true,
                        })
                        .to_string(),
                    );
                }
                (400, Rejection::decode(&reason).body())
            }
        }
    }

    /// The success body for one admitted op, shared by `/api/submit` and
    /// `/api/submit-batch` so the two cannot answer differently.
    ///
    /// An unassigned review request is answered with a node-signed
    /// assignment draw once the request itself is admitted. That draw is a
    /// *further* submission, so it deliberately happens here, after the
    /// batch's own barrier, rather than being folded into it.
    fn batch_result(&self, acc: choir_sequencer::Accepted, sub: &DecodedSubmission) -> serde_json::Value {
        let mut resp = serde_json::json!({ "seq": acc.seq, "hash": acc.hash.to_hex() });
        if let Some(id) = &sub.unassigned_review {
            match self.assign_reviewers(id, &sub.channel) {
                Ok(reviewers) => resp["reviewers"] = serde_json::json!(reviewers),
                // The request stands; it is visibly unassigned, which is
                // a state no verdict can complete.
                Err(e) => resp["assignment_error"] = serde_json::json!(e),
            }
        }
        resp
    }
}

/// One decoded `/api/submit` body: the wire fields turned into the bytes
/// the sequencer wants, plus whatever the response will need afterwards.
struct DecodedSubmission {
    channel: String,
    payload: Vec<u8>,
    author_sig: Option<Witness>,
    /// Review id when this op opens a review naming no reviewers, so the
    /// node knows to draw for it once the op is admitted.
    unassigned_review: Option<String>,
}

fn decode_archive_submission(
    request: &serde_json::Value,
    expected_id: &str,
    expected_workspace: &str,
    expected_revision: &ContentHash,
) -> Result<DecodedSubmission, String> {
    let sub = decode_submission(request).map_err(|reason| {
        Rejection::new(
            Code::MalformedRequest,
            reason,
            "sign the exact ArchiveAuthorization payload with the bound owner key and include channel, payload_hex, key_id and signature_hex",
        )
        .encode()
    })?;
    let authorization = ArchiveAuthorization::from_payload(&sub.payload)
        .map_err(|error| crate::reject::from_view_error(&error).encode())?;
    match authorization {
        ArchiveAuthorization {
            id,
            workspace,
            prev_revision,
            ..
        } if id == expected_id
            && workspace == expected_workspace
            && &prev_revision == expected_revision => Ok(sub),
        _ => Err(Rejection::new(
            Code::WorkspaceState,
            "signed archive payload does not match the requested change, workspace and revision",
            "re-read GET /api/view, rebuild ArchiveAuthorization from that exact state, sign it on the owner channel and retry",
        )
        .encode()),
    }
}

/// Turns one already-parsed request object into a submission.
///
/// Split out so `/api/submit-batch` can decode straight from the parsed
/// request array. The previous batch path re-serialised each element back
/// to a string and re-parsed it through the single-op entry point, which
/// is two extra JSON round-trips per op on the endpoint that exists to
/// avoid per-op overhead.
fn decode_submission(req: &serde_json::Value) -> Result<DecodedSubmission, String> {
    let field = |name: &str| req.get(name).and_then(|v| v.as_str());
    let channel = match (field("channel"), field("workspace")) {
        (Some(channel), None) | (None, Some(channel)) => channel,
        (Some(channel), Some(legacy)) if channel == legacy => channel,
        (Some(_), Some(_)) => {
            return Err("channel and legacy workspace fields disagree".to_string())
        }
        (None, None) => return Err("need channel (or legacy workspace)".to_string()),
    };
    let (Some(payload_hex), Some(key_id), Some(signature_hex)) = (
        field("payload_hex"),
        field("key_id"),
        field("signature_hex"),
    ) else {
        return Err("need payload_hex, key_id, signature_hex".to_string());
    };
    let (Some(payload), Some(signature)) = (hex_decode(payload_hex), hex_decode(signature_hex))
    else {
        return Err("bad hex".to_string());
    };
    let unassigned_review = match ViewOp::from_payload(&payload) {
        Ok(op) => match op.kind {
            OpKind::RequestReview { id, reviewers, .. } if reviewers.is_empty() => Some(id),
            _ => None,
        },
        Err(_) => None,
    };
    Ok(DecodedSubmission {
        channel: channel.to_string(),
        payload,
        author_sig: Some(Witness {
            key_id: key_id.to_string(),
            signature,
        }),
        unassigned_review,
    })
}

/// JSON shape of one log entry, shared by the in-memory window and the
/// on-disk resync path so a catching-up reader cannot tell them apart.
///
/// Every field of the hashed form is here, which is what makes the
/// chain checkable by someone who does not trust this node: the client
/// rebuilds the canonical bytes, hashes them, and compares against
/// `hash`. `SYNC.md` writes that recipe out, and
/// `tests/sync_contract.rs` executes it — if a field is added here and
/// not there, that test fails rather than a third-party client
/// silently losing the ability to verify.
fn entry_json(e: &OpEntry) -> serde_json::Value {
    serde_json::json!({
        "seq": e.seq,
        "workspace": e.channel,
        "payload_hex": hex_encode(&e.payload),
        "author_key": e.author_sig.as_ref().map(|w| w.key_id.clone()),
        // Chain position. `parent` alone lets a client join two pages
        // (page N+1's first parent is page N's last hash); `hash` is
        // the node's claim about this entry, which the client is meant
        // to recompute from the fields below rather than believe.
        "hash": e.content_hash().to_hex(),
        "parent": e.parent.as_ref().map(ContentHash::to_hex),
        "format_version": e.format_version,
        // Empty until Phase 2 (D16), and sent anyway: witnesses are
        // inside the hashed form, so a client that left them out of its
        // recomputation would verify fine today and break on the first
        // cosigned entry.
        "witnesses": &e.witnesses,
        // The signature itself, not just whose it is. Without the bytes
        // a client can only take the node's word for authorship.
        "author_sig_hex": e.author_sig.as_ref().map(|w| hex_encode(&w.signature)),
    })
}

/// Reads up to `take` entries starting at `from` straight out of the
/// persisted JSON-lines log, for readers behind the in-memory window.
///
/// Line number is sequence number, so this skips rather than parses the
/// prefix — O(bytes before `from`) per call, which is the price of a
/// resync and is paid only by readers that fell behind.
fn replay_from_disk(
    path: &std::path::Path,
    from: usize,
    take: usize,
) -> Result<Vec<serde_json::Value>, String> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).map_err(|e| format!("open log: {e}"))?;
    let mut reader = std::io::BufReader::new(file);

    // Skip by scanning for newlines rather than `.lines().skip(from)`,
    // which allocates and UTF-8 validates a String for every line thrown
    // away. Still O(bytes before `from`), but it no longer allocates
    // per skipped entry, and a resync from a long log is exactly the case
    // where "one allocation per line you do not want" is worst.
    //
    // `read_line` into a reused buffer would be the obvious fix; this
    // uses `read_until` on the raw bytes so the skipped prefix is never
    // UTF-8 checked either. The entries actually returned are still
    // decoded through `serde_json`, which validates them properly.
    let mut scratch = Vec::new();
    for _ in 0..from {
        scratch.clear();
        let n = reader
            .read_until(b'\n', &mut scratch)
            .map_err(|e| format!("read log: {e}"))?;
        if n == 0 {
            // `from` is past the end of the log: an empty page, not an
            // error. The caller already answered 409 for the case where
            // the reader is behind the window with no log to fall back on.
            return Ok(Vec::new());
        }
    }

    let mut rows = Vec::with_capacity(take.min(LOG_PAGE));
    for _ in 0..take {
        scratch.clear();
        let n = reader
            .read_until(b'\n', &mut scratch)
            .map_err(|e| format!("read log: {e}"))?;
        if n == 0 {
            break;
        }
        let entry: OpEntry = serde_json::from_slice(trim_newline(&scratch))
            .map_err(|e| format!("decode log line: {e}"))?;
        rows.push(entry_json(&entry));
    }
    Ok(rows)
}

/// Drops a trailing `\n` and an optional preceding `\r`, so a line read
/// with `read_until` decodes the same as one produced by `.lines()`.
fn trim_newline(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// JSON shape of one review's state (shared by /api/view and
/// /api/reviews).
fn review_json(r: &choir_view::ReviewState) -> serde_json::Value {
    let verdicts: std::collections::BTreeMap<_, _> = r
        .verdicts
        .iter()
        .map(|(who, (v, note))| {
            (
                who.clone(),
                serde_json::json!({ "verdict": format!("{v:?}"), "note": note }),
            )
        })
        .collect();
    serde_json::json!({
        "target": r.target.as_ref().map(choir_oplog::ContentHash::to_hex),
        "target_ref": r.target_ref,
        "reviewers": r.reviewers,
        "verdicts": verdicts,
        "complete": r.complete(),
        "approved": r.approved(),
        "approval_weight": r.approval_weight(),
        // Empty reviewers on a live review means unassigned; on an
        // archived one it means emptied. A reader must be able to tell.
        "archived": matches!(r.status, choir_view::ReviewStatus::Archived { .. }),
    })
}

/// Decodes lowercase/uppercase hex; `None` on any bad input.
pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Encodes bytes as lowercase hex (client-side convenience, used by
/// tests and the demo).
pub fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
