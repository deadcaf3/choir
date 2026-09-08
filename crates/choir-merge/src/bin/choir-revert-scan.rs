//! `choir-revert-scan`: falsification (c), as a binary with a number.
//!
//! Points [`choir_merge::silent_revert`] at one or more repositories and
//! reports how many landed merges removed or added lines that neither
//! side did. Read-only: it clones nothing, fetches nothing, and writes
//! nothing anywhere.
//!
//! ```text
//! choir-revert-scan [--max N] [--quiet] <repo>...
//! ```
//!
//! `--max N` caps the merges examined per repository (0, the default, is
//! all of them). `--quiet` prints the counts without the individual
//! findings, which is what you want across fifty repositories and not
//! what you want before quoting the number at anybody: the findings are
//! where the false positives are visible.
//!
//! Exit status is 0 whether or not anything is found. **Both outcomes
//! are answers** -- the kill criterion was fixed before the number was
//! known -- so a zero here is a result, not a failure, and must not be
//! wired to a gate that treats it as one.

use choir_merge::silent_revert::{scan_repo, ScanReport};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut max = 0usize;
    let mut quiet = false;
    let mut repos: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--max" => match it.next().and_then(|n| n.parse().ok()) {
                Some(n) => max = n,
                None => fail("--max wants a number"),
            },
            "--quiet" => quiet = true,
            other if other.starts_with("--") => fail(&format!("unknown flag {other}")),
            other => repos.push(other.to_string()),
        }
    }
    if repos.is_empty() {
        fail("usage: choir-revert-scan [--max N] [--quiet] <repo>...");
    }

    let mut total = ScanReport::default();
    for repo in &repos {
        match scan_repo(std::path::Path::new(repo), max) {
            Ok(report) => {
                println!(
                    "{repo}: {} merges scanned of {} seen, {} findings across {} paths",
                    report.merges_scanned,
                    report.merges_seen,
                    report.findings.len(),
                    report.paths_scanned
                );
                total.absorb(report);
            }
            // One unreadable repository must not cost the other
            // forty-nine. It is counted by being named here and by not
            // appearing in the totals.
            Err(e) => eprintln!("{repo}: skipped, {e}"),
        }
    }

    if !quiet {
        for f in &total.findings {
            println!("\n{} {}", &f.merge[..f.merge.len().min(12)], f.path);
            for line in &f.reverted {
                println!("  reverted: {line}");
            }
            for line in &f.injected {
                println!("  injected: {line}");
            }
        }
    }

    println!("\n  repositories       {}", repos.len());
    println!("  merges seen        {}", total.merges_seen);
    println!("  merges scanned     {}", total.merges_scanned);
    println!("  paths scanned      {}", total.paths_scanned);
    println!("  skipped octopus    {}", total.skipped_octopus);
    println!("  skipped no base    {}", total.skipped_no_base);
    println!("  skipped binary     {}", total.skipped_binary);
    println!("  skipped large      {}", total.skipped_large);
    println!("  lines relocated    {}", total.lines_relocated);
    println!("  paths all moved    {}", total.paths_all_relocated);
    println!("  lines surviving    {}", total.lines_surviving_elsewhere);
    println!("  findings           {}", total.findings.len());
    println!("  of those, surviving {}", total.findings_all_surviving);
    println!("  merges violating   {:.4}", total.merge_violation_rate());
    // Said every run, because the number is the part that travels and
    // this is the part that keeps it honest.
    println!(
        "\n  A finding is evidence, not proof: a merge whose conflicts a\n  \
         human resolved by hand may legitimately drop lines neither\n  \
         parent dropped. \"Surviving\" counts reverted lines still\n  \
         present somewhere in the result, which is what a refactor that\n  \
         moves code looks like; they are counted and still reported,\n  \
         because presence is a cheap test a short line passes by\n  \
         accident. The true rate is between the two.\n  \
         Look at the findings before quoting either number."
    );
}

/// Prints `msg` and exits 2, the way a usage error should.
fn fail(msg: &str) -> ! {
    eprintln!("choir-revert-scan: {msg}");
    std::process::exit(2);
}
