//! The node mints a fresh signing key whenever its key file is absent.
//! That is right on a first start and wrong on a migration: move the op
//! log without the key and the daemon appends under a new author, with
//! both identities individually valid and nothing to mark the seam.
//! The fingerprint beside the log turns that into a refusal to start.

use std::process::Command;

fn state(work: &std::path::Path) -> std::path::PathBuf {
    work.join("repos").join(".choir")
}

/// Start the daemon and find out whether it stayed up.
///
/// Success is read from the daemon's own words: it prints `choir-node
/// serving …` once, after the fingerprint check, and a refusal exits
/// before reaching it. So this waits for that line or for the process to
/// die, and never for a duration.
///
/// Two earlier signals were wrong in opposite directions, and both are
/// worth keeping written down. "The fingerprint file exists" is satisfied
/// by the refusing case before it has had time to fail, so it reported a
/// refusal as a success. "The process has not exited after 1500 ms" is
/// not a fact about the daemon at all — it is a fact about how loaded the
/// machine is, and it duly broke when a sibling module added six tests to
/// this shared harness: a correctly booting daemon missed the window in
/// two runs of three. Raising the number would have bought time until the
/// next module lands. The marker cannot drift that way.
///
/// The remaining timeout is a deadlock guard, deliberately far larger
/// than any plausible boot, and reaching it is a failure rather than a
/// verdict.
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

    // stderr is read on its own thread: the daemon writes the marker and
    // then blocks serving forever, so a read on this thread would too.
    let stderr = child.stderr.take().expect("piped stderr");
    let seen = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let text = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    {
        let (seen, text) = (seen.clone(), text.clone());
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
                let serving = line.contains("choir-node serving");
                text.lock().expect("stderr text").push_str(&line);
                text.lock().expect("stderr text").push('\n');
                if serving {
                    seen.store(true, std::sync::atomic::Ordering::Release);
                }
            }
        });
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let outcome = loop {
        if seen.load(std::sync::atomic::Ordering::Acquire) {
            break Some(true);
        }
        if child.try_wait().expect("wait").is_some() {
            // Let the reader drain the rest of the pipe before we read it.
            std::thread::sleep(std::time::Duration::from_millis(50));
            break Some(false);
        }
        if std::time::Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    child.kill().ok();
    child.wait().ok();
    let text = text.lock().expect("stderr text").clone();
    match outcome {
        Some(started) => (started, text),
        None => panic!("choir-node neither served nor exited within 60s, stderr:\n{text}"),
    }
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
