//! `choir batch` (D17): many operations, one durability barrier, one
//! result line per op.
//!
//! The node has told agents since D26 that `/api/submit-batch` is the
//! primary path for their workloads. Until this command existed the CLI
//! could not reach it, so an agent taking that advice had to hand-roll
//! ed25519 signing and `curl` — the thing this binary exists to prevent.
//! These tests are about the properties that make it usable from a
//! script: order, partial failure, and an exit code that means something.

use choir_identity::{ActorKey, Registry};
use choir_node::{Node, Platform};
use choir_oplog::MemLog;

fn choir(args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_choir"))
        .args(args)
        .output()
        .expect("choir runs")
}

/// A served node whose registry trusts one freshly minted key, plus the
/// key file and the API base.
fn served(tag: &str) -> (String, String, std::path::PathBuf) {
    let work = std::env::temp_dir().join(format!("choir-cli-batch-{tag}"));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");
    let key_file = work.join("agent.key");
    let out = choir(&["key", key_file.to_str().expect("utf-8")]);
    assert!(out.status.success(), "key: {out:?}");
    let pub_hex = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let bytes: [u8; 32] = (0..pub_hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&pub_hex[i..i + 2], 16).expect("hex"))
        .collect::<Vec<u8>>()
        .try_into()
        .expect("32 bytes");

    let mut registry = Registry::new();
    registry.register(&bytes).expect("valid key");
    let mut node = Node::bind(&work.join("repos"), 0).expect("node binds");
    let port = node.port();
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate())
            .expect("platform starts"),
    );
    std::thread::spawn(move || node.serve_forever());
    (
        format!("http://127.0.0.1:{port}"),
        key_file.to_string_lossy().to_string(),
        work,
    )
}

/// One op per line, `SetRef` against a distinct name each time so all of
/// them can be admitted.
fn ops(names: &[&str]) -> String {
    names
        .iter()
        .map(|name| {
            serde_json::json!({
                "format_version": 1,
                "kind": { "SetRef": {
                    "name": name,
                    "commit": {
                        "codec": 30,
                        "digest": choir_oplog::ContentHash::blake3(name.as_bytes()).digest,
                    },
                    "prev": null,
                }},
            })
            .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The whole point: many ops, one invocation, results in request order,
/// and every one of them in the log afterwards.
#[test]
fn a_batch_submits_every_op_in_order_and_reports_one_line_each() {
    let (api, key_file, work) = served("order");
    let file = work.join("ops.jsonl");
    // A trailing newline on purpose: a generated file has one, and it
    // must not read as an empty final operation.
    std::fs::write(&file, format!("{}\n", ops(&["one", "two", "three"]))).expect("ops file");

    let out = choir(&[
        "batch",
        &api,
        &key_file,
        "cli-agent",
        file.to_str().expect("utf-8"),
    ]);
    assert!(out.status.success(), "batch failed: {out:?}");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines.len(), 3, "one line per op: {lines:?}");
    let seqs: Vec<u64> = lines
        .iter()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).expect("a result object")["seq"]
                .as_u64()
                .expect("an accepted op carries its seq")
        })
        .collect();
    // Request order is admission order, which is the property a script
    // reading line n for op n depends on.
    assert_eq!(seqs, vec![0, 1, 2], "results are not in request order");

    // The totals go to stderr, so stdout stays parseable.
    let summary = String::from_utf8_lossy(&out.stderr);
    assert!(summary.contains("3 accepted"), "no summary: {summary}");
    assert!(summary.contains("0 rejected"), "{summary}");

    std::fs::remove_dir_all(&work).ok();
}

/// Partial failure is the case a batch exists to make survivable, and an
/// exit code cannot carry which op failed. The lines can.
#[test]
fn a_refused_op_is_named_by_position_and_the_exit_code_is_nonzero() {
    let (api, key_file, work) = served("partial");
    let file = work.join("ops.jsonl");
    // Two ops naming the same ref with a null `prev`: the first is
    // admitted, the second fails the compare-and-set behind it.
    let mut lines = ops(&["shared", "other"]);
    lines.push('\n');
    lines.push_str(&ops(&["shared"]));
    std::fs::write(&file, lines).expect("ops file");

    let out = choir(&[
        "batch",
        &api,
        &key_file,
        "cli-agent",
        file.to_str().expect("utf-8"),
    ]);
    assert!(
        !out.status.success(),
        "a batch with a refused op exited 0: {out:?}"
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let results: Vec<serde_json::Value> = stdout
        .trim()
        .lines()
        .map(|l| serde_json::from_str(l).expect("a result object"))
        .collect();
    assert_eq!(results.len(), 3, "a result is missing: {stdout}");
    assert!(
        results[0]["seq"].is_u64(),
        "op 1 should have landed: {stdout}"
    );
    assert!(
        results[1]["seq"].is_u64(),
        "op 2 should have landed: {stdout}"
    );
    assert!(
        results[2]["error"].is_string(),
        "the third op should have been refused: {stdout}"
    );
    // The ops that did land, landed. A batch is not a transaction, and
    // the results say so rather than the exit code implying otherwise.
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("2 accepted, 1 rejected"),
        "{:?}",
        String::from_utf8_lossy(&out.stderr)
    );

    std::fs::remove_dir_all(&work).ok();
}

/// A malformed line is named by its line number, before anything is
/// signed or sent. "bad op json" would not say which of forty.
#[test]
fn a_malformed_line_is_named_and_nothing_is_submitted() {
    let (api, key_file, work) = served("malformed");
    let file = work.join("ops.jsonl");
    std::fs::write(
        &file,
        format!("{}\nnot json at all\n{}", ops(&["a"]), ops(&["b"])),
    )
    .expect("ops file");

    let out = choir(&[
        "batch",
        &api,
        &key_file,
        "cli-agent",
        file.to_str().expect("utf-8"),
    ]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "usage failure expected: {out:?}"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains(":2:"), "the bad line is not named: {err}");
    assert!(
        String::from_utf8_lossy(&out.stdout).trim().is_empty(),
        "results were printed for a batch that was never sent"
    );

    // Nothing reached the log: the whole file is parsed before the first
    // signature, so a typo on line 40 does not leave 39 ops admitted.
    let view = std::process::Command::new("curl")
        .args(["-s", &format!("{api}/api/view")])
        .output()
        .expect("curl runs");
    let view: serde_json::Value = serde_json::from_slice(&view.stdout).expect("view is JSON");
    assert_eq!(
        view["refs"].as_object().map(serde_json::Map::len),
        Some(0),
        "a rejected file still submitted something: {view}"
    );

    std::fs::remove_dir_all(&work).ok();
}

/// `choir log --verify` (D17): `SYNC.md`'s three checks as a flag.
///
/// The contract has always been implementable — that is what
/// `choir-node/tests/it/sync_contract.rs` proves, by hand and without
/// calling `content_hash`. What it was not was *reachable*: an agent
/// following the document hand-rolled hash-chain and ed25519 checking.
#[test]
fn verify_checks_the_chain_and_is_honest_about_keys_it_does_not_hold() {
    let (api, key_file, work) = served("verify");
    let file = work.join("ops.jsonl");
    std::fs::write(&file, ops(&["alpha", "beta", "gamma"])).expect("ops file");
    assert!(choir(&[
        "batch",
        &api,
        &key_file,
        "cli-agent",
        file.to_str().expect("utf-8")
    ])
    .status
    .success());

    // Without a key file nothing can be attributed, and the command says
    // so rather than reporting a verified chain. This is the assertion
    // that fails if "unverified" is ever quietly folded into "checked".
    let out = choir(&["log", &api, "--verify"]);
    assert!(out.status.success(), "the chain should hold: {out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("chain holds"), "{err}");
    assert!(err.contains("0 signatures verified"), "{err}");
    assert!(
        err.contains("no key held"),
        "authorship was claimed without a key: {err}"
    );

    // The same page with the key that signed it: now the signatures are
    // actually checked, and the count says how many.
    let pub_hex = String::from_utf8_lossy(&choir(&["key", &key_file]).stdout)
        .trim()
        .to_string();
    let keys = work.join("keys");
    std::fs::write(&keys, format!("cli-agent {pub_hex}\n")).expect("keys file");
    let out = choir(&[
        "log",
        &api,
        "--verify",
        "--keys",
        keys.to_str().expect("utf-8"),
    ]);
    assert!(out.status.success(), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("3 signatures verified"), "{err}");
    assert!(err.contains("0 unverified"), "{err}");

    // Entries go to stdout one per line, like `batch`, so the two
    // commands compose in a pipeline.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout.trim().lines().count(), 3, "{stdout}");
    for line in stdout.trim().lines() {
        let entry: serde_json::Value = serde_json::from_str(line).expect("an entry object");
        assert!(entry["hash"].as_str().is_some(), "{entry}");
    }

    // A cursor past the end is a real state, not an error.
    let out = choir(&["log", &api, "--from", "99", "--verify"]);
    assert!(
        out.status.success() || !String::from_utf8_lossy(&out.stdout).is_empty(),
        "a cursor past the end should not be a chain failure: {out:?}"
    );

    std::fs::remove_dir_all(&work).ok();
}
