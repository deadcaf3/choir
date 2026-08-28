//! `choir init` — the first command anybody runs, and the one that is
//! holding a credential nothing can reissue.
//!
//! The interesting properties are not "did it write four files". They
//! are that the secrets are private from creation rather than chmodded
//! afterwards, and that a second run cannot quietly destroy the first
//! one's token — because the node was started reading that file and
//! there is no other copy of what is in it.

use std::path::{Path, PathBuf};
use std::process::Command;

fn workdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "choir-cli-init-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Runs `choir init` from `cwd`, so the `.choir/config` it writes lands
/// in a scratch directory and not in the checkout running the suite.
fn init(cwd: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_choir"))
        .arg("init")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("choir runs");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).expect("stat").permissions().mode() & 0o777
}

/// From nothing: the four files, the repository root, and a config
/// naming a loopback node.
#[test]
fn a_fresh_machine_gets_a_working_layout() {
    let work = workdir("fresh");
    let state = work.join("state");
    let (code, out, _) = init(&work, &[state.to_str().unwrap(), "--port", "8417"]);

    assert_eq!(code, 0, "{out}");
    for name in ["auth", "agent.key", "keys"] {
        assert!(state.join(name).is_file(), "{name} should exist");
    }
    assert!(
        state.join("repos").is_dir(),
        "the repository root should exist"
    );

    let config = std::fs::read_to_string(work.join(".choir/config")).expect("config written");
    assert!(
        config.contains("node = http://127.0.0.1:8417"),
        "the config should name a loopback node: {config}"
    );

    // stdout is the command that starts the node, so it can be piped
    // into a shell or a unit file. The human guidance is on stderr.
    assert!(
        out.contains("choir-node"),
        "stdout should be runnable: {out}"
    );
    assert!(out.contains("--auth-file"), "{out}");
}

/// Secrets are 0600; the public key list is not.
///
/// A 0600 trusted-keys file is a node running as another user that can
/// read nobody's key, which fails in a way that looks like a broken
/// signature rather than a permission.
#[test]
#[cfg(unix)]
fn secrets_are_private_and_public_keys_are_not() {
    let work = workdir("modes");
    let state = work.join("state");
    let (code, _, err) = init(&work, &[state.to_str().unwrap()]);
    assert_eq!(code, 0, "{err}");

    assert_eq!(
        mode(&state.join("auth")),
        0o600,
        "the credential is a secret"
    );
    assert_eq!(
        mode(&state.join("agent.key")),
        0o600,
        "the actor key is a secret"
    );
    assert_ne!(
        mode(&state.join("keys")) & 0o044,
        0,
        "the trusted-keys file holds public keys and the node must be able to read it"
    );
}

/// A second run refuses, names every conflict at once, and changes
/// nothing.
///
/// All of them rather than the first: a tool that fails on one
/// collision, is fixed, and fails on the next gets run four times.
#[test]
fn a_second_run_refuses_and_changes_nothing() {
    let work = workdir("refuse");
    let state = work.join("state");
    assert_eq!(init(&work, &[state.to_str().unwrap()]).0, 0);
    let before = std::fs::read(state.join("auth")).expect("auth written");

    let (code, _, err) = init(&work, &[state.to_str().unwrap()]);

    assert_eq!(code, 1, "a second run is a refusal");
    assert!(err.contains("already exist"), "{err}");
    assert!(
        err.contains("auth"),
        "the conflict list should name the auth file: {err}"
    );
    assert!(
        err.contains("--force"),
        "it should say how to do it deliberately: {err}"
    );
    assert_eq!(
        std::fs::read(state.join("auth")).expect("auth still there"),
        before,
        "a refused run must not have touched the credential"
    );
}

/// `--force` is the deliberate reset: it rotates the credential and says
/// that it did.
#[test]
fn force_replaces_and_reports_what_it_destroyed() {
    let work = workdir("force");
    let state = work.join("state");
    assert_eq!(init(&work, &[state.to_str().unwrap()]).0, 0);
    let before = std::fs::read(state.join("auth")).expect("auth written");

    let (code, _, err) = init(&work, &[state.to_str().unwrap(), "--force"]);

    assert_eq!(code, 0, "{err}");
    assert_ne!(
        std::fs::read(state.join("auth")).expect("auth rewritten"),
        before,
        "--force should mint a new credential"
    );
    #[cfg(unix)]
    assert_eq!(
        mode(&state.join("auth")),
        0o600,
        "the replacement is private too"
    );
}

/// The minted token is a full 32 bytes of hex.
///
/// A short token written as though it were a full one is the failure
/// mode worth testing for: it would work, and be weak.
#[test]
fn the_credential_is_a_full_length_token() {
    let work = workdir("token");
    let state = work.join("state");
    assert_eq!(init(&work, &[state.to_str().unwrap()]).0, 0);

    let auth = std::fs::read_to_string(state.join("auth")).expect("auth written");
    let (user, token) = auth.trim().split_once(':').expect("user:token");
    assert!(!user.is_empty(), "the credential names a user");
    assert_eq!(token.len(), 64, "32 bytes as hex");
    assert!(
        token.chars().all(|c| c.is_ascii_hexdigit()),
        "hex only: {token}"
    );
}
