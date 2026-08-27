//! Asking for access, and being let in (D72).
//!
//! Before this, a person who found a choir node and wanted in had exactly
//! one route: find the operator somewhere else and ask them there. The
//! node held a complete invite system whose first step was off the node
//! entirely, which is how a private beta becomes a beta of one.
//!
//! What is under test is the loop closing without a message being sent.
//! Nobody's address is collected, so the claim link is the whole channel:
//! the asker keeps it, and the moment an operator grants the request the
//! *same address* stops rendering "nobody has answered" and starts
//! rendering the invite. Every test here is a step of that, or a way it
//! must not be short-circuited.

use choir_identity::{ActorKey, Registry};
use choir_node::{AuthTable, Node, Platform};
use choir_oplog::MemLog;
use sha2::{Digest, Sha256};

struct Served {
    base: String,
    work: std::path::PathBuf,
}

/// `curl` keeping headers and body apart.
fn get(url: &str, args: &[&str]) -> (u16, String, String) {
    let out = std::process::Command::new("curl")
        .args(["-s", "-i", "-w", "\n%{http_code}"])
        .args(args)
        .arg(url)
        .output()
        .expect("curl runs");
    let text = String::from_utf8_lossy(&out.stdout);
    let (rest, code) = text.rsplit_once('\n').expect("status line");
    let (headers, body) = rest.split_once("\r\n\r\n").unwrap_or((rest, ""));
    (
        code.trim().parse().expect("numeric status"),
        headers.to_string(),
        body.to_string(),
    )
}

fn header_value(headers: &str, name: &str) -> Option<String> {
    headers.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name)
            .then(|| value.trim().to_string())
    })
}

fn served(tag: &str) -> Served {
    let work = std::env::temp_dir().join(format!("choir-node-access-{tag}"));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");
    let acl_path = work.join("acl");
    // `alice` is the operator; `bob` holds an ordinary repository grant
    // and must not be able to answer anybody.
    std::fs::write(
        &acl_path,
        "alice @node write\nalice * write\nbob agents/demo.git write\n",
    )
    .expect("acl file");
    let mut table = AuthTable::new();
    table.insert("alice".into(), "a".into());
    table.insert("bob".into(), "b".into());
    let root = work.join("repos");
    let mut node = Node::bind_with_auth(&root, 0, Some(table)).expect("node binds free port");
    let port = node.port();
    node.create_repo("agents/demo.git").expect("repo created");
    node.watch_acl_file(acl_path).expect("acl loads");
    node.enable_accounts(work.join("accounts.json"), None, None)
        .expect("accounts enable");
    node.enable_platform(
        Platform::start(
            Registry::new(),
            Box::new(MemLog::new()),
            ActorKey::generate(),
        )
        .expect("platform starts"),
    );
    std::thread::spawn(move || node.serve_forever());
    Served {
        base: format!("http://127.0.0.1:{port}"),
        work,
    }
}

/// The browser's half of D72's cost, in Rust.
///
/// Deliberately a second implementation rather than a call into the
/// node's own verifier: what the node must agree with is a program in
/// another language that only knows the wire contract, and a test that
/// asked the verifier to solve its own puzzle would agree with itself
/// however the spelling drifted.
fn stamp(challenge: &str, bits: u32) -> String {
    for nonce in 0u64.. {
        let digest = Sha256::digest(format!("{challenge}:{nonce}").as_bytes());
        let mut zeros = 0u32;
        for byte in digest.iter() {
            if *byte == 0 {
                zeros += 8;
                continue;
            }
            zeros += byte.leading_zeros();
            break;
        }
        if zeros >= bits {
            return nonce.to_string();
        }
    }
    unreachable!("the search space is unbounded")
}

impl Served {
    /// Asks for access the way the page does, and returns the claim link.
    fn ask(&self, name: &str, about: &str) -> String {
        let (status, issued) =
            crate::support::curl(&["-X", "POST", &format!("{}/api/access/challenge", self.base)]);
        assert_eq!(status, 200, "{issued}");
        let challenge = issued["challenge"].as_str().expect("a challenge");
        let bits = u32::try_from(issued["bits"].as_u64().expect("a difficulty")).expect("small");
        let nonce = stamp(challenge, bits);
        let body = serde_json::json!({
            "display_name": name,
            "about": about,
            "challenge": challenge,
            "nonce": nonce,
        })
        .to_string();
        let (status, answer) = crate::support::curl(&[
            "-X",
            "POST",
            "-d",
            &body,
            &format!("{}/api/access", self.base),
        ]);
        assert_eq!(status, 200, "{answer}");
        answer["join_url"]
            .as_str()
            .expect("a claim link")
            .to_string()
    }

    /// The pending requests, as the operator's API shows them.
    fn queue(&self) -> Vec<serde_json::Value> {
        let (status, listing) =
            crate::support::curl(&["-u", "alice:a", &format!("{}/api/accounts", self.base)]);
        assert_eq!(status, 200, "{listing}");
        listing["requests"].as_array().cloned().unwrap_or_default()
    }
}

/// The whole point, end to end: one link, no message sent, and the link
/// the asker already holds turns into their invite.
#[test]
fn one_link_carries_somebody_from_stranger_to_account() {
    let s = served("loop");
    let link = s.ask("Ada", "wants to read the queue crate");

    // Before anybody answers, the link is a waiting room and not an
    // invite: nothing on it redeems anything.
    let (status, _, body) = get(&link, &[]);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("Bookmark"), "{body}");
    assert!(body.contains("Ada"), "{body}");
    assert!(
        !body.contains("Join"),
        "the waiting page offered redemption: {body}"
    );

    // The operator sees it, with what they wrote and nothing else.
    let queue = s.queue();
    assert_eq!(queue.len(), 1, "{queue:?}");
    assert_eq!(queue[0]["display_name"], "Ada");
    assert_eq!(queue[0]["about"], "wants to read the queue crate");
    let id = queue[0]["request_id"].as_str().expect("an id").to_string();

    // They say yes.
    let (status, answer) = crate::support::curl(&[
        "-u",
        "alice:a",
        "-X",
        "POST",
        "-d",
        &serde_json::json!({ "request_id": id, "grants": ["agents/demo.git write"] }).to_string(),
        &format!("{}/api/accounts/request/grant", s.base),
    ]);
    assert_eq!(status, 200, "{answer}");
    // No principal yet (D75): granting decides that somebody is in, not
    // what they are called. They pick that themselves when they redeem.
    assert!(answer["user"].is_null(), "{answer}");
    assert_eq!(answer["display_name"], "Ada");

    // The same address, unchanged, is now the invite.
    let (status, _, body) = get(&link, &[]);
    assert_eq!(status, 200, "{body}");
    assert!(!body.contains("Bookmark"), "still the waiting page: {body}");
    assert!(body.contains("Ada"), "{body}");
    assert!(
        body.contains("write"),
        "the invite does not say what it grants: {body}"
    );

    // And it redeems, into an account that can reach the repository.
    // The form body is the link's own query string: the page posts back
    // the two halves it was reached with.
    let form = format!(
        "{}&user=ada",
        link.split_once("/join?").expect("a join link").1
    );
    let (status, _, body) = get(&format!("{}/join", s.base), &["-X", "POST", "-d", &form]);
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("You&#39;re in") || body.contains("You're in"),
        "{body}"
    );
    assert!(
        body.contains("ada"),
        "the name they picked is not theirs: {body}"
    );
    // The queue is empty again: a granted request is not still pending.
    assert!(s.queue().is_empty(), "{:?}", s.queue());
    std::fs::remove_dir_all(&s.work).ok();
}

/// Declining says nothing back. A link that was declined and a link that
/// never existed render the same page, so nobody can probe the queue by
/// watching how their guesses are refused.
#[test]
fn a_declined_link_is_indistinguishable_from_one_that_was_never_valid() {
    let s = served("decline");
    let link = s.ask("Mallory", "");
    let id = s.queue()[0]["request_id"]
        .as_str()
        .expect("an id")
        .to_string();

    let (status, answer) = crate::support::curl(&[
        "-u",
        "alice:a",
        "-X",
        "POST",
        "-d",
        &serde_json::json!({ "request_id": id }).to_string(),
        &format!("{}/api/accounts/request/decline", s.base),
    ]);
    assert_eq!(status, 200, "{answer}");

    let (_, _, declined) = get(&link, &[]);
    let (_, _, never) = get(&format!("{}/join?i=ask-nothing&k=nothing", s.base), &[]);
    assert_eq!(declined, never, "a declined link is distinguishable");
    assert!(s.queue().is_empty());
    std::fs::remove_dir_all(&s.work).ok();
}

/// The cost is real: a stamp is good for exactly one row, because the
/// challenge is spent before it is checked.
#[test]
fn one_solved_challenge_buys_exactly_one_request() {
    let s = served("onceonly");
    let (_, issued) =
        crate::support::curl(&["-X", "POST", &format!("{}/api/access/challenge", s.base)]);
    let challenge = issued["challenge"].as_str().expect("a challenge");
    let bits = u32::try_from(issued["bits"].as_u64().expect("bits")).expect("small");
    let nonce = stamp(challenge, bits);
    let body = serde_json::json!({
        "display_name": "Ada",
        "challenge": challenge,
        "nonce": nonce,
    })
    .to_string();
    let url = format!("{}/api/access", s.base);
    let (first, _) = crate::support::curl(&["-X", "POST", "-d", &body, &url]);
    assert_eq!(first, 200);
    let (second, answer) = crate::support::curl(&["-X", "POST", "-d", &body, &url]);
    assert_eq!(second, 400, "the same stamp was spent twice: {answer}");
    assert_eq!(s.queue().len(), 1);
    std::fs::remove_dir_all(&s.work).ok();
}

/// An unsolved nonce, a challenge this node never issued and a malformed
/// one are all refused, and refused identically.
#[test]
fn a_request_without_the_work_is_refused() {
    let s = served("nowork");
    let (_, issued) =
        crate::support::curl(&["-X", "POST", &format!("{}/api/access/challenge", s.base)]);
    let challenge = issued["challenge"]
        .as_str()
        .expect("a challenge")
        .to_string();
    let url = format!("{}/api/access", s.base);
    let attempts = [
        // Issued, but the work was not done.
        serde_json::json!({ "display_name": "Ada", "challenge": challenge, "nonce": "1" }),
        // Never issued.
        serde_json::json!({
            "display_name": "Ada",
            "challenge": "b3-0000000000000000000000000000000000000000000000000000000000000000",
            "nonce": "1",
        }),
        // Not a hash at all.
        serde_json::json!({ "display_name": "Ada", "challenge": "banana", "nonce": "1" }),
        // No stamp offered.
        serde_json::json!({ "display_name": "Ada" }),
    ];
    let mut said = Vec::new();
    for attempt in &attempts {
        let (status, answer) =
            crate::support::curl(&["-X", "POST", "-d", &attempt.to_string(), &url]);
        assert_eq!(status, 400, "{answer}");
        said.push(answer["error"].as_str().unwrap_or_default().to_string());
    }
    assert!(
        said.windows(2).all(|pair| pair[0] == pair[1]),
        "the refusals differ, which tells a prober how the cost is checked: {said:?}"
    );
    assert!(s.queue().is_empty());
    std::fs::remove_dir_all(&s.work).ok();
}

/// The console is the operator's, and `@node write` is what says so.
#[test]
fn only_the_operator_reaches_the_console() {
    let s = served("console");
    s.ask("Ada", "hello");

    let (status, _, body) = get(&format!("{}/people", s.base), &["-u", "alice:a"]);
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("Ada"),
        "the operator cannot see the queue: {body}"
    );
    assert!(
        body.contains("agents/demo"),
        "no repository to grant: {body}"
    );

    // A repository writer is not an operator.
    let (status, _, body) = get(&format!("{}/people", s.base), &["-u", "bob:b"]);
    assert_eq!(status, 403, "{body}");
    assert!(
        !body.contains("Ada"),
        "the refusal leaked the queue: {body}"
    );

    // And a stranger meets the wall like everywhere else.
    let (status, _, _) = get(&format!("{}/people", s.base), &[]);
    assert_eq!(status, 401);
    std::fs::remove_dir_all(&s.work).ok();
}

/// The console's buttons are plain forms, so the thing that keeps a
/// hostile page from pressing them with an operator's cached credential
/// is the `Origin` check.
#[test]
fn a_form_submitted_from_another_site_changes_nothing() {
    let s = served("crossorigin");
    s.ask("Ada", "hello");
    let id = s.queue()[0]["request_id"]
        .as_str()
        .expect("an id")
        .to_string();

    let (status, _, body) = get(
        &format!("{}/people", s.base),
        &[
            "-u",
            "alice:a",
            "-X",
            "POST",
            "-H",
            "Origin: https://evil.example",
            "-d",
            &format!("action=decline&request_id={id}"),
        ],
    );
    assert_eq!(status, 403, "{body}");
    assert_eq!(s.queue().len(), 1, "a cross-site form emptied the queue");

    // The JSON endpoint an operator's browser could also be steered into.
    let (status, answer) = crate::support::curl(&[
        "-u",
        "alice:a",
        "-X",
        "POST",
        "-H",
        "Origin: https://evil.example",
        "-d",
        r#"{"user":"mallory","grants":["agents/demo.git write"]}"#,
        &format!("{}/api/accounts/invite", s.base),
    ]);
    assert_eq!(status, 403, "{answer}");
    std::fs::remove_dir_all(&s.work).ok();
}

/// The console's own form works, which is the other half of the check
/// above: refusing everything would pass that test too.
#[test]
fn the_operator_lets_somebody_in_from_the_page() {
    let s = served("grantform");
    let link = s.ask("Ada", "hello");
    let id = s.queue()[0]["request_id"]
        .as_str()
        .expect("an id")
        .to_string();

    let (status, headers, _) = get(
        &format!("{}/people", s.base),
        &[
            "-u",
            "alice:a",
            "-X",
            "POST",
            "-d",
            &format!("action=grant&request_id={id}&repo=agents/demo&level=write"),
        ],
    );
    assert_eq!(status, 303, "{headers}");
    assert_eq!(
        header_value(&headers, "Location").as_deref(),
        Some("/people?said=granted"),
        "{headers}"
    );
    assert!(s.queue().is_empty());

    // The link the asker holds is live, and grants what the form said.
    let (status, _, body) = get(&link, &[]);
    assert_eq!(status, 200);
    assert!(body.contains("write"), "{body}");
    assert!(body.contains("agents/demo"), "{body}");
    std::fs::remove_dir_all(&s.work).ok();
}

/// Minting a link by hand, from the same page, without the CLI.
#[test]
fn the_operator_mints_an_invite_from_the_page() {
    let s = served("mintform");
    let (status, _, body) = get(
        &format!("{}/people", s.base),
        &[
            "-u",
            "alice:a",
            "-X",
            "POST",
            "-d",
            "action=invite&display_name=Grace&repo=agents/demo&level=read",
        ],
    );
    assert_eq!(status, 200, "{body}");
    // The link is rendered rather than redirected to, because this node
    // kept only a hash of its secret half.
    assert!(body.contains("/join?i="), "no link on the page: {body}");
    assert!(body.contains("Grace"), "{body}");
    std::fs::remove_dir_all(&s.work).ok();
}

/// The queue is capped, because the endpoint that fills it needs no
/// credential. The sixty-fifth stranger is told the truth rather than
/// silently dropped.
#[test]
fn the_queue_has_a_ceiling() {
    let s = served("ceiling");
    for n in 0..choir_node::accounts::MAX_PENDING_REQUESTS {
        s.ask(&format!("asker {n}"), "");
    }
    let (_, issued) =
        crate::support::curl(&["-X", "POST", &format!("{}/api/access/challenge", s.base)]);
    let challenge = issued["challenge"].as_str().expect("a challenge");
    let bits = u32::try_from(issued["bits"].as_u64().expect("bits")).expect("small");
    let body = serde_json::json!({
        "display_name": "one too many",
        "challenge": challenge,
        "nonce": stamp(challenge, bits),
    })
    .to_string();
    let (status, answer) =
        crate::support::curl(&["-X", "POST", "-d", &body, &format!("{}/api/access", s.base)]);
    assert_eq!(status, 503, "{answer}");
    assert!(
        answer["error"]
            .as_str()
            .unwrap_or_default()
            .contains("full"),
        "{answer}"
    );
    assert_eq!(s.queue().len(), choir_node::accounts::MAX_PENDING_REQUESTS);
    std::fs::remove_dir_all(&s.work).ok();
}

/// The front page's contact line is set from the console, not over ssh.
///
/// It was a file an operator wrote by hand and a restart to take effect,
/// which is a value nobody ever changes. It is still a file, because a
/// personal identifier belongs in the operator's own untracked state and
/// never compiled into a binary; what moved is who writes it.
#[test]
fn the_operator_sets_the_front_pages_contact_from_the_console() {
    let s = served("contact");

    // Nothing offered until somebody names something.
    let (_, _, front) = get(&format!("{}/", s.base), &[]);
    assert!(!front.contains("write to"), "{front}");

    let (status, headers, _) = get(
        &format!("{}/people", s.base),
        &[
            "-u",
            "alice:a",
            "-X",
            "POST",
            "-d",
            "action=contact&contact=someone%40example.invalid",
        ],
    );
    assert_eq!(status, 303, "{headers}");
    assert_eq!(
        header_value(&headers, "Location").as_deref(),
        Some("/people?said=contact"),
        "{headers}"
    );

    // No restart: the next fetch of the front page has it.
    let (_, _, front) = get(&format!("{}/", s.base), &[]);
    assert!(front.contains("write to"), "{front}");
    assert!(front.contains("mailto:someone@example.invalid"), "{front}");

    // And the console shows what is set, so an operator can see it
    // before changing it.
    let (_, _, console) = get(&format!("{}/people", s.base), &["-u", "alice:a"]);
    assert!(console.contains("someone@example.invalid"), "{console}");

    // A value with a newline in it would be a second line in a file
    // whose grammar is one, and a header injection on the page that
    // answers anybody.
    let (status, _, _) = get(
        &format!("{}/people", s.base),
        &[
            "-u",
            "alice:a",
            "-X",
            "POST",
            "-d",
            "action=contact&contact=one%0Atwo",
        ],
    );
    assert_eq!(status, 303);
    let (_, _, front) = get(&format!("{}/", s.base), &[]);
    assert!(
        front.contains("someone@example.invalid"),
        "a refused value replaced the good one: {front}"
    );

    // Clearing takes it off the front page again.
    let (status, _, _) = get(
        &format!("{}/people", s.base),
        &["-u", "alice:a", "-X", "POST", "-d", "action=contact"],
    );
    assert_eq!(status, 303);
    let (_, _, front) = get(&format!("{}/", s.base), &[]);
    assert!(!front.contains("write to"), "{front}");

    std::fs::remove_dir_all(&s.work).ok();
}
