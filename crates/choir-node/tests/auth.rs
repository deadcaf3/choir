//! Auth acceptance: with an [`choir_node::AuthTable`] configured, a real
//! git client succeeds with the right token and gets 401 with a wrong or
//! missing one — before any git plumbing runs.

use choir_node::{AuthTable, Node};

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

#[test]
fn auth_gates_clone_and_push() {
    let work = std::env::temp_dir().join(format!("choir-node-auth-{}", std::process::id()));
    std::fs::create_dir_all(&work).unwrap();

    let mut table = AuthTable::new();
    table.insert("alice".into(), "sekrit-token".into());

    let node = Node::bind_with_auth(&work.join("repos"), 0, Some(table)).unwrap();
    let port = node.port();
    node.create_repo("agents/demo.git").unwrap();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }

    // No credentials: rejected.
    let anon = format!("http://127.0.0.1:{port}/agents/demo.git");
    let out = git(&work, &["clone", "-q", &anon, "anon"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    // git reacts to the 401 challenge by trying to collect credentials;
    // with prompts disabled that surfaces as "could not read Username".
    assert!(
        err.contains("401") || err.contains("Authentication") || err.contains("Username"),
        "{err}"
    );

    // Wrong token: rejected.
    let bad = format!("http://alice:wrong@127.0.0.1:{port}/agents/demo.git");
    let out = git(&work, &["clone", "-q", &bad, "bad"]);
    assert!(!out.status.success());

    // Right token: full clone -> push -> clone roundtrip works.
    let good = format!("http://alice:sekrit-token@127.0.0.1:{port}/agents/demo.git");
    let c1 = work.join("clone1");
    assert!(git(&work, &["clone", "-q", &good, c1.to_str().unwrap()])
        .status
        .success());
    std::fs::write(c1.join("f.txt"), "authed\n").unwrap();
    assert!(git(&c1, &["add", "."]).status.success());
    assert!(git(&c1, &["commit", "-q", "-m", "authed commit"]).status.success());
    assert!(git(&c1, &["push", "-q", "origin", "HEAD:main"]).status.success());
    let c2 = work.join("clone2");
    assert!(git(&work, &["clone", "-q", &good, c2.to_str().unwrap()])
        .status
        .success());
    assert_eq!(
        std::fs::read_to_string(c2.join("f.txt")).unwrap(),
        "authed\n"
    );

    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}
