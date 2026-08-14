//! Phase-0 gate evidence (DECISIONS.md):
//! ">10 concurrent workspaces/repo with <100 ms merge-decision latency".
//! 32 workspaces x 100 ops each; asserts total order, zero loss, per-client
//! FIFO, and prints decision-latency percentiles.

use choir_oplog::MemLog;
use choir_sequencer::Sequencer;
use std::collections::HashMap;
use std::time::Duration;

const WORKSPACES: usize = 32;
const OPS_PER_WORKSPACE: usize = 100;

#[test]
fn total_order_under_concurrency() {
    let sequencer = Sequencer::spawn(Box::new(MemLog::new()));

    let mut threads = Vec::new();
    for w in 0..WORKSPACES {
        let handle = sequencer.handle();
        threads.push(std::thread::spawn(move || {
            let ws = format!("ws-{w}");
            let mut results = Vec::with_capacity(OPS_PER_WORKSPACE);
            for i in 0..OPS_PER_WORKSPACE {
                let accepted = handle.submit(&ws, format!("{ws}:{i}").into_bytes());
                results.push((accepted.seq, accepted.decision_latency));
            }
            (ws, results)
        }));
    }

    let mut latencies: Vec<Duration> = Vec::new();
    let mut per_client: HashMap<String, Vec<u64>> = HashMap::new();
    for t in threads {
        let (ws, results) = t.join().expect("workspace thread");
        let seqs: Vec<u64> = results.iter().map(|(s, _)| *s).collect();
        latencies.extend(results.iter().map(|(_, l)| *l));
        per_client.insert(ws, seqs);
    }

    let log = sequencer.shutdown();

    // Zero loss: every submitted op is in the log exactly once.
    let expected = (WORKSPACES * OPS_PER_WORKSPACE) as u64;
    assert_eq!(log.len(), expected, "no lost or duplicated ops");

    // Total order: seq is dense 0..N and each entry's seq matches its position.
    for seq in 0..expected {
        let e = log.get(seq).expect("dense sequence");
        assert_eq!(e.seq, seq);
    }

    // Hash chain: each entry's parent is the previous entry's hash.
    for seq in 1..expected {
        let prev = log.get(seq - 1).unwrap().content_hash();
        let cur = log.get(seq).unwrap();
        assert_eq!(cur.parent, Some(prev), "hash chain intact at seq {seq}");
    }

    // Per-client FIFO: each workspace's ops appear in submission order.
    for (ws, seqs) in &per_client {
        assert!(
            seqs.windows(2).all(|w| w[0] < w[1]),
            "workspace {ws} ops out of order"
        );
    }

    // Gate metric: decision latency percentiles.
    latencies.sort();
    let p = |q: f64| latencies[((latencies.len() - 1) as f64 * q) as usize];
    println!(
        "decision latency over {} ops from {} workspaces: p50={:?} p99={:?} max={:?}",
        latencies.len(),
        WORKSPACES,
        p(0.50),
        p(0.99),
        latencies[latencies.len() - 1]
    );
    assert!(
        p(0.99) < Duration::from_millis(100),
        "Phase-0 gate: p99 decision latency must be <100 ms, got {:?}",
        p(0.99)
    );
}
