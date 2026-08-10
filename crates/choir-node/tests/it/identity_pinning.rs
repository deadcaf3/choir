//! The node mints a fresh signing key whenever its key file is absent.
//! That is right on a first start and wrong on a migration: move the op
//! log without the key and the daemon appends under a new author, with
//! both identities individually valid and nothing to mark the seam.
//! The fingerprint beside the log turns that into a refusal to start.

use std::process::Command;

fn state(work: &std::path::Path) -> std::path::PathBuf {
    work.join("repos").join(".choir")
}

/// Start the daemon and find out whether it stayed up. The daemon serves
/// forever, so "started" can only be evidenced by *not* exiting — an
/// earlier version of this used "the fingerprint file exists", which the
/// third case below already satisfies before the process has had time to
/// fail, and it reported a refusal as a success.
fn boot(work: &std::path::Path, auth: &std::path::Path) -> (bool, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_choir-node"))
        .arg(work.join("repos"))
        .arg("0")
        .arg("--auth-file")
        .arg(auth)
        // The platform half — node key, fingerprint, op log — only exists
        // when the node is serving the API, which --keys-file turns on.
        .arg("--keys-file")
        .arg(auth.with_file_name("keys"))
        .arg("--bind")
        .arg("127.0.0.1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn choir-node");

    let settle = std::time::Instant::now() + std::time::Duration::from_millis(1500);
    while std::time::Instant::now() < settle {
        if child.try_wait().expect("wait").is_some() {
            let out = child.wait_with_output().expect("output");
            return (
                false,
                format!(
                    "{}{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                ),
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    child.kill().ok();
    let out = child.wait_with_output().expect("output");
    (
        true,
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

#[test]
fn a_moved_log_without_its_key_refuses_to_start() {
    let work = std::env::temp_dir().join(format!("choir-node-pin-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(work.join("repos")).unwrap();
    let auth = work.join("auth");
    std::fs::write(&auth, "choir:pintest\n").unwrap();
    std::fs::write(work.join("keys"), "").unwrap();

    // First start pins whatever key it minted.
    let (ok, text) = boot(&work, &auth);
    assert!(ok, "first start should succeed: {text}");
    let pinned = std::fs::read_to_string(state(&work).join("node.fingerprint"))
        .expect("first start records a fingerprint");
    // Invariant 2: a ContentHash is self-describing, so its hex carries a
    // codec prefix. Assert that shape rather than a magic length, which
    // would have to change if the codec ever did.
    let pin = pinned.trim();
    assert!(
        pin.contains('-') && pin.len() > 64,
        "fingerprint should be a codec-prefixed actor id, got {pin:?}"
    );

    // Restarting with the same state is fine — the pin must not be a
    // one-shot that fires on every boot.
    let (ok, text) = boot(&work, &auth);
    assert!(ok, "restart with matching key should succeed: {text}");

    // Now the migration accident: the log survives, the key does not.
    std::fs::remove_file(state(&work).join("node.key")).unwrap();
    let (ok, text) = boot(&work, &auth);
    assert!(
        !ok,
        "a node that lost its key must refuse to append under a new identity, got: {text}"
    );
    assert!(
        text.contains("node identity changed"),
        "the refusal must say what happened, got: {text}"
    );
    assert!(
        text.contains(&pinned.trim().to_string()),
        "the refusal must name the identity it expected, got: {text}"
    );

    std::fs::remove_dir_all(work).ok();
}
