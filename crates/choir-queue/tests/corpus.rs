//! Base-rate labelling (D23): a merge is weak-labelled bad if history
//! later reverted it inside a window. Driven against a real git repo
//! built inline, because the thing most likely to be wrong is the
//! `git log` format contract, and a hand-written fixture would not
//! notice if git changed it.

use choir_queue::corpus::{base_rate, label_merges, parse_history, Commit};

fn git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args([
            "-c", "commit.gpgsign=false",
            "-c", "tag.gpgsign=false",
            "-c", "init.defaultBranch=main",
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

fn commit(dir: &std::path::Path, file: &str, body: &str, message: &str) -> String {
    std::fs::write(dir.join(file), body).unwrap();
    git(dir, &["add", "."]);
    let out = git(dir, &["commit", "-q", "-m", message]);
    assert!(
        out.status.success() || !out.stderr.is_empty(),
        "commit {message}"
    );
    String::from_utf8(git(dir, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_string()
}

#[test]
fn a_reverted_merge_is_labelled_and_an_untouched_one_is_not() {
    let work = std::env::temp_dir().join(format!("choir-corpus-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    assert!(git(&work, &["init", "-q", "."]).status.success());

    commit(&work, "base.txt", "base\n", "root");

    // Merge 1: gets reverted a couple of commits later.
    git(&work, &["checkout", "-q", "-b", "bad"]);
    commit(&work, "bad.txt", "bad\n", "bad feature");
    git(&work, &["checkout", "-q", "main"]);
    assert!(git(&work, &["merge", "-q", "--no-ff", "-m", "merge bad", "bad"])
        .status
        .success());
    let bad_merge = String::from_utf8(git(&work, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_string();

    commit(&work, "unrelated.txt", "x\n", "unrelated work");
    // git's own revert wording is what the labeller matches, so this uses
    // git revert rather than a hand-written message.
    let out = git(&work, &["revert", "-m", "1", "--no-edit", &bad_merge]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    // Merge 2: never reverted.
    git(&work, &["checkout", "-q", "-b", "good"]);
    commit(&work, "good.txt", "good\n", "good feature");
    git(&work, &["checkout", "-q", "main"]);
    assert!(git(&work, &["merge", "-q", "--no-ff", "-m", "merge good", "good"])
        .status
        .success());
    let good_merge = String::from_utf8(git(&work, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_string();

    let log = choir_queue::corpus::history(&work, 0).expect("git log");
    let history = parse_history(&log);
    // First-parent order: the mainline only. `bad feature` and `good
    // feature` live inside the merged branches and are correctly absent,
    // which is what makes "N commits later" mean "N landings later".
    assert_eq!(
        history.len(),
        5,
        "expected mainline only, got {:?}",
        history.iter().map(|c| c.body.lines().next().unwrap_or("")).collect::<Vec<_>>()
    );

    let labelled = label_merges(&history, 10);
    assert_eq!(labelled.len(), 2, "two merges: {labelled:?}");

    let bad = labelled.iter().find(|m| m.merge == bad_merge).expect("bad merge");
    assert!(bad.reverted_by.is_some(), "reverted merge not labelled: {bad:?}");
    assert_eq!(
        bad.distance,
        Some(2),
        "revert lands 2 mainline commits after the merge: {bad:?}"
    );

    let good = labelled.iter().find(|m| m.merge == good_merge).expect("good merge");
    assert_eq!(good.reverted_by, None, "clean merge labelled bad: {good:?}");

    let rate = base_rate(&history, 10);
    assert_eq!((rate.merges, rate.reverted), (2, 1));
    assert!((rate.rate() - 0.5).abs() < 1e-9);

    // A window shorter than the actual distance must not label it. The
    // window is the caller's knob and it has to actually do something,
    // or every measured rate is really a whole-history rate.
    let narrow = base_rate(&history, 1);
    assert_eq!(narrow.reverted, 0, "window is not being applied");

    std::fs::remove_dir_all(&work).ok();
}

#[test]
fn parsing_survives_bodies_that_look_like_the_format() {
    // Multi-line bodies and blank lines are the normal case, and a commit
    // message may legitimately mention a hash. Fields are separated by
    // ASCII 0x1f/0x1e precisely so a body cannot forge a record boundary.
    let log = "aaa\u{1f}bbb ccc\u{1f}merge: a thing\n\nrefs #12\n\u{1e}\nbbb\u{1f}\u{1f}root\n\u{1e}";
    let history = parse_history(log);
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].id, "aaa");
    assert_eq!(history[0].parents, ["bbb", "ccc"]);
    assert!(history[0].is_merge());
    assert!(history[0].body.contains("refs #12"));
    assert_eq!(history[1].parents, Vec::<String>::new());
    assert!(!history[1].is_merge(), "a root commit is not a merge");

    // Empty input yields no commits rather than one blank one.
    assert!(parse_history("").is_empty());
    assert!(parse_history("\n").is_empty());
}

#[test]
fn revert_detection_matches_gits_wording_and_abbreviated_oids() {
    let full = Commit {
        id: "r1".into(),
        parents: vec!["p".into()],
        body: "Revert \"x\"\n\nThis reverts commit 1234567890abcdef1234567890abcdef12345678.\n".into(),
    };
    assert_eq!(
        full.reverts().as_deref(),
        Some("1234567890abcdef1234567890abcdef12345678")
    );

    // Abbreviated form still parses; matching is by prefix either way.
    let short = Commit {
        id: "r2".into(),
        parents: vec!["p".into()],
        body: "This reverts commit 1234567.".into(),
    };
    assert_eq!(short.reverts().as_deref(), Some("1234567"));

    // A message merely discussing reverts is not one.
    let chatty = Commit {
        id: "r3".into(),
        parents: vec!["p".into()],
        body: "consider whether we should revert this later".into(),
    };
    assert_eq!(chatty.reverts(), None);

    // Too short to be an oid: refuse rather than match everything by prefix.
    let stub = Commit {
        id: "r4".into(),
        parents: vec!["p".into()],
        body: "This reverts commit abc.".into(),
    };
    assert_eq!(stub.reverts(), None, "a 3-char prefix would match anything");
}

#[test]
fn a_revert_that_predates_its_target_is_not_a_label() {
    // History is newest-first. A commit whose message names a *later*
    // commit cannot have reverted it, and treating it as one would
    // inflate the base rate with impossible pairs.
    let history = vec![
        Commit {
            id: "m1".into(),
            parents: vec!["a".into(), "b".into()],
            body: "merge".into(),
        },
        Commit {
            id: "r0".into(),
            parents: vec!["z".into()],
            body: "This reverts commit m1.".into(),
        },
    ];
    // r0 sits *below* m1, so it is older; the label must not fire.
    let labelled = label_merges(&history, 10);
    assert_eq!(labelled.len(), 1);
    assert_eq!(labelled[0].reverted_by, None);
}

#[test]
fn a_history_with_no_merges_reports_zero_merges_not_a_clean_rate() {
    // rate() returns 0.0 for an empty denominator, which reads as "no bad
    // merges". `merges` is what distinguishes that from "nothing measured".
    let history = vec![Commit {
        id: "c1".into(),
        parents: vec!["c0".into()],
        body: "work".into(),
    }];
    let rate = base_rate(&history, 10);
    assert_eq!(rate.merges, 0);
    assert_eq!(rate.commits, 1);
    assert!((rate.rate() - 0.0).abs() < f64::EPSILON);
}
