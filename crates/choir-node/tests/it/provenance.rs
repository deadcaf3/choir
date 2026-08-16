//! D41: push provenance is a schema field the node alone may write.
//!
//! Ops the node signs on behalf of a git pusher were distinguishable
//! from author-signed ops only by convention (channel prefix plus the
//! signer happening to be the node). The label makes the class explicit
//! — and the guard makes the label honest: an op claiming push
//! provenance under any other signer is refused, so downstream readers
//! may trust the field instead of re-deriving the convention.

use choir_identity::{ActorKey, Registry};
use choir_node::platform::hex_decode;
use choir_node::{Node, Platform};
use choir_oplog::{ContentHash, MemLog};
use choir_view::{OpKind, Provenance, ViewOp};

use crate::support::{curl, submit_body};

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
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git runs")
}

#[test]
fn a_push_derived_op_carries_its_provenance_in_the_payload() {
    let work = std::env::temp_dir().join(format!("choir-provenance-pos-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();

    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
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

    let clone = work.join("clone");
    let url = format!("http://127.0.0.1:{port}/agents/demo.git");
    assert!(git(&work, &["clone", "-q", &url, clone.to_str().unwrap()])
        .status
        .success());
    std::fs::write(clone.join("f.txt"), "labeled\n").unwrap();
    git(&clone, &["add", "."]);
    git(&clone, &["commit", "-q", "-m", "labeled"]);
    let out = git(&clone, &["push", "-q", "origin", "HEAD:main"]);
    assert!(
        out.status.success(),
        "push: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The push landed as an op; its payload says how it was authored.
    // An uncertified push is the transport class, and the channel and
    // the label must agree about that.
    let (code, log) = curl(&[&format!("http://127.0.0.1:{port}/api/log?from=0")]);
    assert_eq!(code, 200, "{log}");
    let entries = log["entries"].as_array().expect("log entries");
    let push_entry = entries
        .iter()
        .find(|e| {
            e["channel"]
                .as_str()
                .or_else(|| e["workspace"].as_str())
                .is_some_and(|c| c.starts_with("git/"))
        })
        .expect("a git/-channel entry from the push");
    let payload =
        hex_decode(push_entry["payload_hex"].as_str().expect("payload_hex")).expect("hex payload");
    let op = ViewOp::from_payload(&payload).expect("payload decodes");
    assert_eq!(op.provenance, Some(Provenance::PushTransport), "{op:?}");

    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}

#[test]
fn a_claimed_push_provenance_is_refused_from_any_key_but_the_nodes() {
    let work = std::env::temp_dir().join(format!("choir-provenance-neg-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();

    // ana is trusted — this is not an unknown-key refusal. Her op is
    // valid in every way except the class it claims for itself.
    let ana = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&ana.public_key_bytes()).unwrap();
    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate()).unwrap(),
    );
    let port = node.port();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    let api = format!("http://127.0.0.1:{port}/api");

    let dressed = ViewOp::new(OpKind::SetWorkspaceHead {
        workspace: "ana/w".into(),
        commit: ContentHash::blake3(b"c1"),
        prev: None,
    })
    .with_provenance(Provenance::PushCertified);
    let (code, resp) = curl(&[
        "-X",
        "POST",
        "-d",
        &submit_body(&ana, "ana", &dressed),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 400, "{resp}");
    assert_eq!(resp["code"], "node_only", "{resp}");
    assert!(
        resp["error"]
            .as_str()
            .expect("error text")
            .contains("provenance"),
        "{resp}"
    );

    // Undressed, the same op from the same key is admitted: the guard
    // refuses the label, not the author.
    let plain = ViewOp::new(OpKind::SetWorkspaceHead {
        workspace: "ana/w".into(),
        commit: ContentHash::blake3(b"c1"),
        prev: None,
    });
    let (code, resp) = curl(&[
        "-X",
        "POST",
        "-d",
        &submit_body(&ana, "ana", &plain),
        &format!("{api}/submit"),
    ]);
    assert_eq!(code, 200, "{resp}");

    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}
