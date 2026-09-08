//! The public front door (D57): the landing page and the invite link.
//!
//! These are the only routes on the node that answer without a
//! credential, so what is under test here is mostly what they *do not*
//! do. Four properties carry the design, and each has a test that fails
//! if it stops holding:
//!
//! 1. `GET` never spends the invite, so a chat client's preview fetch
//!    cannot burn a link before its reader clicks it.
//! 2. Every reason an invite does not work renders one page, byte for
//!    byte, so nobody can ask this node which invite ids exist.
//! 3. The secret reaches no file, because it rides in a query string and
//!    the request log is built from the path.
//! 4. The public surface is exactly four routes, and a fifth added by
//!    accident fails a test rather than quietly going live.

use choir_identity::{ActorKey, Registry};
use choir_node::accounts::SshKeysOut;
use choir_node::{AuthTable, Node, Platform};
use choir_oplog::MemLog;

/// A node with account self-service, an operator credential, and one
/// repository to be invited to.
struct Served {
    base: String,
    work: std::path::PathBuf,
}

/// `curl` keeping headers and body apart, since half of what these pages
/// promise is in the headers.
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
    let work = std::env::temp_dir().join(format!("choir-node-join-{tag}"));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");
    let acl_path = work.join("acl");
    std::fs::write(&acl_path, "alice @node write\nalice * write\n").expect("acl file");
    let mut table = AuthTable::new();
    table.insert("alice".into(), "a".into());
    let root = work.join("repos");
    let mut node = Node::bind_with_auth(&root, 0, Some(table)).expect("node binds free port");
    let port = node.port();
    node.create_repo("agents/demo.git").expect("repo created");
    node.watch_acl_file(acl_path).expect("acl loads");
    let handoff = work.join("handoff");
    node.enable_accounts(
        work.join("accounts.json"),
        Some(SshKeysOut {
            path: work.join("authorized_keys"),
            shim: std::path::PathBuf::from(env!("CARGO_BIN_EXE_choir-ssh")),
            root,
            handoff: handoff.clone(),
        }),
        None,
    )
    .expect("accounts enable");
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
        work,
    }
}

impl Served {
    /// Mints an invite as the operator and returns `(id, secret)`.
    fn invite(&self, body: &str) -> (String, String) {
        let (status, answer) = crate::support::curl(&[
            "-u",
            "alice:a",
            "-X",
            "POST",
            "-d",
            body,
            &format!("{}/api/accounts/invite", self.base),
        ]);
        assert_eq!(status, 200, "{answer}");
        let pair = answer["invite"].as_str().expect("invite pair").to_string();
        let (id, secret) = pair.split_once(':').expect("id:secret");
        (id.to_string(), secret.to_string())
    }

    fn join_url(&self, id: &str, secret: &str) -> String {
        format!("{}/join?i={id}&k={secret}", self.base)
    }
}

/// The question that started this: what does a stranger see?
#[test]
fn a_stranger_at_the_bare_address_gets_a_page_rather_than_a_password_box() {
    let s = served("landing");
    let (status, headers, body) = get(&format!("{}/", s.base), &[]);
    assert_eq!(status, 200, "an anonymous visitor was refused: {body}");
    assert!(
        header_value(&headers, "WWW-Authenticate").is_none(),
        "the browser was still told to pop a password box: {headers}"
    );
    assert!(
        body.contains("signed operation"),
        "the page does not say what this is: {body}"
    );
    // It says nothing about *this* node. A repository name here would be
    // readable by anybody who resolves the DNS record.
    assert!(
        !body.contains("agents/demo"),
        "the landing page lists a repository: {body}"
    );

    // The front door says what it takes to use the thing, as the
    // commands a reader actually types. A door that describes a
    // different product from the one behind it is worse than a door that
    // describes nothing.
    //
    // `git clone` rather than `choir git-credential`: the credential
    // helper is configured by `choir join` now, so naming it here would
    // be teaching a step nobody has to take. The install is first
    // because a stranger reading this has no `choir` yet.
    for command in ["install.sh", "choir join", "git clone", "choir propose"] {
        assert!(
            body.contains(command),
            "the front door does not name `{command}`: {body}"
        );
    }
    // Those commands are placeholders, not this node filled in. The
    // static-by-construction property is what keeps the address of a
    // private node off a page anybody can fetch.
    assert!(
        body.contains("choir join 'NODE"),
        "the example was specialised to this node: {body}"
    );
    assert!(
        !body.contains(&s.base),
        "the landing page prints this node's own address: {body}"
    );
    // And it says which state it is in. A private beta that does not say
    // so reads as a public service that is broken for the reader.
    assert!(
        body.contains("Private beta"),
        "the page does not say it is invite-only: {body}"
    );

    // A credential that is presented and wrong must still get the normal
    // challenge, or a reader who mistyped their password lands on a
    // marketing page and cannot retry.
    let (status, headers, _) = get(&format!("{}/", s.base), &["-u", "alice:wrong"]);
    assert_eq!(status, 401, "a wrong password was answered with a page");
    assert!(header_value(&headers, "WWW-Authenticate").is_some());

    // And the authenticated page is untouched.
    let (status, _, body) = get(&format!("{}/", s.base), &["-u", "alice:a"]);
    assert_eq!(status, 200);
    assert!(
        body.contains("agents/demo"),
        "signing in no longer shows the node's own state: {body}"
    );
    std::fs::remove_dir_all(&s.work).ok();
}

/// The failure that breaks real invite systems, rehearsed against the
/// user-agent that causes it.
#[test]
fn a_preview_bot_cannot_spend_an_invite_by_looking_at_it() {
    let s = served("unfurl");
    let (id, secret) = s.invite(r#"{"user":"bea","grants":["agents/demo.git write"]}"#);
    let url = s.join_url(&id, &secret);

    // Discord, Slack and every mail scanner do exactly this, several
    // times, before the human ever clicks.
    for _ in 0..5 {
        let (status, _, body) = get(
            &url,
            &[
                "-A",
                "Mozilla/5.0 (compatible; Discordbot/2.0; +https://discordapp.com)",
            ],
        );
        assert_eq!(status, 200);
        assert!(
            body.contains("You&#39;re invited") || body.contains("You're invited"),
            "the bot was not shown the invite page: {body}"
        );
    }

    // The person then clicks, and the invite is still theirs to spend.
    let (status, _, body) = post_join(&s.base, &id, &secret, "");
    assert_eq!(status, 200, "a preview fetch consumed the invite: {body}");
    assert!(
        body.contains("You&#39;re in") || body.contains("You're in"),
        "{body}"
    );
    std::fs::remove_dir_all(&s.work).ok();
}

/// Submits the join form the way the rendered page submits it.
fn post_join(base: &str, id: &str, secret: &str, ssh_key: &str) -> (u16, String, String) {
    let mut body = format!("i={id}&k={secret}");
    if !ssh_key.is_empty() {
        body.push_str("&ssh_key=");
        body.push_str(
            &ssh_key
                .replace('+', "%2B")
                .replace('=', "%3D")
                .replace(' ', "+"),
        );
    }
    let out = std::process::Command::new("curl")
        .args([
            "-s",
            "-i",
            "-w",
            "\n%{http_code}",
            "-X",
            "POST",
            "-d",
            &body,
        ])
        .arg(format!("{base}/join"))
        .output()
        .expect("curl runs");
    let text = String::from_utf8_lossy(&out.stdout);
    let (rest, code) = text.rsplit_once('\n').expect("status line");
    let (headers, page) = rest.split_once("\r\n\r\n").unwrap_or((rest, ""));
    (
        code.trim().parse().expect("numeric status"),
        headers.to_string(),
        page.to_string(),
    )
}

/// The oracle test. If this fails, the node has become a directory of
/// which accounts are pending on it.
#[test]
fn every_reason_an_invite_fails_answers_with_the_same_bytes() {
    let s = served("oracle");
    let (id, secret) = s.invite(r#"{"user":"bea","grants":["agents/demo.git read"]}"#);

    let unknown = get(
        &s.join_url(
            "invite-0000000000000000000000000000000000000000000000000000000000000000",
            &secret,
        ),
        &[],
    );
    let wrong_secret = get(&s.join_url(&id, "00000000000000000000000000000000"), &[]);
    let absent = get(&format!("{}/join", s.base), &[]);

    assert_eq!(
        unknown.2, wrong_secret.2,
        "an unknown id and a wrong secret render differently, which answers \
         \"does this invite exist?\" for anyone who asks"
    );
    assert_eq!(
        unknown.2, absent.2,
        "a missing parameter renders differently"
    );
    assert_eq!(unknown.0, wrong_secret.0, "the statuses differ");

    // And a spent invite joins them. This is the case a reader is most
    // likely to hit, and the one where a distinct message would be most
    // tempting to write.
    let (status, _, _) = post_join(&s.base, &id, &secret, "");
    assert_eq!(status, 200);
    let spent = get(&s.join_url(&id, &secret), &[]);
    assert_eq!(
        unknown.2, spent.2,
        "a used invite is distinguishable from one that never existed"
    );

    // And the expired one, which is the case a real reader is most likely
    // to meet — a link that sat in a channel overnight. It reaches the
    // node by a different path from the others (the secret still
    // authenticates; the summary is what refuses), so without this the
    // most-travelled failure route is the one nothing covers.
    //
    // Only one direction of the clock is safe to assert on here. This
    // test first also checked the invite was *still valid* before the
    // sleep, which reads as harmless and is not: it asserts that under a
    // second passed between minting and fetching, and in a harness whose
    // modules share a process and run in parallel, that is a race. It
    // flaked on its second full run.
    //
    // What survives needs time only to move forward. Sleeping past a
    // one-second lifetime cannot fail to expire the invite, however
    // loaded the machine is — a slow machine makes this *more* certain,
    // where the assertion it replaced got less.
    let (short_id, short_secret) =
        s.invite(r#"{"user":"cass","grants":["agents/demo.git read"],"expires_in_secs":1}"#);
    std::thread::sleep(std::time::Duration::from_millis(1_500));
    let stale = get(&s.join_url(&short_id, &short_secret), &[]);
    assert_eq!(
        unknown.2, stale.2,
        "an expired invite is distinguishable from one that never existed"
    );
    std::fs::remove_dir_all(&s.work).ok();
}

/// The credential handed over, and the fact that it is handed over once.
#[test]
fn redeeming_hands_over_a_working_credential_exactly_once() {
    let s = served("redeem");
    let (id, secret) = s.invite(r#"{"user":"bea","grants":["agents/demo.git write"]}"#);
    let (status, headers, body) = post_join(&s.base, &id, &secret, "");
    assert_eq!(status, 200, "{body}");

    // A page carrying a live password must not be storable by anything
    // between here and the reader.
    assert_eq!(
        header_value(&headers, "Cache-Control").as_deref(),
        Some("no-store"),
        "the credential page is cacheable: {headers}"
    );

    // The token is in the page, and it works.
    let token = body
        .split("<pre class=\"cmd\">")
        .nth(1)
        .and_then(|rest| rest.split("</pre>").next())
        .expect("the page shows a token")
        .to_string();
    assert!(!token.is_empty());
    let (status, answer) = crate::support::curl(&[
        "-u",
        &format!("bea:{token}"),
        &format!("{}/api/view", s.base),
    ]);
    assert_eq!(
        status, 200,
        "the minted token does not authenticate: {answer}"
    );

    // The page tells them what to do with it, with this node's address
    // already in the command.
    assert!(body.contains("git clone"), "no clone command: {body}");
    assert!(
        body.contains("agents/demo.git"),
        "the command names no repository: {body}"
    );

    // Single use, and the second attempt is the same refusal as any
    // other bad link.
    let (status, _, again) = post_join(&s.base, &id, &secret, "");
    assert_eq!(status, 403);
    assert!(
        !again.contains(&token),
        "the token was shown twice: {again}"
    );
    std::fs::remove_dir_all(&s.work).ok();
}

/// A mistyped ssh key must not cost somebody their invite.
#[test]
fn a_rejected_ssh_key_leaves_the_invite_redeemable() {
    let s = served("badkey");
    let (id, secret) = s.invite(r#"{"user":"bea","grants":["agents/demo.git write"]}"#);
    let (status, _, body) = post_join(&s.base, &id, &secret, "not-a-key");
    assert_eq!(status, 400, "{body}");
    assert!(
        body.contains("has not been used"),
        "the reader is not told their invite survived: {body}"
    );
    // The repair the page suggests actually works.
    let (status, _, body) = post_join(&s.base, &id, &secret, "");
    assert_eq!(status, 200, "a bad key burned the invite: {body}");
    std::fs::remove_dir_all(&s.work).ok();
}

/// The secret is in a query string precisely so that this holds.
#[test]
fn an_invite_secret_never_reaches_the_request_log() {
    let s = served("logging");
    let log = s.work.join("requests.jsonl");
    // The node under test writes its own log, so a second node is needed
    // rather than reconfiguring this one mid-flight.
    let acl_path = s.work.join("acl2");
    std::fs::write(&acl_path, "alice @node write\nalice * write\n").expect("acl");
    let mut table = AuthTable::new();
    table.insert("alice".into(), "a".into());
    let root = s.work.join("repos2");
    let mut node = Node::bind_with_auth(&root, 0, Some(table)).expect("binds");
    let port = node.port();
    node.watch_acl_file(acl_path).expect("acl loads");
    node.enable_accounts(s.work.join("accounts2.json"), None, None)
        .expect("accounts enable");
    node.enable_request_log(log.clone(), 1 << 20)
        .expect("log opens");
    std::thread::spawn(move || node.serve_forever());
    let base = format!("http://127.0.0.1:{port}");

    let secret = "0123456789abcdef0123456789abcdef";
    let (_, _, _) = get(&format!("{base}/join?i=invite-nope&k={secret}"), &[]);
    let recorded = std::fs::read_to_string(&log).expect("the log was written");
    assert!(
        recorded.contains("\"/join\""),
        "the request was not logged at all, so this test proves nothing: {recorded}"
    );
    assert!(
        !recorded.contains(secret),
        "the invite secret was written to disk: {recorded}"
    );
    std::fs::remove_dir_all(&s.work).ok();
}

/// What a third party's servers are allowed to learn from the link.
#[test]
fn the_social_preview_names_nothing_the_channel_should_not_see() {
    let s = served("card");
    let (id, secret) =
        s.invite(r#"{"user":"bea","grants":["agents/demo.git write"],"expires_in_secs":86400}"#);
    let (_, _, body) = get(&s.join_url(&id, &secret), &[]);
    // The whole stylesheet is inlined in the head, and it has English
    // prose in its comments — the word "beat" in one of them matches a
    // username of "bea". A test that reads the sheet is not reading the
    // preview card, so the sheet comes out first.
    let head = body.split("</head>").next().expect("a head");
    let (before, rest) = head.split_once("<style>").expect("the sheet is inlined");
    let (_, after) = rest.split_once("</style>").expect("the sheet closes");
    let head = format!("{before}{after}");
    assert!(head.contains("og:title"), "no preview card at all: {head}");
    for leak in ["bea", "agents/demo", "alice"] {
        assert!(
            !head.contains(leak),
            "the unfurl card names {leak:?}, which is rendered by a third party \
             and shown to everyone in the channel: {head}"
        );
    }
    // The image the card points at must actually be there, and be an
    // image. A dead `og:image` renders as a broken card, which is worse
    // than the plain one it replaced.
    let image = head
        .split("og:image\" content=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("the card names an image")
        .to_string();
    let (status, headers, _) = get(&image, &[]);
    assert_eq!(status, 200, "the preview image 404s: {image}");
    assert_eq!(
        header_value(&headers, "Content-Type").as_deref(),
        Some("image/png")
    );
    assert!(
        header_value(&headers, "Cache-Control")
            .unwrap_or_default()
            .contains("immutable"),
        "a chat client is made to refetch the card every time: {headers}"
    );

    // The specifics are still on the page, for the person who opened it.
    assert!(
        body.contains("bea"),
        "the reader is not told who they become: {body}"
    );
    assert!(
        body.contains("push to agents/demo"),
        "the reader is not told what they get: {body}"
    );
    std::fs::remove_dir_all(&s.work).ok();
}

/// The public surface is a closed list, and this is the test that keeps
/// it closed.
#[test]
fn exactly_the_intended_routes_answer_without_a_credential() {
    let s = served("surface");
    for path in ["/", "/index.html", "/join", "/static/card.png"] {
        let (status, _, _) = get(&format!("{}{path}", s.base), &[]);
        assert_eq!(status, 200, "{path} was expected to be public");
    }
    // D72's two, which are `POST` only: a `GET` of either is not a route.
    for path in ["/api/access", "/api/access/challenge"] {
        let (status, _, _) = get(&format!("{}{path}", s.base), &["-X", "POST", "-d", "{}"]);
        assert_ne!(status, 401, "{path} was expected to be public");
        let (status, _, _) = get(&format!("{}{path}", s.base), &[]);
        assert_eq!(status, 401, "{path} answered a GET without a credential");
    }
    // Everything else still meets the wall. `/status` and `/r/` are the
    // two that would leak the most if this list ever grew by accident.
    for path in ["/status", "/r/", "/api/view", "/llms.txt", "/account"] {
        let (status, _, _) = get(&format!("{}{path}", s.base), &[]);
        assert_eq!(status, 401, "{path} answered without a credential");
    }
    // A POST to the landing page is not a public route either.
    let out = std::process::Command::new("curl")
        .args([
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "-X",
            "POST",
            "-d",
            "x=1",
        ])
        .arg(format!("{}/", s.base))
        .output()
        .expect("curl runs");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "401",
        "POST to the landing page skipped authentication"
    );
    std::fs::remove_dir_all(&s.work).ok();
}

/// A node without `--accounts-file` has no invites to redeem, and says
/// so rather than pretending the reader's link is bad.
#[test]
fn a_node_without_self_service_says_so_instead_of_blaming_the_link() {
    let work = std::env::temp_dir().join("choir-node-join-noselfservice");
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");
    let mut table = AuthTable::new();
    table.insert("alice".into(), "a".into());
    let node = Node::bind_with_auth(&work.join("repos"), 0, Some(table)).expect("binds");
    let port = node.port();
    std::thread::spawn(move || node.serve_forever());
    let base = format!("http://127.0.0.1:{port}");

    let (status, _, body) = get(&format!("{base}/join?i=invite-x&k=y"), &[]);
    assert_eq!(status, 200);
    assert!(
        body.contains("does not accept invites"),
        "a node with the feature off blamed the reader's link: {body}"
    );
    std::fs::remove_dir_all(&work).ok();
}

/// The pages carry the read surface's CSP and run nothing.
#[test]
fn the_public_pages_execute_nothing_and_reach_nowhere() {
    let s = served("csp");
    let (id, secret) = s.invite(r#"{"user":"bea","grants":["agents/demo.git read"]}"#);
    // The invite page is the one that must run nothing at all: its own
    // address is a live credential, so a script on it is a script with a
    // secret in `location`.
    let (_, headers, body) = get(&s.join_url(&id, &secret), &[]);
    let csp = header_value(&headers, "Content-Security-Policy").expect("a policy");
    assert!(csp.contains("default-src 'none'"), "{csp}");
    assert!(
        csp.contains("form-action 'self'"),
        "the form cannot submit: {csp}"
    );
    assert!(
        !csp.contains("script-src"),
        "the invite page licensed script: {csp}"
    );
    assert!(
        !body.contains("<script"),
        "the invite page carries script: {body}"
    );

    // The landing page runs D72's ask, and exactly that: one file off
    // this origin, nothing inline, and nowhere to connect but here.
    let (_, headers, body) = get(&format!("{}/", s.base), &[]);
    let csp = header_value(&headers, "Content-Security-Policy").expect("a policy");
    assert!(csp.contains("default-src 'none'"), "{csp}");
    assert!(csp.contains("script-src 'self'"), "{csp}");
    assert!(csp.contains("connect-src 'self'"), "{csp}");
    assert!(
        !csp.contains("unsafe-inline") || !csp.contains("script-src 'unsafe-inline'"),
        "the landing page licensed inline script: {csp}"
    );
    assert!(
        body.contains("<script src=\"/static/webauthn.js\""),
        "the ask form has no script to run it: {body}"
    );
    assert!(
        body.matches("<script").count() == 1,
        "more than the one shared file: {body}"
    );

    // `same-origin`, not `no-referrer`, and the difference is not a
    // weakening: a `Referer` is still withheld from every other site,
    // which is the whole property here, because this URL carries the
    // invite secret in its query string. `no-referrer` additionally
    // nulls the `Origin` header on every non-GET request, same-origin
    // ones included, which is what broke every form on this surface.
    for url in [format!("{}/", s.base), s.join_url(&id, &secret)] {
        let (_, headers, _) = get(&url, &[]);
        assert_eq!(
            header_value(&headers, "Referrer-Policy").as_deref(),
            Some("same-origin"),
            "the invite secret can leak in a Referer header: {headers}"
        );
    }
    // A crawler must be told not to index a page whose URL is a
    // credential.
    let (_, headers, _) = get(&s.join_url(&id, &secret), &[]);
    assert_eq!(
        header_value(&headers, "X-Robots-Tag").as_deref(),
        Some("noindex, nofollow")
    );
    std::fs::remove_dir_all(&s.work).ok();
}

/// The operator pastes a link, not an `id:secret` pair.
#[test]
fn minting_an_invite_answers_with_the_link_to_paste() {
    let s = served("minturl");
    let (status, answer) = crate::support::curl(&[
        "-u",
        "alice:a",
        "-X",
        "POST",
        "-d",
        r#"{"user":"bea","grants":["agents/demo.git write"]}"#,
        &format!("{}/api/accounts/invite", s.base),
    ]);
    assert_eq!(status, 200, "{answer}");
    let url = answer["join_url"]
        .as_str()
        .unwrap_or_else(|| panic!("no join_url to paste: {answer}"));
    assert!(
        url.starts_with(&s.base),
        "the link points somewhere other than the node the operator reached: {url}"
    );
    // The link the operator was handed is the link that works. Asserting
    // it renders, rather than that it is well formed, is the difference
    // between testing a string and testing a door.
    let (status, _, body) = get(url, &[]);
    assert_eq!(status, 200);
    assert!(
        body.contains("bea"),
        "the minted link does not open the invite: {body}"
    );
    std::fs::remove_dir_all(&s.work).ok();
}

/// An invite link is minted with the scheme the *world* reaches this node
/// on, not the one its own listener speaks (D71).
///
/// The private beta terminates TLS at a reverse proxy and the node itself
/// serves plaintext on loopback, so the scheme it was writing into every
/// join link was `http`. An invite is a bearer credential carried in that
/// URL: the recipient's first request would put it on the wire in
/// cleartext, and only the redirect that followed would be encrypted.
/// Found on a live node the day the proxy went in, which is the whole
/// reason the declaration exists rather than a guess from the listener.
#[test]
fn an_invite_behind_a_tls_proxy_is_minted_as_an_https_link() {
    let work = std::env::temp_dir().join("choir-node-join-proxy-scheme");
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");
    let acl_path = work.join("acl");
    std::fs::write(&acl_path, "alice @node write\nalice * write\n").expect("acl file");
    let mut table = AuthTable::new();
    table.insert("alice".into(), "a".into());
    let root = work.join("repos");
    let mut node = Node::bind_with_auth(&root, 0, Some(table)).expect("node binds free port");
    let port = node.port();
    node.watch_acl_file(acl_path).expect("acl loads");
    // Plaintext on loopback, exactly like the deployed node: the proxy in
    // front is what holds the certificate.
    node.behind_tls_proxy();
    node.enable_accounts(work.join("accounts.json"), None, None)
        .expect("accounts enable");
    std::thread::spawn(move || node.serve_forever());
    let base = format!("http://127.0.0.1:{port}");

    let (status, answer) = crate::support::curl(&[
        "-u",
        "alice:a",
        "-X",
        "POST",
        "--data-binary",
        r#"{"user":"bob","grants":["agents/demo.git write"]}"#,
        &format!("{base}/api/accounts/invite"),
    ]);
    assert_eq!(status, 200, "{answer}");
    let join_url = answer["join_url"]
        .as_str()
        .expect("the invite carries a join_url");
    assert!(
        join_url.starts_with("https://"),
        "a node behind a TLS proxy must mint https links, not {join_url}"
    );

    std::fs::remove_dir_all(&work).ok();
}

/// The landing page tells a stranger how to ask, and takes the address
/// from operator state rather than from this source tree.
///
/// Before this it named the two things to do with a credential and said
/// nothing to a person who has none, which is most people who follow a
/// link to a node they have heard about: a public front door whose only
/// advice assumed you were already expected.
///
/// The address is deliberately not compiled in. A personal identifier
/// baked into a published binary cannot be taken back out of the copies,
/// which is why every tracked file here writes host addresses and owner
/// names as placeholders.
#[test]
fn the_front_door_names_the_operator_only_when_the_operator_named_themselves() {
    let work = std::env::temp_dir().join("choir-node-join-contact");
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");

    let landing = |contact: Option<&str>| {
        let root = work.join(match contact {
            Some(_) => "with",
            None => "without",
        });
        std::fs::create_dir_all(root.join(".choir")).expect("state dir");
        if let Some(contact) = contact {
            std::fs::write(root.join(".choir/contact"), format!("{contact}\n")).expect("contact");
        }
        let mut table = AuthTable::new();
        table.insert("alice".into(), "a".into());
        let node = Node::bind_with_auth(&root, 0, Some(table)).expect("node binds");
        let port = node.port();
        std::thread::spawn(move || node.serve_forever());
        let out = std::process::Command::new("curl")
            .args(["-s", &format!("http://127.0.0.1:{port}/")])
            .output()
            .expect("curl runs");
        String::from_utf8_lossy(&out.stdout).to_string()
    };

    let silent = landing(None);
    assert!(
        !silent.contains("write to"),
        "a node whose operator named nobody must offer no address"
    );

    // A shape, not a real address: what is asserted is that the file's
    // line reaches the page and is linked, never a particular person.
    let named = landing(Some("someone@example.invalid"));
    assert!(
        named.contains("write to"),
        "the invitation to write is missing: {named}"
    );
    assert!(
        named.contains("mailto:someone@example.invalid"),
        "an address must become something a reader can act on: {named}"
    );

    std::fs::remove_dir_all(&work).ok();
}
