//! D24 T3 concentration instrumentation. The node reports exact integer
//! counts and ratios from bound identities; it never assigns an unknown
//! Git transport identity to an operator merely because the channel has
//! an `operator/agent`-looking spelling.

use choir_identity::{ActorKey, Registry};
use choir_node::platform::hex_encode;
use choir_node::{parse_keys_file, Platform};
use choir_oplog::{ContentHash, FileLog, MemLog};
use choir_view::{OpKind, Verdict, ViewOp};

fn submit(platform: &Platform, key: &ActorKey, channel: &str, op: ViewOp) -> serde_json::Value {
    let payload = op.to_payload();
    let sig = key.sign_submission(channel, &payload);
    let (status, body) = platform.handle_api(
        "POST",
        "/api/submit",
        serde_json::json!({
            "channel": channel,
            "payload_hex": hex_encode(&payload),
            "key_id": sig.key_id,
            "signature_hex": hex_encode(&sig.signature),
        })
        .to_string()
        .as_bytes(),
    );
    assert_eq!(status, 200, "{body}");
    serde_json::from_str(&body).expect("submit response is json")
}

fn view(platform: &Platform) -> serde_json::Value {
    let (status, body) = platform.handle_api("GET", "/api/view", b"");
    assert_eq!(status, 200, "{body}");
    serde_json::from_str(&body).expect("view response is json")
}

fn set_ref(name: &str, commit: ContentHash, prev: Option<ContentHash>) -> ViewOp {
    ViewOp::new(OpKind::SetRef {
        name: name.into(),
        commit,
        prev,
    })
}

#[test]
fn exact_t3_boundaries_count_unknown_ownership_in_the_denominator() {
    let work = std::env::temp_dir().join(format!(
        "choir-concentration-boundaries-{}",
        std::process::id()
    ));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let keys_file = work.join("keys");

    let alpha = ActorKey::generate();
    let node_key = ActorKey::generate();
    let mut lines = format!("alpha/agent {}\n", hex_encode(&alpha.public_key_bytes()));
    let mut extra_keys = Vec::new();
    for i in 0..99 {
        let key = ActorKey::generate();
        lines.push_str(&format!(
            "many/agent-{i:03} {}\n",
            hex_encode(&key.public_key_bytes())
        ));
        extra_keys.push(key);
    }
    std::fs::write(&keys_file, &lines).unwrap();

    let mut registry = Registry::new();
    registry.register(&alpha.public_key_bytes()).unwrap();
    let platform = Platform::start_reloading(
        registry,
        Box::new(MemLog::new()),
        ActorKey::from_secret_bytes(&node_key.secret_bytes()),
        Some(keys_file.clone()),
    )
    .unwrap();

    // One bound branch among 100 total is exactly 1%, which does not
    // cross T3's strict `>1%` tripwire. The other 99 are node-signed and
    // deliberately remain unattributed.
    let first = ContentHash::blake3(b"bound branch");
    submit(
        &platform,
        &alpha,
        "alpha/agent",
        set_ref("repo.git:refs/heads/alpha-0", first, None),
    );
    for i in 0..99 {
        submit(
            &platform,
            &node_key,
            "git/transport",
            set_ref(
                &format!("repo.git:refs/heads/unattributed-{i:03}"),
                ContentHash::blake3(format!("unknown {i}").as_bytes()),
                None,
            ),
        );
    }
    let before = view(&platform);
    let concentration = &before["concentration"];
    assert_eq!(concentration["as_of_seq"], 99);
    assert_eq!(concentration["totals"]["active_branches"], 100);
    assert_eq!(concentration["totals"]["unattributed_active_branches"], 99);
    assert_eq!(concentration["operators"]["alpha"]["active_branches"], 1);
    assert_eq!(
        concentration["operators"]["alpha"]["active_branch_share_basis_points"],
        100
    );
    assert_eq!(
        concentration["operators"]["alpha"]["tripwires"]["active_branch_share"],
        false
    );
    assert_eq!(concentration["tripwire_status"], "indeterminate");

    // Two of 101 is greater than 1%, so the same exact comparison must
    // flip. Counting only attributable branches in the denominator would
    // report 100%, and this assertion would catch that flattering lie.
    submit(
        &platform,
        &alpha,
        "alpha/agent",
        set_ref(
            "repo.git:refs/heads/alpha-1",
            ContentHash::blake3(b"second bound branch"),
            None,
        ),
    );
    let after = view(&platform);
    let alpha_row = &after["concentration"]["operators"]["alpha"];
    assert_eq!(after["concentration"]["as_of_seq"], 100);
    assert_eq!(after["concentration"]["totals"]["active_branches"], 101);
    assert_eq!(alpha_row["active_branches"], 2);
    assert_eq!(alpha_row["active_branch_share_basis_points"], 198);
    assert_eq!(alpha_row["tripwires"]["active_branch_share"], true);

    // T3's key threshold is strict too: 100 is allowed, 101 trips. The
    // effective map is one currently bound name per distinct actor id.
    let many = &after["concentration"]["operators"]["many"];
    assert_eq!(many["agent_keys"], 99);
    assert_eq!(many["tripwires"]["agent_keys"], false);
    let hundredth = ActorKey::generate();
    lines.push_str(&format!(
        "many/agent-099 {}\n",
        hex_encode(&hundredth.public_key_bytes())
    ));
    std::fs::write(&keys_file, &lines).unwrap();
    platform.set_key_names(&parse_keys_file(&keys_file).unwrap());
    let at_limit = view(&platform);
    let many = &at_limit["concentration"]["operators"]["many"];
    assert_eq!(many["agent_keys"], 100);
    assert_eq!(many["tripwires"]["agent_keys"], false);

    let hundred_and_first = ActorKey::generate();
    lines.push_str(&format!(
        "many/agent-100 {}\n",
        hex_encode(&hundred_and_first.public_key_bytes())
    ));
    std::fs::write(&keys_file, lines).unwrap();
    platform.set_key_names(&parse_keys_file(&keys_file).unwrap());
    let final_view = view(&platform);
    let many = &final_view["concentration"]["operators"]["many"];
    assert_eq!(many["agent_keys"], 101);
    assert_eq!(many["tripwires"]["agent_keys"], true);

    // Raw JSON order is part of the report contract: stable output keeps
    // diffs and downstream prompt caches useful, not merely pretty.
    let (_, first_body) = platform.handle_api("GET", "/api/view", b"");
    let (_, second_body) = platform.handle_api("GET", "/api/view", b"");
    assert_eq!(first_body, second_body);
    let operator_json = first_body.split_once("\"operators\":{").unwrap().1;
    assert!(
        operator_json.find("\"alpha\":").unwrap()
            < operator_json.find("\"many\":").unwrap(),
        "operator rows must be lexicographically ordered: {first_body}"
    );

    // Keep generated keys alive until after their public bytes have been
    // consumed; their secrets never leave memory.
    assert_eq!(extra_keys.len(), 99);
    std::fs::remove_dir_all(&work).ok();
}

#[test]
fn a_cross_operator_key_binding_is_ambiguous_not_conveniently_assigned() {
    let work = std::env::temp_dir().join(format!(
        "choir-concentration-ambiguous-{}",
        std::process::id()
    ));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let keys_file = work.join("keys");
    let key = ActorKey::generate();
    let public = hex_encode(&key.public_key_bytes());
    // The parser preserves this legacy shape. T3 must not let file order
    // choose which operator receives the key or its ref activity.
    std::fs::write(
        &keys_file,
        format!("alpha/agent {public}\nbeta/agent {public}\n"),
    )
    .unwrap();

    let mut registry = Registry::new();
    registry.register(&key.public_key_bytes()).unwrap();
    let platform = Platform::start_reloading(
        registry,
        Box::new(MemLog::new()),
        ActorKey::generate(),
        Some(keys_file),
    )
    .unwrap();
    submit(
        &platform,
        &key,
        "beta/agent",
        set_ref(
            "repo.git:refs/heads/main",
            ContentHash::blake3(b"ambiguous"),
            None,
        ),
    );
    let current = view(&platform);
    let concentration = &current["concentration"];
    assert_eq!(concentration["totals"]["bound_agent_keys"], 1);
    assert_eq!(concentration["totals"]["ambiguous_agent_keys"], 1);
    assert_eq!(concentration["totals"]["active_branches"], 1);
    assert_eq!(concentration["totals"]["unknown_active_branches"], 0);
    assert_eq!(concentration["totals"]["ambiguous_active_branches"], 1);
    assert!(concentration["operators"].as_object().unwrap().is_empty());
    assert_eq!(concentration["tripwire_status"], "indeterminate");

    std::fs::remove_dir_all(&work).ok();
}

#[test]
fn protected_updates_follow_the_exact_approved_requester_and_replay() {
    let work = std::env::temp_dir().join(format!(
        "choir-concentration-replay-{}",
        std::process::id()
    ));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let keys_file = work.join("keys");
    let pool_file = work.join("reviewers");
    let refs_file = work.join("protected");
    let log_file = work.join("ops.jsonl");
    let protected_ref = "repo.git:refs/heads/main";

    let requester = ActorKey::generate();
    let unbound_requester = ActorKey::generate();
    let reviewer_a = ActorKey::generate();
    let reviewer_b = ActorKey::generate();
    let node_secret = ActorKey::generate().secret_bytes();
    std::fs::write(
        &keys_file,
        format!(
            "alpha/agent {}\n{}\nreview-a/agent {}\nreview-b/agent {}\n",
            hex_encode(&requester.public_key_bytes()),
            hex_encode(&unbound_requester.public_key_bytes()),
            hex_encode(&reviewer_a.public_key_bytes()),
            hex_encode(&reviewer_b.public_key_bytes())
        ),
    )
    .unwrap();
    std::fs::write(&pool_file, "review-a/agent\nreview-b/agent\n").unwrap();
    std::fs::write(&refs_file, format!("{protected_ref}\n")).unwrap();

    let registry = || {
        let mut registry = Registry::new();
        for key in [
            &requester,
            &unbound_requester,
            &reviewer_a,
            &reviewer_b,
        ] {
            registry.register(&key.public_key_bytes()).unwrap();
        }
        registry
    };

    let expected_concentration;
    {
        let node_key = ActorKey::from_secret_bytes(&node_secret);
        let platform = Platform::start_reloading(
            registry(),
            Box::new(FileLog::open(&log_file).unwrap()),
            ActorKey::from_secret_bytes(&node_secret),
            Some(keys_file.clone()),
        )
        .unwrap()
        .with_reviewer_pool(pool_file)
        .with_protected_refs(refs_file.clone())
        .with_required_review();

        let base = ContentHash::blake3(b"base");
        submit(
            &platform,
            &node_key,
            "git/transport",
            set_ref(protected_ref, base.clone(), None),
        );

        let target = ContentHash::blake3(b"reviewed target");
        let response = submit(
            &platform,
            &requester,
            "alpha/agent",
            ViewOp::new(OpKind::RequestReview {
                id: "protected-1".into(),
                target: target.clone(),
                reviewers: Vec::new(),
                target_ref: Some(protected_ref.into()),
            }),
        );
        let drawn: Vec<String> =
            serde_json::from_value(response["reviewers"].clone()).expect("drawn reviewers");
        assert_eq!(drawn.len(), 2);
        for who in drawn {
            let key = match who.as_str() {
                "review-a/agent" => &reviewer_a,
                "review-b/agent" => &reviewer_b,
                _ => panic!("unexpected reviewer {who}"),
            };
            submit(
                &platform,
                key,
                &who,
                ViewOp::new(OpKind::PostVerdict {
                    id: "protected-1".into(),
                    reviewer: who.clone(),
                    verdict: Verdict::Approve,
                    note: String::new(),
                }),
            );
        }

        // Git-derived ref updates are node-signed. Attribution therefore
        // comes from the one bound requester whose approved review names
        // this exact (ref, target), never from `git/transport`'s spelling.
        submit(
            &platform,
            &node_key,
            "git/transport",
            set_ref(protected_ref, target.clone(), Some(base)),
        );
        let current = view(&platform);
        let concentration = &current["concentration"];
        assert_eq!(concentration["totals"]["protected_updates"], 1);
        assert_eq!(concentration["totals"]["unattributed_protected_updates"], 0);
        assert_eq!(concentration["operators"]["alpha"]["protected_updates"], 1);
        assert_eq!(concentration["operators"]["alpha"]["active_branches"], 1);
        assert_eq!(
            concentration["operators"]["alpha"]["tripwires"]["protected_update_share"],
            true
        );
        assert_eq!(concentration["tripwire_status"], "observed");

        // If another exact approved review has a requester whose operator
        // cannot be resolved, choosing the one known requester would be
        // convenient but false. The next update must be ambiguous.
        let second_target = ContentHash::blake3(b"multiply reviewed target");
        for (id, author, channel) in [
            ("protected-2a", &requester, "alpha/agent"),
            ("protected-2b", &unbound_requester, "mystery"),
        ] {
            let response = submit(
                &platform,
                author,
                channel,
                ViewOp::new(OpKind::RequestReview {
                    id: id.into(),
                    target: second_target.clone(),
                    reviewers: Vec::new(),
                    target_ref: Some(protected_ref.into()),
                }),
            );
            let drawn: Vec<String> = serde_json::from_value(response["reviewers"].clone())
                .expect("drawn reviewers");
            for who in drawn {
                let key = match who.as_str() {
                    "review-a/agent" => &reviewer_a,
                    "review-b/agent" => &reviewer_b,
                    _ => panic!("unexpected reviewer {who}"),
                };
                submit(
                    &platform,
                    key,
                    &who,
                    ViewOp::new(OpKind::PostVerdict {
                        id: id.into(),
                        reviewer: who.clone(),
                        verdict: Verdict::Approve,
                        note: String::new(),
                    }),
                );
            }
        }
        submit(
            &platform,
            &node_key,
            "git/transport",
            set_ref(protected_ref, second_target, Some(target)),
        );
        let after_ambiguous = view(&platform);
        let concentration = &after_ambiguous["concentration"];
        assert_eq!(concentration["totals"]["protected_updates"], 2);
        assert_eq!(concentration["totals"]["attributed_protected_updates"], 1);
        assert_eq!(concentration["totals"]["ambiguous_protected_updates"], 1);
        assert_eq!(concentration["totals"]["ambiguous_active_branches"], 1);
        assert_eq!(concentration["operators"]["alpha"]["protected_updates"], 1);
        expected_concentration = concentration.clone();
    }

    // The instrumentation is a deterministic projection of the signed
    // log plus current files, not process-lifetime counters.
    let replayed = Platform::start_reloading(
        registry(),
        Box::new(FileLog::open(&log_file).unwrap()),
        ActorKey::from_secret_bytes(&node_secret),
        Some(keys_file),
    )
    .unwrap()
    .with_protected_refs(refs_file);
    assert_eq!(view(&replayed)["concentration"], expected_concentration);

    std::fs::remove_dir_all(&work).ok();
}
