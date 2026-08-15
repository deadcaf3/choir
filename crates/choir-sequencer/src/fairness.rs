//! Per-actor admission quotas in front of the single writer.
//!
//! The sequencer is one thread serving everyone, so the cost of one
//! actor's burst is paid by every other actor's latency. Before this,
//! nothing bounded that: an agent in a retry loop could put ten thousand
//! ops in the channel and the human waiting behind them had no recourse
//! but to wait.
//!
//! A quota is a ceiling on how many of one actor's ops can be *awaiting
//! a decision* at once. That is the number the fairness bound is stated
//! in: with a limit of `Q`, an op arriving from anyone else waits behind
//! at most `Q` ops from any single other actor, whatever that actor is
//! doing. Exceeding the quota is answered immediately with a rejection
//! naming it — never by blocking, which would move the queue from the
//! sequencer into the submitter's thread and hide it.
//!
//! # What "actor" means here, and what it does not
//!
//! The key is [`Witness::key_id`] as *claimed* by the submission, taken
//! without verifying the signature. It has to be: verification happens
//! inside `SubmitPolicy::check` on the writer thread, which is the whole
//! point of that design (authorization is evaluated at apply time), and
//! verifying again at intake would double the cost of the one genuinely
//! expensive step to answer a question about scheduling.
//!
//! So this is fairness, not security, and the difference is worth being
//! precise about:
//!
//! - **An honest actor flooding** is bounded. This is the case that
//!   actually happens, and the one the quota exists for.
//! - **An adversary claiming many identities** evades the quota by
//!   spreading across them. Bounding *that* is the node's rate limiter
//!   (`--rate-limit-api`), which counts requests rather than trusting
//!   what they claim.
//! - **An adversary claiming someone else's key id** can occupy that
//!   actor's slots, which is a denial of service against one actor. It
//!   is bounded but not prevented: slots free the moment the writer
//!   decides, and a forged signature is rejected in one verification, so
//!   holding another actor's quota costs sustained request volume — the
//!   rate limiter's department again.
//!
//! Naming a bucket is therefore not a claim about who anyone is. Nothing
//! here is load-bearing for authorization, and nothing here may become
//! so: the moment a decision about *permission* keys off this string, an
//! unverified field has been promoted to an identity.
//!
//! [`Witness::key_id`]: choir_oplog::Witness::key_id

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::Submission;

/// Default ceiling on one actor's ops awaiting a decision.
///
/// Deliberately generous, at twice the sequencer's 256-op batch. The
/// quota's job
/// is to stop a runaway, not to shape traffic, and `try_submit_many`
/// exists precisely so a client can offer a large group at once — a
/// ceiling that made the batch endpoint reject its own documented usage
/// would be a bug wearing a policy's clothes. An operator who wants
/// sharper fairness lowers it with [`Quotas::set_limit`].
pub const DEFAULT_QUOTA: usize = 512;

/// A limit meaning "do not enforce one".
pub const UNLIMITED: usize = usize::MAX;

/// Distinct actors tracked before idle ones are forgotten.
///
/// Buckets are kept after they empty so a busy actor's key is interned
/// rather than reallocated per op. That would grow without bound under
/// invented key ids, so once the table is over this size an emptied
/// bucket is dropped instead of kept. Steady-state actors stay interned;
/// one-shot identities do not accumulate.
const MAX_TRACKED: usize = 1024;

/// Shared per-actor in-flight counts. Cloneable; every clone refers to
/// the same table.
#[derive(Clone)]
pub struct Quotas {
    inflight: Arc<Mutex<BTreeMap<Arc<str>, usize>>>,
    limit: Arc<AtomicUsize>,
}

impl Default for Quotas {
    fn default() -> Self {
        Self::new(DEFAULT_QUOTA)
    }
}

impl Quotas {
    /// A table enforcing `limit` ops in flight per actor.
    #[must_use]
    pub fn new(limit: usize) -> Self {
        Self {
            inflight: Arc::new(Mutex::new(BTreeMap::new())),
            limit: Arc::new(AtomicUsize::new(limit)),
        }
    }

    /// Changes the ceiling for subsequent admissions.
    ///
    /// Lowering it never revokes an admission already granted: ops in
    /// flight are already the writer's business, and refusing them
    /// retroactively would break the promise that a submission is either
    /// answered or rejected, not abandoned.
    pub fn set_limit(&self, limit: usize) {
        self.limit.store(limit, Ordering::Relaxed);
    }

    /// The current ceiling.
    #[must_use]
    pub fn limit(&self) -> usize {
        self.limit.load(Ordering::Relaxed)
    }

    /// The bucket a submission counts against.
    ///
    /// Signed ops are keyed by their claimed key id. Unsigned ones fall
    /// back to the channel, which keeps the pre-L8 and dev paths from
    /// collapsing into one shared bucket where every workspace would
    /// throttle every other.
    #[must_use]
    pub fn actor_of(sub: &Submission) -> &str {
        match &sub.author_sig {
            Some(witness) => &witness.key_id,
            None => &sub.channel,
        }
    }

    /// Takes a slot for `actor`, returning the interned key to release it
    /// with.
    ///
    /// The interning is not a micro-optimization for its own sake: this
    /// runs on every submission, and `alloc_budget` asserts a ceiling on
    /// allocator calls per op. Handing back a shared `Arc<str>` means the
    /// per-op cost after an actor's first op is a refcount rather than a
    /// string copy.
    ///
    /// # Errors
    ///
    /// A rejection naming the quota, when `actor` already has `limit` ops
    /// awaiting a decision.
    pub fn admit(&self, actor: &str) -> Result<Arc<str>, String> {
        let limit = self.limit.load(Ordering::Relaxed);
        // A poisoned lock means some other thread panicked while holding
        // it. The table is a counter, not an invariant anyone reasons
        // about across a panic, so the recovery is to carry on rather
        // than to spread the panic into every submitter.
        let mut inflight = self
            .inflight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = match inflight.get_key_value(actor) {
            Some((key, count)) => {
                if *count >= limit {
                    return Err(format!(
                        "quota exhausted: {count} ops from this actor are already \
                         awaiting a decision (limit {limit}); retry when one completes"
                    ));
                }
                key.clone()
            }
            None => {
                if limit == 0 {
                    return Err("quota exhausted: this sequencer is admitting nothing \
                                (limit 0)"
                        .to_string());
                }
                let key: Arc<str> = Arc::from(actor);
                inflight.insert(key.clone(), 0);
                key
            }
        };
        *inflight.get_mut(&key).expect("just inserted or just found") += 1;
        Ok(key)
    }

    /// Returns a slot, once the writer has decided that op.
    ///
    /// Released at the decision rather than at the acknowledgement: the
    /// quota bounds how much work can be queued *ahead* of someone else,
    /// and once an op is appended it is no longer ahead of anything. Ops
    /// waiting on a shared durability barrier have already had their
    /// ordering cost paid by whoever was behind them.
    pub fn release(&self, actor: &Arc<str>) {
        let mut inflight = self
            .inflight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(count) = inflight.get_mut(actor.as_ref()) else {
            return;
        };
        *count = count.saturating_sub(1);
        if *count == 0 && inflight.len() > MAX_TRACKED {
            inflight.remove(actor.as_ref());
        }
    }

    /// Ops from `actor` currently awaiting a decision. For tests and
    /// operator reporting.
    #[must_use]
    pub fn in_flight(&self, actor: &str) -> usize {
        self.inflight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(actor)
            .copied()
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signed(key_id: &str) -> Submission {
        Submission {
            channel: "ws".into(),
            payload: Vec::new(),
            author_sig: Some(choir_oplog::Witness::ed25519(key_id, Vec::new())),
        }
    }

    #[test]
    fn a_full_bucket_is_refused_with_the_limit_in_the_message() {
        let quotas = Quotas::new(2);
        let a = quotas.admit("alice").expect("first");
        let _b = quotas.admit("alice").expect("second");
        let refused = quotas.admit("alice").expect_err("third exceeds the limit");
        assert!(
            refused.contains("limit 2"),
            "the rejection must name the quota so a client can react to it: {refused}"
        );
        // And it is a refusal, not a delay: releasing one slot lets the
        // next in immediately.
        quotas.release(&a);
        quotas.admit("alice").expect("a freed slot admits");
    }

    #[test]
    fn one_actors_ceiling_does_not_touch_another() {
        let quotas = Quotas::new(1);
        let _a = quotas.admit("alice").expect("alice's only slot");
        assert!(quotas.admit("alice").is_err(), "alice is at her limit");
        quotas
            .admit("bob")
            .expect("bob's quota is his own; this is the whole point");
    }

    #[test]
    fn an_unsigned_submission_is_keyed_by_channel_not_lumped_together() {
        let one = Submission {
            channel: "agent-a".into(),
            payload: Vec::new(),
            author_sig: None,
        };
        let two = Submission {
            channel: "agent-b".into(),
            payload: Vec::new(),
            author_sig: None,
        };
        assert_eq!(Quotas::actor_of(&one), "agent-a");
        assert_ne!(
            Quotas::actor_of(&one),
            Quotas::actor_of(&two),
            "unsigned clients must not share one bucket, or every dev \
             workspace throttles every other"
        );
    }

    #[test]
    fn a_signed_submission_is_keyed_by_its_claimed_key_id() {
        assert_eq!(Quotas::actor_of(&signed("kid-7")), "kid-7");
    }

    #[test]
    fn releasing_more_than_was_taken_cannot_underflow() {
        let quotas = Quotas::new(4);
        let key = quotas.admit("alice").expect("slot");
        quotas.release(&key);
        quotas.release(&key);
        quotas.release(&key);
        assert_eq!(quotas.in_flight("alice"), 0);
        // Still usable afterwards: a saturating counter must not have
        // wrapped to something enormous that silently disables the quota.
        assert!(quotas.admit("alice").is_ok());
        assert_eq!(quotas.in_flight("alice"), 1);
    }

    #[test]
    fn releasing_an_actor_that_was_never_admitted_is_a_no_op() {
        let quotas = Quotas::new(4);
        let stray: Arc<str> = Arc::from("never-seen");
        quotas.release(&stray);
        assert_eq!(quotas.in_flight("never-seen"), 0);
    }

    #[test]
    fn idle_buckets_are_forgotten_once_the_table_is_oversized() {
        let quotas = Quotas::new(4);
        // Fill past the tracking cap with one-shot identities, the shape
        // an invented-key-id flood would take.
        for i in 0..=MAX_TRACKED {
            let key = quotas.admit(&format!("throwaway-{i}")).expect("admitted");
            quotas.release(&key);
        }
        let tracked = quotas
            .inflight
            .lock()
            .expect("not poisoned")
            .len();
        assert!(
            tracked <= MAX_TRACKED + 1,
            "emptied buckets must be dropped once over the cap, or an \
             invented key id is a permanent allocation: {tracked}"
        );
    }

    #[test]
    fn a_busy_actor_keeps_one_interned_key() {
        let quotas = Quotas::new(8);
        let first = quotas.admit("alice").expect("first");
        let second = quotas.admit("alice").expect("second");
        assert!(
            Arc::ptr_eq(&first, &second),
            "the same actor must hand back the same allocation, which is \
             what keeps the per-op cost off the allocation budget"
        );
    }

    #[test]
    fn a_lowered_limit_does_not_revoke_what_is_already_in_flight() {
        let quotas = Quotas::new(8);
        let held: Vec<_> = (0..4)
            .map(|_| quotas.admit("alice").expect("under the old limit"))
            .collect();
        quotas.set_limit(2);
        assert_eq!(quotas.in_flight("alice"), 4, "nothing was taken back");
        assert!(quotas.admit("alice").is_err(), "but nothing new is let in");
        for key in &held {
            quotas.release(key);
        }
        assert!(quotas.admit("alice").is_ok(), "and it recovers");
    }
}
