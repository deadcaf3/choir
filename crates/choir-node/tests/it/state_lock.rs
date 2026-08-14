//! One writer per state dir (internal/oak.md item 6): a second daemon on
//! the same root would append to the same ops.jsonl and fork the chain,
//! so serve start takes a PID lock on `.choir`. A dead holder's lock is
//! reaped, so a crashed daemon never wedges the next start.

use std::process::{Child, Command};

/// Spawn the daemon and watch its stderr for the serving marker. Same
/// verdict discipline as identity_pinning::boot: success is the daemon's
/// own "choir-node serving" line, failure is an exit before it, and the
/// 60s deadline is a deadlock guard, not a verdict.
struct Daemon {
    child: Child,
    seen: std::sync::Arc<std::sync::atomic::AtomicBool>,
    text: std::sync::Arc<std::sync::Mutex<String>>,
}

fn spawn(work: &std::path::Path) -> Daemon {
    let mut child = Command::new(env!("CARGO_BIN_EXE_choir-node"))
        .arg(work.join("repos"))
        .arg("0")
        // The state dir — node key, fingerprint, op log, lock — only
        // exists when the platform is on, which --keys-file turns on.
        .arg("--keys-file")
        .arg(work.join("keys"))
        .arg("--bind")
        .arg("127.0.0.1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn choir-node");
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
    Daemon { child, seen, text }
}

impl Daemon {
    /// True once "choir-node serving" appears; false if the process exits
    /// first.
    fn wait_verdict(&mut self) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            if self.seen.load(std::sync::atomic::Ordering::Acquire) {
                return true;
            }
            if self.child.try_wait().expect("wait").is_some() {
                // Let the reader drain the rest of the pipe.
                std::thread::sleep(std::time::Duration::from_millis(50));
                return false;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "choir-node neither served nor exited within 60s, stderr:\n{}",
                self.text()
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn text(&self) -> String {
        self.text.lock().expect("stderr text").clone()
    }

    fn kill(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

#[test]
fn a_second_node_on_the_same_root_refuses_and_a_dead_ones_lock_is_reaped() {
    let work = std::env::temp_dir().join(format!("choir-node-lock-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(work.join("repos")).unwrap();
    std::fs::write(work.join("keys"), "").unwrap();

    let mut first = spawn(&work);
    assert!(first.wait_verdict(), "first daemon should serve: {}", first.text());
    let lock_path = work.join("repos/.choir/wdlock");
    assert!(lock_path.is_file(), "serving daemon should hold the state lock");

    let mut second = spawn(&work);
    assert!(
        !second.wait_verdict(),
        "a second daemon on the same root must refuse: {}",
        second.text()
    );
    assert!(
        second.text().contains("in use by another running choir-node"),
        "the refusal must say who has it and why it matters, got: {}",
        second.text()
    );

    // SIGKILL the holder: the lock file stays behind with a dead PID.
    // The next start must reap it rather than wedge.
    first.kill();
    assert!(lock_path.is_file(), "a killed daemon leaves its lock behind");
    let mut third = spawn(&work);
    assert!(
        third.wait_verdict(),
        "a dead holder's lock must be reaped, got: {}",
        third.text()
    );
    third.kill();

    std::fs::remove_dir_all(&work).ok();
}
