//! The attributed request log and the per-user rate limiter (D33), driven
//! against a real served node with real `curl` and `git` subprocesses.
//!
//! # Why this is its own test binary
//!
//! Both halves are timing-shaped. A token bucket refills against the wall
//! clock, and a log line is written *after* the response reaches the
//! client, so every assertion here either waits for a file to catch up or
//! depends on several requests falling inside one refill window. The
//! merged `tests/it/` harness shares a process and runs its modules on
//! parallel threads, which forbids exactly that — a bucket sized for four
//! quick requests is not sized for four requests interleaved with thirty
//! other modules' work. So this file stays its own binary, the way
//! `throughput.rs` and `budget.rs` do, and pays one relink for it.
//!
//! Run:
//!
//! ```text
//! cargo test -p choir-node --test limits
//! ```
//!
//! The bucket arithmetic itself is unit-tested against an injected clock
//! in `src/limits.rs`; what only a served node can show is here — that the
//! username on the line is the authenticated one, that a query string
//! never reaches the file, that the `429` carries `Retry-After`, and that
//! the three exemptions really exempt.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use choir_identity::{ActorKey, Registry};
use choir_node::{AuthTable, Node, Platform};
use choir_oplog::MemLog;

/// A served node plus the paths a test needs to inspect.
struct Served {
    base: String,
    work: PathBuf,
    log: PathBuf,
}

/// How the node under test is configured. Every field is something one of
/// the tests below varies, and nothing else.
#[derive(Default)]
struct Config<'a> {
    /// Requests per minute for [`choir_node::limits::Class::Api`].
    api: Option<u32>,
    /// Requests per minute for [`choir_node::limits::Class::Git`].
    git: Option<u32>,
    /// ACL file contents, when the test needs a `@node` grant.
    acl: Option<&'a str>,
    /// Rotation bound; `0` means "use the shipped default".
    max_bytes: u64,
    /// Bare repositories to create.
    repos: &'a [&'a str],
    /// Bind without an `AuthTable` at all, to exercise the third
    /// exemption.
    no_auth: bool,
    /// Absolute API body ceiling; `None` uses the shipped default.
    api_body: Option<u64>,
    /// Maximum operations in one batch; `None` uses the shipped default.
    batch_ops: Option<usize>,
    /// Disable browser mutation affordances.
    read_only_browser: bool,
    /// Override the readiness free-space floor.
    ready_min_free_bytes: Option<u64>,
}

/// Binds a node on port 0, serves it on a background thread, and returns
/// its base URL, work directory and request-log path.
fn served(tag: &str, config: &Config<'_>) -> Served {
    let work = std::env::temp_dir().join(format!("choir-node-limits-{tag}"));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");
    let log = work.join("requests.jsonl");

    let mut platform = Platform::start(
        Registry::new(),
        Box::new(MemLog::new()),
        ActorKey::generate(),
    )
    .expect("platform starts");
    if let Some(max) = config.batch_ops {
        platform = platform.with_batch_limit(max);
    }

    let auth = if config.no_auth {
        None
    } else {
        let mut table = AuthTable::new();
        for (user, token) in [("alice", "a"), ("bob", "b"), ("carol", "c")] {
            table.insert(user.into(), token.into());
        }
        Some(table)
    };

    let mut node = Node::bind_with_auth(&work.join("repos"), 0, auth).expect("node binds");
    if let Some(max) = config.api_body {
        node.enable_api_body_limit(std::num::NonZeroU64::new(max).expect("positive test limit"));
    }
    if config.read_only_browser {
        node.disable_browser_writes();
    }
    if let Some(bytes) = config.ready_min_free_bytes {
        node.enable_ready_min_free_bytes(bytes);
    }
    let port = node.port();
    for repo in config.repos {
        node.create_repo(repo).expect("repo created");
    }
    if let Some(text) = config.acl {
        let path = work.join("acl");
        std::fs::write(&path, text).expect("acl file");
        node.watch_acl_file(path).expect("acl loads");
    }
    let max_bytes = if config.max_bytes == 0 {
        choir_node::limits::DEFAULT_LOG_MAX_BYTES
    } else {
        config.max_bytes
    };
    node.enable_request_log(log.clone(), max_bytes)
        .expect("request log opens");
    node.enable_rate_limit(
        config.api.and_then(std::num::NonZeroU32::new),
        config.git.and_then(std::num::NonZeroU32::new),
    );
    node.enable_platform(platform);
    std::thread::spawn(move || node.serve_forever());

    Served {
        base: format!("http://127.0.0.1:{port}"),
        work,
        log,
    }
}

/// Runs curl and returns just the status code.
fn status(args: &[&str]) -> u16 {
    let out = Command::new("curl")
        .args(["-s", "-o", "/dev/null", "-w", "%{http_code}"])
        .args(args)
        .output()
        .expect("curl runs");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("numeric status")
}

/// Runs curl and returns the response headers as one lowercase string.
fn headers(args: &[&str]) -> String {
    let out = Command::new("curl")
        .args(["-s", "-D", "-", "-o", "/dev/null"])
        .args(args)
        .output()
        .expect("curl runs");
    String::from_utf8_lossy(&out.stdout).to_lowercase()
}

/// Runs curl and returns the response body.
fn body(args: &[&str]) -> String {
    let out = Command::new("curl")
        .args(["-sS"])
        .args(args)
        .output()
        .expect("curl runs");
    assert!(
        out.status.success(),
        "curl failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("UTF-8 response")
}

/// Reads the request log, waiting up to five seconds for at least `want`
/// lines: the line is written after the response reaches the client, so a
/// test that read the file the instant curl returned would race it.
fn log_lines(path: &Path, want: usize) -> Vec<serde_json::Value> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("every log line is one JSON object"))
            .collect();
        if lines.len() >= want || Instant::now() >= deadline {
            return lines;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// git, configured the way the other node tests configure it.
fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
            "-c",
            "credential.helper=",
            "-c",
            "credential.interactive=false",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git runs")
}

/// A URL carrying basic-auth credentials, for git's own client.
fn with_creds(base: &str, creds: &str) -> String {
    format!("http://{creds}@{}", base.trim_start_matches("http://"))
}

#[test]
fn every_served_request_is_recorded_against_the_user_who_made_it() {
    let node = served("attribution", &Config::default());
    let view = format!("{}/api/view", node.base);

    assert_eq!(status(&["-u", "alice:a", &view]), 200);
    assert_eq!(status(&["-u", "bob:b", &view]), 200);

    let lines = log_lines(&node.log, 2);
    assert_eq!(lines.len(), 2, "two requests, two lines: {lines:?}");
    let users: Vec<&str> = lines
        .iter()
        .map(|l| l["user"].as_str().expect("a user field"))
        .collect();
    assert_eq!(
        users,
        vec!["alice", "bob"],
        "lines carry the authenticated user"
    );

    // Every field the brief asks for, on the first line.
    let first = &lines[0];
    assert_eq!(first["format_version"], 1);
    assert_eq!(first["method"], "GET");
    assert_eq!(first["path"], "/api/view");
    assert_eq!(first["status"], 200);
    assert!(
        first["bytes"].as_u64().expect("a byte count") > 0,
        "the view body is not empty: {first}"
    );
    assert!(first["us"].as_u64().is_some(), "an elapsed time: {first}");
    assert!(first["at_unix_ms"].as_u64().expect("a timestamp") > 0);
}

#[test]
fn the_browse_surface_is_recorded_like_every_other_route() {
    // D30/D34 pages return before the git fall-through, on their own
    // branch of the router, so "every served request" is a claim about
    // that branch too. It is the one route where an omission would be
    // invisible: an unlogged `/api/` or git request would show up as a
    // missing line in the tests above, and an unlogged page would not.
    let node = served(
        "browse-attribution",
        &Config {
            repos: &["alice/one"],
            ..Config::default()
        },
    );

    assert_eq!(
        status(&["-u", "alice:a", &format!("{}/r/", node.base)]),
        200
    );

    let lines = log_lines(&node.log, 1);
    let page = lines
        .iter()
        .find(|l| l["path"] == "/r/")
        .unwrap_or_else(|| panic!("the browse index is in the log: {lines:?}"));
    assert_eq!(page["user"], "alice");
    assert_eq!(page["status"], 200);
    assert!(
        page["bytes"].as_u64().expect("a byte count") > 0,
        "the rendered page is not empty: {page}"
    );
}

#[test]
fn nothing_that_could_carry_a_token_reaches_the_log() {
    let node = served("redaction", &Config::default());
    // A token smuggled through a query parameter, an Authorization header
    // carrying a real credential, and a body — none of the three may
    // appear anywhere in the file.
    let url = format!("{}/api/log?from=0&access_token=SUPERSECRET", node.base);
    status(&["-u", "alice:a", &url]);
    let submit = format!("{}/api/submit", node.base);
    status(&[
        "-u",
        "alice:a",
        "-X",
        "POST",
        "-d",
        "{\"secret\":\"ALSOSECRET\"}",
        &submit,
    ]);

    let lines = log_lines(&node.log, 2);
    assert_eq!(lines.len(), 2, "{lines:?}");
    let text = std::fs::read_to_string(&node.log).expect("log readable");
    assert!(
        !text.contains("SUPERSECRET"),
        "a query string reached the log: {text}"
    );
    assert!(
        !text.contains("ALSOSECRET"),
        "a request body reached the log: {text}"
    );
    assert!(
        !text.to_lowercase().contains("authorization"),
        "a header reached the log: {text}"
    );
    assert!(
        !text.contains("alice:a"),
        "a credential reached the log: {text}"
    );
    // The path survives, truncated at the `?`.
    assert_eq!(lines[0]["path"], "/api/log");
}

#[test]
fn api_bodies_and_batches_have_hard_ceilings() {
    let body_limited = served(
        "api-body-ceiling",
        &Config {
            api_body: Some(32),
            ..Config::default()
        },
    );
    let submit = format!("{}/api/submit", body_limited.base);
    assert_eq!(
        status(&[
            "-u",
            "alice:a",
            "-X",
            "POST",
            "-d",
            &"x".repeat(32),
            &submit
        ]),
        400,
        "a body exactly at the ceiling reaches normal JSON validation"
    );
    assert_eq!(
        status(&[
            "-u",
            "alice:a",
            "-X",
            "POST",
            "-d",
            &"x".repeat(33),
            &submit
        ]),
        413,
        "one byte over must be refused before parsing"
    );

    let batch_limited = served(
        "batch-ceiling",
        &Config {
            batch_ops: Some(2),
            ..Config::default()
        },
    );
    let batch = format!("{}/api/submit-batch", batch_limited.base);
    assert_eq!(
        status(&[
            "-u",
            "alice:a",
            "-X",
            "POST",
            "-d",
            r#"{"ops":[{},{},{}]}"#,
            &batch,
        ]),
        413,
        "the item ceiling applies before allocating or decoding submissions"
    );
}

#[test]
fn read_only_browser_mode_refuses_browser_mutation_routes() {
    let node = served(
        "read-only-browser",
        &Config {
            read_only_browser: true,
            ..Config::default()
        },
    );
    assert_eq!(
        status(&[
            "-u",
            "alice:a",
            "-X",
            "POST",
            "-d",
            r#"{"kind":"comment","id":"r","body":"x"}"#,
            &format!("{}/api/prepare", node.base),
        ]),
        403
    );
    assert_eq!(
        status(&["-u", "alice:a", &format!("{}/account", node.base)]),
        403
    );
    assert_eq!(
        status(&[&format!("{}/assets/webauthn.js", node.base)]),
        401,
        "read-only beta mode must not leave even the unused browser-write asset anonymous"
    );
}

#[test]
fn authenticated_health_readiness_and_metrics_report_independent_checks() {
    let node = served(
        "observability",
        &Config {
            ready_min_free_bytes: Some(0),
            ..Config::default()
        },
    );
    let state = node.work.join("repos/.choir");
    std::fs::create_dir_all(&state).expect("state directory");
    std::fs::write(state.join("ops.jsonl"), "").expect("empty verified log");

    assert_eq!(status(&[&format!("{}/healthz", node.base)]), 401);
    assert_eq!(
        status(&["-u", "alice:a", &format!("{}/healthz", node.base)]),
        200
    );
    assert_eq!(
        status(&["-u", "alice:a", &format!("{}/readyz", node.base)]),
        200
    );
    let metrics = body(&["-u", "alice:a", &format!("{}/metrics", node.base)]);
    for metric in [
        "choir_ready 1",
        "choir_log_verified 1",
        "choir_sequencer_live 1",
        "choir_storage_writable 1",
        "choir_ref_disagreements 0",
    ] {
        assert!(metrics.contains(metric), "missing {metric}: {metrics}");
    }

    let disk_refusal = served(
        "readiness-disk-refusal",
        &Config {
            ready_min_free_bytes: Some(u64::MAX),
            ..Config::default()
        },
    );
    let state = disk_refusal.work.join("repos/.choir");
    std::fs::create_dir_all(&state).expect("state directory");
    std::fs::write(state.join("ops.jsonl"), "").expect("empty verified log");
    assert_eq!(
        status(&["-u", "alice:a", &format!("{}/readyz", disk_refusal.base),]),
        503,
        "readiness must fail independently when the free-space floor is unmet"
    );
}

#[test]
fn a_refused_credential_is_recorded_as_anon() {
    let node = served("unauthorized", &Config::default());
    let view = format!("{}/api/view", node.base);
    assert_eq!(status(&["-u", "alice:wrong", &view]), 401);

    let lines = log_lines(&node.log, 1);
    assert_eq!(lines.len(), 1, "{lines:?}");
    // Never the attempted username: that string is attacker-chosen, and a
    // log that prints it as if it were a subject invites reading it as
    // one.
    assert_eq!(lines[0]["user"], "anon");
    assert_eq!(lines[0]["status"], 401);
}

#[test]
fn the_log_rotates_at_its_bound_and_keeps_one_generation() {
    // Small enough that a handful of requests crosses it; a line is
    // roughly 150 bytes.
    let node = served(
        "rotation",
        &Config {
            max_bytes: 400,
            ..Config::default()
        },
    );
    let view = format!("{}/api/view", node.base);
    for _ in 0..12 {
        assert_eq!(status(&["-u", "alice:a", &view]), 200);
    }
    log_lines(&node.log, 1);

    let mut rotated = node.log.clone().into_os_string();
    rotated.push(".1");
    let rotated = PathBuf::from(rotated);
    // Give the last write a moment to land before measuring.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !rotated.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        rotated.exists(),
        "twelve requests past a 400-byte bound never rotated"
    );

    // The live file is bounded by the threshold plus the one line that
    // crosses it, and no third generation is ever created.
    let live = std::fs::metadata(&node.log).expect("live log").len();
    assert!(
        live < 400 + 512,
        "the live log grew past its bound: {live} bytes"
    );
    let mut third = node.log.into_os_string();
    third.push(".2");
    assert!(
        !PathBuf::from(third).exists(),
        "rotation kept more than two generations"
    );
}

#[test]
fn a_user_over_their_api_ceiling_gets_429_and_is_told_when_to_come_back() {
    let node = served(
        "api-ceiling",
        &Config {
            api: Some(3),
            ..Config::default()
        },
    );
    let view = format!("{}/api/view", node.base);
    for i in 0..3 {
        assert_eq!(
            status(&["-u", "alice:a", &view]),
            200,
            "request {i} inside the allowance"
        );
    }
    assert_eq!(
        status(&["-u", "alice:a", &view]),
        429,
        "the fourth request is refused"
    );

    // The answer says when, in a header a client can act on and a body a
    // human can read.
    let head = headers(&["-u", "alice:a", &view]);
    assert!(head.contains("429"), "{head}");
    let retry = head
        .lines()
        .find_map(|line| line.strip_prefix("retry-after:"))
        .map(|v| v.trim().parse::<u64>().expect("whole seconds"))
        .expect("a Retry-After header");
    // 3 a minute is one token every 20 seconds.
    assert!(
        (1..=20).contains(&retry),
        "implausible Retry-After: {retry}"
    );

    // A second credential is unaffected: the bucket is per user.
    assert_eq!(status(&["-u", "bob:b", &view]), 200);

    let lines = log_lines(&node.log, 6);
    let refusals = lines.iter().filter(|l| l["status"] == 429).count();
    assert!(
        refusals >= 2,
        "the refusals are in the record too: {lines:?}"
    );
}

#[test]
fn the_git_and_api_ceilings_are_separate_buckets() {
    // Both ceilings are set, and deliberately so: with git left unlimited
    // the limiter never reads the map for a git request at all, and this
    // test would pass even against one bucket shared by both classes. It
    // did, until the mutation that collapsed the map key was run against
    // it.
    let node = served(
        "class-split",
        &Config {
            api: Some(1),
            git: Some(5),
            repos: &["owner/repo.git"],
            ..Config::default()
        },
    );
    let view = format!("{}/api/view", node.base);
    let refs = format!(
        "{}/owner/repo.git/info/refs?service=git-upload-pack",
        node.base
    );

    assert_eq!(status(&["-u", "alice:a", &view]), 200);
    assert_eq!(
        status(&["-u", "alice:a", &view]),
        429,
        "the API bucket is spent"
    );
    // A spent API bucket says nothing about the git one, which still has
    // its whole allowance.
    for i in 0..5 {
        assert_eq!(
            status(&["-u", "alice:a", &refs]),
            200,
            "a spent API bucket stopped git request {i}"
        );
    }
    // And the git ceiling is a real ceiling, not an unmetered path.
    assert_eq!(
        status(&["-u", "alice:a", &refs]),
        429,
        "the git bucket never ran out"
    );
}

#[test]
fn a_node_wide_grant_holder_is_never_limited() {
    let node = served(
        "operator-exempt",
        &Config {
            api: Some(1),
            acl: Some("alice   *       write\ncarol   @node   auditor\n"),
            ..Config::default()
        },
    );
    let view = format!("{}/api/view", node.base);

    // The ordinary user runs out immediately.
    assert_eq!(status(&["-u", "alice:a", &view]), 200);
    assert_eq!(status(&["-u", "alice:a", &view]), 429);

    // The operator does not, at any point. A limiter that can lock out the
    // one actor who can repair the node is worse than no limiter.
    for i in 0..12 {
        assert_eq!(
            status(&["-u", "carol:c", &view]),
            200,
            "the @node grant holder was throttled on request {i}"
        );
    }
}

#[test]
fn the_hook_callback_is_never_limited_so_a_multi_ref_push_lands() {
    let node = served(
        "hook-exempt",
        &Config {
            // One API request a minute. Each ref of the push below makes
            // its own `/api/git-update` callback, so without the internal
            // exemption the second ref is refused, the push fails, and the
            // retraction path runs for refs git never created.
            api: Some(1),
            repos: &["owner/repo.git"],
            ..Config::default()
        },
    );
    let url = with_creds(&node.base, "alice:a");
    let dir = node.work.join("clone");
    std::fs::create_dir_all(&dir).expect("clone dir");
    assert!(
        git(
            &node.work,
            &[
                "clone",
                "-q",
                &format!("{url}/owner/repo.git"),
                dir.to_str().unwrap()
            ]
        )
        .status
        .success(),
        "clone failed"
    );
    std::fs::write(dir.join("f.txt"), "seed\n").expect("write");
    assert!(git(&dir, &["add", "."]).status.success());
    assert!(git(&dir, &["commit", "-q", "-m", "seed"]).status.success());

    // Four refs in one push: four hook callbacks, against a ceiling of one.
    let push = git(
        &dir,
        &[
            "push",
            "-q",
            "origin",
            "HEAD:refs/heads/main",
            "HEAD:refs/heads/one",
            "HEAD:refs/heads/two",
            "HEAD:refs/heads/three",
        ],
    );
    assert!(
        push.status.success(),
        "a four-ref push was throttled by its own hook callbacks: {}",
        String::from_utf8_lossy(&push.stderr)
    );

    // And all four refs really landed in the view, read on a second
    // credential so this check does not spend alice's own allowance.
    let out = Command::new("curl")
        .args(["-s", "-u", "bob:b", &format!("{}/api/view", node.base)])
        .output()
        .expect("curl runs");
    let body = String::from_utf8_lossy(&out.stdout);
    for name in ["main", "one", "two", "three"] {
        assert!(
            body.contains(&format!("refs/heads/{name}")),
            "{name} never reached the view: {body}"
        );
    }
}

#[test]
fn a_node_without_authentication_is_never_limited() {
    // There is no per-user identity to meter, so one shared `anon` bucket
    // would let any client lock out every other one. `main.rs` refuses the
    // flag combination outright; this proves the library agrees, which is
    // the path that would actually cause the lockout.
    let node = served(
        "no-auth",
        &Config {
            api: Some(1),
            no_auth: true,
            ..Config::default()
        },
    );
    let view = format!("{}/api/view", node.base);
    for i in 0..8 {
        assert_eq!(
            status(&[&view]),
            200,
            "request {i} was limited on an unauthenticated node"
        );
    }
    // It is still recorded, as `anon` — attribution and metering are
    // separate questions and only one of them needs a credential.
    let lines = log_lines(&node.log, 8);
    assert!(lines.iter().all(|l| l["user"] == "anon"), "{lines:?}");
}

/// Runs the daemon binary with `args` and returns its stderr, failing if
/// it has not exited in time. A guard that does not refuse would leave the
/// node serving forever, which is a hang rather than a failure — so the
/// wait is bounded and the timeout is the assertion.
///
/// The bound is generous on purpose. A refusal takes milliseconds, so the
/// only thing a long deadline costs is the time a genuine regression takes
/// to report; a short one costs a false failure whenever this machine is
/// also building something else, which is how it is usually run.
fn daemon_refuses(work: &Path, args: &[&str]) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_choir-node"))
        .arg(work)
        .arg("0")
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("daemon starts");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match child.try_wait().expect("wait") {
            Some(exit) => {
                assert!(!exit.success(), "the daemon accepted {args:?}");
                let out = child.wait_with_output().expect("output");
                return String::from_utf8_lossy(&out.stderr).to_string();
            }
            None if Instant::now() >= deadline => {
                child.kill().ok();
                panic!("the daemon did not refuse {args:?}; it is serving");
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

#[test]
fn the_daemon_refuses_attribution_and_metering_without_authentication() {
    let work = std::env::temp_dir().join("choir-node-limits-flags");
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");

    let err = daemon_refuses(
        &work,
        &["--request-log", work.join("r.jsonl").to_str().unwrap()],
    );
    assert!(err.contains("--auth-file"), "{err}");

    let err = daemon_refuses(&work, &["--rate-limit-api", "10"]);
    assert!(err.contains("--auth-file"), "{err}");

    // Zero is not a ceiling, it is a wall with no way back through it.
    let auth = work.join("auth");
    std::fs::write(&auth, "alice:a\n").expect("auth file");
    let err = daemon_refuses(
        &work,
        &[
            "--auth-file",
            auth.to_str().unwrap(),
            "--rate-limit-git",
            "0",
        ],
    );
    assert!(err.contains("positive integer"), "{err}");

    // A flag with no value is a typo, not a default.
    let err = daemon_refuses(
        &work,
        &["--auth-file", auth.to_str().unwrap(), "--rate-limit-api"],
    );
    assert!(err.contains("needs a value"), "{err}");
}
