//! Rejections must name the repair, and a retry of an operation that
//! already landed must be distinguishable from a genuine conflict.
//!
//! The two CAS cases look identical today and call for opposite actions:
//! "your write already landed" means read back and proceed, "someone else
//! moved the head" means re-read and rebase. An agent that cannot tell
//! them apart either retries a completed write or abandons a successful
//! one.

use choir_identity::{ActorKey, Registry};
use choir_node::platform::hex_encode;
use choir_node::reject::{Code, Rejection};
use choir_node::{Node, Platform};
use choir_oplog::MemLog;
use choir_view::{OpKind, ViewOp};

fn curl(args: &[&str]) -> (u16, serde_json::Value) {
    let out = std::process::Command::new("curl")
        .args(["-s", "-w", "\n%{http_code}"])
        .args(args)
        .output()
        .expect("curl runs");
    let text = String::from_utf8_lossy(&out.stdout);
    let (body, code) = text.rsplit_once('\n').expect("status line");
    (
        code.trim().parse().expect("numeric status"),
        serde_json::from_str(body).expect("json body"),
    )
}

fn submit_body(key: &ActorKey, channel: &str, op: &ViewOp) -> String {
    let payload = op.to_payload();
    let sig = key.sign_submission(channel, &payload);
    serde_json::json!({
        "workspace": channel,
        "payload_hex": hex_encode(&payload),
        "key_id": sig.key_id,
        "signature_hex": hex_encode(&sig.signature),
    })
    .to_string()
}

#[test]
fn a_replayed_submission_is_told_where_it_landed_not_that_it_conflicted() {
    let work = std::env::temp_dir().join(format!("choir-reject-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();

    let author = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&author.public_key_bytes()).unwrap();

    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate()).unwrap(),
    );
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
            "-X", "POST", "-d", &submit_body(&author, "alice", op),
            &format!("{api}/submit"),
        ])
    };

    let (code, first) = post(&set_main);
    assert_eq!(code, 200, "{first}");
    let (seq, hash) = (first["seq"].clone(), first["hash"].clone());
    assert!(first["already_applied"].is_null(), "first landing is not a replay");

    // Byte-identical resubmission: the same signed bytes a client would
    // send if its response were lost. CAS fails, but the honest answer is
    // "it already landed, here".
    let (code, replay) = post(&set_main);
    assert_eq!(code, 200, "a completed retry should not read as a conflict: {replay}");
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
    assert!(resp["actual"].is_string(), "conflict must name what it found: {resp}");
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
        assert!(v["next"].as_str().is_some_and(|s| !s.is_empty()), "{code:?}");
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
    assert!(partial.error.contains("stale_head"), "original text kept: {partial:?}");
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
            Code::IdentityState => 13,
            Code::PolicyUnavailable => 14,
            Code::LogEvicted => 15,
            Code::Unclassified => 16,
        }
    }
    const COUNT: usize = 17;

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
