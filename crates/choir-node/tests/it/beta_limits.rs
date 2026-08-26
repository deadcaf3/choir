//! BETA-05. The numbers in the rendered unit are the numbers the node runs on.
//!
//! Four copies of the private beta's ceilings exist: the manifest that
//! declares them, the renderer that emits them as flags, the unit that
//! carries those flags, and the daemon that parses them. Three of those
//! are strings, and until now they agreed only because the same person
//! typed them on the same afternoon. The private-beta renderer used to
//! claim otherwise when it refused a mismatched state directory, saying
//! the manifest differed from "the limits and feature set this renderer
//! emits" while checking nothing of the sort: it compares the manifest
//! file to its own copy of the manifest file, never to the flags it goes
//! on to write. Its message now says what it checks, and this says the
//! rest.
//!
//! This walks the chain once. The manifest's numbers must be the unit's
//! flag values, and the unit's flag values must be the numbers a real
//! daemon reports having parsed. Nothing here is hardcoded; changing a
//! ceiling in the manifest and nowhere else fails at the first leg,
//! changing it in the renderer and nowhere else fails at the same leg
//! from the other side.
//!
//! What it does not prove is enforcement at those sizes: a test that
//! pushes 512 MiB to observe a quota is not a test anyone will run. The
//! ceilings are enforced against small synthetic values in `limits.rs`
//! and `quotas.rs`; this pins the configuration those mechanisms are
//! handed in production.

/// Manifest key, the flag the renderer must carry it as, and the
/// sentence the daemon must say having parsed it. `{}` is the number.
const CEILINGS: &[(&str, &str, &str)] = &[
    (
        "api_body_bytes",
        "--api-body-limit",
        "limits: {} API body bytes",
    ),
    (
        "batch_operations",
        "--batch-limit",
        "{} operations per batch",
    ),
    (
        "ready_min_free_bytes",
        "--ready-min-free-bytes",
        "readiness: at least {} free storage bytes",
    ),
    (
        "api_requests_per_minute",
        "--rate-limit-api",
        "rate limit: {} API requests per minute per user",
    ),
    (
        "git_requests_per_minute",
        "--rate-limit-git",
        "rate limit: {} git requests per minute per user",
    ),
    (
        "git_push_bytes_per_user",
        "--quota-push-bytes",
        "quota: at most {} bytes in one git request per user",
    ),
    (
        "workspaces_per_user",
        "--quota-workspaces",
        "quota: at most {} workspaces held per user",
    ),
    (
        "request_log_max_bytes",
        "--request-log-max-bytes",
        "past {} bytes",
    ),
];

const MANIFEST: &str = include_str!("../../../../scripts/flip/private-beta.manifest");

fn manifest_value(key: &str) -> &'static str {
    MANIFEST
        .lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
        .unwrap_or_else(|| panic!("private-beta.manifest declares no {key}"))
        .trim()
}

/// Renders the private-beta unit and returns its `ExecStart` argv.
fn unit_argv() -> Vec<String> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root")
        .to_path_buf();
    let repos = std::env::temp_dir().join(format!(
        "choir-node-beta-limits-repos-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::write(&repos, "owner/repo.git\n").expect("repos.list");
    let output = std::process::Command::new("sh")
        .arg(root.join("scripts/flip/render_node_service.sh"))
        .args([
            "choir-node",
            "/opt/choir-node",
            "/srv/choir/repos",
            "8417",
            "/srv/choir/auth",
            "/srv/choir/keys",
            "/srv/choir/reviewers",
            "/var/log/choir/node.log",
        ])
        .arg(&repos)
        .args([
            "/srv/choir/newcomer-audit.jsonl",
            "/srv/choir/newcomer-adjudications.jsonl",
            "/srv/choir/protected-refs",
            "require-scope",
            "",
            "",
            "/srv/choir/acl",
            "choir",
        ])
        .output()
        .expect("render the private-beta unit");
    std::fs::remove_file(&repos).ok();
    assert!(
        output.status.success(),
        "renderer refused: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let unit = String::from_utf8(output.stdout).expect("UTF-8 unit");
    let line = unit
        .lines()
        .find(|line| line.starts_with("ExecStart="))
        .expect("unit defines ExecStart");
    line["ExecStart=".len()..]
        .split(' ')
        .map(str::to_string)
        .collect()
}

/// The value the unit passes to `flag`, or a panic naming the flag.
fn flag_value(argv: &[String], flag: &str) -> String {
    let at = argv
        .iter()
        .position(|arg| arg == flag)
        .unwrap_or_else(|| panic!("the private-beta unit carries no {flag}: {argv:?}"));
    argv.get(at + 1)
        .unwrap_or_else(|| panic!("{flag} is the last argument, with no value"))
        .clone()
}

#[test]
fn the_manifest_declares_the_ceilings_the_unit_carries() {
    let argv = unit_argv();
    for (key, flag, _) in CEILINGS {
        assert_eq!(
            flag_value(&argv, flag),
            manifest_value(key),
            "private-beta.manifest {key} and the unit's {flag} disagree"
        );
    }
}

#[test]
fn the_node_runs_on_the_ceilings_the_unit_carries() {
    let argv = unit_argv();
    let work = std::env::temp_dir().join(format!(
        "choir-node-beta-limits-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("work directory");
    // The request-log cap is only announced when the log is enabled, and
    // the log is only enabled beside an auth file, because every line it
    // writes is attributed to a user.
    let auth = work.join("auth");
    std::fs::write(&auth, "alice:a\n").expect("auth file");

    // Port 0, and the readiness signal is read off stderr rather than by
    // connecting. The usual shape here -- bind 0, take the number, drop
    // the listener, hand the number to a child -- leaves a window in
    // which another test's `bind(0)` is handed the same port, and this
    // harness runs its daemons in parallel. This test never needs to
    // speak to the node, only to hear what it parsed, so it can stay out
    // of that race entirely instead of widening it.
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_choir-node"));
    command.args([
        work.join("repos").as_os_str(),
        std::ffi::OsStr::new("0"),
        std::ffi::OsStr::new("--auth-file"),
        auth.as_os_str(),
        std::ffi::OsStr::new("--request-log"),
        work.join("requests.jsonl").as_os_str(),
    ]);
    for (_, flag, _) in CEILINGS {
        command.arg(flag).arg(flag_value(&argv, flag));
    }
    let mut child = command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("start choir-node");

    // Read stderr on a thread so a child that never reaches `serving`
    // cannot block the read forever; the pipe closes when it is killed.
    let mut pipe = child.stderr.take().expect("stderr is piped");
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        use std::io::BufRead;
        let mut announced = String::new();
        for line in std::io::BufReader::new(&mut pipe)
            .lines()
            .map_while(Result::ok)
        {
            let serving = line.contains("serving");
            announced.push_str(&line);
            announced.push('\n');
            if serving {
                tx.send(()).ok();
            }
        }
        announced
    });
    let serving = rx.recv_timeout(std::time::Duration::from_secs(30)).is_ok();
    child.kill().ok();
    child.wait().ok();
    let announced = reader.join().expect("stderr reader");
    std::fs::remove_dir_all(&work).ok();
    assert!(
        serving,
        "choir-node never reported serving on the unit's own ceilings: {announced}"
    );

    for (key, flag, sentence) in CEILINGS {
        let expected = sentence.replace("{}", &flag_value(&argv, flag));
        assert!(
            announced.contains(&expected),
            "the node never said `{expected}` for {key}; it said:\n{announced}"
        );
    }
}
