//! A durability failure must terminate an idle daemon for supervision.
//!
//! This is its own test binary because the assertion is the process exit
//! itself. The parent runs this same test in a child; only the child lets
//! `Node::serve_forever` call `process::exit(75)`.

use choir_identity::{ActorKey, Registry};
use choir_node::{Node, Platform};
use choir_oplog::{ContentHash, LogError, MemLog, OpEntry, OpLog};
use choir_view::{OpKind, ViewOp};

struct FailingSync(MemLog);

impl OpLog for FailingSync {
    fn append(&mut self, entry: OpEntry) -> Result<ContentHash, LogError> {
        self.0.append(entry)
    }

    fn head(&self) -> Option<ContentHash> {
        self.0.head()
    }

    fn len(&self) -> u64 {
        self.0.len()
    }

    fn get(&self, seq: u64) -> Option<OpEntry> {
        self.0.get(seq)
    }

    fn last(&self) -> Option<&OpEntry> {
        self.0.last()
    }

    fn sync(&mut self) -> Result<(), LogError> {
        Err(LogError::Corrupt("injected sync failure".into()))
    }
}

fn child() {
    let work = std::env::temp_dir().join(format!("choir-durability-exit-{}", std::process::id()));
    std::fs::create_dir_all(&work).expect("work dir");
    let author = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&author.public_key_bytes()).unwrap();
    let platform = Platform::start(
        registry,
        Box::new(FailingSync(MemLog::new())),
        ActorKey::generate(),
    )
    .unwrap();
    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
    let port = node.port();
    node.enable_platform(platform);

    std::thread::spawn(move || {
        let op = ViewOp::new(OpKind::SetRef {
            name: "owner/repo:refs/heads/main".into(),
            commit: ContentHash::blake3(b"target"),
            prev: None,
        });
        let payload = op.to_payload();
        let sig = author.sign_submission("alice/agent", &payload);
        let body = serde_json::json!({
            "channel": "alice/agent",
            "payload_hex": choir_node::platform::hex_encode(&payload),
            "key_id": sig.key_id,
            "signature_hex": choir_node::platform::hex_encode(&sig.signature),
        })
        .to_string();
        let _ = std::process::Command::new("curl")
            .args([
                "-s",
                "-X",
                "POST",
                "-d",
                &body,
                &format!("http://127.0.0.1:{port}/api/submit"),
            ])
            .output();
    });

    node.serve_forever();
    panic!("a poisoned daemon returned instead of exiting");
}

#[test]
fn daemon_exits_after_sync_failure_without_another_request() {
    const CHILD: &str = "CHOIR_DURABILITY_EXIT_CHILD";
    if std::env::var_os(CHILD).is_some() {
        child();
        return;
    }

    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "daemon_exits_after_sync_failure_without_another_request",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn child test");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll child") {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().ok();
            panic!("daemon stayed alive after its sequencer lost durability");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert_eq!(
        status.code(),
        Some(75),
        "daemon must exit EX_TEMPFAIL for supervisor restart: {status}"
    );
}
