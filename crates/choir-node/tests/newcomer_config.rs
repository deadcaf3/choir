//! The daemon CLI wires its paired newcomer-harm files into the live view.

use choir_identity::ActorKey;
use choir_node::platform::hex_encode;

struct ChildGuard(std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.0.kill().ok();
        self.0.wait().ok();
    }
}

#[test]
fn daemon_flags_persist_the_activation_boundary() {
    let work = std::env::temp_dir().join(format!("choir-newcomer-config-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let incumbent = ActorKey::generate();
    let keys = work.join("keys");
    let audit = work.join("audit.jsonl");
    let adjudications = work.join("adjudications.jsonl");
    std::fs::write(
        &keys,
        format!(
            "incumbent/agent {}\n",
            hex_encode(&incumbent.public_key_bytes())
        ),
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
            std::ffi::OsStr::new("--newcomer-audit"),
            audit.as_os_str(),
            std::ffi::OsStr::new("--newcomer-adjudications"),
            adjudications.as_os_str(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("start choir-node");
    let mut child = ChildGuard(child);

    // Falling out of this loop unready used to be silent: the curl below
    // then hit a closed port and failed on `output.status.success()`,
    // which names neither the port nor the daemon. Observed for real on
    // 2026-08-10 during a loaded gate run. The budget is raised because
    // a freshly built binary competes with whatever else the machine is
    // doing, but the assertion is the actual fix — a slow spawn should
    // say so rather than impersonate a broken endpoint.
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
    let output = std::process::Command::new("curl")
        .args(["-s", &format!("http://127.0.0.1:{port}/api/view")])
        .output()
        .expect("curl runs");
    assert!(output.status.success());
    let view: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(view["newcomer_harm"]["configured"], true, "{view}");
    assert_eq!(
        view["newcomer_harm"]["audit"]["incumbent_actor_keys_excluded"], 1,
        "{view}"
    );
    assert_eq!(view["newcomer_harm"]["audit"]["first_attempts"], 0);
    assert_eq!(view["newcomer_harm"]["measurement_complete"], false);

    let rows = std::fs::read_to_string(&audit).unwrap();
    let rows: Vec<serde_json::Value> = rows
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["format_version"], 1);
    assert_eq!(rows[0]["kind"], "activation");
    assert_eq!(
        rows[0]["incumbent_actor_keys"],
        serde_json::json!([incumbent.actor_id().to_hex()])
    );

    child.0.kill().unwrap();
    child.0.wait().unwrap();
    std::fs::remove_dir_all(work).unwrap();
}
