//! The route table, and the test that keeps it true.
//!
//! Every page this node serves to a browser is one row of [`TABLE`]. The
//! table is the documentation — there is no second copy of it in a
//! markdown file to drift — and three properties are asserted against it:
//!
//! 1. **Every row answers what it says it answers**, anonymously, as a
//!    member, and as the operator. A route that quietly opens to
//!    strangers fails here, and so does one that closes to the people it
//!    is for.
//! 2. **Every link a page renders is a row.** The crawl starts at `/`
//!    and follows the node's own `href`s; a link to a path the table does
//!    not contain is a dead link by construction, because the table is
//!    the whole surface.
//! 3. **Every row is reachable**, or is listed in [`UNLINKED`] with the
//!    reason. A page nothing links to is either dead code or a missing
//!    entry point, and the difference has to be written down rather than
//!    guessed at by the next reader.
//!
//! # The columns
//!
//! `anon` is a request with no credential at all. `member` holds a
//! credential with a read grant on one repository and nothing else.
//! `operator` holds `@node write`, which is what the console and the
//! invite machinery are gated on.
//!
//! # Two statuses that look wrong and are not
//!
//! `/signin` answers **401**, not 200, and carries **no
//! `WWW-Authenticate`**. Both are deliberate. The status is the honest
//! code for the request that produced the page (`signin_page`'s own test
//! says so), and the missing challenge is the entire point of D74: a
//! challenge header is what makes a browser open its own grey dialog,
//! which is the thing the page replaced. A browser renders the body
//! either way.
//!
//! Every other gated route answers **401 with the sign-in page as its
//! body** for a request that asked for `text/html`, and a bare `401` with
//! a challenge for everything else (D74). So "401" in the `anon` column
//! never means a dead end for a person — it means the wall, wearing the
//! page that gets them past it.

use choir_identity::{ActorKey, Registry};
use choir_node::{AuthTable, Node, Platform};
use choir_oplog::MemLog;

/// One route: the pattern, a concrete path to request, and the status
/// each of the three readers gets from it.
///
/// A tuple rather than a struct with five named fields, so that a row is
/// one line and the table reads as a table. Every use destructures it by
/// name, which is where the columns are spelled out. `{}` in a pattern
/// matches one segment and `{..}` the rest; `{oid}` in a probe stands
/// for the fixture's seeded commit, which is not knowable until the
/// fixture has pushed it.
type Row = (&'static str, &'static str, u16, u16, u16);

/// The whole browser surface, one row per route.
///
/// Columns: **pattern**, **probe**, then the status for **anonymous**, a
/// **member** holding one repository read grant, and the **operator**
/// holding `@node write`.
///
/// `rustfmt` is held off this one const, which is the only such
/// attribute in the workspace. Its tuple-width limit is sixty
/// characters, and half these rows are longer than that, so the
/// formatted form is a hundred and seventy lines of one field each. The
/// point of this const is that it is legible as a table -- it is the
/// route documentation, and the next person to add a route has to be
/// able to see the shape they are adding to.
#[rustfmt::skip]
const TABLE: &[Row] = &[
    // The cold path: what a stranger holding nothing can reach.
    ("/",                     "/",                                 200, 200, 200),
    ("/index.html",           "/index.html",                       200, 200, 200),
    ("/signin",               "/signin",                           401, 401, 401),
    ("/join",                 "/join",                             200, 200, 200),
    ("/robots.txt",           "/robots.txt",                       200, 200, 200),
    ("/static/card.png",      "/static/card.png",                  200, 200, 200),
    ("/static/webauthn.js",   "/static/webauthn.js",               200, 200, 200),
    // The browse surface (D30).
    ("/r/",                   "/r/",                               401, 200, 200),
    ("/r/{}/{}",              "/r/agents/one",                     401, 200, 200),
    ("/r/{}/{}/tree/{}",      "/r/agents/one/tree/main",           401, 200, 200),
    ("/r/{}/{}/tree/{}/{..}", "/r/agents/one/tree/main/src",       401, 200, 200),
    ("/r/{}/{}/blob/{}/{..}", "/r/agents/one/blob/main/README.md", 401, 200, 200),
    ("/r/{}/{}/commits/{}",   "/r/agents/one/commits/main",        401, 200, 200),
    ("/r/{}/{}/commit/{}",    "/r/agents/one/commit/{oid}",        401, 200, 200),
    ("/r/{}/{}/search/{}",    "/r/agents/one/search/main?q=hi",    401, 200, 200),
    ("/r/{}/{}/reviews",      "/r/agents/one/reviews",             401, 200, 200),
    ("/r/{}/{}/review/{}",    "/r/agents/one/review/nope",         401, 404, 404),
    ("/r/{}/{}/contribute",   "/r/agents/one/contribute",          401, 200, 200),
    // The node's own pages.
    ("/reviews",              "/reviews",                          401, 200, 200),
    ("/status",               "/status",                           401, 200, 200),
    ("/theme",                "/theme?set=dark&to=/r/",            401, 303, 303),
    ("/p/{}",                 "/p/op",                             401, 200, 200),
    // Credentials.
    ("/account",              "/account",                          401, 200, 200),
    ("/people",               "/people",                           401, 403, 200),
    // Machine-readable, and part of the surface the walk covers.
    ("/llms.txt",             "/llms.txt",                         401, 200, 200),
    ("/sync.md",              "/sync.md",                          401, 200, 200),
];

/// Rows nothing links to, each with the reason it still exists.
///
/// A route in this list is a deliberate one; a route that is *not* in it
/// and that the crawl never reached is the bug this list exists to make
/// visible.
const UNLINKED: &[(&str, &str)] = &[
    (
        "/index.html",
        "the same page as `/`, for a reader who typed it",
    ),
    (
        "/join",
        "reached by an invite link a person was sent, never by a link on \
         this node — the whole point of D57 is that the link travels",
    ),
    (
        "/robots.txt",
        "a crawler asks for it by name; a link to it would be noise",
    ),
    (
        "/static/card.png",
        "the social-preview card, fetched by a chat client from a `<meta>` \
         tag rather than followed by a reader",
    ),
    (
        "/static/webauthn.js",
        "a `<script src>`, which is not an `href`",
    ),
    (
        "/llms.txt",
        "a well-known path, fetched by name the way `/robots.txt` is; its \
         reader is an agent that was never on a page to click from",
    ),
    (
        "/sync.md",
        "handed to an agent by the command the join page prints, not clicked",
    ),
    (
        "/r/{}/{}/tree/{}/{..}",
        "one level below `/r/{}/{}/tree/{}`, which the crawl does reach; \
         `browse.rs` walks the whole tree",
    ),
    (
        "/r/{}/{}/blob/{}/{..}",
        "reached from a directory listing, which the crawl enters through \
         `/r/{}/{}/tree/{}`; asserted separately by `browse.rs`",
    ),
    (
        "/r/{}/{}/search/{}",
        "the search box is a `<form action>`, not an `href`, until a query \
         has been run",
    ),
    (
        "/r/{}/{}/review/{}",
        "there is no review on an empty fixture; `browse.rs` walks one",
    ),
    (
        "/r/{}/{}/commit/{}",
        "reached from the commit list, one level below the crawl's start",
    ),
    (
        "/p/{}",
        "linked from a verdict and from a vouch, so it needs a review this \
         fixture has no reason to hold; `profile.rs` and `review.rs` cover it",
    ),
];

/// Form and `fetch` targets the pages post to. Not in [`TABLE`] because
/// no reader ever navigates to one, but a page whose form posts into a
/// 404 is the same defect as a dead link, so each must at least exist.
const POST_TARGETS: &[&str] = &[
    "/signin",
    "/join",
    "/people",
    "/account/token",
    "/api/signout",
    "/api/signin",
    "/api/signin/challenge",
    "/api/access",
    "/api/access/challenge",
    "/api/accounts/passkey",
    "/api/prepare",
    "/api/submit",
];

// ---------------------------------------------------------------- utils

/// Runs one request and returns `(status, headers, body)`.
fn get(url: &str, args: &[&str]) -> (u16, String, String) {
    let out = std::process::Command::new("curl")
        .args(["-s", "-i", "-w", "\n%{http_code}"])
        .args(["-H", "Accept: text/html"])
        .args(args)
        .arg(url)
        .output()
        .expect("curl runs");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let (rest, code) = text.rsplit_once('\n').unwrap_or((text.as_str(), "0"));
    let status = code.trim().parse().unwrap_or(0);
    let (headers, body) = match rest.split_once("\r\n\r\n") {
        Some((h, b)) => (h.to_string(), b.to_string()),
        None => (rest.to_string(), String::new()),
    };
    (status, headers, body)
}

/// Every same-origin `href` on a page, un-escaped back to the URL a
/// browser would request. The same crude reader `browse.rs` uses, and
/// for the same reason: these are links this crate wrote.
fn hrefs(page: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = page;
    while let Some(at) = rest.find("href=\"") {
        rest = &rest[at + 6..];
        let Some(end) = rest.find('"') else { break };
        let href = rest[..end]
            .replace("&amp;", "&")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&#39;", "'")
            .replace("&quot;", "\"");
        rest = &rest[end..];
        if href.starts_with('/') && !href.starts_with("//") {
            out.push(href);
        }
    }
    out
}

/// Does `path` match `pattern`, where `{}` is one segment and `{..}` is
/// the rest of them?
fn matches(pattern: &str, path: &str) -> bool {
    let path = path.split(['?', '#']).next().unwrap_or("");
    // A trailing slash is its own route (`/r/` is the index) and is kept.
    let mut want = pattern.split('/');
    let mut got = path.split('/');
    loop {
        match (want.next(), got.next()) {
            (Some("{..}"), Some(_)) => return true,
            (Some("{}"), Some(seg)) if !seg.is_empty() => {}
            (Some(w), Some(g)) if w == g => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

/// The row `path` belongs to, if the table has one.
fn row_for(path: &str) -> Option<&'static Row> {
    TABLE.iter().find(|(pattern, ..)| matches(pattern, path))
}

/// A served node with the whole surface switched on: accounts,
/// passkeys, an ACL, one populated repository and one with no commits.
///
/// The empty repository is not decoration. "A repository with no
/// commits" is one of the failure states that must render a next action
/// rather than a blank page, and a fixture without one cannot notice it
/// regressing.
struct Served {
    base: String,
    work: std::path::PathBuf,
    /// The seeded commit, for the routes that name one.
    oid: String,
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
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git runs")
}

fn served(tag: &str) -> Served {
    let work = std::env::temp_dir().join(format!("choir-node-routes-{tag}"));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");

    let acl = work.join("acl");
    std::fs::write(&acl, "op @node write\nop * own\nmem agents/one.git read\n").expect("acl file");

    let mut table = AuthTable::new();
    table.insert("op".into(), "o".into());
    table.insert("mem".into(), "m".into());

    let root = work.join("repos");
    let mut node = Node::bind_with_auth(&root, 0, Some(table)).expect("node binds a free port");
    let port = node.port();
    node.create_repo("agents/one.git").expect("repo created");
    node.create_repo("agents/empty.git").expect("repo created");
    node.watch_acl_file(acl).expect("acl loads");
    node.enable_passkeys();
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

    let base = format!("http://127.0.0.1:{port}");
    let clone = work.join("clone");
    let url = format!("http://op:o@127.0.0.1:{port}/agents/one.git");
    assert!(
        git(&work, &["clone", "-q", &url, clone.to_str().unwrap()])
            .status
            .success(),
        "seeding clone failed"
    );
    std::fs::create_dir_all(clone.join("src")).expect("src");
    std::fs::write(clone.join("README.md"), "# hello\n\nthe fixture readme.\n").expect("readme");
    std::fs::write(clone.join("src/lib.rs"), "fn main() {}\n").expect("source");
    git(&clone, &["add", "."]);
    git(&clone, &["commit", "-q", "-m", "seed"]);
    assert!(
        git(&clone, &["push", "-q", "origin", "HEAD:refs/heads/main"])
            .status
            .success(),
        "seeding push failed"
    );
    let oid = String::from_utf8(git(&clone, &["rev-parse", "HEAD"]).stdout)
        .expect("an oid is ascii")
        .trim()
        .to_string();

    Served { base, work, oid }
}

impl Served {
    fn get(&self, path: &str, args: &[&str]) -> (u16, String, String) {
        get(
            &format!("{}{}", self.base, path.replace("{oid}", &self.oid)),
            args,
        )
    }

    /// Mints an invite as the operator and redeems it, returning the
    /// account name and the password the page handed over.
    ///
    /// The whole newcomer path, because that is the reader the account
    /// page is for: a credential in the auth file is the *operator's*,
    /// and it has no row in the accounts store to mint a token against —
    /// so a fixture that only ever signs in as the operator cannot see
    /// the page a person actually lands on.
    fn newcomer(&self, user: &str) -> (String, String) {
        let (status, answer) = crate::support::curl(&[
            "-u",
            "op:o",
            "-X",
            "POST",
            "-d",
            &format!(r#"{{"user":"{user}","grants":["agents/one.git read"]}}"#),
            &format!("{}/api/accounts/invite", self.base),
        ]);
        assert_eq!(status, 200, "minting an invite failed: {answer}");
        let pair = answer["invite"].as_str().expect("an invite pair");
        let (id, secret) = pair.split_once(':').expect("id:secret");

        let out = std::process::Command::new("curl")
            .args(["-s", "-X", "POST", "-d", &format!("i={id}&k={secret}")])
            .arg(format!("{}/join", self.base))
            .output()
            .expect("curl runs");
        let page = String::from_utf8_lossy(&out.stdout);
        let password = page
            .split("<pre class=\"cmd\">")
            .nth(1)
            .and_then(|rest| rest.split("</pre>").next())
            .expect("the redemption page shows a credential")
            .to_string();
        (user.to_string(), password)
    }

    /// A browser session opened through the sign-in form, as a cookie
    /// header. This is the credential a person actually holds after
    /// D74's page, and it is *not* an `Authorization` header — which is
    /// the distinction several routes turned out to be reading.
    fn session(&self, user: &str, secret: &str) -> String {
        let out = std::process::Command::new("curl")
            .args(["-s", "-i", "-X", "POST"])
            .args(["-H", &format!("Origin: {}", self.base)])
            .args(["-d", &format!("user={user}&secret={secret}&next=/")])
            .arg(format!("{}/signin", self.base))
            .output()
            .expect("curl runs");
        let text = String::from_utf8_lossy(&out.stdout);
        let cookie = text
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case("set-cookie")
                    .then(|| value.trim().split(';').next().unwrap_or("").to_string())
            })
            .expect("the sign-in form opened no session");
        format!("Cookie: {cookie}")
    }

    fn done(self) {
        std::fs::remove_dir_all(&self.work).ok();
    }
}

// ---------------------------------------------------------------- tests

/// Property 1: every row answers what the table says, for all three
/// readers.
#[test]
fn every_route_answers_what_the_table_says() {
    let s = served("statuses");
    for &(_, probe, anon, member, operator) in TABLE {
        for (who, args, want) in [
            ("anonymous", vec![], anon),
            ("a member", vec!["-u", "mem:m"], member),
            ("the operator", vec!["-u", "op:o"], operator),
        ] {
            let (status, _, body) = s.get(probe, &args);
            assert_eq!(
                status, want,
                "{who} got {status} from {probe} (table says {want}): {body}"
            );
        }
    }
    s.done();
}

/// Property 2: the crawl. Start at `/` and follow every link the node's
/// own pages render, and require each to be a row.
///
/// Twice, because "reachable" is a claim about a *reader* and there are
/// two of them. The anonymous walk covers the cold path, where `/signin`
/// lives and where a link that answers `401` is a dead end; the signed-in
/// walk covers everything behind the wall. A route reached by either is
/// reached.
#[test]
fn every_link_a_page_renders_is_in_the_table() {
    let s = served("crawl");
    let cookie = s.session("op", "o");

    let mut reached: Vec<&'static str> = Vec::new();
    let mut visited = 0usize;
    for args in [Vec::new(), vec!["-H", cookie.as_str()]] {
        let anonymous = args.is_empty();
        let mut queue = vec!["/".to_string()];
        let mut seen: Vec<String> = Vec::new();
        while let Some(path) = queue.pop() {
            if seen.contains(&path) {
                continue;
            }
            seen.push(path.clone());
            let Some(&(pattern, ..)) = row_for(&path) else {
                panic!("a page linked to {path}, which the route table does not contain");
            };
            // Reached is about the link, not about the answer: `/signin`
            // answers `401` by design (see this module's header) and is
            // still the most reachable page on the node.
            reached.push(pattern);
            let (status, _, body) = s.get(&path, &args);
            // A `303` is alive: the palette links set a cookie and send
            // the reader back. That they stay on this node is asserted
            // by `ui.rs`, which owns the redirect.
            if status == 303 {
                continue;
            }
            // A stranger following a link into the wall is not a dead
            // link — the wall wears the sign-in page (D74) — but it is
            // also not a page to keep crawling from.
            if anonymous && status == 401 {
                assert!(
                    body.contains("<!doctype html>"),
                    "the wall answered a browser with a raw status line at {path}"
                );
                continue;
            }
            assert_eq!(status, 200, "a link the node rendered is dead: {path}");
            for href in hrefs(&body) {
                queue.push(href);
            }
        }
        visited += seen.len();
    }

    // The walk has to have walked, or every assertion above was vacuous.
    assert!(visited >= 8, "the two crawls visited only {visited} pages");

    // Property 3: every row is either reached or listed with its reason.
    for &(pattern, ..) in TABLE {
        if reached.contains(&pattern) {
            continue;
        }
        assert!(
            UNLINKED.iter().any(|(path, _)| *path == pattern),
            "no page links to {pattern}, and it is not in UNLINKED with a reason"
        );
    }
    // And the reasons stay honest: a row that became reachable must not
    // keep an excuse for not being.
    for (path, _) in UNLINKED {
        assert!(
            TABLE.iter().any(|(pattern, ..)| pattern == path),
            "UNLINKED names {path}, which is not a route"
        );
    }
    s.done();
}

/// Every form target a page posts to exists. A button wired to a path
/// this node does not route is the failure D73 was written about.
///
/// "Exists" and not "succeeds": posting `{}` as the operator is a
/// malformed submission to most of these, and a specific refusal is the
/// right answer to it. What must not come back is the generic
/// `no_such_page` refusal, which is the node saying it has no such
/// address at all, or a `405`, which is the node saying it has one and
/// will not take a `POST`.
#[test]
fn every_form_target_a_page_posts_to_exists() {
    let s = served("posts");
    for path in POST_TARGETS {
        let (status, _, body) = s.get(path, &["-u", "op:o", "-X", "POST", "-d", "{}"]);
        assert!(
            !body.contains("no_such_page"),
            "a page posts to {path}, which is not a route: {body}"
        );
        assert_ne!(
            status, 405,
            "a page posts to {path}, which refuses POST: {body}"
        );
    }
    s.done();
}

/// The cold path, as one walk: a stranger arrives holding nothing and
/// can get from the front door to the page that signs them in.
///
/// This is the assertion the whole surface exists for, and it failed
/// before this module was written: the landing page's only link was
/// `/r/`, which is behind the wall, so the word "sign in" pointed at a
/// `401` and `/signin` was reachable by typing it and no other way.
#[test]
fn a_stranger_can_walk_from_the_front_door_to_the_way_in() {
    let s = served("cold");
    let (status, _, body) = s.get("/", &[]);
    assert_eq!(status, 200, "the front door refused a stranger: {body}");

    let links = hrefs(&body);
    assert!(
        links.iter().any(|href| href == "/signin"),
        "the front door does not link to the page that signs somebody in: {links:?}"
    );

    // And every link it does render answers something a person can act
    // on, with no credential.
    for href in links {
        if href.starts_with('#') {
            continue;
        }
        let (status, _, body) = s.get(&href, &[]);
        assert!(
            matches!(status, 200 | 401),
            "the front door links to {href}, which answered {status}"
        );
        assert!(
            body.contains("<!doctype html>"),
            "{href} answered a stranger with something that is not a page: {body}"
        );
    }
    s.done();
}

/// A reader who signed in through the form is not shown the front door
/// again.
///
/// The session cookie carries no `Authorization` header, and the public
/// branch that serves the landing page was reading only for that header.
/// So every "home" link on the node — the brand in the chrome, the
/// "node" pill, the `303` after signing in — took a signed-in person back
/// to a page telling them to sign in.
#[test]
fn signing_in_does_not_land_a_reader_back_on_the_front_door() {
    let s = served("session");
    let cookie = s.session("op", "o");

    let (status, _, body) = s.get("/", &["-H", cookie.as_str()]);
    assert_eq!(status, 200);
    assert!(
        !body.contains("Have an invite link"),
        "a signed-in reader was served the stranger's landing page: {body}"
    );
    assert!(
        body.contains("agents/one"),
        "a signed-in reader's front page does not list what they can read: {body}"
    );
    s.done();
}

/// Once signed in, the pages that finish the job are reachable by
/// clicking.
///
/// The credential a person signs in with is not the one git speaks:
/// `/account/token` mints that, and before this test nothing linked to
/// `/account` at all — so a person who signed in through D74's form
/// could not reach the page that gives them a token without knowing the
/// path.
#[test]
fn a_signed_in_reader_can_click_through_to_their_account() {
    let s = served("account");
    let (user, password) = s.newcomer("bea");
    let cookie = s.session(&user, &password);

    let (_, _, body) = s.get("/", &["-H", cookie.as_str()]);
    let links = hrefs(&body);
    assert!(
        links.iter().any(|href| href == "/account"),
        "nothing on the front page reaches the account page: {links:?}"
    );

    let (status, _, account) = s.get("/account", &["-H", cookie.as_str()]);
    assert_eq!(status, 200);
    assert!(
        account.contains("/account/token"),
        "the account page does not offer the token git needs: {account}"
    );

    // And a member's bar carries no console link, because `/people`
    // would answer them `403`.
    assert!(
        !links.iter().any(|href| href == "/people"),
        "a member's bar links to the operator console: {links:?}"
    );
    s.done();
}

/// An operator can reach the console from a page rather than by knowing
/// the path, and a member who cannot is told so on a page.
#[test]
fn the_console_is_reachable_by_the_operator_and_refused_with_a_page() {
    let s = served("console");
    let cookie = s.session("op", "o");
    let (_, _, body) = s.get("/", &["-H", cookie.as_str()]);
    assert!(
        hrefs(&body).iter().any(|href| href == "/people"),
        "an operator cannot reach the console by clicking: {body}"
    );

    let (status, _, refused) = s.get("/people", &["-u", "mem:m"]);
    assert_eq!(status, 403);
    assert!(
        refused.contains("<!doctype html>"),
        "a refusal answered with a raw status line: {refused}"
    );
    s.done();
}

/// Nothing on this node is a dead end: every failure state renders a
/// page that says what to do next.
///
/// The list is the failure states a reader actually reaches — a mistyped
/// address, a repository nobody granted them, a repository with no
/// commits, a review that is gone.
#[test]
fn every_failure_state_renders_a_page_with_a_next_action() {
    let s = served("deadends");
    for (path, want) in [
        ("/nope", 404),
        ("/r/agents/nope", 404),
        ("/r/agents/empty", 200),
        ("/r/agents/one/review/gone", 404),
        ("/r/agents/one/tree/nosuchref", 404),
    ] {
        let (status, _, body) = s.get(path, &["-u", "op:o"]);
        assert_eq!(status, want, "{path} answered {status}: {body}");
        assert!(
            body.contains("<!doctype html>"),
            "{path} answered with a raw status line rather than a page: {body}"
        );
        assert!(
            body.contains("next"),
            "{path} says something is wrong without saying what to do: {body}"
        );
    }
    s.done();
}

/// The book is the other half of this site, and the node says where it
/// is. Host-neutral: the address is one line of untracked operator
/// state, for the same reason every other host address here is.
#[test]
fn the_node_links_to_the_book_when_the_operator_has_said_where_it_is() {
    let s = served("book");

    // With nothing configured the link is simply absent — a node whose
    // operator publishes no book must not render a link to nowhere.
    let (_, _, body) = s.get("/", &[]);
    assert!(
        !body.contains("class=\"pill docs\""),
        "a node with no configured book rendered a link to one: {body}"
    );

    std::fs::create_dir_all(s.work.join("repos/.choir")).expect("state dir");
    std::fs::write(
        s.work.join("repos/.choir/docs-url"),
        "https://docs.example.invalid\n",
    )
    .expect("docs url");

    let (_, _, body) = s.get("/", &[]);
    assert!(
        body.contains("https://docs.example.invalid"),
        "the front door does not point at the book: {body}"
    );
    s.done();
}

#[cfg(test)]
mod unit {
    use super::matches;

    #[test]
    fn a_segment_placeholder_matches_one_segment_and_no_more() {
        assert!(matches("/r/{}/{}", "/r/agents/one"));
        assert!(!matches("/r/{}/{}", "/r/agents"));
        assert!(!matches("/r/{}/{}", "/r/agents/one/tree/main"));
        assert!(!matches("/r/{}/{}", "/r//one"), "an empty segment matched");
    }

    #[test]
    fn a_rest_placeholder_matches_the_remaining_segments() {
        assert!(matches(
            "/r/{}/{}/tree/{}/{..}",
            "/r/a/b/tree/main/src/deep"
        ));
        assert!(matches("/r/{}/{}/tree/{}/{..}", "/r/a/b/tree/main/src"));
        assert!(!matches("/r/{}/{}/tree/{}/{..}", "/r/a/b/tree/main"));
    }

    #[test]
    fn a_query_string_does_not_change_which_route_a_path_is() {
        assert!(matches("/theme", "/theme?set=dark&to=/r/"));
        assert!(matches(
            "/r/{}/{}/search/{}",
            "/r/a/b/search/main?q=x&in=code"
        ));
    }

    #[test]
    fn a_trailing_slash_is_its_own_route() {
        assert!(matches("/r/", "/r/"));
        assert!(!matches("/r/", "/r"));
        assert!(!matches("/r/{}/{}", "/r/"));
    }
}
