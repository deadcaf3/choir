//! D24 T2: the review record's honest answer about a new actor.
//!
//! T2 asks for a new-actor invalid/slop rate. A verdict records whether a
//! change may land, not whether it was valid, and `RequestChanges` is
//! overwritable. So this projection reports the outcomes that exist, keeps
//! every excluded bucket visible, and refuses to name a classifier it does
//! not have. `tripwire_status` stays `indeterminate` on complete data,
//! because the gap is structural rather than one of sample size.

use std::sync::Arc;

use choir_hash::ContentHash;
use choir_identity::{ActorKey, Registry};
use choir_node::platform::{hex_encode, ReviewRetention};
use choir_node::Platform;
use choir_oplog::MemLog;
use choir_view::{OpKind, Verdict, ViewOp};

fn submit(platform: &Platform, key: &ActorKey, channel: &str, op: ViewOp) -> serde_json::Value {
    let payload = op.to_payload();
    let sig = key.sign_submission(channel, &payload);
    let body = serde_json::json!({
        "channel": channel,
        "payload_hex": hex_encode(&payload),
        "key_id": sig.key_id,
        "signature_hex": hex_encode(&sig.signature),
    })
    .to_string()
    .into_bytes();
    let (status, response) = platform.handle_api("POST", "/api/submit", &body);
    assert_eq!(status, 200, "submission failed: {response}");
    serde_json::from_str(&response).expect("JSON response")
}

fn request(id: &str, reviewers: &[&str]) -> ViewOp {
    ViewOp::new(OpKind::RequestReview {
        id: id.into(),
        target: ContentHash::blake3(id.as_bytes()),
        reviewers: reviewers.iter().map(|r| (*r).to_string()).collect(),
        target_ref: None,
    })
}

fn verdict(id: &str, verdict: Verdict) -> ViewOp {
    ViewOp::new(OpKind::PostVerdict {
        id: id.into(),
        reviewer: "rev".into(),
        verdict,
        note: String::new(),
    })
}

fn outcomes(platform: &Platform) -> serde_json::Value {
    let (status, response) = platform.handle_api("GET", "/api/view", &[]);
    assert_eq!(status, 200, "view failed: {response}");
    let view: serde_json::Value = serde_json::from_str(&response).expect("JSON view");
    view["new_actor_review_outcomes"].clone()
}

fn workdir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("choir-t2-{name}-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).expect("workdir");
    dir
}

#[test]
fn new_actor_review_outcomes_report_raw_evidence_and_stay_indeterminate() {
    let dir = workdir("cohort");
    let incumbent = ActorKey::generate();
    let newcomer = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&incumbent.public_key_bytes()).unwrap();
    registry.register(&newcomer.public_key_bytes()).unwrap();

    let platform = Platform::start_reloading_with_review_retention(
        registry,
        Box::new(MemLog::new()),
        ActorKey::generate(),
        None,
        // Five reviews are created below, so this bound settles exactly
        // one of them: the oldest complete review, at the last request.
        ReviewRetention::keep(4),
    )
    .expect("platform starts")
    .with_newcomer_audit(
        dir.join("newcomer-audit.jsonl"),
        dir.join("newcomer-adjudications.jsonl"),
        vec![incumbent.actor_id().to_hex()],
    )
    .expect("audit opens");

    // Outside the cohort: an incumbent's review must never land in a
    // new-actor bucket, and its incompleteness must not read as a
    // newcomer's pending work.
    submit(&platform, &incumbent, "incumbent/agent", request("i-pending", &["rev"]));

    // Every verdict below is posted by the incumbent. Attribution is the
    // `RequestReview` author, so answering a newcomer's review must not
    // move the review out of the cohort.
    for id in ["n-archived", "n-approved"] {
        submit(&platform, &newcomer, "newcomer/agent", request(id, &["rev"]));
        submit(&platform, &incumbent, "rev", verdict(id, Verdict::Approve));
    }
    submit(&platform, &newcomer, "newcomer/agent", request("n-changes", &["rev"]));
    submit(&platform, &incumbent, "rev", verdict("n-changes", Verdict::RequestChanges));
    let response = submit(&platform, &newcomer, "newcomer/agent", request("n-pending", &["rev"]));
    assert_eq!(
        response["archived_reviews"],
        serde_json::json!(["n-archived"]),
        "retention should have settled the oldest complete review: {response}"
    );

    let report = outcomes(&platform);
    assert_eq!(report["cohort"]["available"], true, "{report}");
    assert_eq!(
        report["cohort"]["definition"], "post_activation_non_incumbent_actor_keys",
        "{report}"
    );
    assert_eq!(report["cohort"]["actor_keys"], 1, "{report}");
    assert_eq!(report["totals"]["reviews"], 5, "{report}");
    assert_eq!(report["totals"]["attributed_to_cohort"], 4, "{report}");
    assert_eq!(report["totals"]["excluded_outside_cohort"], 1, "{report}");
    assert_eq!(report["totals"]["excluded_unsigned_author"], 0, "{report}");
    assert_eq!(report["totals"]["excluded_unknown_requester"], 0, "{report}");
    assert_eq!(report["review_outcomes"]["approved"], 1, "{report}");
    assert_eq!(report["review_outcomes"]["request_changes"], 1, "{report}");
    assert_eq!(report["review_outcomes"]["pending"], 1, "{report}");
    assert_eq!(report["review_outcomes"]["archived_evidence_lost"], 1, "{report}");
    assert_eq!(report["review_outcomes"]["archived_classified"], 0, "{report}");
    assert_eq!(report["review_outcomes"]["re_review_required"], 0, "{report}");

    // Load needs no adjudication at all, which is why it survives a flood.
    assert_eq!(report["unit"], "contribution_offered", "{report}");
    assert_eq!(report["load"]["reviews_requested"], 4, "{report}");
    assert_eq!(report["load"]["review_rounds"], 3, "{report}");
    assert_eq!(report["load"]["reviews_landed"], 0, "{report}");
    assert_eq!(report["load"]["reviews_never_landed"], 4, "{report}");
    assert_eq!(report["load"]["review_rounds_on_unlanded"], 3, "{report}");

    // The point of the whole projection: complete, fully attributed
    // evidence still does not answer T2.
    assert_eq!(report["policy"]["graduation"], serde_json::Value::Null, "{report}");
    assert_eq!(report["policy"]["trailing_window"], serde_json::Value::Null, "{report}");
    assert_eq!(report["policy"]["tripwire_subject"], serde_json::Value::Null, "{report}");
    assert_eq!(report["declared_tripwire"]["evaluable"], false, "{report}");
    assert_eq!(
        report["tripwire_observed"],
        serde_json::Value::Null,
        "{report}"
    );
    assert_eq!(report["tripwire_status"], "indeterminate", "{report}");
    assert_eq!(report["evaluation_complete"], false, "{report}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn an_overwritten_request_changes_verdict_moves_to_the_approved_bucket() {
    let dir = workdir("overwrite");
    let author = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&author.public_key_bytes()).unwrap();
    let platform = Platform::start_reloading(
        registry,
        Box::new(MemLog::new()),
        ActorKey::generate(),
        None,
    )
    .expect("platform starts")
    .with_newcomer_audit(
        dir.join("newcomer-audit.jsonl"),
        dir.join("newcomer-adjudications.jsonl"),
        Vec::new(),
    )
    .expect("audit opens");

    submit(&platform, &author, "newcomer/agent", request("r", &["rev"]));
    submit(&platform, &author, "rev", verdict("r", Verdict::RequestChanges));
    let before = outcomes(&platform);
    assert_eq!(before["review_outcomes"]["request_changes"], 1, "{before}");

    submit(&platform, &author, "rev", verdict("r", Verdict::Approve));
    let after = outcomes(&platform);
    assert_eq!(after["review_outcomes"]["request_changes"], 0, "{after}");
    assert_eq!(
        after["review_outcomes"]["approved"], 1,
        "a re-review overwrites the earlier verdict, which is exactly why \
         `RequestChanges` cannot be counted as slop: {after}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn without_the_newcomer_audit_no_cohort_is_invented() {
    let author = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&author.public_key_bytes()).unwrap();
    let platform = Platform::start_reloading(
        registry,
        Box::new(MemLog::new()),
        ActorKey::generate(),
        None,
    )
    .expect("platform starts");

    submit(&platform, &author, "someone/agent", request("r", &["rev"]));

    let report = outcomes(&platform);
    assert_eq!(report["cohort"]["available"], false, "{report}");
    assert_eq!(report["cohort"]["actor_keys"], serde_json::Value::Null, "{report}");
    assert_eq!(report["totals"]["reviews"], 1, "{report}");
    assert_eq!(
        report["totals"]["attributed_to_cohort"],
        serde_json::Value::Null,
        "no audit means no way to tell who is new, which is not the same as \
         a cohort of zero: {report}"
    );
    assert_eq!(report["review_outcomes"], serde_json::Value::Null, "{report}");
    assert_eq!(report["tripwire_status"], "indeterminate", "{report}");
}

#[test]
fn the_projection_is_a_read_and_never_appends_to_the_log() {
    let dir = workdir("readonly");
    let author = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&author.public_key_bytes()).unwrap();
    let platform = Arc::new(
        Platform::start_reloading(
            registry,
            Box::new(MemLog::new()),
            ActorKey::generate(),
            None,
        )
        .expect("platform starts")
        .with_newcomer_audit(
            dir.join("newcomer-audit.jsonl"),
            dir.join("newcomer-adjudications.jsonl"),
            Vec::new(),
        )
        .expect("audit opens"),
    );

    submit(&platform, &author, "newcomer/agent", request("r", &["rev"]));
    let first = outcomes(&platform);
    let second = outcomes(&platform);
    assert_eq!(first["totals"]["attributed_to_cohort"], 1, "{first}");
    assert_eq!(first, second, "the projection must be a pure read");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_lapsed_archive_cannot_launder_an_unanswered_review_into_the_rate() {
    let dir = workdir("lapse");
    let author = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&author.public_key_bytes()).unwrap();
    // Zero is an explicit deterministic boundary, not a sleep: the oldest
    // incomplete review is archived as `Archived { approved: false }` even
    // though no reviewer ever answered it.
    let platform = Platform::start_reloading_with_review_retention(
        registry,
        Box::new(MemLog::new()),
        ActorKey::generate(),
        None,
        ReviewRetention::keep(1).lapse_incomplete_after(std::time::Duration::ZERO),
    )
    .expect("platform starts")
    .with_newcomer_audit(
        dir.join("newcomer-audit.jsonl"),
        dir.join("newcomer-adjudications.jsonl"),
        Vec::new(),
    )
    .expect("audit opens");

    submit(&platform, &author, "newcomer/agent", request("old", &["rev"]));
    let response = submit(&platform, &author, "newcomer/agent", request("new", &["rev"]));
    assert_eq!(
        response["archived_reviews"],
        serde_json::json!(["old"]),
        "{response}"
    );

    let report = outcomes(&platform);
    assert_eq!(
        report["review_outcomes"]["archived_evidence_lost"], 1,
        "a review nobody answered must not gain evidence by being archived: {report}"
    );
    assert_eq!(
        report["adjudication"]["eligible"], 0,
        "a wall-clock lapse must not put an unanswered review in the denominator: {report}"
    );
    assert_eq!(report["review_outcomes"]["pending"], 1, "{report}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_classification_survives_archiving_and_drives_coverage() {
    let dir = workdir("classify");
    let author = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&author.public_key_bytes()).unwrap();
    let adjudications = dir.join("review-adjudications.jsonl");
    let platform = Platform::start_reloading_with_review_retention(
        registry,
        Box::new(MemLog::new()),
        ActorKey::generate(),
        None,
        ReviewRetention::keep(1),
    )
    .expect("platform starts")
    .with_newcomer_audit(
        dir.join("newcomer-audit.jsonl"),
        dir.join("newcomer-adjudications.jsonl"),
        Vec::new(),
    )
    .expect("audit opens")
    .with_review_adjudications(adjudications.clone())
    .expect("adjudications open");

    submit(&platform, &author, "newcomer/agent", request("settled", &["rev"]));
    submit(&platform, &author, "rev", verdict("settled", Verdict::Approve));
    let response = submit(&platform, &author, "newcomer/agent", request("later", &["rev"]));
    assert_eq!(
        response["archived_reviews"],
        serde_json::json!(["settled"]),
        "{response}"
    );
    submit(&platform, &author, "rev", verdict("later", Verdict::Approve));

    let before = outcomes(&platform);
    assert_eq!(before["review_outcomes"]["archived_evidence_lost"], 1, "{before}");
    assert_eq!(before["adjudication"]["adjudicated"], 0, "{before}");

    // The classification is keyed by review id and lives outside the log, so
    // it attaches to a row whose verdict bulk was already discarded.
    std::fs::write(
        &adjudications,
        "{\"format_version\":1,\"review_id\":\"settled\",\"classification\":\"slop\"}\n",
    )
    .unwrap();

    let after = outcomes(&platform);
    assert_eq!(after["adjudication"]["available"], true, "{after}");
    assert_eq!(after["review_outcomes"]["archived_classified"], 1, "{after}");
    assert_eq!(after["review_outcomes"]["archived_evidence_lost"], 0, "{after}");
    assert_eq!(after["adjudication"]["classified"]["slop"], 1, "{after}");
    assert_eq!(after["adjudication"]["eligible"], 2, "{after}");
    assert_eq!(after["adjudication"]["coverage_basis_points"], 5_000, "{after}");
    assert_eq!(
        after["adjudication"]["independent"], false,
        "one operator classifying its own reviewers' approvals is not independent: {after}"
    );
    assert_eq!(after["tripwire_status"], "indeterminate", "{after}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn accepted_operations_count_even_when_no_review_is_requested() {
    let dir = workdir("unit");
    let author = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&author.public_key_bytes()).unwrap();
    let platform = Platform::start_reloading(
        registry,
        Box::new(MemLog::new()),
        ActorKey::generate(),
        None,
    )
    .expect("platform starts")
    .with_newcomer_audit(
        dir.join("newcomer-audit.jsonl"),
        dir.join("newcomer-adjudications.jsonl"),
        Vec::new(),
    )
    .expect("audit opens");

    // The whole reason the unit is not a completed review: a contributor
    // opens their own review, so work they never submit for review would be
    // invisible to a review-shaped denominator.
    for n in 0..3 {
        submit(
            &platform,
            &author,
            "newcomer/agent",
            ViewOp::new(OpKind::RecordProvenance {
                subject: format!("task/{n}"),
                kind: "plan".into(),
                body: "unreviewed work".into(),
            }),
        );
    }

    let report = outcomes(&platform);
    assert_eq!(report["load"]["accepted_operations"], 3, "{report}");
    assert_eq!(
        report["load"]["reviews_requested"], 0,
        "declining to request review must not hide the work: {report}"
    );
    assert_eq!(report["totals"]["attributed_to_cohort"], 0, "{report}");

    std::fs::remove_dir_all(&dir).ok();
}
