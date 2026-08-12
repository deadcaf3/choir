//! What a running node says about itself: the commit it was built from,
//! and whether it is meeting the merge-decision latency gate on the
//! traffic it is actually serving.
//!
//! The second test sets the gate to zero. That is the whole point: a gate
//! only checked at 100 ms on an idle test machine never fires, so nothing
//! proves the path from a slow op to the operator's lag log exists at all.
//! Zero makes every op a breach deterministically, without asserting on
//! wall-clock time (which this shared harness forbids).

use choir_identity::{ActorKey, Registry};
use choir_node::{Node, Platform};
use choir_oplog::MemLog;
use choir_view::{OpKind, ViewOp};

use crate::support::{curl, submit_body};

/// Starts a platform node over a `MemLog`, returning it plus the port.
fn node_with(platform: Platform) -> (std::sync::Arc<Node>, u16, ActorKey) {
    let work = std::env::temp_dir().join(format!(
        "choir-node-observability-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&work).unwrap();
    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
    node.enable_platform(platform);
    let port = node.port();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    (node, port, ActorKey::generate())
}

fn set_ref(name: &str) -> ViewOp {
    ViewOp::new(OpKind::SetRef {
        name: name.into(),
        commit: choir_oplog::ContentHash::blake3(name.as_bytes()),
        prev: None,
    })
}

#[test]
fn the_view_reports_the_build_and_the_latency_gate() {
    let alice = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&alice.public_key_bytes()).unwrap();
    let platform =
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate()).unwrap();
    let (_node, port, _) = node_with(platform);
    let api = format!("http://127.0.0.1:{port}/api");

    // Before any traffic: percentiles are absent rather than zero. Zero
    // would read as "comfortably inside the gate" on a node that has
    // measured nothing at all.
    let (code, before) = curl(&[&format!("{api}/view")]);
    assert_eq!(code, 200, "{before}");
    assert_eq!(before["sequencer_lag"]["observed_ops"], 0);
    assert!(before["sequencer_lag"]["durable"]["p99_us"].is_null(), "{before}");
    assert_eq!(before["sequencer_lag"]["gate_us"], 100_000);

    let (code, resp) = curl(&[
        "-X", "POST", "-d", &submit_body(&alice, "alice", &set_ref("main")),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 200, "{resp}");

    let (code, after) = curl(&[&format!("{api}/view")]);
    assert_eq!(code, 200, "{after}");
    let lag = &after["sequencer_lag"];
    assert_eq!(lag["observed_ops"], 1, "the accepted op must be measured: {lag}");
    assert!(lag["durable"]["p99_us"].as_u64().is_some(), "{lag}");
    // Durable includes the barrier that decision excludes, so it can
    // never be the smaller of the two.
    assert!(
        lag["durable"]["max_us"].as_u64().unwrap() >= lag["decision"]["max_us"].as_u64().unwrap(),
        "{lag}"
    );
    assert_eq!(lag["durable"]["breaches"], 0, "a local MemLog op is not a breach: {lag}");
    // No lag log configured here, so the node says so rather than
    // implying breaches are being recorded somewhere.
    assert_eq!(lag["log_configured"], false, "{lag}");

    // The build stamp. Its value depends on how this binary was built, so
    // the assertion is on shape: a commit or an explicit unknown, never a
    // plausible-looking placeholder.
    let build = &after["build"];
    let commit = build["commit"].as_str().expect("commit is a string");
    assert!(
        commit == "unknown" || (commit.len() == 40 && commit.chars().all(|c| c.is_ascii_hexdigit())),
        "build stamp must be a commit or an explicit unknown: {build}"
    );
    assert!(
        ["env", "git", "unavailable"].contains(&build["source"].as_str().unwrap()),
        "{build}"
    );
    assert_eq!(commit, choir_node::BUILD_COMMIT);
}

#[test]
fn a_breached_gate_reaches_the_operators_lag_log() {
    let work = std::env::temp_dir().join(format!(
        "choir-node-lag-log-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&work).unwrap();
    let lag_log = work.join("lag.jsonl");
    std::fs::remove_file(&lag_log).ok();

    let alice = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&alice.public_key_bytes()).unwrap();
    let platform = Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate())
        .unwrap()
        .with_lag_log(lag_log.clone());
    // Every accepted op now misses the gate.
    platform.lag().set_gate(std::time::Duration::ZERO);
    let (_node, port, _) = node_with(platform);
    let api = format!("http://127.0.0.1:{port}/api");

    let (code, resp) = curl(&[
        "-X", "POST", "-d", &submit_body(&alice, "alice", &set_ref("main")),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 200, "{resp}");

    // Breaches are drained by the accept loop, one request behind: this
    // request performs the drain, the next one can see its effect.
    let (_, _) = curl(&[&format!("{api}/view")]);
    let (code, view) = curl(&[&format!("{api}/view")]);
    assert_eq!(code, 200, "{view}");
    let lag = &view["sequencer_lag"];
    assert_eq!(lag["durable"]["breaches"], 1, "{lag}");
    assert_eq!(lag["log_write_failures"], 0, "{lag}");
    // The path is deliberately not in the response (it names the
    // operator's home directory); only that one is configured.
    assert_eq!(lag["log_configured"], true, "{lag}");

    let written = std::fs::read_to_string(&lag_log).expect("the breach reached the file");
    let record: serde_json::Value =
        serde_json::from_str(written.lines().next().expect("one line per breach")).unwrap();
    assert_eq!(record["event"], "gate_breach");
    assert_eq!(record["seq"], 0, "a breach names the op it happened to: {record}");
    assert_eq!(record["gate_us"], 0);
    assert_eq!(record["batch"], 1);
    assert!(record["durable_us"].as_u64().is_some(), "{record}");
}
