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
//! - `GET /api/view` — the current materialized state plus runtime-only
//!   projections for D24 T3 concentration and complete-view growth. They
//!   derive from the same coherent snapshot and never enter persisted ops
//!   or hash input.
//!
//! Hex (not JSON-embedding) carries the payload because the signature
//! covers the exact bytes the author serialized; re-encoding through a
//! JSON tree could legally reorder/respace them and break verification.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use choir_identity::{ActorKey, Registry};
use choir_oplog::{ContentHash, OpEntry, OpLog, Witness};
use choir_sequencer::journal;
use choir_sequencer::journal::Journal as _;
use choir_sequencer::lag::LagMeter;
use choir_sequencer::{Sequencer, SequencerHandle, Submission, SubmitPolicy};
use choir_view::{
    reviewer_operator, ArchiveAuthorization, Authorization, Basis, ChangeState,
    CreateAuthorization, OpKind, Provenance, ReviewStatus, Verdict, View, ViewOp,
};

use crate::reject::{Code, Rejection};

/// Default maximum operations accepted in one `/api/submit-batch` body.
pub const DEFAULT_BATCH_OPS: usize = 256;

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
    /// Entry hash → seq, for the heads a scoped op may name.
    ///
    /// Same bound, same reason, and it costs no hashing at all: the
    /// sequencer hands `push` the hash it already computed, and eviction
    /// reads the dropped entry's hash out of the next entry's `parent`
    /// rather than re-deriving it.
    by_hash: std::collections::HashMap<ContentHash, u64>,
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

/// D24 T3 fires above these bounds. Shares use exact integer arithmetic;
/// basis points are presentation only and never drive the decision.
const T3_MAX_AGENT_KEYS_PER_OPERATOR: usize = 100;
const T3_MAX_SHARE_PERCENT: usize = 1;

/// D24 T2's declared bounds, recorded so the projection can state exactly
/// which question it is *not* answering. Nothing is ever compared against
/// them: choir persists no validity classification, so the numerator of a
/// slop rate does not exist. See [`new_actor_review_outcomes_json`].
const T2_MAX_INVALID_OR_SLOP_PERCENT: usize = 20;
const T2_MIN_VALID_PERCENT: usize = 5;

/// Version of the append-only newcomer audit and operator adjudication rows.
/// These files are not part of the signed op log, but they are persisted
/// measurement inputs and therefore carry the same explicit-version discipline.
const NEWCOMER_AUDIT_FORMAT_VERSION: u64 = 1;

#[derive(Debug, Clone)]
struct NewcomerAttempt {
    started_at_unix_ms: u64,
    first_outcome: &'static str,
    first_rejection_code: Option<String>,
    first_accepted_at_unix_ms: Option<u64>,
}

/// Sparse, durable T4 evidence. Incumbents are the keys present when the
/// operator enables the audit; only keys first admitted after that boundary are
/// newcomers. At most two outcome records are written per actor (first attempt,
/// then first acceptance after a rejection), so instrumentation cost scales with
/// newcomers rather than submissions.
struct NewcomerAudit {
    file: std::fs::File,
    adjudications_path: std::path::PathBuf,
    activated: bool,
    incumbents: BTreeSet<String>,
    attempts: BTreeMap<u64, NewcomerAttempt>,
    by_actor: BTreeMap<String, u64>,
    appeals: BTreeSet<u64>,
    next_attempt_id: u64,
    available: bool,
}

impl NewcomerAudit {
    fn open(
        audit_path: &std::path::Path,
        adjudications_path: std::path::PathBuf,
        incumbents: BTreeSet<String>,
    ) -> Result<Self, String> {
        if let Some(parent) = audit_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create newcomer audit directory: {e}"))?;
        }
        let existing = match std::fs::read_to_string(audit_path) {
            Ok(existing) => existing,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(format!("read newcomer audit: {error}")),
        };
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(audit_path)
            .map_err(|e| format!("open newcomer audit: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(audit_path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| format!("chmod newcomer audit: {e}"))?;
        }
        let mut audit = Self {
            file,
            adjudications_path,
            activated: false,
            incumbents: BTreeSet::new(),
            attempts: BTreeMap::new(),
            by_actor: BTreeMap::new(),
            appeals: BTreeSet::new(),
            next_attempt_id: 0,
            available: true,
        };
        for (index, line) in existing.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let value: serde_json::Value = serde_json::from_str(line)
                .map_err(|e| format!("newcomer audit line {}: {e}", index + 1))?;
            audit
                .replay(&value)
                .map_err(|e| format!("newcomer audit line {}: {e}", index + 1))?;
        }
        if audit.activated {
            return Ok(audit);
        }
        if existing.lines().any(|line| !line.trim().is_empty()) {
            return Err("newcomer audit has rows before its activation boundary".to_string());
        }
        let record = serde_json::json!({
            "format_version": NEWCOMER_AUDIT_FORMAT_VERSION,
            "kind": "activation",
            "observed_at_unix_ms": unix_ms(),
            "incumbent_actor_keys": incumbents,
        });
        audit.append(&record)?;
        audit.replay(&record)?;
        Ok(audit)
    }

    fn replay(&mut self, value: &serde_json::Value) -> Result<(), String> {
        if value["format_version"].as_u64() != Some(NEWCOMER_AUDIT_FORMAT_VERSION) {
            return Err("unsupported format_version".to_string());
        }
        let kind = value["kind"].as_str().ok_or("missing kind")?;
        match kind {
            "activation" => {
                if self.activated || !self.attempts.is_empty() || !self.appeals.is_empty() {
                    return Err("duplicate or late activation boundary".to_string());
                }
                value["observed_at_unix_ms"]
                    .as_u64()
                    .ok_or("activation needs observed_at_unix_ms")?;
                let incumbent_actor_keys = value["incumbent_actor_keys"]
                    .as_array()
                    .ok_or("activation needs incumbent_actor_keys")?;
                for actor_key in incumbent_actor_keys {
                    let actor_key = actor_key
                        .as_str()
                        .filter(|actor_key| !actor_key.is_empty())
                        .ok_or("incumbent actor keys must be non-empty strings")?;
                    if !self.incumbents.insert(actor_key.to_string()) {
                        return Err("activation has a duplicate incumbent actor key".to_string());
                    }
                }
                self.activated = true;
            }
            "first_attempt" => {
                if !self.activated {
                    return Err("first_attempt precedes activation".to_string());
                }
                let attempt_id = value["attempt_id"].as_u64().ok_or("missing attempt_id")?;
                if self.attempts.contains_key(&attempt_id) {
                    return Err("duplicate first_attempt".to_string());
                }
                let actor_key = value["actor_key"]
                    .as_str()
                    .filter(|value| !value.is_empty())
                    .ok_or("missing actor_key")?
                    .to_string();
                if self.incumbents.contains(&actor_key) {
                    return Err("first_attempt belongs to an incumbent actor key".to_string());
                }
                if self.by_actor.contains_key(&actor_key) {
                    return Err("actor has more than one first_attempt".to_string());
                }
                let started_at_unix_ms = value["started_at_unix_ms"]
                    .as_u64()
                    .ok_or("missing started_at_unix_ms")?;
                let first_outcome = match value["outcome"].as_str() {
                    Some("accepted") => "accepted",
                    Some("rejected") => "rejected",
                    _ => return Err("outcome must be accepted or rejected".to_string()),
                };
                let first_rejection_code = value["rejection_code"].as_str().map(str::to_string);
                if (first_outcome == "rejected") != first_rejection_code.is_some() {
                    return Err("rejected first attempts need one rejection_code".to_string());
                }
                let first_accepted_at_unix_ms = (first_outcome == "accepted").then_some(
                    value["completed_at_unix_ms"]
                        .as_u64()
                        .ok_or("accepted attempt needs completed_at_unix_ms")?,
                );
                self.by_actor.insert(actor_key, attempt_id);
                self.attempts.insert(
                    attempt_id,
                    NewcomerAttempt {
                        started_at_unix_ms,
                        first_outcome,
                        first_rejection_code,
                        first_accepted_at_unix_ms,
                    },
                );
                self.next_attempt_id = self.next_attempt_id.max(attempt_id.saturating_add(1));
            }
            "first_accept" => {
                let attempt_id = value["attempt_id"].as_u64().ok_or("missing attempt_id")?;
                let completed = value["completed_at_unix_ms"]
                    .as_u64()
                    .ok_or("first_accept needs completed_at_unix_ms")?;
                let attempt = self
                    .attempts
                    .get_mut(&attempt_id)
                    .ok_or("first_accept precedes first_attempt")?;
                if attempt.first_outcome != "rejected" {
                    return Err("first_accept follows an accepted first attempt".to_string());
                }
                if attempt
                    .first_accepted_at_unix_ms
                    .replace(completed)
                    .is_some()
                {
                    return Err("duplicate first_accept".to_string());
                }
            }
            "appeal" => {
                let attempt_id = value["attempt_id"].as_u64().ok_or("missing attempt_id")?;
                if !self.attempts.contains_key(&attempt_id) {
                    return Err("appeal references an unknown attempt".to_string());
                }
                if self.attempts[&attempt_id].first_outcome != "rejected" {
                    return Err("appeal references an accepted first attempt".to_string());
                }
                if !self.appeals.insert(attempt_id) {
                    return Err("duplicate appeal".to_string());
                }
            }
            _ => return Err("unknown kind".to_string()),
        }
        Ok(())
    }

    fn append(&mut self, value: &serde_json::Value) -> Result<(), String> {
        serde_json::to_writer(&mut self.file, value)
            .map_err(|e| format!("write newcomer audit: {e}"))?;
        self.file
            .write_all(b"\n")
            .and_then(|_| self.file.sync_data())
            .map_err(|e| format!("sync newcomer audit: {e}"))
    }

    fn observe(
        &mut self,
        actor_key: &str,
        started_at_unix_ms: u64,
        accepted: bool,
        rejection_code: Option<&str>,
    ) -> Result<Option<u64>, String> {
        // `actor_key` is the *claimed* key id, straight off an
        // unverified signature, so nothing may be recorded under it
        // until a signature check has vouched for it. Both signature
        // failures have to be listed: this read `== Some("unknown_key")`
        // while that code covered every verification failure, and
        // splitting `bad_signature` out of it would otherwise have let
        // anyone who knows a trusted key id append audit rows in that
        // actor's name by sending deliberate garbage.
        if self.incumbents.contains(actor_key)
            || matches!(rejection_code, Some("unknown_key" | "bad_signature"))
        {
            return Ok(None);
        }
        let completed_at_unix_ms = unix_ms();
        if let Some(attempt_id) = self.by_actor.get(actor_key).copied() {
            let needs_accept = accepted
                && self.attempts[&attempt_id]
                    .first_accepted_at_unix_ms
                    .is_none();
            if needs_accept {
                let record = serde_json::json!({
                    "format_version": NEWCOMER_AUDIT_FORMAT_VERSION,
                    "kind": "first_accept",
                    "attempt_id": attempt_id,
                    "completed_at_unix_ms": completed_at_unix_ms,
                });
                if let Err(error) = self.append(&record) {
                    self.available = false;
                    return Err(error);
                }
                self.attempts
                    .get_mut(&attempt_id)
                    .expect("attempt exists")
                    .first_accepted_at_unix_ms = Some(completed_at_unix_ms);
            }
            return Ok(Some(attempt_id));
        }

        let attempt_id = self.next_attempt_id;
        let outcome = if accepted { "accepted" } else { "rejected" };
        let mut record = serde_json::json!({
            "format_version": NEWCOMER_AUDIT_FORMAT_VERSION,
            "kind": "first_attempt",
            "attempt_id": attempt_id,
            "actor_key": actor_key,
            "started_at_unix_ms": started_at_unix_ms,
            "completed_at_unix_ms": completed_at_unix_ms,
            "outcome": outcome,
        });
        if let Some(code) = rejection_code {
            record["rejection_code"] = serde_json::json!(code);
        }
        if let Err(error) = self.append(&record) {
            self.available = false;
            return Err(error);
        }
        self.replay(&record)?;
        Ok(Some(attempt_id))
    }

    fn appeal(&mut self, attempt_id: u64) -> Result<(), String> {
        let Some(attempt) = self.attempts.get(&attempt_id) else {
            return Err("no such newcomer attempt".to_string());
        };
        if attempt.first_outcome != "rejected" {
            return Err("only a rejected first attempt can be appealed".to_string());
        }
        if self.appeals.contains(&attempt_id) {
            return Ok(());
        }
        let record = serde_json::json!({
            "format_version": NEWCOMER_AUDIT_FORMAT_VERSION,
            "kind": "appeal",
            "attempt_id": attempt_id,
            "observed_at_unix_ms": unix_ms(),
        });
        if let Err(error) = self.append(&record) {
            self.available = false;
            return Err(error);
        }
        self.replay(&record)
    }
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn median_u64(values: &mut [u64]) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let middle = values.len() / 2;
    if values.len() % 2 == 1 {
        Some(values[middle])
    } else {
        Some(((u128::from(values[middle - 1]) + u128::from(values[middle])) / 2) as u64)
    }
}

fn read_newcomer_adjudications(
    path: &std::path::Path,
    attempts: &BTreeMap<u64, NewcomerAttempt>,
) -> Result<(BTreeMap<u64, bool>, String), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read adjudications: {e}"))?;
    let snapshot_hash = ContentHash::blake3(&bytes).to_hex();
    let text = String::from_utf8(bytes).map_err(|_| "adjudications are not UTF-8".to_string())?;
    let mut rows = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(line)
            .map_err(|e| format!("adjudication line {}: {e}", index + 1))?;
        if value["format_version"].as_u64() != Some(NEWCOMER_AUDIT_FORMAT_VERSION) {
            return Err(format!(
                "adjudication line {} has unsupported format_version",
                index + 1
            ));
        }
        let attempt_id = value["attempt_id"]
            .as_u64()
            .ok_or_else(|| format!("adjudication line {} needs attempt_id", index + 1))?;
        let legitimate = value["legitimate"]
            .as_bool()
            .ok_or_else(|| format!("adjudication line {} needs legitimate", index + 1))?;
        if !attempts.contains_key(&attempt_id) {
            return Err(format!(
                "adjudication line {} references an unknown attempt",
                index + 1
            ));
        }
        if rows.insert(attempt_id, legitimate).is_some() {
            return Err(format!(
                "adjudication line {} duplicates an attempt",
                index + 1
            ));
        }
    }
    Ok((rows, snapshot_hash))
}

fn newcomer_harm_json(audit: Option<&Arc<Mutex<NewcomerAudit>>>) -> serde_json::Value {
    let Some(audit) = audit else {
        return serde_json::json!({
            "format_version": NEWCOMER_AUDIT_FORMAT_VERSION,
            "configured": false,
            "available": false,
            "tripwire_status": "indeterminate",
            "evaluation_complete": false,
        });
    };
    let audit = audit.lock().expect("newcomer audit lock");
    let adjudications = read_newcomer_adjudications(&audit.adjudications_path, &audit.attempts);
    let (rows, snapshot_hash, adjudications_available, adjudications_error) = match adjudications {
        Ok((rows, hash)) => (rows, Some(hash), true, None),
        Err(error) => (BTreeMap::new(), None, false, Some(error)),
    };

    let mut first_accepted = 0usize;
    let mut first_rejected = 0usize;
    let mut rejection_codes: BTreeMap<String, usize> = BTreeMap::new();
    let mut legitimate = 0usize;
    let mut legitimate_first_rejected = 0usize;
    let mut legitimate_pending_acceptance = 0usize;
    let mut accepted_latencies = Vec::new();
    for (attempt_id, attempt) in &audit.attempts {
        if attempt.first_outcome == "accepted" {
            first_accepted += 1;
        } else {
            first_rejected += 1;
            *rejection_codes
                .entry(
                    attempt
                        .first_rejection_code
                        .clone()
                        .expect("rejected attempts carry a code"),
                )
                .or_default() += 1;
        }
        if rows.get(attempt_id) != Some(&true) {
            continue;
        }
        legitimate += 1;
        legitimate_first_rejected += usize::from(attempt.first_outcome == "rejected");
        match attempt.first_accepted_at_unix_ms {
            Some(accepted) => {
                accepted_latencies.push(accepted.saturating_sub(attempt.started_at_unix_ms))
            }
            None => legitimate_pending_acceptance += 1,
        }
    }
    let median_time_to_first_accepted_ms = median_u64(&mut accepted_latencies);
    let adjudicated = rows.len();
    let total = audit.attempts.len();
    let unresolved_appeals = audit
        .appeals
        .iter()
        .filter(|attempt_id| !rows.contains_key(attempt_id))
        .count();
    let false_reject_rate_basis_points =
        (legitimate != 0).then(|| share_basis_points(legitimate_first_rejected, legitimate));
    let measurement_complete = audit.available
        && adjudications_available
        && legitimate != 0
        && adjudicated == total
        && legitimate_pending_acceptance == 0;
    serde_json::json!({
        "format_version": NEWCOMER_AUDIT_FORMAT_VERSION,
        "configured": true,
        "available": audit.available && adjudications_available,
        "scope": "post-activation cryptographically verified signed-API actor keys",
        "thresholds": {
            "false_reject_rate_basis_points": null,
            "median_time_to_first_accepted_ms": null,
            "status": "unset_pending_first_measurement",
        },
        "audit": {
            "available": audit.available,
            "incumbent_actor_keys_excluded": audit.incumbents.len(),
            "first_attempts": total,
            "first_attempts_accepted": first_accepted,
            "first_attempts_rejected": first_rejected,
            "first_rejections_by_code": rejection_codes,
        },
        "appeals": {
            "submitted": audit.appeals.len(),
            "unresolved": unresolved_appeals,
        },
        "adjudications": {
            "available": adjudications_available,
            "snapshot_hash": snapshot_hash,
            "error": adjudications_error,
            "attempts": adjudicated,
            "coverage_basis_points": share_basis_points(adjudicated, total),
            "legitimate_attempts": legitimate,
        },
        "measurements": {
            "legitimate_first_attempts_rejected": legitimate_first_rejected,
            "false_reject_rate_basis_points": false_reject_rate_basis_points,
            "accepted_legitimate_newcomers": accepted_latencies.len(),
            "legitimate_newcomers_pending_acceptance": legitimate_pending_acceptance,
            "median_time_to_first_accepted_ms": median_time_to_first_accepted_ms,
        },
        "measurement_complete": measurement_complete,
        "tripwire_status": "indeterminate",
        "evaluation_complete": false,
        "semantics": {
            "false_reject": "an operator-adjudicated legitimate actor whose first verified signed-API attempt was rejected",
            "time_to_first_accepted": "elapsed wall time from that actor's first verified signed-API attempt to its first accepted signed operation",
            "excluded": "unverified unknown-key claims, incumbent keys, Git/Basic-auth pushes, and HTTP-only workspace provisioning",
        },
    })
}

/// One log author's signed identity claim. Resolution is delayed until a
/// report is read so a hot-reloaded binding file changes the whole current
/// projection consistently, including entries replayed before the reload.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ActorEvidence {
    key_id: Option<String>,
    channel: String,
}

impl ActorEvidence {
    fn from_entry(entry: &OpEntry) -> Self {
        Self {
            key_id: entry.author_sig.as_ref().map(|sig| sig.key_id.clone()),
            channel: entry.channel.clone(),
        }
    }
}

/// Evidence available for one ref move. A directly bound signer wins. A
/// node-signed Git move may instead be attributed to the unique bound
/// operator whose approved review named the exact `(ref, target)` pair.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct RefAttribution {
    direct: ActorEvidence,
    approved_requesters: Vec<ActorEvidence>,
}

/// Which source a binding snapshot was built from.
///
/// D24 T3 attribution and channel admission read *different* sources on
/// purpose, so the snapshot has to say which one it is rather than leaving
/// a reader to infer it from the call site.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum BindingSource {
    /// The operator-written trusted-keys file. Mutable, unsequenced, and
    /// therefore usable for admission but not as attribution evidence.
    #[default]
    KeysFile,
    /// [`View::bindings`]: sequenced `BindKey` ops, replayable from the log.
    DurableLog,
}

impl BindingSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::KeysFile => "keys_file",
            Self::DurableLog => "durable_log",
        }
    }
}

/// Current binding snapshot. The effective map preserves admission's
/// existing one-name-per-actor behaviour; the deterministic records retain
/// enough information to report duplicate cross-operator bindings as
/// ambiguous rather than choosing whichever row happened to come last.
#[derive(Default)]
struct KeyBindings {
    effective: std::collections::HashMap<ContentHash, String>,
    operators_by_actor: BTreeMap<String, BTreeSet<String>>,
    names_by_actor: BTreeMap<String, BTreeSet<String>>,
    unbound_actors: BTreeSet<String>,
    configured: bool,
    available: bool,
    source: BindingSource,
}

impl KeyBindings {
    fn unavailable(configured: bool) -> Self {
        Self {
            configured,
            ..Self::default()
        }
    }

    /// Builds the attribution snapshot from the durable record instead of
    /// the keys file.
    ///
    /// `population` supplies *who is trusted*, which the log cannot answer:
    /// a `BindKey` names a key, but only the keys file says which keys the
    /// node accepts at all. So the two compose rather than compete — the
    /// file decides the denominator, the log decides attribution, and a
    /// trusted key with no sequenced binding lands in `unbound_actors` and
    /// holds `evaluation_complete` at false.
    ///
    /// Without a readable population there is no denominator, so the
    /// snapshot is unavailable rather than reporting completeness over
    /// whatever subset happens to be bound.
    fn from_view(view: &View, population: &Self) -> Self {
        if !population.available {
            return Self {
                source: BindingSource::DurableLog,
                ..Self::unavailable(population.configured)
            };
        }
        let trusted: BTreeSet<&String> = population
            .names_by_actor
            .keys()
            .chain(population.unbound_actors.iter())
            .collect();

        let mut operators_by_actor: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut names_by_actor: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (key_id, binding) in &view.bindings {
            if !trusted.contains(key_id) {
                continue;
            }
            // Revoked keys keep their operator. Attribution must survive
            // withdrawal, or an operator could shed a concentration count
            // by revoking the key that earned it.
            operators_by_actor
                .entry(key_id.clone())
                .or_default()
                .insert(binding.operator.clone());
            // Only a bound channel attributes activity, mirroring the keys
            // file, where a key with no name attributes nothing. The
            // operator is known; which channel it speaks as is not.
            if let Some(channel) = &binding.channel {
                names_by_actor
                    .entry(key_id.clone())
                    .or_default()
                    .insert(channel.clone());
            }
        }
        let unbound_actors = trusted
            .into_iter()
            .filter(|actor| !names_by_actor.contains_key(*actor))
            .cloned()
            .collect();
        Self {
            // Admission is not served from this snapshot; leaving the map
            // empty keeps it structurally unable to answer `bound_name`.
            effective: std::collections::HashMap::new(),
            operators_by_actor,
            names_by_actor,
            unbound_actors,
            configured: population.configured,
            available: true,
            source: BindingSource::DurableLog,
        }
    }

    fn from_signers(signers: &[crate::TrustedKey]) -> Self {
        let mut effective = std::collections::HashMap::new();
        let mut names_by_actor: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut all_actors = BTreeSet::new();
        for signer in signers {
            all_actors.insert(signer.actor_id.clone());
            if let Some(name) = &signer.name {
                effective.insert(ContentHash::blake3(&signer.key), name.clone());
                names_by_actor
                    .entry(signer.actor_id.clone())
                    .or_default()
                    .insert(name.clone());
            }
        }
        let operators_by_actor = names_by_actor
            .iter()
            .map(|(actor, names)| {
                (
                    actor.clone(),
                    names
                        .iter()
                        .map(|name| reviewer_operator(name).to_string())
                        .collect(),
                )
            })
            .collect();
        let unbound_actors = all_actors
            .iter()
            .filter(|actor| !names_by_actor.contains_key(*actor))
            .cloned()
            .collect();
        Self {
            effective,
            operators_by_actor,
            names_by_actor,
            unbound_actors,
            configured: true,
            available: true,
            source: BindingSource::KeysFile,
        }
    }

    fn bound_name(&self, actor_id: &ContentHash) -> Option<&str> {
        self.effective.get(actor_id).map(String::as_str)
    }

    fn resolve_evidence(&self, evidence: &ActorEvidence) -> AttributionResolution {
        let Some(key_id) = evidence.key_id.as_ref() else {
            return AttributionResolution::Unknown;
        };
        let Some(names) = self.names_by_actor.get(key_id) else {
            return AttributionResolution::Unknown;
        };
        if !names.contains(&evidence.channel) {
            return AttributionResolution::Unknown;
        }
        let Some(operators) = self.operators_by_actor.get(key_id) else {
            return AttributionResolution::Unknown;
        };
        match operators.len() {
            0 => AttributionResolution::Unknown,
            1 => AttributionResolution::Operator(operators.first().expect("one operator").clone()),
            _ => AttributionResolution::Ambiguous,
        }
    }

    fn snapshot_hash(&self) -> Option<String> {
        self.available.then(|| {
            let mut records = self.names_by_actor.clone();
            for actor in &self.unbound_actors {
                records.entry(actor.clone()).or_default();
            }
            let bytes =
                serde_json::to_vec(&records).expect("binding snapshot is always serializable");
            ContentHash::blake3(&bytes).to_hex()
        })
    }
}

/// D24 T2 load evidence: objective counts folded straight from the log.
///
/// These answer "how much reviewer work did this cohort create, and how much
/// of it landed" without any human calling anything slop. That matters
/// because a classifier-based slop rate goes blind exactly when it is needed:
/// a flood collapses adjudication coverage, and incomplete coverage must
/// report `indeterminate`. These counts keep working during the flood, and
/// no contributor can suppress them by declining to request a review.
#[derive(Default)]
struct NewActorLoadState {
    /// Accepted entries per signing key. The unit is a *contribution
    /// offered*, not a review: `RequestReview` is authored by the
    /// contributor, so a review-based denominator lets an actor choose
    /// whether to be measured.
    submissions: BTreeMap<String, usize>,
    /// `PostVerdict` ops per review id: reviewer rounds actually consumed.
    /// Re-review after changes is the cost signal, so it is counted rather
    /// than collapsed into a final verdict.
    verdict_rounds: BTreeMap<String, usize>,
    /// Reviews whose exact `(target_ref, target)` was observed live in
    /// [`View::refs`]. Landing is *observed*; approval is not landing.
    landed: BTreeSet<String>,
}

impl NewActorLoadState {
    fn observe(&mut self, entry: &OpEntry, op: &ViewOp, view: &View) {
        if let Some(signature) = &entry.author_sig {
            // Only clone the key the first time it is seen. `entry()` would
            // allocate on every accepted op, and a repeat submitter is the
            // common case: the allocation budget caught exactly that.
            if let Some(count) = self.submissions.get_mut(&signature.key_id) {
                *count += 1;
            } else {
                self.submissions.insert(signature.key_id.clone(), 1);
            }
        }
        match &op.kind {
            OpKind::PostVerdict { id, .. } => {
                *self.verdict_rounds.entry(id.clone()).or_default() += 1;
            }
            OpKind::SetRef { name, commit, .. } => {
                for (id, review) in &view.reviews {
                    if review.target_ref.as_deref() == Some(name)
                        && review.target.as_ref() == Some(commit)
                    {
                        self.landed.insert(id.clone());
                    }
                }
            }
            _ => {}
        }
    }
}

#[derive(Default)]
struct ConcentrationState {
    review_requesters: BTreeMap<String, ActorEvidence>,
    active_branches: BTreeMap<String, RefAttribution>,
    ref_updates: BTreeMap<String, BTreeMap<RefAttribution, usize>>,
    /// T2's load evidence, folded here rather than behind a second mutex:
    /// it needs the same `(entry, op, view)` triple at the same point of the
    /// same single-writer fold, and a parallel tracker would add plumbing
    /// plus a second chance for the two to disagree about `as_of_seq`.
    new_actor_load: NewActorLoadState,
    as_of_seq: Option<u64>,
}

impl ConcentrationState {
    fn observe(&mut self, entry: &OpEntry, op: &ViewOp, view: &View) {
        self.as_of_seq = Some(entry.seq);
        self.new_actor_load.observe(entry, op, view);
        match &op.kind {
            OpKind::RequestReview { id, .. } => {
                self.review_requesters
                    .insert(id.clone(), ActorEvidence::from_entry(entry));
            }
            OpKind::SetRef { name, commit, prev } => {
                let mut approved_requesters: Vec<_> = view
                    .reviews
                    .iter()
                    .filter(|(_, review)| {
                        review.target_ref.as_deref() == Some(name)
                            && review.target.as_ref() == Some(commit)
                            && review.approved()
                    })
                    .filter_map(|(id, _)| self.review_requesters.get(id).cloned())
                    .collect();
                approved_requesters.sort();
                approved_requesters.dedup();
                let attribution = RefAttribution {
                    direct: ActorEvidence::from_entry(entry),
                    approved_requesters,
                };
                if is_branch_ref(name) {
                    self.active_branches
                        .insert(name.clone(), attribution.clone());
                }
                if prev.is_some() {
                    *self
                        .ref_updates
                        .entry(name.clone())
                        .or_default()
                        .entry(attribution)
                        .or_default() += 1;
                }
            }
            OpKind::DeleteRef { name, .. } => {
                self.active_branches.remove(name);
            }
            _ => {}
        }
    }
}

fn is_branch_ref(name: &str) -> bool {
    name.strip_prefix("refs/heads/")
        .or_else(|| name.split_once(":refs/heads/").map(|(_, branch)| branch))
        .is_some_and(|branch| !branch.is_empty())
}

fn share_basis_points(part: usize, total: usize) -> usize {
    if total == 0 {
        return 0;
    }
    ((part as u128 * 10_000) / total as u128) as usize
}

fn share_tripped(part: usize, total: usize) -> bool {
    total != 0 && part as u128 * 100 > total as u128 * T3_MAX_SHARE_PERCENT as u128
}

enum AttributionResolution {
    Operator(String),
    Unknown,
    Ambiguous,
}

fn resolve_attribution(
    attribution: &RefAttribution,
    bindings: &KeyBindings,
) -> AttributionResolution {
    match bindings.resolve_evidence(&attribution.direct) {
        AttributionResolution::Operator(operator) => {
            return AttributionResolution::Operator(operator);
        }
        AttributionResolution::Ambiguous => return AttributionResolution::Ambiguous,
        AttributionResolution::Unknown => {}
    }
    let mut operators = BTreeSet::new();
    let mut ambiguous = false;
    let mut unknown = false;
    for requester in &attribution.approved_requesters {
        match bindings.resolve_evidence(requester) {
            AttributionResolution::Operator(operator) => {
                operators.insert(operator);
            }
            AttributionResolution::Ambiguous => ambiguous = true,
            AttributionResolution::Unknown => unknown = true,
        }
    }
    if ambiguous || (unknown && !operators.is_empty()) {
        return AttributionResolution::Ambiguous;
    }
    if unknown {
        return AttributionResolution::Unknown;
    }
    match operators.len() {
        0 => AttributionResolution::Unknown,
        1 => AttributionResolution::Operator(
            operators.first().expect("one requester operator").clone(),
        ),
        _ => AttributionResolution::Ambiguous,
    }
}

#[derive(Default)]
struct OperatorConcentration {
    agent_keys: usize,
    active_branches: usize,
    protected_updates: usize,
}

struct ProtectedPolicySnapshot {
    configured: bool,
    available: bool,
    hash: Option<String>,
    patterns: Vec<String>,
}

impl ProtectedPolicySnapshot {
    fn read(path: Option<&std::path::Path>) -> Self {
        let Some(path) = path else {
            return Self {
                configured: false,
                available: false,
                hash: None,
                patterns: Vec::new(),
            };
        };
        match std::fs::read_to_string(path) {
            Ok(text) => Self {
                configured: true,
                available: true,
                hash: Some(ContentHash::blake3(text.as_bytes()).to_hex()),
                patterns: text
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty() && !line.starts_with('#'))
                    .map(str::to_string)
                    .collect(),
            },
            Err(_) => Self {
                configured: true,
                available: false,
                hash: None,
                patterns: Vec::new(),
            },
        }
    }

    fn matches(&self, name: &str) -> bool {
        self.patterns.iter().any(|pattern| {
            pattern
                .strip_suffix('*')
                .map_or(name == pattern, |prefix| name.starts_with(prefix))
        })
    }
}

fn concentration_json(
    state: &ConcentrationState,
    bindings: &KeyBindings,
    protected: &ProtectedPolicySnapshot,
) -> serde_json::Value {
    let mut operators: BTreeMap<String, OperatorConcentration> = BTreeMap::new();
    let mut ambiguous_agent_keys = 0usize;
    // Only `KeyBindings::from_view` feeds this function, and the fold lets
    // one key name exactly one operator for its lifetime, so today every
    // set here has exactly one member and `ambiguous_agent_keys` is always
    // zero. The other arms are kept deliberately, not by oversight: the
    // keys-file shape they answer is still constructible by
    // `from_signers`, and if a snapshot from that source is ever routed
    // here, refusing to pick a winner is the behaviour that belongs. Note
    // this is only the *per-key* ambiguity; ambiguity across several
    // requester keys is live and counted in `ambiguous_active_branches`.
    for operator_set in bindings.operators_by_actor.values() {
        match operator_set.len() {
            0 => {}
            1 => {
                operators
                    .entry(operator_set.first().expect("one operator").clone())
                    .or_default()
                    .agent_keys += 1;
            }
            _ => ambiguous_agent_keys += 1,
        }
    }

    let active_total = state.active_branches.len();
    let mut unknown_active = 0usize;
    let mut ambiguous_active = 0usize;
    for attribution in state.active_branches.values() {
        match resolve_attribution(attribution, bindings) {
            AttributionResolution::Operator(operator) => {
                operators.entry(operator).or_default().active_branches += 1;
            }
            AttributionResolution::Unknown => unknown_active += 1,
            AttributionResolution::Ambiguous => ambiguous_active += 1,
        }
    }

    let mut protected_total = 0usize;
    let mut unknown_protected = 0usize;
    let mut ambiguous_protected = 0usize;
    if protected.available {
        for (name, by_attribution) in &state.ref_updates {
            if !protected.matches(name) {
                continue;
            }
            for (attribution, count) in by_attribution {
                protected_total += count;
                match resolve_attribution(attribution, bindings) {
                    AttributionResolution::Operator(operator) => {
                        operators.entry(operator).or_default().protected_updates += count;
                    }
                    AttributionResolution::Unknown => unknown_protected += count,
                    AttributionResolution::Ambiguous => ambiguous_protected += count,
                }
            }
        }
    }

    let mut any_tripwire = false;
    let operator_rows: BTreeMap<String, serde_json::Value> = operators
        .into_iter()
        .map(|(operator, row)| {
            let agent_keys_tripped = row.agent_keys > T3_MAX_AGENT_KEYS_PER_OPERATOR;
            let active_tripped = share_tripped(row.active_branches, active_total);
            let protected_tripped = protected
                .available
                .then(|| share_tripped(row.protected_updates, protected_total));
            any_tripwire |=
                agent_keys_tripped || active_tripped || protected_tripped.unwrap_or(false);
            let protected_updates = protected.available.then_some(row.protected_updates);
            let protected_share = protected
                .available
                .then(|| share_basis_points(row.protected_updates, protected_total));
            (
                operator,
                serde_json::json!({
                    "agent_keys": row.agent_keys,
                    "active_branches": row.active_branches,
                    "active_branch_share_basis_points":
                        share_basis_points(row.active_branches, active_total),
                    "protected_updates": protected_updates,
                    "protected_update_share_basis_points": protected_share,
                    "tripwires": {
                        "agent_keys": agent_keys_tripped,
                        "active_branch_share": active_tripped,
                        "protected_update_share": protected_tripped,
                    },
                }),
            )
        })
        .collect();

    let attributed_active = active_total - unknown_active - ambiguous_active;
    let attributed_protected = protected_total - unknown_protected - ambiguous_protected;
    let evaluation_complete = bindings.available
        && bindings.unbound_actors.is_empty()
        && ambiguous_agent_keys == 0
        && unknown_active == 0
        && ambiguous_active == 0
        && protected.available
        && unknown_protected == 0
        && ambiguous_protected == 0;
    let tripwire_status = if any_tripwire {
        "observed"
    } else if evaluation_complete {
        "not_observed"
    } else {
        "indeterminate"
    };
    serde_json::json!({
        "format_version": 1,
        "as_of_seq": state.as_of_seq,
        "thresholds": {
            "max_agent_keys_per_operator": T3_MAX_AGENT_KEYS_PER_OPERATOR,
            "max_share_basis_points": T3_MAX_SHARE_PERCENT * 100,
            "comparison": "strictly_greater_than",
        },
        "bindings": {
            "configured": bindings.configured,
            "available": bindings.available,
            "snapshot_hash": bindings.snapshot_hash(),
            // Two sources feed this block and one name for both would read
            // as more precise than it is. Attribution is what has to be
            // replayable: `durable_log` means every operator name below
            // came from a sequenced `BindKey`, not from whatever the keys
            // file happened to say at read time. The population — which
            // keys the node trusts at all — is not in the log and stays
            // with the file, which is why coverage can be incomplete even
            // when attribution is sound.
            "attribution_source": bindings.source.as_str(),
            "population_source": "keys_file",
        },
        "protected_policy": {
            "configured": protected.configured,
            "available": protected.available,
            "snapshot_hash": protected.hash.as_deref(),
            "classification": "current_policy",
        },
        "totals": {
            "bound_agent_keys": bindings.available.then_some(bindings.names_by_actor.len()),
            "unbound_agent_keys": bindings.available.then_some(bindings.unbound_actors.len()),
            "ambiguous_agent_keys": bindings.available.then_some(ambiguous_agent_keys),
            "active_branches": active_total,
            "attributed_active_branches": attributed_active,
            "unattributed_active_branches": unknown_active + ambiguous_active,
            "unknown_active_branches": unknown_active,
            "ambiguous_active_branches": ambiguous_active,
            "active_branch_attribution_coverage_basis_points":
                share_basis_points(attributed_active, active_total),
            "protected_updates": protected.available.then_some(protected_total),
            "attributed_protected_updates":
                protected.available.then_some(attributed_protected),
            "unattributed_protected_updates": protected
                .available
                .then_some(unknown_protected + ambiguous_protected),
            "unknown_protected_updates": protected.available.then_some(unknown_protected),
            "ambiguous_protected_updates":
                protected.available.then_some(ambiguous_protected),
            "protected_update_attribution_coverage_basis_points": protected
                .available
                .then(|| share_basis_points(attributed_protected, protected_total)),
        },
        "operators": operator_rows,
        "tripwire_observed": any_tripwire,
        "tripwire_status": tripwire_status,
        "evaluation_complete": evaluation_complete,
        "semantics": {
            "active_branches": "current branch refs grouped by last attributable mover, not ownership",
            "protected_updates": "admitted non-creation ref updates matching the current protected policy, not proof of Git publication or merge commits",
        },
    })
}

/// Version of the operator-written review adjudication rows. Like the
/// newcomer adjudications, this file is not part of the signed op log but is
/// a persisted measurement input, so it carries the same explicit version.
const REVIEW_ADJUDICATION_FORMAT_VERSION: u64 = 1;

/// One operator judgement about a cohort contribution.
///
/// `Invalid` and `Slop` are deliberately distinct. Invalid is good-faith and
/// wrong, which is what newcomers do constantly and must not by itself trip a
/// Sybil wire. Slop is unresponsive work that burns reviewer time, which is
/// what the cited curl bands are actually about. Collapsing them is how a
/// competent newcomer having a bad week reads as an attack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Classification {
    Valid,
    Invalid,
    Slop,
    Unclear,
}

impl Classification {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "valid" => Some(Self::Valid),
            "invalid" => Some(Self::Invalid),
            "slop" => Some(Self::Slop),
            "unclear" => Some(Self::Unclear),
            _ => None,
        }
    }
}

/// Reads the operator's review classifications. Keyed by review id, so a
/// classification survives archiving even though the verdict bulk does not.
///
/// A row naming an unknown review is an error rather than a skipped line: a
/// typo that silently vanished would quietly shrink measured coverage.
fn read_review_adjudications(
    path: &std::path::Path,
    reviews: &BTreeMap<String, choir_view::ReviewState>,
) -> Result<(BTreeMap<String, Classification>, String), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read review adjudications: {e}"))?;
    let snapshot_hash = ContentHash::blake3(&bytes).to_hex();
    let text =
        String::from_utf8(bytes).map_err(|_| "review adjudications are not UTF-8".to_string())?;
    let mut rows = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(line)
            .map_err(|e| format!("review adjudication line {}: {e}", index + 1))?;
        if value["format_version"].as_u64() != Some(REVIEW_ADJUDICATION_FORMAT_VERSION) {
            return Err(format!(
                "review adjudication line {} has unsupported format_version",
                index + 1
            ));
        }
        let review_id = value["review_id"]
            .as_str()
            .ok_or_else(|| format!("review adjudication line {} needs review_id", index + 1))?;
        let classification = value["classification"]
            .as_str()
            .and_then(Classification::parse)
            .ok_or_else(|| {
                format!(
                    "review adjudication line {} needs classification valid|invalid|slop|unclear",
                    index + 1
                )
            })?;
        if !reviews.contains_key(review_id) {
            return Err(format!(
                "review adjudication line {} references an unknown review",
                index + 1
            ));
        }
        if rows.insert(review_id.to_string(), classification).is_some() {
            return Err(format!(
                "review adjudication line {} duplicates a review",
                index + 1
            ));
        }
    }
    Ok((rows, snapshot_hash))
}

/// The T2 cohort: durable, post-activation, non-incumbent actor keys, read
/// from the same T4 audit that defines "newcomer" everywhere else.
///
/// `None` means no cohort is defined — the audit is off, unreadable, or was
/// never activated — which is deliberately different from an empty cohort.
/// An empty cohort is the honest statement "no newcomers yet"; `None` is
/// "this node cannot tell you who is new".
fn new_actor_cohort(audit: Option<&Arc<Mutex<NewcomerAudit>>>) -> Option<BTreeSet<String>> {
    let audit = audit?.lock().expect("newcomer audit lock");
    (audit.available && audit.activated).then(|| audit.by_actor.keys().cloned().collect())
}

/// D24 T2 evidence for the new-actor cohort, and an explicit statement that
/// the tripwire itself is **not evaluable here**.
///
/// T2 asks for an invalid/slop rate. Choir persists no such judgement.
/// [`choir_view::Verdict::Approve`] means "may land" and
/// [`choir_view::Verdict::RequestChanges`] means "needs work", the latter is
/// overwritable by the former, and neither proves a contribution valid,
/// invalid, or slop. Relabelling one as "slop" would manufacture evidence the
/// model does not contain.
///
/// The unit is a **contribution offered**, not a completed review. A review is
/// opened by its own contributor, so a review-shaped denominator lets an actor
/// choose whether to be measured: submit a hundred slop changes, request
/// review on the three good ones, and a review-based rate reads zero. Reviews
/// remain a reported sub-metric.
///
/// Three counting rules exist to close laundering paths rather than to be
/// tidy. An archived review with no classification can never enter the
/// numerator or the denominator, because `--review-lapse-after-secs` turns an
/// unanswered review into `Archived { approved: false }` on a wall clock and
/// that must not become evidence. Pending reviews are reported, never counted.
/// And `load` needs no adjudication at all, so it keeps measuring during the
/// flood in which a classifier's coverage would collapse to `indeterminate`.
///
/// Requester attribution is joined from the `RequestReview` log entry's author
/// signature, never from the rendered review: archiving discards reviewer and
/// verdict detail, so a rendered row cannot supply historical evidence.
/// A `RequestReview` author is also not proven to have authored the commit
/// under review; that limitation ships in the response.
fn new_actor_review_outcomes_json(
    state: &ConcentrationState,
    view: &View,
    cohort: Option<&BTreeSet<String>>,
    adjudications_path: Option<&std::path::Path>,
) -> serde_json::Value {
    let adjudications =
        adjudications_path.map(|path| read_review_adjudications(path, &view.reviews));
    let (classifications, snapshot_hash, adjudications_available, adjudications_error) =
        match &adjudications {
            None => (BTreeMap::new(), None, false, None),
            Some(Ok((rows, hash))) => (rows.clone(), Some(hash.clone()), true, None),
            Some(Err(error)) => (BTreeMap::new(), None, false, Some(error.clone())),
        };

    let mut unknown_requester = 0usize;
    let mut unsigned_author = 0usize;
    let mut signed = 0usize;
    let mut attributed = 0usize;
    let mut approved = 0usize;
    let mut request_changes = 0usize;
    let mut pending = 0usize;
    let mut archived_classified = 0usize;
    let mut archived_evidence_lost = 0usize;
    let mut slashed = 0usize;
    let mut eligible = 0usize;
    let mut classified: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut review_rounds = 0usize;
    let mut review_rounds_unlanded = 0usize;
    let mut landed = 0usize;
    for (id, review) in &view.reviews {
        let Some(evidence) = state.review_requesters.get(id) else {
            unknown_requester += 1;
            continue;
        };
        let Some(key_id) = evidence.key_id.as_deref() else {
            unsigned_author += 1;
            continue;
        };
        signed += 1;
        let Some(cohort) = cohort else { continue };
        if !cohort.contains(key_id) {
            continue;
        }
        attributed += 1;
        if review.re_review_required() {
            slashed += 1;
        }
        let rounds = state
            .new_actor_load
            .verdict_rounds
            .get(id)
            .copied()
            .unwrap_or_default();
        review_rounds += rounds;
        if state.new_actor_load.landed.contains(id) {
            landed += 1;
        } else {
            review_rounds_unlanded += rounds;
        }
        let classification = classifications.get(id).copied();
        let counts_toward_rate = if matches!(review.status, ReviewStatus::Archived { .. }) {
            // Archiving destroys the verdicts an adjudicator needs, and a
            // lapse archives a review nobody ever answered. Without a
            // standing classification such a row is evidence of nothing.
            if classification.is_some() {
                archived_classified += 1;
                true
            } else {
                archived_evidence_lost += 1;
                false
            }
        } else if !review.complete() {
            pending += 1;
            false
        } else if review
            .verdicts
            .values()
            .any(|answer| answer.verdict == Verdict::RequestChanges)
        {
            request_changes += 1;
            true
        } else {
            approved += 1;
            true
        };
        if counts_toward_rate {
            eligible += 1;
            if let Some(classification) = classification {
                *classified
                    .entry(match classification {
                        Classification::Valid => "valid",
                        Classification::Invalid => "invalid",
                        Classification::Slop => "slop",
                        Classification::Unclear => "unclear",
                    })
                    .or_default() += 1;
            }
        }
    }

    let available = cohort.is_some();
    let submissions: usize = cohort
        .map(|cohort| {
            cohort
                .iter()
                .filter_map(|key| state.new_actor_load.submissions.get(key))
                .sum()
        })
        .unwrap_or_default();
    let adjudicated: usize = classified.values().sum();
    let classified_rows: BTreeMap<String, usize> = ["valid", "invalid", "slop", "unclear"]
        .into_iter()
        .map(|name| {
            (
                name.to_string(),
                classified.get(name).copied().unwrap_or_default(),
            )
        })
        .collect();
    serde_json::json!({
        "format_version": 2,
        "as_of_seq": state.as_of_seq,
        "unit": "contribution_offered",
        "cohort": {
            "definition": "post_activation_non_incumbent_actor_keys",
            "source": "newcomer_harm audit",
            "available": available,
            "actor_keys": cohort.map(BTreeSet::len),
        },
        "policy": {
            "graduation": null,
            "trailing_window": null,
            "tripwire_subject": null,
            "sampling": "census",
            "status": "unset_pending_operator_decision",
        },
        "load": available.then(|| serde_json::json!({
            "accepted_operations": submissions,
            "reviews_requested": attributed,
            "review_rounds": review_rounds,
            "review_rounds_on_unlanded": review_rounds_unlanded,
            "reviews_landed": landed,
            "reviews_never_landed": attributed - landed,
        })),
        "declared_tripwire": {
            "max_invalid_or_slop_percent": T2_MAX_INVALID_OR_SLOP_PERCENT,
            "min_valid_percent": T2_MIN_VALID_PERCENT,
            "evaluable": false,
            "blocked_on": "cohort exit, observation window and tripwire subject are unset, and census adjudication cannot survive a flood",
        },
        "totals": {
            "reviews": view.reviews.len(),
            "attributed_to_cohort": available.then_some(attributed),
            "excluded_outside_cohort": available.then(|| signed - attributed),
            "excluded_unsigned_author": unsigned_author,
            "excluded_unknown_requester": unknown_requester,
        },
        "review_outcomes": available.then(|| serde_json::json!({
            "approved": approved,
            "request_changes": request_changes,
            "pending": pending,
            "archived_classified": archived_classified,
            "archived_evidence_lost": archived_evidence_lost,
            "re_review_required": slashed,
        })),
        "adjudication": {
            "configured": adjudications_path.is_some(),
            "available": adjudications_available,
            "snapshot_hash": snapshot_hash,
            "error": adjudications_error,
            "eligible": available.then_some(eligible),
            "adjudicated": available.then_some(adjudicated),
            "coverage_basis_points": available.then(|| share_basis_points(adjudicated, eligible)),
            "classified": available.then_some(classified_rows),
            "independent": false,
        },
        "tripwire_observed": null,
        "tripwire_status": "indeterminate",
        "evaluation_complete": false,
        "semantics": {
            "unit": "a contribution offered by a cohort key; reviews are a sub-metric because the contributor opens their own review and can decline to",
            "accepted_operations": "every accepted signed operation authored by a cohort key, review participation included; counted because work that never reaches review is invisible to a review-shaped denominator",
            "load": "objective and adjudication-free, so it keeps measuring when a classifier's coverage collapses; a rate alone cannot see a flood",
            "approved": "live, every listed reviewer answered, and no verdict is RequestChanges — not a validity judgement",
            "request_changes": "live, every listed reviewer answered, and at least one standing verdict is RequestChanges — 'needs work', overwritable, not slop",
            "pending": "live and unassigned or not yet answered by every listed reviewer; reported, never counted",
            "archived_evidence_lost": "archived with no standing classification; a lapse archives an unanswered review on a wall clock, so it is excluded from both numerator and denominator",
            "re_review_required": "overlaps the buckets above; counts cohort reviews carrying a retroactive approval slash",
            "attribution": "the RequestReview log entry's signing key; that author is not proven to have authored the commit under review",
            "independent": "a single-operator node classifies contributions its own reviewers approved; this is self-adjudication, not an independent judgement",
            "indeterminate": "structural: the cohort has no exit, the window and tripwire subject are unset, and census adjudication goes blind in the flood it should detect",
        },
    })
}

/// Deterministic read-time measurement of the complete authoritative view.
///
/// The serialized total deliberately contains only the four sections owned by
/// `View`. Runtime projections are excluded so adding this report cannot make
/// its own byte count grow recursively, and adding another projection later
/// cannot rewrite the historical meaning of the measurement.
fn view_growth_json(
    counts: serde_json::Value,
    workspaces: &serde_json::Value,
    refs: &serde_json::Value,
    reviews: &serde_json::Value,
    provenance: &serde_json::Value,
    measured: &[(&str, &serde_json::Value)],
    as_of_seq: Option<u64>,
) -> serde_json::Value {
    let serialized_bytes = |value: &serde_json::Value| {
        serde_json::to_vec(value)
            .expect("materialized view JSON is always serializable")
            .len()
    };
    // `measured` is deliberately absent from the total below. The four
    // sections passed by name are the authoritative view and
    // `total_authoritative_view` is a tracked series; folding a fifth
    // section in would move every past reading and destroy comparability.
    // They are still *measured* — see the sibling byte counts — because a
    // map nobody counts is how a view grows without anyone noticing.
    //
    // A slice rather than one argument each, so the next section that
    // has to be watched without being totalled costs a call site and not
    // a signature. The two shapes are not interchangeable: an argument
    // added to the authoritative four changes what a historical number
    // means, and this one cannot.
    let authoritative = serde_json::json!({
        "workspaces": workspaces,
        "refs": refs,
        "reviews": reviews,
        "provenance": provenance,
    });
    let mut bytes = serde_json::json!({
        "workspaces": serialized_bytes(workspaces),
        "refs": serialized_bytes(refs),
        "reviews": serialized_bytes(reviews),
        "provenance": serialized_bytes(provenance),
        "total_authoritative_view": serialized_bytes(&authoritative),
    });
    for (name, value) in measured {
        bytes[*name] = serde_json::json!(serialized_bytes(value));
    }
    serde_json::json!({
        "format_version": 1,
        "as_of_seq": as_of_seq,
        "counts": counts,
        "serialized_bytes": bytes,
    })
}

fn view_growth_counts(view: &View) -> serde_json::Value {
    let live_reviews = view
        .reviews
        .values()
        .filter(|review| matches!(review.status, ReviewStatus::Live))
        .count();
    let provenance_records = view.provenance.values().map(BTreeMap::len).sum::<usize>();
    // Revoked bindings are counted, never subtracted: the row survives
    // revocation, so a `bindings` count that dropped on revoke would
    // understate the map it is meant to size.
    let revoked_bindings = view
        .bindings
        .values()
        .filter(|binding| binding.is_revoked())
        .count();
    // Edges, not subjects: the outer map is what paging bounds, and the
    // inner one is where the graph actually grows. Counting rows would
    // report one number for an operator with a single vouch and for one
    // every operator on the node stands behind.
    let vouch_edges = view.vouches.values().map(BTreeMap::len).sum::<usize>();
    // One row per witness and no more, which is the claim worth
    // measuring: this section is bounded by the witness population, not
    // by how many snapshots the node has taken.
    let witnesses = view.witnessed.len();
    let witnesses_current = view.latest_snapshot.as_ref().map_or(0, |latest| {
        let id = latest.id();
        view.witnessed
            .values()
            .filter(|state| state.snapshot == id)
            .count()
    });
    serde_json::json!({
        "workspaces": view.workspaces.len(),
        "refs": view.refs.len(),
        "reviews": view.reviews.len(),
        "live_reviews": live_reviews,
        "archived_reviews": view.reviews.len() - live_reviews,
        "provenance_subjects": view.provenance.len(),
        "provenance_records": provenance_records,
        "bindings": view.bindings.len(),
        "revoked_bindings": revoked_bindings,
        "vouch_subjects": view.vouches.len(),
        "vouch_edges": vouch_edges,
        "witnesses": witnesses,
        "witnesses_current": witnesses_current,
    })
}

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
    fn push(&mut self, entry: OpEntry, hash: ContentHash) {
        // `signing_hash` covers only (channel, payload); `content_hash`
        // would serialize the whole entry, and this runs per admitted op
        // -- which is why `hash` is passed in by the sequencer that
        // already computed it rather than derived here.
        self.by_signing.insert(entry.signing_hash(), entry.seq);
        self.by_hash.insert(hash, entry.seq);
        self.entries.push_back(entry);
        self.trim();
    }

    /// Whether `head` is an entry still inside the window, which is what
    /// makes an op signed against it admissible.
    fn holds_head(&self, head: &ContentHash) -> bool {
        self.by_hash.contains_key(head)
    }

    /// The head a client should sign its next scope against.
    ///
    /// Derived rather than stored: keeping it would mean cloning a hash
    /// on every admitted op to serve a value only read by `/api/view`
    /// and by the ops the daemon signs itself. `None` for an empty
    /// window, which is also a window that would admit no head.
    fn head_hash(&self) -> Option<ContentHash> {
        self.entries.back().map(OpEntry::content_hash)
    }

    /// The seq an identical submission already landed as, if any. The
    /// caller has usually just computed the signing hash for the
    /// signature check, so it is taken rather than recomputed.
    fn seq_for_signing(&self, signing: &ContentHash) -> Option<u64> {
        self.by_signing.get(signing).copied()
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
                // The dropped entry's own hash is the new front's
                // `parent`, so its eviction costs a lookup instead of a
                // re-serialization. An emptied window holds no heads at
                // all, which is the only case with no successor to ask.
                match self.entries.front().and_then(|e| e.parent.as_ref()) {
                    Some(parent) => {
                        self.by_hash.remove(parent);
                    }
                    None => self.by_hash.clear(),
                }
            }
            self.base += 1;
        }
    }
}

/// Replays the platform's runtime projections in one pass. Concentration
/// attribution needs the entry author as well as the typed payload, while
/// review retention additionally needs request order. Neither belongs in
/// the persisted `ViewOp` format, and neither justifies a second log scan.
fn materialize_platform_state(
    log: &dyn OpLog,
    retention_config: Option<ReviewRetention>,
) -> Result<
    (
        View,
        Option<ReviewRetentionState>,
        ConcentrationState,
        crate::quota::WorkspaceTally,
    ),
    choir_view::ViewError,
> {
    let mut view = View::default();
    let mut retention = retention_config.map(ReviewRetentionState::new);
    let mut concentration = ConcentrationState::default();
    // D37. Folded in this loop rather than in a pass of its own, which is
    // the whole reason a per-user workspace ceiling needs no persisted
    // file: the log a restart already replays is the tally's storage.
    let mut workspace_tally = crate::quota::WorkspaceTally::default();
    // Stored entries have no timestamp. Giving every pre-existing live
    // review `now` starts a fresh grace period after restart, which can
    // delay an incomplete-review lapse but can never trigger one early.
    let observed_at =
        retention_config.and_then(|config| config.lapse_after.map(|_| Instant::now()));
    for seq in 0..log.len() {
        let entry = log.get(seq).expect("seq < len");
        let op = ViewOp::from_payload(&entry.payload)?;
        view.apply(&op)?;
        concentration.observe(&entry, &op, &view);
        workspace_tally.observe(&entry, &op);
        if let Some(retention) = &mut retention {
            retention.observe(&op, observed_at);
        }
    }
    Ok((view, retention, concentration, workspace_tally))
}

/// Verify author signature, then CAS against the shared view. Runs on
/// the sequencer's writer thread; API readers share the view mutex.
/// The op variant's name, from its own externally-tagged serialization
/// rather than a hand-written match: a variant added later journals
/// correctly without anyone remembering to extend a table here.
fn op_type_name(op: &ViewOp) -> Option<String> {
    serde_json::to_value(&op.kind)
        .ok()
        .and_then(|v| v.as_object().and_then(|o| o.keys().next().cloned()))
}

/// A journal slot the builder fills after the writer thread is running.
///
/// [`Sequencer::spawn_with_journal`] takes its journal by value at
/// construction, but every `with_*` builder runs afterwards, so the
/// value handed to the writer has to be a slot rather than a journal.
/// Same shape as the hot-reloadable config beside it.
///
/// `on` is separate from the lock on purpose: [`journal::Journal::enabled`]
/// is consulted once per op on the writer thread, and a node with no
/// journal should pay one relaxed atomic load, not a mutex acquisition.
#[derive(Clone, Default)]
struct SharedJournal {
    inner: Arc<Mutex<Option<Box<dyn journal::Journal>>>>,
    on: Arc<std::sync::atomic::AtomicBool>,
}

impl SharedJournal {
    /// Installs `journal`, and switches recording on.
    fn install(&self, journal: Box<dyn journal::Journal>) {
        if let Ok(mut slot) = self.inner.lock() {
            *slot = Some(journal);
            self.on.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

impl journal::Journal for SharedJournal {
    fn record(&self, event: journal::Event) {
        if !self.enabled() {
            return;
        }
        if let Ok(slot) = self.inner.lock() {
            if let Some(journal) = slot.as_ref() {
                journal.record(event);
            }
        }
    }

    fn enabled(&self) -> bool {
        self.on.load(std::sync::atomic::Ordering::Relaxed)
    }
}

struct ChoirPolicy {
    /// Set when this node is a seed (D80). Every submission is then
    /// refused before anything else is asked: a seed's log is written by
    /// its replicator alone, through [`SubmitPolicy::replicable`].
    home: Arc<std::sync::OnceLock<crate::replica::Home>>,
    /// Where this policy reports CAS failures. The sequencer cannot:
    /// a refusal reaches it as an opaque string, so contention and
    /// nonsense look identical from there.
    journal: SharedJournal,
    /// What the last `check` identified, handed to the journal through
    /// [`SubmitPolicy::subject`] rather than derived a second time.
    subject: (Option<String>, Option<String>),
    registry: Registry,
    view: Arc<Mutex<View>>,
    entries: Arc<Mutex<LogWindow>>,
    /// When set, the trusted-keys file is checked before every signature;
    /// registering or removing a key takes effect on its next submission.
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
    /// The operator's ACL file, read here for [`crate::acl::Level::Own`]
    /// grants alone (D42).
    ///
    /// Deliberately the *file*, not the merged table the HTTP layer
    /// enforces. Self-service (D36) contributes grants to that merge, and
    /// ownership authorizes a landing on a protected ref, so reading the
    /// merge would make ownership reachable by whatever self-service can
    /// issue. Reading the file makes "only the operator grants ownership"
    /// true by construction rather than by auditing the issuer.
    acl_file: Arc<Mutex<Option<std::path::PathBuf>>>,
    /// When set, a protected ref only moves to a commit some approved
    /// review already named — the landing half of the gate.
    require_review: Arc<std::sync::atomic::AtomicBool>,
    /// When set, every submission must carry a [`choir_view::OpScope`]
    /// naming this node and a head still in the window. Shared with
    /// [`Platform`], like the other gates.
    require_scope: Arc<std::sync::atomic::AtomicBool>,
    /// Actor id → the channel name that key is bound to,
    /// for keys whose trusted-keys line carries a name. Keys absent from
    /// this map are unconstrained, which is what every key was before the
    /// name column existed.
    ///
    /// Shared with [`Platform`], because a *tightening* must not wait for
    /// an unrelated event: the accept loop refreshes it on mtime change,
    /// while submission admission refreshes the signature registry too.
    key_names: Arc<Mutex<KeyBindings>>,
    /// Entry-author-aware runtime projection for D24 T3. Updated on the
    /// writer thread immediately after the ordinary view fold.
    concentration: Arc<Mutex<ConcentrationState>>,
    /// Present only when the operator enabled review retention. Updated
    /// after the view fold on the same writer thread, so its FIFO order
    /// matches the sequencer order exactly.
    review_retention: Option<Arc<Mutex<ReviewRetentionState>>>,
    /// The credential store, when the node has one (D39). Shared with
    /// [`Platform`] as a slot rather than taken at construction because
    /// the two are enabled independently and in either order; a node
    /// without accounts simply has no passkey authors.
    passkeys: Arc<Mutex<Option<Arc<crate::accounts::Accounts>>>>,
    /// Webhook delivery handle (D32), shared with [`Platform`] so the
    /// operator's `--hooks-file` can be attached after the writer thread
    /// is already running. Offering an event to it never blocks: see
    /// [`crate::hooks`] for why invariant 5 forbids anything else here.
    hooks: Arc<Mutex<Option<crate::hooks::Hooks>>>,
    /// Which channel holds which workspace (D37). Folded here for the
    /// reason `concentration` is: it needs the entry's channel as well as
    /// the typed payload, and it must advance in the same single-writer
    /// step as the view or the two can disagree about what exists.
    workspace_tally: Arc<Mutex<crate::quota::WorkspaceTally>>,
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
        for signer in &signers {
            registry.register(&signer.key).ok();
        }
        self.registry = registry;
        *self.key_names.lock().expect("key names lock") = KeyBindings::from_signers(&signers);
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
        match names.bound_name(actor_id) {
            Some(bound) if bound != channel => Err(Rejection::new(
                Code::ChannelNotOwned,
                "this key is bound to a different channel",
                "submit on the channel your key is bound to, or ask the operator to bind a \
                 key to the channel you want",
            )
            .with_states(Some(bound.to_string()), Some(channel.to_string()))
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
        let text = std::fs::read_to_string(path).map_err(|e| {
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

    /// The operator's ACL, parsed, or `None` when no file is configured.
    ///
    /// Read per call rather than cached, so granting ownership takes
    /// effect with no restart — the same trade [`Self::ref_is_protected`]
    /// makes, and paid on the same narrow path, since both are reached
    /// only for a landing that is already gated.
    ///
    /// Fails **closed**. An unreadable or malformed file refuses the
    /// landing rather than concluding there are no owners, because "no
    /// owners" is precisely the branch that falls back to the weaker
    /// rule: a gate that loses its policy file must not quietly demote
    /// itself to the policy it was configured to replace.
    fn acl_now(&self) -> Result<Option<crate::acl::Effective>, String> {
        let guard = self.acl_file.lock().expect("acl file lock");
        let Some(path) = guard.as_ref() else {
            return Ok(None);
        };
        let text = std::fs::read_to_string(path).map_err(|e| {
            Rejection::new(
                Code::PolicyUnavailable,
                format!("acl file unreadable: {e}"),
                "this is an operator problem, not a client one: the ownership gate fails \
                 closed rather than guessing. Retry once the operator restores the file",
            )
            .encode()
        })?;
        crate::acl::Acl::parse(&text)
            .map(|table| Some(table.at(crate::accounts::now_secs())))
            .map_err(|e| {
                Rejection::new(
                    Code::PolicyUnavailable,
                    format!("acl file unparseable: {e}"),
                    "ask the operator to repair the ACL file; the ownership gate refuses rather \
                 than enforcing a table it only partly understands",
                )
                .encode()
            })
    }

    /// The ACL user this submission acts as, when one can be established
    /// from evidence rather than from a claim (D42).
    ///
    /// Three populations reach admission and only two of them resolve:
    ///
    /// - **A transport push.** `provenance` is a label only the node may
    ///   apply — refused for any other signer earlier in `check` — so a
    ///   [`Provenance::PushTransport`] op carries a channel the node
    ///   synthesized from the authenticated HTTP user. `git/<user>` is
    ///   therefore evidence about who pushed, not a claim by them.
    /// - **An author-signed op.** The identity is the verified signing
    ///   key, and the operator's trusted-keys name column is what binds a
    ///   key to a name. An **unbound key resolves to nobody**: unbound
    ///   keys are deliberately unconstrained in their choice of channel,
    ///   so honouring the channel there would let any trusted key call
    ///   itself an owner.
    /// - **A certified push.** The channel is `key/<signer>`, naming the
    ///   push certificate's key rather than an ACL user, and the
    ///   authenticated user is not carried into the op. Resolves to
    ///   nobody, so a `git push --signed` cannot assert ownership; see
    ///   the rejection in [`Self::landing_is_authorized`], which says so
    ///   rather than reporting a generic refusal.
    fn acting_user(&self, sub: &Submission, op: &ViewOp, actor_id: &ContentHash) -> Option<String> {
        match op.provenance {
            Some(Provenance::PushTransport) => {
                sub.channel.strip_prefix("git/").map(ToString::to_string)
            }
            Some(Provenance::PushCertified) => None,
            None => {
                let names = self.key_names.lock().expect("key names lock");
                names.bound_name(actor_id).map(ToString::to_string)
            }
        }
    }

    /// The landing half of the protected-ref gate: may this submission
    /// move this protected ref to this commit?
    ///
    /// Two rules, and which one applies is a property of the *repository*
    /// rather than of the actor (D42):
    ///
    /// - **Somebody owns the repository.** An owner's assent is necessary
    ///   and sufficient. No quantity of non-owner approval substitutes,
    ///   and no second opinion is required alongside it.
    /// - **Nobody owns it.** The pre-D42 rule stands untouched: approval
    ///   weight `REQUIRED_APPROVAL_WEIGHT`, from that many distinct
    ///   operators.
    ///
    /// So a node with no `--acl-file`, or one whose ACL grants nobody
    /// `own`, behaves exactly as every node did before D42. Ownership is
    /// opt-in per repository, and opting one repository in leaves every
    /// other one alone.
    ///
    /// **Returns the authorization rather than a bare yes** (D43). The
    /// gate is evaluated at apply time against files that are not in the
    /// log, so this function is the only place that knows why a landing
    /// was allowed. A [`OpKind::Submit`] records that answer, and the
    /// brief's constraint is that the record must come out of the
    /// evaluation that admitted the op and not a second pass that
    /// recomputes it — two computations can disagree, and a field that
    /// can disagree with the decision it describes is decoration. So
    /// there is one function, the `SetRef` path discards its value, and
    /// the `Submit` path writes it down.
    ///
    /// `review` narrows the weight rule to a single named review; `None`
    /// keeps the pre-D43 behaviour of taking the best of every review
    /// naming this `(ref, commit)`.
    fn authorization_for(
        &self,
        name: &str,
        commit: &ContentHash,
        review: Option<&str>,
        sub: &Submission,
        op: &ViewOp,
        actor_id: &ContentHash,
    ) -> Result<Authorization, String> {
        let acl = self.acl_now()?;
        let repo = crate::acl::ref_repo(name);
        if let (Some(acl), Some(repo)) = (acl.as_ref(), repo.as_ref()) {
            if acl.has_owner(repo) {
                return self.owner_assented(acl, repo, name, commit, review, sub, op, actor_id);
            }
        }
        let (approval_weight, approvers) = self.weight_and_approvers(name, commit, review)?;
        if approval_weight < REQUIRED_APPROVAL_WEIGHT {
            return Err(Rejection::new(
                Code::ReviewRequired,
                format!(
                    "{name} is protected and this commit has approval weight \
                     {approval_weight}, below the required {REQUIRED_APPROVAL_WEIGHT}"
                ),
                "open a review naming this ref and commit (`choir review ... --ref \
                 <repo:ref>`), obtain approvals from two distinct operators, then push again",
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
        Ok(Authorization::new(
            Basis::ApprovalWeight {
                required: u32::try_from(REQUIRED_APPROVAL_WEIGHT).unwrap_or(u32::MAX),
                met: u32::try_from(approval_weight).unwrap_or(u32::MAX),
            },
            approvers,
        ))
    }

    /// The approval weight backing a landing, and the actor ids of the
    /// approvals it counted (D43).
    ///
    /// The two come from one call because they must describe the same
    /// review: `approval_weight_for` takes the maximum over every review
    /// naming `(ref, commit)`, so a weight and a separately-derived
    /// approver list could easily belong to different rows.
    ///
    /// Approver ids are resolved only when a `Submit` asked for them
    /// (`review` is `Some`). A plain push does not need them and must not
    /// be refused for an unbound reviewer key, which would change the
    /// pre-D43 push gate.
    fn weight_and_approvers(
        &self,
        name: &str,
        commit: &ContentHash,
        review: Option<&str>,
    ) -> Result<(usize, Vec<ContentHash>), String> {
        let Some(review) = review else {
            return Ok((self.approval_weight_for(name, commit), Vec::new()));
        };
        let view = self.view.lock().expect("view lock");
        let Some(state) = view.reviews.get(review) else {
            return Ok((0, Vec::new()));
        };
        if state.target_ref.as_deref() != Some(name) || state.target.as_ref() != Some(commit) {
            return Ok((0, Vec::new()));
        }
        if !state.approved() {
            return Ok((0, Vec::new()));
        }
        let approvers = state
            .counted_approvers()
            .into_iter()
            .map(|(channel, at)| view.bound_actor_at(channel, at))
            .collect::<Result<Vec<_>, String>>()
            .map_err(|e| Self::unbound_approver(&e))?;
        Ok((state.approval_weight(), approvers))
    }

    /// The rejection for an approval this node cannot name an actor id
    /// for (D43).
    ///
    /// A landing whose approvers cannot be identified is refused rather
    /// than recorded with a gap. That is a liveness cost on a node with
    /// no `BindKey` records, and it buys the property the whole record
    /// exists for: `channel_is_owned` constrains *bound* keys only, so
    /// an approval from an unbound channel is one nobody can be held to.
    fn unbound_approver(reason: &str) -> String {
        Rejection::new(
            Code::ReviewRequired,
            format!("this landing cannot name its approvers: {reason}"),
            "a merge records who approved it as actor ids, which it reads from the log's own \
             key bindings. Ask the operator to bind that reviewer's key (`choir bind-key`), \
             then merge again",
        )
        .encode()
    }

    /// Whether an owner of `repo` assented to this landing, in either of
    /// the two ways that count (D42).
    ///
    /// One question with two answers, not a rule plus an exception:
    /// performing the landing is assent, and so is having approved a
    /// review that names this exact `(ref, commit)`. The first is what
    /// lets an owner land their own work without reviewing themselves —
    /// which they could not do anyway, since the reviewer draw excludes
    /// the requester's own operator.
    ///
    /// Returns which of the two answers applied, so a [`OpKind::Submit`]
    /// can record it (D43). The order is load-bearing and is the order
    /// the two answers are checked in: an owner who both approved and
    /// landed is recorded as having landed, because that is the assent
    /// the gate actually rested on.
    ///
    /// **The two answers match the ACL's user column against different
    /// namespaces, and an operator writing that file has to know which.**
    /// The landing answer asks about [`Self::acting_user`] — for a push
    /// that is the transport channel minus its `git/` prefix, which is
    /// the `--auth-file` username; for a signed op it is the *bound name*
    /// of the signing key. The approval answer asks about a reviewer
    /// channel, the key of [`choir_view::ReviewState::verdicts`], spelled
    /// the way the reviewer pool spells it (`someone/reviewer`).
    ///
    /// So `alice choir/choir.git own` grants the landing answer and never
    /// the approval one, and `alice/reviewer choir/choir.git own` grants
    /// the reverse. Neither is wrong and nothing warns which was meant.
    /// Granting the wrong spelling still flips [`crate::acl::Effective::has_owner`],
    /// which switches the repository out of the approval-weight rule —
    /// so a mismatched grant does not fall back, it narrows the gate to a
    /// rule the intended actor cannot satisfy.
    ///
    /// For an owner landing their own work the landing answer is the only
    /// reachable one regardless, because the reviewer draw excludes the
    /// requester's own operator.
    #[allow(clippy::too_many_arguments)]
    fn owner_assented(
        &self,
        acl: &crate::acl::Effective,
        repo: &str,
        name: &str,
        commit: &ContentHash,
        review: Option<&str>,
        sub: &Submission,
        op: &ViewOp,
        actor_id: &ContentHash,
    ) -> Result<Authorization, String> {
        let acting = self.acting_user(sub, op, actor_id);
        if let Some(user) = acting.as_deref() {
            if acl.allows_repo(user, repo, crate::acl::Level::Own) {
                return Ok(Authorization::new(
                    Basis::OwnerLanded {
                        owner: user.to_string(),
                    },
                    Vec::new(),
                ));
            }
        }
        if let Some((owner, approved_at)) = self.owner_approved(acl, repo, name, commit, review) {
            // Only a `Submit` needs the id, and only a `Submit` may be
            // refused for the want of one. Resolving it on the push path
            // too would make an unbound reviewer key break a landing that
            // D42 admits today, which is a gate change wearing the shape
            // of a record change.
            let approvers = match review {
                Some(_) => vec![self
                    .view
                    .lock()
                    .expect("view lock")
                    .bound_actor_at(&owner, approved_at)
                    .map_err(|e| Self::unbound_approver(&e))?],
                None => Vec::new(),
            };
            return Ok(Authorization::new(
                Basis::OwnerApproved { owner },
                approvers,
            ));
        }
        // A certified push fails here for a reason the generic message
        // would misdescribe: not "you are not an owner" but "the node
        // cannot tell who you are", which has a different repair.
        let next = if matches!(op.provenance, Some(Provenance::PushCertified)) {
            "a signed push cannot assert repository ownership: its channel names the push \
             certificate's key rather than an ACL user. Push without `--signed`, or have an \
             owner approve a review naming this ref and commit"
        } else {
            "have an owner of this repository approve a review naming this ref and commit, \
             or land it as an owner yourself. An owner submitting directly must have their \
             key bound to their name in the trusted-keys file, or the node cannot tell the \
             key is theirs"
        };
        Err(Rejection::new(
            Code::ReviewRequired,
            format!(
                "{name} is protected and {repo} is owned; no owner has assented to this commit"
            ),
            next,
        )
        .with_states(
            Some(format!("owner assent for {}", commit.to_hex())),
            Some(match acting {
                Some(user) => format!("a landing by {user}, who does not own {repo}"),
                None => "a landing by an identity the node cannot resolve to an ACL user".into(),
            }),
        )
        .encode())
    }

    /// Whether an owner of `repo` approved a review naming exactly this
    /// `(ref, commit)` pair (D42).
    ///
    /// Deliberately **not** routed through [`choir_view::ReviewState::approved`],
    /// which returns false until every drawn reviewer has answered. Under
    /// D42 an owner's approval is sufficient on its own, so consulting
    /// `approved()` would let one reviewer who never replies veto a
    /// landing the owner had already assented to — a liveness bug wearing
    /// the shape of a safety check.
    ///
    /// A slashed approval does not count. A retroactively invalidated
    /// verdict is invalidated for an owner exactly as for anybody else,
    /// which is the whole point of `SlashApproval` existing.
    ///
    /// **The slash test is per operator, not per channel** — the same
    /// predicate the weight rule applies, via
    /// [`choir_view::ReviewState::approval_stands`]. D42 shipped this
    /// check spelled `slashes.contains_key(reviewer)`, which let a
    /// slashed operator's *other* channel keep authorizing while its
    /// weight contribution was already gone. Two answers to "is this
    /// approval still good" is one more than a gate may have.
    ///
    /// Returns the approving owner's channel, which a
    /// [`OpKind::Submit`] records (D43). `review` narrows the search to
    /// one named review; `None` searches every review naming this
    /// `(ref, commit)`, which is the pre-D43 push behaviour.
    fn owner_approved(
        &self,
        acl: &crate::acl::Effective,
        repo: &str,
        name: &str,
        commit: &ContentHash,
        review: Option<&str>,
    ) -> Option<(String, u64)> {
        let view = self.view.lock().expect("view lock");
        view.reviews
            .iter()
            .filter(|(id, state)| {
                review.is_none_or(|wanted| wanted == id.as_str())
                    && state.target_ref.as_deref() == Some(name)
                    && state.target.as_ref() == Some(commit)
            })
            .find_map(|(_, state)| {
                state.verdicts.keys().find_map(|reviewer| {
                    // The position comes back with the name because the
                    // approval has to be resolved to the key that was
                    // live when it was cast, not the one holding the
                    // channel now (D44).
                    let at = state.standing_approval_at(reviewer)?;
                    acl.allows_repo(reviewer, repo, crate::acl::Level::Own)
                        .then(|| (reviewer.clone(), at))
                })
            })
    }

    /// Admits a [`OpKind::Submit`] only if the authorization it carries
    /// is the one this node's own gate produces (D43).
    ///
    /// The op is author-signed, so `authorization` arrives as a claim.
    /// It is never trusted and never patched: the gate runs, and the
    /// claim must equal its result exactly. A client that writes its own
    /// basis gets a rejection naming both sides, not a landing.
    ///
    /// Refused outright when no rule would examine the landing — the
    /// operator is not running the review gate, or the ref is not
    /// protected. `Submit`'s entire value is the record, and a record of
    /// a decision nothing made is worse than no record: it reads, to
    /// every later auditor, exactly like one that was checked.
    #[allow(clippy::too_many_arguments)]
    fn submit_is_authorized(
        &self,
        gating: bool,
        review: &str,
        name: &str,
        commit: &ContentHash,
        claimed: &Authorization,
        sub: &Submission,
        op: &ViewOp,
        actor_id: &ContentHash,
    ) -> Result<(), String> {
        if !gating || !self.ref_is_protected(name)? {
            return Err(Rejection::new(
                Code::ReviewRequired,
                format!(
                    "{name} is not gated on this node, so a landing record for it would \
                     assert a review that nothing performed"
                ),
                "move the ref with an ordinary `SetRef`, or ask the operator to protect this \
                 ref and enable `--require-review`",
            )
            .encode());
        }
        let computed = self.authorization_for(name, commit, Some(review), sub, op, actor_id)?;
        if computed == *claimed {
            return Ok(());
        }
        // `expected` carries the gate's answer as its own canonical JSON,
        // not a summary of it. That is what makes the mismatch a usable
        // two-step protocol rather than a dead end: a client submits,
        // reads the authorization it should have signed, and submits
        // again. The alternative is a second implementation of this rule
        // on the read path so a client could ask in advance -- and a
        // record that can disagree with the decision it describes is the
        // one thing this field must never be.
        Err(Rejection::new(
            Code::ReviewRequired,
            format!(
                "the authorization on this submit is not the one {name}'s gate produced: \
                 it admits this landing as {}, and the submit claims {}",
                Self::basis_summary(&computed),
                Self::basis_summary(claimed)
            ),
            "sign and resubmit with the authorization in `expected`, verbatim. It is the \
             gate's own answer and a client cannot assert it",
        )
        .with_states(
            serde_json::to_string(&computed).ok(),
            serde_json::to_string(claimed).ok(),
        )
        .encode())
    }

    /// One-line rendering of an authorization, for the prose half of a
    /// mismatch rejection. Approvers are counted rather than listed: the
    /// hex ids would bury the part that differs.
    fn basis_summary(authorization: &Authorization) -> String {
        let basis = match &authorization.basis {
            Basis::OwnerLanded { owner } => format!("landed by owner {owner}"),
            Basis::OwnerApproved { owner } => format!("approved by owner {owner}"),
            Basis::ApprovalWeight { required, met } => {
                format!("approval weight {met} against a required {required}")
            }
        };
        format!("{basis} with {} approver(s)", authorization.approvers.len())
    }

    /// Refuses a submission that has already been admitted, and one whose
    /// author never bound it to this log at all.
    ///
    /// Together these are the replay defence, and they compose into
    /// at-most-once permanently even though both indexes are bounded by
    /// the window. The argument is short enough to check: a scoped op is
    /// admissible only while the head it names is still in the window;
    /// its own signing hash entered the window at a *later* sequence
    /// than that head, so whenever the head is still there the signing
    /// hash is too and the duplicate check refuses it. Once the head is
    /// gone the scope check refuses it. There is no sequence at which
    /// neither fires.
    ///
    /// Without a scope only the duplicate half applies, which bounds a
    /// replay to the window instead of refusing it outright — the reason
    /// `--require-scope` exists.
    fn admit_once(&self, signing: &ContentHash, op: &ViewOp) -> Result<(), String> {
        // Nothing is cloned out of the window here. Every admitted op runs
        // this, while only a refused one needs the head to explain itself,
        // so the head is read again on the rejection path instead of
        // copied on the hot one -- one allocation per op, which the
        // allocation budget notices.
        let (already, holds_head, nothing_evicted) = {
            let window = self.entries.lock().expect("entries lock");
            (
                window.seq_for_signing(signing),
                op.scope
                    .as_ref()
                    .and_then(|s| s.head.as_ref())
                    .is_some_and(|h| window.holds_head(h)),
                window.base == 0,
            )
        };
        let window_head = || {
            self.entries
                .lock()
                .expect("entries lock")
                .head_hash()
                .as_ref()
                .map(ContentHash::to_hex)
        };
        // A signature is admissible once. `prev` cannot enforce that: it
        // compares state, and state recurs — land a commit, revert it,
        // and the reverted-away op's CAS matches again. The HTTP layer
        // answers any rejection whose submission already landed as 200
        // `already_applied` with the original seq, so a lost-response
        // retry still reads as success while a replay becomes a no-op.
        if let Some(seq) = already {
            return Err(Rejection::new(
                Code::DuplicateSubmission,
                format!("these exact signed bytes already landed at seq {seq}"),
                "if you are retrying, read `seq` from this response — it names the op you \
                 already have. If you meant a second, distinct change, sign a new op: two \
                 otherwise byte-identical ops are told apart by their scope.",
            )
            .with_states(Some(seq.to_string()), None)
            .encode());
        }
        let Some(scope) = &op.scope else {
            if self
                .require_scope
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                return Err(Rejection::new(
                    Code::ScopeRequired,
                    "this node admits only ops signed for its own log and a recent head",
                    "read `log.node` and `log.head` from GET /api/view, put them in the \
                     op's `scope`, and sign that; `choir submit` does it for you",
                )
                .with_states(None, window_head())
                .encode());
            }
            return Ok(());
        };
        if scope.node != self.node_id {
            return Err(Rejection::new(
                Code::ForeignScope,
                "this op was signed for another node's log",
                "sign a scope naming this node; its id is in `actual` and in `log.node` \
                 of GET /api/view",
            )
            .with_states(Some(scope.node.to_hex()), Some(self.node_id.to_hex()))
            .encode());
        }
        match &scope.head {
            Some(_) if holds_head => Ok(()),
            // The op names no head, which is what a client signs when it
            // read an empty log — including every op of a batch it signed
            // in one go, since only the first of those lands against a
            // log that is still empty.
            //
            // Admissible while this window has evicted nothing, because
            // that is exactly the span the duplicate index above covers
            // in full: an unevicted window holds every entry, so a replay
            // of a headless op cannot slip past it. The moment the first
            // entry is evicted the guarantee would thin out, and the op
            // stops being admissible instead.
            None if nothing_evicted => Ok(()),
            None => Err(Rejection::new(
                Code::StaleScope,
                "the op names no head, and this log has evicted entries since",
                "re-read `log.head` from GET /api/view and sign a fresh op against it",
            )
            .with_states(
                Some("a log with nothing evicted".to_string()),
                window_head(),
            )
            .encode()),
            Some(head) => Err(Rejection::new(
                Code::StaleScope,
                "the head this op was signed against is no longer in the window",
                "re-read `log.head` from GET /api/view and sign a fresh op against it; a \
                 signature stays admissible only as long as the head it names does",
            )
            .with_states(Some(head.to_hex()), window_head())
            .encode()),
        }
    }
}

impl ChoirPolicy {
    /// Verifies a passkey submission against the credential the *channel*
    /// enrolled (D39).
    ///
    /// The channel is the account name, and that is the whole binding:
    /// `Accounts::passkey_spki` is keyed by `(account, credential_id)`, so
    /// an assertion by alice's authenticator submitted on bob's channel
    /// finds no key and is refused. There is no separate table mapping
    /// credentials to channels, because the store already is one — and a
    /// second table would be a second thing to keep in agreement.
    ///
    /// The returned actor id is `blake3` of the credential's public key,
    /// the same rule [`choir_identity::ActorKey::actor_id`] uses for an
    /// ed25519 key. It is an in-process authorization handle only: an
    /// `OpEntry` records the channel and the signature, never an actor
    /// id, so this derivation is not frozen into the log and changing it
    /// later would not be a migration.
    fn verify_passkey(
        &self,
        signing: &ContentHash,
        sig: &Witness,
        channel: &str,
    ) -> Result<ContentHash, choir_identity::IdentityError> {
        let store = self.passkeys.lock().expect("passkey store lock").clone();
        let Some(store) = store else {
            // No store at all: the node has no self-service, so it has no
            // enrolled credentials and this key id is unknown to it.
            return Err(choir_identity::IdentityError::UnknownKey(
                sig.key_id.clone(),
            ));
        };
        let spki = store
            .passkey_spki(channel, &sig.key_id)
            .ok_or_else(|| choir_identity::IdentityError::UnknownKey(sig.key_id.clone()))?;
        choir_identity::verify_webauthn_assertion(&spki, signing, sig)?;
        Ok(ContentHash::blake3(&spki))
    }
}

impl SubmitPolicy for ChoirPolicy {
    fn check(&mut self, sub: &Submission) -> Result<(), String> {
        if let Some(home) = self.home.get() {
            self.subject = (None, None);
            return Err(crate::reject::not_home(&home.url).to_string());
        }
        let sig = sub.author_sig.as_ref().ok_or("unsigned submission")?;
        // Refresh before verification so removing a trusted key takes
        // effect on that key's very next request. A failed verification
        // cannot trigger this tightening: a removed key is still present
        // in the stale registry and would verify successfully.
        self.reload_keys();
        let signing = choir_oplog::signing_hash(&sub.channel, &sub.payload);
        let mut verified_actor = if sig.scheme_id() == choir_oplog::scheme::WEBAUTHN_ES256 {
            self.verify_passkey(&signing, sig, &sub.channel)
        } else {
            self.registry.verify_signing_hash(&signing, sig)
        };
        // Retry a failed signature in case the file changed between the
        // pre-verification metadata check and this verification. Only the
        // ed25519 path has a file behind it; a passkey lives in the store
        // and is read fresh on every call already.
        if verified_actor.is_err()
            && sig.scheme_id() != choir_oplog::scheme::WEBAUTHN_ES256
            && self.reload_keys()
        {
            verified_actor = self.registry.verify_signing_hash(&signing, sig);
        }
        self.subject = (None, None);
        let actor_id = verified_actor.map_err(|e| {
            // Two failures with opposite repairs, and one of them is an
            // attack signal, so they cannot share a code. A key id the
            // node has no record of is a trust gap the operator closes.
            // A signature that does not verify under a key the node
            // already trusts is either corruption or a lifted signature
            // replayed onto other bytes -- and answering that with "ask
            // the operator to register your public key" hands the party
            // being impersonated the one repair that helps the attacker.
            // Anything else is reported as the bad signature too: of the
            // two directions to be wrong in, refusing is the safe one.
            let detail = format!("signature check failed: {e:?}");
            match &e {
                choir_identity::IdentityError::UnknownKey(_) => Rejection::new(
                    Code::UnknownKey,
                    detail,
                    "ask the operator to add your public key to the node's trusted-keys file                  (`choir key <file> <you>` prints the line); it takes effect on the next request",
                ),
                _ => Rejection::new(
                    Code::BadSignature,
                    detail,
                    "re-sign the exact bytes you are submitting; a signature covers one \
                     (channel, payload) pair and does not carry to another. Registering a key \
                     does not help here, the key this names is already trusted -- if you did not \
                     send this, a signature of yours was replayed onto bytes you never signed",
                ),
            }
            .encode()
        })?;
        // Derived here, while the signature is already verified, and
        // only when something will read it: naming the op means
        // serializing its kind, which is not free per op.
        let journalling = self.journal.enabled();
        if journalling {
            self.subject.0 = Some(actor_id.to_hex());
        }
        let op = ViewOp::from_payload(&sub.payload).map_err(|e| {
            Rejection::new(
                Code::MalformedOp,
                format!("payload did not decode as a ViewOp: {e:?}"),
                "sign the bytes of a serialized ViewOp; `choir submit` does this correctly",
            )
            .encode()
        })?;
        if journalling {
            self.subject.1 = op_type_name(&op);
        }
        // Before any policy that asks what the op *does*: has this
        // signature already been spent, and was it ever meant for this
        // log at all.
        self.admit_once(&signing, &op)?;
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
        // A comment's claimed author is bound the same way and for a
        // sharper reason: a verdict in the wrong name is a wrong
        // authorization, a comment in the wrong name is words somebody
        // never said. The view stores `author`, so the payload claim is
        // what a reader sees, and it must be the channel that signed.
        if let OpKind::PostComment { author, .. } = &op.kind {
            if *author != sub.channel {
                return Err(Rejection::new(
                    Code::ReviewerMismatch,
                    "a comment's author must be the channel it was signed on",
                    "resubmit on your own channel: `choir comment` signs on the author name by \
                     construction",
                )
                .with_states(Some(sub.channel.clone()), Some(author.clone()))
                .encode());
            }
        }
        // A receipt's claimed viewer is bound the same way: the receipt
        // exists to attribute attention, and attention recorded in
        // somebody else's name is exactly the false "it was looked at"
        // signal the op exists to remove.
        if let OpKind::ViewedReview { viewer, .. } = &op.kind {
            if *viewer != sub.channel {
                return Err(Rejection::new(
                    Code::ReviewerMismatch,
                    "a receipt's viewer must be the channel it was signed on",
                    "resubmit on your own channel: `choir viewed` signs on the viewer name by \
                     construction",
                )
                .with_states(Some(sub.channel.clone()), Some(viewer.clone()))
                .encode());
            }
        }
        // A vouch's claimed voucher is bound the same way, one level up:
        // the payload names an *operator* and the submission is signed on
        // a *channel*, so the comparison is against the channel's
        // operator prefix rather than the channel itself. `ops/agent` and
        // `ops` are the same operator and either may sign; `rival` may
        // not, and an unchecked `voucher` is precisely a Sybil writing
        // somebody else's endorsements (D65).
        if let OpKind::CountersignSnapshot { witness, .. } = &op.kind {
            let operator = reviewer_operator(&sub.channel);
            if witness != operator {
                return Err(Rejection::new(
                    Code::ReviewerMismatch,
                    "a countersignature's witness must be the operator of the channel it was \
                     signed on",
                    "resubmit on a channel belonging to that operator: `choir witness` derives \
                     the witness from the channel by construction",
                )
                .with_states(Some(operator.to_string()), Some(witness.clone()))
                .encode());
            }
        }
        if let OpKind::Vouch { voucher, .. } | OpKind::WithdrawVouch { voucher, .. } = &op.kind {
            let operator = reviewer_operator(&sub.channel);
            if voucher != operator {
                return Err(Rejection::new(
                    Code::ReviewerMismatch,
                    "a vouch's voucher must be the operator of the channel it was signed on",
                    "resubmit on a channel belonging to that operator: `choir vouch` derives \
                     the voucher from the channel by construction",
                )
                .with_states(Some(operator.to_string()), Some(voucher.clone()))
                .encode());
            }
        }
        // ...and the channel itself must belong to the signing key, or
        // the check above only proves a claim is self-consistent, not
        // that it is true. Review ops only: see `channel_is_owned`.
        if matches!(
            op.kind,
            OpKind::PostVerdict { .. }
                | OpKind::RequestReview { .. }
                | OpKind::PostComment { .. }
                | OpKind::ViewedReview { .. }
                | OpKind::Vouch { .. }
                | OpKind::WithdrawVouch { .. }
                | OpKind::CountersignSnapshot { .. }
        ) {
            self.channel_is_owned(&actor_id, &sub.channel)?;
        }
        // D41: a provenance label claims "the node signed this on behalf
        // of a git pusher". Unenforced, the label would be exactly the
        // laundering it exists to prevent — any author could dress an op
        // as push-derived, or downstream code could trust the label
        // without checking the signer. Enforced here, an accepted labeled
        // op always carries the node's own signature.
        if op.provenance.is_some() && actor_id != self.node_id {
            return Err(Rejection::new(
                Code::NodeOnly,
                "only the node may label an op with a push provenance",
                "submit without `provenance`: author-signed ops are the default class and need no label",
            )
            .encode());
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
        if let OpKind::CreateChange {
            id,
            owner,
            workspace,
            base_revision,
            idempotency_key,
            owner_sig: Some(owner_sig),
            cone,
        } = &op.kind
        {
            // The cone is rederived into the authorization rather than
            // trusted from the op, which is what makes it a *declaration
            // by the owner*: a node that attached a scope its owner did
            // not sign produces different bytes here and fails the check
            // below (D50).
            let authorization = CreateAuthorization::new(
                id.clone(),
                owner.clone(),
                workspace.clone(),
                base_revision.clone(),
                idempotency_key.clone(),
            )
            .with_cone(cone.clone())
            .to_payload();
            let mut verified_owner =
                self.registry
                    .verify_submission(owner, &authorization, owner_sig);
            if verified_owner.is_err() && self.reload_keys() {
                verified_owner = self
                    .registry
                    .verify_submission(owner, &authorization, owner_sig);
            }
            let owner_actor = verified_owner.map_err(|error| {
                Rejection::new(
                    Code::UnknownKey,
                    format!("create owner signature check failed: {error:?}"),
                    "register the owner key, then retry the same signed create authorization",
                )
                .encode()
            })?;
            self.channel_is_owned(&owner_actor, owner)?;
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
            let authorization =
                ArchiveAuthorization::new(id.clone(), workspace.clone(), prev_revision.clone())
                    .to_payload();
            let mut verified_owner =
                self.registry
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
            OpKind::SetWorkspaceHead { workspace, .. } | OpKind::DeleteWorkspace { workspace } => {
                self.view
                    .lock()
                    .expect("view lock")
                    .changes
                    .values()
                    .any(|change| change.active_workspace.as_deref() == Some(workspace))
            }
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
        // A slash can withdraw authorization from a review whose detail
        // has already been compacted. Only the node key may make that
        // durable attestation; otherwise any trusted author could erase
        // another operator's approval weight.
        if matches!(op.kind, OpKind::SlashApproval { .. }) && actor_id != self.node_id {
            return Err(Rejection::new(
                Code::NodeOnly,
                "only the node may slash approvals",
                "ask the operator to run `choir slash` with the node key",
            )
            .encode());
        }
        // A ref snapshot is the node's own attestation of its complete
        // ref-state (D25): the unit a witness will cosign and the thing
        // two readers compare to detect equivocation. The fold already
        // refuses an untruthful one; this guard is about authorship —
        // signed by anyone else it attests nothing about the node while
        // reading as though it did.
        if matches!(op.kind, OpKind::RecordRefSnapshot { .. }) && actor_id != self.node_id {
            return Err(Rejection::new(
                Code::NodeOnly,
                "only the node may record ref snapshots",
                "read the latest snapshot from the view; the node attests its own ref-state",
            )
            .encode());
        }
        if matches!(op.kind, OpKind::CountersignSnapshot { .. }) && actor_id == self.node_id {
            return Err(Rejection::new(
                Code::NodeOnly,
                "the node cannot witness its own ref-state attestation",
                "a witness is worth counting only because it is not the node that made the \
                 claim; have an independent operator cosign it",
            )
            .encode());
        }
        // Key bindings are the durable operator record that T3 attribution
        // and T1's ordering primitive read, so a binding any trusted key
        // could author is evidence forgeable by the actors it is meant to
        // weigh -- worse than no record, because it reads as sequenced
        // proof. `check` is a series of per-variant guards with a
        // fall-through to `validate`, i.e. admit-by-default, so these
        // variants have to name themselves here to be refused.
        //
        // This also supplies the second condition on re-binding: the fold
        // lets a binding correct its channel (keeping `bound_at` pinned),
        // and that correction is only safe while the node is the one
        // making it.
        if matches!(op.kind, OpKind::BindKey { .. } | OpKind::RevokeKey { .. })
            && actor_id != self.node_id
        {
            return Err(Rejection::new(
                Code::NodeOnly,
                "only the node may bind or revoke operator keys",
                "ask the operator to record this binding with the node key",
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
        let gating = self
            .require_review
            .load(std::sync::atomic::Ordering::Relaxed);
        if gating {
            match &op.kind {
                OpKind::SetRef { name, commit, prev } if self.ref_is_protected(name)? => {
                    // Creating a protected ref is allowed: there is no
                    // history to hijack yet, and deletion is refused
                    // below, so "delete then re-create" is not a way in.
                    if prev.is_some() {
                        self.authorization_for(name, commit, None, sub, &op, &actor_id)?;
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
        // Deliberately outside the `gating` block above, and deliberately
        // not guarded on `prev`. A `Submit` exists to carry an
        // authorization record; letting one through unexamined -- because
        // the operator turned the gate off, or because the ref is not
        // protected, or because it creates the ref rather than moving it
        // -- would mint a signed claim that a rule admitted a landing
        // when no rule looked at it. The claim is the asset here, so it
        // is the thing that must never be issued unbacked.
        if let OpKind::Submit {
            review,
            name,
            commit,
            authorization,
            ..
        } = &op.kind
        {
            self.submit_is_authorized(
                gating,
                review,
                name,
                commit,
                authorization,
                sub,
                &op,
                &actor_id,
            )?;
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
            .map_err(|e| {
                // A lost CAS is recorded as contention in its own right,
                // carrying both sides. A rejection count alone cannot
                // separate "two writers raced this ref" from "a client
                // sent nonsense", and only the first is a fact about
                // load.
                if let choir_view::ViewError::StaleHead {
                    expected, actual, ..
                } = &e
                {
                    self.journal.record(journal::Event::CasFailure {
                        workspace: sub.channel.clone(),
                        expected: expected.as_ref().map(ContentHash::to_hex),
                        actual: actual.as_ref().map(ContentHash::to_hex),
                    });
                }
                crate::reject::from_view_error(&e).encode()
            })
    }

    fn subject(&self) -> (Option<String>, Option<String>) {
        self.subject.clone()
    }

    /// A replicated entry is folded, never admitted: no signature, grant,
    /// scope or quota is asked about, because the home already decided.
    /// What is asked is whether this build's view can take it at all,
    /// which is exactly the question `accepted` would otherwise answer
    /// with a panic.
    fn replicable(&mut self, entry: &OpEntry) -> Result<(), String> {
        let op = ViewOp::from_payload(&entry.payload)
            .map_err(|e| format!("payload does not decode as an op this build knows: {e:?}"))?;
        self.view
            .lock()
            .expect("view lock")
            .validate(&op)
            .map_err(|e| format!("the view cannot fold it: {e:?}"))
    }

    fn accepted(&mut self, entry: &OpEntry, hash: &ContentHash) {
        let op = ViewOp::from_payload(&entry.payload).expect("checked in check()");
        {
            let mut view = self.view.lock().expect("view lock");
            view.apply(&op).expect("checked in check()");
            self.concentration
                .lock()
                .expect("concentration lock")
                .observe(entry, &op, &view);
            // D37, inside the same view-lock scope as the fold above:
            // a reader that took the tally between the two would see a
            // workspace the view already has and the tally does not.
            self.workspace_tally
                .lock()
                .expect("workspace tally lock")
                .observe(entry, &op);
        }
        if let Some(retention) = &self.review_retention {
            let mut retention = retention.lock().expect("review retention lock");
            let observed_at = retention.config.lapse_after.map(|_| Instant::now());
            retention.observe(&op, observed_at);
        }
        self.entries
            .lock()
            .expect("entries lock")
            .push(entry.clone(), hash.clone());
        self.offer_hook(entry, hash, &op);
    }
}

/// A ref value as a receiver wants to read it: the git oid when the
/// value is one, and the self-describing content hash otherwise (the
/// platform API can point a ref at something that is not a git object).
fn oid_text(hash: &ContentHash) -> String {
    hash.git_oid().unwrap_or_else(|| hash.to_hex())
}

impl ChoirPolicy {
    /// Hands a landed ref to the webhook delivery thread (D32).
    ///
    /// This runs on the writer thread, which is why it does nothing but
    /// build a small struct and `try_send` it. Matching, config reload,
    /// address vetting and `curl` all happen on the delivery thread; a
    /// full queue drops the event and counts it. Invariant 5 is the
    /// reason, and it is not a stylistic one: the target address belongs
    /// to somebody else, so a receiver that stops answering would
    /// otherwise stall op admission for everyone.
    fn offer_hook(&self, entry: &OpEntry, hash: &ContentHash, op: &ViewOp) {
        let guard = self.hooks.lock().expect("hooks lock");
        let Some(hooks) = guard.as_ref() else {
            return;
        };
        let (name, old, new) = match &op.kind {
            OpKind::SetRef { name, commit, prev } => {
                (name, prev.as_ref().map(oid_text), Some(oid_text(commit)))
            }
            OpKind::DeleteRef { name, prev } => (name, prev.as_ref().map(oid_text), None),
            _ => return,
        };
        hooks.offer(crate::hooks::RefEvent {
            key: name.clone(),
            old,
            new,
            seq: entry.seq,
            entry: hash.to_hex(),
            actor: entry.channel.clone(),
            key_id: entry.author_sig.as_ref().map(|sig| sig.key_id.clone()),
        });
    }
}

/// A running platform: the sequencer plus the shared view it maintains.
pub struct Platform {
    /// The slot `with_journal` fills; shared with the writer thread and
    /// the admission policy, which both hold clones.
    journal: SharedJournal,
    handle: SequencerHandle,
    view: Arc<Mutex<View>>,
    /// The daemon's own key: signs ops it derives from authenticated git
    /// pushes. Attribution: a verified push certificate names the
    /// pusher's key (`key/<principal>`); otherwise the basic-auth user
    /// (`git/<user>`).
    node_key: Arc<ActorKey>,
    entries: Arc<Mutex<LogWindow>>,
    /// Operator-curated file of eligible reviewer names, one per line.
    /// Read fresh on every draw, so editing it takes effect at once.
    /// `None` = no pool, and unassigned reviews stay unassigned.
    reviewer_pool: Option<std::path::PathBuf>,
    /// Optional operator conflict graph plus the maximum graph distance
    /// excluded from a review draw. The file is read fresh on every draw,
    /// like the reviewer pool. Edges are undirected operator pairs.
    reviewer_conflict_graph: Option<(std::path::PathBuf, usize)>,
    /// The persisted op log, for readers that have fallen behind the
    /// in-memory window. `None` (an in-memory log) means such a reader
    /// gets a loud gap error instead of a resync.
    log_path: Option<std::path::PathBuf>,
    /// Absolute operation-count ceiling for one signed batch.
    batch_limit: usize,
    /// Shared with the policy: when set, self-named reviewers are
    /// refused and every review goes through the node's draw.
    require_assignment: Arc<std::sync::atomic::AtomicBool>,
    /// Shared with the policy: the same refusal, but only for reviews
    /// that propose to land on a ref the operator marked protected.
    protected_refs: Arc<Mutex<Option<std::path::PathBuf>>>,
    /// Shared with the policy: when set, a protected ref only moves to a
    /// commit an approved review already named.
    require_review: Arc<std::sync::atomic::AtomicBool>,
    /// Shared with the policy: the operator's ACL file, consulted for
    /// [`crate::acl::Level::Own`] grants when a landing is gated (D42).
    acl_file: Arc<Mutex<Option<std::path::PathBuf>>>,
    /// Shared with the policy: the credential store passkey submissions
    /// are verified against (D39). Filled by [`Platform::attach_accounts`].
    passkeys: Arc<Mutex<Option<Arc<crate::accounts::Accounts>>>>,
    /// Shared with the policy: when set, only scoped ops are admitted.
    require_scope: Arc<std::sync::atomic::AtomicBool>,
    /// Shared with the policy: actor id → bound channel name.
    key_names: Arc<Mutex<KeyBindings>>,
    /// The trusted-keys table as last read, refreshed in the same step as
    /// [`Platform::key_names`], for `GET /api/signers`. The log records a
    /// key's actor id and never the key, so a reader verifying signatures
    /// has nowhere else to learn one from.
    signers: Mutex<Vec<crate::TrustedKey>>,
    /// D24 T3 runtime projection, replayed from the signed log at startup.
    concentration: Arc<Mutex<ConcentrationState>>,
    /// D37 per-user workspace tally, replayed from the same log in the
    /// same pass. Shared with the policy, which advances it.
    workspace_tally: Arc<Mutex<crate::quota::WorkspaceTally>>,
    /// Sequence-ordered live reviews, allocated only under an explicit
    /// retention configuration.
    review_retention: Option<Arc<Mutex<ReviewRetentionState>>>,
    /// Opt-in D24 T4 audit. Sparse and separate from the signed op log:
    /// rejected requests never enter that log, while this evidence must.
    newcomer_audit: Option<Arc<Mutex<NewcomerAudit>>>,
    /// Opt-in D24 T2 operator classifications, re-read on every report so a
    /// fresh judgement lands without a restart. Absent means uncounted, not
    /// unclassified: coverage simply stays zero.
    review_adjudications: Option<std::path::PathBuf>,
    /// Serializes concurrent maintenance passes. User submissions still
    /// race normally through the sequencer; only duplicate pruning scans
    /// and archive batches are coalesced.
    review_prune_lock: Mutex<()>,
    /// Shared with the policy: the webhook delivery handle (D32), or
    /// `None` when the operator passed no `--hooks-file`.
    hooks: Arc<Mutex<Option<crate::hooks::Hooks>>>,
    /// The writer's own latency record for the traffic this node is
    /// serving, so the Phase-0 decision-latency gate is checked in
    /// production and not only by the test suite.
    lag: Arc<LagMeter>,
    /// Where drained gate breaches are appended, one JSON object per
    /// line. `None` keeps them in memory only, where the ring eventually
    /// drops the oldest (reported, never silent).
    lag_log: Option<std::path::PathBuf>,
    /// Last failure to write the lag log and how many writes have failed,
    /// surfaced in the report. A breach record that could not be written
    /// is itself an operational fact; swallowing it would make the lag log
    /// a check that cannot fail. The count does not reset on a later
    /// success, so a transient failure is still visible afterwards.
    lag_log_error: Mutex<(Option<String>, u64)>,
    /// The home this platform's log is a copy of, when it runs as a seed
    /// (D80). Set once, before the platform serves anything, and never
    /// cleared: a node does not stop being a copy of somebody's log.
    ///
    /// Shared with the policy, which refuses every submission on a seed,
    /// so a write that reached the writer by any path, HTTP or the node's
    /// own, is answered the same way.
    home: Arc<std::sync::OnceLock<crate::replica::Home>>,
    /// Where replication stands, once a [`crate::replica::Replica`] is
    /// writing into this platform.
    replica: std::sync::OnceLock<Arc<crate::replica::Shared>>,
    // Kept alive for the daemon's lifetime; the writer thread exits with
    // the process.
    _sequencer: Sequencer,
}

/// Exact owner-authorized binding the node records after materializing a
/// physical workspace.
pub struct AuthorizedChangeCreate<'a> {
    /// Stable logical contribution id.
    pub id: &'a str,
    /// Bound signing channel.
    pub owner: &'a str,
    /// Namespaced physical workspace id.
    pub workspace: &'a str,
    /// Full Git object id selected as the immutable base.
    pub base_hex: &'a str,
    /// Owner-scoped retry identity.
    pub idempotency_key: &'a str,
    /// Owner proof over the matching [`CreateAuthorization`].
    pub owner_sig: Witness,
    /// Directory prefixes the owner declared this change works within,
    /// covered by `owner_sig`. Empty means the whole tree.
    pub cone: Vec<String>,
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
    /// (on mtime change) before signature verification: registering a key
    /// is appending a line, no restart, and removing one refuses its next
    /// submission. The file's contents replace the whole registry.
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
        let (view, review_retention, concentration, workspace_tally) =
            materialize_platform_state(log.as_ref(), retention)
                .map_err(|e| format!("replay: {e:?}"))?;
        let review_retention = review_retention.map(|state| Arc::new(Mutex::new(state)));
        let view = Arc::new(Mutex::new(view));
        let concentration = Arc::new(Mutex::new(concentration));
        let workspace_tally = Arc::new(Mutex::new(workspace_tally));
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
            by_hash: std::collections::HashMap::new(),
        };
        // Seeding needs each entry's hash, and the log stores it already:
        // every entry's `parent` is its predecessor's hash, and the last
        // one's is the log head. So a restart re-indexes the window
        // without hashing anything, and a scope signed just before the
        // restart is still admissible just after it.
        let mut pending: Option<OpEntry> = None;
        for i in start..len {
            let current = log.get(i);
            if let (Some(previous), Some(current)) = (pending.take(), current.as_ref()) {
                let hash = current
                    .parent
                    .clone()
                    .unwrap_or_else(|| previous.content_hash());
                window.push(previous, hash);
            }
            pending = current;
        }
        if let Some(last) = pending {
            let hash = log.head().unwrap_or_else(|| last.content_hash());
            window.push(last, hash);
        }
        let entries = Arc::new(Mutex::new(window));
        let keys_mtime = keys_file
            .as_ref()
            .and_then(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
        // Name bindings are read from the same file at startup; a
        // malformed file here is not fatal because `start_reloading`
        // already accepted the caller's registry.
        let parsed = keys_file
            .as_ref()
            .map(|path| crate::parse_keys_file(path).ok());
        let key_names = Arc::new(Mutex::new(match &parsed {
            Some(Some(signers)) => KeyBindings::from_signers(signers),
            Some(None) => KeyBindings::unavailable(true),
            None => KeyBindings::unavailable(false),
        }));
        let signers = Mutex::new(parsed.flatten().unwrap_or_default());
        let require_assignment = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let protected_refs = Arc::new(Mutex::new(None));
        let acl_file = Arc::new(Mutex::new(None));
        let require_review = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let require_scope = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hooks = Arc::new(Mutex::new(None));
        let passkeys = Arc::new(Mutex::new(None));
        let shared_journal = SharedJournal::default();
        let home = Arc::new(std::sync::OnceLock::new());
        let sequencer = Sequencer::spawn_with_journal(
            log,
            Box::new(ChoirPolicy {
                home: home.clone(),
                journal: shared_journal.clone(),
                subject: (None, None),
                require_assignment: require_assignment.clone(),
                protected_refs: protected_refs.clone(),
                acl_file: acl_file.clone(),
                require_review: require_review.clone(),
                require_scope: require_scope.clone(),
                registry,
                view: view.clone(),
                entries: entries.clone(),
                keys_file,
                keys_mtime,
                node_pub: node_key.public_key_bytes().to_vec(),
                node_id: node_key.actor_id(),
                key_names: key_names.clone(),
                concentration: concentration.clone(),
                review_retention: review_retention.clone(),
                hooks: hooks.clone(),
                workspace_tally: workspace_tally.clone(),
                passkeys: passkeys.clone(),
            }),
            Box::new(shared_journal.clone()),
        );
        let platform = Self {
            journal: shared_journal,
            handle: sequencer.handle(),
            lag: sequencer.lag(),
            lag_log: None,
            lag_log_error: Mutex::new((None, 0)),
            view,
            node_key: Arc::new(node_key),
            entries,
            reviewer_pool: None,
            reviewer_conflict_graph: None,
            log_path: None,
            batch_limit: DEFAULT_BATCH_OPS,
            require_assignment,
            protected_refs,
            acl_file,
            require_review,
            require_scope,
            passkeys,
            key_names,
            signers,
            concentration,
            workspace_tally,
            review_retention,
            newcomer_audit: None,
            review_adjudications: None,
            review_prune_lock: Mutex::new(()),
            hooks,
            home,
            replica: std::sync::OnceLock::new(),
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

    /// Enables sparse, durable D24 T4 newcomer measurement.
    ///
    /// `incumbent_actor_keys` is the trusted-key snapshot at activation;
    /// those actors are excluded because the audit cannot reconstruct their
    /// first attempt or time-to-first-acceptance. The audit records only a
    /// later actor's first verified signed-API attempt, its first eventual
    /// acceptance, and an optional appeal. Operator adjudications are JSONL
    /// rows in the separate file and are re-read for every report.
    ///
    /// Both files are created mode 0600. Neither changes the signed op log or
    /// any hash input.
    ///
    /// # Errors
    ///
    /// Unusable paths or an invalid existing audit file.
    pub fn with_newcomer_audit(
        mut self,
        audit_path: std::path::PathBuf,
        adjudications_path: std::path::PathBuf,
        incumbent_actor_keys: Vec<String>,
    ) -> Result<Self, String> {
        if let Some(parent) = adjudications_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create adjudications directory: {e}"))?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options
            .open(&adjudications_path)
            .map_err(|e| format!("open adjudications: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&adjudications_path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| format!("chmod adjudications: {e}"))?;
        }
        let audit = NewcomerAudit::open(
            &audit_path,
            adjudications_path,
            incumbent_actor_keys.into_iter().collect(),
        )?;
        self.newcomer_audit = Some(Arc::new(Mutex::new(audit)));
        Ok(self)
    }

    /// Enables the D24 T2 operator classification file: versioned 0600 JSONL
    /// rows of `{"format_version":1,"review_id":…,"classification":…}` where
    /// classification is `valid`, `invalid`, `slop` or `unclear`.
    ///
    /// Separate from the signed op log on purpose, exactly like the T4
    /// adjudications: a judgement about a contribution is the operator's
    /// opinion, not a sequenced claim any actor can make. Keying it by review
    /// id rather than by verdict is what lets a classification outlive
    /// archiving, which discards the verdict bulk.
    ///
    /// Enabling it does not make T2 evaluable. The cohort still has no exit
    /// rule, the observation window and tripwire subject are unset, and census
    /// adjudication cannot survive the flood it would need to detect.
    ///
    /// # Errors
    ///
    /// The file or its directory cannot be created.
    pub fn with_review_adjudications(mut self, path: std::path::PathBuf) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create review adjudications directory: {e}"))?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options
            .open(&path)
            .map_err(|e| format!("open review adjudications: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| format!("chmod review adjudications: {e}"))?;
        }
        self.review_adjudications = Some(path);
        Ok(self)
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

    /// Points the admission policy at the operator's ACL file, so a
    /// landing on a protected ref can ask who owns the repository (D42).
    ///
    /// Without this the ownership rule is simply absent and every
    /// protected ref keeps the approval-weight gate, which is the
    /// behaviour of every node built before D42. Ownership is opt-in per
    /// repository even once the file is attached: a repository nobody
    /// holds `own` over is unaffected.
    ///
    /// The path, not a parsed table, because the file is hot-reloadable
    /// and admission must see an ownership change without a restart —
    /// the same reason [`Platform::with_protected_refs`] takes a path.
    #[must_use]
    pub fn with_acl_file(self, path: std::path::PathBuf) -> Self {
        *self.acl_file.lock().expect("acl file lock") = Some(path);
        self
    }

    /// Gives the admission policy the credential store, so a submission
    /// signed by an enrolled passkey can be verified (D39).
    ///
    /// A setter rather than a constructor argument because accounts and
    /// the platform are enabled independently, in either order, by
    /// separate flags. Until this is called a passkey submission is
    /// refused for want of a store, which is the same answer a node
    /// without self-service gives permanently.
    pub fn attach_accounts(&self, store: Arc<crate::accounts::Accounts>) {
        *self.passkeys.lock().expect("passkey store lock") = Some(store);
    }

    /// Writes the credential's public key into a passkey submission, so
    /// the entry it becomes can be checked by someone holding nothing
    /// but the log (D45).
    ///
    /// The node supplies this rather than the client because the client
    /// *cannot*: `getPublicKey()` exists on a WebAuthn registration
    /// response only, so a browser holding an assertion has the
    /// credential id and no key. The value written is the one the
    /// admission check is about to verify against, read from the same
    /// store by the same `(channel, credential id)` pair.
    ///
    /// **The request cannot influence this field.** `decode_submission`
    /// never reads a `credential_key` from the body, so there is no
    /// mismatch to refuse and no path by which a caller asserts key
    /// material for a credential it does not hold. "Refuse a bad value"
    /// and "make a bad value unrepresentable" answer the same question;
    /// the second needs no test to stay true.
    ///
    /// **Absent signature, absent store and unknown credential are all
    /// left alone.** Each is a reason [`ChoirPolicy::verify_passkey`]
    /// refuses this submission a moment later, with a code and a repair.
    /// Refusing here as well would be a second refusal for one cause,
    /// raised from the shape of the request instead of by the check that
    /// owns the question.
    fn stamp_credential_key(&self, sub: &mut DecodedSubmission) {
        let Some(sig) = sub.author_sig.as_mut() else {
            return;
        };
        if sig.scheme_id() != choir_oplog::scheme::WEBAUTHN_ES256 {
            return;
        }
        let store = self.passkeys.lock().expect("passkey store lock").clone();
        if let Some(spki) = store.and_then(|s| s.passkey_spki(&sub.channel, &sig.key_id)) {
            sig.credential_key = Some(spki);
        }
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

    /// Admits only ops whose author bound them to this log and a head
    /// still in the window — the replay defence, turned on.
    ///
    /// Off by default because it is a wire-compatibility break, not
    /// because unscoped is safe: an unscoped signature is admissible on
    /// any node that trusts the key, and admissible again on the node it
    /// came from as soon as CAS state returns to what it expected. Every
    /// client in this repository always sends a scope, so turning this
    /// on costs them nothing; a client that predates scopes stops
    /// working, which is the whole reason for the flag.
    #[must_use]
    pub fn with_required_scope(self) -> Self {
        self.require_scope
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self
    }

    /// The log identity a client signs a scope against: this node's
    /// actor id and the head it should name.
    #[must_use]
    pub fn scope_now(&self) -> (ContentHash, Option<ContentHash>) {
        let head = self.entries.lock().expect("entries lock").head_hash();
        // On a seed the log is the home's, entry for entry, so the node a
        // scope must name is the home and the head is a head of the
        // home's log. A client that signs against a seed's view therefore
        // signs something the home will admit, which is what lets a
        // refused write be resent there unchanged.
        match self.home.get() {
            Some(home) => (home.node_id.clone(), head),
            None => (self.node_key.actor_id(), head),
        }
    }

    /// Makes this platform a seed of `home` (D80): its log becomes a copy
    /// of the home's, taken through [`Platform::replicate`] and nothing
    /// else.
    ///
    /// Call before the platform serves anything.
    #[must_use]
    pub fn as_seed_of(self, home: crate::replica::Home) -> Self {
        // A second call would change whose copy this is mid-life; the
        // first one stands, and the daemon never makes a second.
        let _ = self.home.set(home);
        self
    }

    /// The home this platform is a seed of, or `None` on a home.
    #[must_use]
    pub fn seed_home(&self) -> Option<&crate::replica::Home> {
        self.home.get()
    }

    /// Records the replicator writing into this platform, and the home it
    /// copies. The first call stands.
    pub(crate) fn attach_replica(
        &self,
        home: crate::replica::Home,
        shared: Arc<crate::replica::Shared>,
    ) {
        let _ = self.home.set(home);
        let _ = self.replica.set(shared);
    }

    /// What a cached page about this node must be re-rendered for beyond
    /// the view's position: on a seed, how far the home has run ahead and
    /// whether replication has stopped, neither of which moves the view.
    pub(crate) fn replica_marker(&self) -> Option<String> {
        let status = self
            .replica
            .get()?
            .status
            .lock()
            .expect("replica status lock");
        Some(format!(
            "{:?}:{}:{}",
            status.home_head_seq,
            status.halted.is_some(),
            status.gap
        ))
    }

    /// Appends entries the home already sequenced, through this platform's
    /// own writer thread, and folds them into its view. See
    /// [`SequencerHandle::replicate`] for the checks each one passes.
    ///
    /// # Errors
    ///
    /// The first entry that could not be taken; everything before it was.
    pub fn replicate(
        &self,
        entries: Vec<choir_sequencer::Sequenced>,
    ) -> Result<choir_sequencer::Replicated, choir_sequencer::ReplicateError> {
        self.handle.replicate(entries)
    }

    /// Runs `read` against the view as it stands, under its lock.
    pub(crate) fn with_view<R>(&self, read: impl FnOnce(&View) -> R) -> R {
        read(&self.view.lock().expect("view lock"))
    }

    /// The round of proposals aimed at `branch` of `repo` (D68).
    ///
    /// `None` when the branch does not exist. A round with no proposals
    /// is `Some` with an empty list: "nobody has proposed anything" and
    /// "there is nothing to propose to" are different answers, and only
    /// the second is a misconfiguration.
    #[must_use]
    pub fn proposal_round(&self, repo: &str, branch: &str) -> Option<crate::queue::ProposalRound> {
        let view = self.view.lock().expect("view lock");
        crate::queue::ProposalRound::from_view(&view, repo, branch)
    }

    /// A landing that moves `refname` from `base`, signed by this node.
    ///
    /// The scope is read per landing rather than captured here, because
    /// a round appends as it goes; see [`crate::queue::ScopeSource`].
    #[must_use]
    pub fn ref_landing(&self, refname: String, base: &str) -> crate::queue::RefLanding {
        let entries = Arc::clone(&self.entries);
        let node = self.node_key.actor_id();
        crate::queue::RefLanding::new(
            Arc::clone(&self.node_key),
            refname,
            base,
            Box::new(move || {
                (
                    node.clone(),
                    entries.lock().expect("entries lock").head_hash(),
                )
            }),
        )
    }

    /// A check reporter that this node signs and scopes (D49, D68).
    ///
    /// The reason `set_check_reporter` was off by default and composed
    /// into nothing: the identity a check is reported under belongs to
    /// whoever runs the queue, and a bridge has no node-side identity to
    /// use. A node does -- its own key -- and its landings are in a log
    /// that is kept rather than thrown away, so a report against them is
    /// answerable later.
    #[must_use]
    pub fn check_reporter(
        &self,
        name: String,
        target_ref: Option<String>,
    ) -> choir_queue::CheckReporter {
        let key = Arc::clone(&self.node_key);
        let entries = Arc::clone(&self.entries);
        let node = self.node_key.actor_id();
        choir_queue::CheckReporter {
            channel: crate::queue::QUEUE_CHANNEL.to_string(),
            name,
            target_ref,
            seal: Some(Arc::new(move |channel: &str, op: &ViewOp| {
                let head = entries.lock().expect("entries lock").head_hash();
                let payload = op.clone().in_scope(node.clone(), head).to_payload();
                let sig = key.sign_submission(channel, &payload);
                (payload, Some(sig))
            })),
        }
    }

    /// Runs one speculative round over this node's own log (D5, D68).
    ///
    /// `workdir` must be a **worktree of the repository this node
    /// serves**, made with `git worktree add --detach` against the bare
    /// repo, and owned outright by the queue: every speculative merge
    /// detaches it and forces it to a candidate state, so a tree
    /// anybody else is working in is the wrong argument.
    ///
    /// A worktree and not an independent clone, and this is not a
    /// preference. A landing names the merge commit the speculator
    /// built. An independent clone writes that commit into its own
    /// object store, where the served repository cannot see it, so the
    /// log would name a commit git does not have --- which
    /// [`Platform::reconcile_git_refs`] correctly reads as the view
    /// being wrong and compensates back to git's value, silently
    /// undoing every landing in the round. A worktree shares the object
    /// store, so the commit is already there when the op is submitted.
    ///
    /// `None` when the branch does not exist. Otherwise the report says
    /// what landed, in order, and every landing in it is an op in this
    /// node's log rather than a number the round kept to itself.
    pub fn run_proposal_queue(
        &self,
        repo: &str,
        branch: &str,
        workdir: &std::path::Path,
        ci: &mut dyn choir_queue::executor::CiExecutor,
        template: choir_queue::JobTemplate,
    ) -> Option<choir_queue::QueueReport> {
        let round = self.proposal_round(repo, branch)?;
        let mut queue = choir_queue::MergeQueue::with_speculator(
            &round.base,
            Box::new(choir_queue::git::GitSpeculator::new(workdir.to_path_buf())),
        );
        queue.set_job_template(template);
        queue.set_landing(Box::new(self.ref_landing(round.target_ref(), &round.base)));
        // Every verdict the round reaches is recorded (D49). The subject
        // is the speculative commit CI actually ran against, so a check
        // is answerable about a tree rather than about a round number.
        queue.set_check_reporter(
            self.check_reporter("ci/queue".to_string(), Some(round.target_ref())),
        );
        for change in round.changes() {
            queue.submit(change);
        }
        Some(self.drain_queue(&mut queue, ci))
    }

    /// Drains a caller-built queue through this node's sequencer.
    ///
    /// The escape hatch under [`Platform::run_proposal_queue`], for a
    /// caller whose round is not simply "every proposal on this
    /// branch". The sequencer is the point: a landing recorded through
    /// anything else is not in the log this node serves.
    pub fn drain_queue(
        &self,
        queue: &mut choir_queue::MergeQueue,
        ci: &mut dyn choir_queue::executor::CiExecutor,
    ) -> choir_queue::QueueReport {
        queue.drain(ci, &self._sequencer)
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

    /// Sets the absolute operation-count ceiling for one batch request.
    #[must_use]
    pub fn with_batch_limit(mut self, max_ops: usize) -> Self {
        self.batch_limit = max_ops.max(1);
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
        *self.key_names.lock().expect("key names lock") = KeyBindings::from_signers(signers);
        *self.signers.lock().expect("signers lock") = signers.to_vec();
    }

    /// `GET /api/signers`: this node's own public key and every key in
    /// its trusted-keys table, with the channel name a key is bound to
    /// when its line carries one.
    ///
    /// Public keys are public data. It sits behind the log's own grant
    /// anyway, because the reader it exists for is one that already
    /// holds that grant: somebody replaying `/api/log` who wants to check
    /// authorship, which SYNC.md's third check cannot do without a key.
    fn signers_json(&self) -> serde_json::Value {
        let signers: Vec<serde_json::Value> = self
            .signers
            .lock()
            .expect("signers lock")
            .iter()
            .map(|signer| {
                serde_json::json!({
                    "actor_id": signer.actor_id,
                    "public_key_hex": hex_encode(&signer.key),
                    "name": signer.name,
                })
            })
            .collect();
        serde_json::json!({
            "format_version": 1,
            "node": {
                "actor_id": self.node_key.actor_id().to_hex(),
                "public_key_hex": hex_encode(&self.node_key.public_key_bytes()),
            },
            "signers": signers,
        })
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

    /// Excludes reviewer operators whose shortest path from the requester
    /// in `path` is at most `max_distance`. Each non-comment line is one
    /// undirected `<operator> <operator>` edge. Operator names, not full
    /// `operator/agent` channels, belong in the graph.
    ///
    /// The graph is operator-supplied runtime policy rather than op-log
    /// state: changing it affects future draws without changing persisted
    /// operations or replay. It is read for every draw and fails closed;
    /// an unreadable or malformed graph leaves the review unassigned.
    /// Distance zero retains the existing same-operator exclusion.
    #[must_use]
    pub fn with_reviewer_conflict_graph(
        mut self,
        path: std::path::PathBuf,
        max_distance: usize,
    ) -> Self {
        self.reviewer_conflict_graph = Some((path, max_distance));
        self
    }
}

/// The channel a push-derived op is attributed to, and how it was
/// established.
///
/// Extracted so the ref op and the review a magic push opens beside it
/// cannot disagree about who pushed. They are two ops about one act, and
/// a review drawn against a different channel than the ref it belongs to
/// would exclude the wrong operator from its own reviewer draw.
fn push_attribution(user: &str, cert: Option<(&str, &str)>) -> (String, Provenance) {
    // Verified push certificate ("G" = good signature) attributes the op
    // to the pusher's own key; otherwise the transport user. Either way
    // the node signs, and the payload says so (D41): the channel prefix
    // alone carried this class only by convention.
    match cert {
        Some(("G", signer)) if !signer.is_empty() => {
            (format!("key/{signer}"), Provenance::PushCertified)
        }
        _ => (crate::quota::channel_for(user), Provenance::PushTransport),
    }
}

/// A proposal pushed to the magic refspec, as parsed from its refname
/// (D53).
///
/// Gerrit's `refs/for/<branch>` and AGit's after it are the spelling
/// every reviewer of a git-hosted project already knows, and the point
/// of borrowing it is that the client is `git` and nothing else: no
/// binary to install, no key to mint, one push.
///
/// Two departures from Gerrit, both deliberate:
///
/// 1. **The ref is really created.** Gerrit intercepts `refs/for/*` in a
///    server it owns end to end and, in its own documentation's words,
///    lies to the client about the result. Doing that over git's wire
///    protocol needs a `proc-receive` hook, which is a second hook type
///    and a second protocol; and a node that reported a ref it did not
///    write would be a node whose push receipts cannot be trusted. Here
///    the ref exists, holds the objects, and is sequenced like any other.
/// 2. **A topic is required.** `refs/for/main` alone is one ref shared by
///    everyone proposing onto `main`, so the second contributor's push
///    would be a non-fast-forward against the first one's proposal
///    rather than a proposal of their own. The topic is what makes the
///    ref theirs.
#[derive(Debug)]
struct MagicRef {
    /// Branch the proposal asks to land on, as a short name.
    onto: String,
    /// Review id, derived so that re-pushing the same topic reaches the
    /// same review rather than opening a second one.
    review_id: String,
}

impl MagicRef {
    /// Parses `refs/for/<branch>/<topic>`, or `None` for any other ref.
    ///
    /// `Err` is reserved for a ref that *is* under `refs/for/` and
    /// cannot be used, because that is a pusher who meant to propose and
    /// needs telling why it did not work -- the one case where silence
    /// would look like success.
    fn parse(refname: &str) -> Option<Result<Self, String>> {
        let rest = refname.strip_prefix("refs/for/")?;
        let mut segments = rest.split('/').filter(|s| !s.is_empty());
        let (Some(onto), Some(first_topic)) = (segments.next(), segments.next()) else {
            return Some(Err(format!(
                "push to refs/for/<branch>/<topic>, not {refname}: a topic is what makes this \
                 proposal yours rather than one ref shared by everyone proposing onto that branch"
            )));
        };
        let topic: Vec<&str> = std::iter::once(first_topic).chain(segments).collect();
        let topic = topic.join("-");
        if !crate::provision::safe_segment(onto) {
            return Some(Err(format!(
                "`{onto}` is not a branch name this node can land on"
            )));
        }
        // The id reaches a URL (`/r/<repo>/review/<id>`), which admits
        // one path segment. The branch is folded in so that the same
        // topic proposed onto two branches is two reviews.
        let review_id = format!("for-{onto}-{topic}");
        if !crate::provision::safe_segment(&review_id) {
            return Some(Err(format!(
                "`{topic}` holds characters a review id cannot: use letters, digits, `-`, `_` \
                 or `.`"
            )));
        }
        Some(Ok(Self {
            onto: onto.to_string(),
            review_id,
        }))
    }
}

impl Platform {
    /// Opens a review for a push to the magic refspec, if the ref was one.
    ///
    /// Runs after the ref op is already durable, and returns `Ok` when
    /// the ref was not a magic one -- so an ordinary push pays nothing
    /// and cannot fail here.
    ///
    /// A second push to the same topic does **not** re-request: a review
    /// is one long-lived object per proposal, re-posting a verdict is the
    /// re-review flow, and asking again would be refused. What advances
    /// is the ref, which is where a reviewer reads the current commit
    /// from.
    fn open_magic_review(
        &self,
        repo: &str,
        magic: &MagicRef,
        new_hex: &str,
        user: &str,
        cert: Option<(&str, &str)>,
    ) -> Result<(), String> {
        if self
            .view
            .lock()
            .expect("view lock")
            .reviews
            .contains_key(&magic.review_id)
        {
            return Ok(());
        }
        let target = ContentHash::from_git_oid(new_hex).ok_or("bad new oid")?;
        self.submit_ref_op(
            OpKind::RequestReview {
                id: magic.review_id.clone(),
                target,
                // Empty on purpose: the node draws them, and a pusher
                // cannot name their own reviewers here any more than
                // they can through the CLI.
                reviewers: Vec::new(),
                target_ref: Some(format!("{repo}:refs/heads/{}", magic.onto)),
            },
            user,
            cert,
        )?;
        // The draw is a second, separate submission -- the same shape
        // `/api/submit` uses, and for the same reason: it names
        // reviewers, so it cannot be folded into the request that has
        // not been admitted yet.
        //
        // A failed draw does not fail the push. The request stands and
        // is visibly unassigned, which is a state no verdict can
        // complete; refusing the push instead would throw away objects
        // the node has already accepted over an empty reviewer pool.
        let (channel, _) = push_attribution(user, cert);
        if let Err(reason) = self.assign_reviewers(&magic.review_id, &channel) {
            eprintln!(
                "choir: review {} opened unassigned: {reason}",
                magic.review_id
            );
        }
        Ok(())
    }
}

/// Refuses a ref update that the pusher's grant does not reach (D60).
///
/// The other half of [`crate::acl::Level::Propose`], and the half that
/// has a refname to look at. [`crate::acl::git_requirement`] admits the
/// push at `propose` because git sends the ref list only after the
/// server has agreed to receive the pack, so there is nothing to check
/// at that boundary. The refname first exists here, when the
/// `pre-receive` hook reports it.
///
/// Checked against the **merged** table rather than
/// [`Platform::acl_now`]. That reader exists so `own` cannot be
/// self-issued (D42); `write` carries no such rule, and a grant issued
/// by self-service (D36) is as real as one the operator typed. Reading
/// the file alone here would refuse a legitimate pusher whose grant came
/// from an invite.
///
/// Refuses before anything is submitted, which is the rule
/// [`Platform::git_update`] already follows for a proposal ref it cannot
/// parse: git applies no ref until the hook exits zero, so a refusal at
/// this point leaves no op in the log and never enters the compensating
/// retraction pass that `Node::create_repo` documents.
///
/// `None` whenever the question does not arise: a body this does not
/// understand, or a pusher who holds `write` and is therefore not
/// limited to proposals.
pub(crate) fn proposal_denial(
    acl: &crate::acl::Effective,
    body: &[u8],
) -> Option<crate::acl::Denial> {
    let json: serde_json::Value = serde_json::from_slice(body).ok()?;
    let field = |key: &str| {
        json.get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
    };
    let (repo, refname, user) = (field("repo"), field("refname"), field("user"));
    if repo.is_empty() || refname.is_empty() {
        return None;
    }
    if acl.allows_repo(user, repo, crate::acl::Level::Write) {
        return None;
    }
    let refuse = |reason: String| {
        Some(crate::acl::Denial {
            status: 403,
            reason,
        })
    };
    let Some(rest) = refname.strip_prefix("refs/for/") else {
        return refuse(format!(
            "`{user}` may propose to {repo} but not write {refname}; \
             push to refs/for/<branch>/{user}/<topic> to open a review instead"
        ));
    };
    // Under their own name, so two proposers cannot reach one ref. A
    // `propose` grant is the level given to somebody the repository does
    // not trust, and several of them hold it at once: without this,
    // whoever pushes second silently takes over the first one's proposal,
    // or deletes it.
    //
    // Read off the raw segments rather than `MagicRef`'s topic, because
    // that topic is joined with dashes and therefore lossy -- `a/b` and
    // `a-b` reach the same review id, and a rule about who owns a ref
    // must not be decided by a form that has already merged two names.
    //
    // A ref under `refs/for/` that this refuses is answered here rather
    // than by `git_update`'s own "a topic is what makes this proposal
    // yours": for this pusher, naming themselves is the missing part, and
    // the message that says so is the more useful of the two. A `write`
    // holder never reaches here and still gets the other one.
    let mut segments = rest.split('/').filter(|s| !s.is_empty());
    let (branch, owner) = (segments.next(), segments.next());
    if owner != Some(user) {
        let branch = branch.unwrap_or("<branch>");
        return refuse(format!(
            "`{user}` may propose to {repo} only under their own name; \
             push to refs/for/{branch}/{user}/<topic>"
        ));
    }
    None
}

impl Platform {
    /// Routes one git ref update (from a repo's `update` hook) through
    /// the sequencer: CAS against the view, node-signed, totally ordered
    /// with API ops. Refs are namespaced `<repo>:<refname>`; git oids
    /// enter the envelope with their own codec ([`ContentHash::from_git_oid`]).
    ///
    /// A push to `refs/for/<branch>/<topic>` additionally opens a review
    /// targeting that branch, which is the whole magic-refspec path: one
    /// `git push`, no client but git, and reviewers drawn by the node.
    ///
    /// # Errors
    ///
    /// The policy's rejection reason (stale CAS = concurrent update git
    /// itself would also have refused), or the reason a `refs/for/` push
    /// could not be read as a proposal.
    pub fn git_update(
        &self,
        repo: &str,
        refname: &str,
        old_hex: &str,
        new_hex: &str,
        user: &str,
        cert: Option<(&str, &str)>,
    ) -> Result<(), String> {
        // Read before anything is submitted: a `refs/for/` push that
        // cannot be read as a proposal must be refused whole, not left
        // as a created ref with no review beside it.
        let magic = match MagicRef::parse(refname) {
            Some(Ok(magic)) => Some(magic),
            Some(Err(reason)) => return Err(reason),
            None => None,
        };
        let name = format!("{repo}:{refname}");
        let prev = if is_zero_oid(old_hex) {
            None
        } else {
            Some(ContentHash::from_git_oid(old_hex).ok_or("bad old oid")?)
        };
        let deleting = is_zero_oid(new_hex);
        let kind = if deleting {
            OpKind::DeleteRef { name, prev }
        } else {
            OpKind::SetRef {
                name,
                commit: ContentHash::from_git_oid(new_hex).ok_or("bad new oid")?,
                prev,
            }
        };
        self.submit_ref_op(kind, user, cert)?;
        // Only on a ref that now points somewhere. Deleting a proposal
        // ref withdraws the objects; it does not open a review on the
        // zero oid, and it deliberately does not close the review
        // either -- a review is append-only history, and abandoning one
        // is its own signed act.
        match magic {
            Some(magic) if !deleting => self.open_magic_review(repo, &magic, new_hex, user, cert),
            _ => Ok(()),
        }
    }

    /// Retracts a ref op this push already had accepted, because the push
    /// as a whole is being refused and git will apply none of it.
    ///
    /// `pre-receive` submits one op per ref but git applies no ref until
    /// the hook exits zero, so a push whose third ref is refused has
    /// already put two ops in the durable log. Without this the view keeps
    /// refs git never created — and they cannot be pushed afterwards
    /// either, because the pusher's `old` is git's (absent) value while
    /// the view holds the stranded one, so every retry loses the CAS. The
    /// ref becomes permanently unpushable.
    ///
    /// The log is append-only, so the repair is a compensating op, not an
    /// erasure: the abort is part of the history rather than hidden from
    /// it. `old`/`new` are the same values the accepted op carried, so the
    /// inverse restores exactly what git still has.
    ///
    /// # Errors
    ///
    /// The policy's rejection reason — most likely a lost CAS, meaning
    /// something else moved the ref between the accept and this retraction
    /// and the stranded value is no longer what would be undone.
    pub fn git_abort(
        &self,
        repo: &str,
        refname: &str,
        old_hex: &str,
        new_hex: &str,
        user: &str,
        cert: Option<(&str, &str)>,
    ) -> Result<(), String> {
        let name = format!("{repo}:{refname}");
        // CAS on what the accepted op set, so a retraction that races a
        // real update loses instead of clobbering it.
        let prev = Some(ContentHash::from_git_oid(new_hex).ok_or("bad new oid")?);
        let kind = if is_zero_oid(old_hex) {
            // The push was creating the ref, so undoing it removes it.
            OpKind::DeleteRef { name, prev }
        } else {
            // It existed before: put it back where git still has it.
            OpKind::SetRef {
                name,
                commit: ContentHash::from_git_oid(old_hex).ok_or("bad old oid")?,
                prev,
            }
        };
        self.submit_ref_op(kind, user, cert)
    }

    /// Brings every bare repo under `root` back into agreement with the
    /// view, which is the source of truth. Run at startup, before the
    /// node serves anything, so nothing races the repair.
    ///
    /// The hook's retraction path handles a push that is refused while
    /// the daemon is alive. This handles the rest, and it does so without
    /// needing to know which of them happened: power loss between the
    /// hook's 200 and git writing the ref, a per-ref failure *after*
    /// `pre-receive` passed (`receive.deny*`, an `update` hook, a write
    /// error), or a retraction that could not be delivered. All of them
    /// leave the same state, and it is the state this reads.
    ///
    /// Two repairs, chosen by whether git can honour the view:
    ///
    /// - the commit exists in the repo, so git is simply behind: the ref
    ///   is written. Git is the follower (D21 single-canonical), so
    ///   moving it is the defined direction.
    /// - the commit is absent — a refused push has its objects discarded
    ///   from the quarantine — so the view names something git can never
    ///   have: a compensating op puts the view back to git's value.
    ///
    /// A ref git holds and the view does not is **reported, never
    /// adopted**. Appending an op for it would launder an out-of-band
    /// `update-ref` into the signed log as though it had been submitted.
    pub fn reconcile_git_refs(&self, root: &std::path::Path) -> RefReconciliation {
        let mut report = RefReconciliation::default();
        for finding in self.survey_git_refs(root) {
            let full = finding.name();
            match finding.state {
                // Git is behind on a commit it already has, so move it.
                RefState::GitBehind => {
                    let Some(want) = &finding.log_oid else {
                        continue;
                    };
                    let path = root.join(&finding.repo);
                    match write_git_ref(&path, &finding.refname, want, finding.git_oid.as_deref()) {
                        Ok(()) => report.applied.push(full),
                        Err(e) => report.unreconciled.push(format!("{full}: {e}")),
                    }
                }
                // Git can never hold this value, so the log gives way.
                // `git_abort` builds exactly this inverse: back to git's
                // value, or gone if git has none.
                RefState::LogUnbackable => {
                    let Some(want) = &finding.log_oid else {
                        continue;
                    };
                    let old = finding
                        .git_oid
                        .clone()
                        .unwrap_or_else(|| "0".repeat(want.len()));
                    match self.git_abort(&finding.repo, &finding.refname, &old, want, "node", None)
                    {
                        Ok(()) => report.retracted.push(full),
                        Err(e) => report.unreconciled.push(format!("{full}: {e}")),
                    }
                }
                RefState::GitOnly | RefState::Unreadable => report
                    .unreconciled
                    .push(format!("{full}: {}", finding.reason)),
            }
        }
        report
    }

    /// Every way the repos under `root` and the view currently disagree,
    /// with nothing written and no op appended.
    ///
    /// This is the half of [`Platform::reconcile_git_refs`] that can be
    /// run against a live node. The repair deliberately cannot: it writes
    /// refs, so it belongs before the first request, where nothing races
    /// it. Reading is safe at any time, and without it a divergence that
    /// appears while the daemon is up is invisible until the next
    /// restart — which is the difference between a monitor and an
    /// autopsy.
    ///
    /// Served by `GET /api/ref-agreement`, which is deliberately its own
    /// endpoint rather than a field on `/api/view`: this shells out to
    /// git once per repo, and `/api/view` is on the hot path.
    ///
    /// Scope, stated because [`RefState::GitOnly`] reads like a stronger
    /// claim than it is: only repos the log already names are compared.
    /// A repo with refs and no log entry at all is not surveyed, so this
    /// finds an out-of-band ref beside logged ones, not an entire
    /// smuggled repo.
    pub fn survey_git_refs(&self, root: &std::path::Path) -> Vec<RefFinding> {
        let mut findings = Vec::new();
        let view_refs: Vec<(String, ContentHash)> = self
            .view
            .lock()
            .expect("view lock")
            .refs
            .iter()
            .map(|(name, hash)| (name.clone(), hash.clone()))
            .collect();

        // One `for-each-ref` per repo rather than one `rev-parse` per
        // ref: a view with thousands of refs would otherwise start the
        // daemon with thousands of subprocesses.
        let mut repos: BTreeMap<String, Vec<(String, ContentHash)>> = BTreeMap::new();
        for (name, hash) in view_refs {
            match name.split_once(':') {
                Some((repo, refname)) => repos
                    .entry(repo.to_string())
                    .or_default()
                    .push((refname.to_string(), hash)),
                // Not a git-derived ref (the API can set any name), so
                // there is no repo to compare it against.
                None => continue,
            }
        }

        for (repo, refs) in repos {
            // Ref names come out of the log, which anyone admitted can
            // write to, so they get the same traversal guard as a repo
            // name off the wire.
            if repo.split('/').any(|c| c == ".." || c.is_empty()) || repo.starts_with('/') {
                findings.push(RefFinding::unreadable(&repo, "", "refused as a repo path"));
                continue;
            }
            let path = root.join(&repo);
            let Some(in_git) = read_git_refs(&path) else {
                findings.push(RefFinding::unreadable(
                    &repo,
                    "",
                    &format!(
                        "the log holds {} ref(s) for a repo that cannot be read here",
                        refs.len()
                    ),
                ));
                continue;
            };
            let wanted: BTreeSet<String> = refs.iter().map(|(name, _)| name.clone()).collect();
            for (refname, want) in refs {
                let Some(want_oid) = want.git_oid() else {
                    findings.push(RefFinding::unreadable(
                        &repo,
                        &refname,
                        "log value is not a git oid",
                    ));
                    continue;
                };
                let have = in_git.get(&refname).cloned();
                if have.as_deref() == Some(want_oid.as_str()) {
                    continue;
                }
                // Whether git *can* be moved to the log is the whole
                // difference between the two repairs, so it is decided
                // here, while reading, and not again while writing.
                let state = if object_exists(&path, &want_oid) {
                    RefState::GitBehind
                } else {
                    RefState::LogUnbackable
                };
                findings.push(RefFinding {
                    repo: repo.clone(),
                    refname,
                    log_oid: Some(want_oid),
                    git_oid: have,
                    reason: match state {
                        RefState::GitBehind => "git is behind a commit it already has".into(),
                        _ => "the log names a commit this repo does not have".into(),
                    },
                    state,
                });
            }
            for (refname, oid) in &in_git {
                if !wanted.contains(refname) {
                    // A remote-tracking ref is this repo's own record of
                    // what it pushed elsewhere — the on-box follower feed
                    // writes refs/remotes/<follower>/* on every push — not
                    // canonical state, so its absence from the log is not a
                    // divergence. Skipped only on this side: a log that
                    // *does* name one is still compared above, and still
                    // retracted if git cannot back it.
                    if refname.starts_with("refs/remotes/") {
                        continue;
                    }
                    findings.push(RefFinding {
                        repo: repo.clone(),
                        refname: refname.clone(),
                        log_oid: None,
                        git_oid: Some(oid.clone()),
                        state: RefState::GitOnly,
                        reason: "in git, not in the log".into(),
                    });
                }
            }
        }
        findings
    }

    /// Signs a git-derived ref op as the node and submits it, attributing
    /// it to the pusher's own key when the push certificate verified.
    fn submit_ref_op(
        &self,
        kind: OpKind,
        user: &str,
        cert: Option<(&str, &str)>,
    ) -> Result<(), String> {
        let (node, head) = self.scope_now();
        // Verified push certificate ("G" = good signature) attributes
        // the op to the pusher's own key; otherwise the transport user.
        // Either way the node signs, and the payload says so (D41): the
        // channel prefix alone carried this class only by convention.
        let (workspace, provenance) = push_attribution(user, cert);
        let payload = ViewOp::new(kind)
            .in_scope(node, head)
            .with_provenance(provenance)
            .to_payload();
        let sig = self.node_key.sign_submission(&workspace, &payload);
        self.handle
            .try_submit(&workspace, payload, Some(sig))
            .map(|_| ())?;
        // Every ref movement the node authors ends in an attestation of
        // where the refs now stand. Per accepted ref rather than per
        // push, because the pre-receive protocol has no end-of-push
        // signal to hang a single emission on.
        self.record_snapshot();
        Ok(())
    }

    /// Attests the current ref-state (D25): submits a node-signed
    /// [`OpKind::RecordRefSnapshot`] of the view as it stands, and on
    /// admission projects the snapshot's canonical bytes to
    /// `refs.snapshot` beside the op log — the detached copy a backup
    /// pulls with the log, byte-identical to the payload in it.
    ///
    /// Best-effort on both legs. A lost submission race means another
    /// writer moved the view between read and submit, and that writer's
    /// own ref op ends in another attestation, so the chain catches up
    /// without retries here. The file write is tmp-plus-rename with a
    /// per-call tmp name: concurrent admissions cannot tear the file,
    /// and if their renames land out of order it briefly holds the older
    /// of two valid snapshots until the next attestation replaces it.
    fn record_snapshot(&self) {
        let snapshot = self.view.lock().expect("view lock").snapshot();
        let bytes = snapshot.canonical_bytes();
        let (node, head) = self.scope_now();
        let payload = ViewOp::new(OpKind::RecordRefSnapshot { snapshot })
            .in_scope(node, head)
            .to_payload();
        let channel = "node/snapshot";
        let sig = self.node_key.sign_submission(channel, &payload);
        if self.handle.try_submit(channel, payload, Some(sig)).is_err() {
            return;
        }
        let Some(log) = &self.log_path else { return };
        static TMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = TMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = log.with_file_name(format!("refs.snapshot.{n}.tmp"));
        if std::fs::write(&tmp, &bytes)
            .and_then(|()| std::fs::rename(&tmp, log.with_file_name("refs.snapshot")))
            .is_err()
        {
            // The attestation is in the log either way; a missing
            // detached copy fails the next backup pull loudly, which is
            // the reader that cares.
            std::fs::remove_file(&tmp).ok();
        }
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
        let (node, head) = self.scope_now();
        let payload = ViewOp::new(OpKind::SetWorkspaceHead {
            workspace: workspace.to_string(),
            commit,
            prev,
        })
        .in_scope(node, head)
        .to_payload();
        let sig = self.node_key.sign_submission(attribution, &payload);
        self.handle
            .try_submit(attribution, payload, Some(sig))
            .map(|_| ())
    }

    /// Atomically creates a stable change and registers its workspace at
    /// an exact Git revision with a node-signed operation.
    pub fn create_change(
        &self,
        request: AuthorizedChangeCreate<'_>,
        attribution: &str,
    ) -> Result<choir_sequencer::Accepted, String> {
        let AuthorizedChangeCreate {
            id,
            owner,
            workspace,
            base_hex,
            idempotency_key,
            owner_sig,
            cone,
        } = request;
        let base_revision = ContentHash::from_git_oid(base_hex).ok_or("bad base revision")?;
        let payload = ViewOp::new(OpKind::CreateChange {
            id: id.to_string(),
            owner: owner.to_string(),
            workspace: workspace.to_string(),
            base_revision,
            idempotency_key: idempotency_key.to_string(),
            owner_sig: Some(owner_sig),
            cone,
        })
        .to_payload();
        let sig = self.node_key.sign_submission(attribution, &payload);
        match self
            .handle
            .try_submit(attribution, payload.clone(), Some(sig))
        {
            Ok(accepted) => Ok(accepted),
            Err(reason) => match self
                .entries
                .lock()
                .expect("entries lock")
                .already_applied(attribution, &payload)
            {
                Some((seq, hash)) => Ok(choir_sequencer::Accepted {
                    seq,
                    hash,
                    decision_latency: Duration::ZERO,
                }),
                None => Err(reason),
            },
        }
    }

    /// Decodes the exact owner-signed create binding before any physical
    /// workspace is copied. Sequencer admission verifies the embedded
    /// signature before recording the node-authored change operation.
    pub fn decode_create_change_request(
        &self,
        request: &serde_json::Value,
        expected_id: &str,
        expected_owner: &str,
        expected_workspace: &str,
        expected_revision: &ContentHash,
        expected_idempotency_key: &str,
    ) -> Result<(Witness, Vec<String>), String> {
        let (sub, cone) = decode_create_submission(
            request,
            expected_id,
            expected_owner,
            expected_workspace,
            expected_revision,
            expected_idempotency_key,
        )?;
        // The cone comes back out of the *signed* authorization and
        // never out of the request body around it. That is the whole
        // guarantee: the node reports a scope its owner signed, or it
        // reports none (D50).
        Ok((
            sub.author_sig
                .expect("decode_submission always returns a signature"),
            cone,
        ))
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
        let sub =
            decode_archive_submission(request, expected_id, expected_workspace, expected_revision)?;
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
        decode_archive_submission(request, expected_id, expected_workspace, expected_revision)
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
        let sub =
            decode_archive_submission(request, expected_id, expected_workspace, expected_revision)
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

    /// Inactive change generations that previously used `workspace`.
    /// This supports migration of the original unversioned archive path
    /// when a later generation reuses a deterministic workspace name.
    #[must_use]
    pub fn archived_change_ids_for_workspace(&self, workspace: &str) -> Vec<String> {
        self.view
            .lock()
            .expect("view lock")
            .changes
            .iter()
            .filter(|(_, change)| {
                change.workspace_id == workspace && change.active_workspace.is_none()
            })
            .map(|(id, _)| id.clone())
            .collect()
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

    /// How many workspaces `channel` currently holds (D37).
    ///
    /// Read from the projection replayed out of the op log, so a node
    /// that has just restarted answers the same number it answered
    /// before — the property a per-user ceiling is worthless without.
    #[must_use]
    pub fn workspaces_held_by(&self, channel: &str) -> usize {
        self.workspace_tally
            .lock()
            .expect("workspace tally lock")
            .held_by(channel)
    }

    /// Every workspace the D37 tally is tracking, sorted.
    ///
    /// Exposed for the test that pins the tally against
    /// [`View::workspaces`]: the two are folded from the same operations
    /// and a divergence between them is the tripwire on the D37 register
    /// row.
    #[must_use]
    pub fn tallied_workspaces(&self) -> Vec<String> {
        self.workspace_tally
            .lock()
            .expect("workspace tally lock")
            .workspaces()
            .map(ToString::to_string)
            .collect()
    }

    /// Draws reviewers for unassigned review `id` and records them with
    /// a node-signed op.
    ///
    /// Candidates are the pool minus everyone sharing the requester's
    /// **operator** and, when configured, every operator within the chosen
    /// conflict-graph distance. The draw takes at most one reviewer per
    /// operator so `REQUIRED_APPROVAL_WEIGHT` reviewers means that many
    /// *independent* ones under the configured policy.
    ///
    /// Excluding only the requester's own name was the original rule and
    /// it does not survive the multi-operator case, which is the normal
    /// one: an operator running three agents in the pool satisfies
    /// two-person integrity by themselves, and can manufacture more
    /// agreement by registering more agents. That is the Sybil move D24
    /// says must be blocked at the operator level, so the exclusion has
    /// to be at that level too.
    ///
    /// A pool that cannot supply `REQUIRED_APPROVAL_WEIGHT` distinct operators
    /// draws fewer rather than doubling up — a visibly under-assigned
    /// review beats one that looks independent and is not.
    ///
    /// # Errors
    ///
    /// No pool configured, an unreadable pool or conflict graph, malformed
    /// graph data, no candidate outside the conflict distance, or the
    /// sequencer's rejection reason (e.g. the review was assigned by a
    /// concurrent request).
    pub fn assign_reviewers(&self, id: &str, requester: &str) -> Result<Vec<String>, String> {
        let path = self
            .reviewer_pool
            .as_ref()
            .ok_or("no reviewer pool configured")?;
        let text = std::fs::read_to_string(path).map_err(|e| format!("read reviewer pool: {e}"))?;
        let mine = reviewer_operator(requester);
        let excluded_operators = match &self.reviewer_conflict_graph {
            Some((path, max_distance)) => operators_within_distance(path, mine, *max_distance)?,
            None => BTreeSet::from([mine.to_string()]),
        };
        let mut pool: Vec<String> = text
            .lines()
            .map(str::trim)
            .filter(|l| {
                !l.is_empty()
                    && !l.starts_with('#')
                    && !excluded_operators.contains(reviewer_operator(l))
            })
            .map(String::from)
            .collect();
        if pool.is_empty() {
            return Err(format!(
                "reviewer pool has nobody outside the configured conflict distance from {mine:?}"
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

        let (node, head) = self.scope_now();
        let payload = ViewOp::new(OpKind::AssignReviewers {
            id: id.to_string(),
            reviewers: pool.clone(),
        })
        .in_scope(node, head)
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
        let (node, head) = self.scope_now();
        let submissions: Vec<Submission> = candidates
            .iter()
            .map(|(id, lapsed)| {
                let payload = ViewOp::new(OpKind::ArchiveReview {
                    id: id.clone(),
                    lapsed: *lapsed,
                })
                .in_scope(node.clone(), head.clone())
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

    fn add_newcomer_outcome(
        &self,
        response: &mut serde_json::Value,
        sub: &DecodedSubmission,
        started_at_unix_ms: u64,
        accepted: bool,
        rejection_code: Option<&str>,
    ) {
        let Some(audit) = &self.newcomer_audit else {
            return;
        };
        let Some(actor_key) = sub
            .author_sig
            .as_ref()
            .map(|signature| signature.key_id.as_str())
        else {
            return;
        };
        match audit.lock().expect("newcomer audit lock").observe(
            actor_key,
            started_at_unix_ms,
            accepted,
            rejection_code,
        ) {
            Ok(Some(attempt_id)) => {
                response["newcomer_attempt_id"] = serde_json::json!(attempt_id);
            }
            Ok(None) => {}
            Err(error) => {
                response["newcomer_audit_error"] = serde_json::json!(error);
            }
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

    /// Records every admission decision to `path` as JSONL.
    ///
    /// Derived data (see [`journal`]): the file is appended to from its
    /// own thread, never read back, and its loss or truncation changes
    /// no decision this node makes. Safe to rotate by moving it aside;
    /// the daemon keeps writing to the open handle until restarted.
    ///
    /// # Errors
    ///
    /// If `path` cannot be opened for append.
    pub fn with_journal(self, path: &std::path::Path) -> std::io::Result<Self> {
        self.journal
            .install(Box::new(journal::FileJournal::create(path)?));
        Ok(self)
    }

    /// Appends gate breaches to `path`, one JSON object per line.
    ///
    /// Separate from the op log on purpose: a breach is an observation
    /// about this node's storage and load, not a fact about the ordered
    /// history, and it must not change a hash anyone else replays.
    #[must_use]
    pub fn with_lag_log(mut self, path: std::path::PathBuf) -> Self {
        self.lag_log = Some(path);
        self
    }

    /// Enables outbound ref-landed webhooks (D32) from the subscription
    /// file `config`, recording every delivery attempt in `log`.
    ///
    /// Starts one delivery thread. The sequencer's writer thread never
    /// waits on it: it offers events to a bounded queue and drops
    /// (counted, and written to `log`) when that queue is full, because
    /// a receiver this node does not control must not be able to delay
    /// op admission. See [`crate::hooks`].
    ///
    /// # Errors
    ///
    /// Returns a message when the subscription file cannot be read or
    /// does not parse, or when the delivery thread cannot be started.
    pub fn with_hooks(
        self,
        config: std::path::PathBuf,
        log: std::path::PathBuf,
    ) -> Result<Self, String> {
        let hooks = crate::hooks::Hooks::start(config, log)?;
        *self.hooks.lock().expect("hooks lock") = Some(hooks);
        Ok(self)
    }

    /// Events the webhook queue dropped because it was full. Zero when
    /// no `--hooks-file` is configured.
    #[must_use]
    pub fn hook_drops(&self) -> u64 {
        self.hooks
            .lock()
            .expect("hooks lock")
            .as_ref()
            .map_or(0, crate::hooks::Hooks::dropped)
    }

    /// The live latency record, for a caller that wants to tighten the
    /// gate or read it without going through the API.
    #[must_use]
    pub fn lag(&self) -> Arc<LagMeter> {
        self.lag.clone()
    }

    /// Writes any breaches recorded since the last drain to the lag log.
    ///
    /// Called by the daemon's accept loop, which is the same place it
    /// polls for a failed durability barrier: both are things the writer
    /// thread can only report, never act on. No traffic means no drain,
    /// which is harmless because no traffic also means no breaches.
    pub fn drain_lag_log(&self) {
        let Some(path) = self.lag_log.as_ref() else {
            return;
        };
        let (breaches, dropped) = self.lag.drain();
        if breaches.is_empty() && dropped == 0 {
            return;
        }
        let gate_us = u64::try_from(self.lag.gate().as_micros()).unwrap_or(u64::MAX);
        let mut lines = String::new();
        if dropped > 0 {
            // The gap is written into the log rather than only counted,
            // so a reader of the file alone can see that it is not the
            // whole story.
            lines.push_str(
                &serde_json::json!({
                    "format_version": 1,
                    "event": "breaches_dropped",
                    "count": dropped,
                })
                .to_string(),
            );
            lines.push('\n');
        }
        for breach in breaches {
            lines.push_str(
                &serde_json::json!({
                    "format_version": 1,
                    "event": "gate_breach",
                    "seq": breach.seq,
                    "at_unix_ms": breach.at_unix_ms,
                    "gate_us": gate_us,
                    "decision_us": breach.decision_us,
                    "durable_us": breach.durable_us,
                    "batch": breach.batch,
                })
                .to_string(),
            );
            lines.push('\n');
        }
        let write = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut file| std::io::Write::write_all(&mut file, lines.as_bytes()));
        if let Err(e) = write {
            // The reason, not the path: this string is served over the
            // API, and the path names the operator's home directory.
            let mut last = self.lag_log_error.lock().expect("lag log error lock");
            last.0 = Some(e.to_string());
            last.1 += 1;
        }
    }

    /// The latency report as served under `/api/view`.
    fn lag_json(&self) -> serde_json::Value {
        let report = self.lag.report();
        let last_error = self
            .lag_log_error
            .lock()
            .expect("lag log error lock")
            .clone();
        serde_json::json!({
            "format_version": 1,
            "gate_us": report.gate_us,
            "gate_basis": "dequeue to acknowledgement, durability barrier included",
            "observed_ops": report.observed_ops,
            "decision": {
                "basis": "dequeue to append; the Phase-0 gate as written",
                "p50_us": report.decision_p50_us,
                "p99_us": report.decision_p99_us,
                "max_us": report.decision_max_us,
                "breaches": report.decision_breaches,
            },
            "durable": {
                "basis": "dequeue to acknowledgement; what a submitter waits out",
                "p50_us": report.durable_p50_us,
                "p99_us": report.durable_p99_us,
                "max_us": report.durable_max_us,
                "breaches": report.durable_breaches,
            },
            "percentile_basis": "power-of-two bucket upper bound capped at the observed maximum: over-estimates by at most 2x, never under-estimates",
            "since": "process start; not replayed from the log",
            // Whether, not where. An API client learning the daemon's
            // filesystem layout (which carries the operator's home
            // directory) buys nothing it can act on; the operator already
            // knows the path from the runbook.
            "log_configured": self.lag_log.is_some(),
            "log_error": last_error.0,
            "log_write_failures": last_error.1,
            "pending_breaches": report.pending_breaches,
        })
    }

    /// The sequence the next admitted op will occupy, which is the
    /// cheapest complete description of "what state is this node in".
    ///
    /// Exposed for the browser surface's cache: it asks this before
    /// deciding whether to rebuild a page, so an unchanged node costs
    /// one `u64` read rather than a full view serialization.
    pub fn view_seq(&self) -> u64 {
        self.view.lock().expect("view lock").next_seq
    }

    /// The store's generation and the handle-to-name map to render
    /// channels through (D46).
    ///
    /// Both together because a caller that resolves names has to cache
    /// on the generation the map was read at: revoking an account
    /// deletes a name and appends no op, so the view sequence does not
    /// move and a page keyed on it alone would keep showing the deleted
    /// name.
    ///
    /// `(0, empty)` on a node with no accounts store attached, which
    /// resolves nothing and renders every channel as itself — the same
    /// answer a store gives for an account issued before D46 or issued
    /// with an explicit `user`.
    #[must_use]
    pub fn roster(&self) -> (u64, std::collections::BTreeMap<String, String>) {
        match &*self.passkeys.lock().expect("accounts lock") {
            Some(accounts) => (accounts.generation(), accounts.roster()),
            None => (0, std::collections::BTreeMap::new()),
        }
    }

    /// Repository the review `id` proposes to land on, in the canonical
    /// D29 spelling, or `None` when the review is unknown or unbound.
    ///
    /// Exposed for per-repository authorization: it is what lets posting
    /// a verdict require write on the repository under review, instead
    /// of the node-wide grant every review op would otherwise need.
    pub fn review_repo(&self, id: &str) -> Option<String> {
        self.view
            .lock()
            .expect("view lock")
            .reviews
            .get(id)?
            .target_ref
            .as_deref()
            .and_then(|target| target.split_once(':'))
            .map(|(repo, _)| crate::acl::normalize_repo(repo))
    }

    /// One review as the API renders it, or `None` when no such review
    /// exists.
    ///
    /// Exposed for the D34 review page, which needs one review rather
    /// than the whole view. Same `review_json` the API uses, so the page
    /// and the API cannot describe a review differently.
    pub fn review_json(&self, id: &str) -> Option<serde_json::Value> {
        let view = self.view.lock().expect("view lock");
        view.reviews.get(id).map(review_json)
    }

    /// Every review whose target ref names `repo`, newest id last.
    ///
    /// A review with no target ref belongs to no repository and is
    /// omitted: it cannot be shown under one without asserting a
    /// relationship the requester never signed.
    pub fn reviews_for_repo(&self, repo: &str) -> Vec<(String, serde_json::Value)> {
        let wanted = crate::acl::normalize_repo(repo);
        let view = self.view.lock().expect("view lock");
        view.reviews
            .iter()
            .filter(|(_, r)| {
                r.target_ref
                    .as_deref()
                    .and_then(|target| target.split_once(':'))
                    .is_some_and(|(named, _)| crate::acl::normalize_repo(named) == wanted)
            })
            .map(|(id, r)| (id.clone(), review_json(r)))
            .collect()
    }

    /// Every live review that `channel` was drawn for and has not
    /// answered, paired with the repository its target ref names.
    ///
    /// The reviewer's own queue, which the view has always held and no
    /// page has ever shown. Three filters and no others:
    ///
    /// - **drawn**: `channel` is on the review's reviewer list. Being
    ///   able to read a repository is not being asked about it.
    /// - **unanswered**: no verdict of theirs stands. A review they have
    ///   already answered is not owed, whatever anybody else has said.
    /// - **live**: not archived. An archived review has dropped its
    ///   verdicts, so an answer to it would land nowhere.
    ///
    /// A review with no target ref is omitted for the same reason
    /// [`Self::reviews_for_repo`] omits one: it belongs to no repository,
    /// and this page's rows are grouped by one. It cannot become
    /// invisible that way — nothing can be drawn on a ref that is not
    /// named.
    ///
    /// **This does no authorization.** The caller filters by what the
    /// reader may read, because the ACL lives there and a second copy of
    /// that decision here is a second copy that can disagree.
    pub fn reviews_awaiting(&self, channel: &str) -> Vec<(String, String, serde_json::Value)> {
        let view = self.view.lock().expect("view lock");
        view.reviews
            .iter()
            .filter(|(_, r)| !matches!(r.status, choir_view::ReviewStatus::Archived { .. }))
            .filter(|(_, r)| r.reviewers.iter().any(|who| who == channel))
            .filter(|(_, r)| !r.verdicts.contains_key(channel))
            .filter_map(|(id, r)| {
                let repo = r
                    .target_ref
                    .as_deref()
                    .and_then(|target| target.split_once(':'))
                    .map(|(named, _)| crate::acl::normalize_repo(named))?;
                Some((id.clone(), repo, review_json(r)))
            })
            .collect()
    }

    /// Handles one `/api/...` request, returning `(status, json_body)`.
    pub fn handle_api(&self, method: &str, path: &str, body: &[u8]) -> (u16, String) {
        match (method, path) {
            // Query-tolerant: `bound` reads `?limit=`/`?offset=` off the
            // same URL after this returns, and an exact match here would
            // have sent every paged request to the catch-all instead.
            ("GET", path) if path == "/api/view" || path.starts_with("/api/view?") => {
                let protected_path = self
                    .protected_refs
                    .lock()
                    .expect("protected refs lock")
                    .clone();
                let protected = ProtectedPolicySnapshot::read(protected_path.as_deref());
                // Read before the view guard: the audit mutex is taken
                // under the view lock nowhere else, and this keeps it
                // that way.
                let cohort = new_actor_cohort(self.newcomer_audit.as_ref());
                // Writer order is view -> concentration. Holding both
                // through snapshot construction prevents a response whose
                // heads include op N while `as_of_seq` and attribution stop
                // at N-1. The guards are dropped before byte measurement
                // and final response serialization.
                let view = self.view.lock().expect("view lock");
                let concentration_state = self.concentration.lock().expect("concentration lock");
                let key_names = self.key_names.lock().expect("key names lock");
                let ws: BTreeMap<_, _> = view
                    .workspaces
                    .iter()
                    .map(|(k, v)| (k.clone(), v.to_hex()))
                    .collect();
                let refs: BTreeMap<_, _> = view
                    .refs
                    .iter()
                    .map(|(k, v)| (k.clone(), v.to_hex()))
                    .collect();
                let reviews: BTreeMap<_, _> = view
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
                                // Omitted when empty, so an unscoped
                                // change reads as it always did rather
                                // than gaining an empty list (D50).
                                "cone": (!change.cone.is_empty())
                                    .then(|| change.cone.clone()),
                            }),
                        )
                    })
                    .collect();
                let bindings: BTreeMap<_, _> = view
                    .bindings
                    .iter()
                    .map(|(key_id, binding)| (key_id.clone(), binding_json(binding)))
                    .collect();
                // The latest admitted ref-state attestation (D25), as a
                // summary: `refs` above already carries the full map, so
                // repeating it here would double the hot-path response
                // for no reader.
                let snapshot = view.latest_snapshot.as_ref().map(|s| {
                    serde_json::json!({
                        "id": s.id().to_hex(),
                        "at_seq": s.at_seq,
                        "prev_snapshot": s.prev_snapshot.as_ref().map(ContentHash::to_hex),
                    })
                });
                let ws = serde_json::json!(ws);
                let refs = serde_json::json!(refs);
                let reviews = serde_json::json!(reviews);
                let provenance = serde_json::json!(&view.provenance);
                // Not folded into `view_growth`'s authoritative total:
                // that byte count is a tracked series, and a fifth
                // section would move every past reading. Measured beside
                // `bindings` instead, for the same reason.
                let checks = serde_json::json!(&view.checks);
                let bindings = serde_json::json!(bindings);
                // D65. Serialized straight from the fold's own maps:
                // there is no projection to write, and a hand-built one
                // would be a second place for the shape to drift.
                let vouches = serde_json::json!(&view.vouches);
                // D67, on the same terms: the fold's map, not a
                // projection of it.
                let witnessed = serde_json::json!(&view.witnessed);
                let counts = view_growth_counts(&view);
                let as_of_seq = concentration_state.as_of_seq;
                // T3 attribution reads the durable record, not the keys
                // file: a tripwire whose evidence the operator can edit in
                // place measures the operator's honesty, not concentration.
                // `key_names` still supplies the trusted population, which
                // no op in the log can answer.
                let durable_bindings = KeyBindings::from_view(&view, &key_names);
                let concentration =
                    concentration_json(&concentration_state, &durable_bindings, &protected);
                let new_actor_review_outcomes = new_actor_review_outcomes_json(
                    &concentration_state,
                    &view,
                    cohort.as_ref(),
                    self.review_adjudications.as_deref(),
                );
                drop(key_names);
                drop(concentration_state);
                // Read under the guard, reported below it. The guard is
                // released before `scope_now`, so the position has to be
                // taken while the view still holds still.
                let next_seq = view.next_seq;
                drop(view);
                let view_growth = view_growth_json(
                    counts,
                    &ws,
                    &refs,
                    &reviews,
                    &provenance,
                    &[
                        ("bindings", &bindings),
                        ("vouches", &vouches),
                        ("witnessed", &witnessed),
                    ],
                    as_of_seq,
                );
                let newcomer_harm = newcomer_harm_json(self.newcomer_audit.as_ref());
                // What a client needs to bind its next signature to this
                // log: which node, which head, and whether that head is
                // being enforced. Read here rather than under the view
                // guard above — an op that lands in between only makes
                // `head` one entry stale, and a scope naming any head
                // still in the window is admissible.
                let (node, head) = self.scope_now();
                let log = serde_json::json!({
                    "node": node.to_hex(),
                    "head": head.as_ref().map(ContentHash::to_hex),
                    // The position this view describes. It belongs to the
                    // log rather than beside it, which is also what keeps
                    // it readable: `log` is `Disclosure::Public`, and a
                    // reader granted one repository still needs to know
                    // where the view they were handed sits. It says no
                    // more about a repository than `head` already does.
                    //
                    // Without it the view could not say its own position,
                    // and a caller wanting the age of anything had
                    // nothing to measure against -- which is exactly how
                    // `ops_since_binding` came to be zero for every key
                    // on every real node while its doctest passed.
                    "next_seq": next_seq,
                    "scope_required": self
                        .require_scope
                        .load(std::sync::atomic::Ordering::Relaxed),
                });
                let mut body = serde_json::json!({
                    "log": log,
                    "snapshot": snapshot,
                    "workspaces": ws,
                    "changes": changes,
                    "refs": refs,
                    "reviews": reviews,
                    "provenance": provenance,
                    "checks": checks,
                    "bindings": bindings,
                    "vouches": vouches,
                    "witnessed": witnessed,
                    "concentration": concentration,
                    "view_growth": view_growth,
                    "newcomer_harm": newcomer_harm,
                    "new_actor_review_outcomes": new_actor_review_outcomes,
                    "sequencer_lag": self.lag_json(),
                    "build": crate::build_json(),
                });
                // D80. `null` on a home, which is nobody's copy; on a seed,
                // whose copy it is and how far behind.
                body["replica"] = match (self.home.get(), self.replica.get()) {
                    (Some(home), Some(shared)) => shared
                        .status
                        .lock()
                        .expect("replica status lock")
                        .to_json(home),
                    _ => serde_json::Value::Null,
                };
                (200, body.to_string())
            }
            // Pending queue for one reviewer: reviews that fanned out to
            // them and are still unanswered by them.
            ("GET", path) if path.starts_with("/api/reviews") => {
                let reviewer = decode_query_value(
                    path.split_once("reviewer=")
                        .map(|(_, v)| v.split('&').next().unwrap_or(v))
                        .unwrap_or(""),
                );
                let reviewer = reviewer.as_str();
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
            ("POST", "/api/appeal") => {
                let req: serde_json::Value = match serde_json::from_slice(body) {
                    Ok(value) => value,
                    Err(error) => {
                        return (
                            400,
                            Rejection::new(
                                Code::MalformedRequest,
                                format!("request body is not valid JSON: {error}"),
                                "send {\"attempt_id\": N} using the newcomer_attempt_id from \
                                 the rejected response",
                            )
                            .body(),
                        );
                    }
                };
                let Some(attempt_id) = req.get("attempt_id").and_then(serde_json::Value::as_u64)
                else {
                    return (
                        400,
                        Rejection::new(
                            Code::MalformedRequest,
                            "appeal needs an integer attempt_id",
                            "send the newcomer_attempt_id from the rejected response",
                        )
                        .body(),
                    );
                };
                let Some(audit) = &self.newcomer_audit else {
                    return (
                        503,
                        Rejection::new(
                            Code::PolicyUnavailable,
                            "newcomer audit is not enabled",
                            "ask the operator to enable the newcomer audit before filing appeals",
                        )
                        .body(),
                    );
                };
                match audit
                    .lock()
                    .expect("newcomer audit lock")
                    .appeal(attempt_id)
                {
                    Ok(()) => (
                        200,
                        serde_json::json!({ "appealed": attempt_id }).to_string(),
                    ),
                    Err(error) => (
                        400,
                        Rejection::new(
                            Code::MalformedRequest,
                            error,
                            "use the attempt id from a rejected first-attempt response",
                        )
                        .body(),
                    ),
                }
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
                if ops.len() > self.batch_limit {
                    return (
                        413,
                        serde_json::json!({
                            "error": "batch has too many operations",
                            "limit_ops": self.batch_limit,
                            "actual_ops": ops.len(),
                        })
                        .to_string(),
                    );
                }
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
                    let mut one = decode_submission(op);
                    if let Ok(sub) = one.as_mut() {
                        self.stamp_credential_key(sub);
                    }
                    decoded.push(one);
                }
                let started_at_unix_ms: Vec<u64> = decoded.iter().map(|_| unix_ms()).collect();
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
                for (index, d) in decoded.iter().enumerate() {
                    let value = match d {
                        Err(reason) => {
                            rejected += 1;
                            serde_json::json!({ "error": reason })
                        }
                        Ok(sub) => match outcomes.next().expect("one outcome per submitted op") {
                            Ok(acc) => {
                                accepted += 1;
                                let mut value = self.batch_result(acc, sub);
                                self.add_newcomer_outcome(
                                    &mut value,
                                    sub,
                                    started_at_unix_ms[index],
                                    true,
                                    None,
                                );
                                value
                            }
                            Err(reason) => {
                                rejected += 1;
                                let rejection = Rejection::decode(&reason).to_json();
                                let code = rejection["code"].as_str().map(str::to_string);
                                let mut value = serde_json::json!({ "error": reason });
                                self.add_newcomer_outcome(
                                    &mut value,
                                    sub,
                                    started_at_unix_ms[index],
                                    false,
                                    code.as_deref(),
                                );
                                value
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
                let f = |k: &str| {
                    req.get(k)
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string()
                };
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
            // The other half of the same hook: the push is being refused,
            // so every ref already accepted for it has to be put back.
            ("POST", "/api/git-abort") => {
                let req: serde_json::Value = match serde_json::from_slice(body) {
                    Ok(v) => v,
                    Err(e) => return (400, Rejection::new(
                            Code::MalformedRequest,
                            format!("request body is not valid JSON: {e}"),
                            "send a JSON object; GET /llms.txt lists the fields each endpoint wants",
                        )
                        .body()),
                };
                let f = |k: &str| {
                    req.get(k)
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string()
                };
                let (status, signer) = (f("cert_status"), f("signer"));
                match self.git_abort(
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
            ("GET", "/api/signers") => (200, self.signers_json().to_string()),
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
        let req: serde_json::Value =
            match serde_json::from_slice(body) {
                Ok(v) => v,
                Err(e) => return (
                    400,
                    Rejection::new(
                        Code::MalformedRequest,
                        format!("request body is not valid JSON: {e}"),
                        "send a JSON object; GET /llms.txt lists the fields each endpoint wants",
                    )
                    .body(),
                ),
            };
        let mut sub = match decode_submission(&req) {
            Ok(sub) => sub,
            Err(reason) => return (400, Rejection::decode(&reason).body()),
        };
        self.stamp_credential_key(&mut sub);
        let started_at_unix_ms = unix_ms();
        match self
            .handle
            .try_submit(&sub.channel, sub.payload.clone(), sub.author_sig.clone())
        {
            Ok(acc) => {
                let mut response = self.batch_result(acc, &sub);
                self.add_retention_outcome(&mut response);
                self.add_newcomer_outcome(&mut response, &sub, started_at_unix_ms, true, None);
                (200, response.to_string())
            }
            Err(reason) => {
                // A retry of an operation that already landed fails CAS
                // in exactly the same way as a genuine conflict. They
                // call for opposite actions -- read back and proceed,
                // versus re-read and rebase -- so an agent that cannot
                // tell them apart either retries a completed write or
                // abandons a successful one.
                //
                // Only the policy's own duplicate refusal is converted,
                // and that is load-bearing. This lookup used to run for
                // *any* rejection, which meant a submission carrying a
                // corrupted signature over an already-applied payload was
                // answered 200 `already_applied`: the signature check had
                // failed, and the response said success. It changed no
                // state, but a check whose failure is reported as a
                // success is not a check. `duplicate_submission` is
                // raised only after the signature verifies.
                if Rejection::decode(&reason).code == Code::DuplicateSubmission.as_str() {
                    if let Some((seq, hash)) = self
                        .entries
                        .lock()
                        .expect("entries lock")
                        .already_applied(&sub.channel, &sub.payload)
                    {
                        let mut response = serde_json::json!({
                            "seq": seq,
                            "hash": hash.to_hex(),
                            "already_applied": true,
                        });
                        self.add_newcomer_outcome(
                            &mut response,
                            &sub,
                            started_at_unix_ms,
                            true,
                            None,
                        );
                        return (200, response.to_string());
                    }
                }
                let mut response = Rejection::decode(&reason).to_json();
                let code = response["code"].as_str().map(str::to_string);
                self.add_newcomer_outcome(
                    &mut response,
                    &sub,
                    started_at_unix_ms,
                    false,
                    code.as_deref(),
                );
                (400, response.to_string())
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
    fn batch_result(
        &self,
        acc: choir_sequencer::Accepted,
        sub: &DecodedSubmission,
    ) -> serde_json::Value {
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

/// How one ref disagrees between the log and a bare repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefState {
    /// The log names a commit the repo has but is not pointing at. Git is
    /// the follower, so this is repaired by moving git.
    GitBehind,
    /// The log names a commit the repo does not have and cannot get — a
    /// refused push's objects go out with the quarantine. Repaired by the
    /// log giving way, through a compensating op.
    LogUnbackable,
    /// Git holds a ref the log has never seen. Reported only: adopting it
    /// would launder an out-of-band `update-ref` into the signed log.
    GitOnly,
    /// The comparison could not be made at all.
    Unreadable,
}

impl RefState {
    /// Stable wire name, so a monitor can match on it.
    pub fn as_str(self) -> &'static str {
        match self {
            RefState::GitBehind => "git_behind",
            RefState::LogUnbackable => "log_unbackable",
            RefState::GitOnly => "git_only",
            RefState::Unreadable => "unreadable",
        }
    }
}

/// One disagreement found by [`Platform::survey_git_refs`].
#[derive(Debug, Clone)]
pub struct RefFinding {
    /// Repo the ref lives in, as it appears in the namespaced log name.
    pub repo: String,
    /// Ref name inside that repo; empty when the whole repo is the
    /// problem.
    pub refname: String,
    /// What the log says, as a git oid.
    pub log_oid: Option<String>,
    /// What the repo says.
    pub git_oid: Option<String>,
    /// Which disagreement this is.
    pub state: RefState,
    /// Human-readable cause, for the startup log and the endpoint.
    pub reason: String,
}

impl RefFinding {
    fn unreadable(repo: &str, refname: &str, reason: &str) -> Self {
        Self {
            repo: repo.to_string(),
            refname: refname.to_string(),
            log_oid: None,
            git_oid: None,
            state: RefState::Unreadable,
            reason: reason.to_string(),
        }
    }

    /// The namespaced `<repo>:<refname>` name, as the log spells it.
    pub fn name(&self) -> String {
        if self.refname.is_empty() {
            self.repo.clone()
        } else {
            format!("{}:{}", self.repo, self.refname)
        }
    }

    /// The finding as it goes over the wire.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "ref": self.name(),
            "state": self.state.as_str(),
            "log_oid": self.log_oid,
            "git_oid": self.git_oid,
            "reason": self.reason,
        })
    }
}

/// What [`Platform::reconcile_git_refs`] did, so a caller can report it.
/// An all-empty report is the normal case and the only silent one.
#[derive(Debug, Default)]
pub struct RefReconciliation {
    /// Refs written into git because the view held a commit git already
    /// had but had not pointed at.
    pub applied: Vec<String>,
    /// Refs the view gave up, because git can never hold the commit they
    /// named. Each one is a compensating op in the log.
    pub retracted: Vec<String>,
    /// Disagreements left standing, each with its reason. These need an
    /// operator: repairing them automatically would either lose history
    /// or launder an out-of-band ref into the signed log.
    pub unreconciled: Vec<String>,
}

impl RefReconciliation {
    /// Whether anything at all was out of agreement.
    pub fn is_empty(&self) -> bool {
        self.applied.is_empty() && self.retracted.is_empty() && self.unreconciled.is_empty()
    }
}

/// Every ref in the bare repo at `path`, or `None` if it cannot be read
/// (no such repo, or not a repo).
pub(crate) fn read_git_refs(path: &std::path::Path) -> Option<BTreeMap<String, String>> {
    let out = std::process::Command::new("git")
        .args(["for-each-ref", "--format=%(refname) %(objectname)"])
        .current_dir(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| {
                let (refname, oid) = line.split_once(' ')?;
                Some((refname.to_string(), oid.to_string()))
            })
            .collect(),
    )
}

/// Whether `oid` names an object the repo actually has. A refused push
/// leaves its objects in a discarded quarantine, so the view can name a
/// commit that was never admitted to the repo.
fn object_exists(path: &std::path::Path, oid: &str) -> bool {
    std::process::Command::new("git")
        .args(["cat-file", "-e", &format!("{oid}^{{object}}")])
        .current_dir(path)
        .output()
        .is_ok_and(|out| out.status.success())
}

/// Points `refname` at `oid`, CAS'd on `have` so a concurrent writer
/// loses rather than gets clobbered.
fn write_git_ref(
    path: &std::path::Path,
    refname: &str,
    oid: &str,
    have: Option<&str>,
) -> Result<(), String> {
    let old = have.map_or_else(|| "0".repeat(oid.len()), str::to_string);
    let out = std::process::Command::new("git")
        .args(["update-ref", refname, oid, &old])
        .current_dir(path)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        return Ok(());
    }
    Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
}

/// Git's "this ref does not exist" oid: all zeros, at whatever width the
/// repo's hash function uses.
fn is_zero_oid(hex: &str) -> bool {
    !hex.is_empty() && hex.chars().all(|c| c == '0')
}

/// Parses an undirected operator graph and returns every operator within
/// `max_distance` of `start`, including `start` itself at distance zero.
fn operators_within_distance(
    path: &std::path::Path,
    start: &str,
    max_distance: usize,
) -> Result<BTreeSet<String>, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("read reviewer conflict graph: {e}"))?;
    let mut graph: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_whitespace();
        let left = fields.next();
        let right = fields.next();
        if left.is_none() || right.is_none() || fields.next().is_some() {
            return Err(format!(
                "reviewer conflict graph line {} must be '<operator> <operator>'",
                index + 1
            ));
        }
        let left = left.expect("checked above");
        let right = right.expect("checked above");
        if left.contains('/') || right.contains('/') {
            return Err(format!(
                "reviewer conflict graph line {} must name operators, not operator/agent channels",
                index + 1
            ));
        }
        graph
            .entry(left.to_string())
            .or_default()
            .insert(right.to_string());
        graph
            .entry(right.to_string())
            .or_default()
            .insert(left.to_string());
    }

    let mut seen = BTreeSet::from([start.to_string()]);
    let mut queue = VecDeque::from([(start.to_string(), 0usize)]);
    while let Some((operator, distance)) = queue.pop_front() {
        if distance == max_distance {
            continue;
        }
        let Some(neighbors) = graph.get(&operator) else {
            continue;
        };
        for neighbor in neighbors {
            if seen.insert(neighbor.clone()) {
                queue.push_back((neighbor.clone(), distance + 1));
            }
        }
    }
    Ok(seen)
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

fn decode_create_submission(
    request: &serde_json::Value,
    expected_id: &str,
    expected_owner: &str,
    expected_workspace: &str,
    expected_revision: &ContentHash,
    expected_idempotency_key: &str,
) -> Result<(DecodedSubmission, Vec<String>), String> {
    let sub = decode_submission(request).map_err(|reason| {
        Rejection::new(
            Code::MalformedRequest,
            reason,
            "sign the exact CreateAuthorization payload with the requested owner key and include channel, payload_hex, key_id and signature_hex",
        )
        .encode()
    })?;
    let authorization = CreateAuthorization::from_payload(&sub.payload)
        .map_err(|error| crate::reject::from_view_error(&error).encode())?;
    match authorization {
        CreateAuthorization {
            id,
            owner,
            workspace,
            base_revision,
            idempotency_key,
            cone,
            ..
        } if id == expected_id
            && owner == expected_owner
            && sub.channel == expected_owner
            && workspace == expected_workspace
            && &base_revision == expected_revision
            && idempotency_key == expected_idempotency_key => Ok((sub, cone)),
        _ => Err(Rejection::new(
            Code::WorkspaceState,
            "signed create payload does not match the requested owner, change, workspace, base and idempotency key",
            "rebuild CreateAuthorization from the exact request binding, sign it on the owner channel and retry",
        )
        .encode()),
    }
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
    // A submission that names no scheme is ed25519, the only one that
    // existed before D39 -- the same rule `Witness::scheme_id` applies to
    // stored entries, so the wire and the log agree about what silence
    // means.
    let author_sig = match req.get("scheme").and_then(serde_json::Value::as_u64) {
        None => Witness::ed25519(key_id.to_string(), signature),
        Some(tag) if tag == u64::from(choir_oplog::scheme::WEBAUTHN_ES256) => {
            let (Some(auth_hex), Some(client_hex)) = (
                field("authenticator_data_hex"),
                field("client_data_json_hex"),
            ) else {
                return Err(
                    "a webauthn signature needs authenticator_data_hex and client_data_json_hex"
                        .to_string(),
                );
            };
            let (Some(authenticator_data), Some(client_data_json)) =
                (hex_decode(auth_hex), hex_decode(client_hex))
            else {
                return Err("bad hex in the webauthn fields".to_string());
            };
            Witness::webauthn_es256(
                key_id.to_string(),
                signature,
                authenticator_data,
                client_data_json,
            )
        }
        // Named rather than silently reinterpreted. A scheme this binary
        // cannot check is refused here, where the caller learns which tag
        // was rejected, instead of being handed to ed25519 and failing as
        // a bad signature.
        Some(tag) => return Err(format!("unknown signature scheme {tag}")),
    };
    Ok(DecodedSubmission {
        channel: channel.to_string(),
        payload,
        author_sig: Some(author_sig),
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
    let mut value = serde_json::json!({
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
    });
    // D39 put three more fields inside `Witness` and D45 a fourth, and
    // therefore inside the hashed form. Omitting them here made a
    // passkey-signed entry unreproducible: a client rebuilding from the
    // served fields computes a different hash and cannot tell a
    // legitimate entry from a lying node. That is the exact failure the
    // witnesses comment above warns about, arriving through a different
    // field — the fields were added to the format and not to this shape.
    //
    // So this block grows with `Witness`, every time, and forgetting it
    // is silent. `sync_contract.rs` is where that is caught.
    //
    // Emitted only when present, so an ed25519 entry's served JSON is
    // byte-identical to what it always was and no existing client sees a
    // new key. That mirrors how they are serialized in the hashed form.
    if let Some(sig) = e.author_sig.as_ref() {
        let object = value.as_object_mut().expect("entry_json builds an object");
        if let Some(scheme) = sig.scheme {
            object.insert("author_scheme".into(), serde_json::json!(scheme));
        }
        if let Some(data) = sig.authenticator_data.as_ref() {
            object.insert(
                "authenticator_data_hex".into(),
                serde_json::json!(hex_encode(data)),
            );
        }
        if let Some(data) = sig.client_data_json.as_ref() {
            object.insert(
                "client_data_json_hex".into(),
                serde_json::json!(hex_encode(data)),
            );
        }
        if let Some(key) = sig.credential_key.as_ref() {
            object.insert(
                "credential_key_hex".into(),
                serde_json::json!(hex_encode(key)),
            );
        }
    }
    value
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

/// Percent-decodes one query-string value.
///
/// `/api/reviews?reviewer=` compares its value against a channel name,
/// and every channel here is `operator/agent` — a name with a slash in
/// it. A client that escapes the slash, which the `choir` CLI does, was
/// answered with an empty queue rather than with its reviews: `choir
/// reviews` reported nothing to do to the very reviewer `choir state`
/// named as blocking a change. The endpoint's own tests missed it by
/// using single-word reviewer names, which no real channel is.
///
/// Undecodable input is returned unchanged rather than dropped: a name
/// that was never encoded is still a name, and the comparison it fails
/// is the right outcome for one that is genuinely unknown.
fn decode_query_value(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let Some(byte) = raw
                    .get(i + 1..i + 3)
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                else {
                    return raw.to_string();
                };
                out.push(byte);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| raw.to_string())
}

/// Drops a trailing `\n` and an optional preceding `\r`, so a line read
/// with `read_until` decodes the same as one produced by `.lines()`.
fn trim_newline(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// JSON shape of one review's state (shared by /api/view and
/// /api/reviews).
/// One durable key binding, as `/api/view` reports it.
///
/// Keyed by actor id, so a client joins this against `author_sig.key_id`
/// and the `key_id` a witness carries without deriving anything.
///
/// `bound_at` is the seq of the op that *first* bound the key and never
/// moves, which is what makes it orderable; a later re-bind changes only
/// `channel`. `revoked` is present and non-null once withdrawn, and the
/// row survives revocation on purpose — attribution for past work must
/// not disappear at the moment revocation makes it interesting.
fn binding_json(binding: &choir_view::KeyBinding) -> serde_json::Value {
    serde_json::json!({
        "operator": binding.operator,
        "channel": binding.channel,
        "bound_at": binding.bound_at,
        "revoked": binding.revoked.as_ref().map(|revocation| {
            serde_json::json!({ "at": revocation.at, "reason": revocation.reason })
        }),
    })
}

fn review_json(r: &choir_view::ReviewState) -> serde_json::Value {
    let verdicts: std::collections::BTreeMap<_, _> = r
        .verdicts
        .iter()
        .map(|(who, answer)| {
            (
                who.clone(),
                // `at` is served for the same reason `comments[].at` is,
                // plus one specific to D44: it is what decides which key
                // gets credited for this approval, so a client checking
                // the `expected` authorization a rejection hands back
                // cannot rederive it without this number.
                serde_json::json!({
                    "verdict": format!("{:?}", answer.verdict),
                    "note": answer.note,
                    "at": answer.at,
                }),
            )
        })
        .collect();
    // An array, not an object: the thread's order is the order the
    // sequencer admitted it, and a JSON object keyed by comment id would
    // invite a reader to sort by something else (D38).
    let comments: Vec<_> = r
        .comments
        .iter()
        .map(|c| {
            serde_json::json!({
                "id": c.id,
                "author": c.author,
                "body": c.body,
                "at": c.at,
            })
        })
        .collect();
    serde_json::json!({
        "target": r.target.as_ref().map(choir_oplog::ContentHash::to_hex),
        "target_ref": r.target_ref,
        "reviewers": r.reviewers,
        "comments": comments,
        "verdicts": verdicts,
        // viewer → fold position of that viewer's first read. What lets
        // an author tell "reviewed and ignored" from "nobody looked".
        "viewed": r.viewed,
        "slashes": r.slashes,
        "complete": r.complete(),
        "approved": r.approved(),
        "approval_weight": r.approval_weight(),
        "re_review_required": r.re_review_required(),
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

#[cfg(test)]
mod magic_ref_tests {
    use super::MagicRef;

    fn ok(refname: &str) -> MagicRef {
        MagicRef::parse(refname)
            .unwrap_or_else(|| panic!("{refname} is not recognised as a magic ref"))
            .unwrap_or_else(|e| panic!("{refname} refused: {e}"))
    }

    fn refused(refname: &str) -> String {
        MagicRef::parse(refname)
            .unwrap_or_else(|| panic!("{refname} was not read as a magic ref at all"))
            .expect_err("expected a refusal")
    }

    #[test]
    fn an_ordinary_ref_is_not_magic_at_all() {
        // The cost of the feature on every normal push is this `None`.
        assert!(MagicRef::parse("refs/heads/main").is_none());
        assert!(MagicRef::parse("refs/tags/v1").is_none());
        // Adjacent but not under the prefix.
        assert!(MagicRef::parse("refs/format/main/x").is_none());
    }

    #[test]
    fn a_branch_and_topic_become_a_destination_and_a_review_id() {
        let magic = ok("refs/for/main/fix-parser");
        assert_eq!(magic.onto, "main");
        assert_eq!(magic.review_id, "for-main-fix-parser");
    }

    #[test]
    fn re_pushing_the_same_topic_reaches_the_same_review() {
        assert_eq!(
            ok("refs/for/main/fix-parser").review_id,
            ok("refs/for/main/fix-parser").review_id
        );
    }

    #[test]
    fn the_same_topic_onto_two_branches_is_two_reviews() {
        // Without the branch in the id, retargeting would silently
        // collide with somebody's proposal onto another branch.
        assert_ne!(
            ok("refs/for/main/fix").review_id,
            ok("refs/for/release/fix").review_id
        );
    }

    #[test]
    fn a_deeper_topic_still_yields_one_path_segment() {
        // Review ids reach a URL, which admits one segment.
        let magic = ok("refs/for/main/team/fix-parser");
        assert!(!magic.review_id.contains('/'));
        assert_eq!(magic.review_id, "for-main-team-fix-parser");
    }

    #[test]
    fn a_missing_topic_is_refused_with_the_reason() {
        // The case that would otherwise make every proposal onto `main`
        // fight over one ref.
        let reason = refused("refs/for/main");
        assert!(reason.contains("<topic>"), "{reason}");
        assert!(reason.contains("shared"), "{reason}");
    }

    #[test]
    fn a_topic_that_cannot_be_a_review_id_is_refused_not_mangled() {
        // Silently sanitising would map two topics onto one review.
        let reason = refused("refs/for/main/fix parser");
        assert!(reason.contains("review id"), "{reason}");
    }

    #[test]
    fn an_unsafe_branch_is_refused() {
        assert!(refused("refs/for/../fix").contains("branch name"));
    }
}
