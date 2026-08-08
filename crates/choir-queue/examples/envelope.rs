//! Prints the precision a detector can reach at a given bad-merge rate.
//!
//! ```text
//! cargo run -p choir-queue --example envelope -- [prevalence]
//! ```
//!
//! Defaults to 0.004, `choir-queue::corpus`'s revert-labelled rate on
//! rust-lang/rust. That is a borrowed lower bound, not our prevalence.

use choir_queue::envelope::{required_false_positive_rate, Detector};

fn main() {
    let p: f64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(0.004);
    println!("bad-merge prevalence: {p:.4}\n");

    println!("Published operating points, transferred to this prevalence:");
    println!(
        "{:<34}{:>7}{:>7}{:>12}{:>12}",
        "detector (corpus prevalence 1/3)", "recall", "prec", "prec here", "FP per hit"
    );
    for (name, prec, rec) in [
        ("Borba disjunction of 4 analyses", 0.43, 0.60),
        ("  same, 31-unit subsample", 0.65, 0.88),
        ("Override Assignment alone", 0.47, 0.28),
    ] {
        let d = Detector::from_reported(prec, rec, 1.0 / 3.0).expect("valid operating point");
        println!(
            "{name:<34}{rec:>7.2}{prec:>7.2}{:>12.4}{:>12.0}",
            d.precision_at(p),
            d.false_alarms_per_hit(p).unwrap_or(f64::NAN)
        );
    }

    println!("\nWhat a useful detector would need here:");
    println!("{:<34}{:>12}{:>16}", "target precision", "max FP rate", "vs Borba's 0.40");
    for target in [0.25, 0.50, 0.75, 0.90] {
        let need = required_false_positive_rate(p, 0.60, target).expect("valid target");
        let label = format!("{:.0}%", target * 100.0);
        println!("{label:<34}{need:>12.5}{:>15.0}x", 0.3977 / need);
    }

    println!("\nDifferential testing, where the FP rate IS the flake rate:");
    println!("{:<34}{:>7}{:>12}{:>12}", "spurious-failure rate", "recall", "precision", "FP per hit");
    for flake in [0.05, 0.01, 0.005, 0.001, 0.0005] {
        let d = Detector { recall: 0.80, false_positive_rate: flake };
        println!(
            "{:<34}{:>7.2}{:>12.3}{:>12.1}",
            format!("{:.2}%", flake * 100.0),
            d.recall,
            d.precision_at(p),
            d.false_alarms_per_hit(p).unwrap_or(f64::NAN)
        );
    }
}
