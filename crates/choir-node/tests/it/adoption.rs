//! A repository is served with a `pre-receive` hook or it is not served.
//!
//! Adoption installs that hook, and until this existed adoption only
//! ever happened for repositories named in `--create`. Anything that
//! arrived another way — restored from a bundle, moved in, created
//! against a running node — was listed nowhere, hooked never, and
//! served anyway: every push into it bypassed the sequencer and landed
//! refs no op in the log recorded.
//!
//! That is invariant 5 (only the sequencer appends) and invariant 6 (a
//! conflict is a value, which presumes the ordering saw the write) both,
//! broken by a repository nobody remembered to list. These tests are the
//! regression.

use choir_identity::{ActorKey, Registry};
use choir_node::{Node, Platform};
use choir_oplog::MemLog;

fn work(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "choir-adoption-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
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
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git runs")
}

/// A bare repository placed under the root by something other than the
/// node — which is what restoring a bundle looks like — is adopted, and
/// the hook it arrived without is there afterwards.
#[test]
fn a_repository_nobody_listed_is_adopted_and_hooked() {
    let work = work("unlisted");
    let root = work.join("repos");
    std::fs::create_dir_all(&root).unwrap();

    // Exactly how a restore leaves one: objects and refs, no hook.
    let smuggled = root.join("owner/smuggled.git");
    std::fs::create_dir_all(smuggled.parent().unwrap()).unwrap();
    assert!(
        git(&work, &["init", "--bare", "-q", smuggled.to_str().unwrap()])
            .status
            .success()
    );
    let hook = smuggled.join("hooks/pre-receive");
    assert!(
        !hook.exists(),
        "a bare init leaves no hook; that is the premise"
    );

    let node = Node::bind(&root, 0).unwrap();
    let adopted = node.adopt_existing_repos().expect("adoption succeeds");

    assert_eq!(adopted, 1, "the walk should have found the one repository");
    assert!(
        hook.exists(),
        "an adopted repository has the hook that routes its pushes to the sequencer"
    );
}

/// Adoption is safe to run on every start.
///
/// The value that must survive is `receive.certNonceSeed`: rotating it
/// would refuse the signed pushes already in flight against the old
/// seed, so a restart that reseeded would break exactly the pushes a
/// restart is least able to explain.
#[test]
fn adopting_twice_changes_nothing() {
    let work = work("idempotent");
    let root = work.join("repos");
    let node = Node::bind(&root, 0).unwrap();
    node.create_repo("owner/demo.git").unwrap();

    let seed = |()| -> String {
        let out = std::process::Command::new("git")
            .args(["config", "--get", "receive.certNonceSeed"])
            .current_dir(root.join("owner/demo.git"))
            .output()
            .expect("git config runs");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let before = seed(());
    assert!(!before.is_empty(), "create_repo seeds a nonce");

    assert_eq!(node.adopt_existing_repos().unwrap(), 1);
    assert_eq!(node.adopt_existing_repos().unwrap(), 1);

    assert_eq!(seed(()), before, "the nonce seed must survive a restart");
}

/// The walk counts repositories, not directories.
///
/// A `.git` directory with no `HEAD` is a half-written one, and `.choir`
/// holds the node's own state rather than anything to serve. Adopting
/// either would be adopting something that is not a repository.
#[test]
fn the_walk_ignores_what_is_not_a_repository() {
    let work = work("selective");
    let root = work.join("repos");
    std::fs::create_dir_all(root.join("owner/halfway.git")).unwrap();
    std::fs::create_dir_all(root.join(".choir")).unwrap();
    std::fs::create_dir_all(root.join("owner/notarepo")).unwrap();

    let node = Node::bind(&root, 0).unwrap();
    assert_eq!(
        node.adopt_existing_repos().expect("walk succeeds"),
        0,
        "none of those is a repository"
    );
}

/// A push into a repository that was never listed lands in the log.
///
/// The hook test above proves the file is there; this proves it is
/// wired to the sequencer, which is the property that actually matters.
#[test]
fn a_push_into_an_adopted_repository_is_sequenced() {
    let work = work("sequenced");
    let root = work.join("repos");
    std::fs::create_dir_all(&root).unwrap();
    let smuggled = root.join("agents/restored.git");
    std::fs::create_dir_all(smuggled.parent().unwrap()).unwrap();
    assert!(
        git(&work, &["init", "--bare", "-q", smuggled.to_str().unwrap()])
            .status
            .success()
    );

    let registry = Registry::new();
    let mut node = Node::bind(&root, 0).unwrap();
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate()).unwrap(),
    );
    node.adopt_existing_repos().expect("adoption succeeds");
    let port = node.port();
    let node = std::sync::Arc::new(node);
    std::thread::spawn(move || node.serve_forever());

    let clone = work.join("clone");
    let url = format!("http://127.0.0.1:{port}/agents/restored.git");
    assert!(git(&work, &["clone", "-q", &url, clone.to_str().unwrap()])
        .status
        .success());
    std::fs::write(clone.join("f.txt"), "hi").unwrap();
    assert!(git(&clone, &["add", "-A"]).status.success());
    assert!(git(&clone, &["commit", "-qm", "c1"]).status.success());
    let pushed = git(&clone, &["push", "-q", "origin", "HEAD:refs/heads/main"]);
    assert!(
        pushed.status.success(),
        "push failed: {}",
        String::from_utf8_lossy(&pushed.stderr)
    );

    let out = std::process::Command::new("curl")
        .args(["-s", &format!("http://127.0.0.1:{port}/api/view")])
        .output()
        .expect("curl runs");
    let view: serde_json::Value = serde_json::from_slice(&out.stdout).expect("view json");
    let refs = view["refs"].as_object().expect("refs object");
    assert!(
        refs.keys().any(|k| k.starts_with("agents/restored.git:")),
        "the push must be recorded as an op, not merely accepted by git: {refs:?}"
    );
}

/// A repository can be created on a node that is already serving.
///
/// This is the property the whole endpoint exists for: until it, the
/// only way to add a repository was to name it in `--create` and
/// restart, so a person who had just installed choir could not put a
/// repository on the instance they had just started.
#[test]
fn a_repository_can_be_created_on_a_running_node() {
    let work = work("runtime-create");
    let root = work.join("repos");
    std::fs::create_dir_all(&root).unwrap();

    let registry = Registry::new();
    let mut node = Node::bind(&root, 0).unwrap();
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate()).unwrap(),
    );
    let port = node.port();
    let node = std::sync::Arc::new(node);
    std::thread::spawn(move || node.serve_forever());

    let post = |body: &str| -> (u16, String) {
        let out = std::process::Command::new("curl")
            .args([
                "-s",
                "-X",
                "POST",
                "-d",
                body,
                "-w",
                "\n%{http_code}",
                &format!("http://127.0.0.1:{port}/api/repo"),
            ])
            .output()
            .expect("curl runs");
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        let (body, code) = text.rsplit_once('\n').expect("status on the last line");
        (
            code.trim().parse().expect("numeric status"),
            body.to_string(),
        )
    };

    let (code, _) = post(r#"{"name":"me/thing.git"}"#);
    assert_eq!(code, 201, "a new repository is created");

    // The hook is the whole point: a repository created without one is
    // served while every push into it bypasses the sequencer.
    assert!(
        root.join("me/thing.git/hooks/pre-receive").exists(),
        "a created repository is hooked from its first push"
    );

    // Asking again is not an error worth a 500 — the caller wanted it to
    // exist and it does — but it is not a 201 either, because a caller
    // that would have pushed into an empty one needs to know it is not.
    let (code, _) = post(r#"{"name":"me/thing.git"}"#);
    assert_eq!(
        code, 409,
        "an existing repository is a conflict, not a crash"
    );
}

/// Repository names are checked before anything touches the filesystem.
///
/// `..`, `@` and `*` are refused for the reasons `repo_path` gives:
/// traversal, and aliasing the ACL's `@node` and `*` pseudo-names, which
/// would let a repository be created that an ACL entry silently governs
/// — or silently does not.
#[test]
fn a_created_repository_cannot_escape_or_alias_the_acl() {
    let work = work("names");
    let root = work.join("repos");
    std::fs::create_dir_all(&root).unwrap();

    let registry = Registry::new();
    let mut node = Node::bind(&root, 0).unwrap();
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate()).unwrap(),
    );
    let port = node.port();
    let node = std::sync::Arc::new(node);
    std::thread::spawn(move || node.serve_forever());

    for name in [
        "../escape.git",
        "@node.git",
        "*.git",
        "me/../../escape.git",
        "no-dot-git",
    ] {
        let out = std::process::Command::new("curl")
            .args([
                "-s",
                "-o",
                "/dev/null",
                "-X",
                "POST",
                "-d",
                &format!(r#"{{"name":"{name}"}}"#),
                "-w",
                "%{http_code}",
                &format!("http://127.0.0.1:{port}/api/repo"),
            ])
            .output()
            .expect("curl runs");
        let code = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert_eq!(code, "400", "`{name}` must be refused, not created");
    }
    assert!(
        !work.join("escape.git").exists() && !root.join("@node.git").exists(),
        "nothing was created outside or aliasing the ACL"
    );
}
