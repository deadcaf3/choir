//! The decision journal, over a real node: what the writer decided and
//! why, as JSONL a person can read with `jq`.
//!
//! The journal is derived data — nothing replays it and no hash covers
//! it — so these tests assert what a reader gets, not what the log
//! holds. The point of each is that a fact reaching the file is one no
//! `/api/view` response carries: a *refusal* leaves no trace in the view
//! by construction, and contention leaves none either.

use choir_identity::{ActorKey, Registry};
use choir_node::{Node, Platform};
use choir_oplog::MemLog;
use choir_view::{OpKind, ViewOp};

use crate::support::{curl, submit_body};

/// A private directory per test: this harness shares a process and runs
/// on parallel threads, so a shared temp-dir name is a flake.
fn workdir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "choir-journal-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn serve(platform: Platform, dir: &std::path::Path) -> (std::sync::Arc<Node>, u16) {
    let mut node = Node::bind(&dir.join("repos"), 0).unwrap();
    node.enable_platform(platform);
    let port = node.port();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    (node, port)
}

fn set_ref(name: &str, prev: Option<choir_oplog::ContentHash>) -> ViewOp {
    set_ref_to(name, name, prev)
}

/// A ref update naming a specific commit, so two submissions can race
/// the *same* ref with *different* values. Resubmitting identical signed
/// bytes is the idempotency path (`already_applied`), not a CAS failure
/// -- which this test learned the hard way.
fn set_ref_to(name: &str, seed: &str, prev: Option<choir_oplog::ContentHash>) -> ViewOp {
    ViewOp::new(OpKind::SetRef {
        name: name.into(),
        commit: choir_oplog::ContentHash::blake3(seed.as_bytes()),
        prev,
    })
}

/// Reads the journal as parsed lines. Every line must be valid JSON on
/// its own — that is the whole contract of JSONL, and a half-written
/// record would break `jq` for every line after it.
fn read_journal(path: &std::path::Path) -> Vec<serde_json::Value> {
    let body = std::fs::read_to_string(path).expect("journal file exists");
    body.lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("bad JSONL line: {e}: {line}")))
        .collect()
}

/// An acceptance and a refusal both reach the file, and the refusal
/// carries its reason. The refusal is the interesting half: it never
/// appears in `/api/view`, so before this the only record of *why* a
/// client was turned away was the client's own copy of the answer.
#[test]
fn both_an_acceptance_and_a_refusal_reach_the_journal() {
    let dir = workdir("decisions");
    let path = dir.join("ops.jsonl");
    std::fs::remove_file(&path).ok();

    let alice = ActorKey::generate();
    let mallory = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&alice.public_key_bytes()).unwrap();
    let platform = Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate())
        .unwrap()
        .with_journal(&path)
        .expect("journal opens");
    let (_node, port) = serve(platform, &dir);
    let api = format!("http://127.0.0.1:{port}/api");

    let (code, resp) = curl(&[
        "-X", "POST", "-d", &submit_body(&alice, "alice", &set_ref("main", None)),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 200, "{resp}");

    // An untrusted key: refused before it is ordered.
    let (code, resp) = curl(&[
        "-X", "POST", "-d", &submit_body(&mallory, "mallory", &set_ref("other", None)),
        &format!("{api}/submit"),
    ]);
    assert_ne!(code, 200, "an unknown key must not be admitted: {resp}");

    // One more accepted op: the journal thread writes on its own
    // schedule, and this both flushes the burst and proves ordering.
    let (code, resp) = curl(&[
        "-X", "POST", "-d", &submit_body(&alice, "alice", &set_ref("second", None)),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 200, "{resp}");

    let decisions: Vec<serde_json::Value> = read_journal(&path)
        .into_iter()
        .filter(|v| v["kind"] == "decision")
        .collect();

    let accepted: Vec<&serde_json::Value> = decisions
        .iter()
        .filter(|v| v["decision"] == "accepted")
        .collect();
    assert_eq!(accepted.len(), 2, "both acceptances recorded: {decisions:?}");
    assert_eq!(accepted[0]["seq"], 0);
    assert_eq!(accepted[0]["op_type"], "SetRef");
    assert!(
        accepted[0]["actor_id"].as_str().is_some_and(|s| !s.is_empty()),
        "the verified author is carried, not re-derived"
    );

    let rejected: Vec<&serde_json::Value> = decisions
        .iter()
        .filter(|v| v["decision"] == "rejected")
        .collect();
    assert_eq!(rejected.len(), 1, "the refusal was recorded: {decisions:?}");
    assert!(
        rejected[0]["reject_reason"]
            .as_str()
            .is_some_and(|s| s.contains("unknown_key") || s.contains("signature")),
        "the refusal carries why: {:?}",
        rejected[0]["reject_reason"]
    );
    assert!(rejected[0]["seq"].is_null(), "nothing was ordered");
}

/// Two writers racing one ref produce a `cas_failure` alongside the
/// refusal. Recorded separately on purpose: a rejection count alone
/// cannot tell contention from a client sending nonsense, and only the
/// first says anything about load.
#[test]
fn a_lost_race_on_one_ref_is_recorded_as_contention() {
    let dir = workdir("cas");
    let path = dir.join("ops.jsonl");
    std::fs::remove_file(&path).ok();

    let alice = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&alice.public_key_bytes()).unwrap();
    let platform = Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate())
        .unwrap()
        .with_journal(&path)
        .expect("journal opens");
    let (_node, port) = serve(platform, &dir);
    let api = format!("http://127.0.0.1:{port}/api");

    // First writer takes the ref.
    let (code, resp) = curl(&[
        "-X", "POST", "-d", &submit_body(&alice, "alice", &set_ref("main", None)),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 200, "{resp}");

    // A different value for the same ref, still believing it unset: the
    // CAS loses. Different bytes, so this is a genuine race rather than
    // the idempotent replay of the first submission.
    let (code, resp) = curl(&[
        "-X", "POST", "-d",
        &submit_body(&alice, "alice", &set_ref_to("main", "rival", None)),
        &format!("{api}/submit"),
    ]);
    assert_ne!(code, 200, "a stale prev must not be admitted: {resp}");

    // Flush the journal thread with a decision that must land after it.
    let (_, _) = curl(&[
        "-X", "POST", "-d", &submit_body(&alice, "alice", &set_ref("flush", None)),
        &format!("{api}/submit"),
    ]);

    let lines = read_journal(&path);
    let cas: Vec<&serde_json::Value> =
        lines.iter().filter(|v| v["kind"] == "cas_failure").collect();
    assert_eq!(cas.len(), 1, "one lost race, one contention record: {lines:?}");
    assert!(
        cas[0]["actual"].as_str().is_some(),
        "the record names what the ref actually was, which is what a \
         retry needs: {:?}",
        cas[0]
    );

    // And the refusal is still a decision line of its own: the two are
    // different facts about the same event, not one written twice.
    assert!(
        lines
            .iter()
            .any(|v| v["kind"] == "decision" && v["decision"] == "rejected"),
        "the refusal is recorded as a decision as well"
    );
}

/// Without the flag, nothing is written and nothing is paid. The second
/// half matters more than the first: building a record costs several
/// allocations on the writer thread, and `submit_path_allocation_budget`
/// failed when this path was unguarded.
#[test]
fn a_node_without_the_flag_writes_no_journal() {
    let dir = workdir("absent");
    let path = dir.join("ops.jsonl");
    std::fs::remove_file(&path).ok();

    let alice = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&alice.public_key_bytes()).unwrap();
    let platform =
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate()).unwrap();
    let (_node, port) = serve(platform, &dir);
    let api = format!("http://127.0.0.1:{port}/api");

    let (code, resp) = curl(&[
        "-X", "POST", "-d", &submit_body(&alice, "alice", &set_ref("main", None)),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 200, "{resp}");
    assert!(
        !path.exists(),
        "a node given no journal path must not create one"
    );
}
