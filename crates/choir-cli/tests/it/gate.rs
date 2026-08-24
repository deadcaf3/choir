//! The release gate must return the status of every stage it claims to
//! gate — in every lane, and only the stages that lane actually runs.

fn gate() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../gate")
}

fn scratch_dir() -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let scratch = std::env::temp_dir().join(format!(
        "choir-gate-injection-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&scratch).expect("gate test scratch directory");
    scratch
}

/// One gate run against a caller-owned scratch directory. The gate keys
/// its log and its verdict cache off `TMPDIR`, so two runs sharing a
/// scratch share a cache and two runs with their own see none of each
/// other's — which is what tells a cache test from every other test here.
fn run_in(
    scratch: &std::path::Path,
    lane: Option<&str>,
    injected: Option<&str>,
    cache: bool,
) -> std::process::Output {
    let mut command = std::process::Command::new("sh");
    command.arg(gate());
    if let Some(lane) = lane {
        command.arg(lane);
    }
    command
        .env("CHOIR_GATE_TEST_MODE", "1")
        .env("TMPDIR", scratch)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if cache {
        command.env("CHOIR_GATE_TEST_CACHE", "1");
    }
    if let Some(stage) = injected {
        command.env("CHOIR_GATE_INJECT_FAILURE", stage);
    }
    command.output().expect("run gate in injection mode")
}

fn run(lane: Option<&str>, injected: Option<&str>) -> std::process::Output {
    let scratch = scratch_dir();
    let output = run_in(&scratch, lane, injected, false);
    std::fs::remove_dir_all(scratch).ok();
    output
}

/// Which stages a run reported as served from the verdict cache.
fn cached_stages(output: &std::process::Output) -> Vec<String> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.contains("cached"))
        .filter_map(|line| line.split_whitespace().nth(1).map(str::to_string))
        .collect()
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

/// The verdict cache must never turn a green run into a claim about a
/// tree that did not produce it. Three properties, and each one is a
/// way the cache could go wrong quietly:
///
/// 1. A green run's verdicts are reused, so the second run skips them.
/// 2. A red stage is never cached, so it re-proves itself every run.
/// 3. A voided freshness receipt deletes the verdicts that run minted,
///    because those stages ran against a tree no key names.
///
/// Test mode runs no cargo, so what is exercised here is which stages a
/// run decides to skip, which is the whole of the cache's behaviour that
/// can be wrong. A shared scratch directory is what makes two runs share
/// one cache.
#[test]
fn the_verdict_cache_reuses_green_and_never_red() {
    let scratch = scratch_dir();

    let first = run_in(&scratch, None, None, true);
    assert!(
        first.status.success(),
        "the synthetic all-green gate must pass with the cache on: {}{}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        cached_stages(&first).is_empty(),
        "the first run against an empty cache can serve nothing from it: {:?}",
        cached_stages(&first)
    );

    let second = run_in(&scratch, None, None, true);
    assert!(
        second.status.success(),
        "a cached green run must stay green"
    );
    let reused = cached_stages(&second);
    for stage in ["build", "workspace", "node", "clippy", "rustdoc"] {
        assert!(
            reused.iter().any(|s| s == stage),
            "the second run re-ran {stage}, which the first already proved: {reused:?}"
        );
    }

    // Red is never cached. Inject a clippy failure into a fresh cache,
    // then run green: clippy must run again rather than be served.
    let red_scratch = scratch_dir();
    let red = run_in(&red_scratch, None, Some("clippy"), true);
    assert!(
        !red.status.success(),
        "an injected clippy failure was masked"
    );
    let after_red = run_in(&red_scratch, None, None, true);
    assert!(
        !cached_stages(&after_red).iter().any(|s| s == "clippy"),
        "a failed clippy was served from the cache: {:?}",
        cached_stages(&after_red)
    );

    // A voided receipt voids what that run minted. The freshness stage
    // runs last and fails the run, and every marker it wrote goes with it.
    let stale_scratch = scratch_dir();
    let voided = run_in(&stale_scratch, None, Some("freshness"), true);
    assert!(
        !voided.status.success(),
        "an injected freshness failure was masked"
    );
    let after_void = run_in(&stale_scratch, None, None, true);
    assert!(
        cached_stages(&after_void).is_empty(),
        "a voided receipt left verdicts behind: {:?}",
        cached_stages(&after_void)
    );

    for dir in [scratch, red_scratch, stale_scratch] {
        std::fs::remove_dir_all(dir).ok();
    }
}

/// The cache is opt-in twice over: off in test mode unless asked for,
/// and off entirely under `CHOIR_GATE_NO_CACHE`. The second is the
/// escape hatch a person reaches for when they suspect it, so it has to
/// work even when every key would have hit.
#[test]
fn the_cache_can_be_turned_off() {
    let scratch = scratch_dir();
    assert!(run_in(&scratch, None, None, true).status.success());

    let mut command = std::process::Command::new("sh");
    let output = command
        .arg(gate())
        .env("CHOIR_GATE_TEST_MODE", "1")
        .env("CHOIR_GATE_TEST_CACHE", "1")
        .env("CHOIR_GATE_NO_CACHE", "1")
        .env("TMPDIR", &scratch)
        .output()
        .expect("run gate with the cache disabled");
    assert!(
        output.status.success(),
        "the disabled-cache run must be green"
    );
    assert!(
        cached_stages(&output).is_empty(),
        "CHOIR_GATE_NO_CACHE still served verdicts from the cache: {:?}",
        cached_stages(&output)
    );
    std::fs::remove_dir_all(scratch).ok();
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
