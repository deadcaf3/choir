//! Precision transfer (D23): a detector's reported precision does not
//! survive a change of prevalence, and the arithmetic that says so
//! decides whether a detector is worth building at all. Verified against
//! independently computed counts, not just against itself.

use choir_queue::envelope::{required_false_positive_rate, Detector};

/// Precision from raw counts over a population, the long way round.
fn precision_from_counts(pop: f64, prevalence: f64, recall: f64, fpr: f64) -> f64 {
    let bad = pop * prevalence;
    let good = pop - bad;
    let tp = bad * recall;
    let fp = good * fpr;
    tp / (tp + fp)
}

#[test]
fn transfer_is_identity_at_the_corpus_it_was_measured_on() {
    // The recovered detector, evaluated back at its own corpus, must
    // reproduce the reported precision. If it does not, the recovery is
    // wrong and every other number here is decoration.
    for (precision, recall, prevalence) in [
        (0.43, 0.60, 1.0 / 3.0),
        (0.65, 0.88, 0.30),
        (0.47, 0.28, 0.5),
    ] {
        let d = Detector::from_reported(precision, recall, prevalence).unwrap();
        assert!(
            (d.precision_at(prevalence) - precision).abs() < 1e-9,
            "round trip failed for {precision}/{recall} at {prevalence}"
        );
        assert!((d.recall - recall).abs() < 1e-12);
    }
}

#[test]
fn the_borba_operating_point_collapses_at_a_real_mainline_rate() {
    // ~60% recall at 43% precision is the best real semantic-conflict
    // detector operating point in the literature, measured on a corpus
    // that is roughly one-third positive. Deployed where bad merges are
    // 0.4% of the population, the same detector is a pager that cries
    // wolf.
    let d = Detector::from_reported(0.43, 0.60, 1.0 / 3.0).unwrap();

    // Recovered false-positive rate is high in absolute terms, which is
    // exactly why it does not survive dilution.
    assert!(
        (d.false_positive_rate - 0.3977).abs() < 1e-3,
        "fpr {}",
        d.false_positive_rate
    );

    let precision = d.precision_at(0.004);
    // Cross-check against counts computed a different way.
    let by_counts = precision_from_counts(100_000.0, 0.004, d.recall, d.false_positive_rate);
    assert!(
        (precision - by_counts).abs() < 1e-9,
        "{precision} vs {by_counts}"
    );

    assert!(
        precision < 0.01,
        "precision at 0.4% prevalence: {precision}"
    );
    let noise = d.false_alarms_per_hit(0.004).unwrap();
    assert!(noise > 100.0, "false alarms per hit: {noise}");
}

#[test]
fn precision_falls_monotonically_as_the_population_gets_cleaner() {
    let d = Detector::from_reported(0.43, 0.60, 1.0 / 3.0).unwrap();
    let mut last = 1.0;
    for p in [0.5, 0.33, 0.1, 0.01, 0.004, 0.001] {
        let cur = d.precision_at(p);
        assert!(
            cur < last,
            "precision rose from {last} to {cur} at prevalence {p}"
        );
        last = cur;
    }
}

#[test]
fn the_required_false_positive_rate_is_what_makes_this_actionable() {
    // Half the alarms being real, at 0.4% prevalence and 60% recall,
    // needs a false-positive rate around a quarter of a percent -- about
    // 160x better than the literature's measured 39%.
    let need = required_false_positive_rate(0.004, 0.60, 0.5).unwrap();
    assert!((need - 0.00241).abs() < 1e-4, "required fpr {need}");

    let d = Detector::from_reported(0.43, 0.60, 1.0 / 3.0).unwrap();
    assert!(
        d.false_positive_rate / need > 100.0,
        "gap {}",
        d.false_positive_rate / need
    );

    // And the requirement is self-consistent: a detector built exactly to
    // it hits the target precision.
    let built = Detector {
        recall: 0.60,
        false_positive_rate: need,
    };
    assert!((built.precision_at(0.004) - 0.5).abs() < 1e-9);
}

#[test]
fn differential_testing_is_bounded_by_flakiness_not_by_recall() {
    // The tractable option's false positives come from nondeterminism, so
    // the flake rate *is* the false-positive rate. This is the number
    // that decides whether merged-vs-parents differential testing is
    // worth building, and it is a much lower bar than the static
    // detectors have to clear.
    let bad_rate = 0.004;
    let generous_recall = 0.80;
    for (flake, floor, ceiling) in [
        (0.01, 0.20, 0.30),  // 1% spurious failures: ~3 false alarms per hit
        (0.001, 0.70, 0.80), // 0.1%: usable
    ] {
        let d = Detector {
            recall: generous_recall,
            false_positive_rate: flake,
        };
        let p = d.precision_at(bad_rate);
        assert!(p > floor && p < ceiling, "flake {flake} gave precision {p}");
    }

    // Perfect recall does not rescue a flaky suite: doubling recall from
    // 0.4 to 0.8 barely moves precision, while a 10x better flake rate
    // transforms it. That asymmetry is the whole design guidance.
    let flaky_high_recall = Detector {
        recall: 0.8,
        false_positive_rate: 0.01,
    };
    let flaky_low_recall = Detector {
        recall: 0.4,
        false_positive_rate: 0.01,
    };
    let clean_low_recall = Detector {
        recall: 0.4,
        false_positive_rate: 0.001,
    };
    assert!(
        clean_low_recall.precision_at(bad_rate) > flaky_high_recall.precision_at(bad_rate),
        "halving flake should beat doubling recall"
    );
    assert!(flaky_high_recall.precision_at(bad_rate) > flaky_low_recall.precision_at(bad_rate));
}

#[test]
fn bad_inputs_are_refused_rather_than_returning_a_plausible_number() {
    assert!(Detector::from_reported(1.5, 0.6, 0.3).is_err());
    assert!(Detector::from_reported(0.4, 0.6, -0.1).is_err());
    assert!(
        Detector::from_reported(0.0, 0.6, 0.3).is_err(),
        "zero precision"
    );
    assert!(
        Detector::from_reported(0.4, 0.0, 0.3).is_err(),
        "zero recall"
    );
    assert!(
        Detector::from_reported(0.4, 0.6, 1.0).is_err(),
        "no negatives"
    );
    assert!(
        required_false_positive_rate(0.004, 0.6, 1.0).is_err(),
        "demands zero fp"
    );
    assert!(required_false_positive_rate(0.004, 0.6, f64::NAN).is_err());

    // A detector that never fires reports 0.0 precision and no ratio,
    // rather than NaN leaking into a decision.
    let dead = Detector {
        recall: 0.0,
        false_positive_rate: 0.0,
    };
    assert!((dead.precision_at(0.004) - 0.0).abs() < f64::EPSILON);
    assert_eq!(dead.false_alarms_per_hit(0.004), None);
}
