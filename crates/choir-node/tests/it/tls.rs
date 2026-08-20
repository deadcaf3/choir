//! TLS acceptance: the daemon serves git + API over https with a
//! self-signed cert (sequencer hook callback included), and a
//! non-loopback bind without TLS is refused outright — the standing
//! privacy rule enforced in code.

use choir_identity::{ActorKey, Registry};
use choir_node::{Node, Platform};
use choir_oplog::MemLog;

fn git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "tag.gpgsign=false",
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

#[test]
fn non_loopback_without_tls_is_refused() {
    let work = std::env::temp_dir().join(format!("choir-tls-refuse-{}", std::process::id()));
    let err = match Node::bind_full(&work, "0.0.0.0", 0, None, None) {
        Ok(_) => panic!("non-loopback bind without TLS must fail"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("refusing non-loopback"));
    std::fs::remove_dir_all(&work).ok();
}

#[test]
fn git_and_api_work_over_https() {
    let work = std::env::temp_dir().join(format!("choir-tls-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();

    // Self-signed cert for localhost.
    let cert = work.join("cert.pem");
    let key = work.join("key.pem");
    let out = std::process::Command::new("openssl")
        .args([
            "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
        ])
        .args(["-subj", "/CN=localhost"])
        .arg("-keyout")
        .arg(&key)
        .arg("-out")
        .arg(&cert)
        .output()
        .expect("openssl runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let mut node = Node::bind_full(
        &work.join("repos"),
        "127.0.0.1",
        0,
        None,
        Some((std::fs::read(&cert).unwrap(), std::fs::read(&key).unwrap())),
    )
    .unwrap();
    node.enable_platform(
        Platform::start(
            Registry::new(),
            Box::new(MemLog::new()),
            ActorKey::generate(),
        )
        .unwrap(),
    );
    let port = node.port();
    node.create_repo("agents/demo.git").unwrap();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    let url = format!("https://127.0.0.1:{port}/agents/demo.git");

    // Full git roundtrip over https, sequencer hook included.
    let c1 = work.join("clone1");
    assert!(git(&work, &["clone", "-q", &url, c1.to_str().unwrap()])
        .status
        .success());
    std::fs::write(c1.join("f.txt"), "over tls\n").unwrap();
    git(&c1, &["add", "."]);
    git(&c1, &["commit", "-q", "-m", "tls commit"]);
    let out = git(&c1, &["push", "-q", "origin", "HEAD:main"]);
    assert!(
        out.status.success(),
        "push over tls: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The push's ref update reached the sequencer through the https
    // callback, and the API answers over https too.
    let out = std::process::Command::new("curl")
        .args(["-sk", &format!("https://127.0.0.1:{port}/api/view")])
        .output()
        .expect("curl runs");
    let view: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(view["refs"]["agents/demo.git:refs/heads/main"].is_string());

    // The palette cookie is marked `Secure` here and is not on the
    // loopback node, because the flag follows the scheme actually being
    // served (D56). Hardcoding it either way fails silently in one
    // direction: always-on and a plain-HTTP browser drops the cookie so
    // the palette never sticks, with nothing in the response saying so;
    // never-on and a TLS deployment hands a browser a cookie it will
    // send in clear the first time anything reaches the node without
    // TLS. Asserted over a real https listener rather than reasoned
    // about, because "the node knows its own scheme" is the whole claim.
    let out = std::process::Command::new("curl")
        .args([
            "-ski",
            "-o",
            "/dev/null",
            "-D",
            "-",
            &format!("https://127.0.0.1:{port}/theme?set=dark&to=/"),
        ])
        .output()
        .expect("curl runs");
    let headers = String::from_utf8_lossy(&out.stdout);
    let cookie = headers
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("set-cookie:"))
        .unwrap_or_else(|| panic!("no cookie was set over https: {headers}"));
    assert!(
        cookie.contains("Secure"),
        "a cookie set over tls is not marked Secure: {cookie}"
    );
    assert!(cookie.contains("HttpOnly"), "{cookie}");

    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}
