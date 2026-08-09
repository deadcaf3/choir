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

fn wait_for(path: &std::path::Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
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

    if synced_rx.recv_timeout(Duration::from_secs(5)).is_err() {
        let _ = release_sync_tx.send(());
        std::fs::write(&release_publish, b"").ok();
        panic!("push never reached the durable log barrier");
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
    assert!(!is_ref(&bare, "refs/heads/main", &expected));
    assert!(matches!(push_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));

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
        .recv_timeout(Duration::from_secs(5))
        .expect("client receives final acknowledgement");
    assert!(
        pushed.status.success(),
        "push failed: {}",
        String::from_utf8_lossy(&pushed.stderr)
    );

    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}
