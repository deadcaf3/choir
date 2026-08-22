//! Git over SSH (D31): the host's sshd, a forced command, and the
//! `choir-ssh` shim.
//!
//! The property under test is not "ssh works". It is that a push arriving
//! over SSH joins the same total order an HTTPS push joins — the
//! repository's `pre-receive` hook fires, the sequencer records the ref,
//! and the view shows it under the same `<repo>.git:<refname>` name the
//! HTTP path produces. A transport that skipped that would be a second,
//! unsequenced way in, which is the one thing this feature must not be.
//!
//! The first test drives a real `sshd` on an ephemeral port from a temp
//! directory, with real `git` at both ends. Where no `sshd` binary
//! exists the test says so on stderr and returns; the shim's own
//! decisions are covered without it by the tests below, which invoke the
//! forced command exactly as sshd would.

use choir_identity::{ActorKey, Registry};
use choir_node::{Node, Platform};
use choir_oplog::MemLog;

/// The shim under test, as cargo built it for this run.
const SHIM: &str = env!("CARGO_BIN_EXE_choir-ssh");

fn git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    git_env(dir, args, &[])
}

fn git_env(dir: &std::path::Path, args: &[&str], env: &[(&str, &str)]) -> std::process::Output {
    let mut command = std::process::Command::new("git");
    command
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
        .env("GIT_COMMITTER_EMAIL", "t@t");
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().expect("git runs")
}

fn view(port: u16) -> serde_json::Value {
    let out = std::process::Command::new("curl")
        .args(["-s", &format!("http://127.0.0.1:{port}/api/view")])
        .output()
        .expect("curl runs");
    serde_json::from_slice(&out.stdout).expect("view json")
}

/// A directory nothing else in this harness uses. Tests share a process,
/// so the name carries the test's own label rather than just the pid.
fn workdir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("choir-ssh-{label}-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// A node with the platform enabled, serving `agents/demo.git`, plus the
/// handoff file the shim needs to reach its sequencer.
fn node_with_repo(work: &std::path::Path) -> (std::sync::Arc<Node>, u16, std::path::PathBuf) {
    let mut node = Node::bind(&work.join("repos"), 0).expect("bind");
    node.enable_platform(
        Platform::start(
            Registry::new(),
            Box::new(MemLog::new()),
            ActorKey::generate(),
        )
        .expect("platform"),
    );
    let port = node.port();
    node.create_repo("agents/demo.git").expect("create repo");
    let handoff = work.join("handoff");
    node.write_ssh_handoff(&handoff).expect("handoff");
    let node = std::sync::Arc::new(node);
    {
        let node = std::sync::Arc::clone(&node);
        std::thread::spawn(move || node.serve_forever());
    }
    (node, port, handoff)
}

/// Runs the shim the way sshd runs it: flags from the forced command,
/// the client's request in `SSH_ORIGINAL_COMMAND`.
///
/// Stdin carries a lone flush packet, which is what a git client sends
/// when it has read the ref advertisement and wants nothing further. It
/// is how `upload-pack` is told to stop without an error, so a shim that
/// really reached git exits zero here. A shim that refused the request
/// exited before reading anything, and the unwritten packet is expected.
fn shim(args: &[&str], original: &str) -> std::process::Output {
    let mut child = std::process::Command::new(SHIM)
        .args(args)
        .env("SSH_ORIGINAL_COMMAND", original)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("shim runs");
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().expect("stdin piped");
        stdin.write_all(b"0000").ok();
    }
    child.wait_with_output().expect("shim exits")
}

/// A port nothing is listening on, by binding one and letting it go.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("probe port");
    listener.local_addr().expect("addr").port()
}

/// A running sshd, killed when this value is dropped.
struct Sshd {
    child: std::process::Child,
    port: u16,
    key: std::path::PathBuf,
}

impl Drop for Sshd {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

/// Starts a private sshd in `dir` whose only key is authorized to run
/// `forced_command`. `None` when this host has no sshd to run.
fn start_sshd(dir: &std::path::Path, forced_command: &str) -> Option<Sshd> {
    let sshd = std::path::Path::new("/usr/sbin/sshd");
    if !sshd.exists() {
        return None;
    }
    let keygen = |name: &str| {
        let path = dir.join(name);
        let out = std::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", "choir-test", "-f"])
            .arg(&path)
            .output()
            .expect("ssh-keygen runs");
        assert!(out.status.success(), "ssh-keygen: {out:?}");
        path
    };
    let host_key = keygen("host_key");
    let client_key = keygen("client_key");
    let authorized = dir.join("authorized_keys");
    let public = std::fs::read_to_string(dir.join("client_key.pub")).expect("public key");
    std::fs::write(
        &authorized,
        format!("command=\"{forced_command}\",restrict {public}"),
    )
    .expect("authorized_keys");
    let config = dir.join("sshd_config");
    std::fs::write(
        &config,
        format!(
            "ListenAddress 127.0.0.1\n\
             HostKey {}\n\
             AuthorizedKeysFile {}\n\
             StrictModes no\n\
             UsePAM no\n\
             PasswordAuthentication no\n\
             KbdInteractiveAuthentication no\n\
             PermitUserEnvironment no\n",
            host_key.display(),
            authorized.display(),
        ),
    )
    .expect("sshd_config");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in [&host_key, &authorized] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        }
    }

    // sshd has no ephemeral-port mode, so a probed port plus a retry is
    // the closest thing to one. Retries also cover losing the probed port
    // to something else between the probe and the bind.
    for _ in 0..5 {
        let port = free_port();
        let log = std::fs::File::create(dir.join("sshd.log")).expect("sshd log");
        let Ok(child) = std::process::Command::new(sshd)
            .arg("-D")
            .arg("-e")
            .args(["-f".as_ref(), config.as_os_str()])
            .args(["-p", &port.to_string()])
            .stderr(std::process::Stdio::from(log))
            .spawn()
        else {
            return None;
        };
        let mut sshd = Sshd {
            child,
            port,
            key: client_key.clone(),
        };
        for _ in 0..100 {
            if sshd.child.try_wait().expect("wait").is_some() {
                break;
            }
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return Some(sshd);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        drop(sshd);
    }
    None
}

impl Sshd {
    /// What git should use to reach this daemon. No username: the test
    /// authenticates as whoever is running it, which keeps a real account
    /// name out of both the repository and the assertions.
    fn git_ssh_command(&self) -> String {
        format!(
            "ssh -i {} -o IdentitiesOnly=yes -o StrictHostKeyChecking=no \
             -o UserKnownHostsFile=/dev/null -o BatchMode=yes",
            self.key.display()
        )
    }

    fn url(&self, repo: &str) -> String {
        format!("ssh://127.0.0.1:{}/{repo}", self.port)
    }
}

/// The whole point: a push over SSH is sequenced exactly like a push over
/// HTTP, under the same ref name, and a fetch over SSH sees it.
#[test]
fn an_ssh_push_lands_in_the_op_log() {
    let work = workdir("e2e");
    let (node, port, handoff) = node_with_repo(&work);
    let forced = format!(
        "{SHIM} --root {} --user alice --handoff {} --git-binary {}",
        work.join("repos").display(),
        handoff.display(),
        git_binary().display(),
    );
    let Some(sshd) = start_sshd(&work, &forced) else {
        eprintln!(
            "ssh::an_ssh_push_lands_in_the_op_log: no usable sshd on this host, skipped. \
             The shim's own decisions are still covered by the other tests in this module."
        );
        node.unblock();
        return;
    };

    let clone = work.join("clone");
    let out = git_env(
        &work,
        &[
            "clone",
            "-q",
            &sshd.url("agents/demo.git"),
            clone.to_str().unwrap(),
        ],
        &[("GIT_SSH_COMMAND", &sshd.git_ssh_command())],
    );
    assert!(
        out.status.success(),
        "clone over ssh: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    std::fs::write(clone.join("f.txt"), "over ssh\n").unwrap();
    git(&clone, &["add", "."]);
    git(&clone, &["commit", "-q", "-m", "first"]);
    let out = git_env(
        &clone,
        &["push", "-q", "origin", "HEAD:main"],
        &[("GIT_SSH_COMMAND", &sshd.git_ssh_command())],
    );
    assert!(
        out.status.success(),
        "push over ssh: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The hook fired, so the ref is in the log under the same name an
    // HTTP push would have produced.
    let head = String::from_utf8(git(&clone, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_string();
    let v = view(port);
    assert_eq!(
        v["refs"]["agents/demo.git:refs/heads/main"].as_str(),
        Some(format!("11-{head}").as_str()),
        "an ssh push must be sequenced under the same ref name as an http push; \
         the view's refs are {}",
        v["refs"]
    );

    // And the same transport reads it back.
    let out = git_env(
        &work,
        &["ls-remote", &sshd.url("agents/demo.git"), "refs/heads/main"],
        &[("GIT_SSH_COMMAND", &sshd.git_ssh_command())],
    );
    assert!(out.status.success(), "ls-remote: {out:?}");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(&head),
        "fetch over ssh sees the pushed head"
    );

    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}

/// An ACL that grants read and not write refuses the push, and the log
/// stays empty — the denial happens before git is reached, so there is
/// nothing to un-sequence.
#[test]
fn the_acl_gates_ssh_the_way_it_gates_http() {
    let work = workdir("acl");
    let (node, port, handoff) = node_with_repo(&work);
    let acl = work.join("acl");
    std::fs::write(&acl, "reader agents/demo read\nwriter agents/demo write\n").unwrap();
    let root = work.join("repos");
    let flags = |user: &str| {
        vec![
            "--root".to_string(),
            root.display().to_string(),
            "--user".to_string(),
            user.to_string(),
            "--acl-file".to_string(),
            acl.display().to_string(),
            "--handoff".to_string(),
            handoff.display().to_string(),
        ]
    };
    let run = |user: &str, command: &str| {
        let flags = flags(user);
        let args: Vec<&str> = flags.iter().map(String::as_str).collect();
        shim(&args, command)
    };

    let denied = run("reader", "git-receive-pack 'agents/demo.git'");
    assert!(!denied.status.success(), "a read grant must not push");
    let message = String::from_utf8_lossy(&denied.stderr).to_string();
    assert!(
        message.contains("no write grant"),
        "message was {message:?}"
    );

    // A user with no grant at all learns nothing about the repository.
    let unknown = run("stranger", "git-upload-pack 'agents/demo.git'");
    assert!(
        !unknown.status.success(),
        "an ungranted user must not fetch"
    );
    assert!(
        String::from_utf8_lossy(&unknown.stderr).contains("no such repository"),
        "an ungranted repository must not be confirmed to exist"
    );

    // Nor does a repository that is not there, told apart from the above
    // by nothing.
    let absent = run("writer", "git-upload-pack 'agents/absent.git'");
    assert!(!absent.status.success());
    assert!(String::from_utf8_lossy(&absent.stderr).contains("no such repository"));

    // The granted read reaches real git: `upload-pack` on a closed stdin
    // advertises the refs and stops.
    let allowed = run("reader", "git-upload-pack 'agents/demo.git'");
    assert!(
        allowed.status.success(),
        "a read grant must fetch: {}",
        String::from_utf8_lossy(&allowed.stderr)
    );
    // The repository is empty, so the advertisement is a capability line
    // and no refs. Its presence is the receipt that real git ran; a
    // refusal produces no stdout at all.
    assert!(
        String::from_utf8_lossy(&allowed.stdout).contains("capabilities"),
        "upload-pack advertises capabilities: {:?}",
        String::from_utf8_lossy(&allowed.stdout)
    );

    assert!(view(port)["refs"]
        .as_object()
        .is_none_or(|refs| refs.is_empty()));
    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}

/// A deadline is enforced on this transport too (D66).
///
/// It gets its own test because the ssh shim is a *third* reader of the
/// ACL, after the HTTP request path and the ownership gate. The compiler
/// found it when `allows` moved off the undated type; no list of
/// enforcement points had it, and nothing else here would notice if it
/// stopped dating its table. The deadlines are absolute -- 2001 and 2033
/// -- so nothing sleeps and nothing depends on when this runs.
#[test]
fn a_lapsed_grant_does_not_push_over_ssh() {
    let work = workdir("acl-deadline");
    let (node, _port, handoff) = node_with_repo(&work);
    let acl = work.join("acl");
    std::fs::write(
        &acl,
        "lapsed agents/demo read\n\
         lapsed agents/demo write until=1000000000\n\
         live   agents/demo write until=2000000000\n",
    )
    .unwrap();
    let root = work.join("repos");
    let run = |user: &str, command: &str| {
        let flags = [
            "--root".to_string(),
            root.display().to_string(),
            "--user".to_string(),
            user.to_string(),
            "--acl-file".to_string(),
            acl.display().to_string(),
            "--handoff".to_string(),
            handoff.display().to_string(),
        ];
        let args: Vec<&str> = flags.iter().map(String::as_str).collect();
        shim(&args, command)
    };

    let denied = run("lapsed", "git-receive-pack 'agents/demo.git'");
    assert!(
        !denied.status.success(),
        "a write grant that expired in 2001 pushed over ssh"
    );
    assert!(
        String::from_utf8_lossy(&denied.stderr).contains("no write grant"),
        "the refusal was not the ACL's: {:?}",
        String::from_utf8_lossy(&denied.stderr)
    );

    // The permanent read underneath it survives, which is the difference
    // between lending a privilege and deleting an account.
    let allowed = run("lapsed", "git-upload-pack 'agents/demo.git'");
    assert!(
        allowed.status.success(),
        "the permanent read went with the lapsed write: {:?}",
        String::from_utf8_lossy(&allowed.stderr)
    );

    // A deadline still ahead is an ordinary write grant. Asserted as
    // "not refused by the ACL" rather than as success, because what
    // `receive-pack` does on a closed stdin is git's business.
    let live = run("live", "git-receive-pack 'agents/demo.git'");
    assert!(
        !String::from_utf8_lossy(&live.stderr).contains("no write grant"),
        "a deadline in 2033 refused a push: {:?}",
        String::from_utf8_lossy(&live.stderr)
    );

    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}

/// A forced command that forgot `--acl-file` still enforces the ACL,
/// because the daemon named it in the handoff. The alternative is one
/// mistyped `authorized_keys` line quietly turning into a key that
/// reaches every repository on an authorized node.
#[test]
fn a_forced_command_without_the_acl_flag_inherits_the_daemon_s() {
    let work = workdir("inherit");
    let acl = work.join("acl");
    std::fs::write(&acl, "reader agents/demo read\n").unwrap();

    let mut node = Node::bind(&work.join("repos"), 0).expect("bind");
    node.enable_platform(
        Platform::start(
            Registry::new(),
            Box::new(MemLog::new()),
            ActorKey::generate(),
        )
        .expect("platform"),
    );
    node.create_repo("agents/demo.git").expect("create repo");
    node.watch_acl_file(acl).expect("acl loads");
    let handoff = work.join("handoff");
    node.write_ssh_handoff(&handoff).expect("handoff");
    let node = std::sync::Arc::new(node);
    {
        let node = std::sync::Arc::clone(&node);
        std::thread::spawn(move || node.serve_forever());
    }

    let root = work.join("repos");
    let base = [
        "--root".to_string(),
        root.display().to_string(),
        "--user".to_string(),
        "reader".to_string(),
        "--handoff".to_string(),
        handoff.display().to_string(),
    ];
    let args: Vec<&str> = base.iter().map(String::as_str).collect();

    let push = shim(&args, "git-receive-pack 'agents/demo.git'");
    assert!(!push.status.success(), "a read grant must not push");
    assert!(
        String::from_utf8_lossy(&push.stderr).contains("no write grant"),
        "the inherited table must be the one that refused: {:?}",
        String::from_utf8_lossy(&push.stderr)
    );
    // And the grant it does hold still works, so inheriting the table is
    // not the same as refusing everything.
    assert!(
        shim(&args, "git-upload-pack 'agents/demo.git'")
            .status
            .success(),
        "a read grant must still fetch"
    );

    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}

/// Everything the shim refuses before it would run anything, from the
/// forced command's own point of view.
#[test]
fn the_shim_refuses_what_is_not_a_git_request() {
    let work = workdir("refuse");
    let (node, _port, handoff) = node_with_repo(&work);
    let root = work.join("repos");
    let base = [
        "--root".to_string(),
        root.display().to_string(),
        "--user".to_string(),
        "alice".to_string(),
        "--handoff".to_string(),
        handoff.display().to_string(),
    ];
    let args: Vec<&str> = base.iter().map(String::as_str).collect();

    // Each case carries the reason it must be refused for. Asserting
    // only the exit status would pass on the wrong refusal: every path
    // below also fails to exist, so "no such repository" would have
    // covered a canonicalizer that had stopped checking anything.
    for (command, because) in [
        // Injection: the argument is one repository or it is nothing.
        (
            "git-upload-pack 'agents/demo.git'; touch /tmp/choir-ssh-pwned",
            "not allowed in a git command",
        ),
        (
            "git-upload-pack 'agents/demo.git' 'agents/demo.git'",
            "exactly one repository",
        ),
        ("git-upload-pack `id`", "not allowed in a git command"),
        // Traversal, and the node's own state directory.
        ("git-upload-pack '../../etc/passwd'", "is not a repository"),
        (
            "git-upload-pack 'agents/../../repos/agents/demo.git'",
            "is not a repository",
        ),
        (
            "git-upload-pack '.choir/node.key'",
            "may not start with a dot",
        ),
        ("git-upload-pack 'agents/.git'", "may not start with a dot"),
        // Services this account does not serve.
        ("git-upload-archive 'agents/demo.git'", "is not served here"),
        ("sh -c id", "is not served here"),
        ("scp -t /tmp", "is not served here"),
    ] {
        let out = shim(&args, command);
        assert!(!out.status.success(), "shim accepted {command:?}");
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        assert!(
            stderr.contains(because),
            "{command:?} was refused for the wrong reason: {stderr:?}"
        );
    }
    assert!(
        !std::path::Path::new("/tmp/choir-ssh-pwned").exists(),
        "the injected command must never have run"
    );

    // No SSH_ORIGINAL_COMMAND at all is someone asking for a shell.
    let interactive = std::process::Command::new(SHIM)
        .args(&args)
        .env_remove("SSH_ORIGINAL_COMMAND")
        .output()
        .expect("shim runs");
    assert!(!interactive.status.success());
    assert!(String::from_utf8_lossy(&interactive.stderr).contains("no shell"));

    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}

/// A shim installed without `--handoff` serves fetches and refuses
/// pushes. The alternative is a push that never reaches the sequencer,
/// which is the failure this whole transport exists to avoid.
#[test]
fn a_push_with_nowhere_to_report_is_refused() {
    let work = workdir("nohandoff");
    let (node, _port, _handoff) = node_with_repo(&work);
    let root = work.join("repos");
    let base = [
        "--root".to_string(),
        root.display().to_string(),
        "--user".to_string(),
        "alice".to_string(),
    ];
    let args: Vec<&str> = base.iter().map(String::as_str).collect();

    let push = shim(&args, "git-receive-pack 'agents/demo.git'");
    assert!(!push.status.success());
    assert!(
        String::from_utf8_lossy(&push.stderr).contains("bypass the sequencer"),
        "stderr was {:?}",
        String::from_utf8_lossy(&push.stderr)
    );

    let fetch = shim(&args, "git-upload-pack 'agents/demo.git'");
    assert!(fetch.status.success(), "fetches need no handoff");

    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}

/// git as this test found it, so the shim under sshd's stripped `PATH`
/// runs the same binary the test does.
fn git_binary() -> std::path::PathBuf {
    let out = std::process::Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .expect("command -v runs");
    std::path::PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
