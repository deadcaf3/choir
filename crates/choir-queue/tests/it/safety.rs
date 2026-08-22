//! Train enforcement of the merge-safety invariant:
//! a strategy that resolves by discarding work already in the speculative
//! state is evicted first-class, before CI, without blocking the train.

use choir_merge::{MergeOutcome, MergeStrategy, Pipeline};
use choir_oplog::MemLog;
use choir_queue::executor::Synthetic;
use choir_queue::{Change, MergeQueue, Rejection};
use choir_sequencer::Sequencer;

/// A confidently wrong strategy: "resolves" every merge by taking the
/// proposal wholesale — the failure mode of a non-deterministic resolver
/// slot (D19) that the safety check exists to make admissible.
struct TakeProposal;

impl MergeStrategy for TakeProposal {
    fn name(&self) -> &'static str {
        "take-proposal"
    }
    fn merge(&self, _base: &str, _left: &str, right: &str) -> MergeOutcome {
        MergeOutcome::Resolved(right.to_string())
    }
}

fn base() -> String {
    (0..160).map(|i| format!("line {i}\n")).collect()
}

fn disjoint_change(id: u64) -> Change {
    let mut lines: Vec<String> = base().lines().map(String::from).collect();
    lines[(3 * id) as usize] = format!("line {} edited by change {id}", 3 * id);
    Change {
        id,
        workspace: format!("ws-{id}"),
        base: base(),
        proposed: lines.join("\n") + "\n",
        depends: vec![],
    }
}

#[test]
fn reverting_resolution_is_evicted_without_blocking_the_train() {
    // Change 0 lands first: the target still equals its base, so taking the
    // proposal wholesale is coincidentally honest. Change 1's proposal,
    // authored against the original base, then reverts change 0's edit when
    // taken wholesale — the check must evict it and name both the reverted
    // line and the resurrected original.
    let mut queue = MergeQueue::with_pipeline(&base(), Pipeline::new(vec![Box::new(TakeProposal)]));
    for change in (0..2).map(disjoint_change) {
        queue.submit(change);
    }
    let sequencer = Sequencer::spawn(Box::new(MemLog::new()));
    let report = queue.drain(&mut Synthetic::passing(), &sequencer);
    let ops = sequencer.shutdown().len();

    assert_eq!(report.merged, vec![0]);
    assert_eq!(ops, 1, "only the honest resolution reached the sequencer");
    assert_eq!(report.rejected.len(), 1);
    let (id, rejection) = &report.rejected[0];
    assert_eq!(*id, 1);
    match rejection {
        Rejection::SafetyViolation {
            strategy,
            violation,
        } => {
            assert_eq!(*strategy, "take-proposal");
            assert_eq!(
                violation.reverted,
                vec!["line 0 edited by change 0".to_string()]
            );
            assert_eq!(violation.injected, vec!["line 0".to_string()]);
        }
        other => panic!("expected a safety violation, got {other:?}"),
    }
    // The reverted work survives on the speculative tip.
    assert!(report.final_state.contains("edited by change 0"));
}

#[test]
fn honest_resolutions_pass_the_safety_gate() {
    // The default pipeline's resolutions must never trip the check: same
    // disjoint-edit train as the queue tests, now asserting no rejections
    // of any kind while the gate is active on every resolution.
    let (report, ops) = choir_queue::run_batch(
        &base(),
        (0..15).map(disjoint_change).collect(),
        &mut Synthetic::passing(),
    );
    assert!(
        report.rejected.is_empty(),
        "false positive: {:?}",
        report.rejected
    );
    assert_eq!(report.merged.len(), 15);
    assert_eq!(ops, 15);
}
