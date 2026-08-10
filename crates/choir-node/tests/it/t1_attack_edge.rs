//! D24 T1 attack-edge rehearsal.
//!
//! Choir does not yet persist key age, vouches, scoped privilege grants or
//! bonds, so the numeric T1 tripwire is not measurable. This test fixes the
//! boundary that does exist: operator registration admits a fresh key
//! immediately, while an existing protected ref still needs one exact review
//! carrying approval weight from two distinct operators.

use choir_identity::{ActorKey, Registry};
use choir_node::platform::{hex_decode, hex_encode};
use choir_node::{Node, Platform};
use choir_oplog::{ContentHash, MemLog};
use choir_view::{OpKind, Verdict, ViewOp};

use crate::support::curl;

use crate::support::submit_body_legacy as submit_body;

fn trusted_line(channel: &str, key: &ActorKey) -> String {
    format!("{channel} {}\n", hex_encode(&key.public_key_bytes()))
}

#[test]
fn fresh_key_is_admitted_by_registration_not_age_or_vouches() {
    let work = std::env::temp_dir().join(format!("choir-t1-edge-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let keys_file = work.join("keys");
    let pool_file = work.join("reviewers");
    let refs_file = work.join("protected-refs");

    let fresh = ActorKey::generate();
    let reviewer_a = ActorKey::generate();
    let reviewer_b = ActorKey::generate();
    let fresh_channel = "requester/agent";
    let reviewers = [
        ("reviewer-a/agent", &reviewer_a),
        ("reviewer-b/agent", &reviewer_b),
    ];
    let initial_keys = reviewers
        .iter()
        .map(|(channel, key)| trusted_line(channel, key))
        .collect::<String>();
    std::fs::write(&keys_file, &initial_keys).unwrap();
    std::fs::write(&pool_file, "reviewer-a/agent\nreviewer-b/agent\n").unwrap();
    let protected_ref = "repo.git:refs/heads/main";
    std::fs::write(&refs_file, format!("{protected_ref}\n")).unwrap();

    let mut registry = Registry::new();
    registry.register(&reviewer_a.public_key_bytes()).unwrap();
    registry.register(&reviewer_b.public_key_bytes()).unwrap();
    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
    node.enable_platform(
        Platform::start_reloading(
            registry,
            Box::new(MemLog::new()),
            ActorKey::generate(),
            Some(keys_file.clone()),
        )
        .unwrap()
        .with_reviewer_pool(pool_file)
        .with_protected_refs(refs_file)
        .with_required_review(),
    );
    let port = node.port();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    let api = format!("http://127.0.0.1:{port}/api");
    let submit = |key: &ActorKey, channel: &str, op: &ViewOp| {
        curl(&[
            "-X",
            "POST",
            "-d",
            &submit_body(key, channel, op),
            &format!("{api}/submit"),
        ])
    };

    let feature = ViewOp::new(OpKind::SetRef {
        name: "repo.git:refs/heads/feature".into(),
        commit: ContentHash::blake3(b"fresh feature"),
        prev: None,
    });
    let (code, resp) = submit(&fresh, fresh_channel, &feature);
    assert_eq!(code, 400, "an unregistered key wrote a ref: {resp}");
    assert_eq!(resp["code"], "unknown_key", "{resp}");

    // Registration is the whole standing key gate today. There is no age,
    // vouch, activity or bond record to wait for or evaluate.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(
        &keys_file,
        format!("{initial_keys}{}", trusted_line(fresh_channel, &fresh)),
    )
    .unwrap();
    let (code, resp) = submit(&fresh, fresh_channel, &feature);
    assert_eq!(code, 200, "registered fresh key was not admitted: {resp}");
    assert_eq!(
        resp["seq"], 0,
        "its first accepted activity must be this write"
    );

    // A protected ref can be created immediately because there is no prior
    // history to hijack. Advancing that ref is a separate, per-change gate.
    let base = ContentHash::blake3(b"protected base");
    let candidate = ContentHash::blake3(b"protected candidate");
    let create = ViewOp::new(OpKind::SetRef {
        name: protected_ref.into(),
        commit: base.clone(),
        prev: None,
    });
    assert_eq!(submit(&fresh, fresh_channel, &create).0, 200);
    let advance = ViewOp::new(OpKind::SetRef {
        name: protected_ref.into(),
        commit: candidate.clone(),
        prev: Some(base),
    });
    let (code, resp) = submit(&fresh, fresh_channel, &advance);
    assert_eq!(code, 400, "fresh key bypassed protected landing: {resp}");
    assert_eq!(resp["code"], "review_required", "{resp}");

    let review = ViewOp::new(OpKind::RequestReview {
        id: "t1-exact-review".into(),
        target: candidate.clone(),
        reviewers: Vec::new(),
        target_ref: Some(protected_ref.into()),
    });
    let (code, resp) = submit(&fresh, fresh_channel, &review);
    assert_eq!(code, 200, "{resp}");
    let drawn: Vec<String> = serde_json::from_value(resp["reviewers"].clone()).unwrap();
    assert_eq!(drawn.len(), 2, "{resp}");
    for reviewer in drawn {
        let key = reviewers
            .iter()
            .find_map(|(channel, key)| (*channel == reviewer).then_some(*key))
            .expect("drawn reviewer has a bound key");
        let verdict = ViewOp::new(OpKind::PostVerdict {
            id: "t1-exact-review".into(),
            reviewer: reviewer.clone(),
            verdict: Verdict::Approve,
            note: String::new(),
        });
        let (code, resp) = submit(key, &reviewer, &verdict);
        assert_eq!(code, 200, "{resp}");
    }
    let (code, resp) = submit(&fresh, fresh_channel, &advance);
    assert_eq!(code, 200, "exact approved advance was refused: {resp}");

    let (_, view) = curl(&[&format!("{api}/view")]);
    assert_eq!(view["refs"][protected_ref], candidate.to_hex(), "{view}");

    // Verify the activity claim from the signed log, not from request order:
    // the first accepted entry authored by this key is exactly the feature
    // write above, at genesis sequence zero.
    let (_, log) = curl(&[&format!("{api}/log?from=0")]);
    let authored: Vec<&serde_json::Value> = log["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["author_key"] == fresh.actor_id().to_hex())
        .collect();
    assert_eq!(authored[0]["seq"], 0, "{log}");
    let payload = hex_decode(authored[0]["payload_hex"].as_str().unwrap()).unwrap();
    assert_eq!(ViewOp::from_payload(&payload).unwrap(), feature);

    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}
