//! What a round of the train actually costs (D5, D18).
//!
//! Two numbers decide whether the speculative queue is affordable, and
//! this project has never measured either: **CI runs per landed
//! change**, and **where the AIMD window settles**. Both are quoted
//! everywhere from borrowed populations whose regime is not ours, which
//! is the same mistake `crate::corpus`'s module doc refuses to make
//! about a semantic-conflict rate. So they are measured here instead.
//!
//! The sweep is four cells, because failure *structure* matters and not
//! just failure rate: one bad plan produces several bad changes, and a
//! declared dependency turns one eviction into many. Injecting
//! independent failures alone would understate the cost of the case we
//! expect from agents.
//!
//! | | deps absent | deps declared |
//! |---|---|---|
//! | i.i.d. failures | scattered, halving | scattered, mostly halving |
//! | clustered failures | correlated, halving | correlated, ejection by declaration |
//!
//! The declared cells matter because [`choir_queue::MergeQueue::drain`]
//! does not halve the window when it ejects by declaration, so a
//! failure that declarations can explain skips the cost governor. That
//! is the right trade when failures follow the declarations and a bad
//! one when they do not, which is what the two structure rows separate.
//! Declaring is therefore not free even when it does not help.
//!
//! Run `cargo test -p choir-queue --test it cost:: -- --nocapture` for
//! the table; the assertions below are the parts that must not move.

use choir_queue::executor::{Synthetic, Verdict};
use choir_queue::{run_batch, Change, QueueReport};
use std::collections::BTreeSet;

/// Changes per sweep cell. Comfortably above the initial window of 20,
/// so the window's additive-increase half is exercised as well.
const CHANGES: u64 = 120;

/// Members per dependency group, and per failure cluster.
const GROUP: u64 = 3;

/// Base with room for one disjoint single-line edit per change.
fn base() -> String {
    (0..(3 * CHANGES + 3))
        .map(|i| format!("line {i}\n"))
        .collect()
}

/// A change editing line `3 * id`, disjoint from every other change,
/// so nothing here is measuring the merge strategy.
fn change(id: u64, depends: Vec<u64>) -> Change {
    let mut lines: Vec<String> = base().lines().map(String::from).collect();
    lines[(3 * id) as usize] = format!("line {} edited by change {id}", 3 * id);
    Change {
        id,
        workspace: format!("ws-{id}"),
        base: base(),
        proposed: lines.join("\n") + "\n",
        depends,
    }
}

/// xorshift64, so the sweep is deterministic without a dev-dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

/// Consecutive runs of [`GROUP`] ids: a dependency group, and the unit
/// a clustered failure takes out.
fn groups() -> Vec<Vec<u64>> {
    (0..CHANGES)
        .collect::<Vec<u64>>()
        .chunks(GROUP as usize)
        .map(<[u64]>::to_vec)
        .collect()
}

/// Each change fails independently with probability `pct/100`.
fn iid_failures(pct: u64, seed: u64) -> BTreeSet<u64> {
    let mut rng = Rng(seed);
    (0..CHANGES).filter(|_| rng.next() % 100 < pct).collect()
}

/// Whole groups fail, for about the same overall rate: one bad plan
/// yielding several bad changes.
fn clustered_failures(pct: u64, seed: u64) -> BTreeSet<u64> {
    let mut rng = Rng(seed);
    let all = groups();
    let want = (CHANGES * pct / 100).div_ceil(GROUP);
    let mut chosen: BTreeSet<usize> = BTreeSet::new();
    while (chosen.len() as u64) < want {
        chosen.insert((rng.next() % all.len() as u64) as usize);
    }
    chosen.into_iter().flat_map(|g| all[g].clone()).collect()
}

/// Every group member after the first declares the first, which is what
/// an agent decomposing one plan into several changes would say.
fn declared() -> Vec<Change> {
    groups()
        .into_iter()
        .flat_map(|g| {
            let head = g[0];
            g.into_iter()
                .map(move |id| change(id, if id == head { vec![] } else { vec![head] }))
        })
        .collect()
}

fn undeclared() -> Vec<Change> {
    (0..CHANGES).map(|id| change(id, vec![])).collect()
}

/// One cell of the sweep.
#[derive(Debug)]
struct Cost {
    runs: usize,
    landed: usize,
    rejected: usize,
    window_final: usize,
    window_min: usize,
}

impl Cost {
    /// CI runs per change that actually landed: the multiplier the
    /// `1 + p*k` estimates are about.
    fn multiplier(&self) -> f64 {
        self.runs as f64 / self.landed as f64
    }
}

fn measure(changes: Vec<Change>, failing: &BTreeSet<u64>) -> Cost {
    let owned: BTreeSet<String> = failing.iter().map(u64::to_string).collect();
    let mut ci = Synthetic::new(move |job| {
        if owned.contains(&job.label) {
            Verdict::Failed { exit_code: Some(1) }
        } else {
            Verdict::Passed
        }
    });
    let (report, _) = run_batch(&base(), changes, &mut ci);
    summarize(&report)
}

fn summarize(r: &QueueReport) -> Cost {
    Cost {
        runs: r.ci_runs,
        landed: r.merged.len(),
        rejected: r.rejected.len(),
        window_final: *r.window_trace.last().unwrap_or(&0),
        window_min: *r.window_trace.iter().min().unwrap_or(&0),
    }
}

/// The four-cell sweep across failure rates. Prints the table and
/// asserts the relations that must survive a change to the queue.
#[test]
fn the_cost_of_a_round_across_failure_rate_and_structure() {
    let mut cells: Vec<(u64, &str, &str, Cost)> = Vec::new();
    println!(
        "\n{:>4} {:>10} {:>8} {:>7} {:>7} {:>7} {:>7}",
        "p%", "structure", "deps", "mult", "landed", "wmin", "wend"
    );
    for pct in [0, 5, 10, 25] {
        for (structure, failing) in [
            ("iid", iid_failures(pct, 0x5eed_0001)),
            ("clustered", clustered_failures(pct, 0x5eed_0002)),
        ] {
            for (deps, changes) in [("absent", undeclared()), ("declared", declared())] {
                let c = measure(changes, &failing);
                println!(
                    "{pct:>4} {structure:>10} {deps:>8} {:>7.2} {:>7} {:>7} {:>7}",
                    c.multiplier(),
                    c.landed,
                    c.window_min,
                    c.window_final
                );
                assert_eq!(
                    c.landed + c.rejected,
                    CHANGES as usize,
                    "every change either landed or was rejected with a reason; \
                     none may be silently dropped ({structure}, deps {deps}, p={pct}%)"
                );
                cells.push((pct, structure, deps, c));
            }
        }
    }
    println!();

    let cell = |pct: u64, structure: &str, deps: &str| -> &Cost {
        &cells
            .iter()
            .find(|(p, s, d, _)| *p == pct && *s == structure && *d == deps)
            .expect("cell in the sweep")
            .3
    };

    // 1. Correlated failures cost more than scattered ones at the same
    //    rate. This is why the sweep has a structure axis at all: a
    //    model that injects failures independently understates the case
    //    we expect from agents, where one bad plan yields several bad
    //    changes.
    for pct in [5, 10, 25] {
        let iid = cell(pct, "iid", "absent").multiplier();
        let clustered = cell(pct, "clustered", "absent").multiplier();
        assert!(
            clustered > iid,
            "clustered failures must cost more than scattered ones at p={pct}%: \
             {clustered:.2}x vs {iid:.2}x"
        );
    }

    // 2. The cost governor. Halving the window on failure bounds the
    //    multiplier no matter how bad the failure rate gets -- the
    //    queue stops speculating rather than spending without limit.
    //    The price is throughput, which is what `window_final` shows.
    for (pct, structure, deps, c) in &cells {
        if *deps == "absent" {
            assert!(
                c.multiplier() < 3.0,
                "with halving in play the multiplier stays bounded \
                 ({structure}, p={pct}%): {:.2}x",
                c.multiplier()
            );
        }
    }
    assert!(
        cell(25, "iid", "absent").window_final < 10,
        "at a 25% failure rate the window has collapsed: the queue has stopped \
         speculating, which is how the multiplier above stayed bounded"
    );

    // 3. Declared dependencies are a conditional win, not a free one.
    //    Dependency mode names the blast radius instead of guessing it,
    //    and deliberately does not halve the window -- which removes the
    //    governor asserted above. That pays when failures cluster along
    //    the declarations and is a large regression when they do not.
    assert!(
        cell(10, "clustered", "declared").multiplier()
            < cell(10, "clustered", "absent").multiplier(),
        "declarations pay when failures follow them"
    );
    assert!(
        cell(10, "iid", "declared").multiplier() < 2.0 * cell(10, "iid", "absent").multiplier(),
        "and they stay within a factor of two of not declaring when failures are \
         scattered. The rule that entered dependency mode whenever declarations \
         existed anywhere read 4.81x against 1.64x here, because one change \
         declaring one dependency disabled halving for every failure in the drain"
    );
    assert!(
        cell(25, "iid", "declared").window_final < 10,
        "the governor is reached in the declared cells too: a failure nothing \
         depends on halves the window, whatever the rest of the queue declared"
    );
    assert!(
        cell(25, "iid", "declared").landed < cell(25, "iid", "absent").landed,
        "what declaring still costs is throughput, not runaway spend: a dependent \
         of a failure is ejected rather than retried, which is what it declared"
    );
}

/// The floor: a train nothing fails in costs exactly one CI run per
/// change, which is the whole reason to speculate. If this ever moves,
/// the multiplier numbers above are measuring something else.
#[test]
fn a_green_train_costs_exactly_one_run_per_change() {
    let none = BTreeSet::new();
    let c = measure(undeclared(), &none);
    assert_eq!(c.runs, CHANGES as usize);
    assert_eq!(c.landed, CHANGES as usize);
    assert!((c.multiplier() - 1.0).abs() < f64::EPSILON);
}

/// Declaring dependencies is not free. It disables window halving for
/// the whole drain, so a cell with declarations and scattered failures
/// is a real regression risk rather than a pure saving -- which is why
/// the sweep has four cells and not two.
#[test]
fn declaring_dependencies_changes_the_cost_even_when_failures_are_scattered() {
    let failing = iid_failures(10, 0x5eed_0001);
    let absent = measure(undeclared(), &failing);
    let present = measure(declared(), &failing);
    assert_ne!(
        (absent.runs, absent.landed),
        (present.runs, present.landed),
        "if declarations changed neither cost nor throughput, dependency-aware \
         ejection would not be doing anything"
    );
}
