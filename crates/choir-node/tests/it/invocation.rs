//! What the daemon says when the invocation is wrong.
//!
//! D58 settled this for `choir-cli` and never reached this binary, so
//! every mistyped flag here came out as
//! `Error: Custom { kind: InvalidInput, error: "..." }` — the message
//! intact and wrapped in the name of the type carrying it, because
//! `fn main() -> io::Result<()>` prints a returned error with `Debug`.
//!
//! These drive the built binary rather than the library, because the
//! rendering under test is the runtime's and not any function's: calling
//! `run()` in-process would return the same `io::Error` whichever way
//! `main` is written, and the test would pass with nothing fixed.

use std::process::Command;

/// `(exit code, stderr)` for one invocation of the daemon.
fn refuse(args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_choir-node"))
        .args(args)
        .output()
        .expect("choir-node runs");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
    )
}

/// The refusal is a sentence. Nothing of the type that carried it
/// reaches the reader, and the status is unchanged at 1 — the backup and
/// restore scripts branch on nonzero rather than on a value, so moving
/// usage to 2 would be a behaviour change nobody asked for.
#[test]
fn a_refusal_is_a_sentence_and_not_a_debug_struct() {
    for args in [
        &["--verify-export", "/choir-nonexistent-export"][..],
        &["/choir-nonexistent-root", "0", "--auth-file"][..],
    ] {
        let (code, stderr) = refuse(args);
        assert_eq!(code, 1, "{args:?} exited {code}: {stderr}");
        for leak in ["Custom {", "kind:", "InvalidInput", "InvalidData", "Os {"] {
            assert!(
                !stderr.contains(leak),
                "{args:?} showed the reader `{leak}`: {stderr}"
            );
        }
        assert!(
            stderr.starts_with("choir-node: "),
            "{args:?} did not name the tool: {stderr}"
        );
    }
}

/// A usage line is meant to be copied, so it opens with the binary's own
/// name — and must therefore not be prefixed with that name a second
/// time.
#[test]
fn a_usage_line_names_the_binary_once() {
    let (code, stderr) = refuse(&["--verify-log", "one", "two", "three"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.starts_with("usage: choir-node --verify-log"),
        "a usage refusal must lead with the line to copy: {stderr}"
    );
    assert_eq!(
        stderr.matches("choir-node").count(),
        1,
        "the tool is named twice in one line: {stderr}"
    );
}
