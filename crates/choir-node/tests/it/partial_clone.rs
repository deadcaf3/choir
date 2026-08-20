//! Filtered fetch and sparse checkout against the daemon (D48).
//!
//! The feature under test is git's, not ours. What is ours is the two
//! lines of repository config that let a client use it, and those are
//! exactly the kind of thing that is set once, never exercised, and
//! discovered missing by a user with a large repository.
//!
//! So this asserts the capability end to end rather than asserting the
//! config values: a filtered clone downloads no blobs, and the checkout
//! afterwards hydrates them over the same transport. The second half is
//! the one that fails when only `uploadpack.allowFilter` is set, because
//! a promisor fetch asks for blob oids by exact id and upload-pack
//! refuses unadvertised oids by default. That failure looks like a
//! successful clone whose first file read errors, which is why it gets
//! its own assertion instead of being folded into the first.

use choir_node::Node;

fn git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "tag.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
            "-c",
            "protocol.version=2",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git runs")
}

fn ok(dir: &std::path::Path, args: &[&str]) -> String {
    let out = git(dir, args);
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A repository the node created serves filtered fetches, and a client
/// that took one can still read a file.
#[test]
fn filtered_clone_hydrates_on_checkout() {
    let work = std::env::temp_dir().join("choir-node-partial-clone-hydrate");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).unwrap();

    let node = Node::bind(&work.join("repos"), 0).unwrap();
    let port = node.port();
    node.create_repo("agents/big.git").unwrap();
    let node = std::sync::Arc::new(node);
    std::thread::spawn(move || node.serve_forever());
    let url = format!("http://127.0.0.1:{port}/agents/big.git");

    // Seed two directories so the cone in the next test has something to
    // exclude, and make the blobs distinctive enough to assert on.
    let seed = work.join("seed");
    ok(&work, &["clone", "-q", &url, seed.to_str().unwrap()]);
    std::fs::create_dir_all(seed.join("services/api")).unwrap();
    std::fs::create_dir_all(seed.join("docs")).unwrap();
    std::fs::write(seed.join("services/api/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(seed.join("docs/guide.md"), "the guide\n").unwrap();
    ok(&seed, &["add", "."]);
    ok(&seed, &["commit", "-q", "-m", "seed"]);
    ok(&seed, &["push", "-q", "origin", "HEAD:main"]);

    // The server advertises the filter. A node that had not been
    // configured for it fails here, and the message is git's own
    // "filtering not recognized by server".
    let lazy = work.join("lazy");
    let out = git(
        &work,
        &[
            "clone",
            "-q",
            "--filter=blob:none",
            "--no-checkout",
            &url,
            lazy.to_str().unwrap(),
        ],
    );
    assert!(
        out.status.success(),
        "filtered clone refused: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // No blob arrived with the clone. `--no-checkout` means nothing has
    // asked for one yet, so this measures the filter and not the
    // checkout that follows.
    let missing = ok(
        &lazy,
        &["rev-list", "--objects", "--all", "--missing=print"],
    );
    assert!(
        missing.lines().any(|l| l.starts_with('?')),
        "filtered clone brought every blob; the filter did nothing:\n{missing}"
    );

    // The hydrating half: this is the assertion that fails when
    // `uploadpack.allowAnySHA1InWant` is missing.
    let out = git(&lazy, &["checkout", "-q", "main"]);
    assert!(
        out.status.success(),
        "promisor hydration failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(lazy.join("services/api/main.rs")).unwrap(),
        "fn main() {}\n"
    );

    let _ = std::fs::remove_dir_all(&work);
}

/// The Perforce-style cone: clone a subtree, widen it, and confirm the
/// out-of-cone file is known but absent from the working tree.
#[test]
fn sparse_cone_scopes_the_working_tree() {
    let work = std::env::temp_dir().join("choir-node-partial-clone-cone");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).unwrap();

    let node = Node::bind(&work.join("repos"), 0).unwrap();
    let port = node.port();
    node.create_repo("agents/mono.git").unwrap();
    let node = std::sync::Arc::new(node);
    std::thread::spawn(move || node.serve_forever());
    let url = format!("http://127.0.0.1:{port}/agents/mono.git");

    let seed = work.join("seed");
    ok(&work, &["clone", "-q", &url, seed.to_str().unwrap()]);
    for (dir, file, body) in [
        ("services/api", "main.rs", "fn main() {}\n"),
        ("libs/shared", "lib.rs", "pub fn shared() {}\n"),
        ("docs", "guide.md", "the guide\n"),
    ] {
        std::fs::create_dir_all(seed.join(dir)).unwrap();
        std::fs::write(seed.join(dir).join(file), body).unwrap();
    }
    ok(&seed, &["add", "."]);
    ok(&seed, &["commit", "-q", "-m", "seed"]);
    ok(&seed, &["push", "-q", "origin", "HEAD:main"]);

    let cone = work.join("cone");
    let out = git(
        &work,
        &[
            "clone",
            "-q",
            "--filter=blob:none",
            "--sparse",
            &url,
            cone.to_str().unwrap(),
        ],
    );
    assert!(
        out.status.success(),
        "sparse clone refused: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    ok(&cone, &["sparse-checkout", "set", "services/api"]);
    assert!(cone.join("services/api/main.rs").is_file());
    assert!(
        !cone.join("docs/guide.md").exists(),
        "out-of-cone file was written to the working tree"
    );

    // Widening pulls the new subtree in without a reclone.
    ok(&cone, &["sparse-checkout", "add", "libs/shared"]);
    assert!(cone.join("libs/shared/lib.rs").is_file());

    // The out-of-cone path is withheld, not gone: git still lists it, so
    // a narrowed client can tell "not mine to read" from "deleted".
    let listed = ok(&cone, &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(
        listed.contains("docs/guide.md"),
        "out-of-cone path vanished from the tree listing:\n{listed}"
    );

    let _ = std::fs::remove_dir_all(&work);
}
