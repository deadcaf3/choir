//! Seeds, not peers (D80): a home and a seed in one process, the home
//! driven by real `git push` and `curl`, the seed brought up to date by
//! calling `replicate_once` directly.
//!
//! Nothing sleeps and nothing binds a fixed port. Every temp directory is
//! named for its test, since the harness runs modules on parallel threads
//! in one process.

use choir_identity::{ActorKey, Registry};
use choir_node::platform::hex_encode;
use choir_node::replica::{
    self, Credential, Relation, Replica, ReplicaError, SignedStatement, SnapshotChain,
    WitnessStatement,
};
use choir_node::{AuthTable, Node, Platform};
use choir_oplog::MemLog;
use choir_view::{OpKind, ViewOp};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::support::{curl, submit_body};

/// The operator name the home binds the seed's key to, and the principal
/// its credential authenticates as.
const SEED: &str = "seed-a";

fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
            "-c",
            "credential.helper=",
            "-c",
            "credential.interactive=false",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git runs")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn scratch(tag: &str) -> PathBuf {
    let work = std::env::temp_dir().join(format!("choir-seed-{tag}-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("scratch dir");
    work
}

/// A served node, and the platform it serves, held twice so a test can
/// reach it without going through HTTP.
struct Served {
    url: String,
    host: String,
    platform: Arc<Platform>,
}

fn serve(
    root: &Path,
    auth: &[(&str, &str)],
    acl: Option<&Path>,
    platform: Arc<Platform>,
) -> Served {
    let mut table = AuthTable::new();
    for (user, token) in auth {
        table.insert((*user).to_string(), (*token).to_string());
    }
    let mut node = Node::bind_with_auth(root, 0, Some(table)).expect("node binds a free port");
    if let Some(acl) = acl {
        node.watch_acl_file(acl.to_path_buf()).expect("acl loads");
    }
    node.enable_shared_platform(platform.clone());
    let port = node.port();
    let node = Arc::new(node);
    std::thread::spawn(move || node.serve_forever());
    Served {
        url: format!("http://127.0.0.1:{port}"),
        host: format!("127.0.0.1:{port}"),
        platform,
    }
}

/// A home: one repository, alice who pushes, and the seed's principal
/// holding the two grants a seed needs — the node-wide read that reads
/// the log, and read on the repositories it should hold.
struct HomeFixture {
    work: PathBuf,
    served: Served,
    node_key: ActorKey,
    alice: ActorKey,
    seed_key: ActorKey,
}

fn home(tag: &str) -> HomeFixture {
    let work = scratch(tag);
    let alice = ActorKey::generate();
    let seed_key = ActorKey::generate();
    // The operator's three lines, in the shapes they already have: the
    // seed's key registered, and two ordinary grants for its principal.
    let keys = work.join("keys");
    std::fs::write(
        &keys,
        format!(
            "alice {}\n{} {}\n",
            hex(&alice.public_key_bytes()),
            SEED,
            hex(&seed_key.public_key_bytes())
        ),
    )
    .expect("keys file");
    let acl = work.join("acl");
    std::fs::write(
        &acl,
        format!("alice * write\nalice @node write\n{SEED} @node auditor\n{SEED} * read\n"),
    )
    .expect("acl file");

    let node_key = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&alice.public_key_bytes()).unwrap();
    let platform = Arc::new(
        Platform::start_reloading(
            registry,
            Box::new(MemLog::new()),
            ActorKey::from_secret_bytes(&node_key.secret_bytes()),
            Some(keys),
        )
        .expect("home platform starts"),
    );
    let root = work.join("home");
    std::fs::create_dir_all(&root).unwrap();
    let served = serve(&root, &[("alice", "a"), (SEED, "s")], Some(&acl), platform);
    // Repository creation appends nothing, so the first op below is the
    // binding.
    let (status, body) = curl(&[
        "-u",
        "alice:a",
        "-d",
        r#"{"name":"agents/demo.git"}"#,
        &format!("{}/api/repo", served.url),
    ]);
    assert!(status < 300, "repo create: {status} {body}");

    let fixture = HomeFixture {
        work,
        served,
        node_key,
        alice,
        seed_key,
    };
    // The third line: bind the seed's key to its operator, which only the
    // node may author.
    fixture.submit_as_node(&ViewOp::new(OpKind::BindKey {
        operator: SEED.into(),
        key: fixture.seed_key.actor_id(),
        channel: None,
    }));
    fixture
}

impl HomeFixture {
    fn submit_as_node(&self, op: &ViewOp) -> u64 {
        let (code, body) = self.served.platform.handle_api(
            "POST",
            "/api/submit",
            submit_body(&self.node_key, "node", op).as_bytes(),
        );
        let value: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(code, 200, "{value}");
        value["seq"].as_u64().expect("seq")
    }

    /// Clones, commits `body` and pushes to main as alice. Returns the
    /// commit id.
    fn push(&self, clone: &str, body: &str) -> String {
        let url = format!("http://alice:a@{}/agents/demo.git", self.served.host);
        let dir = self.work.join(clone);
        if !dir.exists() {
            let cloned = git(&self.work, &["clone", "-q", &url, clone]);
            assert!(
                cloned.status.success(),
                "{}",
                String::from_utf8_lossy(&cloned.stderr)
            );
        }
        std::fs::write(dir.join("f.txt"), body).unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-q", "-m", body]);
        let pushed = git(&dir, &["push", "-q", "origin", "HEAD:main"]);
        assert!(
            pushed.status.success(),
            "{}",
            String::from_utf8_lossy(&pushed.stderr)
        );
        String::from_utf8_lossy(&git(&dir, &["rev-parse", "HEAD"]).stdout)
            .trim()
            .to_string()
    }

    fn view(&self) -> serde_json::Value {
        let (status, body) = curl(&["-u", "alice:a", &format!("{}/api/view", self.served.url)]);
        assert_eq!(status, 200, "{body}");
        body
    }
}

/// A seed of `home_url`, with its own root, key, credential file and
/// served surface. `reader:r` is the one credential it issues.
struct SeedFixture {
    root: PathBuf,
    replica: Arc<Replica>,
    served: Served,
}

fn seed(work: &Path, name: &str, home_url: &str, key: ActorKey) -> SeedFixture {
    let root = work.join(name);
    std::fs::create_dir_all(&root).unwrap();
    let credential_file = work.join(format!("{name}.credential"));
    std::fs::write(&credential_file, format!("{SEED}:s\n")).unwrap();
    let credential = Credential::read(&credential_file).expect("credential");
    let home = replica::contact(&root, home_url, &credential).expect("first contact pins");
    let platform = Arc::new(
        Platform::start(Registry::new(), Box::new(MemLog::new()), key)
            .expect("seed platform starts")
            .as_seed_of(home.clone()),
    );
    let replica = Arc::new(Replica::new(
        root.clone(),
        home,
        credential,
        platform.clone(),
    ));
    let served = serve(&root, &[("reader", "r")], None, platform);
    SeedFixture {
        root,
        replica,
        served,
    }
}

impl SeedFixture {
    fn view(&self) -> serde_json::Value {
        let (status, body) = curl(&["-u", "reader:r", &format!("{}/api/view", self.served.url)]);
        assert_eq!(status, 200, "{body}");
        body
    }
}

#[test]
fn a_seed_takes_the_homes_log_verified_and_serves_the_same_view_and_objects() {
    let home = home("replicates");
    let commit = home.push("pusher", "one\n");

    // The endpoint a seed learns keys from, verbatim in the report.
    let (status, signers) = curl(&[
        "-u",
        &format!("{SEED}:s"),
        &format!("{}/api/signers", home.served.url),
    ]);
    assert_eq!(status, 200, "{signers}");
    println!("GET /api/signers on the home:\n{signers}");
    assert_eq!(signers["format_version"], 1);
    assert_eq!(
        signers["node"]["actor_id"],
        home.node_key.actor_id().to_hex().as_str()
    );
    assert_eq!(
        signers["node"]["public_key_hex"],
        hex_encode(&home.node_key.public_key_bytes()).as_str()
    );
    let listed: Vec<&str> = signers["signers"]
        .as_array()
        .expect("signers")
        .iter()
        .map(|row| row["actor_id"].as_str().expect("actor id"))
        .collect();
    assert_eq!(
        listed,
        [
            home.alice.actor_id().to_hex().as_str(),
            home.seed_key.actor_id().to_hex().as_str()
        ]
    );

    let seed = seed(&home.work, "seed", &home.served.url, ActorKey::generate());
    let progress = seed.replica.replicate_once().expect("a clean round");
    let home_view = home.view();
    let seed_view = seed.view();

    // The same position, the same refs, the same attestation.
    let at = home_view["log"]["next_seq"]
        .as_u64()
        .expect("home position");
    assert!(at >= 3, "binding, push and attestation: {home_view}");
    assert_eq!(seed_view["log"]["next_seq"], at, "{seed_view}");
    assert_eq!(seed_view["refs"], home_view["refs"]);
    assert_eq!(seed_view["snapshot"]["id"], home_view["snapshot"]["id"]);
    assert_eq!(seed_view["log"]["head"], home_view["log"]["head"]);
    // A scope signed against the seed's view names the home, so it is one
    // the home will admit.
    assert_eq!(seed_view["log"]["node"], home_view["log"]["node"]);

    let status = &progress.status;
    assert_eq!(status.head_seq, Some(at - 1));
    assert_eq!(status.home_head_seq, status.head_seq);
    assert_eq!((status.refs_verified, status.refs_pending), (1, 0));
    println!(
        "unverified entries on a fresh home: {} of {at}",
        status.unverified_entries
    );
    assert_eq!(
        status.unverified_entries, 0,
        "every entry on a fresh home is signed by its node key, which /api/signers lists"
    );
    assert!(!status.gap && status.halted.is_none(), "{status:?}");
    // The seed says so in its view, and only a seed does.
    assert!(home_view["replica"].is_null(), "a home is nobody's copy");
    let replica = &seed.view()["replica"];
    println!("replica on the seed: {replica}");
    assert_eq!(replica["format_version"], 1);
    assert_eq!(replica["home"], home.served.url.as_str());
    assert_eq!(
        replica["home_node_id"],
        home.node_key.actor_id().to_hex().as_str()
    );
    assert_eq!(replica["head_seq"], at - 1);
    assert_eq!(replica["home_head_seq"], at - 1);
    assert_eq!(replica["refs_verified"], 1);
    assert_eq!(replica["refs_pending"], 0);
    assert_eq!(replica["unverified_entries"], 0);
    assert_eq!(replica["gap"], false);
    assert!(replica["halted"].is_null(), "{replica}");
    // And on its landing page, in one line.
    let page = std::process::Command::new("curl")
        .args([
            "-s",
            "-u",
            "reader:r",
            &format!("{}/status", seed.served.url),
        ])
        .output()
        .expect("curl runs");
    let page = String::from_utf8_lossy(&page.stdout);
    assert!(
        page.contains(&format!("seed of {}, 0 behind", home.served.url)),
        "{page}"
    );

    // The home's key is pinned beside the seed's log.
    let pin = std::fs::read_to_string(seed.root.join(".choir/home.fingerprint")).expect("pin");
    assert_eq!(pin.trim(), home.node_key.actor_id().to_hex());

    // And the objects: a clone from the seed yields the home's commit.
    let url = format!("http://reader:r@{}/agents/demo.git", seed.served.host);
    let cloned = git(&home.work, &["clone", "-q", &url, "from-seed"]);
    assert!(
        cloned.status.success(),
        "{}",
        String::from_utf8_lossy(&cloned.stderr)
    );
    let head = git(&home.work.join("from-seed"), &["rev-parse", "HEAD"]);
    assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), commit);

    // A second push, a second round: only the new entries move.
    home.push("pusher", "two\n");
    let progress = seed.replica.replicate_once().expect("a second round");
    assert_eq!(progress.appended, 2, "one ref op and its attestation");
    assert_eq!(seed.view()["refs"], home.view()["refs"]);
    assert_eq!(progress.status.refs_pending, 0);
}

/// Serves the home's own answers, except that one entry of `/api/log`
/// has a byte of its payload flipped: a home, or anything between it and
/// the seed, lying about one entry.
fn tampering(home: &str, flip: u64) -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("a free port");
    let port = server.server_addr().to_ip().expect("ip").port();
    let home = home.to_string();
    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            let url = request.url().to_string();
            let (status, mut body) = curl(&["-u", &format!("{SEED}:s"), &format!("{home}{url}")]);
            if url.starts_with("/api/log") {
                for entry in body["entries"].as_array_mut().into_iter().flatten() {
                    if entry["seq"] == flip {
                        let payload = entry["payload_hex"].as_str().expect("payload").to_string();
                        let first = if payload.starts_with('7') { '6' } else { '7' };
                        entry["payload_hex"] =
                            serde_json::json!(format!("{first}{}", &payload[1..]));
                    }
                }
            }
            let _ = request.respond(
                tiny_http::Response::from_string(body.to_string()).with_status_code(status),
            );
        }
    });
    format!("http://127.0.0.1:{port}")
}

#[test]
fn a_tampered_entry_halts_replication_there_and_the_prefix_is_still_served() {
    let home = home("tamper");
    home.push("pusher", "one\n");
    home.push("pusher", "two\n");
    let end = home.view()["log"]["next_seq"].as_u64().expect("position");
    // The second push's ref op: after the binding, the first push and its
    // attestation.
    let flip = 3;
    assert!(end > flip + 1, "the flipped entry is not the last: {end}");

    let seed = seed(
        &home.work,
        "seed",
        &tampering(&home.served.url, flip),
        ActorKey::generate(),
    );
    let halted = seed.replica.replicate_once().expect_err("a tampered page");
    let ReplicaError::Halted { seq, reason } = &halted else {
        panic!("not a halt: {halted:?}");
    };
    println!("tampered page: {halted}");
    assert_eq!(*seq, flip);
    assert!(reason.contains("does not hash to"), "{reason}");

    // The prefix is held and served, and the halt is the reader's to see.
    let view = seed.view();
    assert_eq!(view["log"]["next_seq"], flip, "{view}");
    let status = seed.replica.status();
    assert_eq!(status.head_seq, Some(flip - 1));
    assert_eq!(status.halted.as_ref().map(|(seq, _)| *seq), Some(flip));
    println!("replica after the halt: {}", view["replica"]);
    assert_eq!(view["replica"]["halted"]["seq"], flip);
    assert_eq!(view["replica"]["head_seq"], flip - 1);
    assert_eq!(
        view["replica"]["home_head_seq"],
        end - 1,
        "how far the home got is still reported"
    );

    // And it stays halted: the next round does not skip past it.
    let again = seed.replica.replicate_once().expect_err("still halted");
    assert_eq!(again, halted);
    assert_eq!(seed.view()["log"]["next_seq"], flip);
}

#[test]
fn a_seed_refuses_a_home_signing_with_another_key_than_it_pinned() {
    let first = home("pin-first");
    let second = home("pin-second");
    let root = first.work.join("seed");
    std::fs::create_dir_all(&root).unwrap();
    let credential = Credential::parse(&format!("{SEED}:s")).unwrap();

    let met = replica::contact(&root, &first.served.url, &credential).expect("first contact");
    assert_eq!(met.node_id, first.node_key.actor_id());
    // The same home again is the ordinary restart.
    replica::contact(&root, &first.served.url, &credential).expect("same home, same key");

    let refused = replica::contact(&root, &second.served.url, &credential)
        .expect_err("a home with another node key");
    assert!(refused.contains("home identity changed"), "{refused}");
    assert!(
        refused.contains(&first.node_key.actor_id().to_hex()),
        "names the pinned key: {refused}"
    );
}

#[test]
fn every_write_at_a_seed_is_answered_with_its_home() {
    let home = home("not-home");
    home.push("pusher", "one\n");
    let seed = seed(&home.work, "seed", &home.served.url, ActorKey::generate());
    seed.replica.replicate_once().expect("a clean round");

    // A real push to the seed fails, and git hands the pusher the home.
    let clone = home.work.join("pusher");
    let to_seed = format!("http://reader:r@{}/agents/demo.git", seed.served.host);
    std::fs::write(clone.join("f.txt"), "from the wrong place\n").unwrap();
    git(&clone, &["commit", "-qam", "wrong place"]);
    let pushed = git(&clone, &["push", &to_seed, "HEAD:main"]);
    let stderr = String::from_utf8_lossy(&pushed.stderr);
    println!("git push to a seed:\n{stderr}");
    assert!(!pushed.status.success(), "a seed took a push: {stderr}");
    assert!(stderr.contains(&home.served.url), "{stderr}");
    assert!(stderr.contains("not_home"), "{stderr}");

    // A signed op, well-formed and admissible at home, is routed there.
    let op = ViewOp::new(OpKind::RecordProvenance {
        subject: "agents/demo/ws".into(),
        kind: "note".into(),
        body: String::new(),
    });
    let (status, body) = curl(&[
        "-u",
        "reader:r",
        "-d",
        &submit_body(&home.alice, "alice", &op),
        &format!("{}/api/submit", seed.served.url),
    ]);
    println!("POST /api/submit to a seed: {status} {body}");
    assert_eq!(status, 421, "{body}");
    assert_eq!(body["code"], "not_home");
    assert_eq!(body["home"], home.served.url.as_str());
    assert!(body["next"]
        .as_str()
        .is_some_and(|next| next.contains(&home.served.url)));

    // The same for every other write route, before any of them acts.
    // `/api/git-update` is not here: without the loopback secret it is
    // refused as an internal endpoint first, and with it, it is only ever
    // reached by the push the seed has already refused above.
    for path in [
        "/api/submit-batch",
        "/api/repo",
        "/api/workspace",
        "/api/accounts/invite",
    ] {
        let (status, body) = curl(&[
            "-u",
            "reader:r",
            "-d",
            "{}",
            &format!("{}{path}", seed.served.url),
        ]);
        assert_eq!(
            (status, body["code"].as_str()),
            (421, Some("not_home")),
            "{path}: {body}"
        );
    }

    // And the writer itself refuses, whatever path a write took to it.
    let (_, refused) = seed.served.platform.handle_api(
        "POST",
        "/api/submit",
        submit_body(&home.alice, "alice", &op).as_bytes(),
    );
    assert!(refused.contains("not_home"), "{refused}");

    // Reads are untouched, and nothing above reached the seed's log.
    assert_eq!(
        seed.view()["log"]["next_seq"],
        home.view()["log"]["next_seq"]
    );
}

impl HomeFixture {
    /// The home's attestation chain, read from its own log by a reader
    /// holding the node-wide grant, from `from` to the head.
    fn chain(&self, from: u64) -> SnapshotChain {
        let mut chain = SnapshotChain::default();
        let mut cursor = from;
        loop {
            let (status, page) = curl(&[
                "-u",
                "alice:a",
                &format!("{}/api/log?from={cursor}", self.served.url),
            ]);
            assert_eq!(status, 200, "{page}");
            let entries = page["entries"].as_array().expect("entries").clone();
            let Some(last) = entries.last() else {
                return chain;
            };
            cursor = last["seq"].as_u64().expect("seq") + 1;
            chain.read_page(&entries);
        }
    }
}

/// `GET /api/witness` on `seed`, as JSON.
fn witness(seed: &SeedFixture) -> serde_json::Value {
    let (status, body) = curl(&[
        "-u",
        "reader:r",
        &format!("{}/api/witness", seed.served.url),
    ]);
    assert_eq!(status, 200, "{body}");
    body
}

#[test]
fn a_seeds_statement_endorses_the_homes_chain_and_a_forged_one_is_a_fork() {
    let home = home("witness");
    home.push("pusher", "one\n");
    // One key: the key the home registered and bound as `seed-a` is the
    // seed's node key, so the statement's witness is that operator.
    let seed_key = ActorKey::from_secret_bytes(&home.seed_key.secret_bytes());
    let seed = seed(&home.work, "seed", &home.served.url, seed_key);
    seed.replica.replicate_once().expect("a clean round");

    let served = witness(&seed);
    println!("GET /api/witness on the seed: {served}");
    assert_eq!(served["format_version"], 1);
    assert_eq!(served["home"], home.served.url.as_str());
    let first = SignedStatement::from_json(&served["latest"]).expect("a statement");
    first
        .verify()
        .expect("the statement verifies against the key it carries");
    assert_eq!(first.statement.witness, home.seed_key.actor_id().to_hex());
    assert_eq!(
        home.view()["bindings"][&first.statement.witness]["operator"],
        SEED,
        "the witness is the operator the home bound"
    );
    assert_eq!(
        first.statement.home_node_id,
        home.node_key.actor_id().to_hex()
    );

    // The fork rule, walked over the home's own log: the statement names
    // the home's current attestation.
    let current = home.view()["snapshot"]["id"]
        .as_str()
        .expect("an attestation")
        .to_string();
    let chain = home.chain(first.statement.at_seq);
    assert_eq!(
        chain.relate(&first.statement.snapshot, &current),
        Relation::Same
    );

    // The home moves on. The old statement is now an ancestor, which is
    // agreement: it endorses everything the new attestation extends.
    home.push("pusher", "two\n");
    let current = home.view()["snapshot"]["id"]
        .as_str()
        .expect("an attestation")
        .to_string();
    let chain = home.chain(first.statement.at_seq);
    assert_eq!(
        chain.relate(&first.statement.snapshot, &current),
        Relation::Behind(1)
    );
    seed.replica.replicate_once().expect("a second round");
    let served = witness(&seed);
    assert_eq!(served["history"].as_array().map(Vec::len), Some(2));
    let second = SignedStatement::from_json(&served["latest"]).expect("a statement");
    assert_eq!(second.statement.snapshot, current);
    // Two statements from one seed agree the same way.
    assert_eq!(
        chain.relate(&first.statement.snapshot, &second.statement.snapshot),
        Relation::Behind(1)
    );

    // A statement over an attestation that is not on the home's chain is
    // a fork, whoever signed it and however well: the signature is good,
    // and the history it vouches for is not the one this reader was shown.
    let forged = SignedStatement::sign(
        &ActorKey::from_secret_bytes(&home.seed_key.secret_bytes()),
        WitnessStatement {
            snapshot: choir_hash::ContentHash::blake3(b"a history nobody else saw").to_hex(),
            ..first.statement
        },
    );
    forged.verify().expect("well signed");
    assert_eq!(
        chain.relate(&forged.statement.snapshot, &current),
        Relation::Fork
    );

    // A tampered statement does not verify at all.
    let mut tampered = second;
    tampered.statement.at_seq += 1;
    assert!(tampered.verify().is_err(), "a changed statement verified");

    // The statements survive a restart: they are kept beside the log.
    let kept = std::fs::read_to_string(seed.root.join(".choir/witness.jsonl")).expect("kept");
    assert_eq!(kept.lines().count(), 2, "{kept}");
}
