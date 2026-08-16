//! Log-backend seam conformance suite (DECISIONS.md D16).
//! Every OpLog implementation must pass every case here.

use choir_oplog::{FileLog, MemLog, OpEntry, OpLog, FORMAT_VERSION};

fn entry(parent: Option<choir_oplog::ContentHash>, seq: u64, ws: &str) -> OpEntry {
    OpEntry {
        format_version: FORMAT_VERSION,
        parent,
        seq,
        channel: ws.to_string(),
        payload: format!("op-{seq}").into_bytes(),
        witnesses: Vec::new(),
        author_sig: None,
    }
}

fn conformance(log: &mut dyn OpLog) {
    assert!(log.is_empty());
    assert!(log.head().is_none());

    // Genesis append.
    let h0 = log.append(entry(None, 0, "a")).expect("genesis append");
    assert_eq!(log.head(), Some(h0.clone()));
    assert_eq!(log.len(), 1);

    // Chained append.
    let h1 = log
        .append(entry(Some(h0.clone()), 1, "b"))
        .expect("chained append");
    assert_eq!(log.head(), Some(h1));

    // Stale-parent append must be rejected (single-writer invariant).
    assert!(log.append(entry(Some(h0), 2, "c")).is_err());
    assert_eq!(log.len(), 2, "rejected append must not mutate the log");

    // Reads.
    let e0 = log.get(0).expect("get(0)");
    assert_eq!(e0.channel, "a");
    assert!(log.get(99).is_none());

    // Witness fields exist and are empty pre-Phase-2.
    assert!(e0.witnesses.is_empty());
    assert_eq!(e0.format_version, FORMAT_VERSION);
}

#[test]
fn memlog_conforms() {
    conformance(&mut MemLog::new());
}

#[test]
fn filelog_conforms() {
    let dir = std::env::temp_dir().join(format!("choir-oplog-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("log.jsonl");
    conformance(&mut FileLog::open(&path).unwrap());

    // FileLog extra: reopen rebuilds head and length identically.
    let reopened = FileLog::open(&path).unwrap();
    assert_eq!(reopened.len(), 2);
    assert!(reopened.head().is_some());
    std::fs::remove_dir_all(&dir).ok();
}
