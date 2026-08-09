//! The daemon CLI wires reviewer conflict-graph policy into real draws.

use choir_identity::ActorKey;
use choir_node::platform::hex_encode;
use choir_view::{OpKind, ViewOp};

struct ChildGuard(std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.0.kill().ok();
        self.0.wait().ok();
    }
}

#[test]
fn daemon_flags_enforce_the_reviewer_conflict_distance() {
    let work = std::env::temp_dir().join(format!(
        "choir-node-reviewer-conflict-config-{}",
        std::process::id()
    ));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();

    let author = ActorKey::generate();
    let keys = work.join("keys");
    let reviewers = work.join("reviewers");
    let graph = work.join("reviewer-conflicts");
    std::fs::write(
        &keys,
        format!(
            "alice/requester {}\n",
            hex_encode(&author.public_key_bytes())
        ),
    )
    .unwrap();
    std::fs::write(
        &reviewers,
        "alice/sibling\nbob/one\ncarol/one\ndave/one\n",
    )
    .unwrap();
    std::fs::write(&graph, "alice bob\nbob carol\n").unwrap();

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let child = std::process::Command::new(env!("CARGO_BIN_EXE_choir-node"))
        .args([
            work.join("repos").as_os_str(),
            std::ffi::OsStr::new(&port.to_string()),
            std::ffi::OsStr::new("--keys-file"),
            keys.as_os_str(),
            std::ffi::OsStr::new("--reviewers-file"),
            reviewers.as_os_str(),
            std::ffi::OsStr::new("--reviewer-conflict-graph"),
            graph.as_os_str(),
            std::ffi::OsStr::new("--reviewer-conflict-distance"),
            std::ffi::OsStr::new("2"),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("start choir-node");
    let mut child = ChildGuard(child);

    let api = format!("http://127.0.0.1:{port}/api");
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        if let Some(status) = child.0.try_wait().unwrap() {
            panic!("choir-node exited before serving: {status}");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    let op = ViewOp::new(OpKind::RequestReview {
        id: "configured-graph".into(),
        target: choir_oplog::ContentHash::blake3(b"target"),
        reviewers: Vec::new(),
        target_ref: None,
    });
    let payload = op.to_payload();
    let signature = author.sign_submission("alice/requester", &payload);
    let body = serde_json::json!({
        "channel": "alice/requester",
        "payload_hex": hex_encode(&payload),
        "key_id": signature.key_id,
        "signature_hex": hex_encode(&signature.signature),
    })
    .to_string();
    let output = std::process::Command::new("curl")
        .args(["-s", "-X", "POST", "-d", &body, &format!("{api}/submit")])
        .output()
        .expect("curl runs");
    assert!(output.status.success());
    let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        response["reviewers"],
        serde_json::json!(["dave/one"]),
        "{response}"
    );

    child.0.kill().unwrap();
    child.0.wait().unwrap();
    std::fs::remove_dir_all(&work).unwrap();
}
