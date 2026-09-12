//! Seeds from the client's side (D80): `seeds =` in `.choir/config`, a
//! write sent to a seed following its `not_home` answer once, and
//! `choir doctor`'s fork check against an honest home.
//!
//! A home and a seed in this process, on port 0, driven by the built
//! binary. The seed is brought up to date by calling `replicate_once`,
//! so nothing waits on its daemon loop.

use choir_identity::{ActorKey, Registry};
use choir_node::replica::{self, Credential, Replica};
use choir_node::{Node, Platform};
use choir_oplog::MemLog;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn run(dir: &Path, program: &str, args: &[&str]) -> std::process::Output {
    std::process::Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("runs")
}

fn choir(dir: &Path, args: &[&str]) -> std::process::Output {
    run(dir, env!("CARGO_BIN_EXE_choir"), args)
}

fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    let mut all = vec![
        "-c",
        "commit.gpgsign=false",
        "-c",
        "init.defaultBranch=main",
        "-c",
        "credential.helper=",
    ];
    all.extend_from_slice(args);
    run(dir, "git", &all)
}

/// A home with one pushed commit, a seed that has caught up with it, and
/// an agent key the home trusts.
struct Pair {
    work: PathBuf,
    home: String,
    seed: String,
    replica: Arc<Replica>,
    key_file: String,
}

fn serve(root: &Path, platform: Arc<Platform>, repo: Option<&str>) -> String {
    let mut node = Node::bind(root, 0).expect("node binds");
    if let Some(repo) = repo {
        node.create_repo(repo).expect("repo");
    }
    node.enable_shared_platform(platform);
    let port = node.port();
    std::thread::spawn(move || node.serve_forever());
    format!("http://127.0.0.1:{port}")
}

fn pair(tag: &str) -> Pair {
    let work = std::env::temp_dir().join(format!("choir-cli-seeds-{tag}-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("scratch dir");

    let key_file = work.join("agent.key");
    let out = choir(&work, &["key", key_file.to_str().expect("utf-8")]);
    assert!(out.status.success(), "key: {out:?}");
    let public = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let bytes: [u8; 32] = (0..public.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&public[i..i + 2], 16).expect("hex"))
        .collect::<Vec<u8>>()
        .try_into()
        .expect("32 bytes");
    let mut registry = Registry::new();
    registry.register(&bytes).expect("valid key");

    let home_root = work.join("home");
    let home = serve(
        &home_root,
        Arc::new(
            Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate())
                .expect("home starts"),
        ),
        Some("agents/demo.git"),
    );
    let clone = work.join("clone");
    let cloned = git(
        &work,
        &["clone", "-q", &format!("{home}/agents/demo.git"), "clone"],
    );
    assert!(cloned.status.success(), "{cloned:?}");
    std::fs::write(clone.join("f.txt"), "one\n").unwrap();
    git(&clone, &["add", "."]);
    git(&clone, &["commit", "-q", "-m", "one"]);
    let pushed = git(&clone, &["push", "-q", "origin", "HEAD:main"]);
    assert!(pushed.status.success(), "{pushed:?}");

    let seed_root = work.join("seed");
    std::fs::create_dir_all(&seed_root).unwrap();
    let credential = Credential::parse("seed-a:s").expect("credential");
    let met = replica::contact(&seed_root, &home, &credential).expect("first contact");
    let platform = Arc::new(
        Platform::start(
            Registry::new(),
            Box::new(MemLog::new()),
            ActorKey::generate(),
        )
        .expect("seed starts")
        .as_seed_of(met.clone()),
    );
    let replica = Arc::new(Replica::new(
        seed_root.clone(),
        met,
        credential,
        platform.clone(),
    ));
    let seed = serve(&seed_root, platform, None);
    replica.replicate_once().expect("the seed catches up");
    Pair {
        work,
        home,
        seed,
        replica,
        key_file: key_file.to_string_lossy().to_string(),
    }
}

fn next_seq(api: &str) -> u64 {
    let out = std::process::Command::new("curl")
        .args(["-s", &format!("{api}/api/view")])
        .output()
        .expect("curl runs");
    let view: serde_json::Value = serde_json::from_slice(&out.stdout).expect("view");
    view["log"]["next_seq"].as_u64().expect("position")
}

#[test]
fn a_write_sent_to_a_seed_lands_at_its_home_and_says_so() {
    let pair = pair("follow");
    let before = next_seq(&pair.home);
    let op = serde_json::json!({
        "format_version": 1,
        "kind": { "RecordProvenance": {
            "subject": "agents/demo/ws",
            "kind": "note",
            "body": "",
        }},
    })
    .to_string();

    // Signed against the seed's view, sent to the seed, admitted at home.
    let out = choir(
        &pair.work,
        &["submit", &pair.seed, &pair.key_file, "agent", &op],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    println!("choir submit to a seed:\n{stderr}");
    assert!(
        out.status.success(),
        "{stderr}{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        stderr.contains(&format!(
            "{} is a seed of {}; sent this to the home instead",
            pair.seed, pair.home
        )),
        "{stderr}"
    );
    assert_eq!(next_seq(&pair.home), before + 1, "it landed at the home");

    // And the seed takes it one round later, from the home, like anything else.
    pair.replica.replicate_once().expect("next round");
    assert_eq!(next_seq(&pair.seed), before + 1);
}

#[test]
fn the_doctor_runs_the_fork_check_for_each_configured_seed() {
    let pair = pair("doctor");
    std::fs::create_dir_all(pair.work.join(".choir")).unwrap();
    std::fs::write(
        pair.work.join(".choir/config"),
        format!("node = {}\nseeds = {}\n", pair.home, pair.seed),
    )
    .unwrap();
    let out = choir(
        &pair.work,
        &["doctor", "--state", &pair.work.to_string_lossy()],
    );
    let report = String::from_utf8_lossy(&out.stdout);
    println!("choir doctor with one seed:\n{report}");
    let row = report
        .lines()
        .find(|line| line.contains("fork check"))
        .unwrap_or_else(|| panic!("no fork check row: {report}"));
    assert!(row.contains("ok"), "{row}");
    assert!(row.contains(&pair.seed), "{row}");
    assert!(row.contains("agrees"), "{row}");
    assert!(!report.contains("fork:"), "{report}");
}
