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

/// A clone that has to infer the default branch gets one.
///
/// `git init --bare` takes `HEAD` from the host's `init.defaultBranch`,
/// so a node on a host that says `master` while every push says `main`
/// advertises a default branch that never comes into existence. git then
/// behaves in two ways: a plain `git clone` fetches every ref and
/// guesses, so it works, while `--single-branch` and `--depth` ask for
/// `HEAD`'s branch, are told nothing, and report an empty repository.
/// The suite only ever did the first, which is why a node served broken
/// clone URLs to strangers with every test green.
///
/// All three halves are the property: the branch is pinned at creation
/// so the host's config decides nothing, a repository whose `HEAD` is
/// already wrong repairs itself on the next fetch, and the clone that
/// could not infer a branch can.
#[test]
fn a_clone_that_must_infer_the_default_branch_gets_one() {
    let work = std::env::temp_dir().join(format!("choir-node-head-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();

    let root = work.join("repos");
    let node = Node::bind(&root, 0).unwrap();
    let port = node.port();
    node.create_repo("agents/demo.git").unwrap();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }
    let url = format!("http://127.0.0.1:{port}/agents/demo.git");
    let head_file = root.join("agents/demo.git/HEAD");

    // Pinned at creation, whatever this host's `init.defaultBranch` says.
    let head = std::fs::read_to_string(&head_file).expect("HEAD is a file");
    assert_eq!(
        head.trim(),
        "ref: refs/heads/main",
        "the node took its default branch from the host's git config"
    );

    let seed = work.join("seed");
    git(&work, &["clone", "-q", &url, seed.to_str().unwrap()]);
    std::fs::write(seed.join("f.txt"), "content\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "only commit"]);
    git(&seed, &["push", "-q", "origin", "HEAD:main"]);

    // The clone every shallow checkout and most CI configurations make.
    let single = work.join("single");
    git(
        &work,
        &[
            "clone",
            "-q",
            "--single-branch",
            &url,
            single.to_str().unwrap(),
        ],
    );
    assert!(
        single.join("f.txt").exists(),
        "a --single-branch clone fetched nothing, so no default branch was advertised"
    );

    // A repository whose HEAD is already wrong repairs itself, because
    // the ones on disk were created before the line above existed.
    std::fs::write(&head_file, "ref: refs/heads/nobody-made-this\n").unwrap();
    let healed = work.join("healed");
    git(
        &work,
        &[
            "clone",
            "-q",
            "--single-branch",
            &url,
            healed.to_str().unwrap(),
        ],
    );
    assert!(
        healed.join("f.txt").exists(),
        "a repository with an unborn HEAD did not repair itself on fetch"
    );
    assert_eq!(
        std::fs::read_to_string(&head_file).unwrap().trim(),
        "ref: refs/heads/main",
        "HEAD was not repointed at a branch that exists"
    );

    node.unblock();
    std::fs::remove_dir_all(&work).ok();
}
