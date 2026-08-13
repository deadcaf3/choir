//! D24 T4 newcomer-harm measurement over the real authenticated HTTP edge.
//!
//! Thresholds remain deliberately unset until the first operator-adjudicated
//! measurement. This test fixes the evidence lifecycle: the first verified
//! rejection is durable, an appeal names that attempt without changing
//! privilege, adjudication is a separate operator input, and a later acceptance
//! remains attributed to the same newcomer across a daemon restart.

use choir_identity::{ActorKey, Registry};
use choir_node::platform::hex_encode;
use choir_node::{AuthTable, Node, Platform};
use choir_oplog::MemLog;
use choir_view::{OpKind, ViewOp};

const AUTH: &str = "operator:test-token";

use crate::support::curl;

fn authenticated(args: &[&str]) -> (u16, serde_json::Value) {
    let mut all = vec!["-u", AUTH];
    all.extend_from_slice(args);
    curl(&all)
}

use crate::support::submit_body;

fn auth_table() -> AuthTable {
    let mut auth = AuthTable::new();
    auth.insert("operator".to_string(), "test-token".to_string());
    auth
}

fn start_node(
    work: &std::path::Path,
    keys_file: &std::path::Path,
    audit_file: &std::path::Path,
    adjudications_file: &std::path::Path,
    registry: Registry,
    incumbents: Vec<String>,
) -> (std::sync::Arc<Node>, std::thread::JoinHandle<()>, String) {
    let platform = Platform::start_reloading(
        registry,
        Box::new(MemLog::new()),
        ActorKey::generate(),
        Some(keys_file.to_path_buf()),
    )
    .unwrap()
    .with_newcomer_audit(
        audit_file.to_path_buf(),
        adjudications_file.to_path_buf(),
        incumbents,
    )
    .unwrap();
    let mut node = Node::bind_with_auth(&work.join("repos"), 0, Some(auth_table())).unwrap();
    node.enable_platform(platform);
    let port = node.port();
    let node = std::sync::Arc::new(node);
    let serving = {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever())
    };
    let api = format!("http://127.0.0.1:{port}/api");
    (node, serving, api)
}

fn stop_node(node: std::sync::Arc<Node>, serving: std::thread::JoinHandle<()>) {
    node.unblock();
    serving.join().expect("node exits after unblock");
}

#[test]
fn rejected_newcomer_appeal_and_acceptance_survive_restart() {
    let work = std::env::temp_dir().join(format!("choir-newcomer-harm-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let keys_file = work.join("trusted-keys");
    let audit_file = work.join("newcomer-audit.jsonl");
    let adjudications_file = work.join("newcomer-adjudications.jsonl");

    let incumbent = ActorKey::generate();
    let newcomer = ActorKey::generate();
    let incumbent_id = incumbent.actor_id().to_hex();
    let newcomer_id = newcomer.actor_id().to_hex();
    let mut initial_registry = Registry::new();
    initial_registry
        .register(&incumbent.public_key_bytes())
        .unwrap();

    // The missing file gives the reload path an unambiguous None -> Some
    // metadata transition without a wall-clock sleep.
    let (node, serving, api) = start_node(
        &work.join("first"),
        &keys_file,
        &audit_file,
        &adjudications_file,
        initial_registry,
        vec![incumbent_id.clone()],
    );
    std::fs::write(
        &keys_file,
        format!(
            "incumbent/agent {}\nnewcomer/agent {}\n",
            hex_encode(&incumbent.public_key_bytes()),
            hex_encode(&newcomer.public_key_bytes()),
        ),
    )
    .unwrap();

    let incumbent_op = ViewOp::new(OpKind::RecordProvenance {
        subject: "task/incumbent".into(),
        kind: "plan".into(),
        body: "incumbent activity".into(),
    });
    let (code, response) = authenticated(&[
        "-X",
        "POST",
        "-d",
        &submit_body(&incumbent, "incumbent/agent", &incumbent_op),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 200, "{response}");
    assert!(response.get("newcomer_attempt_id").is_none(), "{response}");

    let rejected_op = ViewOp::new(OpKind::RecordProvenance {
        subject: String::new(),
        kind: "plan".into(),
        body: "first attempt".into(),
    });
    let (code, response) = authenticated(&[
        "-X",
        "POST",
        "-d",
        &submit_body(&newcomer, "newcomer/agent", &rejected_op),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 400, "{response}");
    assert_eq!(response["code"], "provenance_state", "{response}");
    assert_eq!(response["newcomer_attempt_id"], 0, "{response}");

    // Appeals are behind the same Basic-auth boundary as every API route.
    let unauthenticated = std::process::Command::new("curl")
        .args(["-s", "-o", "/dev/null", "-w", "%{http_code}"])
        .args(["-X", "POST", "-d", r#"{"attempt_id":0}"#])
        .arg(format!("{api}/appeal"))
        .output()
        .expect("curl runs");
    assert_eq!(String::from_utf8_lossy(&unauthenticated.stdout), "401");
    let (code, response) = authenticated(&[
        "-X",
        "POST",
        "-d",
        r#"{"attempt_id":0}"#,
        &format!("{api}/appeal"),
    ]);
    assert_eq!(code, 200, "{response}");
    assert_eq!(response["appealed"], 0, "{response}");
    // Retry is idempotent and does not add another durable row.
    let (code, response) = authenticated(&[
        "-X",
        "POST",
        "-d",
        r#"{"attempt_id":0}"#,
        &format!("{api}/appeal"),
    ]);
    assert_eq!(code, 200, "{response}");

    let (code, view) = authenticated(&[&format!("{api}/view")]);
    assert_eq!(code, 200, "{view}");
    let harm = &view["newcomer_harm"];
    assert_eq!(harm["audit"]["first_attempts"], 1, "{harm}");
    assert_eq!(harm["audit"]["first_attempts_rejected"], 1, "{harm}");
    assert_eq!(harm["appeals"]["submitted"], 1, "{harm}");
    assert_eq!(harm["appeals"]["unresolved"], 1, "{harm}");
    assert_eq!(
        harm["thresholds"]["false_reject_rate_basis_points"],
        serde_json::Value::Null
    );
    assert_eq!(harm["tripwire_status"], "indeterminate", "{harm}");
    stop_node(node, serving);

    std::fs::write(
        &adjudications_file,
        "{\"format_version\":1,\"attempt_id\":0,\"legitimate\":true}\n",
    )
    .unwrap();

    // A normal daemon restart now sees both keys in trusted-keys. The audit's
    // persisted activation row, not that current key snapshot, must decide who
    // was incumbent; otherwise this acceptance would be silently dropped.
    let mut restarted_registry = Registry::new();
    restarted_registry
        .register(&incumbent.public_key_bytes())
        .unwrap();
    restarted_registry
        .register(&newcomer.public_key_bytes())
        .unwrap();
    let (node, serving, api) = start_node(
        &work.join("restarted"),
        &keys_file,
        &audit_file,
        &adjudications_file,
        restarted_registry,
        vec![incumbent_id, newcomer_id],
    );
    let accepted_op = ViewOp::new(OpKind::RecordProvenance {
        subject: "task/newcomer".into(),
        kind: "plan".into(),
        body: "corrected attempt".into(),
    });
    let (code, response) = authenticated(&[
        "-X",
        "POST",
        "-d",
        &submit_body(&newcomer, "newcomer/agent", &accepted_op),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 200, "{response}");
    assert_eq!(response["newcomer_attempt_id"], 0, "{response}");

    let (code, view) = authenticated(&[&format!("{api}/view")]);
    assert_eq!(code, 200, "{view}");
    let harm = &view["newcomer_harm"];
    assert_eq!(harm["audit"]["incumbent_actor_keys_excluded"], 1, "{harm}");
    assert_eq!(
        harm["adjudications"]["coverage_basis_points"], 10_000,
        "{harm}"
    );
    assert_eq!(harm["adjudications"]["legitimate_attempts"], 1, "{harm}");
    assert_eq!(
        harm["measurements"]["legitimate_first_attempts_rejected"], 1,
        "{harm}"
    );
    assert_eq!(
        harm["measurements"]["false_reject_rate_basis_points"], 10_000,
        "{harm}"
    );
    assert_eq!(
        harm["measurements"]["accepted_legitimate_newcomers"], 1,
        "{harm}"
    );
    assert_eq!(
        harm["measurements"]["legitimate_newcomers_pending_acceptance"], 0,
        "{harm}"
    );
    assert_eq!(harm["measurement_complete"], true, "{harm}");
    assert_eq!(harm["evaluation_complete"], false, "{harm}");
    assert_eq!(harm["tripwire_status"], "indeterminate", "{harm}");

    let rows: Vec<serde_json::Value> = std::fs::read_to_string(&audit_file)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows.len(), 4, "{rows:?}");
    assert_eq!(rows[0]["kind"], "activation");
    assert_eq!(rows[1]["kind"], "first_attempt");
    assert_eq!(rows[2]["kind"], "appeal");
    assert_eq!(rows[3]["kind"], "first_accept");
    assert!(rows.iter().all(|row| row["format_version"] == 1));
    let elapsed = rows[3]["completed_at_unix_ms"]
        .as_u64()
        .unwrap()
        .saturating_sub(rows[1]["started_at_unix_ms"].as_u64().unwrap());
    assert_eq!(
        harm["measurements"]["median_time_to_first_accepted_ms"], elapsed,
        "report latency must be derived from the durable rows: {harm}"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&audit_file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&adjudications_file)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    stop_node(node, serving);
    std::fs::remove_dir_all(&work).ok();
}
