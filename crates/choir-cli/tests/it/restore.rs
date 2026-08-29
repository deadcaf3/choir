//! `choir backup restore`: the refusals, which are the safety property.
//!
//! The ordering under test is read, refuse, then write. A restore that
//! fails halfway has already destroyed the thing an operator would fall
//! back to, so every one of these builds a broken backup and checks that
//! nothing reached the target.
//!
//! The rehearsal boot is not exercised here: it needs a real
//! `choir-node`, which this crate's test harness has no path to. What is
//! exercised is everything that decides whether the boot is allowed to
//! happen at all, plus the pure pieces the boot's own proofs rest on —
//! the port parse, the attestation projection and the ref comparison.
//!
//! `--verify-log` is stood in for by `/bin/true` and `/bin/false`. The
//! check under test is "what does a restore do when the daemon accepts
//! or rejects this log", and a stand-in answers that exactly, without
//! this crate needing to build a daemon to be told no.

use choir_cli::restore::{
    attested_refs, bundle_for, read, ref_mismatches, resuming, secrets, served_refs, serving_port,
    Refusal,
};
use std::path::PathBuf;

fn yes() -> PathBuf {
    PathBuf::from("/bin/echo")
}

fn no() -> PathBuf {
    PathBuf::from("/bin/false")
}

struct Fixture {
    src: PathBuf,
    work: PathBuf,
    root: PathBuf,
}

/// A backup that passes every read-side check, so one thing at a time
/// can be broken in it.
fn fixture(tag: &str) -> Fixture {
    let base = std::env::temp_dir().join(format!("choir-cli-restore-{tag}"));
    std::fs::remove_dir_all(&base).ok();
    let src = base.join("backup");
    std::fs::create_dir_all(src.join("repos")).expect("backup");
    std::fs::create_dir_all(src.join("policy")).expect("policy");
    std::fs::write(src.join("ops.jsonl"), "{\"seq\":0}\n{\"seq\":1}\n").expect("log");
    std::fs::write(src.join("node.fingerprint"), "pin\n").expect("fingerprint");
    for name in ["keys", "reviewers"] {
        std::fs::write(src.join("policy").join(name), "x\n").expect("policy member");
    }
    std::fs::write(src.join("policy/repos.list"), "me/thing.git\n").expect("repos.list");
    std::fs::write(src.join("repos/thing.bundle"), "not really a bundle\n").expect("bundle");
    let work = base.join("work");
    std::fs::create_dir_all(&work).expect("work");
    Fixture {
        src,
        work,
        root: base.join("root"),
    }
}

fn read_ok(f: &Fixture) -> Result<choir_cli::restore::Backup, Refusal> {
    read(&f.src, &f.work, &yes())
}

#[test]
fn a_complete_backup_reads() {
    let f = fixture("complete");
    let backup = read_ok(&f).expect("this backup should read");
    assert_eq!(backup.ops, 2);
    assert_eq!(backup.repos, ["me/thing.git"]);
}

/// Verified before a byte reaches the target, so a log the daemon
/// refuses never becomes a half-restored root.
#[test]
fn a_log_the_daemon_refuses_stops_the_restore() {
    let f = fixture("badlog");
    let error = read(&f.src, &f.work, &no()).expect_err("refuses");
    assert_eq!(error.code, 1);
    assert!(error.message.contains("verification"), "{}", error.message);
}

#[test]
fn an_empty_log_is_not_a_backup() {
    let f = fixture("emptylog");
    std::fs::write(f.src.join("ops.jsonl"), "").expect("truncate");
    let error = read_ok(&f).expect_err("refuses");
    assert!(error.message.contains("empty"), "{}", error.message);
}

#[test]
fn a_directory_with_no_log_is_not_a_backup() {
    let f = fixture("nolog");
    std::fs::remove_file(f.src.join("ops.jsonl")).expect("remove");
    let error = read_ok(&f).expect_err("refuses");
    assert!(error.message.contains("not a backup"), "{}", error.message);
}

/// A node restored without reviewers does not boot, so this is refused
/// rather than discovered by a daemon that will not start.
#[test]
fn policy_missing_a_required_file_stops_the_restore() {
    let f = fixture("thinpolicy");
    std::fs::remove_file(f.src.join("policy/reviewers")).expect("remove");
    let error = read_ok(&f).expect_err("refuses");
    assert!(error.message.contains("reviewers"), "{}", error.message);
}

#[test]
fn no_policy_at_all_stops_the_restore() {
    let f = fixture("nopolicy");
    std::fs::remove_dir_all(f.src.join("policy")).expect("remove");
    let error = read_ok(&f).expect_err("refuses");
    assert!(error.message.contains("no policy"), "{}", error.message);
}

/// The same assertion a backup leg makes about its own output, made here
/// about its input: it holds whoever put the file there.
#[test]
fn a_secret_in_the_backup_stops_the_restore() {
    let f = fixture("leak");
    std::fs::write(f.src.join("policy/auth"), "choir:token\n").expect("a credential");
    let error = read_ok(&f).expect_err("refuses");
    assert!(error.message.contains("SECRETS"), "{}", error.message);
    assert!(error.message.contains("auth"), "{}", error.message);
}

#[test]
fn a_repository_with_no_bundle_stops_the_restore() {
    let f = fixture("nobundle");
    std::fs::remove_file(f.src.join("repos/thing.bundle")).expect("remove");
    let error = read_ok(&f).expect_err("refuses");
    assert!(error.message.contains("no bundle"), "{}", error.message);
}

#[test]
fn a_repos_list_naming_nothing_stops_the_restore() {
    let f = fixture("norepos");
    std::fs::write(f.src.join("policy/repos.list"), "# only a comment\n\n").expect("empty list");
    let error = read_ok(&f).expect_err("refuses");
    assert!(
        error.message.contains("nothing to serve"),
        "{}",
        error.message
    );
}

/// Both backup legs name bundles differently, and the existence check
/// and the unbundle loop must never disagree about which file they mean.
#[test]
fn a_bundle_is_found_under_either_legs_naming() {
    let base = std::env::temp_dir().join("choir-cli-restore-naming");
    std::fs::remove_dir_all(&base).ok();
    std::fs::create_dir_all(base.join("repos/me")).expect("repos");

    // The flip-era leg: `basename $repo .git`.bundle, flat.
    std::fs::write(base.join("repos/thing.bundle"), "b\n").expect("flat");
    assert_eq!(
        bundle_for(&base, "me/thing.git"),
        Some(base.join("repos/thing.bundle"))
    );

    // The other leg: the full repository path.
    std::fs::write(base.join("repos/me/thing.git.bundle"), "b\n").expect("nested");
    assert_eq!(
        bundle_for(&base, "me/thing.git"),
        Some(base.join("repos/me/thing.git.bundle")),
        "the full path wins when both are present"
    );
    assert_eq!(bundle_for(&base, "me/absent.git"), None);
}

/// Never write over a log. Which of two logs is real is not a decision
/// available here, and refusing is what preserves the evidence needed to
/// make it.
#[test]
fn a_target_holding_a_different_log_is_refused() {
    let f = fixture("occupied");
    let backup = read_ok(&f).expect("reads");
    std::fs::create_dir_all(f.root.join(".choir")).expect("root");
    std::fs::write(f.root.join(".choir/ops.jsonl"), "{\"seq\":0}\n").expect("somebody's log");
    let error = resuming(&f.root, &backup).expect_err("refuses");
    assert!(
        error.message.contains("not this backup"),
        "{}",
        error.message
    );
}

/// The one exception, and the reason it exists: the documented recovery
/// path is stop, supply the secret, re-run. Refusing the re-run would
/// make it unreachable — the first run places the files and the second
/// could never get past this check.
#[test]
fn a_target_holding_this_backup_is_resumed() {
    let f = fixture("resume");
    let backup = read_ok(&f).expect("reads");
    std::fs::create_dir_all(f.root.join(".choir")).expect("root");
    std::fs::copy(f.src.join("ops.jsonl"), f.root.join(".choir/ops.jsonl")).expect("placed");
    assert!(resuming(&f.root, &backup).expect("resumes"));
}

#[test]
fn a_fresh_target_is_not_a_resume() {
    let f = fixture("fresh");
    let backup = read_ok(&f).expect("reads");
    assert!(!resuming(&f.root, &backup).expect("fresh"));
}

#[test]
fn a_target_already_holding_the_repository_is_refused() {
    let f = fixture("existingrepo");
    let backup = read_ok(&f).expect("reads");
    std::fs::create_dir_all(f.root.join("me/thing.git")).expect("an existing repo");
    let error = resuming(&f.root, &backup).expect_err("refuses");
    assert!(error.message.contains("empty root"), "{}", error.message);
}

/// A minted node key is a *new node* wearing the old node's log, so the
/// pin plus no key is a decision and never a default.
#[test]
fn a_pinned_identity_with_no_key_is_a_decision() {
    let root = std::env::temp_dir().join("choir-cli-restore-pinned");
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(root.join(".choir")).expect("root");
    std::fs::write(root.join(".choir/node.fingerprint"), "pin\n").expect("pin");
    std::fs::write(root.join(".choir/auth"), "choir:t\n").expect("auth");
    let error = secrets(&root, &root.join(".choir/auth"), 2).expect_err("decides");
    assert_eq!(error.code, 3, "an operator decision, not a failed check");
    assert!(error.message.contains("(a)"), "{}", error.message);
    assert!(error.message.contains("(b)"), "{}", error.message);
}

/// Option (b) taken: the pin is gone, so there is no identity to keep
/// and the restore proceeds — loudly, naming the seq the seam falls at.
#[test]
fn no_key_and_no_pin_proceeds_with_a_warning_naming_the_seam() {
    let root = std::env::temp_dir().join("choir-cli-restore-unpinned");
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(root.join(".choir")).expect("root");
    std::fs::write(root.join(".choir/auth"), "choir:t\n").expect("auth");
    let warning = secrets(&root, &root.join(".choir/auth"), 7)
        .expect("proceeds")
        .expect("a warning");
    assert!(warning.contains("seq 7"), "{warning}");
    assert!(warning.contains("0..6"), "{warning}");
}

/// Backups carry no credential by design, so this is the second hole
/// only a person can fill.
#[test]
fn no_credential_is_a_decision_not_a_failure() {
    let root = std::env::temp_dir().join("choir-cli-restore-nocred");
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(root.join(".choir")).expect("root");
    let error = secrets(&root, &root.join(".choir/auth"), 2).expect_err("decides");
    assert_eq!(error.code, 3);
    assert!(error.message.contains("openssl rand"), "{}", error.message);
}

/// A key present and a credential present is the ordinary case.
#[test]
fn a_key_and_a_credential_ask_nothing() {
    let root = std::env::temp_dir().join("choir-cli-restore-ready");
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(root.join(".choir")).expect("root");
    std::fs::write(root.join(".choir/node.key"), "k").expect("key");
    std::fs::write(root.join(".choir/auth"), "choir:t\n").expect("auth");
    assert!(secrets(&root, &root.join(".choir/auth"), 2)
        .expect("proceeds")
        .is_none());
}

/// The rehearsal asks for port 0, so only the daemon knows what it got.
#[test]
fn the_port_is_read_out_of_the_daemons_own_line() {
    assert_eq!(
        serving_port("choir-node serving 1 repo on http://127.0.0.1:53211/\n"),
        Some(53211)
    );
    assert_eq!(serving_port("choir-node starting up\n"), None);
    assert_eq!(serving_port(""), None);
}

/// The attestation holds canonical content hashes and the view holds
/// their display form, so the projection goes through `to_hex` itself —
/// a comparison written against imagined hex compares nothing at all.
#[test]
fn the_attestation_is_projected_into_the_form_the_view_serves() {
    let hash = choir_hash::ContentHash::from_git_oid(&"a".repeat(40)).expect("a git oid");
    let snapshot = serde_json::json!({
        "at_seq": 4,
        "refs": { "me/thing.git:refs/heads/main": hash },
    });
    let attested = attested_refs(&snapshot).expect("projects");
    assert_eq!(
        attested.get("me/thing.git:refs/heads/main"),
        Some(&hash.to_hex())
    );

    let view = serde_json::json!({
        "refs": { "me/thing.git:refs/heads/main": hash.to_hex() },
    });
    assert!(ref_mismatches(&attested, &served_refs(&view)).is_empty());
}

/// The half a checksum cannot reach: the bytes can arrive perfectly and
/// still be replayed into a different view.
#[test]
fn a_view_that_disagrees_with_the_attestation_names_every_ref() {
    let a = choir_hash::ContentHash::from_git_oid(&"a".repeat(40)).expect("oid");
    let b = choir_hash::ContentHash::from_git_oid(&"b".repeat(40)).expect("oid");
    let attested = attested_refs(&serde_json::json!({
        "at_seq": 1,
        "refs": { "r:main": a, "r:gone": a },
    }))
    .expect("projects");
    let served = served_refs(&serde_json::json!({
        "refs": { "r:main": b.to_hex() },
    }));
    let differences = ref_mismatches(&attested, &served);
    assert_eq!(differences.len(), 2, "{differences:?}");
    assert!(differences.iter().any(|d| d.contains("r:main")));
    assert!(differences.iter().any(|d| d.contains("r:gone")));
}

#[test]
fn an_attestation_that_is_not_one_is_reported() {
    assert!(attested_refs(&serde_json::json!({ "at_seq": 1 })).is_err());
    assert!(attested_refs(&serde_json::json!({ "refs": { "r": "nonsense" } })).is_err());
}

/// Exit 3 is its own outcome. Collapsing it into 1 would make the
/// documented recovery path indistinguishable from a corrupt backup.
#[test]
fn a_decision_and_a_failure_have_different_exit_codes() {
    assert_eq!(Refusal::decide("x").code, 3);
    assert_eq!(Refusal::fail("x").code, 1);
}

/// The work directory is per-call, not per-process: the harness runs
/// every module in one process on parallel threads.
#[test]
fn two_restores_in_one_process_do_not_share_a_work_directory() {
    let f = fixture("parallel-a");
    let g = fixture("parallel-b");
    assert_ne!(f.work, g.work);
    assert!(read_ok(&f).is_ok());
    assert!(read_ok(&g).is_ok());
}
