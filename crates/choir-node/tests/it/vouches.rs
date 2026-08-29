//! The vouch graph over HTTP (D65).
//!
//! The fold's rules have their own tests in `choir-view`. What only a
//! served node can show is the half the fold structurally cannot: the
//! fold is handed an op and never its signer, so `voucher` is a claim
//! until admission binds it to the channel that signed. Endorsements
//! anybody could write in anybody's name would make the graph an
//! attacker's document rather than a record.
//!
//! The other half is disclosure. Vouches are node-wide, and a profile is
//! derived from the view *this caller* may read, so a reader holding one
//! repository is told there are no vouches rather than shown the node's
//! whole social graph.

use choir_identity::{ActorKey, Registry};
use choir_node::{AuthTable, Node, Platform};
use choir_oplog::{ContentHash, MemLog};
use choir_view::{OpKind, ViewOp};

use crate::support::{curl, submit_body};

/// A served node with two users, and the node key that binds operators.
fn served(tag: &str, acl: &str) -> (String, ActorKey, ActorKey) {
    let work = std::env::temp_dir().join(format!("choir-node-vouches-{tag}"));
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
    (format!("http://127.0.0.1:{port}"), actor, node_key)
}

fn post(api: &str, user: &str, key: &ActorKey, channel: &str, op: &ViewOp) -> serde_json::Value {
    let (code, body) = curl(&[
        "-u",
        user,
        "-X",
        "POST",
        "-d",
        &submit_body(key, channel, op),
        &format!("{api}/api/submit"),
    ]);
    let mut body = body;
    body["_status"] = serde_json::json!(code);
    body
}

/// Binds `operator` to a key only this operator holds, which is what
/// every vouch below needs at both ends.
fn bind(api: &str, node_key: &ActorKey, operator: &str) {
    let op = ViewOp::new(OpKind::BindKey {
        operator: operator.into(),
        key: ContentHash::blake3(format!("{operator} key").as_bytes()),
        channel: Some(operator.into()),
    });
    let resp = post(api, "alice:a", node_key, "node/bind", &op);
    assert_eq!(resp["_status"], 200, "binding {operator}: {resp}");
}

fn vouch(voucher: &str, subject: &str, note: &str) -> ViewOp {
    ViewOp::new(OpKind::Vouch {
        voucher: voucher.into(),
        subject: subject.into(),
        note: note.into(),
    })
}

/// One request, as `(status, body)`, for the pages that are HTML.
fn get_page(url: &str, user: &str) -> (u16, String) {
    let out = std::process::Command::new("curl")
        .args(["-s", "-w", "\n%{http_code}", "-u", user, url])
        .output()
        .expect("curl runs");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let (body, code) = text.rsplit_once('\n').expect("status line");
    (code.trim().parse().expect("numeric status"), body.into())
}

/// The floor is the node's too, not only the fold's: an edge needs a
/// bound operator at each end, and once it stands it reaches the view,
/// the growth counts, the profile and the page.
#[test]
fn a_vouch_needs_bound_operators_and_then_reaches_every_reading_surface() {
    let (base, actor, node_key) = served("surfaces", "");

    // Before any binding exists, the op is refused with a code that
    // names the repair rather than a bare 400.
    let early = post(&base, "alice:a", &actor, "ana", &vouch("ana", "bob", ""));
    assert_eq!(early["_status"], 400, "{early}");
    assert_eq!(early["code"], "vouch_state", "{early}");

    bind(&base, &node_key, "ana");
    bind(&base, &node_key, "bob");
    let landed = post(
        &base,
        "alice:a",
        &actor,
        "ana",
        &vouch("ana", "bob", "shipped the parser"),
    );
    assert_eq!(landed["_status"], 200, "{landed}");

    // The view carries the edge in the direction the fold stores it,
    // and the growth report counts edges rather than subject rows.
    let (code, view) = curl(&["-u", "alice:a", &format!("{base}/api/view")]);
    assert_eq!(code, 200, "{view}");
    assert_eq!(view["vouches"]["bob"]["ana"]["note"], "shipped the parser");
    assert_eq!(view["view_growth"]["counts"]["vouch_edges"], 1, "{view}");
    assert_eq!(view["view_growth"]["counts"]["vouch_subjects"], 1, "{view}");
    // Measured, and deliberately outside the tracked authoritative
    // total: folding a fifth section in would move every past reading
    // of a series that predates this one (D64).
    let growth = &view["view_growth"]["serialized_bytes"];
    assert!(
        growth["vouches"].as_u64().unwrap_or_default() > 0,
        "the vouch section is unmeasured: {growth}"
    );
    // The profile reports the edge, names whose graph it is, and does
    // not claim a reciprocal that does not exist.
    let (code, bob) = curl(&["-u", "alice:a", &format!("{base}/api/profile?channel=bob")]);
    assert_eq!(code, 200, "{bob}");
    assert_eq!(bob["vouches"]["operator"], "bob", "{bob}");
    assert_eq!(bob["vouches"]["received"][0]["voucher"], "ana", "{bob}");
    assert_eq!(bob["vouches"]["received"][0]["reciprocal"], false, "{bob}");
    assert_eq!(bob["vouches"]["given"], 0, "{bob}");
    assert_eq!(bob["known"], true, "{bob}");

    // An agent channel reads its operator's graph. `bob/agent` is not a
    // second identity, and a page showing nothing for it would be
    // withholding the record rather than reporting it.
    let (code, agent) = curl(&[
        "-u",
        "alice:a",
        &format!("{base}/api/profile?channel=bob/agent"),
    ]);
    assert_eq!(code, 200, "{agent}");
    assert_eq!(agent["vouches"]["operator"], "bob", "{agent}");
    assert_eq!(agent["vouches"]["received"][0]["voucher"], "ana", "{agent}");

    // And `ana` is the giving end of the same edge, counted there.
    let (code, ana) = curl(&["-u", "alice:a", &format!("{base}/api/profile?channel=ana")]);
    assert_eq!(code, 200, "{ana}");
    assert_eq!(ana["vouches"]["given"], 1, "{ana}");
    assert_eq!(ana["vouches"]["received"].as_array().map(Vec::len), Some(0));

    // The page is read, not merely fetched: a green assertion on status
    // has been wrong here before, and what a reader sees is the text.
    let (code, page) = get_page(&format!("{base}/p/bob"), "alice:a");
    assert_eq!(code, 200);
    assert!(page.contains("shipped the parser"), "{page}");
    assert!(page.contains("one way"), "{page}");
    assert!(
        page.contains("Vouches are between operators"),
        "the page does not say whose graph it is: {page}"
    );
    assert!(
        !page.contains("Nothing on this node"),
        "the page still claims vouches cannot exist: {page}"
    );

    // Vouching back makes both edges mutual, on both pages.
    let back = post(
        &base,
        "bob:b",
        &actor,
        "bob",
        &vouch("bob", "ana", "and back"),
    );
    assert_eq!(back["_status"], 200, "{back}");
    let (_, page) = get_page(&format!("{base}/p/bob"), "alice:a");
    assert!(page.contains("mutual"), "{page}");
    let (_, page) = get_page(&format!("{base}/p/ana"), "alice:a");
    assert!(page.contains("mutual"), "{page}");
}

/// The claim the fold cannot check: `voucher` is payload data, so
/// without this an agent writes endorsements in any operator's name.
#[test]
fn nobody_writes_a_vouch_in_another_operators_name() {
    let (base, actor, node_key) = served("attribution", "");
    bind(&base, &node_key, "ana");
    bind(&base, &node_key, "bob");
    bind(&base, &node_key, "cy");

    // Signed on `ana`, claiming to be `bob` endorsing `cy`. Every field
    // of it is otherwise admissible, which is the point: the only thing
    // wrong is who said it.
    let forged = post(
        &base,
        "alice:a",
        &actor,
        "ana",
        &vouch("bob", "cy", "trust them"),
    );
    assert_eq!(forged["_status"], 400, "a forged vouch landed: {forged}");
    assert_eq!(forged["code"], "reviewer_mismatch", "{forged}");
    assert_eq!(forged["expected"], "ana", "{forged}");
    assert_eq!(forged["actual"], "bob", "{forged}");

    // The withdrawal side is bound the same way, and it matters more:
    // an unchecked one would let anybody delete anybody's endorsements.
    let real = post(
        &base,
        "bob:b",
        &actor,
        "bob",
        &vouch("bob", "cy", "trust them"),
    );
    assert_eq!(real["_status"], 200, "{real}");
    let stolen = post(
        &base,
        "alice:a",
        &actor,
        "ana",
        &ViewOp::new(OpKind::WithdrawVouch {
            voucher: "bob".into(),
            subject: "cy".into(),
            reason: "not yours to take back".into(),
        }),
    );
    assert_eq!(
        stolen["_status"], 400,
        "a stolen withdrawal landed: {stolen}"
    );
    assert_eq!(stolen["code"], "reviewer_mismatch", "{stolen}");
    let (code, cy) = curl(&["-u", "alice:a", &format!("{base}/api/profile?channel=cy")]);
    assert_eq!(code, 200, "{cy}");
    assert_eq!(cy["vouches"]["received"][0]["voucher"], "bob", "{cy}");

    // An agent channel of the same operator may speak for it: `bob` and
    // `bob/agent` are one identity, and requiring the bare name would
    // mean an operator's agents can never vouch at all.
    let by_agent = post(
        &base,
        "bob:b",
        &actor,
        "bob/agent",
        &vouch("bob", "ana", "same operator, different channel"),
    );
    assert_eq!(by_agent["_status"], 200, "{by_agent}");
}

/// Vouches are node-wide, so a reader granted one repository sees none
/// of them -- and is told that, in the same words as an actor who has
/// none, because the alternative discloses the graph by omission.
#[test]
fn a_per_repository_reader_is_shown_no_part_of_the_graph() {
    let (base, actor, node_key) = served(
        "disclosure",
        "alice  @node       write\nbob    agents/one  read\n",
    );
    bind(&base, &node_key, "ana");
    bind(&base, &node_key, "bob");
    let landed = post(
        &base,
        "alice:a",
        &actor,
        "ana",
        &vouch("ana", "bob", "a private note"),
    );
    assert_eq!(landed["_status"], 200, "{landed}");

    // ...and cannot write one either. `docs/operating/authorization.md`
    // tells operators that `@node auditor` is what puts somebody in the
    // web of trust, which is a claim about the ACL and so is asserted
    // here rather than believed: a vouch is node-scoped at `read`, and a
    // repository grant is not a node-wide one.
    let denied = post(
        &base,
        "bob:b",
        &actor,
        "bob",
        &vouch("bob", "ana", "let me in"),
    );
    assert_ne!(
        denied["_status"], 200,
        "a repository grant vouched: {denied}"
    );

    // The node-wide reader sees it.
    let (code, wide) = curl(&["-u", "alice:a", &format!("{base}/api/view")]);
    assert_eq!(code, 200, "{wide}");
    assert_eq!(wide["vouches"]["bob"]["ana"]["note"], "a private note");

    // The repository reader does not, in either surface, and the note
    // is nowhere in the bytes they were served.
    let (code, narrow) = curl(&["-u", "bob:b", &format!("{base}/api/view")]);
    assert_eq!(code, 200, "{narrow}");
    assert!(narrow["vouches"].is_null(), "{narrow}");
    assert!(
        !narrow.to_string().contains("a private note"),
        "the note reached a reader with no node-wide grant: {narrow}"
    );

    let (code, profile) = curl(&["-u", "bob:b", &format!("{base}/api/profile?channel=bob")]);
    assert_eq!(code, 200, "{profile}");
    assert_eq!(profile["vouches"]["operator"], "bob", "{profile}");
    assert_eq!(
        profile["vouches"]["received"].as_array().map(Vec::len),
        Some(0),
        "a narrowed profile disclosed an edge: {profile}"
    );
    // The page goes further than hiding the note. With the bindings and
    // the vouches both withheld, this reader's view holds nothing about
    // `bob` at all, so the page they get is the one any unknown name
    // gets -- identical once the heading is swapped. A reader who could
    // tell "withheld" from "no such actor" would be reading the graph
    // through the shape of the refusal.
    let (code, page) = get_page(&format!("{base}/p/bob"), "bob:b");
    assert_eq!(code, 200);
    assert!(!page.contains("a private note"), "{page}");
    let (_, stranger) = get_page(&format!("{base}/p/nobody"), "bob:b");
    assert_eq!(
        page.replace(">bob<", ">nobody<"),
        stranger,
        "a narrowed profile is distinguishable from an unknown one"
    );
    assert!(page.contains("no record"), "{page}");
}
