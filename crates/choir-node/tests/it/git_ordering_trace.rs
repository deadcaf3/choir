//! Independent trace check for the successful Git publication path.
//!
//! The assertions deliberately do not use `View`, `/api/view`, or the
//! production hash-chain verifier. They observe three distinct boundaries:
//! raw durable log bytes, the bare Git ref, and the pushing client's reply.

#![cfg(unix)]

use std::sync::mpsc;
use std::time::{Duration, Instant};

use choir_identity::{ActorKey, Registry};
use choir_node::{Node, Platform};
use choir_oplog::{ContentHash, FileLog, LogError, OpEntry, OpLog};

struct BlockingSyncLog {
    inner: FileLog,
    synced: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}

impl OpLog for BlockingSyncLog {
    fn append(&mut self, entry: OpEntry) -> Result<ContentHash, LogError> {
        self.inner.append(entry)
    }

    fn head(&self) -> Option<ContentHash> {
        self.inner.head()
    }

    fn len(&self) -> u64 {
        self.inner.len()
    }

    fn get(&self, seq: u64) -> Option<OpEntry> {
        self.inner.get(seq)
    }

    fn sync(&mut self) -> Result<(), LogError> {
        // Signal only after the real FileLog durability barrier. Holding
        // the return keeps the sequencer from acknowledging the hook.
        self.inner.sync()?;
        self.synced
            .send(())
            .map_err(|e| LogError::Io(std::io::Error::other(e.to_string())))?;
        self.release
            .recv()
            .map_err(|e| LogError::Io(std::io::Error::other(e.to_string())))?;
        Ok(())
    }
}

fn git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "tag.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "trace")
        .env("GIT_AUTHOR_EMAIL", "trace@example.invalid")
        .env("GIT_COMMITTER_NAME", "trace")
        .env("GIT_COMMITTER_EMAIL", "trace@example.invalid")
        .output()
        .expect("git runs")
}

fn is_ref(repo: &std::path::Path, name: &str, expected: &str) -> bool {
    let out = git(repo, &["rev-parse", "--verify", name]);
    out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == expected
}

/// Liveness budget for every barrier in this test.
///
/// Deliberately enormous relative to the ~300 ms this normally takes. None
/// of this test's assertions are about *timing* — they are about order, and
/// each barrier exists only so a hang fails instead of blocking forever. So
/// the budget costs nothing when the code is right and must not be tight
/// enough to fail when the code is right but the machine is slow.
///
/// Measured 2026-08-11 on this M1, idle, same commit throughout: the wait
/// from spawning the push to the durability signal ranged from 291 ms to
/// 16.7 s, and the signal arrived in 32 runs out of 32 when given 30 s.
/// Against that spread a 5 s budget failed 5 times in 30 one hour and 0
/// times in 30 the next, which made it look like a code regression and cost
/// a bisect that pointed at an innocent commit. `PHASE0.md` has the story.
const BARRIER_BUDGET: Duration = Duration::from_secs(60);

fn wait_for(path: &std::path::Path) -> bool {
    let deadline = Instant::now() + BARRIER_BUDGET;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

#[test]
fn durable_log_precedes_ref_publication_precedes_client_ack() {
    let work = std::env::temp_dir().join(format!("choir-git-trace-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let log_path = work.join("ops.jsonl");

    let (synced_tx, synced_rx) = mpsc::channel();
    let (release_sync_tx, release_sync_rx) = mpsc::channel();
    let log = BlockingSyncLog {
        inner: FileLog::open(&log_path).unwrap(),
        synced: synced_tx,
        release: release_sync_rx,
    };

    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
    node.enable_platform(
        Platform::start(Registry::new(), Box::new(log), ActorKey::generate()).unwrap(),
    );
    let port = node.port();
    node.create_repo("agents/trace.git").unwrap();
    let bare = work.join("repos/agents/trace.git");

    // `post-receive` runs after Git has published refs but before
    // receive-pack answers the client. Its two files form an observation
    // latch independent of the Choir implementation.
    let published = bare.join("hooks/trace-published");
    let release_publish = bare.join("hooks/trace-release");
    let post_receive = bare.join("hooks/post-receive");
    std::fs::write(
        &post_receive,
        "#!/bin/sh\n: > hooks/trace-published\nwhile [ ! -e hooks/trace-release ]; do sleep 0.01; done\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&post_receive, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    let url = format!("http://127.0.0.1:{port}/agents/trace.git");
    let checkout = work.join("checkout");
    assert!(
        git(&work, &["clone", "-q", &url, checkout.to_str().unwrap()])
            .status
            .success()
    );
    std::fs::write(checkout.join("trace.txt"), "ordered\n").unwrap();
    assert!(git(&checkout, &["add", "."]).status.success());
    assert!(git(&checkout, &["commit", "-q", "-m", "trace ordering"])
        .status
        .success());
    let expected = String::from_utf8(git(&checkout, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_string();

    let (push_tx, push_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = push_tx.send(git(
            &checkout,
            &["push", "-q", "origin", "HEAD:refs/heads/main"],
        ));
    });

    if synced_rx.recv_timeout(BARRIER_BUDGET).is_err() {
        let _ = release_sync_tx.send(());
        std::fs::write(&release_publish, b"").ok();
        // The barrier timing out is the symptom; git's own complaint is the
        // cause, and it is already sitting unread in `push_rx`. Panicking
        // without it names the thing that did not happen and discards the
        // reason, which is how this failure stayed a mystery for so long.
        //
        // Read this carefully if it ever fires again: the push result below
        // is collected *after* the barriers are released, so an exit status
        // of 0 here means the push completed once unblocked. It is not
        // evidence that the push completed before the timeout, and reading
        // it that way is how this was once mistaken for a push being
        // accepted with no op appended.
        let detail = match push_rx.recv_timeout(Duration::from_secs(30)) {
            Ok(out) => format!(
                "git push exited {:?}; stdout {:?}; stderr {:?}",
                out.status.code(),
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
            Err(error) => format!("git push had not returned either ({error})"),
        };
        panic!("push never reached the durable log barrier: {detail}");
    }

    // Stage 1: independently visible durable bytes, but Git has not moved
    // the ref and the client has not received an acknowledgement.
    let raw = std::fs::read_to_string(&log_path).expect("synced log is readable");
    let mut lines = raw.lines();
    let first = lines.next().expect("one durable log row");
    assert!(
        lines.next().is_none(),
        "one ref update must produce one durable row"
    );
    let row: serde_json::Value = serde_json::from_str(first).expect("raw JSON log row");
    assert_eq!(row["seq"], 0);
    assert_eq!(row["workspace"], "git/anon");
    let payload_bytes: Vec<u8> =
        serde_json::from_value(row["payload"].clone()).expect("payload byte array");
    let payload: serde_json::Value =
        serde_json::from_slice(&payload_bytes).expect("ViewOp-shaped payload JSON");
    let set_ref = &payload["kind"]["SetRef"];
    assert_eq!(set_ref["name"], "agents/trace.git:refs/heads/main");
    assert_eq!(set_ref["prev"], serde_json::Value::Null);
    assert_eq!(
        set_ref["commit"],
        serde_json::to_value(ContentHash::from_git_oid(&expected).unwrap()).unwrap(),
        "the durable op must authorize the exact oid Git publishes"
    );
    assert!(!is_ref(&bare, "refs/heads/main", &expected));
    assert!(matches!(push_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));

    release_sync_tx.send(()).unwrap();

    // The ref op's D25 attestation rides the same durability path: the
    // node appends a RecordRefSnapshot of the state the push left, and
    // that append's own barrier must be released before the hook can be
    // acknowledged and Git can publish.
    assert!(
        synced_rx.recv_timeout(BARRIER_BUDGET).is_ok(),
        "the ref op's attestation never reached the durability barrier"
    );
    let raw = std::fs::read_to_string(&log_path).expect("synced log is readable");
    assert_eq!(
        raw.lines().count(),
        2,
        "the attestation is the second durable row, before any publication"
    );
    assert!(!is_ref(&bare, "refs/heads/main", &expected));
    release_sync_tx.send(()).unwrap();

    if !wait_for(&published) {
        std::fs::write(&release_publish, b"").ok();
        panic!("push never reached post-receive publication boundary");
    }

    // Stage 2: Git has published the ref, while the blocking post-receive
    // hook proves receive-pack still has not acknowledged the client.
    assert!(is_ref(&bare, "refs/heads/main", &expected));
    assert!(matches!(push_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));

    std::fs::write(&release_publish, b"").unwrap();
    let pushed = push_rx
        .recv_timeout(BARRIER_BUDGET)
        .expect("client receives final acknowledgement");
    assert!(
        pushed.status.success(),
        "push failed: {}",
        String::from_utf8_lossy(&pushed.stderr)
    );

    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}
