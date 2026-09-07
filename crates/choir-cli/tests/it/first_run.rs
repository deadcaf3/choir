//! The first five minutes, from a `$HOME` with nothing in it.
//!
//! The claim is not that `choir join` returns 0. It is that a person who
//! pastes one link is *finished*: the next thing they type is `git
//! clone`, and the thing after that is `choir propose` with no
//! arguments. Every intermediate step this product used to ask for -- an
//! invite file, a key path, a credential-helper line, a node URL on
//! every command -- is absent from this test on purpose. If one of them
//! comes back, one of these assertions stops holding.
//!
//! `HOME` and `GIT_CONFIG_GLOBAL` are set on the child processes, never
//! on this one: these modules share a process with every other file in
//! the harness and run on parallel threads.

use choir_identity::ActorKey;
use choir_identity::Registry;
use choir_node::{AuthTable, Node, Platform};
use choir_oplog::MemLog;

const ACL: &str = "alice @node write\nalice * write\nbea * write\nbea/agent * write\n";

/// One contributor machine: a `$HOME` nothing has written to yet, and
/// the git config file `--global` will land in.
struct Machine {
    home: std::path::PathBuf,
    gitconfig: std::path::PathBuf,
}

impl Machine {
    fn command(&self, program: &str) -> std::process::Command {
        let mut command = std::process::Command::new(program);
        command
            .env("HOME", &self.home)
            .env("GIT_CONFIG_GLOBAL", &self.gitconfig)
            // `GIT_CONFIG_GLOBAL` replaces `~/.gitconfig` and nothing
            // else, so without this git still reads the system config --
            // which on macOS sets `credential.helper = osxkeychain`. Git
            // then calls `store` on *every* configured helper after a
            // successful authentication, and that one blocks on the
            // login keychain. Harmless for a person; under a parallel
            // test suite it is a push that never returns.
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t");
        command
    }

    fn choir(&self, dir: &std::path::Path, args: &[&str]) -> std::process::Output {
        self.command(env!("CARGO_BIN_EXE_choir"))
            .args(args)
            .current_dir(dir)
            .output()
            .expect("choir runs")
    }

    fn git(&self, dir: &std::path::Path, args: &[&str]) -> std::process::Output {
        self.command("git")
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git runs")
    }
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

fn shown(out: &std::process::Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

struct Served {
    work: std::path::PathBuf,
    api: String,
}

/// A node that admits newcomers by invite and binds their key on the
/// way in, seeded with one commit so a proposal has a base.
fn served(tag: &str) -> Served {
    let work = std::env::temp_dir().join(format!("choir-first-run-{tag}"));
    std::fs::remove_dir_all(&work).ok();
    // Loudly. A run that was killed mid-flight leaves `repos` behind
    // with a `main` in it, and `create_repo` then hands back a
    // repository whose refs the fresh `MemLog` view knows nothing about
    // -- so the seeding push fails its compare-and-swap and the test
    // reports a CAS bug that is really a directory nobody deleted.
    assert!(
        !work.exists(),
        "{} survived removal; delete it and re-run",
        work.display()
    );
    std::fs::create_dir_all(&work).unwrap();

    std::fs::write(work.join("auth"), "alice:a\n").unwrap();
    let acl_path = work.join("acl");
    std::fs::write(&acl_path, ACL).unwrap();
    let keys_path = work.join("keys");
    std::fs::write(&keys_path, "").unwrap();

    let mut table = AuthTable::new();
    table.insert("alice".into(), "a".into());
    let mut node = Node::bind_with_auth(&work.join("repos"), 0, Some(table)).unwrap();
    let port = node.port();
    node.create_repo("agents/demo.git").unwrap();
    node.watch_acl_file(acl_path).unwrap();
    node.watch_keys_file(keys_path.clone());
    node.enable_accounts(work.join("accounts.json"), None, Some(keys_path.clone()))
        .expect("accounts enable");
    node.enable_platform(
        Platform::start_reloading(
            Registry::new(),
            Box::new(MemLog::new()),
            ActorKey::generate(),
            Some(keys_path),
        )
        .expect("platform starts"),
    );
    std::thread::spawn(move || node.serve_forever());
    let api = format!("http://127.0.0.1:{port}");

    // Seeded as the operator, whose credential is in the URL because the
    // operator is not the persona under test here. Their `$HOME` is this
    // machine's: only the contributor's is replaced, and replacing one
    // more than the claim needs is one more way for the fixture to fail
    // for a reason the test is not about.
    let seed = work.join("seed");
    let url = format!("http://alice:a@127.0.0.1:{port}/agents/demo.git");
    let operator = Machine {
        home: std::env::var_os("HOME").map_or_else(std::path::PathBuf::new, Into::into),
        gitconfig: work.join("operator-gitconfig"),
    };
    let cloned = operator.git(&work, &["clone", "-q", &url, seed.to_str().unwrap()]);
    assert!(cloned.status.success(), "seeding clone: {}", shown(&cloned));
    std::fs::write(seed.join("f.txt"), "v1\n").unwrap();
    operator.git(&seed, &["add", "."]);
    operator.git(&seed, &["commit", "-q", "-m", "first"]);
    let pushed = operator.git(&seed, &["push", "-q", "origin", "HEAD:main"]);
    assert!(pushed.status.success(), "seeding push: {}", shown(&pushed));

    Served { work, api }
}

/// Mints an invite as the operator and returns the one link a
/// contributor is sent, exactly as the node builds it.
fn link(s: &Served, user: Option<&str>) -> String {
    let body = match user {
        Some(user) => format!(r#"{{"user":"{user}","grants":["agents/demo write"]}}"#),
        None => r#"{"grants":["agents/demo write"]}"#.to_string(),
    };
    let (status, answer) = curl(&[
        "-u",
        "alice:a",
        "-X",
        "POST",
        "-d",
        &body,
        &format!("{}/api/accounts/invite", s.api),
    ]);
    assert_eq!(status, 200, "{answer}");
    let doc: serde_json::Value = serde_json::from_str(&answer).expect("invite json");
    let pair = doc["invite"].as_str().expect("invite pair");
    let (id, secret) = pair.split_once(':').expect("<id>:<secret>");
    let built = format!("{}/join?i={id}&k={secret}", s.api);
    // The node mints the same grammar itself. Asserting they agree is
    // what keeps this test from passing against a link nobody is sent.
    if let Some(minted) = doc["join_url"].as_str() {
        assert_eq!(minted, built, "the node's link and this test's disagree");
    }
    built
}

#[test]
fn one_link_leaves_a_contributor_able_to_clone_push_and_propose() {
    let s = served("whole-path");
    let machine = Machine {
        home: s.work.join("home"),
        gitconfig: s.work.join("home").join("gitconfig"),
    };
    std::fs::create_dir_all(&machine.home).unwrap();
    let invite = link(&s, Some("bea"));

    // 1. The one command. No invite file, no key path, no node URL.
    let joined = machine.choir(&s.work, &["join", &invite]);
    assert!(joined.status.success(), "join failed: {}", shown(&joined));
    let report = String::from_utf8_lossy(&joined.stdout).to_string();
    assert!(report.contains("Joined"), "{report}");
    assert!(
        report.contains("choir propose"),
        "the report names no next step: {report}"
    );

    // What it left behind, at the paths every later command looks in.
    let key = machine.home.join(".choir").join("agent.key");
    let auth = machine.home.join(".choir").join("auth");
    let config = machine.home.join(".choir").join("config");
    for path in [&key, &auth, &config] {
        assert!(path.is_file(), "{} was not written", path.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in [&key, &auth] {
            let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{} is mode {mode:04o}", path.display());
        }
        let mode = std::fs::metadata(machine.home.join(".choir"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "~/.choir is mode {mode:04o}");
    }
    let written = std::fs::read_to_string(&config).unwrap();
    assert!(written.contains(&format!("node = {}", s.api)), "{written}");

    // 2. An ordinary clone, with no credential anywhere in the URL and
    //    no helper configured by hand.
    let clone = s.work.join("clone");
    let cloned = machine.git(
        &s.work,
        &[
            "clone",
            "-q",
            &format!("{}/agents/demo.git", s.api),
            clone.to_str().unwrap(),
        ],
    );
    assert!(cloned.status.success(), "clone failed: {}", shown(&cloned));
    assert!(clone.join("f.txt").exists());
    // The token is in the file join wrote, and nowhere git recorded.
    let stored = std::fs::read_to_string(&auth).unwrap();
    let token = stored.trim().split_once(':').expect("user:token").1;
    let git_config = std::fs::read_to_string(clone.join(".git").join("config")).unwrap();
    assert!(
        !git_config.contains(token),
        "the token landed in .git/config"
    );

    // 3. A push, which is the half of git a read-only clone would not
    //    have exercised.
    std::fs::write(clone.join("g.txt"), "from bea\n").unwrap();
    machine.git(&clone, &["checkout", "-q", "-b", "bea-first"]);
    machine.git(&clone, &["add", "."]);
    machine.git(&clone, &["commit", "-q", "-m", "add g"]);
    let pushed = machine.git(
        &clone,
        &["push", "-q", "origin", "HEAD:refs/heads/bea-scratch"],
    );
    assert!(pushed.status.success(), "push failed: {}", shown(&pushed));

    // 4. `choir propose`, with nothing after it.
    let proposed = machine.choir(&clone, &["propose"]);
    assert!(
        proposed.status.success(),
        "propose failed: {}",
        shown(&proposed)
    );
    let summary: serde_json::Value =
        serde_json::from_slice(&proposed.stdout).unwrap_or_else(|_| panic!("{}", shown(&proposed)));
    assert_eq!(summary["review"], "opened");
    assert!(summary["change"].as_str().is_some_and(|id| !id.is_empty()));

    // Re-running reaches the same change rather than a second one: the
    // zero-argument form must derive the same identity twice.
    let again = machine.choir(&clone, &["propose"]);
    assert!(again.status.success(), "{}", shown(&again));
    let second: serde_json::Value = serde_json::from_slice(&again.stdout).expect("json");
    assert_eq!(second["change"], summary["change"]);

    std::fs::remove_dir_all(&s.work).ok();
}

/// The name is the one thing the link cannot carry when the invite left
/// it open, and a pipe has nobody to ask.
#[test]
fn an_open_invite_with_no_terminal_names_the_flag_instead_of_guessing() {
    let s = served("open-invite");
    let machine = Machine {
        home: s.work.join("home"),
        gitconfig: s.work.join("home").join("gitconfig"),
    };
    std::fs::create_dir_all(&machine.home).unwrap();
    let invite = link(&s, None);

    let out = machine.choir(&s.work, &["join", &invite]);
    assert_eq!(out.status.code(), Some(2), "{}", shown(&out));
    let said = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(said.contains("next: choir join"), "{said}");
    assert!(said.contains("--user"), "{said}");

    // Naming it works, and the invite the refusal did not spend is
    // still there to redeem.
    let out = machine.choir(&s.work, &["join", &invite, "--user", "cyd"]);
    assert!(out.status.success(), "{}", shown(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("as cyd"));

    std::fs::remove_dir_all(&s.work).ok();
}

/// A key is an identity the node may already have bound, so the
/// defaulted path is never written over in silence.
#[test]
fn a_second_join_refuses_rather_than_replacing_the_key() {
    let s = served("second-join");
    let machine = Machine {
        home: s.work.join("home"),
        gitconfig: s.work.join("home").join("gitconfig"),
    };
    std::fs::create_dir_all(&machine.home).unwrap();

    let first = machine.choir(&s.work, &["join", &link(&s, Some("dev"))]);
    assert!(first.status.success(), "{}", shown(&first));
    let key = machine.home.join(".choir").join("agent.key");
    let before = std::fs::read(&key).unwrap();

    let second = machine.choir(&s.work, &["join", &link(&s, Some("eve"))]);
    assert_eq!(second.status.code(), Some(1), "{}", shown(&second));
    let said = String::from_utf8_lossy(&second.stderr).to_string();
    assert!(said.contains("already joined"), "{said}");
    assert!(said.contains("next: choir join"), "{said}");
    assert_eq!(std::fs::read(&key).unwrap(), before, "the key was replaced");

    std::fs::remove_dir_all(&s.work).ok();
}

/// A machine with nothing set up gets the orientation, not the index.
#[test]
fn a_bare_choir_on_a_clean_machine_is_six_lines() {
    let work = std::env::temp_dir().join("choir-first-run-orientation");
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let machine = Machine {
        home: work.join("home"),
        gitconfig: work.join("home").join("gitconfig"),
    };
    std::fs::create_dir_all(&machine.home).unwrap();

    let out = machine.choir(&work, &[]);
    let said = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        said.lines().count() <= 6,
        "{} lines, not six:\n{said}",
        said.lines().count()
    );
    assert!(said.contains("choir join"), "{said}");
    assert!(said.contains("choir init"), "{said}");
    assert!(!said.contains("workspace-archive"), "the index: {said}");

    // And `choir doctor` says which of the three this machine is.
    let doctor = machine.choir(&work, &["doctor"]);
    let report = String::from_utf8_lossy(&doctor.stdout).to_string();
    assert!(report.contains("nothing configured yet"), "{report}");

    std::fs::remove_dir_all(&work).ok();
}
