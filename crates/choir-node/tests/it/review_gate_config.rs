//! BETA-01. The review gate must refuse a configuration it cannot enforce.
//!
//! `--require-review` needs two other files to mean anything: a reviewer
//! pool to draw assent from, and a protected-refs list saying which
//! landings the assent gates. Named without either, the flag is
//! decorative — the daemon starts, the operator's shell history says the
//! gate is on, and every protected ref lands unreviewed. A gate that
//! silently does nothing is worse than no gate, so all three shapes exit
//! nonzero at startup rather than at the first landing nobody blocked.

use choir_identity::ActorKey;
use choir_node::platform::hex_encode;

/// Runs the daemon with `args` after the root and port, and returns
/// whether it exited nonzero together with everything it said on stderr.
///
/// The wait is bounded because the failure mode here is a daemon that
/// *serves*: a refusal these tests lose does not make the child exit with
/// the wrong status, it makes the child never exit at all. Blocking on
/// output would turn that into a hung suite rather than a red one, so the
/// child is killed after ten seconds and reported as not having refused.
fn refusal(name: &str, args: &[&str]) -> (bool, String) {
    let work = std::env::temp_dir().join(format!(
        "choir-node-review-gate-config-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();

    let keys = work.join("keys");
    std::fs::write(
        &keys,
        format!(
            "alice/agent {}\n",
            hex_encode(&ActorKey::generate().public_key_bytes())
        ),
    )
    .unwrap();

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_choir-node"));
    command.args([
        work.join("repos").as_os_str(),
        std::ffi::OsStr::new("0"),
        std::ffi::OsStr::new("--keys-file"),
        keys.as_os_str(),
    ]);
    for arg in args {
        // A `%KEYS%` placeholder lets a case name the pool file that this
        // helper is the only thing to know the path of.
        command.arg(if *arg == "%KEYS%" {
            keys.to_string_lossy().to_string()
        } else {
            (*arg).to_string()
        });
    }
    let mut child = command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("start choir-node");
    let mut exit = None;
    for _ in 0..500 {
        if let Some(status) = child.try_wait().expect("wait on choir-node") {
            exit = Some(status);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if exit.is_none() {
        child.kill().ok();
        child.wait().ok();
    }
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        use std::io::Read;
        pipe.read_to_string(&mut stderr).ok();
    }
    std::fs::remove_dir_all(&work).ok();
    (exit.is_some_and(|status| !status.success()), stderr)
}

#[test]
fn require_review_without_a_reviewer_pool_is_refused() {
    let (failed, stderr) = refusal("no-pool", &["--require-review"]);
    assert!(failed, "a review gate with no pool must fail: {stderr}");
    assert!(
        stderr.contains("review assignment policy needs --reviewers-file"),
        "{stderr}"
    );
}

#[test]
fn require_review_without_protected_refs_is_refused() {
    let (failed, stderr) = refusal(
        "no-protected-refs",
        &["--reviewers-file", "%KEYS%", "--require-review"],
    );
    assert!(
        failed,
        "a review gate protecting no ref must fail: {stderr}"
    );
    assert!(
        stderr.contains("--require-review needs --protected-refs"),
        "{stderr}"
    );
}

#[test]
fn protected_refs_without_a_reviewer_pool_is_refused() {
    let (failed, stderr) = refusal("refs-no-pool", &["--protected-refs", "%KEYS%"]);
    assert!(
        failed,
        "protected refs with nobody able to review them must fail: {stderr}"
    );
    assert!(
        stderr.contains("review assignment policy needs --reviewers-file"),
        "{stderr}"
    );
}
