//! Complete-view growth instrumentation. The authoritative view remains
//! complete and unbounded; this suite measures that meaning without adding
//! eviction, retention, or a second state tracker.

use choir_identity::{ActorKey, Registry};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use choir_node::platform::hex_encode;
use choir_node::Platform;
use choir_oplog::{ContentHash, MemLog};
use choir_view::{append_op, OpKind, Verdict, ViewOp};

fn current_view(platform: &Platform) -> (String, serde_json::Value) {
    let (status, body) = platform.handle_api("GET", "/api/view", b"");
    assert_eq!(status, 200, "{body}");
    let value = serde_json::from_str(&body).expect("view response is json");
    (body, value)
}

fn serialized_len(value: &serde_json::Value) -> usize {
    serde_json::to_vec(value)
        .expect("view section serializes")
        .len()
}

/// Bindings are projected and measured, and must never move the tracked
/// total.
///
/// `total_authoritative_view` is a series compared across readings, so
/// folding a fifth section into it would silently rewrite the meaning of
/// every past number. The opposite failure is just as real: an append-only
/// map that nothing counts is how a view grows without anyone noticing.
/// Both are asserted here, because the safe-looking fix for either one
/// breaks the other.
#[test]
fn bindings_are_measured_but_stay_out_of_the_authoritative_total() {
    let node_key = ActorKey::generate();
    let platform = Platform::start(
        Registry::new(),
        Box::new(MemLog::new()),
        ActorKey::from_secret_bytes(&node_key.secret_bytes()),
    )
    .expect("platform starts");

    let (_, before) = current_view(&platform);
    let total_before = before["view_growth"]["serialized_bytes"]["total_authoritative_view"]
        .as_u64()
        .expect("total is a number");
    assert_eq!(before["bindings"], serde_json::json!({}));
    assert_eq!(before["view_growth"]["serialized_bytes"]["bindings"], 2);

    let subject = ContentHash::blake3(b"an agent key");
    let submit = |op: ViewOp| {
        let payload = op.to_payload();
        let sig = node_key.sign_submission("node/bind", &payload);
        let (status, body) = platform.handle_api(
            "POST",
            "/api/submit",
            serde_json::json!({
                "channel": "node/bind",
                "payload_hex": hex_encode(&payload),
                "key_id": sig.key_id,
                "signature_hex": hex_encode(&sig.signature),
            })
            .to_string()
            .as_bytes(),
        );
        assert_eq!(status, 200, "{body}");
    };
    submit(ViewOp::new(OpKind::BindKey {
        operator: "alpha".into(),
        key: subject.clone(),
        channel: Some("alpha/agent".into()),
    }));

    let (_, bound) = current_view(&platform);
    let row = &bound["bindings"][subject.to_hex()];
    assert_eq!(row["operator"], "alpha", "{}", bound["bindings"]);
    assert_eq!(row["channel"], "alpha/agent");
    assert_eq!(
        row["bound_at"], 0,
        "the first binding pins the log position"
    );
    assert!(row["revoked"].is_null(), "an unrevoked row reports null");
    assert_eq!(bound["view_growth"]["counts"]["bindings"], 1);
    assert_eq!(bound["view_growth"]["counts"]["revoked_bindings"], 0);
    assert_eq!(
        bound["view_growth"]["serialized_bytes"]["bindings"],
        serialized_len(&bound["bindings"]),
        "the binding map must be measured, not merely projected"
    );
    assert_eq!(
        bound["view_growth"]["serialized_bytes"]["total_authoritative_view"], total_before,
        "a binding must not move the tracked authoritative total"
    );

    // Revocation keeps the row, so the count must not fall back.
    submit(ViewOp::new(OpKind::RevokeKey {
        key: subject.clone(),
        reason: "key material rotated".into(),
    }));
    let (_, revoked) = current_view(&platform);
    let row = &revoked["bindings"][subject.to_hex()];
    assert_eq!(row["operator"], "alpha", "attribution survives revocation");
    assert_eq!(
        row["bound_at"], 0,
        "revoking must not move the first binding"
    );
    assert_eq!(row["revoked"]["at"], 1);
    assert_eq!(row["revoked"]["reason"], "key material rotated");
    assert_eq!(revoked["view_growth"]["counts"]["bindings"], 1);
    assert_eq!(revoked["view_growth"]["counts"]["revoked_bindings"], 1);
    assert_eq!(
        revoked["view_growth"]["serialized_bytes"]["total_authoritative_view"], total_before,
        "a revocation must not move the tracked authoritative total either"
    );
}

fn authoritative_view(value: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "workspaces": value["workspaces"],
        "refs": value["refs"],
        "reviews": value["reviews"],
        "provenance": value["provenance"],
    })
}

fn assert_growth_matches_sections(view: &serde_json::Value) {
    let growth = &view["view_growth"];
    let provenance = view["provenance"].as_object().expect("provenance map");
    let provenance_records: usize = provenance
        .values()
        .map(|kinds| kinds.as_object().expect("provenance kind map").len())
        .sum();
    let reviews = view["reviews"].as_object().expect("review map");
    let archived_reviews = reviews
        .values()
        .filter(|review| review["archived"] == true)
        .count();
    assert_eq!(
        growth["counts"],
        serde_json::json!({
            "workspaces": view["workspaces"].as_object().unwrap().len(),
            "refs": view["refs"].as_object().unwrap().len(),
            "reviews": reviews.len(),
            "live_reviews": reviews.len() - archived_reviews,
            "archived_reviews": archived_reviews,
            "provenance_subjects": provenance.len(),
            "provenance_records": provenance_records,
            "bindings": view["bindings"].as_object().expect("binding map").len(),
            "revoked_bindings": view["bindings"]
                .as_object()
                .expect("binding map")
                .values()
                .filter(|binding| !binding["revoked"].is_null())
                .count(),
            "vouch_subjects": view["vouches"].as_object().expect("vouch map").len(),
            // Edges, not rows: the outer map is what paging bounds, and
            // the inner one is where the graph grows (D65).
            "vouch_edges": view["vouches"]
                .as_object()
                .expect("vouch map")
                .values()
                .map(|from| from.as_object().expect("voucher map").len())
                .sum::<usize>(),
        })
    );
    for section in [
        "workspaces",
        "refs",
        "reviews",
        "provenance",
        "bindings",
        "vouches",
    ] {
        assert_eq!(
            growth["serialized_bytes"][section],
            serialized_len(&view[section]),
            "wrong byte count for {section}"
        );
    }
    assert_eq!(
        growth["serialized_bytes"]["total_authoritative_view"],
        serialized_len(&authoritative_view(view))
    );
}

#[test]
fn empty_view_reports_exact_non_self_referential_sizes() {
    let platform = Platform::start(
        Registry::new(),
        Box::new(MemLog::new()),
        ActorKey::generate(),
    )
    .expect("platform starts");
    let (_, view) = current_view(&platform);
    let growth = &view["view_growth"];

    assert_eq!(growth["format_version"], 1);
    assert!(growth["as_of_seq"].is_null());
    assert_eq!(
        growth["counts"],
        serde_json::json!({
            "workspaces": 0,
            "refs": 0,
            "reviews": 0,
            "live_reviews": 0,
            "archived_reviews": 0,
            "provenance_subjects": 0,
            "provenance_records": 0,
            "bindings": 0,
            "revoked_bindings": 0,
            "vouch_subjects": 0,
            "vouch_edges": 0,
        })
    );
    for section in [
        "workspaces",
        "refs",
        "reviews",
        "provenance",
        "bindings",
        "vouches",
    ] {
        assert_eq!(
            growth["serialized_bytes"][section],
            serialized_len(&view[section]),
            "wrong byte count for {section}"
        );
    }
    assert_eq!(
        growth["serialized_bytes"]["total_authoritative_view"],
        serialized_len(&authoritative_view(&view)),
        "the total is the four authoritative sections, excluding runtime projections"
    );
}

#[test]
fn counts_live_archived_and_latest_provenance_records_deterministically() {
    let mut log = MemLog::new();
    let workspace_head = ContentHash::blake3(b"workspace head");
    append_op(
        &mut log,
        "author",
        ViewOp::new(OpKind::SetWorkspaceHead {
            workspace: "op/agent".into(),
            commit: workspace_head,
            prev: None,
        }),
    )
    .unwrap();
    append_op(
        &mut log,
        "author",
        ViewOp::new(OpKind::SetRef {
            name: "repo.git:refs/heads/main".into(),
            commit: ContentHash::blake3(b"main head"),
            prev: None,
        }),
    )
    .unwrap();
    append_op(
        &mut log,
        "author",
        ViewOp::new(OpKind::RequestReview {
            id: "live".into(),
            target: ContentHash::blake3(b"live review"),
            reviewers: vec!["review/live".into()],
            target_ref: None,
        }),
    )
    .unwrap();
    append_op(
        &mut log,
        "author",
        ViewOp::new(OpKind::RequestReview {
            id: "archived".into(),
            target: ContentHash::blake3(b"archived review"),
            reviewers: vec!["review/archive".into()],
            target_ref: None,
        }),
    )
    .unwrap();
    append_op(
        &mut log,
        "review/archive",
        ViewOp::new(OpKind::PostVerdict {
            id: "archived".into(),
            reviewer: "review/archive".into(),
            verdict: Verdict::Approve,
            note: "review detail dropped by archiving".into(),
        }),
    )
    .unwrap();
    append_op(
        &mut log,
        "node",
        ViewOp::new(OpKind::ArchiveReview {
            id: "archived".into(),
            lapsed: false,
        }),
    )
    .unwrap();
    for (kind, body) in [
        ("task", "first task body"),
        ("plan", "one plan body"),
        ("task", "replacement task body is the visible record"),
    ] {
        append_op(
            &mut log,
            "author",
            ViewOp::new(OpKind::RecordProvenance {
                subject: "view-growth".into(),
                kind: kind.into(),
                body: body.into(),
            }),
        )
        .unwrap();
    }

    let platform = Platform::start(Registry::new(), Box::new(log), ActorKey::generate())
        .expect("platform replays");
    let (first_body, view) = current_view(&platform);
    let growth = &view["view_growth"];

    assert_eq!(growth["as_of_seq"], 8);
    assert_eq!(
        growth["counts"],
        serde_json::json!({
            "workspaces": 1,
            "refs": 1,
            "reviews": 2,
            "live_reviews": 1,
            "archived_reviews": 1,
            "provenance_subjects": 1,
            // Three provenance ops become two visible records because
            // latest-wins is part of the complete view's meaning.
            "provenance_records": 2,
            "bindings": 0,
            "revoked_bindings": 0,
            "vouch_subjects": 0,
            "vouch_edges": 0,
        })
    );
    for section in [
        "workspaces",
        "refs",
        "reviews",
        "provenance",
        "bindings",
        "vouches",
    ] {
        assert_eq!(
            growth["serialized_bytes"][section],
            serialized_len(&view[section]),
            "wrong byte count for {section}"
        );
    }
    assert_eq!(
        growth["serialized_bytes"]["total_authoritative_view"],
        serialized_len(&authoritative_view(&view))
    );

    let (second_body, second_view) = current_view(&platform);
    assert_eq!(
        first_body, second_body,
        "the raw projection must be deterministic"
    );
    assert_eq!(view["view_growth"], second_view["view_growth"]);
}

#[test]
fn every_concurrent_read_is_one_coherent_log_prefix() {
    const OPS: usize = 200;

    let key = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&key.public_key_bytes()).unwrap();
    let platform = Arc::new(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate())
            .expect("platform starts"),
    );
    let done = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicUsize::new(0));
    let writer = {
        let platform = platform.clone();
        let done = done.clone();
        let reads = reads.clone();
        std::thread::spawn(move || {
            let run = || -> Result<(), String> {
                for index in 0..OPS {
                    let op = ViewOp::new(OpKind::RecordProvenance {
                        subject: "concurrent".into(),
                        kind: format!("record-{index:03}"),
                        body: format!("body-{index:03}"),
                    });
                    let payload = op.to_payload();
                    let sig = key.sign_submission("writer", &payload);
                    let (status, body) = platform.handle_api(
                        "POST",
                        "/api/submit",
                        serde_json::json!({
                            "channel": "writer",
                            "payload_hex": hex_encode(&payload),
                            "key_id": sig.key_id,
                            "signature_hex": hex_encode(&sig.signature),
                        })
                        .to_string()
                        .as_bytes(),
                    );
                    if status != 200 {
                        return Err(body);
                    }
                    if index == 0 {
                        while reads.load(Ordering::Acquire) == 0 {
                            std::thread::yield_now();
                        }
                    }
                }
                Ok(())
            }();
            done.store(true, Ordering::Release);
            run
        })
    };

    while !done.load(Ordering::Acquire) || reads.load(Ordering::Relaxed) == 0 {
        let (_, view) = current_view(&platform);
        assert_growth_matches_sections(&view);
        let records = view["view_growth"]["counts"]["provenance_records"]
            .as_u64()
            .unwrap();
        match view["view_growth"]["as_of_seq"].as_u64() {
            Some(seq) => assert_eq!(seq + 1, records),
            None => assert_eq!(records, 0),
        }
        reads.fetch_add(1, Ordering::Release);
    }
    writer.join().expect("writer thread joins").unwrap();

    let (_, final_view) = current_view(&platform);
    assert_growth_matches_sections(&final_view);
    assert_eq!(final_view["view_growth"]["as_of_seq"], OPS - 1);
    assert_eq!(
        final_view["view_growth"]["counts"]["provenance_records"],
        OPS
    );
}

/// Archiving must actually reclaim, and the measurement says by how much.
///
/// Every previous reading of this series was taken with zero archived
/// rows, so it measured live accumulation and said nothing about the
/// question retention exists to answer. Measured here on 200 reviews of
/// five comments each: a live row is ~884 bytes and an archived one ~302,
/// so archiving reclaims about two thirds and an archived row costs about
/// a third of a live one forever.
///
/// The ratio is asserted rather than the bytes. Both numbers move with
/// any field added to the projection, and a byte assertion would fail on
/// a change that preserves exactly the property worth keeping: that the
/// bulk goes and the outcome stays. What would break it is a refactor
/// that stops emptying `comments`, `verdicts` or `viewed` on archive, and
/// then nothing else in this suite would notice.
#[test]
fn an_archived_row_costs_a_fraction_of_the_live_one() {
    fn build(archive: bool) -> Platform {
        let mut log = MemLog::new();
        append_op(
            &mut log,
            "author",
            ViewOp::new(OpKind::RequestReview {
                id: "r".into(),
                target: ContentHash::blake3(b"target"),
                reviewers: vec!["rev/a".into(), "rev/b".into()],
                target_ref: Some("agents/demo.git:refs/heads/main".into()),
            }),
        )
        .unwrap();
        for c in 0..5 {
            append_op(
                &mut log,
                "author",
                ViewOp::new(OpKind::PostComment {
                    id: "r".into(),
                    comment: format!("c{c}"),
                    author: "author".into(),
                    body: "roughly the length of a real review comment, give or take".into(),
                }),
            )
            .unwrap();
        }
        for reviewer in ["rev/a", "rev/b"] {
            append_op(
                &mut log,
                reviewer,
                ViewOp::new(OpKind::PostVerdict {
                    id: "r".into(),
                    reviewer: reviewer.into(),
                    verdict: Verdict::Approve,
                    note: "looks right to me".into(),
                }),
            )
            .unwrap();
        }
        if archive {
            append_op(
                &mut log,
                "node",
                ViewOp::new(OpKind::ArchiveReview {
                    id: "r".into(),
                    lapsed: false,
                }),
            )
            .unwrap();
        }
        Platform::start(Registry::new(), Box::new(log), ActorKey::generate())
            .expect("platform replays")
    }

    let (_, live) = current_view(&build(false));
    let (_, archived) = current_view(&build(true));
    let live_row = serialized_len(&live["reviews"]["r"]);
    let archived_row = serialized_len(&archived["reviews"]["r"]);

    assert!(
        archived_row * 2 < live_row,
        "archiving reclaimed less than half: {live_row} -> {archived_row} bytes"
    );
    // The outcome survives; only the bulk goes. An archived row that kept
    // its comments would still shrink if the verdicts went, so this is
    // asserted field by field rather than inferred from the ratio.
    let row = &archived["reviews"]["r"];
    assert_eq!(row["archived"], true);
    assert_eq!(
        row["approved"], true,
        "the outcome did not survive archiving"
    );
    for bulk in ["comments", "verdicts", "viewed", "reviewers"] {
        assert!(
            row[bulk].as_array().is_none_or(Vec::is_empty)
                && row[bulk].as_object().is_none_or(serde_json::Map::is_empty),
            "{bulk} survived archiving: {}",
            row[bulk]
        );
    }
}

/// Cycles of create-then-delete, enough that a per-op leak is unmissable.
const CHURN: u64 = 200;

/// Distinct live refs, for the linearity half.
const LIVE: u64 = 50;

/// The view grows with live data, not with log length.
///
/// This is the Phase-1 question about the complete view, and no existing
/// test asks it. `bindings_are_measured_but_stay_out_of_the_authoritative_total`
/// is about which sections the total *means*; `retention_cost.rs` is
/// about what pruning costs the submit path. Neither says what happens to
/// a node that has simply been running for a long time.
///
/// The claim: the authoritative total is a function of what is live now.
/// A create/delete cycle leaves nothing live, so two hundred of them must
/// leave the total exactly where it started, however long the log got.
/// If the total ever tracked the log instead, every reader of a
/// long-running node would pay for history none of them can see, and the
/// first symptom would be a slow `/api/view` on the busiest node rather
/// than an error anywhere a test would look.
///
/// The second half is the other direction, because a total that never
/// moves is equally broken: fifty distinct live refs must cost fifty
/// times one ref, not more. Linear in live data is the property; a total
/// that grew superlinearly would still pass the churn half.
///
/// Deterministic on purpose — serialized byte counts, no wall clock — so
/// it belongs in the merged harness and does not move with machine load.
/// Same argument as `tests/phase1_spawns.rs` makes for git spawns.
#[test]
fn the_authoritative_view_tracks_live_data_and_not_log_length() {
    let node_key = ActorKey::generate();
    let platform = Platform::start(
        Registry::new(),
        Box::new(MemLog::new()),
        ActorKey::from_secret_bytes(&node_key.secret_bytes()),
    )
    .expect("platform starts");

    let submit = |op: ViewOp| {
        let payload = op.to_payload();
        let sig = node_key.sign_submission("node/test", &payload);
        let (status, body) = platform.handle_api(
            "POST",
            "/api/submit",
            serde_json::json!({
                "channel": "node/test",
                "payload_hex": hex_encode(&payload),
                "key_id": sig.key_id,
                "signature_hex": hex_encode(&sig.signature),
            })
            .to_string()
            .as_bytes(),
        );
        assert_eq!(status, 200, "{body}");
    };
    let reading = || {
        let (_, view) = current_view(&platform);
        let growth = &view["view_growth"];
        (
            growth["serialized_bytes"]["total_authoritative_view"]
                .as_u64()
                .expect("total is a number"),
            // `null` until the first op is folded, which is exactly the
            // baseline state this test starts from.
            growth["as_of_seq"].as_u64(),
            growth["counts"]["refs"]
                .as_u64()
                .expect("count is a number"),
        )
    };

    let (baseline, seq_before, refs_before) = reading();
    assert_eq!(refs_before, 0, "the fixture starts with no refs");
    assert_eq!(seq_before, None, "the fixture starts with an unfolded log");

    for i in 0..CHURN {
        let commit = ContentHash::blake3(format!("churn {i}").as_bytes());
        submit(ViewOp::new(OpKind::SetRef {
            name: "agents/one.git:refs/heads/churn".into(),
            commit: commit.clone(),
            prev: None,
        }));
        submit(ViewOp::new(OpKind::DeleteRef {
            name: "agents/one.git:refs/heads/churn".into(),
            prev: Some(commit),
        }));
    }

    let (after_churn, seq_after, refs_after) = reading();
    // Vacuity first. A total that did not move because nothing was
    // sequenced would pass the real assertion silently, which is how a
    // deleted counter passed the spawn budget before it was two-sided.
    assert_eq!(
        seq_after,
        Some(2 * CHURN - 1),
        "the churn did not reach the log, so the totals below prove nothing"
    );
    assert_eq!(refs_after, 0, "the churn left a ref behind");
    assert_eq!(
        after_churn,
        baseline,
        "the authoritative view grew by {} bytes over {} operations that left \
         nothing live, so it is tracking log length rather than live data",
        after_churn as i64 - baseline as i64,
        2 * CHURN
    );

    // The other direction: live data must actually cost something, and
    // must cost it linearly.
    let one = ContentHash::blake3(b"first live ref");
    submit(ViewOp::new(OpKind::SetRef {
        name: "agents/one.git:refs/heads/live0000".into(),
        commit: one,
        prev: None,
    }));
    let (with_one, _, _) = reading();
    let per_ref = with_one
        .checked_sub(baseline)
        .expect("a live ref costs something");
    assert!(per_ref > 0, "a live ref must move the authoritative total");

    for i in 1..LIVE {
        submit(ViewOp::new(OpKind::SetRef {
            name: format!("agents/one.git:refs/heads/live{i:04}"),
            commit: ContentHash::blake3(format!("live {i}").as_bytes()),
            prev: None,
        }));
    }
    let (with_many, _, refs_live) = reading();
    assert_eq!(refs_live, LIVE, "the live refs did not all land");
    // Every ref name here is the same length and every commit is a
    // BLAKE3 hash, so the rows are the same size and the total is exactly
    // linear -- to the byte, separators included. The `LIVE - 1` is the
    // commas: the first entry in a JSON map has nothing before it and
    // every later one costs a separator, which is why fifty rows are 49
    // bytes more than fifty times one row rather than a round multiple.
    //
    // Asserting equality rather than a bound is the point. A tolerance is
    // where superlinear growth hides, and the first version of this
    // assertion was a plain multiple that failed by exactly those 49
    // bytes -- the model was wrong, not the node, and a loose bound would
    // have hidden which.
    assert_eq!(
        with_many - baseline,
        per_ref * LIVE + (LIVE - 1),
        "{LIVE} identical-shaped refs cost {} bytes where one costs {per_ref} \
         and a separator costs 1; the view is not linear in live data",
        with_many - baseline
    );
}
