//! A repository the ACL publishes, read by somebody with no credential.
//!
//! The node's wall is D74's: every browse route is behind the auth gate,
//! and a stranger meets the sign-in page. That is right for a private
//! beta and wrong for source that is already public elsewhere, and the
//! gap it left was total -- a node serving an open-source project could
//! show a reader nothing at all without also issuing them an account.
//!
//! What closes it is one reserved principal rather than a second
//! authorization rule. `@anon` is a name no account can be spelled as,
//! granted `read` on repositories somebody named, and an unauthenticated
//! request becomes that principal instead of a `401`. Every check after
//! the gate is the one that was already there.
//!
//! So the tests that matter here are the ones about what *did not*
//! change: a repository nobody published, the op log, and the push half
//! of git are each still refused, and refused for the same reason as
//! before rather than by a new rule that happens to agree today.

use choir_identity::{ActorKey, Registry};
use choir_node::{AuthTable, Node, Platform};
use choir_oplog::MemLog;

struct Served {
    base: String,
    work: std::path::PathBuf,
}

/// The status of a `curl` with no credentials anywhere.
fn anon(url: &str) -> u16 {
    let out = std::process::Command::new("curl")
        .args(["-s", "-o", "/dev/null", "-w", "%{http_code}"])
        .arg(url)
        .output()
        .expect("curl runs");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("numeric status")
}

fn git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        // No credential helper and no terminal: a route that asks for a
        // username fails here rather than hanging, which is what makes
        // "the push was refused" an assertion instead of a timeout.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git runs")
}

/// A node holding one published repository and one that is not, with a
/// commit in each so a clone has something to fetch.
fn served(tag: &str, acl: &str) -> Served {
    let work = std::env::temp_dir().join(format!("choir-node-public-read-{tag}"));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");
    let acl_path = work.join("acl");

    let platform = Platform::start(
        Registry::new(),
        Box::new(MemLog::new()),
        ActorKey::generate(),
    )
    .expect("platform starts");

    let mut table = AuthTable::new();
    table.insert("alice".into(), "a".into());

    let mut node =
        Node::bind_with_auth(&work.join("repos"), 0, Some(table)).expect("node binds free port");
    let port = node.port();
    node.create_repo("open/source.git").expect("repo created");
    node.create_repo("closed/thing.git").expect("repo created");
    std::fs::write(&acl_path, acl).expect("acl file");
    node.watch_acl_file(acl_path).expect("acl loads");
    node.enable_platform(platform);
    std::thread::spawn(move || node.serve_forever());

    let base = format!("http://127.0.0.1:{port}");
    for repo in ["open/source.git", "closed/thing.git"] {
        let seed = work.join(format!("seed-{}", repo.replace('/', "-")));
        std::fs::create_dir_all(&seed).expect("seed dir");
        assert!(git(&seed, &["init", "-q", "."]).status.success());
        std::fs::write(seed.join("f.txt"), "content\n").expect("file");
        git(&seed, &["config", "user.email", "a@b.invalid"]);
        git(&seed, &["config", "user.name", "a"]);
        git(&seed, &["add", "f.txt"]);
        git(&seed, &["commit", "-q", "-m", "first"]);
        let url = format!("http://alice:a@127.0.0.1:{port}/{repo}");
        assert!(
            git(&seed, &["push", "-q", &url, "HEAD:refs/heads/main"])
                .status
                .success(),
            "seeding {repo} failed"
        );
    }
    Served { base, work }
}

/// The published repository, and only it, answers a reader with nothing.
#[test]
fn a_published_repository_answers_a_reader_who_has_no_account() {
    let s = served("browse", "@anon\topen/source.git\tread\nalice\t*\twrite\n");

    assert_eq!(
        anon(&format!("{}/r/open/source", s.base)),
        200,
        "a published repository still met the wall"
    );
    // Not `401`: the refusal a reader without a grant receives is the
    // one a reader of a repository that does not exist receives, and
    // keeping them identical is what stops the wall confirming which
    // private repositories are here.
    assert_eq!(
        anon(&format!("{}/r/closed/thing", s.base)),
        404,
        "an unpublished repository was visible, or answered differently \
         from one that does not exist"
    );
    assert_eq!(
        anon(&format!("{}/r/open/nosuch", s.base)),
        404,
        "the refusals stopped agreeing"
    );
}

/// Publishing a repository publishes the repository, not the node.
#[test]
fn the_op_log_and_the_node_scope_stay_behind_the_wall() {
    let s = served("scope", "@anon\topen/source.git\tread\nalice\t*\twrite\n");

    // `/api/view` and `/api/log` are `GET`s that serve the op log. A
    // gate that let anonymous *safe methods* through would have
    // published both, which is why the route test is an allowlist.
    for path in ["/api/view", "/api/log", "/api/repos"] {
        assert_eq!(
            anon(&format!("{}{path}", s.base)),
            401,
            "{path} answered a caller with no credential"
        );
    }
    let _ = &s.work;
}

/// The read half of git smart-HTTP opens; the write half does not.
#[test]
fn an_anonymous_clone_works_and_an_anonymous_push_does_not() {
    let s = served("git", "@anon\topen/source.git\tread\nalice\t*\twrite\n");

    let clone = s.work.join("clone");
    let url = format!("{}/open/source.git", s.base);
    let out = git(
        &s.work,
        &["clone", "-q", &url, clone.to_str().expect("utf-8 path")],
    );
    assert!(
        out.status.success(),
        "anonymous clone failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(clone.join("f.txt").exists(), "the clone fetched no tree");

    // The push half is `Level::Propose`, which `@anon` cannot parse its
    // way into holding, so the node challenges and git — with no
    // terminal and no helper — fails rather than becoming somebody.
    std::fs::write(clone.join("g.txt"), "mine\n").expect("file");
    git(&clone, &["config", "user.email", "m@b.invalid"]);
    git(&clone, &["config", "user.name", "m"]);
    git(&clone, &["add", "g.txt"]);
    git(&clone, &["commit", "-q", "-m", "second"]);
    let pushed = git(&clone, &["push", "-q", "origin", "main"]);
    assert!(
        !pushed.status.success(),
        "an anonymous push was accepted into a published repository"
    );

    // The unpublished repository is not fetchable either, by the same
    // refusal the browse surface gives.
    let denied = git(
        &s.work,
        &[
            "clone",
            "-q",
            &format!("{}/closed/thing.git", s.base),
            s.work.join("denied").to_str().expect("utf-8 path"),
        ],
    );
    assert!(
        !denied.status.success(),
        "an unpublished repository was cloneable"
    );
}

/// A node that never names the principal is the node it was before.
#[test]
fn a_table_that_does_not_publish_anything_still_meets_every_stranger_with_the_wall() {
    let s = served("closed", "alice\t*\twrite\n");

    for path in ["/r/open/source", "/r/closed/thing"] {
        assert_eq!(
            anon(&format!("{}{path}", s.base)),
            401,
            "{path} opened without anybody publishing it"
        );
    }
    let fetched = anon(&format!(
        "{}/open/source.git/info/refs?service=git-upload-pack",
        s.base
    ));
    assert_eq!(fetched, 401, "git opened without anybody publishing it");
}
