//! Production measurement of the merge-decision latency gate.
//!
//! The Phase-0 gate ("p99 decision latency < 100 ms") is asserted in the
//! test suite, on a machine doing nothing else, against a synthetic
//! workload. That is a check that cannot fail in production: the running
//! daemon measured nothing, so a node that had drifted past the gate under
//! real traffic would look exactly like one that had not.
//!
//! [`LagMeter`] closes that. The writer thread records every accepted op
//! at the single point where every accepted op passes, so a new submit
//! path cannot be added and silently go unmeasured. Two latencies, because
//! they answer different questions and PHASE0 measured them as an order of
//! magnitude apart once `sync_data` landed:
//!
//! - `decision`: dequeue -> append. The metric the gate is *stated* in.
//! - `durable`: dequeue -> acknowledgement, which includes the batch's
//!   durability barrier. The number a submitter actually waits out, and
//!   the one an fsync stall shows up in. Gating on `decision` alone would
//!   have been the same non-check in a new place: the dominant cost sits
//!   entirely outside it.
//!
//! A breach is `durable >= gate`, for that reason. `decision` breaches are
//! counted separately so the gate as literally written stays checkable.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The Phase-0 merge-decision latency gate.
pub const DEFAULT_GATE: Duration = Duration::from_millis(100);

/// Most breaches held for the daemon to drain before the oldest are
/// dropped. A breach storm is itself the signal; the count of what was
/// dropped is reported rather than the ring silently eating it.
const MAX_PENDING_BREACHES: usize = 256;

/// Number of power-of-two microsecond buckets. Bucket `i` covers
/// `[2^(i-1), 2^i)` us for `i > 0` and `[0, 1)` for `i = 0`, so bucket 31
/// tops out above 35 minutes: nothing real falls off the end.
const BUCKETS: usize = 32;

/// One op that missed the gate, as recorded for the lag log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Breach {
    /// Position in the total order, so the breach can be tied back to the
    /// op in the log rather than floating free as a timestamp.
    pub seq: u64,
    /// Dequeue -> append, microseconds.
    pub decision_us: u64,
    /// Dequeue -> acknowledgement including the durability barrier,
    /// microseconds.
    pub durable_us: u64,
    /// Ops sharing this op's durability barrier. A large batch explains a
    /// large `durable` without implicating the storage.
    pub batch: usize,
    /// Wall clock at record time, milliseconds since the epoch. Only for
    /// correlating with other logs; `seq` is the identity.
    pub at_unix_ms: u64,
}

/// A power-of-two-bucketed latency histogram in microseconds.
#[derive(Debug, Default, Clone)]
struct Histogram {
    buckets: [u64; BUCKETS],
    count: u64,
    max_us: u64,
}

impl Histogram {
    fn record(&mut self, us: u64) {
        let index = if us == 0 {
            0
        } else {
            // 64 - leading_zeros is the 1-based bit length, i.e. the
            // exponent of the next power of two above `us`.
            usize::try_from(u64::BITS - us.leading_zeros()).unwrap_or(BUCKETS - 1)
        };
        self.buckets[index.min(BUCKETS - 1)] += 1;
        self.count += 1;
        self.max_us = self.max_us.max(us);
    }

    /// Upper bound of the bucket holding the `percentile`th observation.
    ///
    /// Bucketed, so this is an over-estimate bounded by a factor of two,
    /// never an under-estimate. That direction is deliberate: a gate
    /// report that errs toward "you are closer to the limit than this"
    /// cannot quietly pass a node that is actually breaching.
    fn quantile_us(&self, percentile: f64) -> Option<u64> {
        if self.count == 0 {
            return None;
        }
        #[allow(clippy::cast_precision_loss, clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let target = ((self.count as f64) * percentile / 100.0).ceil().max(1.0) as u64;
        let mut seen = 0;
        for (index, hits) in self.buckets.iter().enumerate() {
            seen += hits;
            if seen >= target {
                // Bucket i's exclusive upper bound is 2^i, capped at the
                // observed maximum so a report never claims more lag than
                // was measured. The cap cannot understate: every value in
                // the bucket is <= max_us by definition.
                let bound = 1u64
                    .checked_shl(u32::try_from(index).unwrap_or(0))
                    .unwrap_or(u64::MAX);
                return Some(bound.min(self.max_us));
            }
        }
        Some(self.max_us)
    }
}

#[derive(Debug, Default)]
struct State {
    decision: Histogram,
    durable: Histogram,
    decision_breaches: u64,
    durable_breaches: u64,
    pending: VecDeque<Breach>,
    dropped: u64,
}

/// Shared latency record for one sequencer's writer thread.
///
/// Written by the writer, read and drained by the daemon. Deliberately a
/// plain observation surface with no policy of its own: what a node *does*
/// about a breach (write a file, page someone, refuse traffic) is an
/// operator decision, the same reasoning that keeps
/// `SequencerHandle::durability_failed` an observation rather than an
/// action.
#[derive(Debug)]
pub struct LagMeter {
    state: Mutex<State>,
    gate: Mutex<Duration>,
}

impl Default for LagMeter {
    fn default() -> Self {
        Self::new()
    }
}

impl LagMeter {
    /// A meter gated at [`DEFAULT_GATE`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State::default()),
            gate: Mutex::new(DEFAULT_GATE),
        }
    }

    /// The latency at or above which an op is recorded as a breach.
    #[must_use]
    pub fn gate(&self) -> Duration {
        *self.gate.lock().expect("gate lock")
    }

    /// Moves the gate. Live rather than construction-time so a gate can be
    /// tightened against a running node, and so a test can prove the check
    /// is capable of failing.
    pub fn set_gate(&self, gate: Duration) {
        *self.gate.lock().expect("gate lock") = gate;
    }

    /// Records one accepted op.
    pub fn record(&self, seq: u64, decision: Duration, durable: Duration, batch: usize) {
        let gate = self.gate();
        let decision_us = u64::try_from(decision.as_micros()).unwrap_or(u64::MAX);
        let durable_us = u64::try_from(durable.as_micros()).unwrap_or(u64::MAX);
        let mut state = self.state.lock().expect("lag state lock");
        state.decision.record(decision_us);
        state.durable.record(durable_us);
        if decision >= gate {
            state.decision_breaches += 1;
        }
        if durable >= gate {
            state.durable_breaches += 1;
            if state.pending.len() == MAX_PENDING_BREACHES {
                state.pending.pop_front();
                state.dropped += 1;
            }
            state.pending.push_back(Breach {
                seq,
                decision_us,
                durable_us,
                batch,
                at_unix_ms: u64::try_from(
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis(),
                )
                .unwrap_or(u64::MAX),
            });
        }
    }

    /// Takes the breaches recorded since the last drain, and how many were
    /// dropped from the ring before this drain could reach them.
    pub fn drain(&self) -> (Vec<Breach>, u64) {
        let mut state = self.state.lock().expect("lag state lock");
        let dropped = std::mem::take(&mut state.dropped);
        (state.pending.drain(..).collect(), dropped)
    }

    /// A point-in-time summary since process start.
    #[must_use]
    pub fn report(&self) -> LagReport {
        let state = self.state.lock().expect("lag state lock");
        LagReport {
            gate_us: u64::try_from(self.gate().as_micros()).unwrap_or(u64::MAX),
            observed_ops: state.durable.count,
            decision_p50_us: state.decision.quantile_us(50.0),
            decision_p99_us: state.decision.quantile_us(99.0),
            decision_max_us: state.decision.max_us,
            decision_breaches: state.decision_breaches,
            durable_p50_us: state.durable.quantile_us(50.0),
            durable_p99_us: state.durable.quantile_us(99.0),
            durable_max_us: state.durable.max_us,
            durable_breaches: state.durable_breaches,
            pending_breaches: state.pending.len(),
        }
    }
}

/// What the meter has seen since the process started.
///
/// Percentiles are bucket upper bounds capped at the observed maximum, so
/// they over-estimate by at most a factor of two and never under-estimate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LagReport {
    /// The gate these counts are measured against, microseconds.
    pub gate_us: u64,
    /// Accepted ops measured. Rejections are not ops and are not counted.
    pub observed_ops: u64,
    /// Median dequeue -> append: the gate as literally written.
    pub decision_p50_us: Option<u64>,
    /// 99th percentile dequeue -> append.
    pub decision_p99_us: Option<u64>,
    /// Slowest dequeue -> append observed.
    pub decision_max_us: u64,
    /// Accepted ops whose append alone reached the gate.
    pub decision_breaches: u64,
    /// Median dequeue -> acknowledgement, durability barrier included:
    /// what a submitter waits out.
    pub durable_p50_us: Option<u64>,
    /// 99th percentile dequeue -> acknowledgement.
    pub durable_p99_us: Option<u64>,
    /// Slowest dequeue -> acknowledgement observed.
    pub durable_max_us: u64,
    /// Accepted ops that reached the gate through the barrier. This is
    /// the breach count the lag log records.
    pub durable_breaches: u64,
    /// Breaches recorded but not yet drained to the operator's lag log.
    pub pending_breaches: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantiles_never_understate_and_stay_within_a_factor_of_two() {
        let meter = LagMeter::new();
        // 95 fast ops and 5 slow ones. Nearest-rank p99 over 100 samples
        // is the 99th ordered value, so it must land on a slow one: a tail
        // that thick reading clean is the failure mode this guards.
        for seq in 0..95 {
            meter.record(seq, Duration::from_micros(50), Duration::from_micros(50), 1);
        }
        for seq in 95..100 {
            meter.record(seq, Duration::from_millis(400), Duration::from_millis(400), 1);
        }
        let report = meter.report();
        assert_eq!(report.observed_ops, 100);
        let p99 = report.durable_p99_us.expect("observations recorded");
        assert!(p99 >= 400_000, "p99 must not understate the slow op: {p99}");
        assert!(p99 <= 800_000, "bucketing may over-estimate at most 2x: {p99}");
        let p50 = report.durable_p50_us.expect("observations recorded");
        assert!((50..=100).contains(&p50), "p50 sits on the fast ops: {p50}");
        assert_eq!(report.durable_max_us, 400_000);
    }

    #[test]
    fn a_breach_is_recorded_and_drained_once() {
        let meter = LagMeter::new();
        meter.record(7, Duration::from_millis(1), Duration::from_millis(250), 4);
        meter.record(8, Duration::from_millis(1), Duration::from_millis(1), 4);
        let report = meter.report();
        assert_eq!(report.durable_breaches, 1);
        // The op was fast to append and slow to make durable. Gating on
        // the append alone is exactly the blind spot this exists for.
        assert_eq!(report.decision_breaches, 0);

        let (breaches, dropped) = meter.drain();
        assert_eq!(dropped, 0);
        assert_eq!(breaches.len(), 1);
        assert_eq!(breaches[0].seq, 7);
        assert_eq!(breaches[0].batch, 4);
        assert!(breaches[0].durable_us >= 250_000);
        // Drained means handed over, not forgotten: the totals survive so
        // a summary is not reset by whoever reads the log.
        assert_eq!(meter.report().durable_breaches, 1);
        assert!(meter.drain().0.is_empty(), "a breach is handed over once");
    }

    #[test]
    fn overflowing_the_ring_reports_what_it_dropped() {
        let meter = LagMeter::new();
        for seq in 0..(MAX_PENDING_BREACHES as u64 + 5) {
            meter.record(seq, Duration::ZERO, Duration::from_millis(200), 1);
        }
        let (breaches, dropped) = meter.drain();
        assert_eq!(breaches.len(), MAX_PENDING_BREACHES);
        assert_eq!(dropped, 5, "silently dropping breaches would be the bug");
        // The oldest went, not the newest: a storm's tail is the part an
        // operator still has other evidence for.
        assert_eq!(breaches[0].seq, 5);
    }

    #[test]
    fn an_empty_meter_reports_no_percentiles_rather_than_zero() {
        let report = LagMeter::new().report();
        assert_eq!(report.observed_ops, 0);
        // Zero would read as "well inside the gate" on a node that has
        // measured nothing at all.
        assert_eq!(report.durable_p99_us, None);
        assert_eq!(report.decision_p99_us, None);
        assert_eq!(report.gate_us, 100_000);
    }
}
