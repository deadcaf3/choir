//! The two halves of D37 that cannot be asserted inside the merged
//! `tests/it/` harness, driven against a real served node with real
//! `git` and `curl` subprocesses.
//!
//! # Why this is its own test binary
//!
//! One test restarts the daemon — it stops the node, drops the platform,
//! reopens the same persisted op log and starts again — which the merged
//! harness forbids outright: its modules share a process and run on
//! parallel threads. The other reads the request log's tail, which is
//! written *after* the response reaches the client and so has to be
//! waited for. `tests/limits.rs` is its own binary for the same two
//! reasons and this file follows it.
//!
//! Run:
//!
//! ```text
//! cargo test -p choir-node --test quotas
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use choir_identity::{ActorKey, Registry};
use choir_node::{AuthTable, Node, Platform};
use choir_oplog::FileLog;

fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
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

/// `bytes` of incompressible content, so a pack of it is about as large
/// as the file. Hand-rolled xorshift rather than a `rand` dev-dependency,
/// which is the bar this workspace sets for a test that needs noise.
fn noise(bytes: usize) -> Vec<u8> {
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut out = Vec::with_capacity(bytes);
    while out.len() < bytes {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(bytes);
    out
}

fn curl_json(args: &[&str]) -> (u16, serde_json::Value) {
    let out = Command::new("curl")
        .args(["-s", "-w", "\n%{http_code}"])
        .args(args)
        .output()
        .expect("curl runs");
    let text = String::from_utf8_lossy(&out.stdout);
    let (body, code) = text.rsplit_once('\n').expect("status line");
    (
        code.trim().parse().expect("numeric status"),
        serde_json::from_str(body).unwrap_or_else(|e| panic!("JSON response ({e}): {body:?}")),
    )
}

/// Reads the request log, waiting up to five seconds for at least `want`
/// lines: a line is written after the response reaches the client, so
/// reading the instant curl or git returned would race it.
fn log_lines(path: &Path, want: usize) -> Vec<serde_json::Value> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("each line is one JSON object"))
            .collect();
        if lines.len() >= want || Instant::now() > deadline {
            return lines;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_push_over_the_ceiling_is_refused_without_the_hook_ever_running() {
    let work = std::env::temp_dir().join("choir-node-quota-push");
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");
    let root = work.join("repos");
    let request_log = work.join("requests.jsonl");

    let mut auth = AuthTable::new();
    auth.insert("alice".into(), "a".into());
    auth.insert("carol".into(), "c".into());
    let mut node = Node::bind_with_auth(&root, 0, Some(auth)).expect("node binds");
    let port = node.port();
    node.create_repo("agents/demo.git").expect("repo created");
    let acl = work.join("acl");
    std::fs::write(
        &acl,
        "alice   *       write\ncarol   *       write\ncarol   @node   auditor\n",
    )
    .expect("acl file");
    node.watch_acl_file(acl).expect("acl loads");
    node.enable_request_log(
        request_log.clone(),
        choir_node::limits::DEFAULT_LOG_MAX_BYTES,
    )
    .expect("request log opens");
    // 100 KiB: comfortably over a one-file commit and comfortably under
    // the two oversized pushes below.
    node.enable_quotas(std::num::NonZeroU64::new(100 * 1024), None);
    node.enable_platform(
        Platform::start(
            Registry::new(),
            Box::new(choir_oplog::MemLog::new()),
            ActorKey::generate(),
        )
        .expect("platform starts"),
    );
    std::thread::spawn(move || node.serve_forever());

    let url = format!("http://alice:a@127.0.0.1:{port}/agents/demo.git");
    let view_url = format!("http://127.0.0.1:{port}/api/view");
    let clone = work.join("clone");
    assert!(git(
        &work,
        &["clone", "-q", &url, clone.to_str().expect("utf-8 path")]
    )
    .status
    .success());

    // Control first: an ordinary push is not refused, and it does reach
    // the sequencer. Without this the two refusals below could be a
    // broken push path rather than a working ceiling.
    std::fs::write(clone.join("small.txt"), "v1\n").expect("small file");
    git(&clone, &["add", "."]);
    git(&clone, &["commit", "-q", "-m", "small"]);
    let push = git(&clone, &["push", "-q", "origin", "HEAD:main"]);
    assert!(push.status.success(), "{push:?}");
    let head = String::from_utf8_lossy(&git(&clone, &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();
    let ref_name = "agents/demo.git:refs/heads/main";
    let (code, view) = curl_json(&["-u", "alice:a", &view_url]);
    assert_eq!(code, 200, "{view}");
    let landed = view["refs"][ref_name].clone();
    assert!(
        landed.as_str().is_some_and(|value| value.ends_with(&head)),
        "the control push must have reached the sequencer as {head}: {view}"
    );

    // 300 KiB of noise: over the ceiling, and small enough that git sends
    // it with a Content-Length.
    std::fs::write(clone.join("big.bin"), noise(300 * 1024)).expect("big file");
    git(&clone, &["add", "."]);
    git(&clone, &["commit", "-q", "-m", "big"]);
    let refused = git(&clone, &["push", "origin", "HEAD:main"]);
    assert!(!refused.status.success(), "{refused:?}");
    let text = String::from_utf8_lossy(&refused.stderr).to_lowercase();
    assert!(
        text.contains("413") || text.contains("push refused"),
        "the pusher must be told the size was the problem: {text}"
    );

    // 2 MiB of noise: over git's `http.postBuffer`, so this one arrives
    // chunked with no Content-Length at all — the case a check that
    // trusted the header would wave through.
    std::fs::write(clone.join("huge.bin"), noise(2 * 1024 * 1024)).expect("huge file");
    git(&clone, &["add", "."]);
    git(&clone, &["commit", "-q", "-m", "huge"]);
    let refused = git(&clone, &["push", "origin", "HEAD:main"]);
    assert!(!refused.status.success(), "{refused:?}");

    // The point of enforcing before the CGI spawn: no `pre-receive` hook
    // ran, so nothing was submitted, so the ref is exactly where the
    // control push left it. A ceiling enforced inside the hook would
    // leave the log holding a ref this push did not create, and would
    // have had to drive the retraction pass to take it back.
    let (code, view) = curl_json(&["-u", "alice:a", &view_url]);
    assert_eq!(code, 200, "{view}");
    assert_eq!(
        view["refs"][ref_name], landed,
        "a refused push must not have moved the ref: {view}"
    );
    assert_eq!(
        view["refs"].as_object().map(serde_json::Map::len),
        Some(1),
        "a refused push must not have created a ref either: {view}"
    );

    // The refusal names the size that was actually sent, not the ceiling
    // plus one. That is the only externally visible consequence of
    // draining the remainder rather than dropping it, and without this
    // assertion removing the drain passes every other test here — it was
    // mutation-tested, it did exactly that, and this line is the answer.
    // (Driven with curl rather than git because git does not reliably
    // surface a 413's body to its caller.)
    let oversized = work.join("oversized.bin");
    std::fs::write(&oversized, noise(200 * 1024)).expect("oversized body");
    let receive_pack = format!("http://127.0.0.1:{port}/agents/demo.git/git-receive-pack");
    let out = Command::new("curl")
        .args([
            "-s",
            "-u",
            "alice:a",
            "-X",
            "POST",
            "-H",
            "Content-Type: application/x-git-receive-pack-request",
            "--data-binary",
            &format!("@{}", oversized.display()),
            &receive_pack,
        ])
        .output()
        .expect("curl runs");
    let refusal = String::from_utf8_lossy(&out.stdout);
    assert!(
        refusal.contains(&format!("{} bytes", 200 * 1024)),
        "the refusal must name the real size, not the ceiling plus one: {refusal:?}"
    );
    assert!(
        refusal.contains(&format!("{} bytes per request", 100 * 1024)),
        "and the ceiling it was measured against: {refusal:?}"
    );

    // The same oversized push from a holder of a D29 `@node` grant is
    // not refused. D33's exemption, applied here unchanged: that grant is
    // already total authority over the node, and the actor who can repair
    // it must not be the one the ceiling stops. Without the two refusals
    // above this would only show that pushing works.
    let exempt_url = format!("http://carol:c@127.0.0.1:{port}/agents/demo.git");
    let exempt = git(&clone, &["push", "-q", &exempt_url, "HEAD:main"]);
    assert!(
        exempt.status.success(),
        "the @node grant holder was refused: {exempt:?}"
    );
    let (code, view) = curl_json(&["-u", "carol:c", &view_url]);
    assert_eq!(code, 200, "{view}");
    assert_ne!(
        view["refs"][ref_name], landed,
        "the exempt push must have landed: {view}"
    );

    // And every refusal is in the D33 record, against the user it was
    // charged to rather than against `anon`.
    let lines = log_lines(&request_log, 1);
    let refusals: Vec<_> = lines.iter().filter(|line| line["status"] == 413).collect();
    assert_eq!(
        refusals.len(),
        3,
        "two refused pushes and the direct one are all recorded: {lines:#?}"
    );
    for line in &refusals {
        assert_eq!(line["user"], "alice", "{line}");
        assert_eq!(line["method"], "POST", "{line}");
        assert!(
            line["path"]
                .as_str()
                .is_some_and(|p| p.ends_with("/git-receive-pack")),
            "{line}"
        );
    }
}

/// Provisions one workspace for `user`, returning the HTTP status.
fn create(port: u16, user: &str, name: &str) -> (u16, serde_json::Value) {
    let url = format!("http://127.0.0.1:{port}/api/workspace");
    curl_json(&[
        "-u",
        user,
        "-X",
        "POST",
        "-d",
        &format!(r#"{{"repo":"agents/demo","name":"{name}"}}"#),
        &url,
    ])
}

/// Binds a node over `root`, gives it a platform backed by the persisted
/// log at `log_path`, serves it, and hands back the port plus a shutdown.
fn serve(root: &Path, log_path: &Path, ceiling: u32) -> (u16, Box<dyn FnOnce()>) {
    let mut node = Node::bind_with_auth(root, 0, {
        let mut auth = AuthTable::new();
        auth.insert("alice".into(), "a".into());
        Some(auth)
    })
    .expect("node binds");
    let port = node.port();
    node.enable_quotas(None, std::num::NonZeroU32::new(ceiling));
    node.enable_platform(
        Platform::start(
            Registry::new(),
            Box::new(FileLog::open(log_path).expect("log opens")),
            ActorKey::generate(),
        )
        .expect("platform starts"),
    );
    let node = std::sync::Arc::new(node);
    let serving = {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever())
    };
    let stop = move || {
        node.unblock();
        serving.join().expect("serving thread joins");
        // The last handle: dropping it drops the platform, which stops
        // the writer thread and closes the log this test is about to
        // reopen.
        drop(node);
    };
    (port, Box::new(stop))
}

fn seed_commit(work: &Path, bare: &Path) {
    let head_ref = String::from_utf8_lossy(&git(bare, &["symbolic-ref", "HEAD"]).stdout)
        .trim()
        .to_string();
    let seed = work.join("seed");
    std::fs::create_dir_all(&seed).expect("seed dir");
    assert!(git(&seed, &["init", "-q"]).status.success());
    std::fs::write(seed.join("f.txt"), "v1\n").expect("seed file");
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "first"]);
    let refspec = format!("HEAD:{head_ref}");
    let push = git(
        &seed,
        &["push", "-q", bare.to_str().expect("utf-8 path"), &refspec],
    );
    assert!(push.status.success(), "{push:?}");
}

#[test]
fn the_workspace_tally_is_rebuilt_from_the_log_on_restart() {
    let work: PathBuf = std::env::temp_dir().join("choir-node-quota-restart");
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");
    let root = work.join("repos");
    let log_path = work.join("ops.jsonl");

    // First life: alice spends her whole allowance.
    let (port, stop) = serve(&root, &log_path, 2);
    {
        let node = Node::bind_with_auth(&root, 0, None).expect("node binds");
        node.create_repo("agents/demo.git").expect("repo created");
    }
    seed_commit(&work, &root.join("agents/demo.git"));
    for name in ["one", "two"] {
        let (code, body) = create(port, "alice:a", name);
        assert_eq!(code, 200, "{name}: {body}");
    }
    let (code, body) = create(port, "alice:a", "three");
    assert_eq!(code, 403, "before the restart the ceiling holds: {body}");
    stop();

    // Second life: same persisted log, a brand new process-level state.
    // The tally is not written anywhere, so if it were only ever held in
    // memory this node would start at zero and admit three more.
    let (port, stop) = serve(&root, &log_path, 2);
    let (code, body) = create(port, "alice:a", "three");
    assert_eq!(
        code, 403,
        "the tally must be rebuilt from the log, not restarted at zero: {body}"
    );
    assert_eq!(body["actual"], "2 workspaces", "{body}");
    stop();

    // The tally and the view are folded from the same operations in the
    // same replay, so after that replay they must name the same
    // workspaces. A divergence here is the tripwire on the D37 row.
    let platform = Platform::start(
        Registry::new(),
        Box::new(FileLog::open(&log_path).expect("log opens")),
        ActorKey::generate(),
    )
    .expect("platform starts");
    let (status, view) = platform.handle_api("GET", "/api/view", b"");
    assert_eq!(status, 200, "{view}");
    let view: serde_json::Value = serde_json::from_str(&view).expect("view is json");
    let mut in_view: Vec<String> = view["workspaces"]
        .as_object()
        .expect("workspaces object")
        .keys()
        .cloned()
        .collect();
    in_view.sort();
    assert_eq!(
        platform.tallied_workspaces(),
        in_view,
        "the tally and the view disagree about what exists"
    );
    assert_eq!(platform.workspaces_held_by("git/alice"), 2);
    assert_eq!(platform.workspaces_held_by("git/bob"), 0);
}
