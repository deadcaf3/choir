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
