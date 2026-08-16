//! Outbound ref-landed webhooks (D32), end to end: a real node, a real
//! `curl`, and a real socket answering on the other side.
//!
//! What these hold down is what would still look like a working feature
//! if it broke. A matcher that fires on every ref delivers *something*
//! to *someone*, which reads as success until the wrong repository's
//! branch shows up; so the "wrong ref" test asserts on which delivery
//! arrives first, in a fixed order, rather than on a count that a
//! too-eager matcher would also satisfy. An SSRF refusal that silently
//! delivers anyway is the same shape of failure, so that test asserts
//! both the refusal record and the silence at the receiver.
//!
//! The timing property — a slow receiver may not delay op admission —
//! is not here. It needs wall-clock assertions, which this shared
//! harness forbids, so it lives in `tests/hooks_isolation.rs`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use choir_identity::{ActorKey, Registry};
use choir_node::{Node, Platform};
use choir_oplog::MemLog;
use choir_view::{OpKind, ViewOp};

use crate::support::{curl, submit_body};

/// Long enough that a busy parallel test run cannot fail it, short
/// enough that a genuinely broken delivery path does not hang the suite.
/// A liveness bound, never an assertion about how fast delivery is.
const PATIENCE: Duration = Duration::from_secs(30);

struct Delivered {
    request_line: String,
    headers: Vec<String>,
    body: serde_json::Value,
}

impl Delivered {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            if !key.eq_ignore_ascii_case(name) {
                return None;
            }
            Some(value.trim())
        })
    }
}

/// A receiver on a fresh loopback port that answers every request with
/// `status` and reports what it was sent.
fn receiver(status: u16) -> (u16, Receiver<Delivered>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("receiver binds");
    let port = listener.local_addr().expect("receiver address").port();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let Ok(peer) = stream.try_clone() else {
                continue;
            };
            let mut reader = BufReader::new(peer);
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                continue;
            }
            let mut headers = Vec::new();
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        let line = line.trim_end().to_string();
                        if line.is_empty() {
                            break;
                        }
                        headers.push(line);
                    }
                    Err(_) => break,
                }
            }
            let length = headers
                .iter()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    if !key.eq_ignore_ascii_case("content-length") {
                        return None;
                    }
                    value.trim().parse::<usize>().ok()
                })
                .unwrap_or(0);
            let mut body = vec![0u8; length];
            let read = reader.read_exact(&mut body).is_ok();
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.flush();
            if read {
                let _ = tx.send(Delivered {
                    request_line: request_line.trim_end().to_string(),
                    headers,
                    body: serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
                });
            }
        }
    });
    (port, rx)
}

struct Fixture {
    node: std::sync::Arc<Node>,
    api: String,
    key: ActorKey,
    subscriptions: std::path::PathBuf,
    deliveries: std::path::PathBuf,
}

/// A node whose platform delivers webhooks from `lines`.
fn node_with_hooks(label: &str, lines: &str) -> Fixture {
    let work = std::env::temp_dir().join(format!(
        "choir-node-hooks-{label}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let subscriptions = work.join("hooks");
    let deliveries = work.join("hooks.jsonl");
    std::fs::write(&subscriptions, lines).unwrap();

    let key = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&key.public_key_bytes()).unwrap();
    let platform = Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate())
        .unwrap()
        .with_hooks(subscriptions.clone(), deliveries.clone())
        .unwrap();

    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
    node.enable_platform(platform);
    let api = format!("http://127.0.0.1:{}/api", node.port());
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    Fixture {
        node,
        api,
        key,
        subscriptions,
        deliveries,
    }
}

impl Fixture {
    fn land(&self, name: &str) {
        let op = ViewOp::new(OpKind::SetRef {
            name: name.into(),
            commit: choir_oplog::ContentHash::blake3(name.as_bytes()),
            prev: None,
        });
        let (code, response) = curl(&[
            "-X",
            "POST",
            "-d",
            &submit_body(&self.key, "alice", &op),
            &format!("{}/submit", self.api),
        ]);
        assert_eq!(code, 200, "{response}");
    }

    /// Waits for a delivery record matching `wanted`, returning it.
    ///
    /// The delivery log is written by the delivery thread after the
    /// request finishes, so it lags the receiver rather than leading it.
    fn wait_for_record(&self, wanted: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
        let deadline = std::time::Instant::now() + PATIENCE;
        loop {
            if let Ok(text) = std::fs::read_to_string(&self.deliveries) {
                for line in text.lines() {
                    let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
                        continue;
                    };
                    if wanted(&record) {
                        return record;
                    }
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no matching delivery record in {}",
                self.deliveries.display()
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

fn next(rx: &Receiver<Delivered>) -> Delivered {
    match rx.recv_timeout(PATIENCE) {
        Ok(delivered) => delivered,
        Err(RecvTimeoutError::Timeout) => panic!("no webhook arrived"),
        Err(RecvTimeoutError::Disconnected) => panic!("the receiver stopped"),
    }
}

#[test]
fn a_landed_ref_reaches_its_subscriber_with_the_subscription_secret() {
    let (port, rx) = receiver(200);
    let secret = "3f8c1d2e4b5a6978";
    let fixture = node_with_hooks(
        "delivered",
        &format!("owner/repo:refs/heads/* http://127.0.0.1:{port}/hook {secret} allow-private\n"),
    );

    fixture.land("owner/repo:refs/heads/main");
    let delivered = next(&rx);

    assert!(
        delivered.request_line.starts_with("POST /hook "),
        "{}",
        delivered.request_line
    );
    assert_eq!(delivered.header("X-Choir-Hook-Secret"), Some(secret));
    assert_eq!(delivered.header("Content-Type"), Some("application/json"));

    // What moved, in the words the receiver has to act on.
    let body = &delivered.body;
    assert_eq!(body["event"], "ref-landed", "{body}");
    assert_eq!(body["repo"], "owner/repo", "{body}");
    assert_eq!(body["ref"], "refs/heads/main", "{body}");
    assert_eq!(body["ref_key"], "owner/repo:refs/heads/main", "{body}");
    assert!(
        body["old"].is_null(),
        "a created ref has no old value: {body}"
    );
    assert_eq!(
        body["new"],
        choir_oplog::ContentHash::blake3(b"owner/repo:refs/heads/main").to_hex(),
        "{body}"
    );
    assert_eq!(body["seq"], 0, "{body}");
    assert_eq!(body["actor"], "alice", "{body}");
    assert_eq!(body["key_id"], fixture.key.actor_id().to_hex(), "{body}");
    // The entry hash is what lets a receiver discard a delivery it has
    // already acted on, so it has to be there and it has to be the
    // entry's, not a fresh hash of the payload.
    let entry = body["entry"].as_str().expect("an entry hash");
    assert!(!entry.is_empty(), "{body}");

    let record = fixture.wait_for_record(|record| record["event"] == "delivered");
    assert_eq!(record["status"], 200, "{record}");
    assert_eq!(record["attempt"], 1, "{record}");
    assert_eq!(record["entry"], entry, "{record}");

    fixture.node.unblock();
}

#[test]
fn a_ref_the_pattern_does_not_name_fires_nothing() {
    let (port, rx) = receiver(200);
    let fixture = node_with_hooks(
        "pattern",
        &format!("owner/repo:refs/heads/main http://127.0.0.1:{port}/hook s3cret allow-private\n"),
    );

    // Order is the assertion. Both ops go through one queue and one
    // delivery thread, so if the matcher fired on anything but the
    // subscribed ref, the unsubscribed one would arrive first.
    fixture.land("owner/repo:refs/heads/other");
    fixture.land("other/repo:refs/heads/main");
    fixture.land("owner/repo:refs/heads/main");

    let delivered = next(&rx);
    assert_eq!(
        delivered.body["ref_key"], "owner/repo:refs/heads/main",
        "only the subscribed ref may fire: {}",
        delivered.body
    );

    fixture.node.unblock();
}

#[test]
fn an_internal_target_is_refused_without_allow_private_and_the_refusal_is_recorded() {
    let (port, rx) = receiver(200);
    // Same loopback receiver as every other test here, minus the
    // keyword that admits it.
    let fixture = node_with_hooks(
        "ssrf",
        &format!("owner/repo:refs/heads/* http://127.0.0.1:{port}/hook s3cret\n"),
    );

    fixture.land("owner/repo:refs/heads/main");

    let record = fixture.wait_for_record(|record| record["event"] == "refused");
    let reason = record["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("allow-private"),
        "the refusal must name the way out: {record}"
    );
    assert!(
        rx.try_recv().is_err(),
        "a refused target must not be contacted"
    );

    fixture.node.unblock();
}

#[test]
fn an_edited_subscription_file_takes_effect_without_a_restart() {
    let (port, rx) = receiver(200);
    let fixture = node_with_hooks(
        "reload",
        &format!("owner/repo:refs/tags/* http://127.0.0.1:{port}/hook s3cret allow-private\n"),
    );

    // Not subscribed yet: this one exists to be the delivery that would
    // arrive first if the reload silently kept the old file *and* the
    // matcher were wrong. Nothing waits on it.
    fixture.land("owner/repo:refs/heads/first");

    // mtime has 1 s granularity on some filesystems; make the edit
    // unambiguous rather than racing the clock.
    std::thread::sleep(Duration::from_millis(1100));
    std::fs::write(
        &fixture.subscriptions,
        format!("owner/repo:refs/heads/* http://127.0.0.1:{port}/hook s3cret allow-private\n"),
    )
    .unwrap();

    fixture.land("owner/repo:refs/heads/second");
    let delivered = next(&rx);
    assert_eq!(
        delivered.body["ref_key"], "owner/repo:refs/heads/second",
        "the edited file must take effect on the next event: {}",
        delivered.body
    );

    fixture.node.unblock();
}

#[test]
fn a_deleted_ref_fires_too_and_says_what_it_was() {
    let (port, rx) = receiver(200);
    let fixture = node_with_hooks(
        "delete",
        &format!("owner/repo:refs/heads/* http://127.0.0.1:{port}/hook s3cret allow-private\n"),
    );

    let name = "owner/repo:refs/heads/doomed";
    let commit = choir_oplog::ContentHash::blake3(name.as_bytes());
    fixture.land(name);
    let created = next(&rx);
    assert_eq!(created.body["new"], commit.to_hex(), "{}", created.body);

    let (code, response) = curl(&[
        "-X",
        "POST",
        "-d",
        &submit_body(
            &fixture.key,
            "alice",
            &ViewOp::new(OpKind::DeleteRef {
                name: name.into(),
                prev: Some(commit.clone()),
            }),
        ),
        &format!("{}/submit", fixture.api),
    ]);
    assert_eq!(code, 200, "{response}");

    // A deletion is a ref that moved, so it fires; `new` is null and
    // `old` carries what was there, which is the pair a receiver needs
    // to tell a deletion from a creation.
    let deleted = next(&rx);
    assert_eq!(deleted.body["ref_key"], name, "{}", deleted.body);
    assert!(deleted.body["new"].is_null(), "{}", deleted.body);
    assert_eq!(deleted.body["old"], commit.to_hex(), "{}", deleted.body);

    fixture.node.unblock();
}

#[test]
fn a_failing_receiver_is_retried_and_every_attempt_is_recorded() {
    let (port, _rx) = receiver(500);
    let fixture = node_with_hooks(
        "retry",
        &format!("owner/repo:refs/heads/* http://127.0.0.1:{port}/hook s3cret allow-private\n"),
    );

    fixture.land("owner/repo:refs/heads/main");

    // Best-effort, but never silent: the last attempt says it was the
    // last one, so an operator reading this file sees an abandoned
    // delivery rather than an absence.
    let record =
        fixture.wait_for_record(|record| record["event"] == "failed" && record["final"] == true);
    assert_eq!(record["attempt"], 3, "{record}");
    assert_eq!(record["status"], 500, "{record}");

    fixture.node.unblock();
}
