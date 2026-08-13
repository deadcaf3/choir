//! Invariant 5 under a receiver that will not cooperate (D32).
//!
//! This is the property the whole webhook design exists to protect: a
//! webhook target belongs to somebody else, so its latency is unbounded,
//! and the sequencer's writer thread must never inherit it. Two ways
//! that could fail, one test each:
//!
//! 1. **Slow receiver.** Delivery on the writer's path would add the
//!    receiver's latency to every accepted op. The test admits a batch of
//!    ops against a receiver that answers slowly and asserts the batch
//!    still lands in a fraction of what serialized delivery would cost.
//! 2. **Receiver that never answers.** The queue then fills, and the
//!    only two options are to block the writer or to drop. It drops,
//!    counts, and says so in the delivery log.
//!
//! Its own binary rather than a module of `tests/it`: that harness runs
//! its modules on parallel threads in one process and forbids wall-clock
//! assertions, and both tests here are assertions about wall-clock time.
//! `throughput.rs` and `retention_cost.rs` are separate for the same
//! reason.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use choir_identity::{ActorKey, Registry};
use choir_node::hooks::QUEUE_CAPACITY;
use choir_node::platform::hex_encode;
use choir_node::Platform;
use choir_oplog::MemLog;
use choir_view::{OpKind, ViewOp};

/// What a slow receiver costs per delivery. Large enough that
/// serialized delivery of `OPS` events could not hide inside the
/// assertion below, small enough that the test is quick when the code
/// is right.
const SLOW_RESPONSE: Duration = Duration::from_millis(300);

/// Ops admitted while the slow receiver is answering.
const OPS: usize = 20;

/// The ceiling on admitting them. Serialized delivery would cost at
/// least `OPS * SLOW_RESPONSE` (6 s); in-process admission of 20 ops
/// against a `MemLog` is milliseconds, so the margin absorbs a loaded
/// machine without absorbing the bug.
const ADMISSION_CEILING: Duration = Duration::from_secs(2);

/// Accepts connections and answers each one after `delay`. With
/// `delay = None` it accepts and never answers at all, holding the
/// connection open until the process ends.
fn receiver(delay: Option<Duration>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("receiver binds");
    let port = listener.local_addr().expect("receiver address").port();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let Ok(peer) = stream.try_clone() else { continue };
            match delay {
                Some(delay) => {
                    std::thread::spawn(move || {
                        let mut reader = BufReader::new(peer);
                        let mut line = String::new();
                        while reader.read_line(&mut line).unwrap_or(0) > 0 {
                            if line.trim_end().is_empty() {
                                break;
                            }
                            line.clear();
                        }
                        std::thread::sleep(delay);
                        let _ = write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        );
                        let _ = stream.flush();
                    });
                }
                // Never answer, never close: this is the receiver the
                // writer thread must not be waiting on.
                None => held.push(stream),
            }
        }
    });
    port
}

struct Fixture {
    platform: Platform,
    key: ActorKey,
    deliveries: std::path::PathBuf,
}

fn fixture(label: &str, port: u16) -> Fixture {
    let work = std::env::temp_dir().join(format!(
        "choir-node-hooks-isolation-{label}-{}",
        std::process::id()
    ));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let subscriptions = work.join("hooks");
    let deliveries = work.join("hooks.jsonl");
    std::fs::write(
        &subscriptions,
        format!("owner/repo:refs/heads/* http://127.0.0.1:{port}/hook s3cret allow-private\n"),
    )
    .unwrap();

    let key = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&key.public_key_bytes()).unwrap();
    let platform = Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate())
        .unwrap()
        .with_hooks(subscriptions, deliveries.clone())
        .unwrap();
    Fixture {
        platform,
        key,
        deliveries,
    }
}

impl Fixture {
    /// Admits one op through the same in-process path the HTTP handler
    /// uses. No socket, so what is measured is admission itself.
    fn land(&self, name: &str) {
        let op = ViewOp::new(OpKind::SetRef {
            name: name.into(),
            commit: choir_oplog::ContentHash::blake3(name.as_bytes()),
            prev: None,
        });
        let payload = op.to_payload();
        let sig = self.key.sign_submission("alice", &payload);
        let body = serde_json::json!({
            "channel": "alice",
            "payload_hex": hex_encode(&payload),
            "key_id": sig.key_id,
            "signature_hex": hex_encode(&sig.signature),
        })
        .to_string();
        let (code, response) = self
            .platform
            .handle_api("POST", "/api/submit", body.as_bytes());
        assert_eq!(code, 200, "{response}");
    }
}

#[test]
fn a_slow_receiver_does_not_delay_op_admission() {
    let port = receiver(Some(SLOW_RESPONSE));
    let fixture = fixture("slow", port);

    let start = Instant::now();
    for i in 0..OPS {
        fixture.land(&format!("owner/repo:refs/heads/b{i}"));
    }
    let elapsed = start.elapsed();

    assert!(
        elapsed < ADMISSION_CEILING,
        "admitting {OPS} ops took {elapsed:?}; serialized delivery to a {SLOW_RESPONSE:?} \
         receiver would cost at least {:?}, so the writer thread is waiting on the receiver",
        SLOW_RESPONSE * u32::try_from(OPS).unwrap()
    );
}

#[test]
fn a_full_queue_drops_and_counts_rather_than_blocking_the_writer() {
    let port = receiver(None);
    let fixture = fixture("full", port);

    // One event is in the delivery thread's hands and stuck there; the
    // queue takes the next `QUEUE_CAPACITY`. Everything after that has
    // nowhere to go, and the only alternative to dropping it would be to
    // make the sequencer wait on a receiver that never answers.
    let ops = QUEUE_CAPACITY + 64;
    let start = Instant::now();
    for i in 0..ops {
        fixture.land(&format!("owner/repo:refs/heads/b{i}"));
    }
    let elapsed = start.elapsed();

    assert!(
        elapsed < ADMISSION_CEILING,
        "admitting {ops} ops took {elapsed:?} against a receiver that never answers"
    );
    let dropped = fixture.platform.hook_drops();
    assert!(
        dropped > 0,
        "the queue must overflow into a counted drop, not into the writer thread"
    );

    // And the drop is visible to the operator, not only to this test.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let text = std::fs::read_to_string(&fixture.deliveries).unwrap_or_default();
        if let Some(record) = text
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|record| record["event"] == "dropped")
        {
            assert!(record["dropped_total"].as_u64().unwrap_or(0) > 0, "{record}");
            assert_eq!(record["queue_capacity"], QUEUE_CAPACITY, "{record}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "a dropped event must reach {}",
            fixture.deliveries.display()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}
