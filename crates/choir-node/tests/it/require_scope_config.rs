//! The daemon CLI must not couple replay containment to reviewer policy.

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
fn daemon_flag_requires_scope_without_a_reviewer_file() {
    let work = std::env::temp_dir().join(format!(
        "choir-node-require-scope-config-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();

    let author = ActorKey::generate();
    let keys = work.join("keys");
    std::fs::write(
        &keys,
        format!("alice/agent {}\n", hex_encode(&author.public_key_bytes())),
    )
    .unwrap();

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let child = std::process::Command::new(env!("CARGO_BIN_EXE_choir-node"))
        .args([
            work.join("repos").as_os_str(),
            std::ffi::OsStr::new(&port.to_string()),
            std::ffi::OsStr::new("--keys-file"),
            keys.as_os_str(),
            std::ffi::OsStr::new("--require-scope"),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("start choir-node");
    let mut child = ChildGuard(child);

    let mut ready = false;
    for _ in 0..500 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            ready = true;
            break;
        }
        if let Some(status) = child.0.try_wait().unwrap() {
            panic!("choir-node exited before serving: {status}");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(ready, "choir-node did not bind port {port} within 10s");

    let api = format!("http://127.0.0.1:{port}/api");
    let (code, view) = crate::support::curl(&[&format!("{api}/view")]);
    assert_eq!(code, 200, "{view}");
    assert_eq!(view["log"]["scope_required"], true, "{view}");

    let op = ViewOp::new(OpKind::SetRef {
        name: "owner/repo:refs/heads/main".into(),
        commit: choir_oplog::ContentHash::blake3(b"target"),
        prev: None,
    });
    let body = crate::support::submit_body(&author, "alice/agent", &op);
    let (code, refusal) =
        crate::support::curl(&["-X", "POST", "-d", &body, &format!("{api}/submit")]);
    assert_eq!(code, 400, "{refusal}");
    assert_eq!(refusal["code"], "scope_required", "{refusal}");

    child.0.kill().unwrap();
    child.0.wait().unwrap();
    std::fs::remove_dir_all(work).unwrap();
}

#[test]
fn require_scope_without_the_platform_is_refused() {
    let work = std::env::temp_dir().join(format!(
        "choir-node-require-scope-no-platform-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::remove_dir_all(&work).ok();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_choir-node"))
        .args([
            work.join("repos").as_os_str(),
            std::ffi::OsStr::new("0"),
            std::ffi::OsStr::new("--require-scope"),
        ])
        .output()
        .expect("run choir-node");
    assert!(
        !output.status.success(),
        "a decorative security flag must fail"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--require-scope needs --keys-file"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::remove_dir_all(work).ok();
}
