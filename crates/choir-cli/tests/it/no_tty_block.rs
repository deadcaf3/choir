//! No command in this binary may block waiting for a terminal.
//!
//! Borrowed from a competitor's `feedback` command, which documents that
//! it "exits rather than blocking when there's no terminal — so an agent
//! can file one mid-task without stalling". The discipline is worth more
//! than the command was: an agent driving this CLI has no terminal, and
//! a single prompt anywhere in the surface hangs it with no error, no
//! timeout, and nothing in a log to explain it.
//!
//! choir already satisfies this, by accident rather than by decision.
//! That is precisely what makes it worth asserting: an accident holds
//! until the first person who adds a confirmation prompt to a
//! destructive verb, and nothing today would notice.
//!
//! Two assertions, because either alone is weak. The static one catches
//! the *shape* — an `$EDITOR` launch or a terminal probe appearing
//! anywhere in the crate — including in commands this harness cannot
//! reach without a live node. The behavioural one proves the claim for
//! real on the commands that need no network, since a source scan only
//! ever checks the patterns somebody thought of.

use std::io::Write;

fn crate_sources() -> Vec<(String, String)> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("the crate has a src directory") {
            let path = entry.expect("readable directory entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&path).expect("source file is utf-8");
                out.push((path.display().to_string(), text));
            }
        }
    }
    assert!(out.len() > 3, "found almost no sources; the walk is wrong");
    out
}

/// Nothing in the crate opens an editor or asks whether it has a
/// terminal.
///
/// `is_terminal` was on this list, banned outright, on the reasoning
/// that a command which asks has a branch for the answer and the branch
/// taken on "yes" is the one that prompts.
///
/// It is now permitted in exactly one file. `style.rs` asks so that it
/// can decide whether to emit colour, which is the one question about a
/// terminal that has no prompting branch behind it: both answers print
/// the same words and exit the same way, and the only difference is
/// whether four escape sequences surround them. Data written to stdout
/// is never styled at all, so a pipeline reads identical bytes either
/// way.
///
/// The exemption is by file rather than by pattern because a pattern
/// exemption would let the next `is_terminal` in, wherever it appeared.
/// A second file asking the question fails this test, and should: that
/// is where a prompt would go.
#[test]
fn no_source_reaches_for_a_terminal() {
    let banned = [
        "is_terminal",
        "IsTerminal",
        "EDITOR",
        "VISUAL",
        "read_line",
        "rpassword",
        "dialoguer",
    ];
    // Colour, and nothing else. Named here so that widening it is an
    // edit to this list rather than to the rule.
    let terminal_probes_allowed_in = "style.rs";
    let mut findings = Vec::new();
    for (path, text) in crate_sources() {
        let colour_module = path.ends_with(terminal_probes_allowed_in);
        for (number, line) in text.lines().enumerate() {
            // The rule is about code. This test names every pattern it
            // bans, and so does the module doc above it.
            let code = line.trim_start();
            if code.starts_with("//") || code.starts_with("///") || code.starts_with("//!") {
                continue;
            }
            for needle in banned {
                // Only the terminal probe is exempted, and only there.
                // A prompt primitive in the colour module is still a
                // finding -- that is the failure the exemption must not
                // create a hole for.
                let exempt = colour_module && matches!(needle, "is_terminal" | "IsTerminal");
                if line.contains(needle) && !exempt {
                    findings.push(format!("{path}:{}: {needle}", number + 1));
                }
            }
        }
    }
    assert!(
        findings.is_empty(),
        "a CLI path now depends on having a terminal:\n  {}\n\n\
         If this is deliberate, the command must still complete without a\n\
         TTY, and this test should be updated to say why rather than\n\
         deleted.",
        findings.join("\n  ")
    );
}

/// The two commands that touch no network complete with stdin closed.
///
/// `key` writes a file and `skill install` writes a directory; both are
/// plausible places for a "file exists, overwrite?" prompt to be added
/// later. Run with an empty stdin: a command that tried to read would
/// see EOF, and one that opened `/dev/tty` would block — which is what
/// the wait below would catch.
#[test]
fn the_offline_commands_finish_with_stdin_closed() {
    let work = std::env::temp_dir().join("choir-cli-no-tty-block");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).expect("temp dir");

    let key_path = work.join("agent.key");
    let cases: Vec<Vec<String>> = vec![
        vec!["key".into(), key_path.display().to_string()],
        // Twice: the second call finds the file already there, which is
        // the state a prompt would most likely be added for.
        vec!["key".into(), key_path.display().to_string()],
        vec![
            "skill".into(),
            "install".into(),
            "--into".into(),
            work.join("skills").display().to_string(),
        ],
    ];

    for args in cases {
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_choir"))
            .args(&args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("the choir binary runs");
        // Close stdin immediately, which is the agent's situation.
        drop(child.stdin.take().expect("piped stdin"));

        // Poll rather than `wait()`: a blocked child would hang the test
        // binary itself, and a hung suite is far harder to read than a
        // named failure. The bound is generous because these commands
        // generate a key and write files, and this harness runs on
        // parallel threads with everything else.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let status = loop {
            match child.try_wait().expect("child is waitable") {
                Some(status) => break Some(status),
                None if std::time::Instant::now() >= deadline => {
                    let _ = child.kill();
                    break None;
                }
                None => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        };
        let Some(status) = status else {
            // Written to stderr as well so the reason survives a run
            // that only captured output.
            let _ = writeln!(std::io::stderr(), "hung: choir {args:?}");
            panic!("`choir {args:?}` did not finish with stdin closed; it is waiting on input");
        };
        assert!(
            status.success(),
            "`choir {args:?}` exited {status:?} with stdin closed"
        );
    }

    let _ = std::fs::remove_dir_all(&work);
}
