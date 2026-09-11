//! The node's own merge queue (D68).
//!
//! The bridge queues pull requests from somebody else's forge. A node
//! has no pull requests; it has proposals, which are pushes to
//! `refs/for/<branch>/<user>/<topic>` admitted by the `propose` grant
//! (D53, D60). This is the reading that turns those refs into a round,
//! and the landing that writes the round's result back where the log
//! decides.
//!
//! Two things differ from the bridge's composition, and both come from
//! the same fact: here the log *is* the source of truth.
//!
//! - A landing is real. It moves `refs/heads/<branch>`, so it is an
//!   [`OpKind::SetRef`] carrying the value the round started from as
//!   its compare-and-swap `prev`. A push that beat the round makes the
//!   landing fail rather than clobber.
//! - The landing itself moves no git ref. The log leads and git
//!   follows, which is the direction D21 fixed: `/api/queue/run` writes
//!   git's ref after the round, CAS'd on the round's base, and
//!   [`crate::Platform::reconcile_git_refs`] repairs anything that
//!   still lags at the next startup.

use std::sync::Arc;

use choir_hash::ContentHash;
use choir_identity::ActorKey;
use choir_queue::landing::Landing;
use choir_queue::Change;
use choir_sequencer::SequencerHandle;
use choir_view::{OpKind, View, ViewOp};

/// The channel the node's queue authors under.
///
/// Its own, not the proposer's: the ordering of a round is the node's
/// statement, and attributing it to whoever happened to be first in the
/// train would put a landing decision in somebody else's name.
pub const QUEUE_CHANNEL: &str = "node/queue";

/// One proposal in a round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    /// Round-local address, the position of [`Proposal::refname`] in the
    /// round's sorted order. Not an identity: the queue's
    /// cross-round identity is the patch identity
    /// [`choir_queue::speculate::Speculator::identity`] computes, which
    /// survives a rebase and a rename and this does not.
    pub id: u64,
    /// The proposal's full name in the view, `<repo>:refs/for/...`.
    pub refname: String,
    /// The commit the proposal points at.
    pub head: String,
}

/// The proposals aimed at one branch of one repository, plus the base
/// they are aimed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposalRound {
    /// The repository, as the view names it (`<owner>/<name>.git`).
    pub repo: String,
    /// The branch proposals are aimed at, short form (`main`).
    pub branch: String,
    /// The branch's commit when the round was read, and the `prev` every
    /// landing in it is CAS'd against.
    pub base: String,
    /// The round's proposals, in refname order.
    pub proposals: Vec<Proposal>,
}

impl ProposalRound {
    /// Reads the round for `repo` and `branch` out of `view`.
    ///
    /// `None` when the branch does not exist: there is nothing to
    /// propose to, and a queue that invented a base would be deciding
    /// what a repository's history starts from.
    ///
    /// Ordering is by refname, and deliberately not by when the ref was
    /// pushed. The view records no push time, and taking the order from
    /// the op log's sequence numbers would make a round's behaviour
    /// depend on how far the log had been compacted.
    #[must_use]
    pub fn from_view(view: &View, repo: &str, branch: &str) -> Option<Self> {
        let base = view.refs.get(&format!("{repo}:refs/heads/{branch}"))?;
        let base = base.git_oid()?;
        let prefix = format!("{repo}:refs/for/{branch}/");
        let proposals = view
            .refs
            .range(prefix.clone()..)
            .take_while(|(name, _)| name.starts_with(&prefix))
            .filter_map(|(name, head)| Some((name.clone(), head.git_oid()?)))
            .enumerate()
            .map(|(i, (refname, head))| Proposal {
                // One-based, so a report never addresses a change as 0
                // and leaves a reader wondering whether it means "none".
                id: i as u64 + 1,
                refname,
                head,
            })
            .collect();
        Some(Self {
            repo: repo.to_string(),
            branch: branch.to_string(),
            base,
            proposals,
        })
    }

    /// The round as queue input.
    ///
    /// The workspace is the proposal's own refname, so a landing
    /// recorded against a workspace names the thing that was proposed
    /// rather than a number only this round knows.
    #[must_use]
    pub fn changes(&self) -> Vec<Change> {
        self.proposals
            .iter()
            .map(|p| Change {
                id: p.id,
                workspace: p.refname.clone(),
                base: self.base.clone(),
                proposed: p.head.clone(),
                depends: Vec::new(),
            })
            .collect()
    }

    /// The view key of the branch this round lands on.
    #[must_use]
    pub fn target_ref(&self) -> String {
        format!("{}:refs/heads/{}", self.repo, self.branch)
    }
}

/// Where a landing reads the scope it signs: this node's id and a head
/// its window still holds.
///
/// Read per landing rather than once per round, because a round appends
/// as it goes and a node running with `--require-scope` admits only a
/// *recent* head. A scope captured at the start would go stale in
/// exactly the rounds that land the most.
pub type ScopeSource = Box<dyn Fn() -> (ContentHash, Option<ContentHash>) + Send>;

/// Lands by moving the branch the proposals were aimed at (D68).
pub struct RefLanding {
    key: Arc<ActorKey>,
    refname: String,
    scope: ScopeSource,
    at: Option<ContentHash>,
}

impl RefLanding {
    /// Lands on `refname`, CAS'd from `base` and moving forward from
    /// each landing to the next.
    ///
    /// `base` is the value the round was read at. It is the first
    /// landing's `prev`, which is what makes a push that beat the round
    /// win: the op is refused, and the queue stops rather than
    /// overwriting somebody's work with a merge computed against a
    /// branch that has moved.
    #[must_use]
    pub fn new(key: Arc<ActorKey>, refname: String, base: &str, scope: ScopeSource) -> Self {
        Self {
            key,
            refname,
            scope,
            at: ContentHash::from_git_oid(base),
        }
    }
}

impl Landing for RefLanding {
    fn name(&self) -> &'static str {
        "ref"
    }

    fn record(
        &mut self,
        handle: &SequencerHandle,
        _change: &Change,
        commit: ContentHash,
    ) -> Result<(), String> {
        let (node, head) = (self.scope)();
        let payload = ViewOp::new(OpKind::SetRef {
            name: self.refname.clone(),
            commit: commit.clone(),
            prev: self.at.clone(),
        })
        .in_scope(node, head)
        .to_payload();
        let sig = self.key.sign_submission(QUEUE_CHANNEL, &payload);
        handle.try_submit(QUEUE_CHANNEL, payload, Some(sig))?;
        // Only after the sequencer took it. Advancing on a refusal would
        // make the next landing CAS against a value the log never held,
        // turning one lost race into a round that can never land.
        self.at = Some(commit);
        Ok(())
    }
}
