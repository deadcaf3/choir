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

/// A cached verdict is only worth as much as the key it hangs on, and
/// the way a key goes wrong is by missing an input: it then serves a
/// green for a tree that never produced one. Nothing above would notice
/// that, because nothing above changes a file.
///
/// So this one does, in a throwaway worktree of HEAD — a test that
/// edited the checkout it runs in would race every other test here and
/// leave the tree dirty on a panic. Three edits, each asserting a
/// different half of the key:
///
/// - a leaf crate nobody depends on invalidates itself and leaves the
///   node's stage cached, which is what says the closure is a closure
///   and not just "everything";
/// - the crate everything depends on invalidates all of it;
/// - `.cargo/config.toml` does too, though it belongs to no crate — it
///   carries the flags every build in this workspace needs.
///
/// All three edits are unstaged, so they also assert the key reads the
/// working tree rather than the index.
#[test]
fn the_cache_key_follows_the_files_that_produced_it() {
    let scratch = scratch_dir();
    let tree = scratch.join("worktree");
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .args(args)
            .output()
            .expect("git")
    };
    let added = git(&[
        "worktree",
        "add",
        "--detach",
        tree.to_str().expect("worktree path"),
        "HEAD",
    ]);
    assert!(
        added.status.success(),
        "could not make a worktree to mutate: {}",
        String::from_utf8_lossy(&added.stderr)
    );
    // `worktree add` checks out HEAD, which is the one gate this test
    // must not run: it would exercise the last commit's script while the
    // edit under test sits in the checkout. Every other test here runs
    // the gate as it is on disk, and so does this one.
    std::fs::copy(gate(), tree.join("gate")).expect("the gate under test, not HEAD's");

    let run_tree = |lane: Option<&str>| {
        let mut command = std::process::Command::new("sh");
        if let Some(lane) = lane {
            command.arg(tree.join("gate")).arg(lane);
        } else {
            command.arg(tree.join("gate"));
        }
        command
            .env("CHOIR_GATE_TEST_MODE", "1")
            .env("CHOIR_GATE_TEST_CACHE", "1")
            .env("TMPDIR", &scratch)
            .output()
            .expect("run the worktree's gate")
    };
    // The probe must leave the file valid for whatever reads it. A `//`
    // appended to `.cargo/config.toml` is not TOML, `cargo tree` then
    // fails, and the cache turns itself off -- which passes every
    // assertion below for the opposite of the reason it claims.
    let append = |path: &str| {
        let file = tree.join(path);
        let mut body = std::fs::read_to_string(&file).expect("read a file to change");
        let comment = if path.ends_with(".toml") { "#" } else { "//" };
        body.push_str(&format!("\n{comment} cache key probe\n"));
        std::fs::write(&file, body).expect("change a file");
    };

    assert!(run_tree(None).status.success(), "populate the cache");
    let warm = cached_stages(&run_tree(None));
    assert!(
        warm.iter().any(|s| s == "node"),
        "the second run served nothing, so nothing below proves anything: {warm:?}"
    );

    // A leaf nobody depends on. choir-demo is in the workspace stage and
    // in nobody's dependency graph, so its stage misses and the node's
    // does not.
    append("crates/choir-demo/src/main.rs");
    let leaf = cached_stages(&run_tree(None));
    assert!(
        !leaf.iter().any(|s| s == "workspace"),
        "an edited crate was still served from the cache: {leaf:?}"
    );
    assert!(
        leaf.iter().any(|s| s == "node"),
        "a leaf edit invalidated a crate that does not depend on it: {leaf:?}"
    );

    // The crate everything depends on.
    append("crates/choir-hash/src/lib.rs");
    let root = cached_stages(&run_tree(None));
    assert!(
        root.is_empty(),
        "editing the crate everything depends on left verdicts standing: {root:?}"
    );

    // A build input that belongs to no crate at all.
    assert!(run_tree(None).status.success(), "repopulate the cache");
    assert!(
        !cached_stages(&run_tree(None)).is_empty(),
        "cache is warm again"
    );
    append(".cargo/config.toml");
    let config = cached_stages(&run_tree(None));
    assert!(
        config.is_empty(),
        "a change to .cargo/config.toml left verdicts standing: {config:?}"
    );

    git(&[
        "worktree",
        "remove",
        "--force",
        tree.to_str().expect("worktree path"),
    ]);
    std::fs::remove_dir_all(scratch).ok();
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
