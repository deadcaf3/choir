//! End-to-end smart-HTTP test: real `git` clones from and pushes to the
//! daemon, then a second clone verifies the pushed history (the D20
//! "push/pull works" criterion at daemon level).

use choir_node::Node;

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
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
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn clone_push_clone_roundtrip() {
    let work = std::env::temp_dir().join(format!("choir-node-test-{}", std::process::id()));
    std::fs::create_dir_all(&work).unwrap();

    let node = Node::bind(&work.join("repos"), 0).unwrap();
    let port = node.port();
    node.create_repo("agents/demo.git").unwrap();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    let url = format!("http://127.0.0.1:{port}/agents/demo.git");

    // Clone the empty repo, commit, push.
    let c1 = work.join("clone1");
    git(&work, &["clone", "-q", &url, c1.to_str().unwrap()]);
    std::fs::write(c1.join("hello.txt"), "from the platform\n").unwrap();
    git(&c1, &["add", "."]);
    git(
        &c1,
        &["commit", "-q", "-m", "first commit through choir-node"],
    );
    git(&c1, &["push", "-q", "origin", "HEAD:main"]);

    // Fresh clone sees the pushed commit.
    let c2 = work.join("clone2");
    git(&work, &["clone", "-q", &url, c2.to_str().unwrap()]);
    let log = git(&c2, &["log", "--oneline"]);
    assert!(log.contains("first commit through choir-node"));
    let content = std::fs::read_to_string(c2.join("hello.txt")).unwrap();
    assert_eq!(content, "from the platform\n");

    // Path traversal is rejected.
    assert!(node.create_repo("../escape.git").is_err());

    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}
