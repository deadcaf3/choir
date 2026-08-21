//! Git subprocesses per page, as a budget rather than a stopwatch.
//!
//! Phase 1 asks for a read p99 under 100 ms. `phase1_reads.rs` measures
//! that and reports it, but it cannot *gate* on it: the same repository
//! page measured p99 94 ms and p99 246 ms an hour apart on the same
//! commit, because the machine had a toolchain build on it the second
//! time. A tripwire that fires on the weather gets disabled, which is
//! worse than not having one.
//!
//! What does not move with the weather is the number of times rendering
//! a page shells out to git. That is also the thing the latency is made
//! of — a spawn costs around 10 ms here, and the pages cost roughly
//! their spawn count times that — so budgeting spawns gates the target
//! by gating its cause. `tests/alloc_budget.rs` makes the same argument
//! about allocations and is the pattern this follows.
//!
//! **The numbers are a ratchet, and it holds in both directions.** Each
//! page carries the count it measures today and the budget just above
//! it, and a page outside that window fails either way. Above is the
//! obvious finding: the page got more expensive. Below is the one that
//! caught a hole in this file — a mutation deleting the counter's
//! `fetch_add` left every page reading zero spawns, and a one-sided
//! budget passed all of them while measuring nothing at all. So a drop
//! is a finding too: either the page really did get cheaper, in which
//! case lower both numbers in the same commit and say what did it, or
//! the counter stopped counting and this file went quietly hollow.
//!
//! Its own binary because the counter is process-global, and the merged
//! harness runs its modules on parallel threads — the same reason
//! `alloc_budget` is its own binary.

use choir_identity::{ActorKey, Registry};
use choir_node::{Node, Platform};
use choir_oplog::MemLog;

fn git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
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

/// One `GET`, and the number of git spawns the node made serving it.
fn spawns(base: &str, path: &str) -> u64 {
    let before = choir_node::git_invocations();
    let out = std::process::Command::new("curl")
        .args([
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            &format!("{base}{path}"),
        ])
        .output()
        .expect("curl runs");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "200",
        "{path} did not answer 200"
    );
    choir_node::git_invocations() - before
}

#[test]
fn a_page_render_stays_inside_its_git_spawn_budget() {
    let work = std::env::temp_dir().join("choir-node-phase1-spawns");
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");

    let mut node = Node::bind(&work.join("repos"), 0).expect("node binds");
    node.create_repo("agents/one.git").expect("repo created");
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
    std::fs::write(seed.join("README.md"), "# a repository\n\nwith a readme.\n").unwrap();
    assert!(git(&seed, &["add", "."]).status.success());
    assert!(git(&seed, &["commit", "-q", "-m", "a tree with substance"])
        .status
        .success());
    assert!(git(&seed, &["push", "-q", "origin", "HEAD:main"])
        .status
        .success());

    // Warm: the first render of a repository pays for things a second
    // one does not, and budgeting the cold path would budget a cost no
    // reader after the first ever pays.
    for path in ["/r/agents/one/", "/r/agents/one/tree/main/src"] {
        let _ = spawns(&base, path);
    }

    println!("== Phase-1 git-spawn budget ==");
    let mut outside = Vec::new();
    for (path, measured, budget) in [
        // The repository front page: resolve, default branch, tree
        // listing, refs, commit count, readme, and one walk for the
        // listing's dates. The walk is one call for the whole listing --
        // it was one per row until the read measurement found it.
        ("/r/agents/one/", 9u64, 10u64),
        // A forty-file directory, at three. It is *cheaper* than the
        // root, which carries refs, a commit count and a readme that a
        // subdirectory does not -- and it does not grow with the number
        // of files, because the dates are one walk. A per-row query put
        // this over forty, which is what this number exists to stop
        // coming back.
        ("/r/agents/one/tree/main/src", 3, 4),
    ] {
        let used = spawns(&base, path);
        println!("{path}: {used} spawns (measured {measured}, budget {budget})");
        if used > budget {
            outside.push(format!(
                "{path} spent {used} git spawns, over its budget of {budget} -- \
                 the page got more expensive, and spawns are what the read \
                 latency is made of"
            ));
        }
        if used < measured {
            outside.push(format!(
                "{path} spent {used} git spawns, under the {measured} it is \
                 recorded as costing -- either it genuinely got cheaper, and \
                 both numbers move in this commit, or the counter stopped \
                 counting and this file is no longer measuring anything"
            ));
        }
    }
    assert!(outside.is_empty(), "{outside:#?}");
}
