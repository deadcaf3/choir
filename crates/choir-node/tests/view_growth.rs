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
        })
    );
    for section in ["workspaces", "refs", "reviews", "provenance"] {
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
        })
    );
    for section in ["workspaces", "refs", "reviews", "provenance"] {
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
        })
    );
    for section in ["workspaces", "refs", "reviews", "provenance"] {
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
