//! What a refusal says, and who it says it to.
//!
//! Every wrong invocation used to print the same forty-line index: the
//! reader who mistyped a name, the reader who got a known command's
//! arguments wrong, and the reader who asked what the tool can do all
//! got the answer to the third question. Above a prompt that means the
//! one line that mattered scrolled off the top.
//!
//! What is asserted here is mostly *absence* -- that the index is not
//! printed when it was not the question. An assertion that the right
//! text is present would pass just as well with the wall of text still
//! underneath it.

fn choir() -> &'static str {
    env!("CARGO_BIN_EXE_choir")
}

fn run(args: &[&str]) -> (i32, String, String) {
    let out = std::process::Command::new(choir())
        .args(args)
        .output()
        .expect("choir runs");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A phrase that appears in the full index and nowhere else, so its
/// absence is proof the index was not printed.
const INDEX_MARKER: &str = "reading the node";

#[test]
fn an_unknown_command_is_named_without_the_whole_index() {
    let (code, out, err) = run(&["frobnicate"]);
    assert_eq!(code, 2, "an unknown command is a usage error");
    assert!(out.is_empty(), "a refusal wrote to stdout: {out}");
    assert!(
        err.contains("frobnicate"),
        "the refusal does not name what was typed: {err}"
    );
    assert!(
        !err.contains(INDEX_MARKER),
        "the whole index was printed at a reader who mistyped one word: {err}"
    );
    assert!(
        err.contains("choir --help"),
        "the refusal does not say where the index is: {err}"
    );
}

#[test]
fn a_near_miss_is_offered_the_command_it_missed() {
    let (_, _, err) = run(&["revieww"]);
    assert!(
        err.contains("did you mean") && err.contains("review"),
        "a one-character typo got no suggestion: {err}"
    );
}

#[test]
fn a_word_far_from_every_command_is_offered_nothing() {
    // A wrong suggestion is worse than none: the reader types it, is
    // refused a second time, and stops trusting the first refusal.
    let (_, _, err) = run(&["deploy"]);
    assert!(
        !err.contains("did you mean"),
        "a word resembling no command was given a suggestion anyway: {err}"
    );
}

#[test]
fn half_of_a_two_word_command_is_offered_the_other_half() {
    // `acl render` is seven insertions away from `acl` by edit distance,
    // so nothing distance-based would ever suggest it -- and it is the
    // only thing the reader can have meant.
    let (_, _, err) = run(&["acl"]);
    assert!(
        err.contains("acl render"),
        "the second word of the only two-word command was not offered: {err}"
    );
}

#[test]
fn a_known_command_given_wrong_arguments_gets_its_own_spec() {
    let (code, _, err) = run(&["review"]);
    assert_eq!(code, 2);
    assert!(
        err.contains("<git-oid>"),
        "the command's own argument spec is missing: {err}"
    );
    assert!(
        !err.contains(INDEX_MARKER),
        "the index was printed instead of the one command in question: {err}"
    );
}

#[test]
fn a_bare_invocation_still_gets_the_index() {
    // The index is not wrong, it is an answer to one question. This is
    // that question, asked on a machine that runs a node: one with nothing
    // set up gets the orientation instead, and a fresh CI runner is that
    // machine.
    let home = std::env::temp_dir().join("choir-ux-operator-home");
    std::fs::create_dir_all(home.join(".choir/repos")).expect("operator home");
    let out = std::process::Command::new(choir())
        .env("HOME", &home)
        .output()
        .expect("choir runs");
    let (code, err) = (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    );
    assert_eq!(code, 2);
    assert!(
        err.contains(INDEX_MARKER),
        "a bare invocation no longer lists what the tool can do: {err}"
    );
}

#[test]
fn nothing_is_painted_when_the_output_is_not_a_terminal() {
    // `Command::output` gives the child pipes, which is exactly the
    // condition under which colour must be off. A CLI that paints into a
    // pipe corrupts every log file and every `grep` that reads it.
    for args in [vec!["frobnicate"], vec!["review"], vec![], vec!["--help"]] {
        let (_, out, err) = run(&args);
        assert!(
            !out.contains('\x1b') && !err.contains('\x1b'),
            "an escape sequence reached a pipe from `choir {}`",
            args.join(" ")
        );
    }
}

#[test]
fn the_binary_reports_the_commit_it_was_built_from() {
    // Asked of the binary rather than of a checkout: "I rebuilt it" and
    // "the rebuild is what is running" are different claims, and only
    // this one can settle the second.
    let (code, out, _) = run(&["--version"]);
    assert_eq!(code, 0, "--version is not an error");
    assert!(
        out.starts_with("choir build "),
        "--version does not name a build: {out}"
    );
    let stamp = out
        .split_whitespace()
        .nth(2)
        .expect("a third word to the version line");
    assert!(
        stamp == "unknown" || stamp.chars().all(|c| c.is_ascii_hexdigit()),
        "the stamp is neither a commit nor an honest `unknown`: {stamp}"
    );
}

#[test]
fn the_short_help_flag_is_the_long_one() {
    let (short_code, short, _) = run(&["-h"]);
    let (long_code, long, _) = run(&["--help"]);
    assert_eq!(short_code, 0);
    assert_eq!(long_code, 0);
    assert_eq!(short, long, "-h and --help disagree");
}

#[test]
fn the_short_help_flag_stops_being_help_where_a_value_stands() {
    // The first version of this rewrote every `-h` in the argument list
    // to `--help` before parsing, which is a convenience that corrupts
    // invocations that were already correct. `choir key <file> <name>`
    // prints the name back beside the key, so an actor legitimately
    // called `-h` shows exactly what the argument list contained -- and
    // under the rewriting version it printed `--help`.
    let dir = std::env::temp_dir().join("choir-cli-ux-dashh");
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let file = dir.join("actor.ed25519");
    let (code, out, err) = run(&["key", file.to_str().expect("utf-8 path"), "-h"]);
    assert_eq!(code, 0, "the command did not run: {err}");
    assert!(
        out.starts_with("-h "),
        "a value in argument position was read as a help flag: {out}"
    );
}

#[test]
fn the_mcp_adapter_answers_the_help_flag_rather_than_parsing_it() {
    // It used to read `--help` as the node address and complain about
    // its URL scheme, which is the least useful true statement
    // available.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_choir-mcp"))
        .arg("--help")
        .output()
        .expect("choir-mcp runs");
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("usage: choir-mcp"),
        "--help did not print usage: {text}"
    );
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("http://"),
        "--help was still parsed as an address"
    );
}

#[test]
fn a_human_summary_never_reaches_the_document_on_stdout() {
    // `skill install` answers with a JSON document that agents and the
    // tests read. The summary a person sees beside it is decoration, and
    // decoration that leaked into this stream would break every reader
    // of it -- so stdout must be the document and nothing else, with no
    // heading, no padding, and no escape sequence.
    let into = std::env::temp_dir().join("choir-cli-ux-skill");
    let _ = std::fs::remove_dir_all(&into);
    let (code, out, _) = run(&[
        "skill",
        "install",
        "--into",
        into.to_str().expect("utf-8 path"),
    ]);
    assert_eq!(code, 0);
    let document: serde_json::Value =
        serde_json::from_str(out.trim()).expect("stdout is one JSON document");
    assert_eq!(document["wrote"], serde_json::Value::Bool(true));
    assert_eq!(
        out.trim_end(),
        document.to_string(),
        "something other than the document reached stdout"
    );
    let _ = std::fs::remove_dir_all(&into);
}
