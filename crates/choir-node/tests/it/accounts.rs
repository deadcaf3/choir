//! Account and token self-service (D36), driven the way a second person
//! is actually onboarded: the operator mints an invite over the API, the
//! newcomer redeems it, and then clones — over HTTPS with the token they
//! were handed, and over SSH with the key they registered in the same
//! request.
//!
//! The property under test is not "an account can be created". It is
//! that nothing under the operator's hand moved: the `--auth-file` and
//! the `--acl-file` are byte-identical before and after, the D29 table
//! still fails closed for the new credential, and D33's `@node` exemption
//! is still something only the operator's own file can confer.

use choir_identity::{ActorKey, Registry};
use choir_node::accounts::SshKeysOut;
use choir_node::{AuthTable, Node, Platform};
use choir_oplog::MemLog;

use crate::support::curl;

/// The operator's own row: node-wide write (which is what issues
/// invites) plus write on every repository.
const OPERATOR_ACL: &str = "alice @node write\nalice * write\n";

fn git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    git_env(dir, args, &[])
}

fn git_env(dir: &std::path::Path, args: &[&str], env: &[(&str, &str)]) -> std::process::Output {
    let mut command = std::process::Command::new("git");
    command
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
        .env("GIT_COMMITTER_EMAIL", "t@t");
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().expect("git runs")
}

/// Like [`crate::support::curl`], but keeps the body as text.
///
/// The node answers an unauthenticated request with a plain-text
/// `unauthorized`, so the JSON helper cannot observe a 401 at all — it
/// panics on the body before the status is ever compared.
fn raw(args: &[&str]) -> (u16, String) {
    let out = std::process::Command::new("curl")
        .args(["-s", "-w", "\n%{http_code}"])
        .args(args)
        .output()
        .expect("curl runs");
    let text = String::from_utf8_lossy(&out.stdout);
    let (body, code) = text.rsplit_once('\n').expect("status line");
    (
        code.trim().parse().expect("numeric status"),
        body.to_string(),
    )
}

/// A served node with exactly one hand-written credential — the
/// operator's — and self-service turned on beside it.
struct Served {
    base: String,
    host: String,
    work: std::path::PathBuf,
    auth_path: std::path::PathBuf,
    acl_path: std::path::PathBuf,
    keys_path: std::path::PathBuf,
    handoff: std::path::PathBuf,
    root: std::path::PathBuf,
}

fn served(tag: &str, repos: &[&str], rate_limit: Option<u32>) -> Served {
    let work = std::env::temp_dir().join(format!("choir-node-accounts-{tag}"));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");

    // The two files the operator would otherwise be editing by hand.
    // Written once here and never again; every test asserts they did not
    // move.
    let auth_path = work.join("auth");
    std::fs::write(&auth_path, "alice:a\n").expect("auth file");
    let acl_path = work.join("acl");
    std::fs::write(&acl_path, OPERATOR_ACL).expect("acl file");

    let mut table = AuthTable::new();
    table.insert("alice".into(), "a".into());

    let root = work.join("repos");
    let mut node = Node::bind_with_auth(&root, 0, Some(table)).expect("node binds free port");
    let port = node.port();
    for repo in repos {
        node.create_repo(repo).expect("repo created");
    }
    node.watch_acl_file(acl_path.clone()).expect("acl loads");
    let keys_path = work.join("authorized_keys");
    let handoff = work.join("handoff");
    node.enable_accounts(
        work.join("accounts.json"),
        Some(SshKeysOut {
            path: keys_path.clone(),
            shim: std::path::PathBuf::from(env!("CARGO_BIN_EXE_choir-ssh")),
            root: root.clone(),
            handoff: handoff.clone(),
        }),
    )
    .expect("accounts enable");
    if let Some(per_minute) = rate_limit {
        node.enable_rate_limit(std::num::NonZeroU32::new(per_minute), None);
    }
    node.enable_platform(
        Platform::start(
            Registry::new(),
            Box::new(MemLog::new()),
            ActorKey::generate(),
        )
        .expect("platform starts"),
    );
    node.write_ssh_handoff(&handoff).expect("handoff");
    std::thread::spawn(move || node.serve_forever());

    Served {
        base: format!("http://127.0.0.1:{port}"),
        host: format!("127.0.0.1:{port}"),
        work,
        auth_path,
        acl_path,
        keys_path,
        handoff,
        root,
    }
}

impl Served {
    /// Mints an invite as the operator and returns its `id:secret` pair.
    fn invite(&self, body: &str) -> (u16, serde_json::Value) {
        curl(&[
            "-u",
            "alice:a",
            "-X",
            "POST",
            "--data-binary",
            body,
            &format!("{}/api/accounts/invite", self.base),
        ])
    }

    /// Redeems an invite, presenting it the only way it can be
    /// presented: as the credential. Text rather than JSON, because a
    /// refused redemption is answered by the auth wall, which does not
    /// speak JSON.
    fn redeem(&self, invite: &str, body: &str) -> (u16, String) {
        raw(&[
            "-u",
            invite,
            "-X",
            "POST",
            "--data-binary",
            body,
            &format!("{}/api/accounts/redeem", self.base),
        ])
    }

    /// The operator's two hand-written files, as bytes, for the
    /// before/after comparison that is the whole claim of this feature.
    ///
    /// The ACL half is the load-bearing one: the node really reads that
    /// file, hot-reloads it, and issuing a grant by appending to it is
    /// the obvious implementation this design rejected. The auth half is
    /// a regression guard — this harness hands the node its table
    /// directly, so no code path writes the file today, and the
    /// assertion exists to notice the day one does.
    fn operator_files(&self) -> (Vec<u8>, Vec<u8>) {
        (
            std::fs::read(&self.auth_path).expect("auth file"),
            std::fs::read(&self.acl_path).expect("acl file"),
        )
    }
}

/// The JSON body of a redemption that was supposed to succeed.
fn redeemed_json(status: u16, body: &str) -> serde_json::Value {
    assert_eq!(status, 200, "redemption refused: {body}");
    serde_json::from_str(body).expect("redemption json")
}

/// Seeds a repository with one commit using the operator's credential,
/// so a newcomer has something to clone.
fn seed(s: &Served, repo: &str) {
    let url = format!("http://alice:a@{}/{repo}", s.host);
    let dir = s.work.join(format!("seed-{}", repo.replace('/', "-")));
    assert!(
        git(&s.work, &["clone", "-q", &url, dir.to_str().unwrap()])
            .status
            .success(),
        "seeding clone failed"
    );
    std::fs::write(dir.join("f.txt"), "seed\n").unwrap();
    assert!(git(&dir, &["add", "."]).status.success());
    assert!(git(&dir, &["commit", "-q", "-m", "seed"]).status.success());
    let push = git(&dir, &["push", "-q", "origin", "HEAD:main"]);
    assert!(
        push.status.success(),
        "{}",
        String::from_utf8_lossy(&push.stderr)
    );
}

/// An ed25519 keypair from `ssh-keygen`, as the person being onboarded
/// would already have. Returns `(private key path, public key line)`.
fn keypair(dir: &std::path::Path, name: &str) -> (std::path::PathBuf, String) {
    let path = dir.join(name);
    let out = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "choir-test", "-f"])
        .arg(&path)
        .output()
        .expect("ssh-keygen runs");
    assert!(out.status.success(), "ssh-keygen: {out:?}");
    let public = std::fs::read_to_string(dir.join(format!("{name}.pub"))).expect("public key");
    (path, public.trim().to_string())
}

/// The end-to-end claim: a second person is issued a credential and uses
/// it, and neither file the operator maintains by hand has changed.
#[test]
fn a_second_person_is_issued_a_credential_and_clones_over_https() {
    let s = served("https", &["agents/demo.git", "agents/secret.git"], None);
    seed(&s, "agents/demo.git");
    let before = s.operator_files();

    let (status, invite) = s.invite(r#"{"user":"bob","grants":["agents/demo read"]}"#);
    assert_eq!(status, 200, "invite refused: {invite}");
    let pair = invite["invite"].as_str().expect("invite pair").to_string();

    let (_, key) = keypair(&s.work, "bob_key");
    let (status, body) = s.redeem(&pair, &serde_json::json!({ "ssh_key": key }).to_string());
    let redeemed = redeemed_json(status, &body);
    let token = redeemed["token"].as_str().expect("token").to_string();
    assert_eq!(redeemed["user"], "bob");

    // The granted repository clones with the issued token.
    let url = format!("http://bob:{token}@{}/agents/demo.git", s.host);
    let dir = s.work.join("bob");
    let out = git(&s.work, &["clone", "-q", &url, dir.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "an issued credential could not clone: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The grant it was issued with is the grant it has: `read` cannot
    // push, and a repository it was not granted does not exist for it.
    std::fs::write(dir.join("g.txt"), "bob\n").unwrap();
    assert!(git(&dir, &["add", "."]).status.success());
    assert!(git(&dir, &["commit", "-q", "-m", "bob"]).status.success());
    let refused = git(&dir, &["push", "origin", "HEAD:main"]);
    assert!(!refused.status.success(), "a read grant pushed");
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("403"), "not a forbidden: {stderr}");

    let other = format!("http://bob:{token}@{}/agents/secret.git", s.host);
    let out = git(&s.work, &["clone", "-q", &other, "bob-secret"]);
    assert!(
        !out.status.success(),
        "an issued credential reached a repo it was not granted"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("403"),
        "a denial confirmed the repo exists: {stderr}"
    );

    // And the point of the whole row: the operator edited nothing.
    assert_eq!(
        before,
        s.operator_files(),
        "issuing a credential rewrote a file the operator maintains by hand"
    );

    // The key registered during redemption is in the generated file,
    // under the account's name rather than whatever comment it carried.
    let generated = std::fs::read_to_string(&s.keys_path).expect("generated keys");
    let lines: Vec<&str> = generated
        .lines()
        .filter(|line| !line.starts_with('#'))
        .collect();
    assert_eq!(lines.len(), 1, "expected one forced command: {generated}");
    assert!(
        lines[0].starts_with("command=\""),
        "not a forced command: {}",
        lines[0]
    );
    assert!(lines[0].contains("--user bob"), "no user: {}", lines[0]);
    assert!(
        lines[0].ends_with(" bob"),
        "comment not replaced: {}",
        lines[0]
    );
    assert!(
        !lines[0].contains("choir-test"),
        "the client's comment survived into the file: {}",
        lines[0]
    );
}

/// The invite is a credential for exactly one thing. Anything else it
/// touches is refused, and it works once.
#[test]
fn an_invite_reaches_nothing_but_its_own_redemption() {
    let s = served("invite", &["agents/demo.git"], None);
    let (status, invite) = s.invite(r#"{"user":"bob","grants":["agents/demo read"]}"#);
    assert_eq!(status, 200, "invite refused: {invite}");
    let pair = invite["invite"].as_str().expect("invite pair").to_string();

    // Endpoints that need no grant at all are exactly the ones a
    // gate-by-grants scheme would have let an invite through.
    for path in ["/api/view", "/api/accounts", "/api/reviews"] {
        let (status, body) = curl(&["-u", &pair, &format!("{}{path}", s.base)]);
        assert_eq!(status, 403, "an invite read {path}: {body}");
    }
    let (status, body) = curl(&[
        "-u",
        &pair,
        &format!(
            "{}/agents/demo.git/info/refs?service=git-upload-pack",
            s.base
        ),
    ]);
    assert_eq!(status, 403, "an invite reached a git route: {body}");

    // It redeems once.
    let (status, first) = s.redeem(&pair, "{}");
    assert_eq!(status, 200, "redemption refused: {first}");
    // And the second attempt is not "already redeemed" but "who are
    // you": the invite stopped being a credential the moment it was
    // spent.
    let (status, second) = s.redeem(&pair, "{}");
    assert_eq!(status, 401, "a spent invite still authenticated: {second}");
}

/// An invite that has run out is not a credential, and its window is the
/// operator's to choose.
///
/// The sleep is one-sided: after it, an invite with a one-second life is
/// expired under any load. Nothing here asserts that something happened
/// *within* a duration, which is what this shared-process harness forbids.
#[test]
fn an_expired_invite_stops_being_a_credential() {
    let s = served("expiry", &["agents/demo.git"], None);
    let (status, invite) =
        s.invite(r#"{"user":"bob","grants":["agents/demo read"],"expires_in_secs":1}"#);
    assert_eq!(status, 200, "invite refused: {invite}");
    let pair = invite["invite"].as_str().expect("invite pair").to_string();
    std::thread::sleep(std::time::Duration::from_millis(1_100));
    let (status, body) = s.redeem(&pair, "{}");
    assert_eq!(status, 401, "an expired invite still authenticated: {body}");
    // Refused as a lifetime, not silently clamped: an invite the operator
    // believes expires tomorrow and does not is the failure worth naming.
    let (status, body) =
        s.invite(r#"{"user":"carol","grants":["agents/demo read"],"expires_in_secs":0}"#);
    assert_eq!(status, 400, "a zero lifetime was accepted: {body}");
}

/// Revocation is deletion, and it takes effect on the next request
/// through every path the credential had.
#[test]
fn revocation_stops_the_token_and_removes_the_key() {
    let s = served("revoke", &["agents/demo.git"], None);
    seed(&s, "agents/demo.git");
    let (_, invite) = s.invite(r#"{"user":"bob","grants":["agents/demo read"]}"#);
    let pair = invite["invite"].as_str().expect("invite pair").to_string();
    let (_, key) = keypair(&s.work, "bob_key");
    let (status, body) = s.redeem(&pair, &serde_json::json!({ "ssh_key": key }).to_string());
    let token = redeemed_json(status, &body)["token"]
        .as_str()
        .expect("token")
        .to_string();

    let url = format!("http://bob:{token}@{}/agents/demo.git", s.host);
    assert!(
        git(&s.work, &["clone", "-q", &url, "bob-before"])
            .status
            .success(),
        "the issued credential could not clone before revocation"
    );
    let generated = std::fs::read_to_string(&s.keys_path).expect("generated keys");
    assert!(generated.contains("--user bob"), "key never registered");

    let (status, body) = curl(&[
        "-u",
        "alice:a",
        "-X",
        "POST",
        "--data-binary",
        r#"{"user":"bob"}"#,
        &format!("{}/api/accounts/revoke", s.base),
    ]);
    assert_eq!(status, 200, "revocation refused: {body}");
    assert_eq!(body["account_revoked"], true);

    let out = git(&s.work, &["clone", "-q", &url, "bob-after"]);
    assert!(!out.status.success(), "a revoked token still cloned");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("401") || stderr.contains("Authentication"),
        "a revoked token was refused for the wrong reason: {stderr}"
    );
    let generated = std::fs::read_to_string(&s.keys_path).expect("generated keys");
    assert!(
        !generated.contains("--user bob"),
        "a revoked account kept its ssh key: {generated}"
    );
    // The grant went with it, so nothing is left holding it.
    let (status, body) = curl(&["-u", "alice:a", &format!("{}/api/accounts", s.base)]);
    assert_eq!(status, 200, "roster refused: {body}");
    assert_eq!(body["accounts"].as_array().expect("accounts").len(), 0);
}

/// A revoked name is never reissued, because the op log has it frozen
/// inside the signature of every op its holder authored.
///
/// The failure this prevents is silent: a second person issued the same
/// name inherits the first person's attribution — their workspace tally,
/// their provenance, their reviews — and nothing afterwards can separate
/// them, because rewriting the channel would invalidate the signature
/// that makes each entry admissible.
#[test]
fn a_revoked_name_is_never_reissued() {
    let s = served("reuse", &["agents/demo.git"], None);
    let (_, invite) = s.invite(r#"{"user":"bob","grants":["agents/demo read"]}"#);
    let pair = invite["invite"].as_str().expect("invite pair").to_string();
    let (status, body) = s.redeem(&pair, "{}");
    redeemed_json(status, &body);
    let (status, body) = curl(&[
        "-u",
        "alice:a",
        "-X",
        "POST",
        "--data-binary",
        r#"{"user":"bob"}"#,
        &format!("{}/api/accounts/revoke", s.base),
    ]);
    assert_eq!(status, 200, "revocation refused: {body}");

    // The name is spent. Not "already exists" — the record is gone; the
    // refusal is about the history the log still attributes to it.
    let (status, answer) = s.invite(r#"{"user":"bob","grants":["agents/demo read"]}"#);
    assert_eq!(status, 409, "a revoked name was reissued: {answer}");
    assert!(
        answer["error"]
            .as_str()
            .unwrap_or_default()
            .contains("never reused"),
        "the refusal should say why: {answer}"
    );
    // And the operator can see the reason without guessing at it.
    let (status, roster) = curl(&["-u", "alice:a", &format!("{}/api/accounts", s.base)]);
    assert_eq!(status, 200, "roster refused: {roster}");
    assert_eq!(roster["retired"][0], "bob");

    // A name whose *invite* was cancelled before redemption never wrote
    // an op, so it has no attribution to inherit and stays issuable.
    // Over-refusing here would burn a name over a typo.
    let (status, answer) = s.invite(r#"{"user":"carol","grants":["agents/demo read"]}"#);
    assert_eq!(status, 200, "invite refused: {answer}");
    let (status, answer) = curl(&[
        "-u",
        "alice:a",
        "-X",
        "POST",
        "--data-binary",
        r#"{"user":"carol"}"#,
        &format!("{}/api/accounts/revoke", s.base),
    ]);
    assert_eq!(status, 200, "cancelling an invite failed: {answer}");
    assert_eq!(answer["account_revoked"], false);
    let (status, answer) = s.invite(r#"{"user":"carol","grants":["agents/demo read"]}"#);
    assert_eq!(
        status, 200,
        "a name that never held an account was burned by a cancelled invite: {answer}"
    );
}

/// Self-service can never issue node-wide authority, which is what keeps
/// D33's rate-limit exemption operator-conferred. Both halves are here
/// because the refusal alone would prove only that a string was
/// rejected.
#[test]
fn self_service_can_never_issue_a_node_grant() {
    let s = served("nodegrant", &["agents/demo.git"], Some(1));
    for grants in [
        r#"["@node auditor"]"#,
        r#"["@node write"]"#,
        r#"["agents/demo read","@node auditor"]"#,
    ] {
        let body = format!(r#"{{"user":"bob","grants":{grants}}}"#);
        let (status, answer) = s.invite(&body);
        assert_eq!(
            status, 400,
            "a node grant was issued for {grants}: {answer}"
        );
    }

    let (_, invite) = s.invite(r#"{"user":"bob","grants":["agents/demo read"]}"#);
    let pair = invite["invite"].as_str().expect("invite pair").to_string();
    let (status, body) = s.redeem(&pair, "{}");
    let token = redeemed_json(status, &body)["token"]
        .as_str()
        .expect("token")
        .to_string();
    let bob = format!("bob:{token}");

    // The operator holds `@node write` from the file, so the limiter
    // does not apply to them however many requests they make (D33).
    for attempt in 0..3 {
        let (status, body) = curl(&["-u", "alice:a", &format!("{}/api/view", s.base)]);
        assert_eq!(
            status, 200,
            "the operator was throttled on attempt {attempt}: {body}"
        );
    }
    // The self-served account holds no node grant, so it is metered like
    // anybody else — one request per minute, then refused.
    let (status, _) = curl(&["-u", &bob, &format!("{}/api/view", s.base)]);
    assert_eq!(status, 200, "the first request should pass");
    let (status, body) = curl(&["-u", &bob, &format!("{}/api/view", s.base)]);
    assert_eq!(
        status, 429,
        "a self-served account was exempt from the limiter: {body}"
    );
}

/// Issuance is the operator's, not anyone's who holds a credential.
#[test]
fn only_a_node_write_holder_may_issue_or_revoke() {
    let s = served("authority", &["agents/demo.git"], None);
    let (_, invite) = s.invite(r#"{"user":"bob","grants":["agents/demo write"]}"#);
    let pair = invite["invite"].as_str().expect("invite pair").to_string();
    let (status, body) = s.redeem(&pair, "{}");
    let token = redeemed_json(status, &body)["token"]
        .as_str()
        .expect("token")
        .to_string();
    let bob = format!("bob:{token}");

    // A write grant on a repository is not authority over the node.
    for (method, path, body) in [
        (
            "POST",
            "/api/accounts/invite",
            r#"{"user":"mallory","grants":["agents/demo write"]}"#,
        ),
        ("POST", "/api/accounts/revoke", r#"{"user":"bob"}"#),
    ] {
        let (status, answer) = curl(&[
            "-u",
            &bob,
            "-X",
            method,
            "--data-binary",
            body,
            &format!("{}{path}", s.base),
        ]);
        assert_eq!(status, 403, "a repo grant reached {path}: {answer}");
    }
    let (status, answer) = curl(&["-u", &bob, &format!("{}/api/accounts", s.base)]);
    assert_eq!(status, 403, "a repo grant read the roster: {answer}");

    // And an unauthenticated request never reaches the question.
    let (status, answer) = raw(&[
        "-X",
        "POST",
        "--data-binary",
        r#"{"user":"mallory","grants":["agents/demo write"]}"#,
        &format!("{}/api/accounts/invite", s.base),
    ]);
    assert_eq!(
        status, 401,
        "an anonymous request minted an invite: {answer}"
    );
}

/// The registered key is the one field a newcomer controls freely, and
/// it is interpolated into a file whose lines are forced commands.
#[test]
fn a_registered_key_cannot_forge_a_forced_command() {
    let s = served("keys", &["agents/demo.git"], None);
    let (_, invite) = s.invite(r#"{"user":"bob","grants":["agents/demo read"]}"#);
    let pair = invite["invite"].as_str().expect("invite pair").to_string();
    let (_, real) = keypair(&s.work, "bob_key");

    for (label, key) in [
        (
            "a second line carrying its own forced command",
            format!("{real}\ncommand=\"/bin/sh\",restrict {real}"),
        ),
        (
            "an unsupported algorithm",
            "ssh-rsa AAAAB3NzaC1yc2E= mallory".to_string(),
        ),
        (
            "a blob that is not a key",
            "ssh-ed25519 AAAA mallory".to_string(),
        ),
        ("not base64 at all", "ssh-ed25519 !!!! mallory".to_string()),
    ] {
        let (status, body) = s.redeem(&pair, &serde_json::json!({ "ssh_key": key }).to_string());
        assert_eq!(status, 400, "{label} was registered: {body}");
    }
    // The invite survived every refusal, so a rejected key costs the
    // newcomer a retry rather than the invite.
    let (status, body) = s.redeem(&pair, &serde_json::json!({ "ssh_key": real }).to_string());
    assert_eq!(status, 200, "the real key was refused: {body}");
    let generated = std::fs::read_to_string(&s.keys_path).expect("generated keys");
    let lines = generated
        .lines()
        .filter(|line| !line.starts_with('#'))
        .count();
    assert_eq!(lines, 1, "expected exactly one line: {generated}");
    assert!(
        !generated.contains("/bin/sh"),
        "a client-supplied command reached the file: {generated}"
    );
}

/// A running `sshd`, killed when this value is dropped.
struct Sshd {
    child: std::process::Child,
    port: u16,
}

impl Drop for Sshd {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

/// Starts a private sshd whose authorized keys are whatever the node
/// generated — the file under test, not one this test wrote.
fn start_sshd(dir: &std::path::Path, authorized: &std::path::Path) -> Option<Sshd> {
    let sshd = std::path::Path::new("/usr/sbin/sshd");
    if !sshd.exists() {
        return None;
    }
    let host_key = dir.join("host_key");
    let out = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "choir-test", "-f"])
        .arg(&host_key)
        .output()
        .expect("ssh-keygen runs");
    assert!(out.status.success(), "ssh-keygen: {out:?}");
    let config = dir.join("sshd_config");
    std::fs::write(
        &config,
        format!(
            "ListenAddress 127.0.0.1\n\
             HostKey {}\n\
             AuthorizedKeysFile {}\n\
             StrictModes no\n\
             UsePAM no\n\
             PasswordAuthentication no\n\
             KbdInteractiveAuthentication no\n\
             PermitUserEnvironment no\n",
            host_key.display(),
            authorized.display(),
        ),
    )
    .expect("sshd_config");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&host_key, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }
    for _ in 0..5 {
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("probe port");
            listener.local_addr().expect("addr").port()
        };
        let log = std::fs::File::create(dir.join("sshd.log")).expect("sshd log");
        let Ok(child) = std::process::Command::new(sshd)
            .arg("-D")
            .arg("-e")
            .args(["-f".as_ref(), config.as_os_str()])
            .args(["-p", &port.to_string()])
            .stderr(std::process::Stdio::from(log))
            .spawn()
        else {
            return None;
        };
        let mut sshd = Sshd { child, port };
        for _ in 0..100 {
            if sshd.child.try_wait().expect("wait").is_some() {
                break;
            }
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return Some(sshd);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        drop(sshd);
    }
    None
}

/// The other half of the claim: the key registered at redemption serves
/// a clone over SSH, through a file the operator never opened.
#[test]
fn a_self_served_key_clones_over_ssh_through_the_generated_file() {
    let s = served("ssh", &["agents/demo.git"], None);
    seed(&s, "agents/demo.git");
    let before = s.operator_files();
    let (_, invite) = s.invite(r#"{"user":"bob","grants":["agents/demo read"]}"#);
    let pair = invite["invite"].as_str().expect("invite pair").to_string();
    let (private, public) = keypair(&s.work, "bob_key");
    let (status, body) = s.redeem(&pair, &serde_json::json!({ "ssh_key": public }).to_string());
    redeemed_json(status, &body);

    let Some(sshd) = start_sshd(&s.work, &s.keys_path) else {
        eprintln!(
            "accounts::a_self_served_key_clones_over_ssh_through_the_generated_file: no usable \
             sshd on this host, skipped. The generated file's shape is still asserted by the \
             other tests in this module."
        );
        return;
    };
    let ssh_command = format!(
        "ssh -i {} -o IdentitiesOnly=yes -o StrictHostKeyChecking=no \
         -o UserKnownHostsFile=/dev/null -o BatchMode=yes",
        private.display()
    );
    let url = format!("ssh://127.0.0.1:{}/agents/demo.git", sshd.port);
    let dir = s.work.join("bob-ssh");
    let out = git_env(
        &s.work,
        &["clone", "-q", &url, dir.to_str().unwrap()],
        &[("GIT_SSH_COMMAND", &ssh_command)],
    );
    assert!(
        out.status.success(),
        "a self-served key could not clone over ssh: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(dir.join("f.txt").exists(), "the clone is empty");

    // The shim graded the grant from the store, through the handoff, so
    // the read grant is a read grant on this transport too.
    std::fs::write(dir.join("g.txt"), "bob\n").unwrap();
    assert!(git(&dir, &["add", "."]).status.success());
    assert!(git(&dir, &["commit", "-q", "-m", "bob"]).status.success());
    let refused = git_env(
        &dir,
        &["push", "origin", "HEAD:main"],
        &[("GIT_SSH_COMMAND", &ssh_command)],
    );
    assert!(!refused.status.success(), "a read grant pushed over ssh");

    assert_eq!(
        before,
        s.operator_files(),
        "the ssh path moved a file the operator maintains by hand"
    );
    // The handoff names the store, which is what let the shim see a
    // grant that is in no ACL file.
    let handoff = std::fs::read_to_string(&s.handoff).expect("handoff");
    assert!(
        handoff.lines().any(|line| line.starts_with("accounts ")),
        "the handoff never named the store: {handoff}"
    );
    assert!(s.root.join("agents/demo.git").is_dir(), "repo missing");
}
