//! `choir node serve`: the derived daemon invocation, and what it does
//! rather than derive.
//!
//! The argv is the contract. Every flag `plan` omits is a default the
//! daemon picks for itself, so a test that only checked "a process
//! started" would not notice one going missing — which is how a node
//! comes up with its platform API off and answers 503 to every read.

use choir_cli::serve::{plan, Invocation, Layout};
use std::path::{Path, PathBuf};

/// A state directory with the two files a node cannot start without.
fn ready(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("choir-cli-serve-{tag}"));
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(&root).expect("state dir");
    std::fs::write(root.join("auth"), "choir:token\n").expect("auth");
    std::fs::write(root.join("keys"), "op ab\n").expect("keys");
    root
}

fn built(layout: &Layout, create: &[String], extra: &[String]) -> Invocation {
    plan(PathBuf::from("choir-node"), layout, create, extra).expect("a ready layout plans")
}

#[test]
fn derives_root_port_and_both_files() {
    let state = ready("derives");
    let layout = Layout::new(&state, 8417);
    let invocation = built(&layout, &[], &[]);
    assert_eq!(invocation.program, Path::new("choir-node"));
    assert_eq!(
        invocation.args,
        vec![
            state.join("repos").display().to_string(),
            "8417".to_string(),
            "--auth-file".to_string(),
            state.join("auth").display().to_string(),
            "--keys-file".to_string(),
            state.join("keys").display().to_string(),
        ]
    );
}

/// Without `--keys-file` the platform API stays off and every `/api/*`
/// read answers 503 — a node that looks started and answers nothing.
#[test]
fn always_enables_the_platform_api() {
    let state = ready("keys");
    let invocation = built(&Layout::new(&state, 8417), &[], &[]);
    assert!(invocation.args.iter().any(|a| a == "--keys-file"));
    assert!(invocation.args.iter().any(|a| a == "--auth-file"));
}

#[test]
fn repositories_become_one_create_flag_each() {
    let state = ready("create");
    let repos = ["a/one.git".to_string(), "b/two.git".to_string()];
    let invocation = built(&Layout::new(&state, 8417), &repos, &[]);
    let creates: Vec<&String> = invocation
        .args
        .iter()
        .zip(invocation.args.iter().skip(1))
        .filter(|(flag, _)| *flag == "--create")
        .map(|(_, name)| name)
        .collect();
    assert_eq!(creates, vec!["a/one.git", "b/two.git"]);
}

/// A daemon flag this command has never heard of still has to reach the
/// daemon, or every one of them becomes a reason to stop using `serve`.
#[test]
fn passes_unknown_daemon_flags_through_last() {
    let state = ready("extra");
    let extra = ["--rate-limit-api".to_string(), "60".to_string()];
    let invocation = built(&Layout::new(&state, 8417), &[], &extra);
    let tail = &invocation.args[invocation.args.len() - 2..];
    assert_eq!(tail, ["--rate-limit-api", "60"]);
}

/// The refusal names the command that fixes it. A daemon started without
/// a credential is a node with no auth at all, which is worse than one
/// that did not start.
#[test]
fn refuses_a_state_directory_with_no_node_in_it() {
    let root = std::env::temp_dir().join("choir-cli-serve-empty");
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(&root).expect("empty state");
    let layout = Layout::new(&root, 8417);
    assert_eq!(layout.missing().len(), 2);
    let error = plan(PathBuf::from("choir-node"), &layout, &[], &[]).expect_err("refuses");
    assert!(error.contains("choir init"), "{error}");
    assert!(error.contains("auth"), "{error}");
}

/// Half a state directory is still not a node, and the message says
/// which half.
#[test]
fn names_only_what_is_actually_missing() {
    let root = std::env::temp_dir().join("choir-cli-serve-half");
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(&root).expect("half state");
    std::fs::write(root.join("auth"), "choir:token\n").expect("auth");
    let layout = Layout::new(&root, 8417);
    let missing = layout.missing();
    assert_eq!(missing.len(), 1);
    assert!(missing[0].ends_with("keys"));
}

/// The port is read back out of the configured node URL so `serve` and
/// every client command agree without it being written down twice.
#[test]
fn reads_the_port_back_out_of_a_node_url() {
    use choir_cli::serve::port_of;
    assert_eq!(port_of("http://127.0.0.1:8417"), Some(8417));
    assert_eq!(port_of("http://127.0.0.1:8417/"), Some(8417));
    assert_eq!(port_of("https://example.test:443/api"), Some(443));
    // No port named is not port zero.
    assert_eq!(port_of("http://127.0.0.1"), None);
    assert_eq!(port_of("nonsense"), None);
}

#[test]
fn displays_the_command_it_will_run() {
    let state = ready("display");
    let invocation = built(&Layout::new(&state, 8417), &[], &[]);
    let line = invocation.display();
    assert!(line.starts_with("choir-node "), "{line}");
    assert!(line.contains(" 8417 "), "{line}");
}
