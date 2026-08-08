//! The repeatable submit-path harness (optimization plan S0.3).
//!
//! PHASE0.md records ~1,130 signed ops/s through `/api/submit-batch`, but
//! that run was a one-off against a real upstream: it cannot be re-run on
//! demand, so it cannot gate anything. This is the version that can.
//!
//! **What is measured.** Everything the daemon does with a submission
//! except HTTP transport: JSON parse, hex decode, ed25519 verify, the
//! view-CAS trial apply, BLAKE3 entry hashing, the `FileLog` append, and
//! the sequencer channel round-trip. Client-side signing and hex encoding
//! are done up front and deliberately excluded — they are the load
//! generator's cost, not the server's.
//!
//! Transport is excluded on purpose. PHASE0.md:75 records that curl per op
//! costs 15-20 ms and dominated the pre-batch numbers; leaving it in would
//! measure `curl` rather than anything this workspace can optimize. The
//! ratio between the two paths is the point of `batch_beats_single`.
//!
//! `FileLog` rather than `MemLog` because the recorded figure was "incl.
//! verify + CAS + FileLog" and the append syscall is part of what S1.2
//! proposes to amortize.
//!
//! Run it as a report:
//!
//! ```text
//! cargo test -p choir-node --test throughput --release -- --nocapture
//! ```

use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use choir_hash::ContentHash;
use choir_identity::{ActorKey, Registry};
use choir_node::platform::hex_encode;
use choir_node::Platform;
use choir_oplog::FileLog;
use choir_view::{OpKind, ViewOp};

/// Concurrent clients. Matches the 32 the ForgeMark sweep and
/// `choir-sequencer/tests/concurrency.rs` both use.
const CLIENTS: usize = 32;

/// Ops per client. 32 x 40 gives 1,280 samples, enough for the reported
/// percentiles to mean something, while keeping the debug-profile run --
/// which is what `cargo test --workspace` uses -- from dominating the
/// suite. Every op here costs a real fsync.
const OPS_PER_CLIENT: usize = 40;

/// Ops per `/api/submit-batch` body. The bridge chunks at 500 (PHASE0.md
/// :76), so the batch measurement uses the same shape it was measured at.
const BATCH: usize = 500;

/// Scratch directory that cleans itself up, so a failed run does not leave
/// a log behind to be replayed into the next one.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "choir-throughput-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        Self(dir)
    }

    fn log_path(&self) -> std::path::PathBuf {
        self.0.join("ops.jsonl")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// Nearest-rank percentile over a sorted slice — the same shape
/// `choir-spike` and `concurrency.rs` already use, so the numbers this
/// prints are comparable with the ones PHASE0.md records.
fn percentile(sorted: &[Duration], q: f64) -> Duration {
    sorted[((sorted.len() - 1) as f64 * q) as usize]
}

/// One client's keypair plus the pre-signed bodies it will submit.
struct Client {
    workspace: String,
    bodies: Vec<String>,
}

/// Mints `count` clients, each with a real ed25519 key registered in
/// `registry`, and pre-signs `ops` submissions apiece.
///
/// Every client only ever moves its own workspace head, so the CAS `prev`
/// chain is sequential per client and no two clients contend for the same
/// view key. Rejections would otherwise be measuring the reject path.
fn mint_clients(registry: &mut Registry, count: usize, ops: usize) -> Vec<Client> {
    (0..count)
        .map(|c| {
            let key = ActorKey::generate();
            registry
                .register(&key.public_key_bytes())
                .expect("generated key is valid");
            let workspace = format!("ws{c}");
            let mut prev: Option<ContentHash> = None;
            let bodies = (0..ops)
                .map(|i| {
                    let commit = ContentHash::blake3(format!("{workspace}-{i}").as_bytes());
                    let op = ViewOp::new(OpKind::SetWorkspaceHead {
                        workspace: workspace.clone(),
                        commit: commit.clone(),
                        prev: prev.replace(commit),
                    });
                    submit_body(&key, &workspace, &op)
                })
                .collect();
            Client { workspace, bodies }
        })
        .collect()
}

/// The exact JSON `/api/submit` accepts, signed the way a real client
/// signs it.
fn submit_body(key: &ActorKey, workspace: &str, op: &ViewOp) -> String {
    let payload = op.to_payload();
    let sig = key.sign_submission(workspace, &payload);
    serde_json::json!({
        "workspace": workspace,
        "payload_hex": hex_encode(&payload),
        "key_id": sig.key_id,
        "signature_hex": hex_encode(&sig.signature),
    })
    .to_string()
}

/// Boots a platform over a fresh `FileLog` with `registry` as its trusted
/// key set.
fn platform_over(scratch: &Scratch, registry: Registry) -> Arc<Platform> {
    let log = FileLog::open(&scratch.log_path()).expect("open log");
    Arc::new(
        Platform::start(registry, Box::new(log), ActorKey::generate()).expect("platform starts"),
    )
}

/// The headline number: signed ops/s and decision-latency percentiles
/// through `/api/submit` at [`CLIENTS`] concurrency.
///
/// Reports only. Nothing wall-clock is asserted here -- see the note at the
/// end of the function for why the Phase-0 gate assertion that used to
/// live here was measuring the wrong quantity.
#[test]
fn single_submit_throughput_and_latency() {
    let scratch = Scratch::new("single");
    let mut registry = Registry::new();
    let clients = mint_clients(&mut registry, CLIENTS, OPS_PER_CLIENT);
    let platform = platform_over(&scratch, registry);

    let barrier = Arc::new(Barrier::new(CLIENTS));
    let started = Instant::now();
    // The collect is load-bearing and clippy's needless_collect is wrong
    // here: it forces every thread to spawn before any is joined. Consumed
    // lazily, the iterator would spawn one client, join it, spawn the
    // next -- serialising the run and measuring nothing about concurrency.
    // It would also deadlock, because the barrier waits for all CLIENTS.
    #[allow(clippy::needless_collect)]
    let threads: Vec<_> = clients
        .into_iter()
        .map(|client| {
            let platform = platform.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut latencies = Vec::with_capacity(client.bodies.len());
                barrier.wait();
                for body in &client.bodies {
                    let t = Instant::now();
                    let (status, out) = platform.handle_api(
                        "POST",
                        "/api/submit",
                        std::hint::black_box(body.as_bytes()),
                    );
                    latencies.push(t.elapsed());
                    assert_eq!(status, 200, "{} rejected: {out}", client.workspace);
                    std::hint::black_box(out);
                }
                latencies
            })
        })
        .collect();

    let mut latencies: Vec<Duration> = threads
        .into_iter()
        .flat_map(|t| t.join().expect("client thread"))
        .collect();
    let wall = started.elapsed();
    latencies.sort_unstable();

    let total = CLIENTS * OPS_PER_CLIENT;
    assert_eq!(latencies.len(), total, "every op accounted for");
    println!(
        "/api/submit: {total} ops, {CLIENTS} clients, {:.0} ops/s wall {:?}",
        total as f64 / wall.as_secs_f64(),
        wall
    );
    println!(
        "  latency p50={:?} p90={:?} p99={:?} max={:?}",
        percentile(&latencies, 0.5),
        percentile(&latencies, 0.9),
        percentile(&latencies, 0.99),
        latencies.last().expect("non-empty")
    );
    // Deliberately no wall-clock assertion, and the earlier one here was a
    // mistake worth naming. It asserted the Phase-0 gate ("p99 decision
    // latency < 100 ms") against this figure, but this is not that figure:
    // the gate measures in-writer decision latency, while this is
    // client-observed latency behind 32-way contention and a durability
    // barrier. Little's law alone puts it at clients / throughput.
    //
    // It duly passed release-isolated (6.2 ms) and failed at 116 ms in a
    // debug full-suite run where other test binaries were competing for
    // the same disk -- a flaky test measuring the machine, not the code.
    //
    // The gate keeps its assertion where it belongs, on the metric it
    // names, in choir-sequencer/tests/concurrency.rs. This stays a
    // reporting harness, and the regression guards that can actually hold
    // are the barrier-count and allocation-count tests.
}

/// The `/api/submit-batch` path, in the 500-op chunks the bridge uses —
/// the configuration PHASE0.md:76 measured at ~1,130 signed ops/s.
///
/// Single-threaded on purpose: the batch endpoint is what the bridge
/// drives, and the bridge is one process feeding one daemon.
#[test]
fn batch_submit_throughput() {
    let scratch = Scratch::new("batch");
    let mut registry = Registry::new();
    let clients = mint_clients(&mut registry, CLIENTS, OPS_PER_CLIENT);
    let platform = platform_over(&scratch, registry);

    // Interleave clients so the batch looks like a real mixed workload
    // rather than one workspace advanced 100 times in a row.
    let mut ops: Vec<&str> = Vec::new();
    for i in 0..OPS_PER_CLIENT {
        for client in &clients {
            ops.push(&client.bodies[i]);
        }
    }
    let bodies: Vec<String> = ops
        .chunks(BATCH)
        .map(|chunk| format!(r#"{{"ops":[{}]}}"#, chunk.join(",")))
        .collect();

    let total = ops.len();
    let started = Instant::now();
    let mut accepted = 0u64;
    for body in &bodies {
        let (status, out) =
            platform.handle_api("POST", "/api/submit-batch", std::hint::black_box(body.as_bytes()));
        assert_eq!(status, 200, "batch rejected: {out}");
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("batch reply is json");
        accepted += parsed["accepted"].as_u64().expect("accepted count");
        std::hint::black_box(out);
    }
    let wall = started.elapsed();

    assert_eq!(accepted, total as u64, "every op in every batch admitted");
    println!(
        "/api/submit-batch: {total} ops in {} batches of {BATCH}, {:.0} ops/s wall {:?}",
        bodies.len(),
        total as f64 / wall.as_secs_f64(),
        wall
    );
}

/// Reports what batching is worth on this machine, over an identical op
/// set. Recorded as a ratio because the absolute figures move with the
/// host; the ratio is the thing S1.2 (group commit) has to improve.
#[test]
fn batch_beats_single() {
    let ops = 1_000;
    let one = time_path(false, ops);
    let many = time_path(true, ops);
    println!(
        "same {ops} ops: single {:.0} ops/s, batched {:.0} ops/s ({:.2}x)",
        ops as f64 / one.as_secs_f64(),
        ops as f64 / many.as_secs_f64(),
        one.as_secs_f64() / many.as_secs_f64()
    );
}

/// Hypothesis 1 from the optimization brief: is `ChoirPolicy::check`'s
/// full-`View` clone actually the bottleneck, or only a scaling hazard?
///
/// The clone is O(total view state) per submission, so if it dominates,
/// per-op cost must grow with the number of refs already in the view. This
/// measures the same op against views of very different sizes and prints
/// the ratio. A flat ratio means the clone is not hot at that scale and no
/// throughput win may be claimed for removing it; a ratio tracking the
/// size ratio confirms it.
///
/// Reported, not asserted. The point is to produce a number that decides
/// what Stage 1 does first, and a wall-clock threshold here would be a
/// flaky test on a laptop.
#[test]
fn view_clone_cost_scales_with_view_size() {
    let small = per_op_cost_at_view_size(100);
    let large = per_op_cost_at_view_size(5_000);
    println!(
        "per-op cost vs view size: 100 refs {:?}/op, 5000 refs {:?}/op ({:.2}x for 50x the state)",
        small,
        large,
        large.as_secs_f64() / small.as_secs_f64()
    );
}

/// Fills a view with `refs` distinct refs, then times a further run of
/// ops against it and returns the mean cost of one op.
fn per_op_cost_at_view_size(refs: usize) -> Duration {
    let scratch = Scratch::new(&format!("view{refs}"));
    let mut registry = Registry::new();
    let key = ActorKey::generate();
    registry
        .register(&key.public_key_bytes())
        .expect("generated key is valid");
    let platform = platform_over(&scratch, registry);

    // Load the view up. Distinct ref names, each created once, so the
    // view ends up holding `refs` entries.
    for i in 0..refs {
        let op = ViewOp::new(OpKind::SetRef {
            name: format!("repo.git:refs/heads/b{i}"),
            commit: ContentHash::blake3(format!("c{i}").as_bytes()),
            prev: None,
        });
        let body = submit_body(&key, "loader", &op);
        let (status, out) = platform.handle_api("POST", "/api/submit", body.as_bytes());
        assert_eq!(status, 200, "view fill rejected: {out}");
    }

    // Now measure ops against that loaded view, on a workspace key the
    // view has never seen, so the work per op is identical in both runs
    // and only the size of the state being cloned differs.
    let measured = 500;
    let mut prev: Option<ContentHash> = None;
    let bodies: Vec<String> = (0..measured)
        .map(|i| {
            let commit = ContentHash::blake3(format!("m{i}").as_bytes());
            let op = ViewOp::new(OpKind::SetWorkspaceHead {
                workspace: "measured".to_string(),
                commit: commit.clone(),
                prev: prev.replace(commit),
            });
            submit_body(&key, "measured", &op)
        })
        .collect();

    let started = Instant::now();
    for body in &bodies {
        let (status, out) =
            platform.handle_api("POST", "/api/submit", std::hint::black_box(body.as_bytes()));
        assert_eq!(status, 200, "measured op rejected: {out}");
        std::hint::black_box(out);
    }
    started.elapsed() / measured as u32
}

/// Submits `count` identical ops down either endpoint and returns the wall
/// time, so the two paths can be compared over the same work.
fn time_path(batched: bool, count: usize) -> Duration {
    let scratch = Scratch::new(if batched { "cmp-batch" } else { "cmp-single" });
    let mut registry = Registry::new();
    let clients = mint_clients(&mut registry, 1, count);
    let platform = platform_over(&scratch, registry);
    let bodies = &clients[0].bodies;

    let started = Instant::now();
    if batched {
        for chunk in bodies.chunks(BATCH) {
            let body = format!(r#"{{"ops":[{}]}}"#, chunk.join(","));
            let (status, out) = platform.handle_api("POST", "/api/submit-batch", body.as_bytes());
            assert_eq!(status, 200);
            std::hint::black_box(out);
        }
    } else {
        for body in bodies {
            let (status, out) = platform.handle_api("POST", "/api/submit", body.as_bytes());
            assert_eq!(status, 200);
            std::hint::black_box(out);
        }
    }
    started.elapsed()
}
