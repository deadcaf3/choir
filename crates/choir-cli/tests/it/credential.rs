//! The credential nobody typed.
//!
//! `choir init` writes `~/.choir/auth` and every command that reads the
//! node it created used to answer 401 until `--auth-file` was pointed
//! back at it — a path the reader has to learn before the tool will talk
//! to the node the same tool just made.
//!
//! The interesting half is the boundary. A bearer token filled in
//! because it was found is a token that can be sent somewhere the reader
//! did not intend, so the default is scoped: loopback, or the node
//! `.choir/config` names, and nowhere else. These tests are mostly about
//! that "nowhere else".

use std::path::{Path, PathBuf};

fn choir() -> &'static str {
    env!("CARGO_BIN_EXE_choir")
}

/// A home directory holding a node's credential at the mode `choir init`
/// writes it with, plus a separate working directory to run from.
fn home(tag: &str, node: Option<&str>) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("choir-cli-cred-{tag}"));
    std::fs::remove_dir_all(&root).ok();
    let home = root.join("home");
    let work = root.join("work");
    std::fs::create_dir_all(home.join(".choir")).expect("home");
    std::fs::create_dir_all(&work).expect("work");
    choir_fs::write_atomic_private(&home.join(".choir/auth"), "choir:token\n").expect("auth");
    if let Some(node) = node {
        std::fs::create_dir_all(work.join(".choir")).expect(".choir");
        std::fs::write(work.join(".choir/config"), format!("node = {node}\n")).expect("config");
    }
    (home, work)
}

/// `choir doctor` names the credential it would actually use, which is
/// what makes the default observable without a node to authenticate to.
fn doctor_auth_line(home: &Path, work: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new(choir())
        .args(args)
        .current_dir(work)
        .env("HOME", home)
        .output()
        .expect("choir runs");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find(|line| line.contains("auth file"))
        .unwrap_or_default()
        .to_string()
}

/// Nothing listens on port 1, so the node check fails to connect rather
/// than failing to parse — and the auth line is reached either way.
const DEAD_LOOPBACK: &str = "http://127.0.0.1:1";

#[test]
fn a_loopback_node_gets_the_credential_init_wrote() {
    let (home, work) = home("loopback", None);
    let line = doctor_auth_line(&home, &work, &["doctor", DEAD_LOOPBACK]);
    assert!(line.contains(".choir/auth"), "{line}");
    assert!(line.contains("0600"), "{line}");
}

/// The whole point of scoping it. A URL typed after a command that takes
/// one is not consent to send this machine's bearer token to it.
#[test]
fn a_stranger_does_not_get_it() {
    let (home, work) = home("stranger", None);
    let line = doctor_auth_line(&home, &work, &["doctor", "https://example.invalid"]);
    assert!(line.contains("none given"), "{line}");
    assert!(!line.contains(".choir/auth"), "{line}");
}

/// A node this checkout named is a node this checkout is entitled to
/// authenticate to, however it is spelled.
#[test]
fn the_configured_node_gets_it_even_when_it_is_not_loopback() {
    let (home, work) = home("configured", Some("https://node.invalid"));
    let line = doctor_auth_line(&home, &work, &["doctor", "https://node.invalid"]);
    assert!(line.contains(".choir/auth"), "{line}");
}

/// Same file, but named rather than found: an explicit path is the
/// reader saying which credential goes where, and is never second-
/// guessed by the scoping rule above.
#[test]
fn an_explicit_auth_file_still_wins_everywhere() {
    let (home, work) = home("explicit", None);
    let path = home.join(".choir/auth");
    let named = path.display().to_string();
    let line = doctor_auth_line(
        &home,
        &work,
        &["--auth-file", &named, "doctor", "https://example.invalid"],
    );
    assert!(line.contains(".choir/auth"), "{line}");
}

/// A machine with no node set up yet must still be able to run the
/// command that says so.
#[test]
fn no_credential_anywhere_is_a_warning_not_a_crash() {
    let root = std::env::temp_dir().join("choir-cli-cred-none");
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(root.join("home")).expect("home");
    std::fs::create_dir_all(root.join("work")).expect("work");
    let out = std::process::Command::new(choir())
        .args(["doctor", DEAD_LOOPBACK])
        .current_dir(root.join("work"))
        .env("HOME", root.join("home"))
        .output()
        .expect("choir runs");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("none given, and no default found"), "{text}");
}

/// The commands that take no credential at all must keep refusing one,
/// which is why explicitness is recorded separately from the value.
#[test]
fn init_still_refuses_an_auth_flag() {
    let (home, work) = home("init-guard", None);
    let path = home.join(".choir/auth").display().to_string();
    let out = std::process::Command::new(choir())
        .args(["--auth-file", &path, "init"])
        .current_dir(&work)
        .env("HOME", &home)
        .output()
        .expect("choir runs");
    assert_eq!(out.status.code(), Some(2), "init takes no credential");
}
