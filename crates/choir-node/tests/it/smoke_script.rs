//! `scripts/smoke_private_beta.sh` against a real node over real TLS.
//!
//! The script is what issues receipt 3, so the thing worth asserting is
//! not that it passes on a healthy node - it is that it *fails* on an
//! unhealthy one. Until this test existed the script only ever sent
//! credentials, so a node serving its whole API anonymously passed every
//! check it made.

use choir_identity::{ActorKey, Registry};
use choir_node::{AuthTable, Node, Platform};

/// A TLS node, its https base, and the cert curl has to be told to trust.
struct Beta {
    base: String,
    work: std::path::PathBuf,
    cert: std::path::PathBuf,
    auth_file: std::path::PathBuf,
}

/// Binds a TLS node, optionally with an auth table, and seeds one repo
/// with a commit so the script's clone has something to fetch.
fn beta(tag: &str, authenticated: bool) -> Beta {
    let work = std::env::temp_dir().join(format!("choir-smoke-{tag}-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");

    let cert = work.join("cert.pem");
    let key = work.join("key.pem");
    let out = std::process::Command::new("openssl")
        .args([
            "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
        ])
        .args(["-subj", "/CN=localhost"])
        .args(["-addext", "subjectAltName=IP:127.0.0.1,DNS:localhost"])
        .arg("-keyout")
        .arg(&key)
        .arg("-out")
        .arg(&cert)
        .output()
        .expect("openssl runs");
    assert!(
        out.status.success(),
        "openssl: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let auth = authenticated.then(|| {
        let mut table = AuthTable::new();
        table.insert("alice".into(), "a".into());
        table
    });
    // A real log on disk, not a `MemLog`: readiness verifies the log
    // file, and a beta node has one.
    let state = work.join("repos/.choir");
    std::fs::create_dir_all(&state).expect("state directory");
    let log = choir_oplog::FileLog::open(&state.join("ops.jsonl")).expect("log opens");

    let mut node = Node::bind_full(
        &work.join("repos"),
        "127.0.0.1",
        0,
        auth,
        Some((
            std::fs::read(&cert).expect("cert"),
            std::fs::read(&key).expect("key"),
        )),
    )
    .expect("node binds");
    node.enable_platform(
        Platform::start(Registry::new(), Box::new(log), ActorKey::generate())
            .expect("platform starts"),
    );
    let port = node.port();
    node.create_repo("agents/demo.git").expect("repo created");
    if authenticated {
        let acl = work.join("acl");
        std::fs::write(&acl, "alice agents/demo.git write\nalice @node auditor\n")
            .expect("acl file");
        node.watch_acl_file(acl).expect("acl loads");
    }
    std::thread::spawn(move || node.serve_forever());

    let auth_file = work.join("auth");
    std::fs::write(&auth_file, "alice:a\n").expect("auth file");

    let base = format!("https://127.0.0.1:{port}");
    // The unauthenticated node never gets past the script's first probe,
    // so there is nothing for a clone to fetch and no point seeding it.
    if authenticated {
        seed(&work, &base);
    }
    Beta {
        base,
        work,
        cert,
        auth_file,
    }
}

/// Pushes one commit, so `git clone` inside the script is not cloning an
/// empty repository.
fn seed(work: &std::path::Path, base: &str) {
    let src = work.join("seed");
    std::fs::create_dir_all(&src).expect("seed dir");
    let url = format!(
        "{}/agents/demo.git",
        base.replace("https://", "https://alice:a@")
    );
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.name", "t"],
        vec!["config", "user.email", "t@t"],
    ] {
        assert!(git(&src, &args).status.success(), "git {args:?}");
    }
    std::fs::write(src.join("f.txt"), "seed\n").expect("seed file");
    assert!(git(&src, &["add", "."]).status.success());
    assert!(git(&src, &["commit", "-q", "-m", "seed"]).status.success());
    let out = git(&src, &["push", "-q", &url, "HEAD:main"]);
    assert!(
        out.status.success(),
        "seed push: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_SSL_NO_VERIFY", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git runs")
}

/// Runs the shipped script, returning whether it passed and everything
/// it said. `CURL_CA_BUNDLE` is how the self-signed cert is trusted
/// without the script needing a test-only flag.
fn smoke(node: &Beta, extra: &[&str]) -> (bool, String) {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/smoke_private_beta.sh");
    let out = std::process::Command::new("sh")
        .arg(script)
        .arg(&node.base)
        .arg(&node.auth_file)
        .arg("agents/demo.git")
        .args(extra)
        .current_dir(&node.work)
        .env("CURL_CA_BUNDLE", &node.cert)
        .env("GIT_SSL_NO_VERIFY", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("sh runs");
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), said)
}

#[test]
fn the_smoke_script_passes_against_a_private_beta_node() {
    let node = beta("ok", true);
    let (passed, said) = smoke(&node, &[]);
    assert!(passed, "{said}");
    assert!(
        said.contains("anonymous refusal"),
        "the receipt must name what it checked: {said}"
    );
}

/// The regression the anonymous probes exist for. A node that serves its
/// API to anybody is the failure receipt 2 and the whole loopback/TLS
/// posture exist to prevent, and the script used to certify it clean.
#[test]
fn a_node_serving_anonymously_fails_the_smoke_script() {
    let node = beta("anon", false);
    let (passed, said) = smoke(&node, &[]);
    assert!(
        !passed,
        "a node with authentication off passed the smoke script: {said}"
    );
    assert!(
        said.contains("expected 401"),
        "the refusal must name the anonymous probe that let it through: {said}"
    );
}

/// `--denied` is the only half of the ACL the script can see from
/// outside: the allowed half is the clone it already does.
#[test]
fn a_repository_the_credential_cannot_reach_is_a_receipt_the_script_can_issue() {
    let node = beta("denied", true);
    let (passed, said) = smoke(&node, &["--denied", "other/secret.git"]);
    assert!(passed, "{said}");
    assert!(
        said.contains("denied ACL access passed"),
        "the receipt must say the denied probe ran: {said}"
    );
}
