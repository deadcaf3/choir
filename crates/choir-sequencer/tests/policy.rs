//! L1+L8 meet L2: a composed submit policy — signature verification
//! (choir-identity) plus cached-view CAS (choir-view) — running inside
//! the sequencer's writer thread. This is the production admission shape
//! the daemon needs before leaving localhost; it lives here as the
//! seam's conformance proof.

use choir_identity::{ActorKey, Registry};
use choir_oplog::{ContentHash, MemLog, OpEntry};
use choir_sequencer::{Sequencer, SubmitPolicy, Submission};
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

    fn accepted(&mut self, entry: &OpEntry) {
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
