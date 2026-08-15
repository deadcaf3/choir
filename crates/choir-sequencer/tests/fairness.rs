//! Per-actor fairness at intake, against a running sequencer.
//!
//! The unit tests in `fairness.rs` and `lib.rs` cover the counter and the
//! scheduler in isolation. What is left to show here is the property
//! those two exist for, stated the way an operator would state it: **one
//! actor's burst cannot make another actor wait indefinitely**, and the
//! bound is a number you can point at rather than a hope about timing.
//!
//! Deliberately not timing tests. "B was served within N milliseconds"
//! would be a wall-clock assertion on a laptop under a parallel test
//! suite, which is a flake with a plan. The bound asserted instead is
//! structural: how many of A's ops can be *awaiting a decision* when B
//! arrives. That is the quantity the quota fixes, and it holds whatever
//! the machine is doing.
//!
//! Its own binary, like every other `choir-sequencer` test: these spawn
//! writer threads and count what one of them did.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};

use choir_oplog::{MemLog, Witness};
use choir_sequencer::fairness::{Quotas, DEFAULT_QUOTA, UNLIMITED};
use choir_sequencer::{SequencerHandle, Submission, SubmitPolicy};

/// A submission claiming `key_id`, which is what the quota buckets on.
/// Unsigned here in the cryptographic sense -- the signature is never
/// checked by these policies, and the point of the test is precisely
/// that the *scheduler* never checks it either.
fn from_actor(key_id: &str, payload: &[u8]) -> Submission {
    Submission {
        channel: "ws".to_string(),
        payload: payload.to_vec(),
        author_sig: Some(Witness {
            key_id: key_id.to_string(),
            signature: Vec::new(),
            scheme: None,
            authenticator_data: None,
            client_data_json: None,
        }),
    }
}

/// Blocks the writer inside its first `check` until released, so a test
/// can pile submissions into the queue and know they are all waiting.
struct HoldFirst {
    gate: Arc<Barrier>,
    seen: AtomicUsize,
}

impl SubmitPolicy for HoldFirst {
    fn check(&mut self, _sub: &Submission) -> Result<(), String> {
        if self.seen.fetch_add(1, Ordering::SeqCst) == 0 {
            // Two parties: this writer, and the test that releases it.
            self.gate.wait();
        }
        Ok(())
    }
}

/// An op arriving from a quiet actor waits behind at most `limit` ops
/// from a flooding one, no matter how hard the flooder pushes.
///
/// This is the whole phase in one assertion. Without a quota the flood
/// is unbounded and the answer to "how long does B wait" is "as long as
/// A likes", which is not an answer.
#[test]
fn a_flood_is_bounded_by_the_quota_before_it_ever_reaches_the_writer() {
    let limit = 4;
    let quotas = Quotas::new(limit);

    // A pushes far past its ceiling. Nothing is running to drain it, so
    // every admission that succeeds is genuinely in flight.
    let mut admitted = 0;
    let mut refusals = Vec::new();
    for _ in 0..50 {
        match quotas.admit("actor-a") {
            Ok(_slot) => admitted += 1,
            Err(reason) => refusals.push(reason),
        }
    }

    assert_eq!(
        admitted, limit,
        "the flood is capped at the quota, not at what it asked for"
    );
    assert_eq!(refusals.len(), 46, "every op past the cap was answered");
    assert_eq!(
        quotas.in_flight("actor-a"),
        limit,
        "and the ceiling is what B could find queued ahead of it"
    );

    // B is unaffected: its own bucket is empty regardless.
    quotas
        .admit("actor-b")
        .expect("a quiet actor is admitted while another floods");
}

/// Over quota is a *rejection*, delivered to the caller, naming the
/// limit. Not a block, not a sleep, not a silent drop.
///
/// The distinction is the whole reason the check sits on the calling
/// thread instead of in the writer: a submitter that is parked has no
/// way to back off, retry elsewhere, or tell a human. One that is
/// refused with a number has all three.
#[test]
fn an_over_quota_submission_is_answered_rather_than_parked() {
    let sequencer = choir_sequencer::Sequencer::spawn(Box::new(MemLog::new()));
    let handle = sequencer.handle();
    let quotas = handle.quotas();

    // The ordinary case first: a real ceiling, filled, then exceeded.
    // Held slots rather than submissions, because a submitted op would
    // be decided and hand its slot straight back -- the writer here is
    // running, and the queue this bounds is the one in front of it.
    quotas.set_limit(3);
    let held: Vec<_> = (0..3)
        .map(|i| {
            quotas
                .admit("busy")
                .unwrap_or_else(|e| panic!("slot {i} refused under the ceiling: {e}"))
        })
        .collect();
    let refused = quotas.admit("busy").expect_err("the fourth exceeds three");
    assert!(
        refused.contains("quota"),
        "the reason names the mechanism: {refused}"
    );
    assert!(
        refused.contains("limit 3"),
        "and the ceiling, so a client can act on it: {refused}"
    );
    assert!(
        refused.contains('3'),
        "and how many are already in flight: {refused}"
    );
    drop(held);

    // And the degenerate ceiling, which takes a different branch: an
    // actor with no bucket at all, refused before one is made.
    quotas.set_limit(0);
    let refused = handle
        .try_submit("ws", b"op".to_vec(), None)
        .expect_err("nothing is admitted at limit 0");
    assert!(
        refused.contains("limit 0"),
        "the never-seen actor is refused by the quota too: {refused}"
    );

    // Raising the limit makes the same submission work, which proves the
    // refusal was the quota and not something incidental.
    quotas.set_limit(DEFAULT_QUOTA);
    let accepted = handle
        .try_submit("ws", b"op".to_vec(), None)
        .expect("admitted once the ceiling allows it");
    assert_eq!(accepted.seq, 0);
    sequencer.shutdown();
}

/// A slot is returned once the writer decides, so a submitter at its
/// ceiling recovers by itself rather than staying locked out.
#[test]
fn slots_return_as_the_writer_decides() {
    let sequencer = choir_sequencer::Sequencer::spawn(Box::new(MemLog::new()));
    let handle = sequencer.handle();
    handle.quotas().set_limit(1);

    // Serially, at a limit of one: each op must have freed its slot
    // before the next is admitted, or this deadlocks into a rejection.
    for expected in 0..5u64 {
        let accepted = handle
            .try_submit("ws", b"op".to_vec(), None)
            .unwrap_or_else(|e| panic!("op {expected} refused: {e}"));
        assert_eq!(accepted.seq, expected);
    }
    assert_eq!(
        handle.quotas().in_flight("ws"),
        0,
        "every slot was returned; a leak here locks the actor out for good"
    );
    let log = sequencer.shutdown();
    assert_eq!(log.len(), 5);
}

/// Fairness reorders *between* actors and never within one, and the log
/// it produces is still one contiguous total order.
///
/// The single-writer invariant is the thing most at risk from a change
/// that reorders the writer's input, so it is asserted directly: every
/// seq from 0 to n-1, exactly once, each entry's parent being the
/// previous entry's hash.
#[test]
fn the_total_order_survives_reordering() {
    let actors = 4;
    let per_actor = 25;
    let gate = Arc::new(Barrier::new(2));
    let sequencer = choir_sequencer::Sequencer::spawn_with_policy(
        Box::new(MemLog::new()),
        Box::new(HoldFirst {
            gate: gate.clone(),
            seen: AtomicUsize::new(0),
        }),
    );
    let handle = sequencer.handle();
    handle.quotas().set_limit(UNLIMITED);

    // Every actor's ops carry its own name and index, so the log can be
    // checked for per-actor order after the scheduler has interleaved
    // them.
    let seen: Arc<Mutex<Vec<(String, usize, u64)>>> = Arc::new(Mutex::new(Vec::new()));
    let threads: Vec<_> = (0..actors)
        .map(|a| {
            let handle: SequencerHandle = handle.clone();
            let seen = seen.clone();
            std::thread::spawn(move || {
                let name = format!("actor-{a}");
                let subs: Vec<Submission> = (0..per_actor)
                    .map(|i| from_actor(&name, format!("{name}:{i}").as_bytes()))
                    .collect();
                for (i, result) in handle.try_submit_many(subs).into_iter().enumerate() {
                    let accepted = result.expect("admitted");
                    seen.lock().expect("results").push((name.clone(), i, accepted.seq));
                }
            })
        })
        .collect();

    // Let the writer past the op it is holding, now that the rest are
    // queued behind it and will be drained as one batch.
    gate.wait();
    for thread in threads {
        thread.join().expect("submitter finished");
    }

    let log = sequencer.shutdown();
    let total = actors * per_actor;
    assert_eq!(log.len() as usize, total);

    // One writer, one chain: seqs contiguous, parents linked.
    let mut previous: Option<choir_oplog::ContentHash> = None;
    for position in 0..log.len() {
        let entry = log
            .get(position)
            .unwrap_or_else(|| panic!("seq {position} missing: the total order has a gap"));
        assert_eq!(
            entry.seq, position,
            "entry at position {position} carries seq {}",
            entry.seq
        );
        assert_eq!(
            entry.parent, previous,
            "entry {position} does not chain to its predecessor"
        );
        previous = Some(entry.content_hash());
    }

    // Per-actor order preserved: op i of an actor landed before op i+1.
    let results = seen.lock().expect("results");
    for a in 0..actors {
        let name = format!("actor-{a}");
        let mut mine: Vec<(usize, u64)> = results
            .iter()
            .filter(|(who, _, _)| *who == name)
            .map(|(_, i, seq)| (*i, *seq))
            .collect();
        mine.sort_unstable();
        assert_eq!(mine.len(), per_actor, "{name} lost ops");
        for pair in mine.windows(2) {
            assert!(
                pair[0].1 < pair[1].1,
                "{name} op {} landed at seq {} but op {} landed at {}: an \
                 actor's own ops must never overtake each other",
                pair[0].0,
                pair[0].1,
                pair[1].0,
                pair[1].1
            );
        }
    }
}

/// The default ceiling does not refuse the batch endpoint's own
/// documented usage.
///
/// `try_submit_many` exists so a client can offer a large group at once,
/// and it is the pattern the node uses. A default quota below that would
/// have made the two features contradict each other -- the kind of thing
/// that ships green and fails on the first real batch.
#[test]
fn the_default_quota_admits_a_full_batch_from_one_actor() {
    let sequencer = choir_sequencer::Sequencer::spawn(Box::new(MemLog::new()));
    let handle = sequencer.handle();

    let subs: Vec<Submission> = (0..256)
        .map(|i| from_actor("busy", format!("op-{i}").as_bytes()))
        .collect();
    let results = handle.try_submit_many(subs);

    let refused: Vec<&String> = results.iter().filter_map(|r| r.as_ref().err()).collect();
    assert!(
        refused.is_empty(),
        "the default quota must not reject a documented batch: {refused:?}"
    );
    // Compile-time, on clippy's suggestion, and better for it: lowering
    // the constant below one batch now fails to build rather than
    // failing a test somebody might be tempted to adjust.
    const {
        assert!(
            DEFAULT_QUOTA >= 256,
            "the default quota must admit a full batch from one actor"
        );
    }
    sequencer.shutdown();
}
