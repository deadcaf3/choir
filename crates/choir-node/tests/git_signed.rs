//! Per-actor signed pushes: `git push --signed` sends a push
//! certificate signed with the pusher's ssh-ed25519 key; the daemon
//! verifies it against allowed_signers and attributes the resulting op
//! to `key/<principal>` instead of the transport user.

use choir_identity::{ActorKey, Registry};
use choir_node::{write_allowed_signers, Node, Platform};
use choir_oplog::MemLog;

fn git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args(["-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main"])
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

/// Raw 32 key bytes out of an OpenSSH `ssh-ed25519 <b64>` public line
/// (the blob is two length-prefixed strings; the key is the last 32).
fn raw_from_ssh_pub(line: &str) -> [u8; 32] {
    let b64 = line.split_whitespace().nth(1).expect("key field");
    let mut bytes = Vec::new();
    let table: std::collections::HashMap<u8, u32> =
        (b'A'..=b'Z').chain(b'a'..=b'z').chain(b'0'..=b'9').chain([b'+', b'/'])
            .enumerate()
            .map(|(i, c)| (c, i as u32))
            .collect();
    let mut buf = 0u32;
    let mut bits = 0;
    for c in b64.trim_end_matches('=').bytes() {
        buf = (buf << 6) | table[&c];
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            bytes.push((buf >> bits) as u8);
        }
    }
    bytes[bytes.len() - 32..].try_into().expect("32-byte key")
}

#[test]
fn signed_push_attributes_the_pushers_key() {
    let work = std::env::temp_dir().join(format!("choir-git-signed-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let repos = work.join("repos");

    // The pusher's ssh key, as a real agent would hold it.
    let keyfile = work.join("id_ed25519");
    let out = std::process::Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-C", "agent-alice", "-f"])
        .arg(&keyfile)
        .output()
        .expect("ssh-keygen runs");
    assert!(out.status.success());
    let pub_line = std::fs::read_to_string(work.join("id_ed25519.pub")).unwrap();
    let raw = raw_from_ssh_pub(&pub_line);

    // Server side: key registered, allowed_signers written, repo created.
    let mut registry = Registry::new();
    let principal = registry.register(&raw).unwrap().to_hex();
    std::fs::create_dir_all(&repos).unwrap();
    write_allowed_signers(&repos, &[(principal.clone(), raw)]).unwrap();

    let mut node = Node::bind(&repos, 0).unwrap();
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate()).unwrap(),
    );
    let port = node.port();
    node.create_repo("agents/demo.git").unwrap();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    let url = format!("http://127.0.0.1:{port}/agents/demo.git");

    // Signed push: sign the push certificate with the ssh key.
    let c1 = work.join("clone1");
    assert!(git(&work, &["clone", "-q", &url, c1.to_str().unwrap()])
        .status
        .success());
    std::fs::write(c1.join("f.txt"), "signed\n").unwrap();
    git(&c1, &["add", "."]);
    git(&c1, &["commit", "-q", "-m", "signed work"]);
    let out = git(
        &c1,
        &[
            "-c",
            "gpg.format=ssh",
            "-c",
            &format!("user.signingkey={}", keyfile.display()),
            "push",
            "-q",
            "--signed",
            "origin",
            "HEAD:main",
        ],
    );
    assert!(
        out.status.success(),
        "signed push: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The op is attributed to the pusher's key, not the transport user.
    let out = std::process::Command::new("curl")
        .args(["-s", &format!("http://127.0.0.1:{port}/api/log")])
        .output()
        .expect("curl runs");
    let log: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let entries = log["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    let ws = entries[0]["workspace"].as_str().unwrap();
    assert_eq!(
        ws,
        format!("key/{principal}"),
        "workspace must be the certificate principal"
    );

    // An unsigned push still lands, attributed to the transport user.
    std::fs::write(c1.join("f.txt"), "unsigned\n").unwrap();
    git(&c1, &["add", "."]);
    git(&c1, &["commit", "-q", "-m", "unsigned work"]);
    assert!(git(&c1, &["push", "-q", "origin", "HEAD:main"]).status.success());
    let out = std::process::Command::new("curl")
        .args(["-s", &format!("http://127.0.0.1:{port}/api/log?from=1")])
        .output()
        .expect("curl runs");
    let log: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        log["entries"][0]["workspace"].as_str().unwrap(),
        "git/anon"
    );

    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}
