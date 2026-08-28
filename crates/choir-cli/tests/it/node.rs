//! `choir node status` as an operator runs it: the real binary.
//!
//! The rendering is unit-tested in `choir_cli::node`, where the report
//! is a pure function of the node's own JSON and every disclosure case
//! can be built by hand. What is only checkable here is the wiring: that
//! the command is reachable, that it refuses rather than guesses when it
//! has no node, and that an unreachable one is a nonzero exit rather
//! than an empty report.

use std::process::Command;

/// Runs the built binary from a directory with no `.choir/config` above
/// it, so the parent walk cannot pick up whatever node the developer
/// running the suite happens to have configured.
fn node_status(args: &[&str]) -> (i32, String, String) {
    let dir = std::env::temp_dir().join(format!(
        "choir-cli-nodestatus-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let output = Command::new(env!("CARGO_BIN_EXE_choir"))
        .args(args)
        .current_dir(&dir)
        .output()
        .expect("choir runs");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

/// With no node named and none configured, it says so and exits 2.
///
/// A usage error rather than a finding, unlike `choir doctor`: `doctor`
/// exists to be run on a machine that has nothing set up, and this one
/// is asking about a specific node. "Which node?" is an unanswered
/// argument, not a diagnosis.
#[test]
fn no_node_is_a_usage_error_naming_where_one_is_configured() {
    let (code, _, err) = node_status(&["node", "status"]);
    assert_eq!(code, 2, "{err}");
    assert!(
        err.contains(".choir/config"),
        "should name the file:\n{err}"
    );
}

/// An unreachable node exits nonzero rather than printing a report full
/// of blanks.
#[test]
fn an_unreachable_node_is_a_nonzero_exit() {
    let (code, out, _) = node_status(&["node", "status", "http://127.0.0.1:1"]);
    assert_eq!(code, 1, "{out}");
}

/// The command is reachable under its two-word name, and `--help`
/// answers without satisfying the argument rules first.
#[test]
fn the_two_word_name_resolves_its_own_help() {
    let (code, out, _) = node_status(&["node", "status", "--help"]);
    assert_eq!(code, 0);
    assert!(out.contains("choir node status"), "{out}");
}
