//! L1+L8 meet L2: a composed submit policy — signature verification
//! (choir-identity) plus cached-view CAS (choir-view) — running inside
//! the sequencer's writer thread. This is the production admission shape
//! the daemon needs before leaving localhost; it lives here as the
//! seam's conformance proof.

use choir_identity::{ActorKey, Registry};
use choir_oplog::{ContentHash, MemLog, OpEntry};
use choir_sequencer::journal::Journal as _;
use choir_sequencer::{journal, Sequencer, Submission, SubmitPolicy};
use choir_view::{OpKind, View, ViewOp};

/// Verify author signature, then CAS against a view cached on the
/// writer thread — no re-materialization per submit.
struct ChoirPolicy {
    registry: Registry,
    view: View,
}

impl SubmitPolicy for ChoirPolicy {
    fn check(&mut self, sub: &Submission) -> Result<(), String> {
        let sig = sub.author_sig.as_ref().ok_or("unsigned submission")?;
        self.registry
            .verify_submission(&sub.channel, &sub.payload, sig)
            .map_err(|e| format!("bad signature: {e:?}"))?;
        let op = ViewOp::from_payload(&sub.payload).map_err(|e| format!("bad op: {e:?}"))?;
        // Trial-apply on a clone: check() must not mutate on rejection.
        let mut trial = self.view.clone();
        trial.apply(&op).map_err(|e| format!("stale: {e:?}"))
    }

    fn accepted(&mut self, entry: &OpEntry, _hash: &ContentHash) {
        let op = ViewOp::from_payload(&entry.payload).expect("checked in check()");
        self.view.apply(&op).expect("checked in check()");
    }
}

fn set_head(ws: &str, commit: &ContentHash, prev: Option<&ContentHash>) -> Vec<u8> {
    ViewOp::new(OpKind::SetWorkspaceHead {
        workspace: ws.into(),
        commit: commit.clone(),
        prev: prev.cloned(),
    })
    .to_payload()
}

#[test]
fn signed_cas_admission_end_to_end() {
    let alice = ActorKey::generate();
    let mallory = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&alice.public_key_bytes()).unwrap();

    let sequencer = Sequencer::spawn_with_policy(
        Box::new(MemLog::new()),
        Box::new(ChoirPolicy {
            registry,
            view: View::default(),
        }),
    );
    let handle = sequencer.handle();

    let c1 = ContentHash::blake3(b"commit 1");
    let c2 = ContentHash::blake3(b"commit 2");

    // Signed, fresh CAS: admitted.
    let p1 = set_head("w1", &c1, None);
    let sig1 = alice.sign_submission("w1", &p1);
    assert_eq!(handle.try_submit("w1", p1, Some(sig1)).unwrap().seq, 0);

    // Unsigned: rejected.
    assert!(handle
        .try_submit("w1", set_head("w1", &c2, Some(&c1)), None)
        .unwrap_err()
        .contains("unsigned"));

    // Unknown key: rejected.
    let p_m = set_head("w1", &c2, Some(&c1));
    let sig_m = mallory.sign_submission("w1", &p_m);
    assert!(handle
        .try_submit("w1", p_m, Some(sig_m))
        .unwrap_err()
        .contains("bad signature"));

    // Stale CAS (prev=None but w1 exists): rejected by the cached view.
    let p_stale = set_head("w1", &c2, None);
    let sig_stale = alice.sign_submission("w1", &p_stale);
    assert!(handle
        .try_submit("w1", p_stale, Some(sig_stale))
        .unwrap_err()
        .contains("stale"));

    // Correct CAS: admitted at the next position.
    let p2 = set_head("w1", &c2, Some(&c1));
    let sig2 = alice.sign_submission("w1", &p2);
    assert_eq!(handle.try_submit("w1", p2, Some(sig2)).unwrap().seq, 1);

    // The log holds exactly the admitted ops and replays to the same
    // view the policy tracked incrementally.
    let log = sequencer.shutdown();
    let replayed = View::materialize(log.as_ref()).unwrap();
    assert_eq!(replayed.workspaces.get("w1"), Some(&c2));
    assert_eq!(log.len(), 2);
}

/// The op variant's name, taken from its own externally-tagged
/// serialization rather than a hand-written match: a new variant then
/// journals correctly without anyone remembering to extend a table.
fn op_type_name(op: &ViewOp) -> String {
    serde_json::to_value(&op.kind)
        .ok()
        .and_then(|v| v.as_object().and_then(|o| o.keys().next().cloned()))
        .unwrap_or_else(|| "unknown".to_string())
}

/// The same admission shape, journalled. A policy that already derived
/// the author and op kind while checking hands them to the journal
/// through [`SubmitPolicy::subject`] rather than deriving them twice,
/// and records its own CAS failures — the sequencer cannot, because a
/// refusal reaches it as an opaque string.
struct JournallingPolicy {
    registry: Registry,
    view: View,
    journal: std::sync::Arc<journal::MemJournal>,
    subject: (Option<String>, Option<String>),
}

impl SubmitPolicy for JournallingPolicy {
    fn check(&mut self, sub: &Submission) -> Result<(), String> {
        self.subject = (None, None);
        let sig = sub.author_sig.as_ref().ok_or("unsigned submission")?;
        let actor = self
            .registry
            .verify_submission(&sub.channel, &sub.payload, sig)
            .map_err(|e| format!("bad signature: {e:?}"))?;
        let op = ViewOp::from_payload(&sub.payload).map_err(|e| format!("bad op: {e:?}"))?;
        // Derived once, here, while the signature is already verified.
        self.subject = (Some(actor.to_hex()), Some(op_type_name(&op)));
        let mut trial = self.view.clone();
        trial.apply(&op).map_err(|e| {
            self.journal.record(journal::Event::CasFailure {
                workspace: sub.channel.clone(),
                expected: None,
                actual: None,
            });
            format!("stale: {e:?}")
        })
    }

    fn accepted(&mut self, entry: &OpEntry, _hash: &ContentHash) {
        let op = ViewOp::from_payload(&entry.payload).expect("checked in check()");
        self.view.apply(&op).expect("checked in check()");
    }

    fn subject(&self) -> (Option<String>, Option<String>) {
        self.subject.clone()
    }
}

#[test]
fn the_journal_records_both_an_acceptance_and_a_refusal() {
    let alice = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&alice.public_key_bytes()).unwrap();
    let recorded = std::sync::Arc::new(journal::MemJournal::new());

    let sequencer = Sequencer::spawn_with_journal(
        Box::new(MemLog::new()),
        Box::new(JournallingPolicy {
            registry,
            view: View::default(),
            journal: recorded.clone(),
            subject: (None, None),
        }),
        Box::new(JournalHandle(recorded.clone())),
    );
    let handle = sequencer.handle();

    let c1 = ContentHash::blake3(b"commit 1");
    let c2 = ContentHash::blake3(b"commit 2");

    let p1 = set_head("w1", &c1, None);
    let sig1 = alice.sign_submission("w1", &p1);
    handle.try_submit("w1", p1, Some(sig1)).expect("admitted");

    // Same workspace, prev=None though w1 already exists: the CAS loses.
    let p_stale = set_head("w1", &c2, None);
    let sig_stale = alice.sign_submission("w1", &p_stale);
    handle
        .try_submit("w1", p_stale, Some(sig_stale))
        .expect_err("stale CAS is refused");

    drop(sequencer);
    let lines: Vec<serde_json::Value> = recorded
        .lines()
        .iter()
        .map(|l| serde_json::from_str(l).expect("every line is valid JSON"))
        .collect();

    let decisions: Vec<&serde_json::Value> =
        lines.iter().filter(|v| v["kind"] == "decision").collect();
    assert_eq!(decisions.len(), 2, "one line per decision");

    let accepted = decisions
        .iter()
        .find(|v| v["decision"] == "accepted")
        .expect("the acceptance was journalled");
    assert_eq!(accepted["seq"], 0);
    assert_eq!(accepted["op_type"], "SetWorkspaceHead");
    assert!(
        accepted["actor_id"].as_str().is_some_and(|s| !s.is_empty()),
        "the verified author is carried through `subject`, not re-derived"
    );
    assert!(accepted["reject_reason"].is_null());

    let rejected = decisions
        .iter()
        .find(|v| v["decision"] == "rejected")
        .expect("the refusal was journalled");
    assert!(rejected["reject_reason"]
        .as_str()
        .is_some_and(|s| s.contains("stale")));
    assert!(rejected["seq"].is_null(), "nothing was ordered");

    // The distinct contention fact, not merely a rejection count.
    assert!(
        lines.iter().any(|v| v["kind"] == "cas_failure"),
        "a lost CAS is recorded as contention in its own right"
    );
}

/// Lets one [`journal::MemJournal`] serve both the sequencer and the
/// policy, so a test reads one ordered stream instead of stitching two.
struct JournalHandle(std::sync::Arc<journal::MemJournal>);

impl journal::Journal for JournalHandle {
    fn record(&self, event: journal::Event) {
        self.0.record(event);
    }
}
