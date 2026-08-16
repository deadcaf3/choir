//! Base-rate labelling (D23): a merge is weak-labelled bad if history
//! later reverted it inside a window. Driven against a real git repo
//! built inline, because the thing most likely to be wrong is the
//! `git log` format contract, and a hand-written fixture would not
//! notice if git changed it.

use choir_queue::corpus::{
    attribute, base_rate, label_merges, parse_history, revert_targets, Attribution, Commit,
};

fn git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
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
    assert!(
        git(&work, &["merge", "-q", "--no-ff", "-m", "merge bad", "bad"])
            .status
            .success()
    );
    let bad_merge = String::from_utf8(git(&work, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_string();

    commit(&work, "unrelated.txt", "x\n", "unrelated work");
    // git's own revert wording is what the labeller matches, so this uses
    // git revert rather than a hand-written message.
    let out = git(&work, &["revert", "-m", "1", "--no-edit", &bad_merge]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Merge 2: never reverted.
    git(&work, &["checkout", "-q", "-b", "good"]);
    commit(&work, "good.txt", "good\n", "good feature");
    git(&work, &["checkout", "-q", "main"]);
    assert!(git(
        &work,
        &["merge", "-q", "--no-ff", "-m", "merge good", "good"]
    )
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
        history
            .iter()
            .map(|c| c.body.lines().next().unwrap_or(""))
            .collect::<Vec<_>>()
    );

    let labelled = label_merges(&history, 10, &Attribution::new());
    assert_eq!(labelled.len(), 2, "two merges: {labelled:?}");

    let bad = labelled
        .iter()
        .find(|m| m.merge == bad_merge)
        .expect("bad merge");
    assert!(
        bad.reverted_by.is_some(),
        "reverted merge not labelled: {bad:?}"
    );
    assert_eq!(
        bad.distance,
        Some(2),
        "revert lands 2 mainline commits after the merge: {bad:?}"
    );

    let good = labelled
        .iter()
        .find(|m| m.merge == good_merge)
        .expect("good merge");
    assert_eq!(good.reverted_by, None, "clean merge labelled bad: {good:?}");

    let rate = base_rate(&history, 10, &Attribution::new());
    assert_eq!((rate.merges, rate.reverted), (2, 1));
    assert!((rate.rate() - 0.5).abs() < 1e-9);

    // A window shorter than the actual distance must not label it. The
    // window is the caller's knob and it has to actually do something,
    // or every measured rate is really a whole-history rate.
    let narrow = base_rate(&history, 1, &Attribution::new());
    assert_eq!(narrow.reverted, 0, "window is not being applied");

    std::fs::remove_dir_all(&work).ok();
}

#[test]
fn parsing_survives_bodies_that_look_like_the_format() {
    // Multi-line bodies and blank lines are the normal case, and a commit
    // message may legitimately mention a hash. Fields are separated by
    // ASCII 0x1f/0x1e precisely so a body cannot forge a record boundary.
    let log =
        "aaa\u{1f}bbb ccc\u{1f}merge: a thing\n\nrefs #12\n\u{1e}\nbbb\u{1f}\u{1f}root\n\u{1e}";
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
        body: "Revert \"x\"\n\nThis reverts commit 1234567890abcdef1234567890abcdef12345678.\n"
            .into(),
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
    let labelled = label_merges(&history, 10, &Attribution::new());
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
    let rate = base_rate(&history, 10, &Attribution::new());
    assert_eq!(rate.merges, 0);
    assert_eq!(rate.commits, 1);
    assert!((rate.rate() - 0.0).abs() < f64::EPSILON);
}

#[test]
fn corpus_suitability_separates_a_low_rate_from_an_unmeasurable_one() {
    // Measuring git/git is what forced this: 15,579 merges, 46 revert
    // commits in the whole first-parent history, labelled rate 0.001.
    // Read as safety that is a spectacular claim; read as workflow it is
    // just a project that drops bad topics from an integration branch
    // instead of reverting them. The two have to be distinguishable.
    let quiet: Vec<Commit> = (0..400)
        .map(|i| Commit {
            id: format!("c{i}"),
            parents: if i % 3 == 0 {
                vec!["a".into(), "b".into()]
            } else {
                vec!["a".into()]
            },
            body: "work".into(),
        })
        .collect();
    let r = base_rate(&quiet, 50, &Attribution::new());
    assert_eq!(r.revert_commits, 0);
    assert!(
        !r.corpus_is_suitable(),
        "a revert-free corpus cannot supply a rate"
    );
    assert!(
        (r.rate() - 0.0).abs() < f64::EPSILON,
        "and it still reads 0.0"
    );

    // One revert per 200 commits clears the bar.
    let mut reverting = quiet;
    for i in 0..2 {
        reverting[i * 100 + 1] = Commit {
            id: format!("r{i}"),
            parents: vec!["a".into()],
            body: "This reverts commit 1234567890abcdef.".into(),
        };
    }
    assert!(base_rate(&reverting, 50, &Attribution::new()).corpus_is_suitable());

    // An empty history is unsuitable rather than dividing by zero.
    assert!(!base_rate(&[], 50, &Attribution::new()).corpus_is_suitable());
}

#[test]
fn attribution_catches_a_revert_that_names_the_commit_not_the_merge() {
    // The pull-request workflow, which is the one that matters: a change
    // lands via a merge, and when it goes wrong someone reverts the
    // *change*. Measured on rust-lang/rust, 267 of 307 revert targets are
    // ordinary commits, so without attribution the proxy misses ~87% of
    // reverts and reports a reassuringly tiny rate.
    let work = std::env::temp_dir().join(format!("choir-attrib-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    assert!(git(&work, &["init", "-q", "."]).status.success());

    commit(&work, "base.txt", "base\n", "root");

    // A change lands through a merge.
    git(&work, &["checkout", "-q", "-b", "feature"]);
    let change = commit(&work, "feature.txt", "feature\n", "the change");
    git(&work, &["checkout", "-q", "main"]);
    assert!(git(
        &work,
        &["merge", "-q", "--no-ff", "-m", "merge feature", "feature"]
    )
    .status
    .success());
    let merge = String::from_utf8(git(&work, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_string();

    commit(&work, "other.txt", "other\n", "unrelated");
    // Revert the CHANGE, not the merge — no `-m 1`, because the target is
    // an ordinary commit.
    let out = git(&work, &["revert", "--no-edit", &change]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let log = choir_queue::corpus::history(&work, 0).expect("git log");
    let history = parse_history(&log);

    // Without attribution the merge looks clean, which is precisely the
    // failure mode: the change was taken back and nothing recorded it.
    let bare = base_rate(&history, 20, &Attribution::new());
    assert_eq!(bare.merges, 1);
    assert_eq!(bare.reverted, 0, "exact-oid matching should miss this");

    // With attribution the revert is charged to the merge that landed it.
    let targets = revert_targets(&history);
    assert!(
        targets.contains(&change),
        "revert target should be the change"
    );
    let attribution = attribute(&work, &history, &targets).expect("attribute");
    assert_eq!(attribution.get(&change), Some(merge.as_str()));
    assert!(
        attribution.unresolved().is_empty(),
        "{:?}",
        attribution.unresolved()
    );

    let attributed = base_rate(&history, 20, &attribution);
    assert_eq!(
        attributed.reverted, 1,
        "attribution should charge the merge"
    );
    assert!((attributed.rate() - 1.0).abs() < 1e-9);

    // A commit already on the mainline maps to itself, not to its child.
    let root_targets: std::collections::BTreeSet<String> =
        [history.last().unwrap().id.clone()].into_iter().collect();
    let self_attr = attribute(&work, &history, &root_targets).expect("attribute");
    let root = &history.last().unwrap().id;
    assert_eq!(
        self_attr.get(root),
        Some(root.as_str()),
        "mainline maps to itself"
    );

    // A commit from nowhere is unresolved, not silently attributed.
    let stranger: std::collections::BTreeSet<String> =
        ["0000000000000000000000000000000000000000".to_string()]
            .into_iter()
            .collect();
    let missing = attribute(&work, &history, &stranger);
    match missing {
        Ok(a) => assert_eq!(a.unresolved().len(), 1, "should be unresolved"),
        Err(e) => assert!(e.contains("merge-base"), "{e}"),
    }

    std::fs::remove_dir_all(&work).ok();
}
