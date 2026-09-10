//! `choir join` against a node that binds actor keys at redemption.
//!
//! The claim being tested is not "the request returned 200". It is that
//! the second out-of-band human step is gone: after one command, with no
//! operator touching a file in between, a key the contributor just
//! minted can sign an operation the node accepts. That is the assertion
//! at the end, and everything before it is setup.

use choir_identity::ActorKey;
use choir_identity::Registry;
use choir_node::{AuthTable, Node, Platform};
use choir_oplog::MemLog;

const ACL: &str = "alice @node write\nalice * write\n";

fn choir(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_choir"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("choir runs")
}

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
        // Without this git also reads the system config, which on macOS
        // names `osxkeychain` ahead of any `-c credential.helper`: it
        // answers `get` with whatever an earlier run stored for this
        // loopback port, and `store` blocks on the login keychain.
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git runs")
}

fn curl(args: &[&str]) -> (u16, String) {
    let out = std::process::Command::new("curl")
        .args(["-sS", "-w", "\n%{http_code}"])
        .args(args)
        .output()
        .expect("curl runs");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let (body, code) = text.rsplit_once('\n').expect("status line");
    (
        code.trim().parse().expect("numeric status"),
        body.to_string(),
    )
}

fn json(out: &std::process::Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        panic!(
            "json stdout, got {:?} / stderr {:?}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

struct Served {
    work: std::path::PathBuf,
    api: String,
    keys_path: std::path::PathBuf,
    /// The seeded commit, so a signed request has a base to bind.
    head: String,
}

/// A node with one operator credential, self-service accounts, and the
/// trusted-keys file both watched for reload and offered as the sink a
/// redemption may bind into.
fn served(tag: &str, binds_keys: bool) -> Served {
    let work = std::env::temp_dir().join(format!("choir-join-{tag}"));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();

    let auth_path = work.join("auth");
    std::fs::write(&auth_path, "alice:a\n").unwrap();
    let acl_path = work.join("acl");
    std::fs::write(&acl_path, ACL).unwrap();
    // Starts empty: every key the node ends up trusting in this test got
    // there through a redemption, not through the fixture.
    let keys_path = work.join("keys");
    std::fs::write(&keys_path, "").unwrap();

    let mut table = AuthTable::new();
    table.insert("alice".into(), "a".into());
    let root = work.join("repos");
    let mut node = Node::bind_with_auth(&root, 0, Some(table)).unwrap();
    let port = node.port();
    node.create_repo("agents/demo.git").unwrap();
    node.watch_acl_file(acl_path).unwrap();
    node.watch_keys_file(keys_path.clone());
    node.enable_accounts(
        work.join("accounts.json"),
        None,
        binds_keys.then(|| keys_path.clone()),
    )
    .expect("accounts enable");
    node.enable_platform(
        Platform::start_reloading(
            Registry::new(),
            Box::new(MemLog::new()),
            ActorKey::generate(),
            Some(keys_path.clone()),
        )
        .expect("platform starts"),
    );
    std::thread::spawn(move || node.serve_forever());
    let api = format!("http://127.0.0.1:{port}");

    // One commit, pushed as the operator, so a signed workspace request
    // has a base to bind to.
    let seed = work.join("seed");
    let url = format!("http://alice:a@127.0.0.1:{port}/agents/demo.git");
    assert!(git(&work, &["clone", "-q", &url, seed.to_str().unwrap()])
        .status
        .success());
    std::fs::write(seed.join("f.txt"), "v1\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "first"]);
    assert!(git(&seed, &["push", "-q", "origin", "HEAD:main"])
        .status
        .success());
    let head = String::from_utf8_lossy(&git(&seed, &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();

    Served {
        work,
        api,
        keys_path,
        head,
    }
}

/// Mints an invite as the operator and writes it to a file in the
/// node's own `user:token` auth spelling.
fn invite(s: &Served, user: &str) -> std::path::PathBuf {
    let (status, body) = curl(&[
        "-u",
        "alice:a",
        "-X",
        "POST",
        "-d",
        &format!(r#"{{"user":"{user}","grants":["agents/demo write"]}}"#),
        &format!("{}/api/accounts/invite", s.api),
    ]);
    assert_eq!(status, 200, "{body}");
    let doc: serde_json::Value = serde_json::from_str(&body).expect("invite json");
    // The node already returns the two halves as one basic-auth pair,
    // which is exactly the line an invite file holds.
    let pair = doc["invite"].as_str().expect("invite pair");
    let path = s.work.join(format!("{user}.invite"));
    std::fs::write(&path, format!("{pair}\n")).unwrap();
    path
}

#[test]
fn one_command_admits_a_contributor_whose_key_can_then_sign() {
    let s = served("admits", true);
    let invite_file = invite(&s, "bea");
    let key_file = s.work.join("bea.key");

    let out = choir(
        &s.work,
        &[
            "join",
            &s.api,
            invite_file.to_str().unwrap(),
            key_file.to_str().unwrap(),
        ],
    );
    assert!(
        out.status.success(),
        "join failed: {} {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let summary = json(&out);
    assert_eq!(summary["user"], "bea");
    assert_eq!(summary["channel"], "bea/agent");
    assert_eq!(summary["actor_key_bound"], true);

    // The token was stored, at 0600, in the format --auth-file reads.
    let auth_file = summary["auth_file"].as_str().expect("auth file");
    let stored = std::fs::read_to_string(auth_file).expect("token stored");
    assert!(stored.starts_with("bea:"), "{stored}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(auth_file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "token file is not 0600");
    }

    // The operator's file gained exactly the line they would have
    // pasted, bound to the channel rather than unconstrained.
    let key_hex =
        String::from_utf8_lossy(&choir(&s.work, &["key", key_file.to_str().unwrap()]).stdout)
            .trim()
            .to_string();
    let keys = std::fs::read_to_string(&s.keys_path).unwrap();
    assert_eq!(
        keys.lines().filter(|l| !l.trim().is_empty()).count(),
        1,
        "expected one bound key, got {keys:?}"
    );
    assert!(keys.contains(&format!("bea/agent {key_hex}")), "{keys}");

    // The assertion the whole command exists for: that key can now sign
    // an operation the node accepts, with no operator step in between.
    let submitted = choir(
        &s.work,
        &[
            "--auth-file",
            auth_file,
            "workspace",
            &s.api,
            "agents/demo",
            "bea-first",
            "--base",
            &s.head,
            "--owner",
            "bea/agent",
            "--key-file",
            key_file.to_str().unwrap(),
            "--change",
            "bea-change-1",
            "--idempotency-key",
            "bea-request-1",
        ],
    );
    assert!(
        submitted.status.success(),
        "the bound key could not sign: {} {}",
        String::from_utf8_lossy(&submitted.stdout),
        String::from_utf8_lossy(&submitted.stderr)
    );
}

#[test]
fn the_credential_helper_clones_without_a_token_in_the_url() {
    let s = served("helper", true);
    let invite_file = invite(&s, "fin");
    let key_file = s.work.join("fin.key");
    let out = choir(
        &s.work,
        &[
            "join",
            &s.api,
            invite_file.to_str().unwrap(),
            key_file.to_str().unwrap(),
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let auth_file = json(&out)["auth_file"]
        .as_str()
        .expect("auth file")
        .to_string();

    // A clean URL: no user, no token. Without a helper this clone fails,
    // because the node authenticates every request.
    let helper = format!(
        "!{} git-credential {auth_file}",
        env!("CARGO_BIN_EXE_choir")
    );
    let target = s.work.join("helper-clone");
    let cloned = git(
        &s.work,
        &[
            // An empty value resets the helper list, so the one under
            // test is the only helper git consults.
            "-c",
            "credential.helper=",
            "-c",
            &format!("credential.helper={helper}"),
            "clone",
            "-q",
            &format!("{}/agents/demo.git", s.api),
            target.to_str().unwrap(),
        ],
    );
    assert!(
        cloned.status.success(),
        "clone through the helper failed: {}",
        String::from_utf8_lossy(&cloned.stderr)
    );
    assert!(target.join("f.txt").exists());

    // The point of the helper: the credential is nowhere in the config
    // git just wrote, so `git remote -v` cannot leak it.
    let config = std::fs::read_to_string(target.join(".git").join("config")).unwrap();
    let token = std::fs::read_to_string(&auth_file).unwrap();
    let token = token
        .trim()
        .split_once(':')
        .expect("user:token")
        .1
        .to_string();
    assert!(!config.contains(&token), "the token landed in .git/config");
    assert!(!config.contains("fin:"), "{config}");
}

/// The other half of the contribute page's admission branch.
///
/// `browse.rs` covers a node with no self-service; this covers one that
/// has it, because a boolean whose two branches are never both exercised
/// is a boolean that can be inverted without any test noticing.
#[test]
fn the_contribute_page_promises_invites_when_this_node_issues_them() {
    let s = served("contribute-invites", true);
    let (status, body) = curl(&[
        "-u",
        "alice:a",
        &format!("{}/r/agents/demo/contribute", s.api),
    ]);
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("invite-only"),
        "a node with self-service does not offer an invite: {body}"
    );
    assert!(
        !body.contains("issues no invites"),
        "a node with self-service claims it has none"
    );
}

#[test]
fn the_credential_helper_refuses_to_erase_the_operators_credential() {
    let s = served("helper-erase", true);
    let invite_file = invite(&s, "gus");
    let key_file = s.work.join("gus.key");
    let out = choir(
        &s.work,
        &[
            "join",
            &s.api,
            invite_file.to_str().unwrap(),
            key_file.to_str().unwrap(),
        ],
    );
    assert!(out.status.success());
    let auth_file = json(&out)["auth_file"]
        .as_str()
        .expect("auth file")
        .to_string();
    let before = std::fs::read_to_string(&auth_file).unwrap();

    let erased = choir(&s.work, &["git-credential", &auth_file, "erase"]);
    assert!(erased.status.success());
    assert_eq!(
        std::fs::read_to_string(&auth_file).unwrap(),
        before,
        "a routine auth failure would have destroyed a single-use credential"
    );
}

#[test]
fn a_node_that_does_not_bind_keys_says_so_instead_of_half_admitting() {
    let s = served("unbound", false);
    let invite_file = invite(&s, "cyd");
    let key_file = s.work.join("cyd.key");

    let out = choir(
        &s.work,
        &[
            "join",
            &s.api,
            invite_file.to_str().unwrap(),
            key_file.to_str().unwrap(),
        ],
    );
    assert_eq!(out.status.code(), Some(1));
    let body = String::from_utf8_lossy(&out.stdout);
    assert!(body.contains("--invite-binds-keys"), "{body}");

    // Refused whole: the invite is still redeemable, so the operator
    // does not have to issue a second one to recover.
    assert!(
        std::fs::read_to_string(&s.keys_path)
            .unwrap()
            .trim()
            .is_empty(),
        "a refused redemption still wrote a key"
    );
    let invite_pair = std::fs::read_to_string(&invite_file).unwrap();
    let retry = curl(&[
        "-u",
        invite_pair.trim(),
        "-X",
        "POST",
        "-d",
        "{}",
        &format!("{}/api/accounts/redeem", s.api),
    ]);
    assert_eq!(
        retry.0, 200,
        "the invite was spent by a refusal: {}",
        retry.1
    );
}

#[test]
fn a_contributor_cannot_bind_a_channel_under_another_operator() {
    let s = served("prefix", true);
    let invite_file = invite(&s, "dev");
    let key_file = s.work.join("dev.key");

    // The reviewer draw refuses to draw a reviewer sharing the author's
    // operator prefix. A channel a newcomer could name freely would let
    // them sit outside their own operator, or inside somebody else's.
    let out = choir(
        &s.work,
        &[
            "join",
            &s.api,
            invite_file.to_str().unwrap(),
            key_file.to_str().unwrap(),
            "--channel",
            "alice/agent",
        ],
    );
    assert_eq!(out.status.code(), Some(1));
    let body = String::from_utf8_lossy(&out.stdout);
    assert!(body.contains("dev/"), "{body}");
    assert!(
        std::fs::read_to_string(&s.keys_path)
            .unwrap()
            .trim()
            .is_empty(),
        "a refused channel still bound a key"
    );
}

#[test]
fn a_chosen_channel_under_your_own_name_is_allowed() {
    let s = served("own-prefix", true);
    let invite_file = invite(&s, "eve");
    let key_file = s.work.join("eve.key");

    let out = choir(
        &s.work,
        &[
            "join",
            &s.api,
            invite_file.to_str().unwrap(),
            key_file.to_str().unwrap(),
            "--channel",
            "eve/reviewer",
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(json(&out)["channel"], "eve/reviewer");
    assert!(std::fs::read_to_string(&s.keys_path)
        .unwrap()
        .contains("eve/reviewer "));
}
