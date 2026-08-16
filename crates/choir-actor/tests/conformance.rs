//! D3 seam conformance for the Rivet-backed sequencer: the same
//! properties `choir-sequencer/tests/concurrency.rs` proves for the
//! in-process implementation — total order, intact hash chain,
//! per-client FIFO — under concurrent submitters.
//!
//! Ignored by default because it spawns a local Rivet engine (first run
//! downloads a sha256-verified binary). Run with:
//! `RIVETKIT_ENGINE_AUTO_DOWNLOAD=1 cargo test -p choir-actor -- --ignored`

use choir_actor::{DumpLog, SequencerActor, SubmitOp};
use rivetkit::{test, Registry};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns a local Rivet engine; see module docs"]
async fn total_order_hash_chain_fifo() -> anyhow::Result<()> {
    const CLIENTS: usize = 8;
    const OPS: usize = 10;

    let mut registry = Registry::new();
    choir_actor::register(&mut registry);
    let h = test::setup(registry).await?;

    // First touch creates the actor; do it once before the concurrent
    // phase so client tasks don't race get-or-create.
    let warmup = h.actor_with_key::<SequencerActor>("choirSequencer", vec!["repo-1".to_string()]);
    let reply = warmup
        .send(SubmitOp {
            channel: "warmup".into(),
            payload: b"genesis".to_vec(),
            author_sig: None,
        })
        .await?;
    assert_eq!(reply.seq, 0);

    let mut tasks = Vec::new();
    for c in 0..CLIENTS {
        let actor =
            h.actor_with_key::<SequencerActor>("choirSequencer", vec!["repo-1".to_string()]);
        tasks.push(tokio::spawn(async move {
            let workspace = format!("agent-{c}");
            let mut seqs = Vec::with_capacity(OPS);
            for i in 0..OPS {
                let reply = actor
                    .send(SubmitOp {
                        channel: workspace.clone(),
                        payload: format!("{workspace}:op-{i}").into_bytes(),
                        author_sig: None,
                    })
                    .await
                    .expect("submit succeeds");
                seqs.push(reply.seq);
            }
            (workspace, seqs)
        }));
    }
    let mut per_client = Vec::new();
    for t in tasks {
        per_client.push(t.await?);
    }

    let actor = h.actor_with_key::<SequencerActor>("choirSequencer", vec!["repo-1".to_string()]);
    let entries = actor.send(DumpLog).await?;
    h.shutdown().await;

    // Total order: every op landed exactly once (+1 warmup), seq = position.
    assert_eq!(entries.len(), CLIENTS * OPS + 1);
    for (i, e) in entries.iter().enumerate() {
        assert_eq!(e.seq, i as u64, "dense sequence numbers");
    }

    // Hash chain: each entry's parent is the previous entry's hash.
    assert!(entries[0].parent.is_none());
    for w in entries.windows(2) {
        assert_eq!(
            w[1].parent.as_ref(),
            Some(&w[0].content_hash()),
            "chain intact at seq {}",
            w[1].seq
        );
    }

    // Per-client FIFO: each client's ops appear in submission order,
    // and the seqs the client saw match the log.
    for (workspace, seqs) in &per_client {
        let mine: Vec<&choir_oplog::OpEntry> =
            entries.iter().filter(|e| &e.channel == workspace).collect();
        assert_eq!(mine.len(), OPS);
        for (i, e) in mine.iter().enumerate() {
            assert_eq!(
                e.payload,
                format!("{workspace}:op-{i}").into_bytes(),
                "FIFO for {workspace}"
            );
        }
        let logged: Vec<u64> = mine.iter().map(|e| e.seq).collect();
        assert_eq!(&logged, seqs, "client-observed seqs match the log");
    }
    Ok(())
}
