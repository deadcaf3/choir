//! Allocation budget for the submit path (optimization plan S0.3).
//!
//! Wall-clock benchmarks on a laptop are too noisy to gate on. Allocation
//! counts are not: for a fixed op, the number of times the submit path
//! calls the allocator is deterministic, so "this op must not allocate
//! more than N times" is a test rather than a hope.
//!
//! This replaces the `dhat-rs` the plan suggested. dhat gives sizes,
//! backtraces and peak-heap profiles; a counting [`GlobalAlloc`] wrapper
//! gives the one number being asserted, in thirty lines and no new
//! dependency. The house rule on dependencies decided it.
//!
//! **Why its own file.** Cargo runs each integration-test file as its own
//! binary but runs the tests *within* a binary on parallel threads, and
//! the counters here are process-global. One test per binary is what makes
//! the count attributable.
//!
//! The ceiling is a ratchet. It is set just above the measured figure, and
//! every optimization that removes an allocation should lower it in the
//! same commit. If it ever needs raising, that is the finding.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use choir_hash::ContentHash;
use choir_identity::{ActorKey, Registry};
use choir_node::platform::hex_encode;
use choir_node::Platform;
use choir_oplog::FileLog;
use choir_view::{OpKind, ViewOp};

/// Allocator calls since process start. `Relaxed` is right here: the
/// measured region is bracketed by a blocking round-trip to the writer
/// thread, which already orders every increment against the read.
static ALLOCS: AtomicUsize = AtomicUsize::new(0);

/// Bytes requested, for context in the printed report. Not asserted —
/// byte totals move with capacity-doubling heuristics in a way counts do
/// not, so gating on them would be flaky.
static BYTES: AtomicUsize = AtomicUsize::new(0);

/// Forwards everything to the system allocator, counting on the way past.
struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
    }

    /// Counted separately: a `Vec` that outgrows its capacity reallocates,
    /// and those are exactly the copies the optimization work is hunting.
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(new_size.saturating_sub(layout.size()), Ordering::Relaxed);
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Ops measured. Large enough that per-op figures are stable, small
/// enough that the test stays under a second.
const OPS: usize = 2_000;

/// Ops run before the counters are read, so one-time setup (the log file's
/// buffers, the registry's map growth, lazily built statics) is not
/// charged to the measured ops.
const WARMUP: usize = 200;

/// Ceiling on allocator calls per admitted op.
///
/// Measured at 170 allocs / 17,989 bytes per op on this tree, release
/// profile, before any optimization work. Set to 180 so ordinary variation
/// inside dependencies does not fail the build, while a real regression
/// still does. Lower it whenever a change removes an allocation — that is
/// the entire point of the number.
const MAX_ALLOCS_PER_OP: usize = 180;

#[test]
fn submit_path_allocation_budget() {
    let dir = std::env::temp_dir().join(format!("choir-alloc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let log_path = dir.join("ops.jsonl");

    let key = ActorKey::generate();
    let mut registry = Registry::new();
    registry
        .register(&key.public_key_bytes())
        .expect("generated key is valid");
    let log = FileLog::open(&log_path).expect("open log");
    let platform =
        Platform::start(registry, Box::new(log), ActorKey::generate()).expect("platform starts");

    // Sign every body up front. Client-side signing and hex encoding are
    // the load generator's cost, not the submit path's, and counting them
    // would swamp the number being gated.
    let workspace = "ws0";
    let mut prev: Option<ContentHash> = None;
    let bodies: Vec<String> = (0..WARMUP + OPS)
        .map(|i| {
            let commit = ContentHash::blake3(format!("{workspace}-{i}").as_bytes());
            let op = ViewOp::new(OpKind::SetWorkspaceHead {
                workspace: workspace.to_string(),
                commit: commit.clone(),
                prev: prev.replace(commit),
            });
            let payload = op.to_payload();
            let sig = key.sign_submission(workspace, &payload);
            serde_json::json!({
                "workspace": workspace,
                "payload_hex": hex_encode(&payload),
                "key_id": sig.key_id,
                "signature_hex": hex_encode(&sig.signature),
            })
            .to_string()
        })
        .collect();

    for body in &bodies[..WARMUP] {
        let (status, _) = platform.handle_api("POST", "/api/submit", body.as_bytes());
        assert_eq!(status, 200, "warmup op rejected");
    }

    // `handle_api` blocks until the writer thread has replied, so by the
    // time the loop ends every allocation the writer made for these ops
    // has already happened. No draining needed.
    let before = ALLOCS.load(Ordering::Relaxed);
    let before_bytes = BYTES.load(Ordering::Relaxed);
    for body in &bodies[WARMUP..] {
        let (status, out) =
            platform.handle_api("POST", "/api/submit", std::hint::black_box(body.as_bytes()));
        assert_eq!(status, 200, "measured op rejected: {out}");
        std::hint::black_box(out);
    }
    let allocs = ALLOCS.load(Ordering::Relaxed) - before;
    let bytes = BYTES.load(Ordering::Relaxed) - before_bytes;

    let per_op = allocs / OPS;
    println!(
        "/api/submit allocation budget: {per_op} allocs/op, {} bytes/op ({OPS} ops)",
        bytes / OPS
    );

    std::fs::remove_dir_all(&dir).ok();
    assert!(
        per_op <= MAX_ALLOCS_PER_OP,
        "submit path allocated {per_op} times per op, ceiling is {MAX_ALLOCS_PER_OP}. \
         If this is a deliberate trade, move the ceiling in the same commit and say why."
    );
}
