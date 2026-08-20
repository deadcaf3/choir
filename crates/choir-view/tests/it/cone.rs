//! Declared path cones, and the conflict report they narrow (D50).
//!
//! Two claims are under test. The ordinary one: a cone is carried in the
//! signed authorization, folded onto the change, and reproduced by
//! replay. The unusual one: a conflict *outside* the cone is still
//! reported, as a path and nothing else — which is the capability the
//! separation of log from content buys, and the thing a partial clone
//! alone cannot do.

use choir_hash::ContentHash;
use choir_view::{
    append_op, cone_covers, conflicts_for_cone, Commit, CreateAuthorization, OpKind, TreeEntry,
    View, ViewOp,
};
use std::collections::BTreeMap;

fn blob(seed: &[u8]) -> ContentHash {
    ContentHash::blake3(seed)
}

/// A commit conflicted in two places: one inside a `services/api` cone
/// and one outside it.
fn conflicted_commit() -> Commit {
    let mut tree = BTreeMap::new();
    tree.insert(
        "services/api/main.rs".to_string(),
        TreeEntry::Conflict {
            base: Some(blob(b"api base")),
            left: blob(b"api left"),
            right: blob(b"api right"),
        },
    );
    tree.insert(
        "docs/guide.md".to_string(),
        TreeEntry::Conflict {
            base: None,
            left: blob(b"docs left"),
            right: blob(b"docs right"),
        },
    );
    tree.insert(
        "services/api/clean.rs".to_string(),
        TreeEntry::File { blob: blob(b"ok") },
    );
    Commit {
        format_version: choir_view::FORMAT_VERSION,
        parents: Vec::new(),
        tree,
        author: "ana".into(),
        message: "a merge that collided twice".into(),
        resolves: None,
    }
}

/// The headline property: the out-of-cone conflict is named, and its
/// content addresses are not.
#[test]
fn an_out_of_cone_conflict_is_named_but_not_readable() {
    let commit = conflicted_commit();
    let report = conflicts_for_cone(&commit, &["services/api".to_string()]);

    assert_eq!(report.inside, vec!["services/api/main.rs".to_string()]);
    assert_eq!(
        report.outside,
        vec!["docs/guide.md".to_string()],
        "the reader was not told they collided outside their cone"
    );

    // The type carries paths only, so there is no field that could leak
    // a side. Asserting it anyway, against the rendered form, because
    // the claim being made to a user is about bytes on a wire.
    let rendered = format!("{:?}", report.outside);
    for side in [blob(b"docs left"), blob(b"docs right")] {
        assert!(
            !rendered.contains(&side.to_hex()),
            "an out-of-cone content address reached the report: {rendered}"
        );
    }
}

/// An empty cone is the whole tree, which is what every change written
/// before cones existed decodes to. Nothing may be withheld from it.
#[test]
fn an_empty_cone_covers_everything() {
    let commit = conflicted_commit();
    let report = conflicts_for_cone(&commit, &[]);
    assert!(report.outside.is_empty(), "{:?}", report.outside);
    assert_eq!(report.inside.len(), 2);
    assert!(cone_covers(&[], "anything/at/all"));
}

/// Prefix matching is on a directory boundary. `services/api` must not
/// swallow `services/apiary`, which a bare `starts_with` would.
#[test]
fn a_cone_matches_on_a_directory_boundary() {
    let cone = vec!["services/api".to_string()];
    assert!(cone_covers(&cone, "services/api"));
    assert!(cone_covers(&cone, "services/api/main.rs"));
    assert!(cone_covers(&cone, "services/api/deep/nested.rs"));
    assert!(!cone_covers(&cone, "services/apiary/main.rs"));
    assert!(!cone_covers(&cone, "services/apix"));
    assert!(!cone_covers(&cone, "docs/guide.md"));

    // A trailing slash is the same cone, since that is how a person
    // types a directory half the time.
    assert!(cone_covers(
        &["services/api/".to_string()],
        "services/api/x"
    ));
}

/// A commit with no conflicts reports nothing in either bucket.
#[test]
fn a_clean_commit_reports_no_conflicts() {
    let mut commit = conflicted_commit();
    commit
        .tree
        .retain(|_, e| matches!(e, TreeEntry::File { .. }));
    let report = conflicts_for_cone(&commit, &["services/api".to_string()]);
    assert!(report.is_empty());
}

/// The cone lands on the change and survives replay, and an op written
/// without one folds to an empty cone rather than failing to decode.
#[test]
fn the_cone_folds_onto_the_change_and_replays() {
    let base = ContentHash::blake3(b"base revision");
    let scoped = ViewOp::new(OpKind::CreateChange {
        id: "c1".into(),
        owner: "ana".into(),
        workspace: "acme/mono/feat".into(),
        base_revision: base.clone(),
        idempotency_key: "k1".into(),
        owner_sig: None,
        cone: vec!["libs/shared".into(), "services/api".into()],
    });
    let unscoped = ViewOp::new(OpKind::CreateChange {
        id: "c2".into(),
        owner: "ana".into(),
        workspace: "acme/mono/other".into(),
        base_revision: base,
        idempotency_key: "k2".into(),
        owner_sig: None,
        cone: Vec::new(),
    });

    let mut log = choir_oplog::MemLog::new();
    append_op(&mut log, "ana", scoped).expect("the scoped change lands");
    append_op(&mut log, "ana", unscoped).expect("the unscoped change lands");

    let view = View::materialize(&log).expect("replay");
    assert_eq!(
        view.changes["c1"].cone,
        vec!["libs/shared".to_string(), "services/api".to_string()]
    );
    assert!(view.changes["c2"].cone.is_empty());
}

/// An unscoped authorization serializes exactly as it did before cones
/// existed. This is invariant 1 for this field: every signature over an
/// older authorization must still verify, and it only does if the empty
/// cone contributes no bytes.
#[test]
fn an_empty_cone_adds_no_bytes_to_the_signed_payload() {
    let authorization = CreateAuthorization::new(
        "c1".into(),
        "ana".into(),
        "acme/mono/feat".into(),
        ContentHash::blake3(b"base"),
        "k1".into(),
    );
    let payload = authorization.to_payload();
    let text = String::from_utf8(payload.clone()).expect("json is utf-8");
    assert!(
        !text.contains("cone"),
        "an unscoped authorization grew a field: {text}"
    );

    // And a scoped one does carry it, so the absence above is the
    // skip rule rather than the field being dropped everywhere.
    let scoped = CreateAuthorization::new(
        "c1".into(),
        "ana".into(),
        "acme/mono/feat".into(),
        ContentHash::blake3(b"base"),
        "k1".into(),
    )
    .with_cone(vec!["services/api".into()]);
    let scoped_text = String::from_utf8(scoped.to_payload()).expect("json is utf-8");
    assert!(scoped_text.contains("services/api"), "{scoped_text}");
    assert_ne!(payload, scoped.to_payload());
}
