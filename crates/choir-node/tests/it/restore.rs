//! Restoring a node from a backup, end to end, against the real script.
//!
//! The backup here is not a fixture. A daemon is started, pushed to, and
//! harvested exactly as `scripts/pull_backup.sh` harvests it, because a
//! restore validated against hand-written sample data validates an
//! imagination of the format. What comes out of a live node is the only
//! input worth restoring from.
//!
//! The whole point of the exercise is the last assertion: a restored node
//! that serves is not a restored node. It has to accept a write.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};

/// A private directory per test: this harness shares a process and runs
/// on parallel threads.
fn workdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "choir-restore-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        // The user's own config signs commits; a scratch repo has no key.
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "restore test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "restore test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .expect("git runs")
}

/// The policy set `pull_backup.sh` names, written where a node reads it.
fn write_policy(dir: &Path, repo: &str) {
    std::fs::create_dir_all(dir).expect("policy dir");
    std::fs::write(dir.join("keys"), "").expect("keys");
    std::fs::write(dir.join("reviewers"), "op/one\nop/two\n").expect("reviewers");
    std::fs::write(dir.join("protected-refs"), "").expect("protected-refs");
    std::fs::write(dir.join("newcomer-audit.jsonl"), "").expect("audit");
    std::fs::write(dir.join("newcomer-adjudications.jsonl"), "").expect("adjudications");
    std::fs::write(dir.join("review-adjudications.jsonl"), "").expect("review adjudications");
    std::fs::write(dir.join("repos.list"), format!("{repo}\n")).expect("repos.list");
    std::fs::write(dir.join("acl"), "choir * own\n").expect("acl");
    std::fs::write(
        dir.join("private-beta.manifest"),
        include_str!("../../../../scripts/flip/private-beta.manifest"),
    )
    .expect("beta manifest");
}

/// Starts the daemon on port 0 and waits for its own serving marker,
/// which carries the port it actually got.
///
/// Waiting for a marker rather than a duration, and reading the port from
/// it rather than choosing one, are the same decision twice: nothing here
/// asserts on how fast the machine is or hopes a port is free.
fn boot(root: &Path, policy: &Path, auth: &Path, repo: &str) -> (Child, u16) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_choir-node"))
        .arg(root)
        .arg("0")
        .args(["--bind", "127.0.0.1"])
        .arg("--auth-file")
        .arg(auth)
        .arg("--keys-file")
        .arg(policy.join("keys"))
        .arg("--reviewers-file")
        .arg(policy.join("reviewers"))
        .args(["--create", repo])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn choir-node");

    let stderr = child.stderr.take().expect("piped stderr");
    let port = std::sync::Arc::new(std::sync::Mutex::new(None::<u16>));
    let text = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    {
        let (port, text) = (port.clone(), text.clone());
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stderr)
                .lines()
                .map_while(Result::ok)
            {
                if let Some(rest) = line.split_once("choir-node serving") {
                    if let Some(p) = rest
                        .1
                        .rsplit(':')
                        .next()
                        .and_then(|p| p.trim().parse().ok())
                    {
                        *port.lock().expect("port") = Some(p);
                    }
                }
                let mut t = text.lock().expect("text");
                t.push_str(&line);
                t.push('\n');
            }
        });
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        if let Some(p) = *port.lock().expect("port") {
            return (child, p);
        }
        if child.try_wait().expect("wait").is_some() {
            std::thread::sleep(std::time::Duration::from_millis(50));
            panic!(
                "choir-node exited before serving:\n{}",
                text.lock().expect("text")
            );
        }
        assert!(
            std::time::Instant::now() < deadline,
            "choir-node neither served nor exited within 60s:\n{}",
            text.lock().expect("text")
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn stop(mut child: Child) {
    child.kill().ok();
    child.wait().ok();
}

/// Runs the restore script the way `choirctl` runs it.
fn restore(src: &Path, root: &Path) -> (i32, String, String) {
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/restore_from_backup.sh")
        .canonicalize()
        .expect("script path");
    let out = Command::new("sh")
        .arg(&script)
        .arg(src)
        .arg(root)
        .env("CHOIR_NODE_BIN", env!("CARGO_BIN_EXE_choir-node"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("sh runs");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// A live node, pushed to, harvested into the directory layout
/// `pull_backup.sh` produces. Returns `(backup dir, node key bytes)`.
///
/// The key comes back separately because that is the truth about a
/// backup: it never contains one.
fn make_backup(work: &Path, repo: &str) -> (PathBuf, Vec<u8>) {
    let root = work.join("live");
    let policy = work.join("policy");
    let auth = work.join("auth");
    std::fs::create_dir_all(&root).expect("root");
    std::fs::write(&auth, "choir:restoretest\n").expect("auth");
    write_policy(&policy, repo);

    let (child, port) = boot(&root, &policy, &auth, repo);

    // A commit, pushed the way anything reaches this node: over HTTP,
    // through the hook, into the log.
    let url = format!("http://choir:restoretest@127.0.0.1:{port}/{repo}");
    let clone = work.join("seed");
    assert!(
        git(
            work,
            &["clone", "--quiet", &url, clone.to_str().expect("utf8")]
        )
        .status
        .success(),
        "clone the live node"
    );
    std::fs::write(clone.join("f.txt"), "restored\n").expect("write");
    git(&clone, &["add", "."]);
    git(&clone, &["commit", "--quiet", "-m", "seed"]);
    let pushed = git(
        &clone,
        &["push", "--quiet", "origin", "HEAD:refs/heads/main"],
    );
    assert!(
        pushed.status.success(),
        "push to the live node: {}",
        String::from_utf8_lossy(&pushed.stderr)
    );

    let state = root.join(".choir");
    let key = std::fs::read(state.join("node.key")).expect("the live node has a key");
    stop(child);

    // Harvest, by explicit name, exactly as pull_backup.sh does — and
    // with the same omission, which is the whole reason this test has a
    // second half.
    let backup = work.join("backup");
    std::fs::create_dir_all(backup.join("policy")).expect("backup dir");
    std::fs::create_dir_all(backup.join("repos").join(repo).parent().expect("parent"))
        .expect("bundle dir");
    std::fs::copy(state.join("ops.jsonl"), backup.join("ops.jsonl")).expect("log");
    std::fs::copy(
        state.join("node.fingerprint"),
        backup.join("node.fingerprint"),
    )
    .expect("fingerprint");
    if state.join("refs.snapshot").exists() {
        std::fs::copy(state.join("refs.snapshot"), backup.join("refs.snapshot")).expect("snapshot");
    }
    for f in [
        "keys",
        "reviewers",
        "protected-refs",
        "newcomer-audit.jsonl",
        "newcomer-adjudications.jsonl",
        "review-adjudications.jsonl",
        "repos.list",
        "acl",
        "private-beta.manifest",
    ] {
        std::fs::copy(policy.join(f), backup.join("policy").join(f)).expect("policy file");
    }
    let bundle = backup.join("repos").join(format!("{repo}.bundle"));
    assert!(
        git(
            &root.join(repo),
            &["bundle", "create", bundle.to_str().expect("utf8"), "--all"]
        )
        .status
        .success(),
        "bundle the live repo"
    );

    (backup, key)
}

/// The whole point: a backup, plus the one secret a backup never holds,
/// becomes a node that takes a write.
#[test]
fn a_backup_and_the_key_become_a_node_that_accepts_a_write() {
    let work = workdir("full");
    let repo = "owner/demo.git";
    let (backup, key) = make_backup(&work, repo);

    let root = work.join("restored");
    std::fs::create_dir_all(root.join(".choir")).expect("restored root");
    // The out-of-band half of a restore. Doing it by hand here is not a
    // shortcut around the script: it *is* the step, and the script's job
    // is to refuse until it has happened.
    std::fs::write(root.join(".choir").join("node.key"), &key).expect("key");
    std::fs::write(root.join(".choir").join("auth"), "choir:restoretest\n").expect("auth");

    let (code, out, err) = restore(&backup, &root);
    assert_eq!(
        code, 0,
        "restore should succeed.\nstdout:\n{out}\nstderr:\n{err}"
    );
    assert!(out.contains("canary landed"), "{out}");
    // The D25 comparison is conditional on the backup carrying an
    // attestation, so its success line is the only evidence it ran at
    // all. Without this, a restore whose attestation check silently
    // skipped looks exactly like one that passed it.
    assert!(
        out.contains("view matches the attestation"),
        "the ref attestation must have been checked, not skipped: {out}"
    );

    // The log grew by exactly the canary, and everything restored is
    // still where it was. `pull_backup.sh` asserts the same prefix
    // relation in the other direction, which is what makes the pair a
    // round trip rather than two scripts that both claim to work.
    let backup_log = std::fs::read(backup.join("ops.jsonl")).expect("backup log");
    let restored_log = std::fs::read(root.join(".choir").join("ops.jsonl")).expect("restored log");
    assert!(
        restored_log.len() > backup_log.len(),
        "the canary must have appended"
    );
    assert_eq!(
        &restored_log[..backup_log.len()],
        &backup_log[..],
        "the restore must be a byte-exact continuation of the backup"
    );
    // What was appended, not how much. A count is satisfied by any two
    // ops, including two the restore had no business creating — a
    // retraction pass produces ops too, and would read as a pass here.
    // The payload is a byte array in the entry, so the op kind is not
    // visible in the raw line and a substring match on it reads whatever
    // happens to be there. Decode it, the way any reader of this log must.
    let appended: Vec<String> = String::from_utf8_lossy(&restored_log[backup_log.len()..])
        .lines()
        .map(|line| {
            let entry: serde_json::Value = serde_json::from_str(line).expect("entry decodes");
            let bytes: Vec<u8> = entry["payload"]
                .as_array()
                .expect("payload is bytes")
                .iter()
                .map(|b| u8::try_from(b.as_u64().expect("byte")).expect("byte"))
                .collect();
            String::from_utf8(bytes).expect("payload is a JSON ViewOp")
        })
        .collect();
    assert_eq!(
        appended.len(),
        2,
        "the canary and its attestation: {appended:?}"
    );
    assert!(
        appended[0].contains("SetRef") && appended[0].contains("restore-canary-"),
        "the first appended op is the canary push: {}",
        appended[0]
    );
    // D25: every admitted ref op is followed by the ref-state attestation,
    // so a restored node starts attesting again from its first write.
    assert!(
        appended[1].contains("RecordRefSnapshot"),
        "the canary must be attested like any other ref op: {}",
        appended[1]
    );

    std::fs::remove_dir_all(work).ok();
}

/// Without the key, the restore stops and hands the operator the choice.
///
/// It must not mint one. A node key minted over someone else's log is a
/// new node wearing the old node's history, and every op after the seam
/// has a different author with nothing marking where it changed.
#[test]
fn a_restore_missing_the_node_key_stops_and_names_the_choice() {
    let work = workdir("nokey");
    let repo = "owner/demo.git";
    let (backup, _key) = make_backup(&work, repo);

    let root = work.join("restored");
    let (code, out, err) = restore(&backup, &root);

    assert_eq!(
        code, 3,
        "an operator decision, not a failure.\n{out}\n{err}"
    );
    assert!(err.contains("node.fingerprint"), "{err}");
    assert!(
        err.contains("changes author"),
        "the second option must say what it costs: {err}"
    );
    assert!(
        !root.join(".choir").join("node.key").exists(),
        "the restore must not have minted a key"
    );
    // Stopping is not the same as leaving nothing behind: the files are
    // placed, so the operator's decision is the only remaining step.
    assert!(root.join(".choir").join("ops.jsonl").exists(), "log placed");

    std::fs::remove_dir_all(work).ok();
}

/// A root that already holds a log is refused, untouched.
#[test]
fn a_restore_never_writes_over_an_existing_log() {
    let work = workdir("clobber");
    let repo = "owner/demo.git";
    let (backup, _key) = make_backup(&work, repo);

    let root = work.join("restored");
    std::fs::create_dir_all(root.join(".choir")).expect("root");
    let existing = b"{\"this\":\"is someone's log\"}\n";
    std::fs::write(root.join(".choir").join("ops.jsonl"), existing).expect("existing log");

    let (code, out, err) = restore(&backup, &root);
    // The bytes first, and deliberately: this test is about the log, not
    // about an exit code. Asserting the code first passes the *refusal*
    // check on to whatever refuses next — with the clobber guard removed,
    // the run stops later on the missing node key, and an
    // assert-code-first version reports that as this test doing its job
    // while the log has already been overwritten.
    assert_eq!(
        std::fs::read(root.join(".choir").join("ops.jsonl")).expect("read"),
        existing,
        "the existing log must be exactly as it was.\n{out}{err}"
    );
    assert_eq!(code, 1, "{out}{err}");

    std::fs::remove_dir_all(work).ok();
}

/// A repo that arrives as a bundle has no hook, and a repo with no hook
/// accepts pushes the sequencer never sees.
///
/// This is the failure the restore ordering exists to prevent, tested
/// where it lives rather than only through the script: `adopt_repo` is
/// what puts a hookless repo back under the sequencer.
#[test]
fn adopting_an_unbundled_repo_puts_it_back_under_the_sequencer() {
    let work = workdir("adopt");
    let repo = "owner/demo.git";
    let root = work.join("root");
    std::fs::create_dir_all(root.join("owner")).expect("root");

    // A bare repo the way a restore produces one: `git init --bare`
    // stands in for the bundle clone, since what matters is that nothing
    // in this node created it.
    assert!(
        git(
            &work,
            &[
                "init",
                "--bare",
                "--quiet",
                root.join(repo).to_str().expect("utf8")
            ]
        )
        .status
        .success(),
        "init the unbundled repo"
    );
    let hook = root.join(repo).join("hooks").join("pre-receive");
    assert!(!hook.exists(), "a restored repo starts with no hook");

    let node = choir_node::Node::bind(&root, 0).expect("bind");
    node.adopt_repo(repo).expect("adopt");

    let body = std::fs::read_to_string(&hook).expect("the hook is installed");
    assert!(
        body.contains("CHOIR_API"),
        "and it is the sequencer hook: {body}"
    );
    let cfg = git(&root.join(repo), &["config", "--get", "http.receivepack"]);
    assert_eq!(
        String::from_utf8_lossy(&cfg.stdout).trim(),
        "true",
        "and pushes are enabled"
    );

    // Idempotent: the ordinary restart runs this over a healthy repo.
    node.adopt_repo(repo).expect("adopt again");
    let seed_a = git(
        &root.join(repo),
        &["config", "--get", "receive.certNonceSeed"],
    );
    node.adopt_repo(repo).expect("adopt a third time");
    let seed_b = git(
        &root.join(repo),
        &["config", "--get", "receive.certNonceSeed"],
    );
    assert_eq!(
        String::from_utf8_lossy(&seed_a.stdout),
        String::from_utf8_lossy(&seed_b.stdout),
        "re-seeding the nonce would refuse every signed push already in flight"
    );

    std::fs::remove_dir_all(work).ok();
}
