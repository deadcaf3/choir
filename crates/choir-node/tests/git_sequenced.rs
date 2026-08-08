//! The platform eating git: a real `git push` through the daemon lands
//! as a CAS-checked, node-signed op in the op log, and the view exposes
//! the ref. Branch deletion becomes a DeleteRef op. This is the seam
//! where git semantics and platform semantics become one history.

use choir_identity::{ActorKey, Registry};
use choir_node::{Node, Platform};
use choir_oplog::MemLog;

fn git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false", "-c", "init.defaultBranch=main"])
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

fn view(port: u16) -> serde_json::Value {
    let out = std::process::Command::new("curl")
        .args(["-s", &format!("http://127.0.0.1:{port}/api/view")])
        .output()
        .expect("curl runs");
    serde_json::from_slice(&out.stdout).expect("view json")
}

#[test]
fn git_push_lands_in_the_op_log() {
    let work = std::env::temp_dir().join(format!("choir-git-seq-{}", std::process::id()));
    std::fs::create_dir_all(&work).unwrap();

    let registry = Registry::new();
    let mut node = Node::bind(&work.join("repos"), 0).unwrap();
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

    // Push a commit to main through the daemon.
    let c1 = work.join("clone1");
    assert!(git(&work, &["clone", "-q", &url, c1.to_str().unwrap()])
        .status
        .success());
    std::fs::write(c1.join("f.txt"), "sequenced\n").unwrap();
    git(&c1, &["add", "."]);
    git(&c1, &["commit", "-q", "-m", "first"]);
    let out = git(&c1, &["push", "-q", "origin", "HEAD:main"]);
    assert!(
        out.status.success(),
        "push: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The op log's view holds the ref at git's own oid (sha1 codec 0x11).
    let head = String::from_utf8(git(&c1, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_string();
    let v = view(port);
    let logged = v["refs"]["agents/demo.git:refs/heads/main"]
        .as_str()
        .expect("ref in view");
    assert_eq!(logged, format!("11-{head}"));

    // A second push advances the same ref through CAS.
    std::fs::write(c1.join("f.txt"), "sequenced twice\n").unwrap();
    git(&c1, &["add", "."]);
    git(&c1, &["commit", "-q", "-m", "second"]);
    assert!(git(&c1, &["push", "-q", "origin", "HEAD:main"]).status.success());
    let head2 = String::from_utf8(git(&c1, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_string();
    let v = view(port);
    assert_eq!(
        v["refs"]["agents/demo.git:refs/heads/main"].as_str().unwrap(),
        format!("11-{head2}")
    );

    // Branch create + delete round-trips out of the view.
    assert!(git(&c1, &["push", "-q", "origin", "HEAD:feature"]).status.success());
    assert!(view(port)["refs"]["agents/demo.git:refs/heads/feature"].is_string());
    assert!(git(&c1, &["push", "-q", "origin", ":feature"]).status.success());
    assert!(view(port)["refs"]["agents/demo.git:refs/heads/feature"].is_null());

    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}
