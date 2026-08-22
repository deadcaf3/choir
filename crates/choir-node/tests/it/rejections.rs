//! Rejections must name the repair, and a retry of an operation that
//! already landed must be distinguishable from a genuine conflict.
//!
//! The two CAS cases look identical today and call for opposite actions:
//! "your write already landed" means read back and proceed, "someone else
//! moved the head" means re-read and rebase. An agent that cannot tell
//! them apart either retries a completed write or abandons a successful
//! one.

use choir_identity::{ActorKey, Registry};
use choir_node::platform::AuthorizedChangeCreate;
use choir_node::reject::{Code, Rejection};
use choir_node::{Node, Platform};
use choir_oplog::MemLog;
use choir_view::{ArchiveAuthorization, CreateAuthorization, OpKind, ViewOp};

use crate::support::curl;

use crate::support::submit_body_legacy as submit_body;

#[test]
fn a_replayed_submission_is_told_where_it_landed_not_that_it_conflicted() {
    let work = std::env::temp_dir().join(format!("choir-reject-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();

    let author = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&author.public_key_bytes()).unwrap();

    let platform =
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate()).unwrap();
    let create_authorization = CreateAuthorization::new(
        "change-1".into(),
        "alice".into(),
        "demo/alice".into(),
        choir_oplog::ContentHash::from_git_oid("1111111111111111111111111111111111111111").unwrap(),
        "request-1".into(),
    )
    .to_payload();
    let create_signature = author.sign_submission("alice", &create_authorization);
    platform
        .create_change(
            AuthorizedChangeCreate {
                id: "change-1",
                owner: "alice",
                workspace: "demo/alice",
                base_hex: "1111111111111111111111111111111111111111",
                idempotency_key: "request-1",
                owner_sig: create_signature,
                cone: Vec::new(),
            },
            "git/test",
        )
        .unwrap();
    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
    node.enable_platform(platform);
    let port = node.port();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    let api = format!("http://127.0.0.1:{port}/api");

    let set_main = ViewOp::new(OpKind::SetRef {
        name: "demo:refs/heads/main".into(),
        commit: choir_oplog::ContentHash::blake3(b"c1"),
        prev: None,
    });
    let post = |op: &ViewOp| {
        curl(&[
            "-X",
            "POST",
            "-d",
            &submit_body(&author, "alice", op),
            &format!("{api}/submit"),
        ])
    };

    // Stable changes are provisioned by the node, checkpointed only by
    // their signed owner, and cannot be bypassed with a legacy head move.
    let create_directly = ViewOp::new(OpKind::CreateChange {
        id: "change-2".into(),
        owner: "alice".into(),
        workspace: "demo/alice-2".into(),
        base_revision: choir_oplog::ContentHash::from_git_oid(
            "1111111111111111111111111111111111111111",
        )
        .unwrap(),
        idempotency_key: "request-2".into(),
        owner_sig: None,
        cone: Vec::new(),
    });
    let (code, direct) = post(&create_directly);
    assert_eq!(code, 400, "{direct}");
    assert_eq!(direct["code"], "node_only", "{direct}");

    let archive_authorization = ArchiveAuthorization::new(
        "change-1".into(),
        "demo/alice".into(),
        choir_oplog::ContentHash::from_git_oid("1111111111111111111111111111111111111111").unwrap(),
    );
    let raw_archive = ViewOp::new(OpKind::ArchiveChange {
        id: "change-1".into(),
        workspace: "demo/alice".into(),
        prev_revision: choir_oplog::ContentHash::from_git_oid(
            "1111111111111111111111111111111111111111",
        )
        .unwrap(),
        owner: "alice".into(),
        owner_sig: author.sign_submission("alice", &archive_authorization.to_payload()),
    });
    let (code, raw_archive) = post(&raw_archive);
    assert_eq!(code, 400, "{raw_archive}");
    assert_eq!(raw_archive["code"], "node_only", "{raw_archive}");

    let checkpoint = ViewOp::new(OpKind::CheckpointChange {
        id: "change-1".into(),
        workspace: "demo/alice".into(),
        revision: choir_oplog::ContentHash::from_git_oid(
            "2222222222222222222222222222222222222222",
        )
        .unwrap(),
        prev_revision: choir_oplog::ContentHash::from_git_oid(
            "1111111111111111111111111111111111111111",
        )
        .unwrap(),
    });
    let wrong_owner_body = submit_body(&author, "mallory", &checkpoint);
    let (code, wrong_owner) = curl(&[
        "-X",
        "POST",
        "-d",
        &wrong_owner_body,
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 400, "{wrong_owner}");
    assert_eq!(wrong_owner["code"], "channel_not_owned", "{wrong_owner}");
    let (code, checkpointed) = post(&checkpoint);
    assert_eq!(code, 200, "{checkpointed}");

    let legacy_move = ViewOp::new(OpKind::SetWorkspaceHead {
        workspace: "demo/alice".into(),
        commit: choir_oplog::ContentHash::from_git_oid("3333333333333333333333333333333333333333")
            .unwrap(),
        prev: Some(
            choir_oplog::ContentHash::from_git_oid("2222222222222222222222222222222222222222")
                .unwrap(),
        ),
    });
    let (code, bypass) = post(&legacy_move);
    assert_eq!(code, 400, "{bypass}");
    assert_eq!(bypass["code"], "workspace_state", "{bypass}");

    let (code, first) = post(&set_main);
    assert_eq!(code, 200, "{first}");
    let (seq, hash) = (first["seq"].clone(), first["hash"].clone());
    assert!(
        first["already_applied"].is_null(),
        "first landing is not a replay"
    );

    // Byte-identical resubmission: the same signed bytes a client would
    // send if its response were lost. CAS fails, but the honest answer is
    // "it already landed, here".
    let (code, replay) = post(&set_main);
    assert_eq!(
        code, 200,
        "a completed retry should not read as a conflict: {replay}"
    );
    assert_eq!(replay["already_applied"], true, "{replay}");
    assert_eq!(replay["seq"], seq, "must report the original seq");
    assert_eq!(replay["hash"], hash, "must report the original hash");

    // A genuinely different op that fails CAS is still a conflict, and it
    // now says what it expected and what it found.
    let conflicting = ViewOp::new(OpKind::SetRef {
        name: "demo:refs/heads/main".into(),
        commit: choir_oplog::ContentHash::blake3(b"c2"),
        prev: None, // wrong: main exists now
    });
    let (code, resp) = post(&conflicting);
    assert_eq!(code, 400, "{resp}");
    assert_eq!(resp["code"], "stale_head", "{resp}");
    assert!(
        resp["actual"].is_string(),
        "conflict must name what it found: {resp}"
    );
    assert!(
        resp["next"].as_str().unwrap().contains("resubmit"),
        "a conflict must name the repair: {resp}"
    );

    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}

#[test]
fn every_rejection_names_a_next_action() {
    // The research isolates the gain to naming an admissible alternative,
    // so `next` is required rather than optional. A rejection that cannot
    // say what to do next is one whose author has not finished thinking.
    for code in Code::all() {
        let r = Rejection::new(*code, "something failed", "do the specific thing");
        let v = r.to_json();
        assert_eq!(v["code"], code.as_str());
        assert!(
            v["next"].as_str().is_some_and(|s| !s.is_empty()),
            "{code:?}"
        );
        // Absent, not null, when no comparison happened -- a client
        // should find nothing rather than a null to special-case.
        assert!(v.get("expected").is_none(), "{code:?}");
        assert!(v.get("actual").is_none(), "{code:?}");
    }
}

#[test]
fn decoding_never_drops_a_message_it_did_not_write() {
    // Rejections cross the sequencer seam as strings, and not every
    // reason string originates here. Anything unrecognised has to keep
    // its text rather than being replaced by a generic error.
    let plain = Rejection::decode("some older reason nobody structured");
    assert_eq!(plain.code, "unclassified");
    assert_eq!(plain.error, "some older reason nobody structured");
    assert!(!plain.next.is_empty());

    // Round trip.
    let original = Rejection::new(Code::StaleHead, "CAS failed", "re-read and resubmit")
        .with_states(Some("abc".into()), Some("def".into()));
    assert_eq!(Rejection::decode(&original.encode()), original);

    // A half-shaped body is unclassified rather than a code it cannot
    // back up: reporting `stale_head` with no message would be worse
    // than reporting nothing.
    let partial = Rejection::decode(r#"{"code":"stale_head"}"#);
    assert_eq!(partial.code, "unclassified");
    assert!(
        partial.error.contains("stale_head"),
        "original text kept: {partial:?}"
    );
}

/// `Code::all()` is the list every documentation gate iterates, and until
/// now nothing tested that list against the enum.
///
/// `as_str`, `meaning` and `action` are exhaustive matches, so a new
/// variant already breaks *their* compilation. `all()` is a plain array,
/// so it silently stays short — and a variant missing from it is absent
/// from `ERRORS.md` with nothing failing, which defeats the point of
/// `every_rejection_code_the_node_can_emit_is_documented`: that gate
/// iterates `all()`, so it cannot see what `all()` omits.
///
/// Not hypothetical. Adding `IdentityState` produced a clean generator run
/// reporting `ERRORS.md` "unchanged", and it nearly shipped that way.
///
/// The fix is a compile error rather than a discipline. `index` has no
/// wildcard arm, so a new variant stops this file compiling until it is
/// named; naming it forces `COUNT` up, and `COUNT` is what refuses to
/// match `all().len()` until the variant is listed there too.
#[test]
fn code_all_lists_every_variant() {
    fn index(code: Code) -> usize {
        match code {
            Code::UnknownKey => 0,
            Code::MalformedOp => 1,
            Code::MalformedRequest => 2,
            Code::ReviewerMismatch => 3,
            Code::ChannelNotOwned => 4,
            Code::NodeOnly => 5,
            Code::AssignmentRequired => 6,
            Code::ProtectedRef => 7,
            Code::ReviewRequired => 8,
            Code::RefUndeletable => 9,
            Code::StaleHead => 10,
            Code::ReviewState => 11,
            Code::ProvenanceState => 12,
            Code::ChangeState => 13,
            Code::WorkspaceState => 14,
            Code::IdentityState => 15,
            Code::PolicyUnavailable => 16,
            Code::LogEvicted => 17,
            Code::DuplicateSubmission => 18,
            Code::ScopeRequired => 19,
            Code::ForeignScope => 20,
            Code::StaleScope => 21,
            Code::BadSignature => 22,
            Code::Unclassified => 23,
            Code::QuotaExceeded => 24,
            Code::VouchState => 25,
            Code::WitnessState => 26,
        }
    }
    const COUNT: usize = 27;

    let all = Code::all();
    assert_eq!(
        all.len(),
        COUNT,
        "Code::all() has {} entries but the enum has {COUNT} variants: a variant was given an \
         index above without being listed in all()",
        all.len()
    );

    let mut seen = [false; COUNT];
    for code in all {
        let i = index(*code);
        assert!(!seen[i], "Code::all() lists {} twice", code.as_str());
        seen[i] = true;
    }
    assert!(
        seen.iter().all(|listed| *listed),
        "Code::all() does not cover every variant"
    );

    // The wire strings are the contract, so a duplicate there is as bad as
    // a missing entry: two variants answering to one name make a client's
    // branch ambiguous.
    let mut names: Vec<&str> = all.iter().map(|code| code.as_str()).collect();
    names.sort_unstable();
    let before = names.len();
    names.dedup();
    assert_eq!(before, names.len(), "two Code variants share a wire string");
}
