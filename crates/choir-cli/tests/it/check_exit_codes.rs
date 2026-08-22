//! The exit codes for `choir checks` (D49, extended by D18).
//!
//! The rest of this binary answers with two codes, 0 accepted and 1
//! rejected, which is right for a command that submits something. A
//! command that asks "may I land this" has four answers, and three of
//! them are not "no": land it, do not land it, not yet, and could not
//! be run. An agent that cannot tell the third from the second either
//! gives up on work that was about to go green, or busy-waits on a
//! build that already failed. One that cannot tell the fourth from the
//! third waits forever on a check that never started.
//!
//! Driven through the real binary against a real node, because the
//! contract under test is the process exit status and nothing below the
//! process boundary can assert it.

use choir_identity::{ActorKey, Registry};
use choir_node::{Node, Platform};
use choir_oplog::MemLog;

fn choir(args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_choir"))
        .args(args)
        .output()
        .expect("choir runs")
}

fn code(out: &std::process::Output) -> i32 {
    out.status.code().expect("the process exited normally")
}

#[test]
fn checks_answers_land_do_not_land_and_not_yet() {
    let work = std::env::temp_dir().join("choir-cli-check-exit-codes");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).expect("temp root");

    let key_file = work.join("agent.key");
    let key_file = key_file.to_str().expect("utf-8 path");
    let minted = choir(&["key", key_file]);
    assert_eq!(code(&minted), 0);
    let pub_hex = String::from_utf8_lossy(&minted.stdout).trim().to_string();
    let key_bytes: [u8; 32] = choir_node::platform::hex_decode(&pub_hex)
        .expect("hex")
        .try_into()
        .expect("32 bytes");

    let mut registry = Registry::new();
    registry.register(&key_bytes).expect("register");
    let mut node = Node::bind(&work.join("repos"), 0).expect("node binds");
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate())
            .expect("platform starts"),
    );
    node.create_repo("agents/demo.git").expect("repo");
    let port = node.port();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    let api = format!("http://127.0.0.1:{port}");
    let oid = "1111111111111111111111111111111111111111";
    let target = "agents/demo.git:refs/heads/main";

    // Nothing reported. Exit 1, and the body says which kind of 1 this
    // is, because "no runner is configured" and "the build broke" both
    // mean do-not-land and want opposite follow-up.
    let out = choir(&["checks", &api, oid]);
    assert_eq!(code(&out), 1, "an unchecked commit must not read as green");
    let body: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("the command prints JSON");
    assert_eq!(body["verdict"], "unreported", "{body}");

    let report = |name: &str, status: &str| {
        let out = choir(&[
            "check",
            &api,
            key_file,
            "ci/runner",
            oid,
            name,
            status,
            "run-1",
            "--ref",
            target,
        ]);
        assert_eq!(
            code(&out),
            0,
            "reporting failed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    };

    // Still running: exit 3, the answer that is neither yes nor no.
    report("ci/build", "running");
    let out = choir(&["checks", &api, oid]);
    assert_eq!(code(&out), 3, "a running check must not read as decided");
    let body: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON");
    assert_eq!(body["verdict"], "running", "{body}");

    // Passed: exit 0.
    report("ci/build", "passed");
    let out = choir(&["checks", &api, oid]);
    assert_eq!(code(&out), 0, "a passing check must read as green");
    let body: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON");
    assert_eq!(body["verdict"], "passed", "{body}");

    // A second check fails while a third is still running. Failure wins:
    // waiting on the running one would be waiting for an outcome that
    // cannot change the answer.
    report("ci/lint", "failed");
    report("ci/docs", "running");
    let out = choir(&["checks", &api, oid]);
    assert_eq!(code(&out), 1, "a failure beside a running check must be 1");
    let body: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON");
    assert_eq!(body["verdict"], "failed", "{body}");
    assert_eq!(
        body["checks"].as_object().expect("checks map").len(),
        3,
        "the report must still list every check: {body}"
    );

    // A check that could not run. Exit 4, its own code, because the
    // follow-up differs from every other answer: not fix it (1), not
    // wait for it (3), but run it again. It outranks the running check
    // beside it for the reason a failure does -- waiting will not make
    // a check that never started report -- and yields to the failure,
    // which is about the commit rather than about us.
    report("ci/lint", "errored");
    report("ci/docs", "errored");
    report("ci/build", "errored");
    let out = choir(&["checks", &api, oid]);
    assert_eq!(
        code(&out),
        4,
        "a check that could not run must have its own code"
    );
    let body: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON");
    assert_eq!(body["verdict"], "errored", "{body}");

    report("ci/docs", "running");
    let out = choir(&["checks", &api, oid]);
    assert_eq!(code(&out), 4, "errored must outrank running");
    report("ci/docs", "failed");
    let out = choir(&["checks", &api, oid]);
    assert_eq!(code(&out), 1, "a failure must outrank errored");

    // Usage errors stay 2 and are not confused with any of the above.
    let out = choir(&["check", &api, key_file, "ci/runner", oid, "n", "PASSED"]);
    assert_eq!(code(&out), 2, "an upcased status must be a usage error");
    let out = choir(&["checks", &api, "not-an-oid"]);
    assert_eq!(code(&out), 2, "a malformed oid must be a usage error");

    node.unblock();
    let _ = std::fs::remove_dir_all(&work);
}
