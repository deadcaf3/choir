//! `choir backup verify`: whether a copy can be restored *from*.
//!
//! Every test builds a directory and breaks one thing in it, because a
//! check nobody has watched fail is a check that might not be wired to
//! anything. The daemon is deliberately not passed in most of them: the
//! chain check then warns, which is the point — a machine holding a
//! backup is not necessarily a machine that runs nodes, and everything
//! else here is still worth having on it.

use choir_cli::backup::{is_secret, manifest_value, restorable, verify};
use choir_cli::doctor::{Check, Status};
use std::path::{Path, PathBuf};

fn status_of<'a>(checks: &'a [Check], name: &str) -> &'a Check {
    checks
        .iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("a `{name}` check, in {:?}", names(checks)))
}

fn names(checks: &[Check]) -> Vec<&str> {
    checks.iter().map(|c| c.name.as_str()).collect()
}

fn sha256_of(path: &Path) -> String {
    let out = std::process::Command::new("openssl")
        .args(["dgst", "-sha256", "-r", &path.display().to_string()])
        .output()
        .expect("openssl runs");
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .expect("a digest")
        .to_string()
}

/// A structurally complete backup: four files, a matching checksum, the
/// three required policy members, and one bundle recording a whole
/// history. Nothing here is signed, so the chain check is left to warn.
fn good(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("choir-cli-backup-{tag}"));
    std::fs::remove_dir_all(&root).ok();
    let dir = root.join("backup");
    std::fs::create_dir_all(dir.join("repos")).expect("backup dir");

    std::fs::write(dir.join("ops.jsonl"), "{\"seq\":0}\n").expect("log");
    std::fs::write(dir.join("node.fingerprint"), "fingerprint\n").expect("fingerprint");

    let policy = root.join("policy");
    std::fs::create_dir_all(&policy).expect("policy dir");
    for name in ["keys", "reviewers", "repos.list"] {
        std::fs::write(policy.join(name), "x\n").expect("policy member");
    }
    tar(
        &policy,
        &dir.join("policy.tar"),
        &["keys", "reviewers", "repos.list"],
    );

    let sha = sha256_of(&dir.join("ops.jsonl"));
    std::fs::write(
        dir.join("manifest"),
        format!("format_version 1\nops_sha256 {sha}\nnext_seq 1\n"),
    )
    .expect("manifest");

    bundle(&root, &dir.join("repos/one.bundle"));
    dir
}

fn tar(from: &Path, into: &Path, members: &[&str]) {
    let ok = std::process::Command::new("tar")
        .arg("cf")
        .arg(into)
        .arg("-C")
        .arg(from)
        .args(members)
        .status()
        .expect("tar runs")
        .success();
    assert!(ok, "tar should build the policy archive");
}

/// A real bundle with one commit, so `git bundle verify` has something
/// to agree with.
fn bundle(root: &Path, into: &Path) {
    let repo = root.join("src");
    std::fs::create_dir_all(&repo).expect("src repo");
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(&repo)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git runs")
    };
    git(&["init", "-q", "."]);
    std::fs::write(repo.join("f"), "hi\n").expect("a file");
    git(&["add", "-A"]);
    git(&["commit", "-qm", "one"]);
    let out = git(&["bundle", "create", &into.display().to_string(), "--all"]);
    assert!(out.status.success(), "the fixture bundle should build");
}

#[test]
fn a_complete_backup_is_restorable() {
    let checks = verify(&good("complete"), None);
    assert!(restorable(&checks), "{:?}", names(&checks));
    assert_eq!(status_of(&checks, "files").status, Status::Pass);
    assert_eq!(status_of(&checks, "checksum").status, Status::Pass);
    assert_eq!(status_of(&checks, "policy").status, Status::Pass);
    assert_eq!(status_of(&checks, "secrets").status, Status::Pass);
    assert_eq!(status_of(&checks, "bundles").status, Status::Pass);
}

/// Without a daemon the chain is unwalked, and that is a warning rather
/// than a verdict: a backup is not unrestorable because the machine
/// looking at it has no `choir-node`.
#[test]
fn no_daemon_leaves_the_chain_unchecked_without_condemning_the_backup() {
    let checks = verify(&good("nodaemon"), None);
    assert_eq!(status_of(&checks, "chain").status, Status::Warn);
    assert!(restorable(&checks));
}

/// The manifest is the only record of what the log was when it was
/// taken. A log that no longer matches it was modified or truncated
/// afterwards, and nothing else in the directory would say so.
#[test]
fn a_log_that_no_longer_matches_its_checksum_fails() {
    let dir = good("tampered");
    std::fs::write(dir.join("ops.jsonl"), "{\"seq\":0}\n{\"seq\":1}\n").expect("append");
    let checks = verify(&dir, None);
    assert_eq!(status_of(&checks, "checksum").status, Status::Fail);
    assert!(!restorable(&checks));
}

/// A backup carries the log, the policy and the objects. A backup that
/// also carries the node's identity is a copy of that identity on
/// whatever disk the backup lives on.
#[test]
fn a_credential_in_the_policy_archive_fails() {
    let dir = good("leak");
    let root = dir.parent().expect("a root").to_path_buf();
    let policy = root.join("policy");
    std::fs::write(policy.join("auth"), "choir:token\n").expect("a credential");
    tar(
        &policy,
        &dir.join("policy.tar"),
        &["keys", "reviewers", "repos.list", "auth"],
    );
    let checks = verify(&dir, None);
    assert_eq!(status_of(&checks, "secrets").status, Status::Fail);
    assert!(!restorable(&checks));
}

/// The log restores and every repository is empty, which is the failure
/// that looks most like success.
#[test]
fn a_backup_with_no_bundle_fails() {
    let dir = good("nobundles");
    std::fs::remove_dir_all(dir.join("repos")).expect("drop the bundles");
    let checks = verify(&dir, None);
    assert_eq!(status_of(&checks, "bundles").status, Status::Fail);
    assert!(!restorable(&checks));
}

#[test]
fn policy_missing_a_required_member_fails() {
    let dir = good("thinpolicy");
    let policy = dir.parent().expect("a root").join("policy");
    tar(&policy, &dir.join("policy.tar"), &["keys"]);
    let checks = verify(&dir, None);
    assert_eq!(status_of(&checks, "policy").status, Status::Fail);
    assert!(!restorable(&checks));
}

/// "The node had no ACL" and "this backup lost the ACL" look identical
/// in a restored directory, so the optional members are reported — and
/// reported as a warning, because only one of those is a disaster.
#[test]
fn optional_policy_is_reported_but_never_fatal() {
    let checks = verify(&good("optional"), None);
    let optional = status_of(&checks, "policy (optional)");
    assert_eq!(optional.status, Status::Warn);
    assert!(optional.detail.contains("acl"), "{}", optional.detail);
    assert!(restorable(&checks));
}

/// An incomplete backup is one true statement, not eight consequences
/// of it: the checks that read those files do not run at all.
#[test]
fn a_directory_that_is_not_a_backup_says_only_that() {
    let root = std::env::temp_dir().join("choir-cli-backup-empty");
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(&root).expect("empty dir");
    let checks = verify(&root, None);
    assert_eq!(checks.len(), 1, "{:?}", names(&checks));
    assert_eq!(checks[0].status, Status::Fail);
    for name in ["ops.jsonl", "node.fingerprint", "policy.tar", "manifest"] {
        assert!(checks[0].detail.contains(name), "{}", checks[0].detail);
    }
}

/// A file that exists and holds nothing is a backup that ran and
/// captured nothing, which reads as success everywhere except here.
#[test]
fn an_empty_required_file_is_missing() {
    let dir = good("emptyfile");
    std::fs::write(dir.join("ops.jsonl"), "").expect("truncate");
    let checks = verify(&dir, None);
    assert_eq!(checks[0].status, Status::Fail);
    assert!(checks[0].detail.contains("empty"), "{}", checks[0].detail);
}

#[test]
fn secrets_are_recognised_by_name_anywhere_in_the_archive() {
    for name in [
        "auth",
        "node.key",
        "policy/auth",
        "a/b/signing.pem",
        "x.key",
    ] {
        assert!(is_secret(name), "{name} should be refused");
    }
    for name in ["keys", "repos.list", "acl", "keys.list", "reviewers"] {
        assert!(!is_secret(name), "{name} is not a secret");
    }
}

#[test]
fn the_manifest_is_read_key_by_key() {
    let manifest = "format_version 1\nops_sha256 abc123\nnext_seq 42\n";
    assert_eq!(
        manifest_value(manifest, "ops_sha256").as_deref(),
        Some("abc123")
    );
    assert_eq!(manifest_value(manifest, "next_seq").as_deref(), Some("42"));
    assert_eq!(manifest_value(manifest, "absent"), None);
}

// ---------------------------------------------------------------- taking one

/// A node's state directory as `choir init` and a first push leave it:
/// a real log written by a node in this process, a fingerprint, the
/// three required policy files, and one repository with a pushed commit,
/// listed in repos.list. The node keeps serving on its thread; the log
/// is append-only, which is the property `take` relies on.
fn node_state(tag: &str) -> (PathBuf, choir_cli::serve::Layout) {
    let root = std::env::temp_dir().join(format!("choir-cli-take-{tag}-{}", std::process::id()));
    std::fs::remove_dir_all(&root).ok();
    let state = root.join("state");
    let layout = choir_cli::serve::Layout::new(&state, 8417);
    let node_state = layout.repos.join(".choir");
    std::fs::create_dir_all(&node_state).expect("node state");
    let node_key = choir_identity::ActorKey::generate();
    std::fs::write(
        node_state.join("node.fingerprint"),
        format!("{}\n", node_key.actor_id().to_hex()),
    )
    .expect("fingerprint");
    std::fs::write(node_state.join("refs.snapshot"), "{}\n").expect("snapshot");
    for name in ["keys", "reviewers"] {
        std::fs::write(state.join(name), "x\n").expect("policy");
    }
    std::fs::write(state.join("repos.list"), "me/thing.git\n").expect("repos.list");
    // A secret beside the policy files, which a backup must never take.
    std::fs::write(state.join("auth"), "choir:token\n").expect("auth");

    let log = choir_oplog::FileLog::open(&node_state.join("ops.jsonl")).expect("log opens");
    let platform =
        choir_node::Platform::start(choir_identity::Registry::new(), Box::new(log), node_key)
            .expect("platform");
    let mut node = choir_node::Node::bind(&layout.repos, 0).expect("node binds");
    node.create_repo("me/thing.git").expect("repo");
    node.enable_platform(platform);
    let port = node.port();
    std::thread::spawn(move || node.serve_forever());
    let bare = format!("http://127.0.0.1:{port}/me/thing.git");
    let src = root.join("src");
    std::fs::create_dir_all(&src).expect("src");
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(&src)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git runs");
        assert!(out.status.success(), "{out:?}");
    };
    git(&["init", "-q", "."]);
    std::fs::write(src.join("f"), "hi\n").expect("file");
    git(&["add", "-A"]);
    git(&["commit", "-qm", "one"]);
    git(&["push", "-q", &bare, "HEAD:main"]);
    (root, layout)
}

fn manifest_of(dest: &Path) -> String {
    std::fs::read_to_string(dest.join("manifest")).expect("manifest")
}

#[test]
fn a_backup_taken_is_a_backup_verify_accepts() {
    let (root, layout) = node_state("take");
    let dest = root.join("backup");
    let taken = choir_cli::backup::take(&layout, &dest, None).expect("taken");
    assert!(taken.ops >= 1, "a push wrote at least one entry");
    assert_eq!(taken.next_seq, taken.ops as u64);
    assert!(taken.grew.is_none());
    assert_eq!(taken.bundles.len(), 1);
    assert!(
        taken.bundles[0].starts_with("thing: bundled"),
        "{:?}",
        taken.bundles
    );
    assert!(taken
        .warnings
        .iter()
        .any(|w| w.contains("chain unverified")));

    let manifest = manifest_of(&dest);
    assert!(manifest.contains("format_version 1\n"));
    assert!(manifest.contains(&format!("next_seq {}\n", taken.next_seq)));
    assert!(manifest.contains("repos 1\n"));
    assert!(manifest.contains(&format!(
        "ops_sha256 {}\n",
        sha256_of(&dest.join("ops.jsonl"))
    )));
    assert!(dest.join("refs.snapshot").is_file());
    assert!(dest.join("repos/thing.bundle").is_file());
    assert!(dest.join("repos/thing.refs").is_file());
    assert!(!dest.join(format!(".incoming-{}", "")).exists());
    assert!(
        std::fs::read_dir(&dest).expect("dest").all(|e| !e
            .expect("entry")
            .file_name()
            .to_string_lossy()
            .starts_with(".incoming")),
        "the staging directory is gone"
    );

    let checks = choir_cli::backup::verify(&dest, None);
    assert!(choir_cli::backup::restorable(&checks), "{checks:?}");
    assert_eq!(status_of(&checks, "secrets").status, Status::Pass);
}

#[test]
fn an_unchanged_repository_keeps_its_bundle_and_a_grown_log_is_a_prefix() {
    let (root, layout) = node_state("again");
    let dest = root.join("backup");
    choir_cli::backup::take(&layout, &dest, None).expect("first");
    let first = std::fs::metadata(dest.join("repos/thing.bundle"))
        .expect("bundle")
        .modified()
        .expect("mtime");
    std::thread::sleep(std::time::Duration::from_millis(20));
    let log = layout.repos.join(".choir/ops.jsonl");
    let mut text = std::fs::read_to_string(&log).expect("log");
    let was = text.lines().count() as u64;
    text.push_str(&format!("{{\"seq\":{was}}}\n"));
    std::fs::write(&log, text).expect("append");

    let taken = choir_cli::backup::take(&layout, &dest, None).expect("second");
    assert_eq!(taken.next_seq, was + 1);
    assert!(taken.grew.is_some());
    assert_eq!(taken.bundles, vec!["thing: unchanged, kept".to_string()]);
    let second = std::fs::metadata(dest.join("repos/thing.bundle"))
        .expect("bundle")
        .modified()
        .expect("mtime");
    assert!(second >= first);
    assert!(manifest_of(&dest).contains(&format!("next_seq {}\n", was + 1)));
}

#[test]
fn a_shorter_or_diverged_log_is_refused_and_the_backup_kept() {
    let (root, layout) = node_state("diverged");
    let dest = root.join("backup");
    choir_cli::backup::take(&layout, &dest, None).expect("first");
    let before = manifest_of(&dest);
    let log = layout.repos.join(".choir/ops.jsonl");

    std::fs::write(&log, "{\"seq\":0}\n").expect("shrink");
    let error = choir_cli::backup::take(&layout, &dest, None).expect_err("shorter");
    assert!(error.contains("SHORTER"), "{error}");

    // Longer than before, but not the same bytes: one character of the
    // first record changed is a different history of the same length.
    let held = std::fs::read_to_string(dest.join("ops.jsonl")).expect("held");
    let diverged = held.replacen("format_version", "format_versioN", 1) + "{\"seq\":9}\n";
    assert_ne!(diverged[..held.len()], held[..], "the fixture must diverge");
    std::fs::write(&log, diverged).expect("diverge");
    let error = choir_cli::backup::take(&layout, &dest, None).expect_err("diverged");
    assert!(error.contains("NOT a prefix"), "{error}");

    assert_eq!(manifest_of(&dest), before, "the held backup is untouched");
    assert!(dest.join("ops.jsonl").is_file());
}

#[test]
fn a_torn_or_gapped_log_is_refused() {
    let (root, layout) = node_state("torn");
    let dest = root.join("backup");
    let log = layout.repos.join(".choir/ops.jsonl");
    std::fs::write(&log, "{\"seq\":0}\n{\"seq\":2}\n").expect("gap");
    let error = choir_cli::backup::take(&layout, &dest, None).expect_err("gap");
    assert!(error.contains("not contiguous"), "{error}");
    std::fs::write(&log, "{\"seq\":0}\n{\"se").expect("torn");
    let error = choir_cli::backup::take(&layout, &dest, None).expect_err("torn");
    assert!(error.contains("no seq"), "{error}");
    assert!(!dest.join("manifest").exists());
}

#[test]
fn a_node_with_no_log_is_named_not_backed_up() {
    let root = std::env::temp_dir().join(format!("choir-cli-take-empty-{}", std::process::id()));
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(&root).expect("root");
    let layout = choir_cli::serve::Layout::new(&root.join("state"), 8417);
    let error = choir_cli::backup::take(&layout, &root.join("backup"), None).expect_err("no log");
    assert!(error.contains("no op log"), "{error}");
}

#[test]
fn the_command_refuses_a_destination_inside_the_state_directory() {
    let (root, layout) = node_state("inside");
    let inside = layout.state.join("backup");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_choir"))
        .args([
            "backup",
            "take",
            inside.to_str().unwrap(),
            "--state",
            layout.state.to_str().unwrap(),
        ])
        .current_dir(&root)
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("inside the state directory"));

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_choir"))
        .args([
            "backup",
            "take",
            root.join("b").to_str().unwrap(),
            "--state",
            layout.state.to_str().unwrap(),
        ])
        .current_dir(&root)
        .output()
        .expect("runs");
    let text =
        String::from_utf8_lossy(&out.stderr).to_string() + &String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}");
    // The receipt block is silent off a terminal; the report is not.
    assert!(
        text.contains("taken") && text.contains("next seq"),
        "{text}"
    );
    assert!(
        text.contains("chain") && !text.contains("unverified"),
        "{text}"
    );
    assert!(text.contains("usable"), "{text}");
}

#[test]
fn the_stamp_is_utc_and_sortable() {
    let stamp = choir_cli::backup::utc_stamp();
    assert_eq!(stamp.len(), 16, "{stamp}");
    assert!(stamp.ends_with('Z'));
    assert_eq!(&stamp[8..9], "T");
    assert!(stamp.starts_with("20"), "{stamp}");
}
