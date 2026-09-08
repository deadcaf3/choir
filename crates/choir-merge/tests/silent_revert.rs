//! Falsification (c)'s scanner: the pure halves against fixed text, and
//! the whole thing against a real git repository carrying a merge that
//! drops a line neither side dropped.

use choir_merge::silent_revert::{
    merge_log, parse_batch, parse_merges, scan_merge, scan_repo, Blobs, ScanReport,
    MERGE_LOG_FORMAT,
};
use std::path::{Path, PathBuf};
use std::process::Command;

/// A temp directory named for this test binary and a nanosecond clock,
/// since these tests each build a repository and must not share one.
fn tmp(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("choir-revert-scan-{tag}-{nanos}"));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Runs git in `repo`, asserting it succeeded.
///
/// The operator's own config is neutralized rather than overridden
/// setting by setting, the way `choir-node`'s restore tests do it. A
/// scratch repository has no signing key, so a global `commit.gpgsign`
/// turns every commit here into a pinentry prompt that a test run has
/// nowhere to display; and enumerating the settings that could break a
/// fixture is the kind of list that is only ever complete until the next
/// one. This file set an identity and stopped there, so it passed on a
/// machine with no global config and on one whose gpg-agent happened to
/// hold a cached passphrase, which is how it stayed green until a
/// background gate run had no terminal.
fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A repository with an identity, so committing works on a machine with
/// no global git config.
fn repo(tag: &str) -> PathBuf {
    let dir = tmp(tag);
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["config", "user.email", "test@example.invalid"]);
    git(&dir, &["config", "user.name", "test"]);
    dir
}

fn write(repo: &Path, path: &str, text: &str) {
    std::fs::write(repo.join(path), text).expect("write");
}

fn commit(repo: &Path, message: &str) {
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", message]);
}

#[test]
fn the_log_format_parses_into_merges_and_drops_junk() {
    let log = "aaa\u{1f}bbb ccc\u{1e}\nddd\u{1f}eee\u{1e}\n\u{1f}\u{1e}";
    let merges = parse_merges(log);
    assert_eq!(merges.len(), 2, "the empty-oid record is dropped");
    assert_eq!(merges[0].id, "aaa");
    assert_eq!(merges[0].parents, vec!["bbb", "ccc"]);
    assert_eq!(merges[1].parents, vec!["eee"]);
}

#[test]
fn a_batch_read_is_positional_so_paths_never_have_to_be_parsed_back() {
    // Two blobs and a miss, in git's own framing: header line, contents,
    // newline. The miss echoes the request, which may contain spaces.
    let out = b"aa blob 3\nhi\n\nsome path with spaces missing\nbb blob 0\n\n";
    let got = parse_batch(out, 3).expect("well-formed batch");
    assert_eq!(got[0].as_deref(), Some(&b"hi\n"[..]));
    assert_eq!(got[1], None, "a missing path is a hole, not an error");
    assert_eq!(got[2].as_deref(), Some(&b""[..]));
}

#[test]
fn a_truncated_batch_is_an_error_rather_than_a_short_answer() {
    let out = b"aa blob 40\nnot forty bytes\n";
    assert!(parse_batch(out, 1).is_err(), "must not silently under-read");
}

#[test]
fn a_landing_that_drops_a_target_line_is_a_finding_and_a_faithful_one_is_not() {
    let dropped = Blobs {
        base: "a\n".into(),
        target: "a\nkeep\n".into(),
        proposed: "a\nb\n".into(),
        result: "a\nb\n".into(),
    };
    let faithful = Blobs {
        base: "a\n".into(),
        target: "a\nkeep\n".into(),
        proposed: "a\nb\n".into(),
        result: "a\nkeep\nb\n".into(),
    };
    let mut report = ScanReport::default();
    scan_merge(
        "f00d",
        &[
            ("dropped.txt".to_string(), dropped),
            ("faithful.txt".to_string(), faithful),
        ],
        &mut report,
    );

    assert_eq!(report.merges_scanned, 1);
    assert_eq!(report.paths_scanned, 2);
    assert_eq!(report.findings.len(), 1, "only the dropping path");
    assert_eq!(report.findings[0].path, "dropped.txt");
    assert_eq!(report.findings[0].reverted, vec!["keep".to_string()]);
    assert!(report.findings[0].injected.is_empty());
    assert!(
        (report.merge_violation_rate() - 1.0).abs() < f64::EPSILON,
        "one merge scanned, one violating"
    );
}

#[test]
fn an_empty_scan_reports_zero_rather_than_a_nan() {
    let report = ScanReport::default();
    assert!(report.merge_violation_rate() == 0.0);
}

#[test]
fn absorbing_reports_sums_every_counter_including_the_skips() {
    let mut a = ScanReport {
        merges_seen: 3,
        merges_scanned: 2,
        skipped_octopus: 1,
        paths_scanned: 4,
        skipped_binary: 1,
        skipped_large: 2,
        skipped_no_base: 1,
        lines_relocated: 3,
        paths_all_relocated: 1,
        lines_surviving_elsewhere: 5,
        findings_all_surviving: 2,
        findings: Vec::new(),
    };
    a.absorb(a.clone());
    assert_eq!(a.merges_seen, 6);
    assert_eq!(a.merges_scanned, 4);
    assert_eq!(a.skipped_octopus, 2);
    assert_eq!(a.paths_scanned, 8);
    assert_eq!(a.skipped_binary, 2);
    assert_eq!(a.skipped_large, 4);
    assert_eq!(a.skipped_no_base, 2);
    assert_eq!(a.lines_relocated, 6);
    assert_eq!(a.paths_all_relocated, 2);
    assert_eq!(a.lines_surviving_elsewhere, 10);
    assert_eq!(a.findings_all_surviving, 4);
}

/// A reverted line that is sitting in the merge result, in another
/// file, is counted as surviving -- and still reported.
///
/// This is the refactor shape the relocation cancellation cannot see:
/// the destination addition is attributable to the author's own
/// proposal, so it never enters the injected set, so it cancels
/// nothing. Both halves are asserted, because counting it without
/// reporting it would be the filter this measurement declines to add.
#[test]
fn a_reverted_line_still_present_in_the_result_is_counted_and_still_reported() {
    let out_of = Blobs {
        base: "a\nfn moved_body() { work(); }\n".into(),
        target: "a\nfn moved_body() { work(); }\n".into(),
        proposed: "a\nfn moved_body() { work(); }\n".into(),
        result: "a\n".into(),
    };
    // The author proposed this file's new content, so the arriving line
    // is attributable here and is never an injection.
    let into = Blobs {
        base: "b\n".into(),
        target: "b\n".into(),
        proposed: "b\nfn moved_body() { work(); }\n".into(),
        result: "b\nfn moved_body() { work(); }\n".into(),
    };
    let mut report = ScanReport::default();
    scan_merge(
        "m",
        &[("out.rs".into(), out_of), ("into.rs".into(), into)],
        &mut report,
    );
    assert_eq!(
        report.lines_relocated, 0,
        "the existing cancellation cannot see this shape; if it can, this \
         test is now asserting the wrong thing"
    );
    assert_eq!(report.lines_surviving_elsewhere, 1);
    assert_eq!(report.findings_all_surviving, 1);
    assert_eq!(
        report.findings.len(),
        1,
        "surviving lines are counted, never silently dropped"
    );
    assert_eq!(
        report.findings[0].reverted,
        vec!["fn moved_body() { work(); }".to_string()]
    );
}

/// The counter must not fire on a line that genuinely left the tree.
#[test]
fn a_reverted_line_absent_from_the_result_does_not_count_as_surviving() {
    let lost = Blobs {
        base: "a\nfn work() { real(); }\n".into(),
        target: "a\nfn work() { real(); }\n".into(),
        proposed: "a\nfn work() { real(); }\n".into(),
        result: "a\n".into(),
    };
    let mut report = ScanReport::default();
    scan_merge("m", &[("lost.rs".into(), lost)], &mut report);
    assert_eq!(report.lines_surviving_elsewhere, 0);
    assert_eq!(report.findings_all_surviving, 0);
    assert_eq!(report.findings.len(), 1);
}

#[test]
fn content_moved_between_files_in_one_merge_is_cancelled_not_reported() {
    // The merge moves a section out of one file and into another. Per
    // path that reads as a reversion and an injection; across the merge
    // nothing was lost, and this was 40% of the raw findings on the
    // first corpus the scanner met.
    let out_of = Blobs {
        base: "a\nmoved section\n".into(),
        target: "a\nmoved section\n".into(),
        proposed: "a\nmoved section\n".into(),
        result: "a\n".into(),
    };
    let into = Blobs {
        base: "b\n".into(),
        target: "b\n".into(),
        proposed: "b\n".into(),
        result: "b\nmoved section\n".into(),
    };
    let mut report = ScanReport::default();
    scan_merge(
        "m0ved",
        &[("from.md".to_string(), out_of), ("to.md".to_string(), into)],
        &mut report,
    );

    assert!(
        report.findings.is_empty(),
        "a move is not a loss, got {:?}",
        report.findings
    );
    assert_eq!(report.lines_relocated, 1);
    assert_eq!(report.paths_all_relocated, 2);
    assert!(report.merge_violation_rate() == 0.0);
}

#[test]
fn a_real_loss_alongside_a_move_survives_the_cancellation() {
    let moved_out = Blobs {
        base: "a\nmoved\nkeep\n".into(),
        target: "a\nmoved\nkeep\n".into(),
        proposed: "a\nmoved\nkeep\n".into(),
        result: "a\n".into(),
    };
    let moved_in = Blobs {
        base: "b\n".into(),
        target: "b\n".into(),
        proposed: "b\n".into(),
        result: "b\nmoved\n".into(),
    };
    let mut report = ScanReport::default();
    scan_merge(
        "m1xed",
        &[
            ("from.md".to_string(), moved_out),
            ("to.md".to_string(), moved_in),
        ],
        &mut report,
    );

    assert_eq!(report.lines_relocated, 1, "only `moved` relocated");
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].path, "from.md");
    assert_eq!(
        report.findings[0].reverted,
        vec!["keep".to_string()],
        "the line that went nowhere is still the finding"
    );
}

#[test]
fn blank_lines_are_never_evidence() {
    let blobs = Blobs {
        base: "a\n\n\nb\n".into(),
        target: "a\n\n\nb\n".into(),
        proposed: "a\n\n\nb\n".into(),
        result: "a\nb\n".into(),
    };
    let mut report = ScanReport::default();
    scan_merge("b1ank", &[("f.txt".to_string(), blobs)], &mut report);
    assert!(
        report.findings.is_empty(),
        "whitespace carries no work, got {:?}",
        report.findings
    );
}

#[test]
fn a_hand_resolved_merge_that_drops_a_mainline_line_is_found_in_a_real_repository() {
    let dir = repo("found");
    write(&dir, "f.txt", "a\nb\nc\n");
    commit(&dir, "base");
    let base = git(&dir, &["rev-parse", "HEAD"]).trim().to_string();

    // The proposal edits the first line.
    git(&dir, &["checkout", "-q", "-b", "topic"]);
    write(&dir, "f.txt", "a edited by the proposal\nb\nc\n");
    commit(&dir, "proposal");

    // Mainline moves on, adding a line the proposal never saw.
    git(&dir, &["checkout", "-q", "main"]);
    write(&dir, "f.txt", "a\nb\nc\nadded on mainline\n");
    commit(&dir, "mainline");

    // The merge lands the proposal's edit and quietly loses the
    // mainline line: exactly what no CI can see, because the result
    // still compiles and its tests were green before that line existed.
    git(&dir, &["merge", "--no-commit", "--no-ff", "-q", "topic"]);
    write(&dir, "f.txt", "a edited by the proposal\nb\nc\n");
    commit(&dir, "merge topic");

    let report = scan_repo(&dir, 0).expect("scan runs");
    assert_eq!(report.merges_seen, 1);
    assert_eq!(report.merges_scanned, 1);
    assert_eq!(report.skipped_octopus, 0);
    assert_eq!(report.skipped_no_base, 0);
    assert_eq!(report.findings.len(), 1, "the dropped line is found");
    assert_eq!(report.findings[0].path, "f.txt");
    assert_eq!(
        report.findings[0].reverted,
        vec!["added on mainline".to_string()]
    );

    // And the merge base the scan used is the commit both sides forked
    // from, not either parent.
    let merges = merge_log(&dir, 0).expect("log");
    assert_eq!(merges[0].parents.len(), 2);
    assert_ne!(merges[0].parents[0], base);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_faithful_merge_of_the_same_shape_yields_nothing() {
    let dir = repo("clean");
    write(&dir, "f.txt", "a\nb\nc\n");
    commit(&dir, "base");

    git(&dir, &["checkout", "-q", "-b", "topic"]);
    write(&dir, "f.txt", "a edited by the proposal\nb\nc\n");
    commit(&dir, "proposal");

    git(&dir, &["checkout", "-q", "main"]);
    write(&dir, "f.txt", "a\nb\nc\nadded on mainline\n");
    commit(&dir, "mainline");

    // Git's own merge, resolved automatically: both edits survive.
    git(
        &dir,
        &["merge", "--no-ff", "-q", "-m", "merge topic", "topic"],
    );

    let report = scan_repo(&dir, 0).expect("scan runs");
    assert_eq!(report.merges_scanned, 1);
    assert!(
        report.findings.is_empty(),
        "a clean automatic merge must not be a finding, got {:?}",
        report.findings
    );
    assert!(report.merge_violation_rate() == 0.0);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_file_the_proposal_added_is_not_read_as_an_injection() {
    let dir = repo("added");
    write(&dir, "f.txt", "a\n");
    commit(&dir, "base");

    git(&dir, &["checkout", "-q", "-b", "topic"]);
    write(&dir, "new.txt", "brand new\n");
    commit(&dir, "add a file");

    git(&dir, &["checkout", "-q", "main"]);
    write(&dir, "f.txt", "a\nb\n");
    commit(&dir, "mainline");
    git(
        &dir,
        &["merge", "--no-ff", "-q", "-m", "merge topic", "topic"],
    );

    let report = scan_repo(&dir, 0).expect("scan runs");
    assert_eq!(report.merges_scanned, 1);
    assert!(
        report.findings.is_empty(),
        "an added file is attributable to the proposal, got {:?}",
        report.findings
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn an_octopus_merge_is_skipped_and_counted_rather_than_guessed_at() {
    let dir = repo("octopus");
    write(&dir, "f.txt", "a\n");
    commit(&dir, "base");
    for branch in ["one", "two"] {
        git(&dir, &["checkout", "-q", "-b", branch, "main"]);
        write(&dir, &format!("{branch}.txt"), "x\n");
        commit(&dir, branch);
    }
    git(&dir, &["checkout", "-q", "main"]);
    git(
        &dir,
        &["merge", "--no-ff", "-q", "-m", "octopus", "one", "two"],
    );

    let report = scan_repo(&dir, 0).expect("scan runs");
    assert_eq!(report.merges_seen, 1);
    assert_eq!(report.merges_scanned, 0, "not compared");
    assert_eq!(report.skipped_octopus, 1, "and not silently dropped either");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn the_format_string_is_the_one_the_parser_expects() {
    // A silent divergence between these two is the failure that would
    // make every scan report zero merges and look like a clean corpus.
    assert!(MERGE_LOG_FORMAT.contains("%H"));
    assert!(MERGE_LOG_FORMAT.contains("%P"));
    assert!(MERGE_LOG_FORMAT.contains("x1f") && MERGE_LOG_FORMAT.contains("x1e"));
}
