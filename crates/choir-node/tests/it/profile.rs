//! One actor's standing, over HTTP (D63).
//!
//! The counting is a pure function with a doctest beside it. What only a
//! served node can show is the part that would be a security bug if it
//! were wrong: a profile is derived from the view *this caller* may see,
//! so two readers asking about the same actor get different numbers, and
//! neither learns anything the ACL was withholding from them.

use choir_identity::{ActorKey, Registry};
use choir_node::{AuthTable, Node, Platform};
use choir_oplog::{ContentHash, MemLog};
use choir_view::{OpKind, Verdict, ViewOp};

use crate::support::{curl, submit_body};

/// A served node, its API base, and the keys that speak to it.
///
/// `acl` is written to a file only when non-empty, so one helper covers
/// the ungated and the gated node.
fn served(tag: &str, acl: &str) -> (String, ActorKey, ActorKey, std::path::PathBuf) {
    let work = std::env::temp_dir().join(format!("choir-node-profile-{tag}"));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();

    let actor = ActorKey::generate();
    let node_key = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&actor.public_key_bytes()).unwrap();
    registry.register(&node_key.public_key_bytes()).unwrap();

    let mut table = AuthTable::new();
    table.insert("alice".into(), "a".into());
    table.insert("bob".into(), "b".into());

    let mut node = Node::bind_with_auth(&work.join("repos"), 0, Some(table)).unwrap();
    node.create_repo("agents/one.git").unwrap();
    node.create_repo("agents/two.git").unwrap();
    if !acl.is_empty() {
        let path = work.join("acl");
        std::fs::write(&path, acl).unwrap();
        node.watch_acl_file(path).unwrap();
    }
    node.enable_platform(
        Platform::start(
            registry,
            Box::new(MemLog::new()),
            ActorKey::from_secret_bytes(&node_key.secret_bytes()),
        )
        .unwrap(),
    );
    let port = node.port();
    std::thread::spawn(move || node.serve_forever());
    (format!("http://127.0.0.1:{port}"), actor, node_key, work)
}

/// Opens a review targeting `repo` and has `who` post a verdict on it.
fn reviewed(api: &str, actor: &ActorKey, id: &str, repo: &str, who: &str, user: &str) {
    let request = ViewOp::new(OpKind::RequestReview {
        id: id.into(),
        target: ContentHash::blake3(id.as_bytes()),
        reviewers: vec![who.into()],
        target_ref: Some(format!("{repo}:refs/heads/main")),
    });
    let (code, resp) = curl(&[
        "-u",
        user,
        "-X",
        "POST",
        "-d",
        &submit_body(actor, "author", &request),
        &format!("{api}/api/submit"),
    ]);
    assert_eq!(code, 200, "opening {id}: {resp}");

    let verdict = ViewOp::new(OpKind::PostVerdict {
        id: id.into(),
        reviewer: who.into(),
        verdict: Verdict::Approve,
        note: String::new(),
    });
    let (code, resp) = curl(&[
        "-u",
        user,
        "-X",
        "POST",
        "-d",
        &submit_body(actor, who, &verdict),
        &format!("{api}/api/submit"),
    ]);
    assert_eq!(code, 200, "verdict on {id}: {resp}");
}

/// The counts are the log's, and an actor the node has never heard of is
/// told apart from one who has simply done nothing.
#[test]
fn a_profile_counts_what_the_log_records_and_says_when_it_knows_nothing() {
    let (base, actor, _node_key, _work) = served("counts", "");
    reviewed(&base, &actor, "r-1", "agents/one", "ana", "alice:a");
    reviewed(&base, &actor, "r-2", "agents/one", "ana", "alice:a");

    let (code, ana) = curl(&["-u", "alice:a", &format!("{base}/api/profile?channel=ana")]);
    assert_eq!(code, 200, "{ana}");
    assert_eq!(ana["channel"], "ana");
    assert_eq!(ana["known"], true, "{ana}");
    assert_eq!(ana["reviews"]["assigned"], 2, "{ana}");
    assert_eq!(ana["reviews"]["approved"], 2, "{ana}");
    assert_eq!(ana["reviews"]["changes_requested"], 0, "{ana}");
    // Always an object, and `operator` always present (D65): an empty
    // `received` says "nobody vouches for this operator on the records
    // you may read", which is a different sentence from "this node
    // cannot record vouches" and must not share a rendering with it.
    assert_eq!(ana["vouches"]["operator"], "ana", "{ana}");
    assert_eq!(ana["vouches"]["received"].as_array().map(Vec::len), Some(0));
    assert_eq!(ana["vouches"]["given"], 0, "{ana}");

    let (code, nobody) = curl(&[
        "-u",
        "alice:a",
        &format!("{base}/api/profile?channel=nobody"),
    ]);
    assert_eq!(code, 200, "{nobody}");
    assert_eq!(nobody["known"], false, "{nobody}");
    assert_eq!(nobody["reviews"]["assigned"], 0, "{nobody}");
}

/// The claim the whole design rests on: the profile is counted from the
/// view this caller may see, so it can never total up a review the ACL
/// withheld. Two readers, one actor, different numbers.
#[test]
fn a_profile_counts_only_what_its_reader_was_already_allowed_to_see() {
    let (base, actor, _node_key, _work) = served(
        "acl",
        "alice  agents/one  write\nalice  agents/two  write\nbob  agents/one  read\n",
    );
    reviewed(&base, &actor, "r-one", "agents/one", "ana", "alice:a");
    reviewed(&base, &actor, "r-two", "agents/two", "ana", "alice:a");

    let (code, seen_by_alice) =
        curl(&["-u", "alice:a", &format!("{base}/api/profile?channel=ana")]);
    assert_eq!(code, 200, "{seen_by_alice}");
    assert_eq!(
        seen_by_alice["reviews"]["assigned"], 2,
        "alice holds both grants: {seen_by_alice}"
    );

    let (code, seen_by_bob) = curl(&["-u", "bob:b", &format!("{base}/api/profile?channel=ana")]);
    assert_eq!(code, 200, "{seen_by_bob}");
    assert_eq!(
        seen_by_bob["reviews"]["assigned"], 1,
        "bob was granted one repository and counted two: {seen_by_bob}"
    );

    // And the profile agrees with the view it was derived from, which is
    // the property rather than the number: if these ever disagree, one of
    // the two surfaces is disclosing more than the other.
    let (_, bobs_view) = curl(&["-u", "bob:b", &format!("{base}/api/view")]);
    let in_view = bobs_view["reviews"]
        .as_object()
        .expect("the view carries reviews")
        .len();
    assert_eq!(
        seen_by_bob["reviews"]["assigned"].as_u64().unwrap() as usize,
        in_view,
        "the profile and the view disagree about what bob may see"
    );
}

/// A request with nothing to look up is refused rather than answered
/// about the empty channel, which every actor would fail to match and
/// which would therefore read as a confident "never heard of them".
#[test]
fn a_profile_without_a_channel_is_refused() {
    let (base, _actor, _node_key, _work) = served("refuse", "");
    let (code, body) = curl(&["-u", "alice:a", &format!("{base}/api/profile")]);
    assert_eq!(code, 400, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("channel"),
        "{body}"
    );
}

/// A node with no sequencer says so, rather than reporting an actor with
/// nothing to their name. There is a difference between "they have done
/// nothing" and "this node cannot tell you", and only one of them is
/// safe to act on.
#[test]
fn a_node_without_a_sequencer_refuses_rather_than_answering_zero() {
    let work = std::env::temp_dir().join("choir-node-profile-no-platform");
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let node = Node::bind(&work.join("repos"), 0).unwrap();
    let port = node.port();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }

    let (code, body) = curl(&[&format!("http://127.0.0.1:{port}/api/profile?channel=ana")]);
    assert_eq!(code, 503, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("sequencer"),
        "{body}"
    );
    node.unblock();
}

/// Binds `key` to `channel` under `operator`, so the profile has a key
/// to report an age for.
fn bind(api: &str, node_key: &ActorKey, key: &ContentHash, channel: &str, user: &str) {
    let op = ViewOp::new(OpKind::BindKey {
        operator: channel.into(),
        key: key.clone(),
        channel: Some(channel.into()),
    });
    let (code, resp) = curl(&[
        "-u",
        user,
        "-X",
        "POST",
        "-d",
        &submit_body(node_key, "node", &op),
        &format!("{api}/api/submit"),
    ]);
    assert_eq!(code, 200, "binding {channel}: {resp}");
}

/// Key age is counted in sequenced ops, and the only way to show that is
/// to sequence some and watch it move.
///
/// Two mutations walked past the first version of this file. Reporting
/// the binding's own position as its age survived every assertion, and so
/// did reporting a revoked key as live -- the first because nothing
/// pinned the number, the second because nothing ever revoked anything.
/// Both are the numbers the "no trust score, here is the input" argument
/// rests on, so they are the two that most needed pinning.
#[test]
fn key_age_grows_with_the_log_and_a_withdrawal_is_visible() {
    let (base, actor, node_key, _work) = served("age", "");
    let key = ContentHash::blake3(b"a key with a history");
    bind(&base, &node_key, &key, "mira", "alice:a");

    let age = || -> (u64, u64) {
        let (_, body) = curl(&["-u", "alice:a", &format!("{base}/api/profile?channel=mira")]);
        let row = &body["keys"][0];
        (
            row["bound_at"]
                .as_u64()
                .expect("a bound key has a position"),
            row["ops_since_binding"]
                .as_u64()
                .expect("and an age in ops"),
        )
    };

    let (bound_at, before) = age();
    // Three more ops through the sequencer. The position the key was
    // bound at cannot move -- it is assigned once -- so an age that is
    // really the position would not move either.
    for id in ["a-1", "a-2", "a-3"] {
        reviewed(&base, &actor, id, "agents/one", "mira", "alice:a");
    }
    let (still_bound_at, after) = age();
    assert_eq!(
        bound_at, still_bound_at,
        "a binding position moved, and it is assigned once"
    );
    assert!(
        after > before,
        "age did not move over {} ops: {before} then {after}",
        after.saturating_sub(before)
    );
    // Six ops: three reviews opened, three verdicts posted.
    assert_eq!(
        after - before,
        6,
        "age is counted in sequenced ops, and six were sequenced"
    );

    // Withdrawing the binding is visible on both surfaces. The row stays
    // -- attribution for what the key already did has to survive -- so
    // "still listed" is not evidence that it is still good.
    let revoke = ViewOp::new(OpKind::RevokeKey {
        key,
        reason: "rotated".into(),
    });
    let (code, resp) = curl(&[
        "-u",
        "alice:a",
        "-X",
        "POST",
        "-d",
        &submit_body(&node_key, "node", &revoke),
        &format!("{base}/api/submit"),
    ]);
    assert_eq!(code, 200, "revoking: {resp}");

    let (_, body) = curl(&["-u", "alice:a", &format!("{base}/api/profile?channel=mira")]);
    assert!(
        !body["keys"][0]["revoked"].is_null(),
        "a withdrawn binding still reads as live: {body}"
    );
    let out = std::process::Command::new("curl")
        .args(["-s", "-u", "alice:a", &format!("{base}/p/mira")])
        .output()
        .expect("curl runs");
    let page = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        page.contains("revoked"),
        "the page still shows it live: {page}"
    );
    assert!(
        !page.contains(">live<"),
        "the page shows both states at once: {page}"
    );
}

/// The page and the endpoint are one reading of one view.
///
/// Asserted as agreement rather than as two lists of expected strings,
/// because the failure worth catching is not a missing number but the
/// two surfaces counting differently — which on this page would be a
/// disclosure and not a typo.
#[test]
fn the_page_and_the_endpoint_agree_about_the_same_actor() {
    let (base, actor, node_key, _work) = served("page", "");
    // A bound key, so the age table has a row. Registering a key with
    // the sequencer is not binding it: the first says a signature will
    // verify, the second is the record a profile reports an age from,
    // and only the second reaches this page.
    bind(
        &base,
        &node_key,
        &ContentHash::blake3(b"ana key"),
        "ana",
        "alice:a",
    );
    reviewed(&base, &actor, "r-p1", "agents/one", "ana", "alice:a");

    let out = std::process::Command::new("curl")
        .args(["-s", "-u", "alice:a", &format!("{base}/p/ana")])
        .output()
        .expect("curl runs");
    let page = String::from_utf8_lossy(&out.stdout).to_string();

    let (_, json) = curl(&["-u", "alice:a", &format!("{base}/api/profile?channel=ana")]);
    let assigned = json["reviews"]["assigned"].as_u64().unwrap();
    assert_eq!(assigned, 1, "{json}");

    assert!(page.contains("<title>ana</title>"), "{page}");
    assert!(page.contains("reviews assigned"), "{page}");
    // The row's number, beside its label, rather than anywhere on the
    // page: `1` alone would match the markup in a dozen places.
    assert!(
        page.contains(&format!(
            "<tr><td>reviews assigned</td><td>{assigned}</td></tr>"
        )),
        "the page does not carry the count the endpoint reported: {page}"
    );
    // The unit is on the page, because the number is meaningless without
    // it and misleading with the wrong one.
    assert!(page.contains("sequenced ops"), "{page}");

    // A name with nothing behind it, and a name that is not one, are the
    // same refusal. A reader is never told which they hit.
    let refusal = |path: &str| {
        let out = std::process::Command::new("curl")
            .args([
                "-s",
                "-w",
                "\n%{http_code}",
                "-u",
                "alice:a",
                &format!("{base}{path}"),
            ])
            .output()
            .expect("curl runs");
        String::from_utf8_lossy(&out.stdout).to_string()
    };
    let stranger = refusal("/p/nobody-at-all");
    assert!(stranger.contains("no record"), "{stranger}");
    let malformed = refusal("/p/");
    assert!(malformed.trim().ends_with("404"), "{malformed}");
}
