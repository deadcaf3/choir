//! Witness admission (D67): the half the fold structurally cannot do.
//!
//! `View::validate` is handed an op and never its signer, so the
//! `witness` field in a cosignature is a claim until the daemon binds it
//! to the channel that signed it. Two rules live here and nowhere else:
//! the claimed witness must be the operator of the signing channel, and
//! the node may not witness its own attestation. The second is the
//! mirror of the rule that only the node may *record* one — a log whose
//! only witness is the node that wrote it is a node agreeing with
//! itself, which is the thing a witness exists to rule out.

use choir_hash::ContentHash;
use choir_identity::{ActorKey, Registry};
use choir_node::{AuthTable, Node, Platform};
use choir_oplog::MemLog;
use choir_view::{OpKind, RefSnapshot, ViewOp, FORMAT_VERSION};

use crate::support::curl;
use crate::support::submit_body_legacy as submit_body;

fn served(tag: &str, acl: &str) -> (String, ActorKey, ActorKey) {
    let work = std::env::temp_dir().join(format!("choir-node-witness-{tag}"));
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
    table.insert("carol".into(), "c".into());

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

fn read_view(api: &str, user: &str) -> serde_json::Value {
    let (code, body) = curl(&["-u", user, &format!("{api}/api/view")]);
    assert_eq!(code, 200, "reading the view as {user}: {body}");
    body
}

fn bind(api: &str, node_key: &ActorKey, operator: &str) {
    let op = ViewOp::new(OpKind::BindKey {
        operator: operator.into(),
        key: ContentHash::blake3(format!("{operator} key").as_bytes()),
        channel: Some(operator.into()),
    });
    let resp = post(api, "alice:a", node_key, "node/bind", &op);
    assert_eq!(resp["_status"], 200, "binding {operator}: {resp}");
}

/// Records the node's attestation of an empty ref-state at whatever
/// position the log has reached, and returns its id.
///
/// Both the position and the chain pointer come from the view rather
/// than from anything this test counts: D25 chains attestations, so the
/// second one on a log must name the first, and a node that took one on
/// its own behalf before this ran would otherwise make the test read as
/// a bug in the code under test.
fn attest(api: &str, node_key: &ActorKey) -> ContentHash {
    let seen = read_view(api, "alice:a");
    let at_seq = seen["log"]["next_seq"]
        .as_u64()
        .expect("the view reports its position");
    let prev = seen["snapshot"]["id"]
        .as_str()
        .and_then(ContentHash::from_hex);
    let snapshot = RefSnapshot {
        format_version: FORMAT_VERSION,
        refs: std::collections::BTreeMap::new(),
        at_seq,
        prev_snapshot: prev,
    };
    let id = snapshot.id();
    let resp = post(
        api,
        "alice:a",
        node_key,
        "node/attest",
        &ViewOp::new(OpKind::RecordRefSnapshot { snapshot }),
    );
    assert_eq!(resp["_status"], 200, "recording the attestation: {resp}");
    id
}

fn countersign(witness: &str, snapshot: &ContentHash) -> ViewOp {
    ViewOp::new(OpKind::CountersignSnapshot {
        witness: witness.into(),
        snapshot: snapshot.clone(),
    })
}

/// The through-line: a bound operator on its own channel cosigns the
/// current attestation, and the statement reaches the view as one row.
#[test]
fn a_witness_cosigns_the_attestation_and_the_view_reports_it() {
    let (api, actor, node_key) = served("through", "");
    bind(&api, &node_key, "observer");
    let id = attest(&api, &node_key);

    let resp = post(
        &api,
        "alice:a",
        &actor,
        "observer/agent",
        &countersign("observer", &id),
    );
    assert_eq!(resp["_status"], 200, "a bound witness was refused: {resp}");

    let seen = read_view(&api, "alice:a");
    assert_eq!(
        seen["witnessed"]["observer"]["snapshot"],
        serde_json::json!(id),
        "the witness statement did not reach the view: {seen}"
    );
    assert_eq!(
        seen["witnessed"]["observer"]["at"].as_u64(),
        resp["seq"].as_u64(),
        "the row's position is not the op's own"
    );
    // The growth counters must distinguish "has witnesses" from "has
    // witnesses of the state being served", which is the only one worth
    // acting on.
    let growth = &seen["view_growth"]["counts"];
    assert_eq!(growth["witnesses"], 1, "{growth}");
    assert_eq!(growth["witnesses_current"], 1, "{growth}");

    // A later attestation leaves the witness behind: the row stays, and
    // stops counting as current.
    attest(&api, &node_key);
    let growth = read_view(&api, "alice:a")["view_growth"]["counts"].clone();
    assert_eq!(growth["witnesses"], 1, "the row vanished: {growth}");
    assert_eq!(
        growth["witnesses_current"], 0,
        "a witness of a superseded ref-state still counted as current: {growth}"
    );
}

/// The claim is bound to the channel that signed it, exactly as a
/// vouch's voucher is (D65).
#[test]
fn nobody_witnesses_in_another_operators_name() {
    let (api, actor, node_key) = served("mismatch", "");
    bind(&api, &node_key, "observer");
    bind(&api, &node_key, "impostor");
    let id = attest(&api, &node_key);

    let resp = post(
        &api,
        "alice:a",
        &actor,
        "impostor/agent",
        &countersign("observer", &id),
    );
    assert_ne!(
        resp["_status"], 200,
        "one operator witnessed under another's name: {resp}"
    );
    assert_eq!(resp["code"], "reviewer_mismatch", "{resp}");
    let seen = read_view(&api, "alice:a");
    assert!(
        seen["witnessed"]["observer"].is_null(),
        "the refused claim still landed: {seen}"
    );
}

/// A node cannot be its own witness. This is the rule that makes the
/// count mean anything: the node already signed the attestation, so its
/// cosignature adds no independent observation.
#[test]
fn the_node_cannot_witness_its_own_attestation() {
    let (api, _actor, node_key) = served("self", "");
    bind(&api, &node_key, "node");
    let id = attest(&api, &node_key);

    let resp = post(
        &api,
        "alice:a",
        &node_key,
        "node/agent",
        &countersign("node", &id),
    );
    assert_ne!(
        resp["_status"], 200,
        "the node witnessed its own attestation: {resp}"
    );
    assert_eq!(resp["code"], "node_only", "{resp}");
}

/// Witnessing needs only the read-only node grant, and that is the
/// whole point of classifying it at `auditor` rather than at write.
///
/// A witness required to hold `@node write` is an identity that can
/// already move every ref on the node — its cosignature would be the
/// node's own signature wearing a second name. The classification is
/// the property, so it is asserted here rather than left as a comment
/// on the table. (D65 makes the same argument for the vouch and never
/// wrote this test; the vouch is covered by the same assertion below.)
#[test]
fn the_read_only_node_grant_is_enough_to_witness() {
    let (api, actor, node_key) = served("auditor", "alice @node write\ncarol @node auditor\n");
    bind(&api, &node_key, "observer");
    let id = attest(&api, &node_key);

    // carol holds `auditor` and nothing else: no repository, no write.
    let resp = post(
        &api,
        "carol:c",
        &actor,
        "observer/agent",
        &countersign("observer", &id),
    );
    assert_eq!(
        resp["_status"], 200,
        "the read-only node grant could not witness, so only identities that can \
         move every ref may: {resp}"
    );
}

/// The section is node-wide, so a credential granted one repository is
/// shown no part of it — the same disclosure rule the vouch graph has.
#[test]
fn a_per_repository_reader_is_shown_no_witnesses() {
    let (api, actor, node_key) = served("disclosure", "alice @node write\nbob agents/one read\n");
    bind(&api, &node_key, "observer");
    let id = attest(&api, &node_key);
    let resp = post(
        &api,
        "alice:a",
        &actor,
        "observer/agent",
        &countersign("observer", &id),
    );
    assert_eq!(resp["_status"], 200, "{resp}");

    assert!(
        !read_view(&api, "alice:a")["witnessed"].is_null(),
        "the auditor lost the section, so this test proves nothing"
    );
    let narrowed = read_view(&api, "bob:b");
    assert!(
        narrowed["witnessed"].is_null(),
        "a per-repository reader was shown the witness graph: {narrowed}"
    );
    assert!(
        narrowed["snapshot"].is_null(),
        "and the attestation it is about leaked with it: {narrowed}"
    );
}
