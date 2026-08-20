//! The two things that shortened the command surface: per-command help,
//! and a node URL that does not have to be retyped.
//!
//! Both are conveniences, and the only interesting question about a
//! convenience is what it does when it is wrong. A default that silently
//! sends an operation to the wrong node would be far worse than typing
//! the URL, so what is asserted here is mostly the boundary: an explicit
//! URL always wins, a command that takes no node is never touched, and
//! with no config the behaviour is exactly what it was before.

use std::path::Path;

fn choir() -> &'static str {
    env!("CARGO_BIN_EXE_choir")
}

fn run_in(dir: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new(choir())
        .args(args)
        .current_dir(dir)
        .output()
        .expect("choir runs")
}

/// A scratch directory tree with an optional `.choir/config` at its root
/// and a nested working directory, so the upward walk is actually walked
/// rather than trivially satisfied.
fn tree(tag: &str, node: Option<&str>) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("choir-cli-config-{tag}"));
    std::fs::remove_dir_all(&root).ok();
    let deep = root.join("a/b/c");
    std::fs::create_dir_all(&deep).expect("scratch tree");
    if let Some(node) = node {
        std::fs::create_dir_all(root.join(".choir")).expect(".choir");
        std::fs::write(
            root.join(".choir/config"),
            format!("# the node this checkout belongs to\nnode = {node}\n"),
        )
        .expect("config");
    }
    deep
}

/// Port 1 is reserved and nothing listens on it, so a command that gets
/// this far fails to *connect* rather than failing to parse — which is
/// exactly the difference being measured.
const DEAD: &str = "http://127.0.0.1:1";

/// Exit code 2 is a usage error. Any other code means the arguments
/// parsed and the command ran.
fn is_usage_error(out: &std::process::Output) -> bool {
    out.status.code() == Some(2)
}

#[test]
fn a_configured_node_fills_in_the_url_from_any_depth() {
    let deep = tree("fills", Some(DEAD));
    let out = run_in(&deep, &["reviews", "ana"]);
    assert!(
        !is_usage_error(&out),
        "the node was not filled in, so the command never parsed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn without_a_config_the_url_is_still_required() {
    // The convenience must not become a requirement in reverse: a
    // checkout with no config behaves exactly as it did before this
    // existed, which is a usage error naming what is missing.
    let deep = tree("nofill", None);
    let out = run_in(&deep, &["reviews", "ana"]);
    assert!(
        is_usage_error(&out),
        "a missing node was accepted rather than refused"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("usage:"),
        "the refusal does not say how to invoke it"
    );
}

#[test]
fn an_explicit_url_beats_the_configured_one() {
    // The failure this prevents is the worst one available here: an
    // operator types a node and the tool quietly uses a different one.
    // Asserted by giving a config a URL that would *work* as a parse and
    // an argument that would not, and checking which one the error names.
    let deep = tree("explicit", Some("http://127.0.0.1:9"));
    let out = run_in(&deep, &["reviews", DEAD, "ana"]);
    // Asserted on the exit code, not on the text. The error a failed
    // connection prints does not name the URL it tried, so checking that
    // the configured one is absent from the message passes whether or
    // not it was used -- proved by mutation: inverting the precedence
    // left that assertion green. What cannot be faked is arity. Filling
    // in a node that was already given hands the command one argument
    // too many, and it dies at the pattern match with a usage error
    // instead of reaching the network at all.
    assert!(
        !is_usage_error(&out),
        "an explicit node was overridden rather than used, so the command \
         got two of them: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "the command did not reach the node it was given"
    );
}

#[test]
fn a_command_that_takes_no_node_is_left_alone() {
    // `key` takes a path first. Inserting a URL there would corrupt an
    // argument list that was already correct, which is the way a
    // convenience like this does real damage.
    let deep = tree("nonode", Some(DEAD));
    let key = deep.join("k.key");
    let out = run_in(&deep, &["key", key.to_str().expect("utf-8 path")]);
    assert!(
        out.status.success(),
        "a command with no <api> was rewritten: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("127.0.0.1"),
        "a node URL reached the output of a command that takes none"
    );
}

#[test]
fn a_command_prints_its_own_help_without_satisfying_its_arguments() {
    // The reader asking what a command takes is precisely the reader who
    // cannot yet supply it, so this must not go through argument
    // parsing.
    let deep = tree("help", None);
    let out = run_in(&deep, &["checkpoint", "--help"]);
    assert!(out.status.success(), "--help is not an error");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("<change-id>"),
        "the command's own arguments are missing from its help: {text}"
    );
    assert!(
        text.contains("<api> may be omitted"),
        "help for a command taking a node does not mention the default: {text}"
    );
}

#[test]
fn the_index_lists_every_command_under_a_heading() {
    let deep = tree("index", None);
    let out = run_in(&deep, &["--help"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    for group in choir_cli::surface::GROUPS {
        assert!(text.contains(group), "help omits the {group} section");
    }
    for c in choir_cli::surface::COMMANDS {
        assert!(
            text.contains(c.name),
            "help omits `{}`, so it cannot be found by reading",
            c.name
        );
    }
}
