//! `choir doctor`, driven as a reader in trouble drives it: the real
//! binary, against real files on disk.
//!
//! The thing under test is not "does it print a table" — it is the two
//! properties an operator leans on. That a failure names the command
//! that fixes it, and that the exit code separates "degraded" from
//! "broken", because `choir doctor && …` is how it ends up used.
//!
//! Nothing here binds a port. The node-shaped states are covered where a
//! node already exists to answer; what is checkable without one is every
//! local check plus the no-node path, and those are the ones a reader
//! hits before they have a node at all.

use std::path::PathBuf;
use std::process::Command;

/// A private directory per test: this harness shares a process and runs
/// on parallel threads.
fn workdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "choir-cli-doctor-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Runs the built binary and returns `(exit code, stdout)`.
///
/// Run from the scratch directory, not the checkout: `doctor` walks
/// parents for `.choir/config` the way every other command does, and a
/// test standing in the repository would pick up whatever node the
/// developer running it has configured.
fn doctor(dir: &std::path::Path, args: &[&str]) -> (i32, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_choir"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("choir runs");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).to_string(),
    )
}

/// Writes an auth file at `mode` and returns its path as a string.
fn auth_file(dir: &std::path::Path, name: &str, mode: u32) -> String {
    let path = dir.join(name);
    std::fs::write(&path, "choir:token\n").expect("write auth");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod auth");
    }
    path.to_string_lossy().to_string()
}

/// The tools this workspace shells out to are all reported, and the ones
/// the test suite itself needs are found.
///
/// If `git` were missing, this crate's own tests could not have got
/// here — so a run that reaches this assertion and fails it is reporting
/// a broken check, not a broken machine.
#[test]
fn every_external_tool_is_reported() {
    let dir = workdir("tools");
    let (_, out) = doctor(&dir, &["doctor"]);
    for tool in ["git", "curl", "openssl", "ssh-keygen", "mergiraf", "mdbook"] {
        assert!(out.contains(tool), "no line for `{tool}` in:\n{out}");
    }
    assert!(
        out.lines().any(|l| l.contains("git") && l.contains("ok")),
        "git should be found by a suite that needed it to run:\n{out}"
    );
}

/// A missing optional tool warns; it must never fail the command.
///
/// Stated as the exit code rather than the word, because the exit code
/// is the part other programs read. A node without `mergiraf` still
/// merges — line merge and first-class conflicts — and a doctor that
/// exits nonzero for it teaches operators to stop running it.
#[test]
fn nothing_optional_fails_the_command() {
    let dir = workdir("optional");
    let auth = auth_file(&dir, "auth", 0o600);
    let (code, out) = doctor(&dir, &["--auth-file", &auth, "doctor"]);
    assert_eq!(code, 0, "no required thing is missing here:\n{out}");
}

/// A world-readable auth file is a failure, not a warning, and the fix
/// is the exact `chmod` for that path.
///
/// The file carries a bearer credential for a node. Mode 0644 means
/// every account on the machine holds it, and every command would keep
/// working — which is precisely why nothing else reports it.
#[test]
#[cfg(unix)]
fn a_readable_auth_file_fails_with_its_own_chmod() {
    let dir = workdir("mode");
    let auth = auth_file(&dir, "auth", 0o644);
    let (code, out) = doctor(&dir, &["--auth-file", &auth, "doctor"]);
    assert_eq!(code, 1, "a leaked credential is a failure:\n{out}");
    assert!(
        out.contains("0644"),
        "the mode found should be named:\n{out}"
    );
    assert!(
        out.contains(&format!("chmod 600 {auth}")),
        "the fix should be the runnable command:\n{out}"
    );
}

/// An auth file that is not there names the command that issues one.
#[test]
fn a_missing_auth_file_names_the_command_that_issues_one() {
    let dir = workdir("absent");
    let auth = dir.join("not-here").to_string_lossy().to_string();
    let (code, out) = doctor(&dir, &["--auth-file", &auth, "doctor"]);
    assert_eq!(code, 1, "an unreadable credential is a failure:\n{out}");
    assert!(
        out.contains("choir join"),
        "should point at `choir join`:\n{out}"
    );
}

/// With no node configured and none given, the command still runs and
/// still exits 0.
///
/// This is the state a reader is in *before* they have anything: the
/// one moment a diagnostic tool must not itself refuse to run.
#[test]
fn no_node_configured_is_not_a_failure() {
    let dir = workdir("nonode");
    let (code, out) = doctor(&dir, &["doctor"]);
    assert_eq!(
        code, 0,
        "having no node yet is not a broken machine:\n{out}"
    );
    assert!(out.contains("no node configured"), "{out}");
    assert!(
        out.contains(".choir/config"),
        "should say where a node is named:\n{out}"
    );
}

/// A node that does not answer is a failure, and the failure does not
/// swallow the local checks that passed alongside it.
///
/// Port 1 rather than a high one: nothing binds it, and unlike a
/// random high port there is no chance of colliding with a service this
/// machine happens to be running.
#[test]
fn an_unreachable_node_fails_without_hiding_the_rest() {
    let dir = workdir("unreachable");
    let (code, out) = doctor(&dir, &["doctor", "http://127.0.0.1:1"]);
    assert_eq!(code, 1, "an unreachable node is a failure:\n{out}");
    assert!(out.contains("git"), "local checks still reported:\n{out}");
}

/// A URL that is not one is refused as a finding, not as a usage error.
///
/// `doctor` is the command someone runs *because* their configuration is
/// wrong. Exiting 2 and printing usage at them would answer the question
/// they already know the answer to.
#[test]
fn a_malformed_node_url_is_a_finding() {
    let dir = workdir("badurl");
    let (code, out) = doctor(&dir, &["doctor", "127.0.0.1:8417"]);
    assert_eq!(code, 1, "{out}");
    assert!(
        out.contains("http://"),
        "should say what a URL must start with:\n{out}"
    );
}
