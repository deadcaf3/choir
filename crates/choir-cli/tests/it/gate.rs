//! The release gate must return the status of every stage it claims to
//! gate — in every lane, and only the stages that lane actually runs.

fn gate() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../gate")
}

fn run(lane: Option<&str>, injected: Option<&str>) -> std::process::Output {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let scratch = std::env::temp_dir().join(format!(
        "choir-gate-injection-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&scratch).expect("gate test scratch directory");
    let mut command = std::process::Command::new("sh");
    command.arg(gate());
    if let Some(lane) = lane {
        command.arg(lane);
    }
    command
        .env("CHOIR_GATE_TEST_MODE", "1")
        .env("TMPDIR", &scratch)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(stage) = injected {
        command.env("CHOIR_GATE_INJECT_FAILURE", stage);
    }
    let output = command.output().expect("run gate in injection mode");
    std::fs::remove_dir_all(scratch).ok();
    output
}

#[test]
fn gate_propagates_every_injected_stage_failure() {
    let clean = run(None, None);
    assert!(
        clean.status.success(),
        "the synthetic all-green gate must pass: {}{}",
        String::from_utf8_lossy(&clean.stdout),
        String::from_utf8_lossy(&clean.stderr)
    );

    for stage in [
        "tracked",
        "format",
        "tests",
        "clippy",
        "rustdoc",
        "book",
        "spike",
        "freshness",
        "audit",
        "scan",
    ] {
        let failed = run(None, Some(stage));
        assert!(
            !failed.status.success(),
            "an injected {stage} failure was masked: {}{}",
            String::from_utf8_lossy(&failed.stdout),
            String::from_utf8_lossy(&failed.stderr)
        );
    }
}

#[test]
fn quick_lane_gates_its_stages_and_admits_it_ran_nothing_else() {
    let clean = run(Some("quick"), None);
    assert!(
        clean.status.success(),
        "the synthetic all-green quick lane must pass: {}{}",
        String::from_utf8_lossy(&clean.stdout),
        String::from_utf8_lossy(&clean.stderr)
    );
    // A green quick lane must say what it is, not read like a green suite.
    let stdout = String::from_utf8_lossy(&clean.stdout);
    assert!(
        stdout.contains("no tests ran"),
        "a green quick lane must say no tests ran: {stdout}"
    );

    for stage in ["tracked", "format", "book", "freshness", "audit", "scan"] {
        let failed = run(Some("quick"), Some(stage));
        assert!(
            !failed.status.success(),
            "an injected {stage} failure was masked in the quick lane: {}{}",
            String::from_utf8_lossy(&failed.stdout),
            String::from_utf8_lossy(&failed.stderr)
        );
    }
    // The stages the quick lane skips must not be able to fail it — a
    // failure leaking in from a stage it never ran would mean it ran it.
    for stage in ["tests", "clippy", "rustdoc", "spike"] {
        assert!(
            run(Some("quick"), Some(stage)).status.success(),
            "the quick lane ran {stage}, which it promises never to run"
        );
    }
}

#[test]
fn fast_lane_skips_the_network_and_the_scan() {
    assert!(
        run(Some("fast"), None).status.success(),
        "the synthetic all-green fast lane must pass"
    );
    for stage in ["audit", "scan"] {
        assert!(
            run(Some("fast"), Some(stage)).status.success(),
            "the fast lane ran {stage}, which it promises to skip"
        );
    }
}

/// The touched lane runs the edit loop's two stages and skips every
/// stage that makes it slow: no doctests, no rustdoc, no book, no
/// release measurements, no network, no object scan.
///
/// Asserted by injecting a failure into each skipped stage and
/// requiring the lane to stay green, which is the only way to tell
/// "skipped" from "ran and happened to pass".
#[test]
fn touched_lane_runs_the_edit_loop_and_nothing_else() {
    assert!(
        run(Some("touched"), None).status.success(),
        "the synthetic all-green touched lane must pass"
    );
    for stage in [
        "audit",
        "scan",
        "book",
        "rustdoc",
        "doctests",
        "measure",
        "retention",
    ] {
        assert!(
            run(Some("touched"), Some(stage)).status.success(),
            "the touched lane ran {stage}, which it promises to skip"
        );
    }
    // And it still gates the two it does run. `tracked` and `format`
    // come before any lane branch, so they are the floor every lane
    // stands on.
    for stage in ["tracked", "format"] {
        assert!(
            !run(Some("touched"), Some(stage)).status.success(),
            "the touched lane let a failing {stage} through"
        );
    }
}

#[test]
fn an_unknown_lane_refuses_to_run() {
    let output = run(Some("quikc"), None);
    assert!(
        !output.status.success(),
        "a misspelt lane must refuse, not run some other lane: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}
