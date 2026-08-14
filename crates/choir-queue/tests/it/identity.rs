//! Stable change identity across rebase (Pijul item 4): the identity is
//! the hash of the position-independent diff, so a change rewritten onto
//! a new base keeps it — and the queue refuses the resubmission as
//! already landed instead of re-merging or duplicating it.

use choir_queue::identity::change_identity;
use choir_queue::{run_batch, Change, MergeQueue, Rejection};
use choir_oplog::MemLog;
use choir_sequencer::Sequencer;

fn base() -> String {
    (0..160).map(|i| format!("line {i}\n")).collect()
}

/// A change replacing the content line `line {3 * id}` wherever it sits
/// in `authored_on`. Addressing by content rather than by index is what
/// makes "the same edit, rebased" expressible: after the tip moves, the
/// line is at a different offset but the edit is the same one.
fn change_on(id: u64, authored_on: &str) -> Change {
    let target = format!("line {}", 3 * id);
    let proposed: Vec<String> = authored_on
        .lines()
        .map(|line| {
            if line == target {
                format!("line {} edited by change {id}", 3 * id)
            } else {
                line.to_string()
            }
        })
        .collect();
    // The guard must prove the *replacement happened*, not that the
    // result merely contains the phrase: a base that already holds this
    // edit would otherwise produce a silent no-op change.
    assert!(
        authored_on.lines().any(|line| line == target),
        "the target line must be present and unedited in the base"
    );
    Change {
        id,
        workspace: format!("ws-{id}"),
        base: authored_on.to_string(),
        proposed: proposed.join("\n") + "\n",
        depends: vec![],
    }
}

/// The identity is position-independent: the same logical edit authored
/// against a base the train has since advanced keeps its identity, while
/// a different edit does not.
#[test]
fn identity_survives_a_rebase_and_separates_different_edits() {
    let original = change_on(2, &base());

    // The train lands something else first, so the change is rebased:
    // same edit, new base, shifted offsets. The other edit lands
    // *adjacent* to this one on purpose — a rebase over a distant change
    // leaves the surrounding lines untouched, so it would not detect an
    // identity that quietly depended on its context.
    let mut advanced: Vec<String> = base().lines().map(String::from).collect();
    advanced[5] = "line 5 edited by someone else".to_string();
    advanced.insert(0, "a brand new first line".to_string());
    let advanced = advanced.join("\n") + "\n";
    let rebased = change_on(2, &advanced);

    assert_ne!(rebased.base, original.base, "the rebase moved the base");
    assert_ne!(
        rebased.proposed, original.proposed,
        "and moved the proposed content with it"
    );
    assert_eq!(
        change_identity(&rebased),
        change_identity(&original),
        "the same edit keeps its identity across the rewrite"
    );
    assert_ne!(
        change_identity(&change_on(3, &base())),
        change_identity(&original),
        "a different edit is a different change"
    );
}

/// Item D's pass/fail in the queue: the train lands a change, the author
/// resubmits it rebased onto the rewritten tip, and the queue detects it
/// as already landed rather than merging it a second time.
#[test]
fn a_rewritten_resubmission_is_detected_as_already_landed() {
    let sequencer = Sequencer::spawn(Box::new(MemLog::new()));
    let mut queue = MergeQueue::new(&base());

    // Someone else's change lands first, moving the tip.
    queue.submit(change_on(3, &base()));
    let with_other = queue
        .drain(&mut |_: &Change, _: &str| true, &sequencer)
        .final_state;

    // Then this change lands, authored against the original base.
    queue.submit(change_on(2, &base()));
    let first = queue.drain(&mut |_: &Change, _: &str| true, &sequencer);
    assert_eq!(first.merged, vec![2]);
    let landed_state = first.final_state;

    // The author's branch is rebased onto the tip as it stood without
    // their change — the shape a train rewrite leaves behind — and the
    // same edit is resubmitted. Different base, different content, same
    // logical change; nothing but the identity relates the two.
    let mut resubmission = change_on(2, &with_other);
    resubmission.id = 7;
    resubmission.workspace = "ws-rebased".into();
    assert_ne!(resubmission.base, base(), "the resubmission is rebased");
    queue.submit(resubmission);
    let second = queue.drain(&mut |_: &Change, _: &str| true, &sequencer);
    sequencer.shutdown();

    assert_eq!(second.merged, Vec::<u64>::new(), "it must not land twice");
    assert_eq!(second.rejected, vec![(7, Rejection::AlreadyLanded)]);
    assert_eq!(second.merge_invocations, 0, "and must not be re-merged");
    assert_eq!(second.final_state, landed_state, "the tip must not move");
}

/// A queue seeded with an identity from an earlier instance refuses that
/// change without having landed it itself.
#[test]
fn a_seeded_identity_is_refused_on_arrival() {
    let known = change_on(4, &base());
    let mut queue = MergeQueue::new(&base());
    queue.mark_landed(change_identity(&known));
    queue.submit(known);
    let sequencer = Sequencer::spawn(Box::new(MemLog::new()));
    let report = queue.drain(&mut |_: &Change, _: &str| true, &sequencer);
    sequencer.shutdown();
    assert_eq!(report.rejected, vec![(4, Rejection::AlreadyLanded)]);
    assert_eq!(report.merge_invocations, 0);
}

/// A genuinely new change is unaffected: identity refusal must not eat
/// ordinary traffic.
#[test]
fn an_unseen_change_still_lands() {
    let (report, ops) = run_batch(
        &base(),
        vec![change_on(5, &base())],
        &mut |_: &Change, _: &str| true,
    );
    assert_eq!(report.merged, vec![5]);
    assert_eq!(ops, 1);
}
