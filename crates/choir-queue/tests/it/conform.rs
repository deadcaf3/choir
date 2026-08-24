//! The conformance suite as a binary, pointed at a helper argv.
//!
//! `executor.rs` gates the backends cargo links. This gates the tool
//! that gates the one it cannot: a microVM driver needs KVM, so it is
//! built elsewhere, and `choir-ci-conform` is the only thing that can
//! hold it to the same list. A harness that reported "ok" for a helper
//! answering nonsense would be worse than none, so the cases here are
//! mostly helpers that are wrong on purpose.

use std::process::Command;

const CONFORM: &str = env!("CARGO_BIN_EXE_choir-ci-conform");

/// One run of the tool: stdout, and the exit code.
fn run(args: &[&str]) -> (String, i32) {
    let out = Command::new(CONFORM)
        .args(args)
        .output()
        .expect("choir-ci-conform runs");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        out.status.code().expect("it exits rather than signals"),
    )
}

/// A helper spelled as a shell script, for the answers a well-behaved
/// one never gives.
fn canned(script: &str) -> Vec<String> {
    vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()]
}

fn probe_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("choir-conform-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the probe directory");
    dir
}

#[test]
fn the_reference_helper_passes_the_external_suite() {
    let dir = probe_dir("reference");
    let (out, code) = run(&[
        "--probe-dir",
        dir.to_str().expect("a UTF-8 temp path"),
        "--",
        env!("CARGO_BIN_EXE_choir-ci-local"),
    ]);
    assert_eq!(code, 0, "the reference helper did not conform:\n{out}");
    assert!(
        out.contains("8 passed, 0 failed, 0 skipped"),
        "every check must have run:\n{out}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The whole point of the tool. A helper that answers `passed` to
/// everything is well-formed, index-aligned, and wrong about four
/// separate things -- which is what a driver looks like before its
/// verdict mapping is finished.
#[test]
fn a_helper_that_only_ever_passes_is_caught() {
    let mut args = vec!["--".to_string()];
    args.extend(canned(
        r#"printf '{"name":"yes","protocol":2}\n'; while :; do printf '{"verdict":"passed"}\n'; done"#,
    ));
    let (out, code) = run(&args.iter().map(String::as_str).collect::<Vec<_>>());

    assert_eq!(code, 1, "a helper that only passes was not caught:\n{out}");
    for check in ["failed", "errored", "timed-out", "alignment"] {
        assert!(
            out.lines()
                .any(|l| l.starts_with("FAILED") && l.contains(check)),
            "`{check}` should have failed:\n{out}"
        );
    }
    assert!(
        out.lines()
            .any(|l| l.starts_with("ok") && l.contains("passed")),
        "the one thing it does right must still read as ok:\n{out}"
    );
}

/// A handshake we cannot complete makes every later check meaningless,
/// so they are skipped by name rather than reported as seven failures
/// about a helper that was never asked anything.
#[test]
fn a_version_mismatch_skips_the_rest_instead_of_deriving_failures() {
    let mut args = vec!["--".to_string()];
    args.extend(canned(r#"printf '{"name":"future","protocol":99}\n'"#));
    let (out, code) = run(&args.iter().map(String::as_str).collect::<Vec<_>>());

    assert_eq!(code, 1, "a version mismatch must fail the run:\n{out}");
    assert!(
        out.contains("FAILED   handshake") && out.contains("99"),
        "the failure must name the version:\n{out}"
    );
    assert!(
        out.contains("0 passed, 1 failed, 7 skipped"),
        "the rest must be skipped, not failed:\n{out}"
    );
}

/// A skipped check is not a passed one. Without a directory the far
/// side can see, the `Job::directory` rule is unestablished, and the
/// report says so instead of counting it.
#[test]
fn without_a_probe_directory_the_directory_check_is_skipped_not_passed() {
    let (out, code) = run(&["--", env!("CARGO_BIN_EXE_choir-ci-local")]);
    assert_eq!(code, 0, "skipping must not fail the run:\n{out}");
    assert!(
        out.lines()
            .any(|l| l.starts_with("skipped") && l.contains("directory")),
        "the directory check must read as skipped:\n{out}"
    );
    assert!(
        out.contains("7 passed, 0 failed, 1 skipped"),
        "the summary must count the skip:\n{out}"
    );
}

/// A usage error is neither a pass nor a failed check, and exits on its
/// own code: a CI lane that treats 1 as "the helper is wrong" must not
/// be told that by a typo in its own invocation.
#[test]
fn a_missing_helper_is_a_usage_error() {
    let (_, code) = run(&["--shell", "/bin/sh"]);
    assert_eq!(code, 2, "a missing helper must exit 2");
    let (_, code) = run(&[
        "--probe-dir",
        "/nonexistent/choir-conform",
        "--",
        "/bin/true",
    ]);
    assert_eq!(code, 2, "an unusable probe directory must exit 2");
}
