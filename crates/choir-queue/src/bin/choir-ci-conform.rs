//! Runs the D18 executor conformance suite against a helper argv.
//!
//! `choir-ci-local` is gated by `cargo test`, because cargo can build
//! it. The helper the seam exists for cannot be gated that way: a
//! Firecracker or Cloud Hypervisor driver needs KVM, so it lives on a
//! Linux machine and quite possibly in another repository, and a suite
//! that only runs against what this workspace links would let it ship
//! ungated. This binary is the suite with the helper as an argument.
//!
//! ```text
//! choir-ci-conform [options] -- <helper> [helper args...]
//!
//!   --subject <hex>          subject for every fixture job, so a helper
//!                            that materializes the subject is given one
//!                            it can materialize (default: a blake3 hash
//!                            of "conform", which suits a helper that
//!                            ignores the field)
//!   --shell <path>           what runs the fixture scripts on the far
//!                            side (default: /bin/sh)
//!   --deadline-ms <n>        deadline for the jobs that must finish
//!                            (default: 600000); raise it for a helper
//!                            that boots a machine per job
//!   --slow-deadline-ms <n>   deadline for the job that must time out
//!                            (default: 100)
//!   --probe-dir <dir>        a directory the far side can see, holding
//!                            nothing; enables the Job::directory check,
//!                            which is skipped without it
//! ```
//!
//! Exit 0 when every check that ran passed, 1 when any failed, 2 on a
//! usage error. Skipped checks do not fail the run and are printed as
//! what they are: the report says what it could not establish rather
//! than counting it as evidence.

use choir_queue::conform::{conform, Fixtures, Outcome};
use choir_queue::executor::Job;
use choir_queue::remote::ProtocolRunner;
use std::time::Duration;

const MARKER: &str = "choir-conform-marker";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opts = match parse(&args) {
        Ok(o) => o,
        Err(why) => {
            eprintln!("choir-ci-conform: {why}");
            eprintln!("usage: choir-ci-conform [options] -- <helper> [args...]");
            std::process::exit(2);
        }
    };

    let sh = |script: &str| {
        let mut job = Job::new(
            opts.subject.clone(),
            vec![opts.shell.clone(), "-c".into(), script.into()],
        );
        job.deadline = opts.deadline;
        job
    };
    let mut slow = sh("sleep 30");
    slow.deadline = opts.slow_deadline;

    let in_directory = opts.probe_dir.as_ref().map(|dir| {
        if let Err(e) = std::fs::write(dir.join(MARKER), b"here") {
            eprintln!("choir-ci-conform: cannot write the probe marker in {dir:?}: {e}");
            std::process::exit(2);
        }
        // The probe passes only from inside that directory, so the
        // verdict *is* the assertion about where the job ran.
        let mut job = sh(&format!("test -e {MARKER}"));
        job.directory = Some(dir.clone());
        job
    });

    let checks = conform(
        &mut ProtocolRunner::new(opts.helper.clone()),
        Fixtures {
            passing: sh("exit 0"),
            failing: sh("exit 3"),
            // A path that cannot exist, so the failure is at spawn.
            erroring: Job::new(
                opts.subject.clone(),
                vec!["/nonexistent/choir-not-a-command".into()],
            ),
            slow,
            in_directory,
        },
    );

    let mut failed = 0;
    let mut skipped = 0;
    for check in &checks {
        println!("{check}");
        match check.outcome {
            Outcome::Failed(_) => failed += 1,
            Outcome::Skipped(_) => skipped += 1,
            Outcome::Passed => {}
        }
    }
    println!(
        "{}: {} passed, {failed} failed, {skipped} skipped",
        opts.helper[0],
        checks.len() - failed - skipped
    );
    std::process::exit(i32::from(failed > 0));
}

struct Options {
    helper: Vec<String>,
    subject: choir_hash::ContentHash,
    shell: String,
    deadline: Duration,
    slow_deadline: Duration,
    probe_dir: Option<std::path::PathBuf>,
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut opts = Options {
        helper: Vec::new(),
        subject: choir_hash::ContentHash::blake3(b"conform"),
        shell: "/bin/sh".to_string(),
        deadline: choir_queue::executor::DEFAULT_DEADLINE,
        slow_deadline: Duration::from_millis(100),
        probe_dir: None,
    };
    let mut i = 0;
    while i < args.len() {
        let value = |name: &str| {
            args.get(i + 1)
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match args[i].as_str() {
            "--" => {
                opts.helper = args[i + 1..].to_vec();
                break;
            }
            "--subject" => {
                let hex = value("--subject")?;
                opts.subject = choir_hash::ContentHash::from_hex(&hex)
                    .ok_or_else(|| format!("--subject {hex} is not a content hash"))?;
                i += 2;
            }
            "--shell" => {
                opts.shell = value("--shell")?;
                i += 2;
            }
            "--deadline-ms" => {
                opts.deadline = Duration::from_millis(millis(&value("--deadline-ms")?)?);
                i += 2;
            }
            "--slow-deadline-ms" => {
                opts.slow_deadline = Duration::from_millis(millis(&value("--slow-deadline-ms")?)?);
                i += 2;
            }
            "--probe-dir" => {
                let dir = std::path::PathBuf::from(value("--probe-dir")?);
                if !dir.is_dir() {
                    return Err(format!("--probe-dir {dir:?} is not a directory"));
                }
                opts.probe_dir = Some(dir);
                i += 2;
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    if opts.helper.is_empty() {
        return Err("no helper: pass it after `--`".to_string());
    }
    Ok(opts)
}

fn millis(raw: &str) -> Result<u64, String> {
    raw.parse()
        .map_err(|_| format!("`{raw}` is not a number of milliseconds"))
}
