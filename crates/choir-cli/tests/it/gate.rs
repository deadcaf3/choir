//! The release gate must return the status of every stage it claims to gate.

fn gate() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../gate")
}

fn run(injected: Option<&str>) -> std::process::Output {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let scratch = std::env::temp_dir().join(format!(
        "choir-gate-injection-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&scratch).expect("gate test scratch directory");
    let mut command = std::process::Command::new("sh");
    command
        .arg(gate())
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
    let clean = run(None);
    assert!(
        clean.status.success(),
        "the synthetic all-green gate must pass: {}{}",
        String::from_utf8_lossy(&clean.stdout),
        String::from_utf8_lossy(&clean.stderr)
    );

    for stage in [
        "format",
        "tests",
        "clippy",
        "rustdoc",
        "spike",
        "freshness",
        "scan",
    ] {
        let failed = run(Some(stage));
        assert!(
            !failed.status.success(),
            "an injected {stage} failure was masked: {}{}",
            String::from_utf8_lossy(&failed.stdout),
            String::from_utf8_lossy(&failed.stderr)
        );
    }
}
