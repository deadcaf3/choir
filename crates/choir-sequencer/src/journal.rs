//! A structured, append-only decision journal: what the writer decided,
//! why, and how long it took, one JSON object per line.
//!
//! # Derived data, and the consequences of saying so
//!
//! The journal is **not** the op log and is never authoritative. Nothing
//! reads it back to reconstruct state, no hash covers it, no signature
//! commits to it. Two rules follow, and both are load-bearing:
//!
//! - **Recording never blocks and never fails a submission.**
//!   [`Journal::record`] takes `&self`, cannot return an error, and
//!   [`FileJournal`] drops a record rather than stall the writer. A full
//!   disk must slow nothing and refuse nothing.
//! - **Its corruption cannot reach the log.** A truncated final line is a
//!   truncated final line. Since no one replays it, there is nothing to
//!   repair and no state to lose.
//!
//! # Why the I/O is on another thread
//!
//! [`crate::Sequencer`]'s writer thread is the single appender, and
//! `choir-sequencer/tests/concurrency.rs` asserts p99 decision latency
//! under 100 ms. A synchronous write per decision would put a disk on
//! that path and make the assertion measure the filesystem. So the
//! writer only pushes onto a channel; [`FileJournal`] drains it from its
//! own thread and does every write and flush there.
//!
//! The trade is explicit: a crash loses whatever is still in the channel.
//! For derived data that is the right side to be wrong on.
//!
//! # Examples
//!
//! ```
//! use choir_sequencer::journal::{Event, Journal, MemJournal};
//!
//! let journal = MemJournal::new();
//! journal.record(Event::WindowResize {
//!     from: 20,
//!     to: 10,
//!     cause: "combined build failed".to_string(),
//! });
//! let lines = journal.lines();
//! assert_eq!(lines.len(), 1);
//! assert!(lines[0].contains("\"window_resize\""));
//! assert!(lines[0].contains("\"cause\":\"combined build failed\""));
//! ```

use std::io::Write;
use std::sync::mpsc;
use std::sync::Mutex;

/// Journal record format. Bumped only for an incompatible change; new
/// fields are additive, exactly as the op log's own rule (invariant 1).
///
/// Unlike a persisted op this version guards no hash, because nothing
/// hashes the journal. It is here so a reader can tell which fields to
/// expect rather than guess from their presence.
pub const FORMAT_VERSION: u32 = 1;

/// One journalled event.
///
/// Every variant serializes to a flat JSON object carrying `kind` and
/// `format_version`, so `jq 'select(.kind == "decision")'` works without
/// a schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The writer accepted or refused one submission.
    Decision {
        /// Verified author, when the policy identified one. `None` for a
        /// submission refused before its signature was resolved, which
        /// is itself the interesting case.
        actor_id: Option<String>,
        /// The channel the op was signed on.
        workspace: String,
        /// The op variant, when the policy decoded one.
        op_type: Option<String>,
        /// `true` if the op was ordered and appended.
        accepted: bool,
        /// The refusal, verbatim, when there was one.
        reject_reason: Option<String>,
        /// Position assigned, on acceptance only.
        seq: Option<u64>,
        /// The entry this one chained onto, on acceptance only.
        parent: Option<String>,
        /// Time from dequeue to decision, in microseconds.
        decision_latency_us: u64,
    },
    /// A sample of how many commands were waiting behind the writer.
    QueueDepth {
        /// Commands drained in one wake-up, the writer's own view of
        /// backlog. Sampled rather than continuous: counting costs
        /// nothing but recording every value would dominate the file.
        depth: usize,
    },
    /// The speculative window changed size.
    WindowResize {
        /// Size before.
        from: usize,
        /// Size after.
        to: usize,
        /// Why it moved, so a shrinking window is attributable rather
        /// than merely visible.
        cause: String,
    },
    /// A compare-and-swap precondition did not hold.
    ///
    /// Recorded distinctly from the refusal it also produces, because
    /// "two writers raced this ref" is a fact about contention, and a
    /// rejection count alone cannot separate it from a client sending
    /// nonsense.
    CasFailure {
        /// The channel the op was signed on.
        workspace: String,
        /// What the submitter believed the current value was.
        expected: Option<String>,
        /// What it actually was at decision time.
        actual: Option<String>,
    },
}

impl Event {
    /// Renders one JSONL line, without its newline.
    #[must_use]
    pub fn to_line(&self) -> String {
        let mut o = serde_json::Map::new();
        o.insert("format_version".into(), serde_json::json!(FORMAT_VERSION));
        match self {
            Event::Decision {
                actor_id,
                workspace,
                op_type,
                accepted,
                reject_reason,
                seq,
                parent,
                decision_latency_us,
            } => {
                o.insert("kind".into(), serde_json::json!("decision"));
                o.insert("actor_id".into(), serde_json::json!(actor_id));
                o.insert("workspace".into(), serde_json::json!(workspace));
                o.insert("op_type".into(), serde_json::json!(op_type));
                // A string rather than a bool: `decision` reads the same
                // in a filter as it does in a report, and a third
                // outcome later is a new value instead of a new field.
                o.insert(
                    "decision".into(),
                    serde_json::json!(if *accepted { "accepted" } else { "rejected" }),
                );
                o.insert("reject_reason".into(), serde_json::json!(reject_reason));
                o.insert("seq".into(), serde_json::json!(seq));
                o.insert("parent".into(), serde_json::json!(parent));
                o.insert(
                    "decision_latency_us".into(),
                    serde_json::json!(decision_latency_us),
                );
            }
            Event::QueueDepth { depth } => {
                o.insert("kind".into(), serde_json::json!("queue_depth"));
                o.insert("depth".into(), serde_json::json!(depth));
            }
            Event::WindowResize { from, to, cause } => {
                o.insert("kind".into(), serde_json::json!("window_resize"));
                o.insert("from".into(), serde_json::json!(from));
                o.insert("to".into(), serde_json::json!(to));
                o.insert("cause".into(), serde_json::json!(cause));
            }
            Event::CasFailure {
                workspace,
                expected,
                actual,
            } => {
                o.insert("kind".into(), serde_json::json!("cas_failure"));
                o.insert("workspace".into(), serde_json::json!(workspace));
                o.insert("expected".into(), serde_json::json!(expected));
                o.insert("actual".into(), serde_json::json!(actual));
            }
        }
        serde_json::Value::Object(o).to_string()
    }
}

/// Somewhere decision events go.
///
/// `&self` rather than `&mut self` on purpose: the writer thread holds
/// this while it owns the log, and a journal that needed exclusive
/// access would either serialize behind the writer or force a lock onto
/// the decision path.
pub trait Journal: Send + Sync {
    /// Records one event. Must not block and must not panic.
    fn record(&self, event: Event);

    /// Whether building an [`Event`] for this journal is worth it.
    ///
    /// An [`Event`] owns its strings, so constructing one costs several
    /// allocations *before* `record` can discard it. On the writer
    /// thread, per op, that is not free: wiring the journal in without
    /// this guard pushed the submit path from 174 to 212 allocations per
    /// op and `submit_path_allocation_budget` failed, which is the test
    /// working. Call sites check this first, so a node with no journal
    /// pays nothing at all rather than paying to be ignored.
    fn enabled(&self) -> bool {
        true
    }
}

/// A journal that discards everything.
///
/// The default, so a node that configures no journal pays nothing and
/// every call site can be unconditional.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullJournal;

impl Journal for NullJournal {
    fn record(&self, _event: Event) {}

    /// Nothing is recorded, so nothing should be built.
    fn enabled(&self) -> bool {
        false
    }
}

/// An in-memory journal, for tests and for the doctest above.
#[derive(Debug, Default)]
pub struct MemJournal {
    lines: Mutex<Vec<String>>,
}

impl MemJournal {
    /// An empty journal.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every line recorded so far, in order.
    ///
    /// # Panics
    ///
    /// If a previous holder of the lock panicked.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        self.lines.lock().expect("journal lock").clone()
    }
}

impl Journal for MemJournal {
    fn record(&self, event: Event) {
        if let Ok(mut lines) = self.lines.lock() {
            lines.push(event.to_line());
        }
    }
}

/// A JSONL file written from its own thread.
///
/// [`Journal::record`] only sends on a channel; the thread owns the file
/// and does every write and flush. See the module docs for why the I/O
/// is not on the writer thread.
#[derive(Debug)]
pub struct FileJournal {
    tx: Option<mpsc::Sender<Event>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FileJournal {
    /// Opens `path` for append and starts the writing thread.
    ///
    /// # Errors
    ///
    /// If the file cannot be opened. Once open, later write failures are
    /// swallowed rather than reported: the caller has no useful response
    /// to "the journal could not be written", and the one unacceptable
    /// response is refusing an op over it.
    pub fn create(path: &std::path::Path) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let (tx, rx) = mpsc::channel::<Event>();
        let thread = std::thread::spawn(move || {
            let mut out = std::io::BufWriter::new(file);
            // Drain until every sender is gone. Flushing per wake-up
            // rather than per record keeps a burst to one syscall while
            // still leaving the file complete whenever the writer idles,
            // which is the state anyone reading it is in.
            while let Ok(first) = rx.recv() {
                let _ = writeln!(out, "{}", first.to_line());
                for event in rx.try_iter() {
                    let _ = writeln!(out, "{}", event.to_line());
                }
                let _ = out.flush();
            }
            let _ = out.flush();
        });
        Ok(Self {
            tx: Some(tx),
            thread: Some(thread),
        })
    }
}

impl Journal for FileJournal {
    fn record(&self, event: Event) {
        // A closed or failed channel drops the record. The alternative
        // -- surfacing it -- would put the journal's health on the
        // decision path, which is the coupling this module exists to
        // avoid.
        if let Some(tx) = &self.tx {
            let _ = tx.send(event);
        }
    }
}

impl Drop for FileJournal {
    fn drop(&mut self) {
        // Close the channel first so the thread's `recv` returns, then
        // join it: without the join, a process exiting immediately after
        // a decision loses the flush that would have recorded it.
        self.tx = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Event, FileJournal, Journal, MemJournal, NullJournal};

    fn decision(accepted: bool) -> Event {
        Event::Decision {
            actor_id: Some("actor-1".into()),
            workspace: "ws".into(),
            op_type: Some("SetRef".into()),
            accepted,
            reject_reason: if accepted {
                None
            } else {
                Some("stale_head".into())
            },
            seq: accepted.then_some(7),
            parent: accepted.then(|| "1e-abc".to_string()),
            decision_latency_us: 42,
        }
    }

    #[test]
    fn a_decision_line_is_one_flat_json_object() {
        let line = decision(true).to_line();
        let v: serde_json::Value = serde_json::from_str(&line).expect("valid JSON");
        assert_eq!(v["kind"], "decision");
        assert_eq!(v["decision"], "accepted");
        assert_eq!(v["seq"], 7);
        assert_eq!(v["decision_latency_us"], 42);
        assert!(!line.contains('\n'), "a JSONL record is one line");
    }

    /// A refusal carries the reason and no position, because there is no
    /// position: nothing was ordered.
    #[test]
    fn a_rejection_records_the_reason_and_no_seq() {
        let v: serde_json::Value =
            serde_json::from_str(&decision(false).to_line()).expect("valid JSON");
        assert_eq!(v["decision"], "rejected");
        assert_eq!(v["reject_reason"], "stale_head");
        assert!(v["seq"].is_null());
        assert!(v["parent"].is_null());
    }

    /// Reject reasons are free text and will contain quotes and
    /// newlines. Anything that hand-rolled the escaping would corrupt
    /// the file exactly when something interesting happened.
    #[test]
    fn a_reason_containing_quotes_and_newlines_stays_one_valid_line() {
        let event = Event::Decision {
            actor_id: None,
            workspace: "ws".into(),
            op_type: None,
            accepted: false,
            reject_reason: Some("bad \"sig\"\nsecond line\ttab".into()),
            seq: None,
            parent: None,
            decision_latency_us: 1,
        };
        let line = event.to_line();
        assert!(
            !line.contains('\n'),
            "an embedded newline escaped the record"
        );
        let v: serde_json::Value = serde_json::from_str(&line).expect("valid JSON");
        assert_eq!(v["reject_reason"], "bad \"sig\"\nsecond line\ttab");
    }

    #[test]
    fn every_variant_names_its_kind() {
        for (event, kind) in [
            (Event::QueueDepth { depth: 3 }, "queue_depth"),
            (
                Event::WindowResize {
                    from: 20,
                    to: 10,
                    cause: "ci failure".into(),
                },
                "window_resize",
            ),
            (
                Event::CasFailure {
                    workspace: "ws".into(),
                    expected: Some("aaa".into()),
                    actual: Some("bbb".into()),
                },
                "cas_failure",
            ),
        ] {
            let v: serde_json::Value = serde_json::from_str(&event.to_line()).expect("valid JSON");
            assert_eq!(v["kind"], kind);
            assert_eq!(v["format_version"], 1);
        }
    }

    #[test]
    fn the_null_journal_accepts_and_discards() {
        NullJournal.record(decision(true));
    }

    /// The file must be complete once the journal is dropped, which is
    /// the guarantee the off-thread write would otherwise cost.
    #[test]
    fn a_dropped_file_journal_has_flushed_everything() {
        let dir = std::env::temp_dir().join(format!("choir-journal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("ops.jsonl");
        {
            let journal = FileJournal::create(&path).expect("create");
            for _ in 0..50 {
                journal.record(decision(true));
            }
        }
        let body = std::fs::read_to_string(&path).expect("read back");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 50, "records were lost across drop");
        for line in lines {
            serde_json::from_str::<serde_json::Value>(line).expect("every line is valid JSON");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_mem_journal_keeps_order() {
        let journal = MemJournal::new();
        journal.record(Event::QueueDepth { depth: 1 });
        journal.record(Event::QueueDepth { depth: 2 });
        let lines = journal.lines();
        assert!(lines[0].contains("\"depth\":1"));
        assert!(lines[1].contains("\"depth\":2"));
    }
}
