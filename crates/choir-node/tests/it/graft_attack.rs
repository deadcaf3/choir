//! Graft attacks against invariant 4: the author signs `(channel,
//! payload)` and *nothing else*. This module attacks that boundary from
//! the outside — it never re-signs, it takes the exact `key_id` and
//! `signature_hex` bytes of a submission the author really made and
//! tries to transplant them somewhere the author never sent them.
//!
//! Two halves, and both are the point:
//!
//! - What the signature does cover is proved by rejection: moving the
//!   bytes to another channel, or under another payload, fails the
//!   check.
//! - What it does not cover is proved by acceptance. A signature carries
//!   no position, no log identity and no nonce, so the *only* thing
//!   standing between a captured op and a replay is the CAS `prev`
//!   inside the payload — which is a statement about state, not about
//!   history. Return the state and the op admits again (ABA); offer the
//!   bytes to a second node that trusts the same key and they admit
//!   there too. Both are recorded here as current behaviour, deliberately,
//!   so that a future change to what is signed shows up as a test that
//!   starts failing rather than as a design discussion nobody has.
//!
//! `key_names.rs` is the neighbouring module and a different attack: it
//! re-signs honestly on a channel the key does not own. Here the
//! signature is always genuine and always the author's.

use choir_identity::{ActorKey, Registry};
use choir_node::platform::hex_encode;
use choir_node::{Node, Platform};
use choir_oplog::{ContentHash, MemLog, Witness};
use choir_view::{OpKind, ViewOp};

use crate::support::curl;

/// A `/api/submit` body assembled from parts, so channel, payload and
/// signature can be recombined the way an attacker recombines them.
/// `support::submit_body` cannot express this: it signs what it sends.
fn grafted_body(channel: &str, payload: &[u8], sig: &Witness) -> String {
    serde_json::json!({
        "channel": channel,
        "payload_hex": hex_encode(payload),
        "key_id": sig.key_id,
        "signature_hex": hex_encode(&sig.signature),
    })
    .to_string()
}

/// Move `workspace` to `commit`, expecting it to currently be at `prev`.
fn head(workspace: &str, commit: &str, prev: Option<&str>) -> Vec<u8> {
    ViewOp::new(OpKind::SetWorkspaceHead {
        workspace: workspace.into(),
        commit: ContentHash::blake3(commit.as_bytes()),
        prev: prev.map(|p| ContentHash::blake3(p.as_bytes())),
    })
    .to_payload()
}

/// A serving node that trusts exactly `trusted`. Returns its API base
/// and the node handle to `unblock()` at the end.
fn serving_node(root: &std::path::Path, trusted: &[&ActorKey]) -> (String, std::sync::Arc<Node>) {
    let mut registry = Registry::new();
    for key in trusted {
        registry.register(&key.public_key_bytes()).unwrap();
    }
    let mut node = Node::bind(root, 0).unwrap();
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate()).unwrap(),
    );
    let port = node.port();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    (format!("http://127.0.0.1:{port}/api"), node)
}

#[test]
fn a_genuine_signature_does_not_transplant_across_channel_or_payload() {
    let work = std::env::temp_dir().join(format!("choir-graft-scope-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();

    let ana = ActorKey::generate();
    let (api, node) = serving_node(&work.join("repos"), &[&ana]);
    let post = |body: &str| curl(&["-X", "POST", "-d", body, &format!("{api}/submit")]);

    // The one op ana actually authorises: her own channel, one payload.
    let payload = head("ana/w", "c1", None);
    let sig = ana.sign_submission("ana", &payload);
    let (code, resp) = post(&grafted_body("ana", &payload, &sig));
    assert_eq!(code, 200, "{resp}");

    // Graft 1 — same bytes, another channel. The channel is inside the
    // signed tuple, so this is the attack invariant 4 exists to stop:
    // one signed op cannot become an op attributed to another actor's
    // collaboration channel.
    let (code, resp) = post(&grafted_body("bob", &payload, &sig));
    assert_eq!(code, 400, "{resp}");
    assert_eq!(resp["code"], "unknown_key", "{resp}");

    // Graft 2 — same channel and same signature, a payload ana never
    // signed. The workspace she moves and the commit she moves it to are
    // both inside the signed bytes, so neither can be substituted.
    let elsewhere = head("bob/w", "c1", None);
    let (code, resp) = post(&grafted_body("ana", &elsewhere, &sig));
    assert_eq!(code, 400, "{resp}");
    assert_eq!(resp["code"], "unknown_key", "{resp}");

    let retarget = head("ana/w", "attacker-commit", None);
    let (code, resp) = post(&grafted_body("ana", &retarget, &sig));
    assert_eq!(code, 400, "{resp}");
    assert_eq!(resp["code"], "unknown_key", "{resp}");

    // Nothing partial landed: the view holds ana's one op and neither
    // rejected graft left a trace.
    let (_, view) = curl(&[&format!("{api}/view")]);
    let workspaces = view["workspaces"].as_object().unwrap();
    assert_eq!(workspaces.len(), 1, "{view}");
    assert_eq!(
        workspaces["ana/w"],
        serde_json::json!(ContentHash::blake3(b"c1").to_hex()),
        "{view}"
    );

    // A byte-identical resubmission is a retry, not a graft, and is
    // answered as the op it already is rather than appended twice.
    let (code, resp) = post(&grafted_body("ana", &payload, &sig));
    assert_eq!(code, 200, "{resp}");
    assert_eq!(resp["already_applied"], true, "{resp}");
    assert_eq!(resp["seq"], 0, "{resp}");

    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}

#[test]
fn cas_state_is_the_whole_replay_bound_so_aba_and_a_second_node_admit_the_bytes() {
    let work = std::env::temp_dir().join(format!("choir-graft-replay-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();

    let ana = ActorKey::generate();
    let mallory = ActorKey::generate();
    let (api, node) = serving_node(&work.join("repos"), &[&ana, &mallory]);
    let post = |body: &str| curl(&["-X", "POST", "-d", body, &format!("{api}/submit")]);
    let at = |name: &str| serde_json::json!(ContentHash::blake3(name.as_bytes()).to_hex());

    // ana creates the workspace, then advances it. Mallory is on the
    // wire and keeps the second submission's exact bytes.
    let create = head("ana/w", "c1", None);
    let create_sig = ana.sign_submission("ana", &create);
    let (code, resp) = post(&grafted_body("ana", &create, &create_sig));
    assert_eq!(code, 200, "{resp}");

    let advance = head("ana/w", "c2", Some("c1"));
    let advance_sig = ana.sign_submission("ana", &advance);
    let (code, resp) = post(&grafted_body("ana", &advance, &advance_sig));
    assert_eq!(code, 200, "{resp}");

    // Replayed now, the captured op does not re-apply — the workspace is
    // at c2 and the op expects c1 — and the CAS refusal is answered as
    // the retry it looks like: same seq, `already_applied`. Note what
    // that dedup is keyed on, because the next step turns on it: the
    // signing hash of these exact bytes, consulted *only* on the CAS
    // error path.
    let (code, resp) = post(&grafted_body("ana", &advance, &advance_sig));
    assert_eq!(code, 200, "{resp}");
    assert_eq!(resp["already_applied"], true, "{resp}");
    assert_eq!(resp["seq"], 1, "{resp}");

    // Mallory rolls the state back to c1 with an op of his own. This is
    // ordinary authorised work for a trusted key: a revert.
    let revert = head("ana/w", "c1", Some("c2"));
    let revert_sig = mallory.sign_submission("mallory", &revert);
    let (code, resp) = post(&grafted_body("mallory", &revert, &revert_sig));
    assert_eq!(code, 200, "{resp}");

    // ABA. The captured bytes admit again, because a CAS `prev` asks
    // "is the state what I expect" and not "have I run before" — and the
    // retry index cannot stand in for the missing nonce, since it is
    // reached only when CAS fails. Once CAS passes, the same signing
    // hash that was answered `already_applied` a moment ago appends a
    // second, distinct entry instead. The
    // resulting entry is signed by ana and attributed to ana's channel,
    // at a sequence ana never submitted, moving her workspace at a
    // moment she did not choose. Nothing in what she signed could have
    // prevented it: a signature over (channel, payload) is by
    // construction position-independent and log-independent.
    let (code, resp) = post(&grafted_body("ana", &advance, &advance_sig));
    assert_eq!(
        code, 200,
        "replay after ABA is currently admitted; if this now fails, the signed \
         tuple or the CAS rule changed and invariant 4 needs rewriting: {resp}"
    );
    assert_ne!(resp["already_applied"], true, "{resp}");
    assert_eq!(resp["seq"], 3, "{resp}");
    let (_, view) = curl(&[&format!("{api}/view")]);
    assert_eq!(view["workspaces"]["ana/w"], at("c2"), "{view}");

    // A second node that trusts the same key is the other half of the
    // same gap: the signed tuple names no log, no node and no genesis,
    // so ana's op is equally valid everywhere her key is trusted. The
    // captured create replays onto a node ana never spoke to.
    let (other_api, other) = serving_node(&work.join("repos-2"), &[&ana]);
    let (code, resp) = curl(&[
        "-X",
        "POST",
        "-d",
        &grafted_body("ana", &create, &create_sig),
        &format!("{other_api}/submit"),
    ]);
    assert_eq!(
        code, 200,
        "an op signed for one node currently admits on any node that trusts \
         the key; binding a log id into the signed tuple is what would change \
         this: {resp}"
    );
    assert_eq!(resp["seq"], 0, "{resp}");
    let (_, other_view) = curl(&[&format!("{other_api}/view")]);
    assert_eq!(other_view["workspaces"]["ana/w"], at("c1"), "{other_view}");

    other.unblock();
    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}
