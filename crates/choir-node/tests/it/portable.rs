//! Exporting a node root and settling what the export claims (§E).
//!
//! Every root here comes from a live node with a file-backed log, pushed
//! to over HTTP through the real hook, because an export validated
//! against a hand-written log validates an imagination of the format.
//! What the tests then do to those exports — swap a bundle for an older
//! one, plant a key, drop a repository — is what an export has to survive
//! being asked about.

use choir_identity::{ActorKey, Registry};
use choir_node::portable::{self, Report};
use choir_node::{AuthTable, Node, Platform};
use choir_oplog::FileLog;

use crate::support::curl;

use std::path::{Path, PathBuf};

fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        // The user's own config signs commits; a scratch repo has no key.
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "export test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "export test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .expect("git runs")
}

/// A private directory per test: this harness shares a process and runs
/// on parallel threads.
fn workdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "choir-portable-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// A served node whose log is a file, which is the only kind an export
/// can be taken of. Returns `(base url, root)`.
fn served(work: &Path, repos: &[&str]) -> (String, PathBuf) {
    let root = work.join("repos");
    std::fs::create_dir_all(root.join(".choir")).expect("state dir");
    (serve(&root, repos, &[]), root)
}

/// Serves an existing root, adopting the repositories already in it.
///
/// This is what a node does after a restore, and the only thing that
/// settles whether an import produced a node rather than a directory
/// with the right shape in it.
fn serve(root: &Path, create: &[&str], adopt: &[&str]) -> String {
    let log = root.join(".choir").join("ops.jsonl");

    let platform = Platform::start(
        Registry::new(),
        Box::new(FileLog::open(&log).expect("file log")),
        ActorKey::generate(),
    )
    .expect("platform starts")
    .with_log_path(log);

    let mut table = AuthTable::new();
    table.insert("alice".into(), "a".into());
    let mut node = Node::bind_with_auth(root, 0, Some(table)).expect("node binds free port");
    let port = node.port();
    for repo in create {
        node.create_repo(repo).expect("repo created");
    }
    for repo in adopt {
        node.adopt_repo(repo).expect("repo adopted");
    }
    node.enable_platform(platform);
    std::thread::spawn(move || node.serve_forever());
    format!("http://alice:a@127.0.0.1:{port}")
}

/// Clones `repo`, commits `text`, and pushes it to `refspec`.
fn push(work: &Path, base: &str, repo: &str, name: &str, text: &str, refspec: &str) -> String {
    let dir = work.join(name);
    if !dir.exists() {
        let out = git(
            work,
            &[
                "clone",
                "--quiet",
                &format!("{base}/{repo}"),
                dir.to_str().expect("utf8"),
            ],
        );
        assert!(
            out.status.success(),
            "clone: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    std::fs::write(dir.join("f.txt"), text).expect("write");
    assert!(git(&dir, &["add", "."]).status.success());
    assert!(git(&dir, &["commit", "--quiet", "-m", text])
        .status
        .success());
    let out = git(
        &dir,
        &["push", "--quiet", "origin", &format!("HEAD:{refspec}")],
    );
    assert!(
        out.status.success(),
        "push {refspec}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&git(&dir, &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string()
}

fn manifest(dir: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).expect("manifest"))
        .expect("json")
}

/// The happy path, and the two things about it worth stating: the export
/// carries the refs the *log* names rather than the ones a policy file
/// lists, and `refs/for/*` is among them — a proposal (D53) is state a
/// node would otherwise lose on a move.
#[test]
fn an_export_carries_the_log_and_every_ref_the_log_names() {
    let work = workdir("happy");
    let (base, root) = served(&work, &["owner/p.git"]);
    let branch = push(&work, &base, "owner/p.git", "c", "one\n", "refs/heads/main");
    let proposal = push(
        &work,
        &base,
        "owner/p.git",
        "c",
        "two\n",
        "refs/for/main/alice/topic",
    );

    let out = work.join("export");
    let report = portable::export(&root, &out).expect("export");
    assert_eq!(
        report,
        Report {
            records: report.records,
            head: report.head.clone(),
            bundles: 1,
            refs: 2,
            ahead: 0
        },
        "one repository, both refs matched, nothing ahead"
    );
    assert!(
        report.records > 0 && report.head.is_some(),
        "a pushed node has a log: {report:?}"
    );

    let m = manifest(&out);
    assert_eq!(m["format_version"], 1);
    assert_eq!(m["log_format_version"], 1);
    assert_eq!(m["repos"][0]["name"], "owner/p.git");
    assert_eq!(m["repos"][0]["refs"], 2);

    let heads = String::from_utf8_lossy(
        &git(&out, &["bundle", "list-heads", "repos/owner/p.git.bundle"]).stdout,
    )
    .to_string();
    assert!(
        heads.contains(&format!("{branch} refs/heads/main")),
        "branch missing: {heads}"
    );
    assert!(
        heads.contains(&format!("{proposal} refs/for/main/alice/topic")),
        "the proposal is state the export must carry: {heads}"
    );

    // Reading it back is the same answer, which is the whole claim.
    assert_eq!(portable::verify(&out).expect("verify"), report);
}

/// The failure that matters: a log naming a commit no bundle holds.
///
/// Built the way it actually happens rather than by editing a file — two
/// exports taken either side of a push, then the older objects paired
/// with the newer log. Restoring this produces a node whose view points
/// at objects it does not have, and startup reconciliation answers that
/// by retracting, in signed ops, exactly the refs being restored.
#[test]
fn a_bundle_behind_its_log_is_refused() {
    let work = workdir("behind");
    let (base, root) = served(&work, &["owner/p.git"]);
    push(&work, &base, "owner/p.git", "c", "one\n", "refs/heads/main");
    let old = work.join("old");
    portable::export(&root, &old).expect("first export");

    let newer = push(&work, &base, "owner/p.git", "c", "two\n", "refs/heads/main");
    let out = work.join("new");
    portable::export(&root, &out).expect("second export");

    let bundle = "repos/owner/p.git.bundle";
    std::fs::copy(old.join(bundle), out.join(bundle)).expect("swap in the older objects");
    let refused = portable::verify(&out).expect_err("a bundle behind its log must be refused");
    assert!(
        refused.contains("refs/heads/main") && refused.contains(&newer),
        "the refusal must name the ref and where the log has it: {refused}"
    );

    // An import will not place it either, and refuses before it writes:
    // an export that does not settle must leave no half-node behind.
    // Without this the whole `verify` call at the top of `import` can be
    // deleted and every other test here stays green -- the traversal
    // test is caught by the name check further down, not by this.
    let fresh = work.join("fresh");
    let refused = portable::import(&out, &fresh).expect_err("a bad export must not be imported");
    assert!(
        refused.contains("refs/heads/main") && refused.contains(&newer),
        "the import must refuse for the same reason: {refused}"
    );
    assert!(!fresh.exists(), "a refused import writes nothing");

    // And the same repository with no bundle at all, which is the other
    // shape of the same fault.
    std::fs::remove_file(out.join(bundle)).expect("remove");
    let refused = portable::verify(&out).expect_err("a missing bundle must be refused");
    assert!(refused.contains("not here"), "{refused}");
}

/// A repository nobody has pushed to cannot be bundled — git refuses to
/// write a zero-ref bundle — so it is listed without one. Dropping it
/// instead would lose a repository on every move.
#[test]
fn a_repository_with_no_refs_is_listed_without_a_bundle() {
    let work = workdir("empty");
    let (base, root) = served(&work, &["owner/p.git", "owner/q.git"]);
    push(&work, &base, "owner/p.git", "c", "one\n", "refs/heads/main");

    let out = work.join("export");
    let report = portable::export(&root, &out).expect("export");
    assert_eq!(report.bundles, 1, "only the pushed repository has objects");

    let m = manifest(&out);
    let names: Vec<&str> = m["repos"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|r| r["name"].as_str().expect("name"))
        .collect();
    assert_eq!(
        names,
        ["owner/p.git", "owner/q.git"],
        "both repositories are listed: {m}"
    );
    assert!(
        m["repos"][1]["bundle"].is_null(),
        "the empty one carries no bundle: {m}"
    );
}

/// A real root holds the daemon's signing key beside the log. None of
/// the three shapes must reach the export, and the check is on the
/// output rather than on the care taken writing it.
///
/// They are placed here rather than minted, because it is `main` that
/// persists a key and this node is in-process. That is the point: the
/// export walks what is actually there, so something nothing in this
/// crate knows the name of is still caught by the shape of its name.
#[test]
fn an_export_carries_no_secret() {
    let work = workdir("secret");
    let (base, root) = served(&work, &["owner/p.git"]);
    push(&work, &base, "owner/p.git", "c", "one\n", "refs/heads/main");
    let state = root.join(".choir");
    let planted = ["node.key", "auth", "tls.pem"];
    for name in planted {
        std::fs::write(state.join(name), "x").expect("plant one in the root");
    }

    let out = work.join("export");
    portable::export(&root, &out).expect("export");
    for name in planted {
        assert!(!out.join(name).exists(), "the export must not carry {name}");
    }

    std::fs::write(out.join("node.key"), "x").expect("plant");
    let refused = portable::verify(&out).expect_err("a planted key must be refused");
    assert!(
        refused.contains("node.key") && refused.contains("secret"),
        "{refused}"
    );
}

/// Two refusals with nothing in common but their reason: an export never
/// writes into a directory it did not create, and never reads a layout it
/// does not know.
#[test]
fn an_export_refuses_an_occupied_destination_and_an_unknown_version() {
    let work = workdir("refusals");
    let (base, root) = served(&work, &["owner/p.git"]);
    push(&work, &base, "owner/p.git", "c", "one\n", "refs/heads/main");

    let out = work.join("export");
    portable::export(&root, &out).expect("export");
    let refused = portable::export(&root, &out).expect_err("a second export must not overwrite");
    assert!(refused.contains("already exists"), "{refused}");

    let mut m = manifest(&out);
    m["format_version"] = serde_json::json!(2);
    std::fs::write(out.join("manifest.json"), m.to_string()).expect("rewrite");
    let refused = portable::verify(&out).expect_err("a newer layout must be refused, not guessed");
    assert!(refused.contains("format_version is 2"), "{refused}");
}

/// A root with no log is not a node root, and a log that does not verify
/// is not exportable: the refusal happens before a byte is written, so a
/// failed export leaves nothing behind to mistake for one.
#[test]
fn an_export_refuses_a_root_without_a_verifiable_log() {
    let work = workdir("badlog");
    let bare = work.join("bare");
    std::fs::create_dir_all(&bare).expect("dir");
    let refused = portable::export(&bare, &work.join("out")).expect_err("no log, no export");
    assert!(
        refused.contains("no op log") && refused.contains("--keys-file"),
        "{refused}"
    );
    // A refusal is text somebody reads. rustfmt collapsing a line
    // continuation leaves a run of literal spaces in the middle of it,
    // which no assertion about the words would ever notice.
    assert!(
        !refused.contains("  "),
        "the refusal carries a run of spaces: {refused}"
    );
    assert!(
        !work.join("out").exists(),
        "a refused export writes nothing"
    );

    let (base, root) = served(&work, &["owner/p.git"]);
    push(&work, &base, "owner/p.git", "c", "one\n", "refs/heads/main");
    let log = root.join(".choir").join("ops.jsonl");
    let mut bytes = std::fs::read(&log).expect("log");
    // Damage a terminated record, which is corruption rather than a torn
    // tail: the first byte of the first line stops being `{`.
    bytes[0] = b'x';
    std::fs::write(&log, &bytes).expect("damage");
    let refused = portable::export(&root, &work.join("out2")).expect_err("a broken log is refused");
    assert!(refused.contains("ops.jsonl"), "{refused}");
    assert!(
        !work.join("out2").exists(),
        "a refused export writes nothing"
    );
}

/// The claim the whole thing exists to make: a node goes into a
/// directory and comes back out somewhere else.
///
/// A restored node that serves is not a restored node, so this does not
/// stop at reading the refs back. It boots on the imported root, checks
/// the view the log replays into names what the original's did, and then
/// pushes — because what settles a restore is a node that accepts a
/// write, and the only thing that can prove the objects arrived is git
/// building a commit on top of one of them.
#[test]
fn a_node_survives_the_round_trip_and_still_accepts_a_write() {
    let work = workdir("roundtrip");
    let (base, root) = served(&work, &["owner/p.git"]);
    let branch = push(&work, &base, "owner/p.git", "c", "one\n", "refs/heads/main");
    push(
        &work,
        &base,
        "owner/p.git",
        "c",
        "two\n",
        "refs/for/main/alice/topic",
    );

    // The daemon binary pins its identity beside the log; an in-process
    // node does not, so it is placed here. Losing it on a move is the
    // failure it exists to prevent: a daemon whose key file is absent
    // mints a fresh one and appends happily, and the log changes author
    // with nothing downstream marking the seam.
    std::fs::write(root.join(".choir").join("node.fingerprint"), "fp\n").expect("pin");

    let out = work.join("export");
    let exported = portable::export(&root, &out).expect("export");

    let fresh = work.join("fresh");
    let imported = portable::import(&out, &fresh).expect("import");
    // Both sides report what `verify` read from the same directory, so
    // this says the import checked before it wrote, not that the copy is
    // good. What says that is the boot and the push below.
    assert_eq!(imported, exported, "the import settled the same claim");
    assert!(
        !fresh.join(".choir").join("node.key").exists(),
        "an import cannot invent the signing key it was not given"
    );
    assert!(
        fresh.join(".choir").join("node.fingerprint").is_file(),
        "the fingerprint has to travel, or the daemon mints a fresh key and the \
         log silently changes author"
    );
    // A bundle clone leaves an `origin` pointing at the bundle file,
    // which is gone the moment the export is.
    let remotes = git(&fresh.join("owner").join("p.git"), &["remote"]);
    assert!(
        String::from_utf8_lossy(&remotes.stdout).trim().is_empty(),
        "the imported repository still points at the export it came from"
    );

    let second = serve(&fresh, &[], &["owner/p.git"]);
    let (status, view) = curl(&["-u", "alice:a", &format!("{second}/api/view")]);
    assert_eq!(status, 200, "the imported node serves: {view}");
    assert_eq!(
        view["refs"]["owner/p.git:refs/heads/main"]
            .as_str()
            .map(|r| r.ends_with(&branch)),
        Some(true),
        "the replayed view names the branch it was exported with: {}",
        view["refs"]
    );
    assert!(
        !view["refs"]["owner/p.git:refs/for/main/alice/topic"].is_null(),
        "the proposal came across too: {}",
        view["refs"]
    );

    // And it takes a write, on top of an object that only the bundle
    // could have carried.
    let landed = push(
        &work,
        &second,
        "owner/p.git",
        "after",
        "three\n",
        "refs/heads/main",
    );
    assert_ne!(landed, branch, "the write built on what was restored");
}

/// The manifest is data from a directory somebody else made, so a
/// repository name in it is untrusted input, and both halves join it to
/// a path.
#[test]
fn a_repository_name_cannot_climb_out_of_the_root() {
    let work = workdir("traversal");
    let (base, root) = served(&work, &["owner/p.git"]);
    push(&work, &base, "owner/p.git", "c", "one\n", "refs/heads/main");

    let out = work.join("export");
    portable::export(&root, &out).expect("export");
    let mut m = manifest(&out);
    m["repos"][0]["name"] = serde_json::json!("../../escaped.git");
    std::fs::write(out.join("manifest.json"), m.to_string()).expect("rewrite");

    let refused = portable::verify(&out).expect_err("a climbing name must be refused");
    assert!(refused.contains("not a repository path"), "{refused}");
    let refused = portable::import(&out, &work.join("fresh")).expect_err("and on the way in");
    assert!(refused.contains("not a repository path"), "{refused}");
    assert!(
        !work.join("escaped.git").exists(),
        "nothing was written outside the root"
    );
}

/// An import never writes over a log or a repository, for the reason the
/// restore script gives: the thing being overwritten is the fallback.
#[test]
fn an_import_refuses_a_root_that_is_already_a_node() {
    let work = workdir("occupied");
    let (base, root) = served(&work, &["owner/p.git"]);
    push(&work, &base, "owner/p.git", "c", "one\n", "refs/heads/main");
    let out = work.join("export");
    portable::export(&root, &out).expect("export");

    let refused = portable::import(&out, &root).expect_err("that root is the live one");
    assert!(refused.contains("already holds a log"), "{refused}");

    // A root with the repository but no log is the other half of it.
    let half = work.join("half");
    std::fs::create_dir_all(half.join("owner").join("p.git")).expect("dir");
    let refused = portable::import(&out, &half).expect_err("a repository is in the way");
    assert!(refused.contains("already exists"), "{refused}");
    assert!(
        !half.join(".choir").join("ops.jsonl").exists(),
        "and nothing was placed"
    );
}
