//! Golden vectors: frozen `(value, canonical bytes, content hash)` triples
//! for every persisted, hashed shape in the workspace.
//!
//! These exist so that the *hash input format* — a one-way door — cannot
//! drift by accident. Any change to a `serde` attribute, a field order, a
//! map type, or the hashing pipeline itself breaks a test here rather than
//! silently orphaning every stored hash.
//!
//! **What is actually at risk.** [`OpEntry::payload`] is `Vec<u8>` and
//! therefore opaque: an entry hash covers the payload's bytes verbatim, so
//! editing [`OpKind`] can never move the hash of an entry already on disk.
//! The exposure is *client-side re-serialization* — a client that builds
//! the "same" op after a schema edit and gets different bytes produces a
//! different signature and a different entry hash. That is what the
//! [`ViewOp`] vectors below pin down, and it is why they assert the exact
//! canonical bytes rather than only the digest.
//!
//! The other exposure is invariant 3, which nothing else enforces: hashes
//! are BLAKE3 over `serde_json::to_vec`, and every map inside a hashed
//! struct is a `BTreeMap` for exactly that reason. Swapping one to a
//! `HashMap` compiles, passes the rest of the suite, and silently breaks
//! every hash in the repo. [`commit_is_frozen`] is the test that notices.
//!
//! Two layers per vector, both required:
//!
//! 1. **Canonical bytes**, asserted as a string. A hash-only test would
//!    pass a change that swapped one hash function for another of the same
//!    output length; asserting the input localizes a break to
//!    serialization vs. hashing.
//! 2. **Content hash**, as [`ContentHash::to_hex`] renders it.
//!
//! This crate is the home for all four shapes because it is the only one
//! that can see `OpEntry`, `ViewOp`, `Commit`, and `Manifest` at once.
//!
//! Changing a value here is never a routine fix. It means data written by
//! an earlier build no longer round-trips, which needs a `format_version`
//! decision and a migration. To re-derive the constants after a
//! *deliberate* format change:
//!
//! ```text
//! CHOIR_GOLDEN_REGEN=1 cargo test -p choir-view --test golden -- --nocapture
//! ```

use std::collections::BTreeMap;

use choir_hash::ContentHash;
use choir_oplog::{signing_hash, OpEntry, Witness, FORMAT_VERSION as OPLOG_FORMAT_VERSION};
use choir_store::{ChunkerParams, Manifest, FORMAT_VERSION as STORE_FORMAT_VERSION};
use choir_view::{
    ArchiveAuthorization, Commit, CreateAuthorization, OpKind, RefSnapshot, TreeEntry, Verdict,
    ViewOp,
    FORMAT_VERSION as VIEW_FORMAT_VERSION,
};

/// Whether this run prints fresh constants instead of checking frozen ones.
fn regenerating() -> bool {
    std::env::var_os("CHOIR_GOLDEN_REGEN").is_some()
}

/// A digest with no randomness in it, so the vectors are reproducible.
fn h(tag: &[u8]) -> ContentHash {
    ContentHash::blake3(tag)
}

/// Asserts both layers at once: the canonical bytes and their digest.
/// Under `CHOIR_GOLDEN_REGEN` it prints what it would have asserted.
#[track_caller]
fn assert_golden<T: serde::Serialize>(name: &str, value: &T, canonical: &str, hash_hex: &str) {
    let bytes = serde_json::to_vec(value).expect("golden value serializes");
    let actual = String::from_utf8(bytes.clone()).expect("canonical form is UTF-8");
    let actual_hash = ContentHash::blake3(&bytes).to_hex();
    if regenerating() {
        println!("{name}\n    r#\"{actual}\"#,\n    \"{actual_hash}\",");
        return;
    }
    assert_eq!(actual, canonical, "{name}: canonical serialization drifted");
    assert_eq!(actual_hash, hash_hex, "{name}: content hash drifted");
}

/// Frozen scalar (a hash hex, a head). Same regeneration contract.
#[track_caller]
fn assert_frozen(name: &str, actual: &str, expected: &str) {
    if regenerating() {
        println!("{name}\n    \"{actual}\",");
        return;
    }
    assert_eq!(actual, expected, "{name} drifted");
}

// ---------------------------------------------------------------------------
// The frozen values.
// ---------------------------------------------------------------------------

/// `format_version` constants are themselves part of the hash input.
/// Bumping one changes every stored hash, so it gets its own assertion.
#[test]
fn format_versions_are_frozen() {
    assert_eq!(OPLOG_FORMAT_VERSION, 1);
    assert_eq!(VIEW_FORMAT_VERSION, 1);
    assert_eq!(STORE_FORMAT_VERSION, 1);
}

/// The envelope embedded in every other shape here, so this is the shared
/// prefix of all the vectors below. A `Vec<u8>` digest and a `[u8; 32]`
/// digest serialize to the same JSON array, and this vector is what would
/// prove it if that change is ever made (hypothesis 8 / S2.5).
#[test]
fn content_hash_envelope_is_frozen() {
    assert_golden(
        "blake3 envelope",
        &h(b"choir golden"),
        r#"{"codec":30,"digest":[94,185,213,36,82,5,146,247,84,228,39,33,221,224,100,246,192,220,101,105,50,110,48,244,17,189,63,7,152,181,132,93]}"#,
        "1e-64092f730ba3d1d32a1e41989ba3c8e4353fdab48eb909761b3b41227e23a46c",
    );
    // Git oids enter the same envelope under their own codec, and are 20
    // bytes rather than 32 — the case that rules out a bare `[u8; 32]`.
    assert_golden(
        "git sha-1 envelope",
        &ContentHash::from_git_oid("0123456789abcdef0123456789abcdef01234567")
            .expect("40 hex chars is a sha-1 oid"),
        r#"{"codec":17,"digest":[1,35,69,103,137,171,205,239,1,35,69,103,137,171,205,239,1,35,69,103]}"#,
        "1e-f1a4f6306f26f424dcd820f7d2371ebc4efe88f597ab076c6eb54c1635a695ab",
    );
}

#[test]
fn op_entry_is_frozen() {
    let entry = OpEntry {
        format_version: OPLOG_FORMAT_VERSION,
        parent: Some(h(b"parent entry")),
        seq: 7,
        channel: "agent-1".into(),
        payload: b"opaque payload".to_vec(),
        witnesses: vec![Witness::ed25519("1e-witness", vec![1, 2, 3, 4])],
        author_sig: Some(Witness::ed25519("1e-author", vec![9, 8, 7, 6])),
    };
    assert_golden(
        "signed op entry",
        &entry,
        r#"{"format_version":1,"parent":{"codec":30,"digest":[238,65,22,30,7,194,94,104,44,177,31,206,40,244,251,226,239,146,165,211,227,148,64,190,230,71,87,141,154,184,115,175]},"seq":7,"workspace":"agent-1","payload":[111,112,97,113,117,101,32,112,97,121,108,111,97,100],"witnesses":[{"key_id":"1e-witness","signature":[1,2,3,4]}],"author_sig":{"key_id":"1e-author","signature":[9,8,7,6]}}"#,
        "1e-f5ac519432f22de91eefb38df95da51a93ca8ce377cb540f98c304b06c5bd3ee",
    );
    // `content_hash` must agree with hashing the canonical bytes by hand.
    // That equivalence is precisely what S3.1 — streaming serde straight
    // into the hasher instead of through a `Vec<u8>` — may not break.
    assert_frozen(
        "signed op entry content_hash",
        &entry.content_hash().to_hex(),
        "1e-f5ac519432f22de91eefb38df95da51a93ca8ce377cb540f98c304b06c5bd3ee",
    );
}

/// The additive-field rule (README invariant 1) in its load-bearing form:
/// an entry written before `author_sig` existed must re-serialize
/// byte-identically, so its hash does not move. `skip_serializing_if` is
/// what makes that true; deleting it silently rewrites history.
#[test]
fn op_entry_without_author_sig_omits_the_field() {
    let entry = OpEntry {
        format_version: OPLOG_FORMAT_VERSION,
        parent: None,
        seq: 0,
        channel: "agent-1".into(),
        payload: b"genesis".to_vec(),
        witnesses: Vec::new(),
        author_sig: None,
    };
    let canonical = serde_json::to_string(&entry).expect("serializes");
    assert!(
        !canonical.contains("author_sig"),
        "an unsigned entry must not emit the additive field: {canonical}"
    );
    assert_golden(
        "unsigned genesis entry",
        &entry,
        r#"{"format_version":1,"parent":null,"seq":0,"workspace":"agent-1","payload":[103,101,110,101,115,105,115],"witnesses":[]}"#,
        "1e-c5766de4771347a8be9db0f9304843c2a51888ec008341bd119af0ca33499b9a",
    );
}

/// What an author signs, held separate from what the entry hashes to.
/// `signing_hash` covers `(channel, payload)` only; changing either the
/// tuple shape or its encoding invalidates every signature ever issued.
#[test]
fn signing_hash_is_frozen() {
    let hash = signing_hash("agent-1", b"opaque payload");
    assert_frozen(
        "signing_hash digest",
        &hash.to_hex(),
        "1e-964637d5b4cae870451e1bf4f0a1362bee52e593e99cf71b6ced6e778eabea7d",
    );
    // The *signed message* today is the hex rendering of that digest, not
    // the 32 digest bytes (choir-identity `sign_submission`). Pinned here
    // so that switching to raw bytes is a deliberate, versioned protocol
    // change with a dual-verify transition, and never a silent edit.
    assert_eq!(hash.to_hex().len(), 67, "codec byte + '-' + 64 hex chars");
    assert_eq!(&hash.to_hex()[..3], "1e-");
}

#[test]
fn view_op_variants_are_frozen() {
    assert_golden(
        "SetWorkspaceHead",
        &ViewOp::new(OpKind::SetWorkspaceHead {
            workspace: "agent-1".into(),
            commit: h(b"commit one"),
            prev: None,
        }),
        r#"{"format_version":1,"kind":{"SetWorkspaceHead":{"workspace":"agent-1","commit":{"codec":30,"digest":[113,243,157,180,180,13,146,48,202,21,47,8,14,78,28,196,37,204,129,12,165,24,94,86,43,113,252,6,133,86,128,3]},"prev":null}}}"#,
        "1e-d6385bf4131cd600e06de81738da4beb345f10e5e830e5676360489b0ce64254",
    );
    assert_golden(
        "SetRef",
        &ViewOp::new(OpKind::SetRef {
            name: "repo.git:refs/heads/main".into(),
            commit: h(b"commit two"),
            prev: Some(h(b"commit one")),
        }),
        r#"{"format_version":1,"kind":{"SetRef":{"name":"repo.git:refs/heads/main","commit":{"codec":30,"digest":[85,132,118,97,239,147,219,56,81,251,10,83,89,23,246,20,25,60,73,118,206,203,90,41,65,69,251,150,168,140,62,8]},"prev":{"codec":30,"digest":[113,243,157,180,180,13,146,48,202,21,47,8,14,78,28,196,37,204,129,12,165,24,94,86,43,113,252,6,133,86,128,3]}}}}"#,
        "1e-da4cf41a9219e81107051235b221e12cd5b14ef02dd76b0b61da11aa450ca1c9",
    );
    assert_golden(
        "DeleteRef",
        &ViewOp::new(OpKind::DeleteRef {
            name: "repo.git:refs/heads/topic".into(),
            prev: Some(h(b"commit two")),
        }),
        r#"{"format_version":1,"kind":{"DeleteRef":{"name":"repo.git:refs/heads/topic","prev":{"codec":30,"digest":[85,132,118,97,239,147,219,56,81,251,10,83,89,23,246,20,25,60,73,118,206,203,90,41,65,69,251,150,168,140,62,8]}}}}"#,
        "1e-61446435c7495335be692f744e35243fbe784223672a6b392fa75847228b298e",
    );
    assert_golden(
        "DeleteWorkspace",
        &ViewOp::new(OpKind::DeleteWorkspace {
            workspace: "agent-1".into(),
        }),
        r#"{"format_version":1,"kind":{"DeleteWorkspace":{"workspace":"agent-1"}}}"#,
        "1e-7e7373bc44c635f8fb477fcf77c0fd686c877db71513fa0ee7f49003be8b706d",
    );
    assert_golden(
        "AssignReviewers",
        &ViewOp::new(OpKind::AssignReviewers {
            id: "review-1".into(),
            reviewers: vec!["alice".into(), "bob".into()],
        }),
        r#"{"format_version":1,"kind":{"AssignReviewers":{"id":"review-1","reviewers":["alice","bob"]}}}"#,
        "1e-a74ab6f2db11ff9885bde372e80fb97eeeff48d644b91321981768e61862e5b6",
    );
    assert_golden(
        "PostVerdict",
        &ViewOp::new(OpKind::PostVerdict {
            id: "review-1".into(),
            reviewer: "alice".into(),
            verdict: Verdict::Approve,
            note: "looks right".into(),
        }),
        r#"{"format_version":1,"kind":{"PostVerdict":{"id":"review-1","reviewer":"alice","verdict":"Approve","note":"looks right"}}}"#,
        "1e-a6fb60395b7baf9077da33bb41ea8e1edfc299f489979742702c4f282a264545",
    );
    // D38. A comment is signed like any other op, so its canonical bytes
    // are what a client re-serializes when it retries and what the
    // signature covers. Four `String` fields in declaration order, and no
    // map anywhere in the payload -- the ordering that matters lives in
    // the fold, where the thread is a `Vec` rather than a map.
    assert_golden(
        "PostComment",
        &ViewOp::new(OpKind::PostComment {
            id: "review-1".into(),
            comment: "c1".into(),
            author: "alice/agent".into(),
            body: "why this base?".into(),
        }),
        r#"{"format_version":1,"kind":{"PostComment":{"id":"review-1","comment":"c1","author":"alice/agent","body":"why this base?"}}}"#,
        "1e-5b0fdd7cbf9cc5457742f8fd471b6a85118b3f6a6aed6e1b9c31e7c74184eab9",
    );
    assert_golden(
        "SlashApproval",
        &ViewOp::new(OpKind::SlashApproval {
            id: "review-1".into(),
            reviewer: "alice/agent".into(),
            reason: "retroactive policy finding".into(),
        }),
        r#"{"format_version":1,"kind":{"SlashApproval":{"id":"review-1","reviewer":"alice/agent","reason":"retroactive policy finding"}}}"#,
        "1e-0860735ec01966d4aa4986add081a8038e3d644a38dd141506dd668c5298be63",
    );
    assert_golden(
        "RecordProvenance",
        &ViewOp::new(OpKind::RecordProvenance {
            subject: "agent-1".into(),
            kind: "task-spec".into(),
            body: "make the thing".into(),
        }),
        r#"{"format_version":1,"kind":{"RecordProvenance":{"subject":"agent-1","kind":"task-spec","body":"make the thing"}}}"#,
        "1e-f64fa277e20b2169ae1787abd98159f5f446680be3ddb5eda8f48ffce7c9e1b9",
    );
    assert_golden(
        "CreateChange",
        &ViewOp::new(OpKind::CreateChange {
            id: "change-1".into(),
            owner: "operator/agent".into(),
            workspace: "repo/agent".into(),
            base_revision: h(b"commit one"),
            idempotency_key: "request-1".into(),
            owner_sig: None,
        }),
        r#"{"format_version":1,"kind":{"CreateChange":{"id":"change-1","owner":"operator/agent","workspace":"repo/agent","base_revision":{"codec":30,"digest":[113,243,157,180,180,13,146,48,202,21,47,8,14,78,28,196,37,204,129,12,165,24,94,86,43,113,252,6,133,86,128,3]},"idempotency_key":"request-1"}}}"#,
        "1e-a64d00f040ad8ac87e8d120f57b6fc186a576bf1c9d5a92df8379514c428d7d1",
    );
    assert_golden(
        "CheckpointChange",
        &ViewOp::new(OpKind::CheckpointChange {
            id: "change-1".into(),
            workspace: "repo/agent".into(),
            revision: h(b"commit two"),
            prev_revision: h(b"commit one"),
        }),
        r#"{"format_version":1,"kind":{"CheckpointChange":{"id":"change-1","workspace":"repo/agent","revision":{"codec":30,"digest":[85,132,118,97,239,147,219,56,81,251,10,83,89,23,246,20,25,60,73,118,206,203,90,41,65,69,251,150,168,140,62,8]},"prev_revision":{"codec":30,"digest":[113,243,157,180,180,13,146,48,202,21,47,8,14,78,28,196,37,204,129,12,165,24,94,86,43,113,252,6,133,86,128,3]}}}}"#,
        "1e-ed94035812d9ddd3018557c3c1e98a12b64a1ebd44b8a290c409f240975d2195",
    );
    assert_golden(
        "ArchiveChange",
        &ViewOp::new(OpKind::ArchiveChange {
            id: "change-1".into(),
            workspace: "repo/agent".into(),
            prev_revision: h(b"commit two"),
            owner: "operator/agent".into(),
            owner_sig: Witness::ed25519("owner-key", vec![1, 2, 3]),
        }),
        r#"{"format_version":1,"kind":{"ArchiveChange":{"id":"change-1","workspace":"repo/agent","prev_revision":{"codec":30,"digest":[85,132,118,97,239,147,219,56,81,251,10,83,89,23,246,20,25,60,73,118,206,203,90,41,65,69,251,150,168,140,62,8]},"owner":"operator/agent","owner_sig":{"key_id":"owner-key","signature":[1,2,3]}}}}"#,
        "1e-86f079f00edc377222fd784aaff45ee30e887979385b6e77b2abd60b19fdf2d3",
    );
    assert_golden(
        "BindKey",
        &ViewOp::new(OpKind::BindKey {
            operator: "alice".into(),
            key: h(b"actor key"),
            channel: Some("alice/agent".into()),
        }),
        r#"{"format_version":1,"kind":{"BindKey":{"operator":"alice","key":{"codec":30,"digest":[74,226,209,106,169,109,122,128,69,150,239,183,206,147,93,53,14,89,126,246,185,152,171,32,6,11,49,108,128,53,125,237]},"channel":"alice/agent"}}}"#,
        "1e-6dbe521521bb82be5fd8b04ac8c84c86c59c4deb3d73244945f711a4bf408f1a",
    );
    assert_golden(
        "RevokeKey",
        &ViewOp::new(OpKind::RevokeKey {
            key: h(b"actor key"),
            reason: "key material leaked".into(),
        }),
        r#"{"format_version":1,"kind":{"RevokeKey":{"key":{"codec":30,"digest":[74,226,209,106,169,109,122,128,69,150,239,183,206,147,93,53,14,89,126,246,185,152,171,32,6,11,49,108,128,53,125,237]},"reason":"key material leaked"}}}"#,
        "1e-a5e01e6bb86011c79abd03ceb3ba823cb1e1bbe3ccd860eb613ae0b352a0b5b8",
    );
}

#[test]
fn archive_authorization_is_frozen() {
    assert_golden(
        "ArchiveAuthorization",
        &ArchiveAuthorization::new(
            "change-1".into(),
            "repo/agent".into(),
            h(b"commit two"),
        ),
        r#"{"format_version":1,"id":"change-1","workspace":"repo/agent","prev_revision":{"codec":30,"digest":[85,132,118,97,239,147,219,56,81,251,10,83,89,23,246,20,25,60,73,118,206,203,90,41,65,69,251,150,168,140,62,8]}}"#,
        "1e-d8ea0a752dd478142674409c07b78547d8833ee57a31671088cd288c578448a7",
    );
}

#[test]
fn create_authorization_is_frozen() {
    assert_golden(
        "CreateAuthorization",
        &CreateAuthorization::new(
            "change-1".into(),
            "operator/agent".into(),
            "repo/agent".into(),
            h(b"commit one"),
            "request-1".into(),
        ),
        r#"{"format_version":1,"id":"change-1","owner":"operator/agent","workspace":"repo/agent","base_revision":{"codec":30,"digest":[113,243,157,180,180,13,146,48,202,21,47,8,14,78,28,196,37,204,129,12,165,24,94,86,43,113,252,6,133,86,128,3]},"idempotency_key":"request-1"}"#,
        "1e-d7555586b9295c393ecb2118db7a36943de6a789ebd056c6867d656020749a84",
    );
}

#[test]
fn owner_authorized_create_change_is_frozen() {
    assert_golden(
        "owner-authorized CreateChange",
        &ViewOp::new(OpKind::CreateChange {
            id: "change-1".into(),
            owner: "operator/agent".into(),
            workspace: "repo/agent".into(),
            base_revision: h(b"commit one"),
            idempotency_key: "request-1".into(),
            owner_sig: Some(Witness::ed25519("owner-key", vec![4, 5, 6])),
        }),
        r#"{"format_version":1,"kind":{"CreateChange":{"id":"change-1","owner":"operator/agent","workspace":"repo/agent","base_revision":{"codec":30,"digest":[113,243,157,180,180,13,146,48,202,21,47,8,14,78,28,196,37,204,129,12,165,24,94,86,43,113,252,6,133,86,128,3]},"idempotency_key":"request-1","owner_sig":{"key_id":"owner-key","signature":[4,5,6]}}}}"#,
        "1e-600348c35115eb72878f3c005c1732e14ffdc576fd8b9e5b4f90266724e5ca7f",
    );
}

/// `channel` is the second worked example of an additive field on an enum
/// variant, and it guards the newest one-way door in the model: a binding
/// is the durable operator record, so a client that re-serializes "the
/// same" binding into different bytes after a schema edit produces a
/// different signature and a different entry hash.
#[test]
fn bind_key_additive_field_is_frozen() {
    let channelless = ViewOp::new(OpKind::BindKey {
        operator: "alice".into(),
        key: h(b"actor key"),
        channel: None,
    });
    assert!(
        !serde_json::to_string(&channelless)
            .expect("serializes")
            .contains("channel"),
        "a binding that asserts no channel must not emit the additive field"
    );
    assert_golden(
        "BindKey without a channel",
        &channelless,
        r#"{"format_version":1,"kind":{"BindKey":{"operator":"alice","key":{"codec":30,"digest":[74,226,209,106,169,109,122,128,69,150,239,183,206,147,93,53,14,89,126,246,185,152,171,32,6,11,49,108,128,53,125,237]}}}}"#,
        "1e-18ff7f42bc91a23cc6ddfcc06e60040680229a97394ee6f1a278fc0075f37da4",
    );
}

/// `target_ref` is the worked example of an additive field on an enum
/// variant. `view.rs` already proves an old payload round-trips; this adds
/// the frozen digest, so a future edit cannot quietly change the bytes on
/// *both* sides of that round-trip and still pass.
#[test]
fn request_review_additive_field_is_frozen() {
    let unbound = ViewOp::new(OpKind::RequestReview {
        id: "review-1".into(),
        target: h(b"commit one"),
        reviewers: vec!["alice".into()],
        target_ref: None,
    });
    assert!(
        !serde_json::to_string(&unbound)
            .expect("serializes")
            .contains("target_ref"),
        "an unbound review must not emit the additive field"
    );
    assert_golden(
        "RequestReview unbound",
        &unbound,
        r#"{"format_version":1,"kind":{"RequestReview":{"id":"review-1","target":{"codec":30,"digest":[113,243,157,180,180,13,146,48,202,21,47,8,14,78,28,196,37,204,129,12,165,24,94,86,43,113,252,6,133,86,128,3]},"reviewers":["alice"]}}}"#,
        "1e-075b2d72aa1e588dccd5bb797a45ba93ac6bca9f17cb8ad57a56b64595168ca0",
    );
    assert_golden(
        "RequestReview bound and unassigned",
        &ViewOp::new(OpKind::RequestReview {
            id: "review-2".into(),
            target: h(b"commit two"),
            reviewers: Vec::new(),
            target_ref: Some("repo.git:refs/heads/main".into()),
        }),
        r#"{"format_version":1,"kind":{"RequestReview":{"id":"review-2","target":{"codec":30,"digest":[85,132,118,97,239,147,219,56,81,251,10,83,89,23,246,20,25,60,73,118,206,203,90,41,65,69,251,150,168,140,62,8]},"reviewers":[],"target_ref":"repo.git:refs/heads/main"}}}"#,
        "1e-4eab18d2622808f839b4e0fbf767a902d4bd624f35b3e28a4be9b8d397bd5024",
    );
}

/// `Commit.tree` is a `BTreeMap` so that serialization is canonical, and
/// this vector is the enforcement for invariant 3.
///
/// The tree carries eight paths deliberately. A `HashMap` substituted here
/// would serialize in its own iteration order, and with only two entries
/// that order coincides with sorted order about half the time — the test
/// would pass on a coin flip. Eight entries drop the odds of an accidental
/// pass to roughly `1/8!`, so the swap fails essentially every run rather
/// than intermittently. Verified by making the substitution and watching
/// this test fail; a two-entry version of it passed.
#[test]
fn commit_is_frozen() {
    let mut tree = BTreeMap::new();
    // Names chosen so sorted order and insertion order differ, and so no
    // two orderings of the set are plausible by accident.
    for (path, tag) in [
        ("src/lib.rs", b"blob a".as_slice()),
        ("Cargo.toml", b"blob b".as_slice()),
        ("README.md", b"blob c".as_slice()),
        ("tests/view.rs", b"blob d".as_slice()),
        ("src/bin/tool.rs", b"blob e".as_slice()),
        (".gitignore", b"blob f".as_slice()),
        ("docs/plan.md", b"blob g".as_slice()),
    ] {
        tree.insert(path.to_string(), TreeEntry::File { blob: h(tag) });
    }
    // A conflicted entry is a legal committed state (D9), so it is part of
    // the frozen format, not an error path.
    tree.insert(
        "src/main.rs".to_string(),
        TreeEntry::Conflict {
            base: Some(h(b"blob base")),
            left: h(b"blob left"),
            right: h(b"blob right"),
        },
    );
    assert_eq!(tree.len(), 8, "map-order detection needs a wide tree");
    assert_golden(
        "Commit with a conflicted entry",
        &Commit {
            format_version: VIEW_FORMAT_VERSION,
            parents: vec![h(b"parent commit")],
            tree,
            author: "agent-1".into(),
            message: "first".into(),
        },
        r#"{"format_version":1,"parents":[{"codec":30,"digest":[181,29,25,42,145,146,193,233,221,0,91,242,175,23,170,249,126,21,248,248,86,164,241,121,105,3,42,152,184,107,133,72]}],"tree":{".gitignore":{"File":{"blob":{"codec":30,"digest":[208,172,129,59,103,228,188,228,200,33,179,182,169,27,1,87,16,150,73,242,110,15,34,0,137,181,172,93,15,142,151,35]}}},"Cargo.toml":{"File":{"blob":{"codec":30,"digest":[129,181,50,104,117,216,204,114,155,68,140,215,2,32,92,107,90,96,181,93,215,31,108,217,51,203,210,126,36,60,109,119]}}},"README.md":{"File":{"blob":{"codec":30,"digest":[205,231,220,112,13,97,181,12,170,86,12,215,38,191,148,115,35,165,189,138,73,130,144,50,195,243,41,144,68,212,227,240]}}},"docs/plan.md":{"File":{"blob":{"codec":30,"digest":[229,205,201,63,64,55,171,12,216,52,41,1,218,228,77,51,79,142,163,144,50,85,6,212,16,55,101,250,162,26,39,14]}}},"src/bin/tool.rs":{"File":{"blob":{"codec":30,"digest":[43,186,71,245,80,128,7,16,113,170,180,172,101,242,175,191,223,215,171,66,47,1,27,31,246,107,63,167,254,102,176,244]}}},"src/lib.rs":{"File":{"blob":{"codec":30,"digest":[70,136,166,203,46,155,175,8,195,156,128,172,70,185,16,166,96,33,255,27,234,254,245,119,121,54,110,37,89,189,38,130]}}},"src/main.rs":{"Conflict":{"base":{"codec":30,"digest":[63,22,247,85,132,83,32,39,61,145,212,49,68,10,41,113,121,104,120,151,151,95,228,208,68,94,95,222,234,214,57,93]},"left":{"codec":30,"digest":[10,43,38,88,220,53,61,166,22,247,60,111,223,168,166,113,62,121,83,174,80,191,136,145,27,88,54,79,85,17,47,14]},"right":{"codec":30,"digest":[50,123,238,170,124,35,217,21,238,4,66,157,14,52,215,172,131,166,192,43,60,72,160,145,243,53,142,120,213,147,8,36]}}},"tests/view.rs":{"File":{"blob":{"codec":30,"digest":[127,41,192,24,185,72,75,57,5,172,183,84,100,207,125,86,0,108,52,138,240,252,204,120,73,125,158,109,116,119,90,184]}}}},"author":"agent-1","message":"first"}"#,
        "1e-2fa60bc4fac48afaa2c476babd36c77cbff607c548dd23e3b1b15ef11389837a",
    );
}

#[test]
fn manifest_is_frozen() {
    assert_golden(
        "Manifest",
        &Manifest {
            format_version: STORE_FORMAT_VERSION,
            params: ChunkerParams::default(),
            len: 3,
            chunks: vec![h(b"chunk a"), h(b"chunk b")],
        },
        r#"{"format_version":1,"params":{"min":4096,"avg":16384,"max":65536},"len":3,"chunks":[{"codec":30,"digest":[41,133,194,165,16,250,151,20,4,135,164,248,205,76,198,4,66,71,57,69,45,109,83,240,193,189,169,159,73,60,175,131]},{"codec":30,"digest":[249,66,31,212,149,146,140,64,197,195,74,174,149,177,124,204,15,219,16,118,240,34,248,188,188,90,208,84,117,83,127,161]}]}"#,
        "1e-979c1a794da83934f5453ba0229f24befdb6a75e52e814e2c4249c2e7ca43942",
    );
    // Chunker parameters are recorded per object precisely so they *can*
    // change without orphaning data — but the defaults are what every
    // existing blob was cut with, so they are frozen too.
    assert_eq!(
        ChunkerParams::default(),
        ChunkerParams {
            min: 4 * 1024,
            avg: 16 * 1024,
            max: 64 * 1024
        }
    );
}

/// The D25 attestation unit, registered in `invariant_3.rs` the day it
/// was added. The map key is deliberately hostile: a git ref name may
/// legally carry `"` and non-ASCII bytes (git forbids control bytes,
/// space, `~^:?*[\` — not quotes, not high bytes), and serde_json escapes
/// map *keys* through a different path than string values. This vector is
/// what pins that escaping; a signed snapshot whose canonical bytes drift
/// is every historical attestation orphaned at once.
#[test]
fn ref_snapshot_is_frozen() {
    let mut refs = BTreeMap::new();
    refs.insert("choir/choir.git:refs/heads/main".to_string(), h(b"main head"));
    refs.insert(
        "choir/choir.git:refs/heads/a\"β".to_string(),
        h(b"hostile ref"),
    );
    let first = RefSnapshot {
        format_version: VIEW_FORMAT_VERSION,
        refs: refs.clone(),
        at_seq: 41,
        prev_snapshot: None,
    };
    // A log's first snapshot has no predecessor, and the additive rule
    // applies from birth: `prev_snapshot: None` must not serialize.
    let canonical = serde_json::to_string(&first).expect("serializes");
    assert!(
        !canonical.contains("prev_snapshot"),
        "a first snapshot must omit the additive chain field: {canonical}"
    );
    assert_golden(
        "first ref snapshot (hostile key)",
        &first,
        r#"{"format_version":1,"refs":{"choir/choir.git:refs/heads/a\"β":{"codec":30,"digest":[53,85,138,128,7,62,31,16,126,81,216,201,3,71,105,19,197,111,174,67,209,10,198,75,220,75,70,10,134,233,43,213]},"choir/choir.git:refs/heads/main":{"codec":30,"digest":[202,222,155,104,158,4,227,17,129,30,22,192,7,208,231,85,91,67,231,133,81,79,114,121,158,17,93,74,19,7,57,44]}},"at_seq":41}"#,
        "1e-cebbc7fb5627619b63ec8b23b629b39b6d4a64d96cb80bf64d58e5b633bbc602",
    );

    let chained = RefSnapshot {
        format_version: VIEW_FORMAT_VERSION,
        refs,
        at_seq: 42,
        prev_snapshot: Some(first.id()),
    };
    assert_golden(
        "chained ref snapshot",
        &chained,
        r#"{"format_version":1,"refs":{"choir/choir.git:refs/heads/a\"β":{"codec":30,"digest":[53,85,138,128,7,62,31,16,126,81,216,201,3,71,105,19,197,111,174,67,209,10,198,75,220,75,70,10,134,233,43,213]},"choir/choir.git:refs/heads/main":{"codec":30,"digest":[202,222,155,104,158,4,227,17,129,30,22,192,7,208,231,85,91,67,231,133,81,79,114,121,158,17,93,74,19,7,57,44]}},"at_seq":42,"prev_snapshot":{"codec":30,"digest":[206,187,199,251,86,39,97,155,99,236,139,35,182,41,179,155,109,74,100,217,108,184,11,246,77,88,229,182,51,187,198,2]}}"#,
        "1e-05e3a7ea8154628df2d66244dbfc3d6c23af521ab4616b16488057487ddd679e",
    );
    // `RefSnapshot::id` must agree with hashing the canonical bytes by
    // hand — it is the identity the next snapshot's chain pointer names.
    assert_frozen(
        "chained ref snapshot id",
        &chained.id().to_hex(),
        "1e-05e3a7ea8154628df2d66244dbfc3d6c23af521ab4616b16488057487ddd679e",
    );

    // The op that carries a snapshot into the log, as a client would
    // re-serialize it: the payload-bytes exposure the ViewOp vectors
    // exist for.
    assert_golden(
        "record-ref-snapshot op",
        &ViewOp::new(OpKind::RecordRefSnapshot { snapshot: chained }),
        r#"{"format_version":1,"kind":{"RecordRefSnapshot":{"snapshot":{"format_version":1,"refs":{"choir/choir.git:refs/heads/a\"β":{"codec":30,"digest":[53,85,138,128,7,62,31,16,126,81,216,201,3,71,105,19,197,111,174,67,209,10,198,75,220,75,70,10,134,233,43,213]},"choir/choir.git:refs/heads/main":{"codec":30,"digest":[202,222,155,104,158,4,227,17,129,30,22,192,7,208,231,85,91,67,231,133,81,79,114,121,158,17,93,74,19,7,57,44]}},"at_seq":42,"prev_snapshot":{"codec":30,"digest":[206,187,199,251,86,39,97,155,99,236,139,35,182,41,179,155,109,74,100,217,108,184,11,246,77,88,229,182,51,187,198,2]}}}}}"#,
        "1e-42e7d07b34c6c887aa4f6349ce7694fbf29711464c735453adbbd683b8c08694",
    );
}

/// A stored log, verbatim, as the current code writes it: the decode side
/// of the door. No optimization may change what these bytes mean, and a
/// reader built after them must still replay this file.
const STORED_LOG: &str = include_str!("../fixtures/stored.log");

#[test]
fn a_stored_log_still_decodes_and_replays() {
    use choir_oplog::{MemLog, OpLog};
    use choir_view::View;

    let text = if regenerating() {
        let fresh = build_stored_log();
        std::fs::write(
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/stored.log"),
            &fresh,
        )
        .expect("rewrite the fixture");
        fresh
    } else {
        STORED_LOG.to_string()
    };

    let mut log = MemLog::new();
    for line in text.lines() {
        let entry: OpEntry = serde_json::from_str(line).expect("stored line decodes");
        // Re-serializing must reproduce the stored line exactly. This is
        // the property that lets an entry keep its hash across a reader
        // upgrade; without it, replay recomputes a different chain.
        assert_eq!(
            serde_json::to_string(&entry).expect("re-serializes"),
            line,
            "stored line did not round-trip"
        );
        log.append(entry).expect("stored log is a valid chain");
    }
    assert_eq!(log.len(), 3);
    assert_frozen(
        "stored log head",
        &log.head().expect("non-empty").to_hex(),
        "1e-4414618015a5c234bdfc306257e5a9064338f7673f4e7fb32a170007d9431e5c",
    );

    let view = View::materialize(&log).expect("stored log replays");
    // `next_seq` is folded state, so a replay of an already-persisted log
    // has to land on the same fold position a live node held when it
    // wrote that log. If it did not, every `bound_at` recorded after a
    // restart would be silently wrong, and the drift would surface much
    // later in resync/window behaviour where it is painful to attribute.
    assert_eq!(
        view.next_seq,
        log.len(),
        "replaying a stored log must reproduce the writer's fold position"
    );
    assert_frozen(
        "stored log replayed workspace head",
        &view
            .workspaces
            .get("agent-1")
            .expect("agent-1 has a head")
            .to_hex(),
        "1e-71f39db4b40d9230ca152f080e4e1cc425cc810ca5185e562b71fc0685568003",
    );
    assert!(view.reviews.contains_key("review-1"));
}

/// Rebuilds the three-entry chain that `fixtures/stored.log` freezes. Only
/// reached under `CHOIR_GOLDEN_REGEN`; the checked-in file is the fixture.
fn build_stored_log() -> String {
    use choir_oplog::{MemLog, OpLog};
    use choir_view::append_op;

    let mut log = MemLog::new();
    for (submitter, op) in [
        (
            "agent-1",
            ViewOp::new(OpKind::SetWorkspaceHead {
                workspace: "agent-1".into(),
                commit: h(b"commit one"),
                prev: None,
            }),
        ),
        (
            "agent-1",
            ViewOp::new(OpKind::RequestReview {
                id: "review-1".into(),
                target: h(b"commit one"),
                reviewers: vec!["alice".into()],
                target_ref: None,
            }),
        ),
        (
            "alice",
            ViewOp::new(OpKind::PostVerdict {
                id: "review-1".into(),
                reviewer: "alice".into(),
                verdict: Verdict::Approve,
                note: "looks right".into(),
            }),
        ),
    ] {
        append_op(&mut log, submitter, op).expect("op applies");
    }

    let mut out = (0..log.len())
        .map(|i| serde_json::to_string(&log.get(i).expect("seq < len")).expect("serializes"))
        .collect::<Vec<_>>()
        .join("\n");
    out.push('\n');
    out
}

/// An op variant an older binary does not know does not decode, and the
/// failure ends the replay rather than skipping the op.
///
/// This is the third exposure, alongside the two in the module doc above.
/// `OpKind` is an externally tagged `serde` enum with no `serde(other)`
/// arm, so `from_payload` refuses a name it has never heard of, and
/// `View::at` folds through `ViewOp::from_payload(..)?` — one unrecognized
/// op is not stepped over, it ends the fold. So adding a variant costs
/// exactly this: old *entries* keep hashing identically, because
/// `OpEntry::payload` is opaque bytes, while old *readers* stop dead at the
/// first op carrying the new name.
///
/// Pinned because that door swings on a `serde` attribute. Adding
/// `#[serde(other)]`, or moving to an untagged or internally tagged
/// representation, turns new variants from a reader break into a
/// compatible extension — silently, inside a derive, with no other test in
/// the workspace noticing.
#[test]
fn an_unknown_op_variant_refuses_to_decode() {
    let payload = br#"{"format_version":1,"kind":{"FutureOp":{"id":"x"}}}"#;
    let error = ViewOp::from_payload(payload).expect_err("an unknown variant does not decode");
    let error = format!("{error:?}");
    assert!(
        error.contains("unknown variant") && error.contains("FutureOp"),
        "the decoder names the variant it did not recognize: {error}"
    );
}
