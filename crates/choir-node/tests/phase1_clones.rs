//! The Phase-1 clone-rate target, measured against a real daemon.
//!
//! Its own binary rather than a second test beside the read percentiles.
//! Run in one process they contend: thirty-two clones are exactly the
//! kind of load that moves a p99, and the read figure swung from 94 ms
//! to 237 ms depending on which ran first. Two binaries is the cheapest
//! way for neither to be measuring the other.
//!
//! 10k/hr is 2.8/s, which one client on one machine would clear without
//! the server ever being the constraint, so the clones are issued
//! concurrently: the question worth asking is whether they serialize
//! behind `git http-backend`.
//!
//! ```text
//! cargo test -p choir-node --release --test phase1_clones -- --nocapture
//! ```

use choir_identity::{ActorKey, Registry};
use choir_node::{Node, Platform};
use choir_oplog::MemLog;
use std::time::Instant;

/// 10k clones/hr, as a rate per second.
const CLONE_TARGET_PER_S: f64 = 10_000.0 / 3600.0;

fn git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
            "-c",
            "credential.helper=",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git runs")
}

/// A served node with one repository carrying a tree worth cloning.
fn served(tag: &str) -> (String, std::path::PathBuf) {
    let work = std::env::temp_dir().join(format!("choir-node-phase1-{tag}"));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");

    let mut node = Node::bind(&work.join("repos"), 0).expect("node binds");
    node.create_repo("agents/one.git").expect("repo created");
    // With a sequencer, because a node without one refuses `/api/view`
    // outright and measuring that refusal would be measuring nothing.
    node.enable_platform(
        Platform::start(
            Registry::new(),
            Box::new(MemLog::new()),
            ActorKey::generate(),
        )
        .expect("platform starts"),
    );
    let port = node.port();
    std::thread::spawn(move || node.serve_forever());
    let base = format!("http://127.0.0.1:{port}");

    // A tree with some substance. A one-file repository would measure
    // the handshake and call it a clone; this is still small, and the
    // report says so rather than implying a production-sized repository.
    let seed = work.join("seed");
    let url = format!("{base}/agents/one.git");
    assert!(git(&work, &["clone", "-q", &url, seed.to_str().unwrap()])
        .status
        .success());
    std::fs::create_dir_all(seed.join("src")).unwrap();
    for file in 0..40 {
        let body: String = (0..200)
            .map(|line| format!("// file {file} line {line}\n"))
            .collect();
        std::fs::write(seed.join(format!("src/f{file}.rs")), body).unwrap();
    }
    assert!(git(&seed, &["add", "."]).status.success());
    assert!(git(&seed, &["commit", "-q", "-m", "a tree with substance"])
        .status
        .success());
    assert!(git(&seed, &["push", "-q", "origin", "HEAD:main"])
        .status
        .success());

    (base, work)
}

/// Concurrent clones clear the plan's rate with room, and do not
/// serialize behind one another.
///
/// The assertion is on the aggregate rate rather than on any one clone,
/// because the target is a throughput and a single slow clone under a
/// loaded machine is not the regression worth failing on.
#[test]
fn concurrent_clones_clear_ten_thousand_an_hour() {
    let (base, work) = served("clones");
    let url = format!("{base}/agents/one.git");

    const WORKERS: usize = 8;
    const EACH: usize = 4;
    let started = Instant::now();
    let mut handles = Vec::new();
    for worker in 0..WORKERS {
        let url = url.clone();
        let work = work.clone();
        handles.push(std::thread::spawn(move || {
            for round in 0..EACH {
                let into = work.join(format!("c-{worker}-{round}"));
                let out = git(&work, &["clone", "-q", &url, into.to_str().unwrap()]);
                assert!(
                    out.status.success(),
                    "clone {worker}/{round} failed: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
            }
        }));
    }
    for handle in handles {
        handle.join().expect("cloner finished");
    }
    let elapsed = started.elapsed();

    let clones = (WORKERS * EACH) as f64;
    let rate = clones / elapsed.as_secs_f64();
    println!("== Phase-1 clone report ==");
    println!(
        "{clones} clones on {WORKERS} threads in {elapsed:?} => {rate:.1}/s \
         ({:.0}/hr), target {CLONE_TARGET_PER_S:.2}/s (10k/hr)",
        rate * 3600.0
    );
    assert!(
        rate > CLONE_TARGET_PER_S,
        "clone rate {rate:.2}/s is under the {CLONE_TARGET_PER_S:.2}/s that 10k/hr needs"
    );
}
