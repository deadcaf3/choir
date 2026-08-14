//! Name→key binding (the last D24 layer-5 gap): a trusted-keys line may
//! carry the channel name its holder speaks as, and review ops from a
//! bound key are refused on any other channel. Keys with no name stay
//! unconstrained, so the column is additive in behaviour as well as in
//! format.

use choir_identity::{ActorKey, Registry};
use choir_node::platform::hex_encode;
use choir_node::{Node, Platform};
use choir_oplog::MemLog;
use choir_view::{OpKind, Verdict, ViewOp};

use crate::support::curl;

use crate::support::submit_body_legacy as submit_body;

#[test]
fn parse_keys_file_accepts_both_line_shapes_and_rejects_a_shared_name() {
    let work = std::env::temp_dir().join(format!("choir-keynames-parse-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let path = work.join("keys");

    let (a, b) = (ActorKey::generate(), ActorKey::generate());
    let (ha, hb) = (
        hex_encode(&a.public_key_bytes()),
        hex_encode(&b.public_key_bytes()),
    );

    // Old shape and new shape in one file: a file written before the name
    // column existed keeps parsing, and its keys stay unbound.
    std::fs::write(&path, format!("# keys\n{ha}\nbot {hb}\n")).unwrap();
    let parsed = choir_node::parse_keys_file(&path).unwrap();
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].name, None, "a bare hex line must stay unbound");
    assert_eq!(parsed[1].name.as_deref(), Some("bot"));
    // The principal is derived from the key, never the name — otherwise
    // adding a name column would silently reattribute git pushes.
    assert_eq!(parsed[1].actor_id, b.actor_id().to_hex());

    // One name may not cover two keys: either holder could then speak as
    // that channel, which is the thing the binding exists to prevent.
    std::fs::write(&path, format!("bot {ha}\nbot {hb}\n")).unwrap();
    let err = choir_node::parse_keys_file(&path).unwrap_err();
    assert!(err.to_string().contains("more than one key"), "{err}");

    // A malformed line still fails the whole parse.
    std::fs::write(&path, format!("{ha}\nbot not-hex\n")).unwrap();
    assert!(choir_node::parse_keys_file(&path).is_err());

    std::fs::remove_dir_all(&work).ok();
}

#[test]
fn a_bound_key_cannot_speak_as_another_channel() {
    let work = std::env::temp_dir().join(format!("choir-keynames-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let keys_file = work.join("keys");

    let ana = ActorKey::generate();
    let unbound = ActorKey::generate();
    std::fs::write(
        &keys_file,
        format!(
            "ana {}\n{}\n",
            hex_encode(&ana.public_key_bytes()),
            hex_encode(&unbound.public_key_bytes())
        ),
    )
    .unwrap();

    let mut registry = Registry::new();
    registry.register(&ana.public_key_bytes()).unwrap();
    registry.register(&unbound.public_key_bytes()).unwrap();

    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
    node.enable_platform(
        Platform::start_reloading(
            registry,
            Box::new(MemLog::new()),
            ActorKey::generate(),
            Some(keys_file.clone()),
        )
        .unwrap(),
    );
    let port = node.port();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    let api = format!("http://127.0.0.1:{port}/api");
    let post = |key: &ActorKey, channel: &str, op: &ViewOp| {
        curl(&[
            "-X", "POST", "-d", &submit_body(key, channel, op),
            &format!("{api}/submit"),
        ])
    };
    let request = |id: &str, reviewers: &[&str]| {
        ViewOp::new(OpKind::RequestReview {
            id: id.into(),
            target: choir_oplog::ContentHash::blake3(id.as_bytes()),
            reviewers: reviewers.iter().map(|r| (*r).to_string()).collect(),
            target_ref: None,
        })
    };

    // ana's key on ana's channel: fine.
    let (code, resp) = post(&ana, "ana", &request("k-1", &["bot"]));
    assert_eq!(code, 200, "{resp}");

    // ana's key claiming to be bot: refused, and the error says which
    // name the key actually holds rather than a generic denial.
    let (code, resp) = post(&ana, "bot", &request("k-2", &["ana"]));
    assert_eq!(code, 400, "{resp}");
    assert_eq!(resp["code"], "channel_not_owned", "{resp}");
    let err = resp["error"].as_str().unwrap();
    assert!(err.contains("bound to a different channel"), "{resp}");
    // Which name, structurally, rather than parsed out of prose.
    assert_eq!(resp["expected"], "ana", "{resp}");
    assert_eq!(resp["actual"], "bot", "{resp}");

    // The refusal happened in policy, so nothing landed in the view.
    let (_, view) = curl(&[&format!("{api}/view")]);
    assert!(view["reviews"].get("k-2").is_none(), "{view}");

    // Verdicts reach the same channel-ownership check, and this case is
    // *not* the one that shows it. The unbound key answering as ana is
    // refused because ana is not a reviewer of k-1, which is review
    // state and not identity: `channel_is_owned` refuses a key bound to
    // *another* name, and says nothing about a key bound to no name at
    // all. Asserting the code rather than the status keeps that
    // distinction visible; the comment case below is the one that
    // exercises ownership, with a bound key claiming somebody else.
    let verdict = |id: &str, who: &str| {
        ViewOp::new(OpKind::PostVerdict {
            id: id.into(),
            reviewer: who.into(),
            verdict: Verdict::Approve,
            note: String::new(),
        })
    };
    let (code, resp) = post(&unbound, "ana", &verdict("k-1", "ana"));
    assert_eq!(code, 400, "{resp}");
    assert_eq!(resp["code"], "review_state", "{resp}");

    // And comments (D38), for the sharper version of the same reason: a
    // verdict in the wrong name is a wrong authorization, a comment in
    // the wrong name is words somebody never said. The author field
    // matching the channel proves only that the claim is self-consistent.
    let (code, resp) = post(
        &ana,
        "bot",
        &ViewOp::new(OpKind::PostComment {
            id: "k-1".into(),
            comment: "c1".into(),
            author: "bot".into(),
            body: "I withdraw my objection".into(),
        }),
    );
    assert_eq!(code, 400, "{resp}");
    assert_eq!(resp["code"], "channel_not_owned", "{resp}");
    let (_, view) = curl(&[&format!("{api}/view")]);
    assert!(
        view["reviews"]["k-1"]["comments"].as_array().unwrap().is_empty(),
        "a comment survived a refused submission: {view}"
    );

    // An unbound key on its own unclaimed channel is unconstrained —
    // exactly the behaviour every key had before the name column, which
    // is why adding the column cannot break a running node.
    let (code, resp) = post(&unbound, "carol", &request("k-3", &["ana"]));
    assert_eq!(code, 200, "{resp}");

    // Binding takes effect on the existing hot-reload path: give the
    // unbound key a name, and its old channel stops working. The reload
    // trigger is a failed signature check, so a rejected submission from
    // an unknown key is what re-reads the file.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(
        &keys_file,
        format!(
            "ana {}\ncarol {}\n",
            hex_encode(&ana.public_key_bytes()),
            hex_encode(&unbound.public_key_bytes())
        ),
    )
    .unwrap();
    let stranger = ActorKey::generate();
    let (code, _) = post(&stranger, "nobody", &request("k-x", &[]));
    assert_eq!(code, 400, "unknown key must be refused (and trigger reload)");

    let (code, resp) = post(&unbound, "dave", &request("k-4", &["ana"]));
    assert_eq!(code, 400, "{resp}");
    assert_eq!(resp["expected"], "carol", "{resp}");
    let (code, resp) = post(&unbound, "carol", &request("k-5", &["ana"]));
    assert_eq!(code, 200, "{resp}");

    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}

#[test]
fn binding_a_name_takes_effect_without_waiting_for_a_failure() {
    // A binding is a *tightening*, so it must not depend on the policy's
    // failed-signature reload trigger — that fires at an unpredictable
    // future moment, and a gate that applies unpredictably is not a gate.
    // The accept loop stats the keys file between receiving a request and
    // handling it, so the very next request is already gated.
    let work = std::env::temp_dir().join(format!("choir-keynames-watch-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let keys_file = work.join("keys");

    let agent = ActorKey::generate();
    let hex = hex_encode(&agent.public_key_bytes());
    std::fs::write(&keys_file, format!("{hex}\n")).unwrap();

    let mut registry = Registry::new();
    registry.register(&agent.public_key_bytes()).unwrap();

    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
    node.enable_platform(
        Platform::start_reloading(
            registry,
            Box::new(MemLog::new()),
            ActorKey::generate(),
            Some(keys_file.clone()),
        )
        .unwrap(),
    );
    node.watch_keys_file(keys_file.clone());
    let port = node.port();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    let api = format!("http://127.0.0.1:{port}/api");
    let request = |id: &str| {
        ViewOp::new(OpKind::RequestReview {
            id: id.into(),
            target: choir_oplog::ContentHash::blake3(id.as_bytes()),
            reviewers: vec!["someone".into()],
            target_ref: None,
        })
    };
    let post = |channel: &str, op: &ViewOp| {
        curl(&[
            "-X", "POST", "-d", &submit_body(&agent, channel, op),
            &format!("{api}/submit"),
        ])
    };

    // Unbound: any channel works.
    let (code, resp) = post("whoever", &request("w-1"));
    assert_eq!(code, 200, "{resp}");

    // Bind it. mtime granularity is coarse, so make the edit land in a
    // later second than the file's creation.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(&keys_file, format!("agent {hex}\n")).unwrap();

    // No failed signature in between — the next request is already gated.
    let (code, resp) = post("whoever", &request("w-2"));
    assert_eq!(code, 400, "binding did not take effect: {resp}");
    assert_eq!(resp["expected"], "agent", "{resp}");

    // And the bound channel works.
    let (code, resp) = post("agent", &request("w-3"));
    assert_eq!(code, 200, "{resp}");

    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}
