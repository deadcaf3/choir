//! `choir seed` (D80): the decisions without a home, and one run with one.
//!
//! The argv is the contract, as for `choir node serve`: a seed started
//! with `--keys-file` is refused by the daemon, and one started without
//! `--seed` is a home over a copied log. Both are pinned here on the
//! plan rather than discovered by a supervisor restarting into them.

use choir_cli::seed::{self, registration};
use choir_cli::serve::{plan, Layout, Seed};
use choir_identity::{ActorKey, Registry};
use choir_node::{Node, Platform};
use choir_oplog::MemLog;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn scratch(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("choir-cli-seed-{tag}-{}", std::process::id()));
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(&root).expect("scratch dir");
    root
}

fn parse(args: &[&str]) -> Result<seed::Options, String> {
    seed::parse(args, PathBuf::from("/tmp/choir-seed-default"))
}

fn choir(dir: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_choir"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("choir runs")
}

// -- parsing ---------------------------------------------------------------

#[test]
fn a_seed_needs_a_home_url() {
    let error = parse(&[]).expect_err("no home");
    assert!(error.contains("choir seed https://<home>"), "{error}");
    let error = parse(&["home.example"]).expect_err("not a url");
    assert!(error.contains("http://"), "{error}");
}

#[test]
fn defaults_are_a_serving_seed_named_seed_on_the_usual_port() {
    let options = parse(&["https://home.example/"]).expect("parses");
    assert_eq!(options.home, "https://home.example");
    assert_eq!(options.port, 8417);
    assert_eq!(options.name, "seed");
    assert!(!options.archival);
    assert!(options.credential.is_none());
    assert!(options.state.is_absolute());
}

#[test]
fn every_flag_lands_and_daemon_flags_go_after_a_double_dash() {
    let options = parse(&[
        "https://home.example",
        "--credential",
        "cred",
        "--name",
        "seed-a",
        "--port",
        "9000",
        "--state",
        "st",
        "--foreground",
        "--",
        "--seed-strict",
    ])
    .expect("parses");
    assert_eq!(options.credential.as_deref(), Some(Path::new("cred")));
    assert_eq!(options.name, "seed-a");
    assert_eq!(options.port, 9000);
    assert!(options.state.ends_with("st"));
    assert!(options.foreground);
    assert_eq!(options.extra, vec!["--seed-strict".to_string()]);
}

#[test]
fn archival_and_a_port_contradict() {
    let error =
        parse(&["https://home.example", "--archival", "--port", "9000"]).expect_err("refused");
    assert!(error.contains("--archival"), "{error}");
    assert!(
        parse(&["https://home.example", "--archival"])
            .expect("parses")
            .archival
    );
}

#[test]
fn a_daemon_flag_in_the_wrong_place_is_named() {
    let error = parse(&["https://home.example", "--seed-strict"]).expect_err("refused");
    assert!(error.contains("after `--`"), "{error}");
}

// -- what the home registers -----------------------------------------------

#[test]
fn the_registration_is_a_keys_line_a_binding_and_two_grants() {
    let hex = "ab".repeat(32);
    let lines = registration("seed-a", &hex, "https://home.example");
    assert_eq!(lines.len(), 5);
    assert!(lines[0].contains(&format!("seed-a {hex}")));
    assert!(lines[1].contains("choir bind https://home.example"));
    assert!(lines[1].contains(&format!("seed-a {hex}")));
    assert!(lines[2].starts_with("auth file:   seed-a:"));
    assert!(lines[3].contains("seed-a @node auditor"));
    assert!(lines[4].contains("seed-a * read"));
}

#[test]
fn the_identity_is_minted_once_and_read_back_after() {
    let root = scratch("identity");
    let path = seed::key_path(&root.join("repos"));
    let first = seed::identity(&path).expect("minted");
    assert_eq!(std::fs::read(&path).expect("key file").len(), 32);
    let second = seed::identity(&path).expect("read back");
    assert_eq!(seed::public_hex(&first), seed::public_hex(&second));
    std::fs::write(&path, b"short").expect("corrupt it");
    let Err(error) = seed::identity(&path) else {
        panic!("a five-byte key file should be refused");
    };
    assert!(error.contains("32 bytes"), "{error}");
}

// -- the marker and the plan ------------------------------------------------

#[test]
fn the_marker_round_trips_and_a_torn_one_is_nothing() {
    let marker = Seed {
        home: "https://home.example".into(),
        credential: PathBuf::from("/s/seed-credential"),
        serve: true,
    };
    assert_eq!(Seed::parse(&marker.render()), Some(marker.clone()));
    assert_eq!(Seed::parse("home = https://home.example\n"), None);
    assert_eq!(
        Seed::parse("home = x\ncredential = y\nserve = maybe\n"),
        None
    );
    let archival = Seed {
        serve: false,
        ..marker
    };
    assert_eq!(Seed::parse(&archival.render()), Some(archival));
}

fn seeded(tag: &str, serve: bool) -> (PathBuf, Layout) {
    let state = scratch(tag);
    let layout = Layout::new(&state, 8418);
    std::fs::write(&layout.seed_credential, "seed-a:token\n").expect("credential");
    std::fs::write(
        &layout.seed_marker,
        Seed {
            home: "https://home.example".into(),
            credential: layout.seed_credential.clone(),
            serve,
        }
        .render(),
    )
    .expect("marker");
    (state, layout)
}

#[test]
fn a_serving_seed_plans_its_two_flags_and_no_keys_file() {
    let (_state, layout) = seeded("serving", true);
    std::fs::write(&layout.auth, "choir:t\n").expect("auth");
    std::fs::write(&layout.acl, "choir * own\n").expect("acl");
    let argv = plan(
        PathBuf::from("choir-node"),
        &layout,
        &[],
        &["--seed-strict".into()],
    )
    .expect("plans")
    .args;
    assert_eq!(argv[0], layout.repos.display().to_string());
    assert_eq!(argv[1], "8418");
    let at = |flag: &str| argv.iter().position(|a| a == flag);
    assert_eq!(
        argv[at("--seed").expect("seed") + 1],
        "https://home.example"
    );
    assert_eq!(
        argv[at("--seed-credential").expect("credential") + 1],
        layout.seed_credential.display().to_string()
    );
    assert!(at("--auth-file").is_some());
    assert!(at("--acl-file").is_some());
    assert!(at("--keys-file").is_none(), "{argv:?}");
    assert_eq!(argv.last().map(String::as_str), Some("--seed-strict"));
}

#[test]
fn an_archival_seed_binds_nothing_and_takes_only_its_own_flags() {
    let (_state, layout) = seeded("archival", false);
    std::fs::write(&layout.auth, "choir:t\n").expect("auth");
    let argv = plan(PathBuf::from("choir-node"), &layout, &[], &[])
        .expect("plans")
        .args;
    assert_eq!(
        argv,
        vec![
            layout.repos.display().to_string(),
            "--seed".to_string(),
            "https://home.example".to_string(),
            "--seed-credential".to_string(),
            layout.seed_credential.display().to_string(),
        ]
    );
}

#[test]
fn a_seed_refuses_create_and_keys_file() {
    let (_state, layout) = seeded("refuses", true);
    let error = plan(
        PathBuf::from("choir-node"),
        &layout,
        &["me/x.git".into()],
        &[],
    )
    .expect_err("refused");
    assert!(error.contains("no --create"), "{error}");
    let error = plan(
        PathBuf::from("choir-node"),
        &layout,
        &[],
        &["--keys-file".into(), "k".into()],
    )
    .expect_err("refused");
    assert!(error.contains("no --keys-file"), "{error}");
}

#[test]
fn a_seed_is_missing_only_its_credential() {
    let (_state, layout) = seeded("missing", true);
    assert!(
        layout.missing().is_empty(),
        "auth and keys are not required of a seed"
    );
    std::fs::remove_file(&layout.seed_credential).expect("remove");
    assert_eq!(layout.missing(), vec![layout.seed_credential.as_path()]);
}

// -- the command ------------------------------------------------------------

#[test]
fn the_first_run_mints_the_identity_and_hands_over() {
    let root = scratch("first-run");
    let state = root.join("state");
    let out = choir(
        &root,
        &[
            "seed",
            "http://127.0.0.1:1",
            "--name",
            "seed-a",
            "--state",
            state.to_str().expect("utf-8"),
        ],
    );
    assert_eq!(out.status.code(), Some(3), "{out:?}");
    let text = String::from_utf8_lossy(&out.stderr);
    let key = seed::key_path(&state.join("repos"));
    assert_eq!(std::fs::read(&key).expect("node key minted").len(), 32);
    let hex = seed::public_hex(&seed::identity(&key).expect("reads"));
    assert!(text.contains(&format!("seed-a {hex}")), "{text}");
    assert!(text.contains("seed-a @node auditor"), "{text}");
    assert!(text.contains("--credential <that file>"), "{text}");
    assert!(
        !state.join("seed").exists(),
        "no marker before a credential"
    );

    // Run again: the same identity, not a second one.
    let again = choir(
        &root,
        &[
            "seed",
            "http://127.0.0.1:1",
            "--name",
            "seed-a",
            "--state",
            state.to_str().unwrap(),
        ],
    );
    assert_eq!(again.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&again.stderr).contains(&format!("seed-a {hex}")));
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

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

/// The second run, against a real home, in the foreground. Needs the
/// daemon beside the `choir` under test, which a workspace build gives
/// it; a crate-only build says so and proves nothing.
#[test]
fn the_second_run_serves_a_seed_of_a_real_home() {
    if choir_cli::serve::find_daemon().is_err() {
        eprintln!(
            "skipped: no choir-node beside {}",
            env!("CARGO_BIN_EXE_choir")
        );
        return;
    }
    let root = scratch("second-run");
    let home = serve(
        &root.join("home"),
        Arc::new(
            Platform::start(
                Registry::new(),
                Box::new(MemLog::new()),
                ActorKey::generate(),
            )
            .expect("home starts"),
        ),
        Some("agents/demo.git"),
    );
    let credential = root.join("issued");
    std::fs::write(&credential, "seed-a:s\n").expect("credential");
    let state = root.join("state");
    let port = free_port();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_choir"))
        .args([
            "seed",
            &home,
            "--credential",
            credential.to_str().unwrap(),
            "--state",
            state.to_str().unwrap(),
            "--port",
            &port.to_string(),
            "--foreground",
        ])
        .current_dir(&root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawns");

    let url = format!("http://127.0.0.1:{port}");
    let layout = Layout::new(&state, port);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let view = loop {
        if std::time::Instant::now() > deadline {
            child.kill().ok();
            let mut err = String::new();
            std::io::Read::read_to_string(child.stderr.as_mut().unwrap(), &mut err).ok();
            panic!("the seed never answered on {url}:\n{err}");
        }
        if layout.auth.exists() {
            if let Ok(client) = choir_cli::mcp::HttpClient::new(&url, Some(&layout.auth), None) {
                if let Ok((200, body)) = client.get("/api/view") {
                    break serde_json::from_str::<serde_json::Value>(&body).expect("json");
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    };
    child.kill().ok();
    child.wait().ok();

    assert_eq!(
        view["replica"]["home"]["url"].as_str(),
        Some(home.as_str()),
        "{view}"
    );
    let marker = layout.seed().expect("marker written");
    assert_eq!(marker.home, home);
    assert!(marker.serve);
    assert_eq!(
        std::fs::read_to_string(&layout.seed_credential).expect("copied"),
        "seed-a:s\n"
    );
    let config = std::fs::read_to_string(root.join(".choir/config")).expect("config");
    assert!(config.contains(&format!("node = {home}")), "{config}");
    assert!(config.contains(&format!("seeds = {url}")), "{config}");
}
