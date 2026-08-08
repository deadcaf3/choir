//! `FileLog` durability: buffering is only safe because `sync` empties the
//! buffer, and these are the tests that hold that pairing together.
//!
//! Appends are buffered so a batch costs one write syscall rather than
//! one each. That is a correctness hazard as much as a speed win: until
//! the buffer is flushed the entries are invisible to any other reader of
//! the file, including the `/api/log` resync path, which reads the
//! persisted log directly rather than through this type.

use choir_oplog::{FileLog, MemLog, OpEntry, OpLog, FORMAT_VERSION};

/// Scratch file that removes itself, so one failing test cannot leave a
/// log behind for the next to replay.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "choir-durability-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        Self(dir)
    }

    fn path(&self) -> std::path::PathBuf {
        self.0.join("ops.jsonl")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

fn entry(seq: u64, parent: Option<choir_oplog::ContentHash>) -> OpEntry {
    OpEntry {
        format_version: FORMAT_VERSION,
        parent,
        seq,
        workspace: "agent-1".into(),
        payload: format!("op{seq}").into_bytes(),
        witnesses: Vec::new(),
        author_sig: None,
    }
}

/// The pairing. Appended entries are not required to be visible on disk
/// before `sync`, but after it they must be — otherwise the buffering
/// added for speed silently hides committed history from every other
/// reader of the file.
#[test]
fn sync_makes_appended_entries_visible_to_an_independent_reader() {
    let scratch = Scratch::new("visible");
    let mut log = FileLog::open(&scratch.path()).expect("open");

    let mut head = None;
    for seq in 0..3 {
        head = Some(log.append(entry(seq, head)).expect("append"));
    }
    log.sync().expect("sync");

    // Read the bytes without going through FileLog at all: this is what
    // the resync path does, and what a crash recovery would see.
    let text = std::fs::read_to_string(scratch.path()).expect("read log file");
    assert_eq!(
        text.lines().count(),
        3,
        "after sync every appended entry must be on disk, found: {text:?}"
    );

    // And it must still be a replayable chain, not merely three lines.
    let reopened = FileLog::open(&scratch.path()).expect("reopen");
    assert_eq!(reopened.len(), 3);
    assert_eq!(reopened.head(), head, "head survives a reopen");
}

/// Durability is a property of the file, so a fresh handle must see
/// everything a synced handle wrote — the crash-recovery case, minus the
/// crash.
///
/// Note this test alone would NOT catch a missing flush: the log is
/// dropped at the end of the block and `Drop` flushes as a safety net, so
/// the data lands either way. Verified by deleting the flush and watching
/// this pass while `sync_makes_appended_entries_visible_to_an_independent_reader`
/// failed with an empty file. The live-reader test is the load-bearing
/// one; this covers reopen semantics.
#[test]
fn a_synced_log_reopens_with_its_full_chain() {
    let scratch = Scratch::new("reopen");
    let expected_head = {
        let mut log = FileLog::open(&scratch.path()).expect("open");
        let mut head = None;
        for seq in 0..10 {
            head = Some(log.append(entry(seq, head)).expect("append"));
        }
        log.sync().expect("sync");
        head
    };

    let reopened = FileLog::open(&scratch.path()).expect("reopen");
    assert_eq!(reopened.len(), 10);
    assert_eq!(reopened.head(), expected_head);
    for seq in 0..10 {
        let e = reopened.get(seq).expect("entry present");
        assert_eq!(e.seq, seq);
        assert_eq!(e.payload, format!("op{seq}").into_bytes());
    }
}

/// Repeated syncs with nothing in between are legal and cheap. The
/// sequencer calls `sync` once per batch including batches that admitted
/// nothing, so this is a real path, not a hypothetical.
#[test]
fn syncing_an_unchanged_log_is_a_no_op() {
    let scratch = Scratch::new("idempotent");
    let mut log = FileLog::open(&scratch.path()).expect("open");
    log.append(entry(0, None)).expect("append");
    log.sync().expect("first sync");
    log.sync().expect("second sync");
    log.sync().expect("third sync");
    assert_eq!(
        std::fs::read_to_string(scratch.path())
            .expect("read")
            .lines()
            .count(),
        1
    );
}

/// The seam's default. An in-memory log cannot be made durable and says so
/// by succeeding trivially, so a policy that syncs per batch does not have
/// to know which backend it is talking to.
#[test]
fn a_memory_log_syncs_trivially() {
    let mut log = MemLog::new();
    log.append(entry(0, None)).expect("append");
    log.sync().expect("MemLog sync is infallible");
    assert_eq!(log.len(), 1);
}

/// `get` must be seamless across the flushed/pending boundary.
///
/// Flushed entries are read back from the file at a recorded offset;
/// entries appended since the last flush are not in the file yet and come
/// from an in-memory buffer. A caller cannot tell which, and replay walks
/// straight across the seam, so this covers entries on both sides of it
/// and the seam itself.
#[test]
fn get_reads_across_the_flushed_and_pending_boundary() {
    let scratch = Scratch::new("boundary");
    let mut log = FileLog::open(&scratch.path()).expect("open");

    let mut head = None;
    for seq in 0..5 {
        head = Some(log.append(entry(seq, head)).expect("append"));
    }
    log.sync().expect("sync");
    // These stay in the pending buffer: appended, not yet flushed.
    for seq in 5..9 {
        head = Some(log.append(entry(seq, head)).expect("append"));
    }

    assert_eq!(log.len(), 9);
    for seq in 0..9 {
        let e = log.get(seq).expect("every entry readable regardless of side");
        assert_eq!(e.seq, seq, "entry {seq} came back as {}", e.seq);
        assert_eq!(e.payload, format!("op{seq}").into_bytes());
    }
    assert_eq!(log.get(9), None, "past the end is None");

    // After the second flush everything comes from the file, and the
    // answers must not change.
    log.sync().expect("second sync");
    for seq in 0..9 {
        let e = log.get(seq).expect("still readable once flushed");
        assert_eq!(e.seq, seq);
        assert_eq!(e.payload, format!("op{seq}").into_bytes());
    }
}

/// Entry sizes vary, so offsets cannot be assumed uniform. Payloads of
/// wildly different lengths catch an index that strides rather than
/// records.
#[test]
fn get_handles_entries_of_differing_lengths() {
    let scratch = Scratch::new("sizes");
    let mut log = FileLog::open(&scratch.path()).expect("open");

    let sizes = [1usize, 500, 3, 20_000, 7];
    let mut head = None;
    for (seq, size) in sizes.iter().enumerate() {
        let mut e = entry(seq as u64, head);
        e.payload = vec![b'x'; *size];
        head = Some(log.append(e).expect("append"));
    }
    log.sync().expect("sync");

    for (seq, size) in sizes.iter().enumerate() {
        let e = log.get(seq as u64).expect("entry present");
        assert_eq!(e.payload.len(), *size, "entry {seq} came back the wrong size");
        assert!(e.payload.iter().all(|b| *b == b'x'));
    }

    // And the same after a reopen, which rebuilds the index by scanning.
    let reopened = FileLog::open(&scratch.path()).expect("reopen");
    for (seq, size) in sizes.iter().enumerate() {
        assert_eq!(
            reopened.get(seq as u64).expect("entry present").payload.len(),
            *size,
            "entry {seq} wrong size after reopening"
        );
    }
    assert_eq!(reopened.head(), head, "head survives the rebuild");
}
