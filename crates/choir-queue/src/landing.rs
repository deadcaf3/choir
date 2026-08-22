//! How a landing is written into the log (D68).
//!
//! The queue decides *what* landed and in *what order*; it does not
//! know what a landing means to whoever is running it. For a forge
//! bridge it means nothing outside the round -- upstream is canonical
//! (D21) and the ordering is the queue's own bookkeeping. For a node
//! it means the branch moved, which is the whole point, and the op that
//! says so has to be one the daemon's policy will accept.
//!
//! Hence a seam rather than a hardcoded op, for the same reason
//! [`crate::speculate::Speculator`] is one: the two callers disagree
//! about the answer, and the disagreement is not a parameter.
//!
//! **A refusal is a real outcome, not an error path.** The unsigned
//! [`choir_sequencer::SequencerHandle::submit`] panics when a policy
//! rejects, so a queue that landed by calling it would take the process
//! down the first time it met a daemon that demands signatures. Every
//! implementation here answers with a `Result` and the queue stops the
//! round on `Err`, because a landing the log refused means the base the
//! rest of the train was speculating on is not the base the log has.

use choir_hash::ContentHash;
use choir_sequencer::SequencerHandle;

use crate::Change;

/// How the queue records that a change landed.
pub trait Landing: Send {
    /// The implementation's name, for reports and journals.
    fn name(&self) -> &'static str;

    /// Records that `change` landed, with the merged state named by
    /// `commit`.
    ///
    /// Called once per landing, in landing order, so an implementation
    /// that needs the previous value for a compare-and-swap can keep it
    /// between calls.
    ///
    /// # Errors
    ///
    /// The sequencer refused the op: an unsigned op under a policy that
    /// demands a signature, a failed CAS because something else moved
    /// the target first, or a quota. Every one of them voids the rest
    /// of the round rather than the one change, because the changes
    /// behind it were merged onto a state the log did not accept.
    fn record(
        &mut self,
        handle: &SequencerHandle,
        change: &Change,
        commit: ContentHash,
    ) -> Result<(), String>;
}

/// The in-memory landing: the change's workspace now holds this state.
///
/// The default, and what a caller whose repository is somebody else's
/// wants. `prev` is `None` because a change lands at most once -- the
/// already-landed identity check refuses a resubmission, so the head
/// this names does not exist yet -- and because the ordering it records
/// is the queue's own, which nothing outside the round reads back.
#[derive(Debug, Default, Clone, Copy)]
pub struct WorkspaceLanding;

impl Landing for WorkspaceLanding {
    fn name(&self) -> &'static str {
        "workspace"
    }

    fn record(
        &mut self,
        handle: &SequencerHandle,
        change: &Change,
        commit: ContentHash,
    ) -> Result<(), String> {
        let op = choir_view::ViewOp::new(choir_view::OpKind::SetWorkspaceHead {
            workspace: change.workspace.clone(),
            commit,
            prev: None,
        });
        let payload = serde_json::to_vec(&op).expect("a ViewOp serializes");
        handle
            .try_submit(&change.workspace, payload, None)
            .map(|_| ())
    }
}
