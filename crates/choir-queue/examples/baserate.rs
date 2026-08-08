//! Measures a repository's revert-labelled merge base rate (D23).
//!
//! ```text
//! cargo run -p choir-queue --example baserate -- <repo-path> [window]...
//! ```
//!
//! Prints one row per window so the *sensitivity* is visible, which is
//! the part that matters: if the rate barely moves between a window of
//! 10 and one of 200, the window is not doing any work and the number is
//! really "reverts anywhere in history". If it climbs steadily, most
//! reverts are distant and the proxy is weaker than it looks.
//!
//! Defaults to windows 10, 25, 50, 100, 200.
//!
//! **What this measures is that repository's rate, not ours.** The D23
//! tripwire asks for our own prevalence, and our history is linear and
//! revert-free, so it cannot supply one. A borrowed rate is a reference
//! point from a comparable project — useful for asking whether a
//! detector's false-positive rate is survivable, useless as a claim
//! about choir.

use choir_queue::corpus::{base_rate, label_merges, history, parse_history};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(repo) = args.first() else {
        eprintln!("usage: baserate <repo-path> [window]...");
        std::process::exit(2);
    };
    let windows: Vec<usize> = if args.len() > 1 {
        args[1..].iter().filter_map(|w| w.parse().ok()).collect()
    } else {
        vec![10, 25, 50, 100, 200]
    };

    let log = match history(std::path::Path::new(repo), 0) {
        Ok(log) => log,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    let commits = parse_history(&log);
    let total_merges = commits.iter().filter(|c| c.parents.len() > 1).count();
    println!(
        "{repo}: {} first-parent commits, {total_merges} merges",
        commits.len()
    );

    println!("\n{:>7}  {:>7}  {:>8}  {:>7}", "window", "merges", "reverted", "rate");
    for w in &windows {
        let r = base_rate(&commits, *w);
        println!(
            "{:>7}  {:>7}  {:>8}  {:>6.3}",
            r.window, r.merges, r.reverted, r.rate()
        );
    }

    // Suitability before interpretation. A corpus that does not revert
    // reads as a corpus with no bad merges, and that reading is wrong in
    // the most flattering possible direction.
    let sample = base_rate(&commits, *windows.iter().max().unwrap_or(&200));
    println!(
        "\nrevert commits anywhere in history: {} ({:.2} per 1000 commits)",
        sample.revert_commits,
        sample.revert_commits as f64 * 1000.0 / sample.commits.max(1) as f64
    );
    if !sample.corpus_is_suitable() {
        println!(
            "UNSUITABLE CORPUS: too few reverts for the rate above to mean anything.\n\
             This project drops bad work before the mainline rather than reverting it,\n\
             so a low rate here measures its workflow, not its merge safety."
        );
    }

    // Where the reverts actually sit. If they cluster close, a short
    // window captures nearly all of them and the labelling is cheap; if
    // they are spread, any window choice is arbitrary and should be said
    // to be arbitrary.
    let widest = windows.iter().copied().max().unwrap_or(200);
    let mut distances: Vec<usize> = label_merges(&commits, widest)
        .into_iter()
        .filter_map(|m| m.distance)
        .collect();
    distances.sort_unstable();
    if distances.is_empty() {
        println!("\nno reverted merges found at window {widest}");
        return;
    }
    let pick = |q: f64| distances[((distances.len() - 1) as f64 * q).round() as usize];
    println!(
        "\nrevert distance over {} labelled merges: p50={} p90={} max={}",
        distances.len(),
        pick(0.5),
        pick(0.9),
        distances[distances.len() - 1]
    );
}
