//! A repository the ACL publishes, read by somebody with no credential.
//!
//! The node's wall is D74's: every browse route is behind the auth gate,
//! and a stranger meets the sign-in page. That is right for a private
//! beta and wrong for source that is already public elsewhere, and the
//! gap it left was total -- a node serving an open-source project could
//! show a reader nothing at all without also issuing them an account.
//!
//! What closes it is one reserved principal rather than a second
//! authorization rule. `@anon` is a name no account can be spelled as,
//! granted `read` on repositories somebody named, and an unauthenticated
//! request becomes that principal instead of a `401`. Every check after
//! the gate is the one that was already there.
//!
//! So the tests that matter here are the ones about what *did not*
//! change: a repository nobody published, the op log, and the push half
//! of git are each still refused, and refused for the same reason as
//! before rather than by a new rule that happens to agree today.

use choir_identity::{ActorKey, Registry};
use choir_node::{AuthTable, Node, Platform};
use choir_oplog::MemLog;

struct Served {
    base: String,
    work: std::path::PathBuf,
}

/// The body of a `curl` with no credentials anywhere.
fn anon_body(url: &str) -> String {
    let out = std::process::Command::new("curl")
        .args(["-s"])
        .arg(url)
        .output()
        .expect("curl runs");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The status of a `curl` with no credentials anywhere.
fn anon(url: &str) -> u16 {
    let out = std::process::Command::new("curl")
        .args(["-s", "-o", "/dev/null", "-w", "%{http_code}"])
        .arg(url)
        .output()
        .expect("curl runs");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("numeric status")
}

fn git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        // No credential helper and no terminal: a route that asks for a
        // username fails here rather than hanging, which is what makes
        // "the push was refused" an assertion instead of a timeout.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git runs")
}

/// A node holding one published repository and one that is not, with a
/// commit in each so a clone has something to fetch.
fn served(tag: &str, acl: &str) -> Served {
    served_presenting(tag, acl, None)
}

/// The same node, optionally narrowed to one repository as its site
/// (D78's `--site-repo`), which is what turns `/` into that
/// repository's tree rather than an index.
fn served_presenting(tag: &str, acl: &str, site: Option<&str>) -> Served {
    let work = std::env::temp_dir().join(format!("choir-node-public-read-{tag}"));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");
    let acl_path = work.join("acl");

    let platform = Platform::start(
        Registry::new(),
        Box::new(MemLog::new()),
        ActorKey::generate(),
    )
    .expect("platform starts");

    let mut table = AuthTable::new();
    table.insert("alice".into(), "a".into());

    let mut node =
        Node::bind_with_auth(&work.join("repos"), 0, Some(table)).expect("node binds free port");
    let port = node.port();
    node.create_repo("open/source.git").expect("repo created");
    node.create_repo("closed/thing.git").expect("repo created");
    std::fs::write(&acl_path, acl).expect("acl file");
    node.watch_acl_file(acl_path).expect("acl loads");
    if let Some(site) = site {
        node.serve_single_repository(site)
            .expect("a repository this node can present");
    }
    node.enable_platform(platform);
    std::thread::spawn(move || node.serve_forever());

    let base = format!("http://127.0.0.1:{port}");
    for repo in ["open/source.git", "closed/thing.git"] {
        let seed = work.join(format!("seed-{}", repo.replace('/', "-")));
        std::fs::create_dir_all(&seed).expect("seed dir");
        assert!(git(&seed, &["init", "-q", "."]).status.success());
        std::fs::write(seed.join("f.txt"), "content\n").expect("file");
        git(&seed, &["config", "user.email", "a@b.invalid"]);
        git(&seed, &["config", "user.name", "a"]);
        git(&seed, &["add", "f.txt"]);
        git(&seed, &["commit", "-q", "-m", "first"]);
        let url = format!("http://alice:a@127.0.0.1:{port}/{repo}");
        assert!(
            git(&seed, &["push", "-q", &url, "HEAD:refs/heads/main"])
                .status
                .success(),
            "seeding {repo} failed"
        );
    }
    Served { base, work }
}

/// The published repository, and only it, answers a reader with nothing.
#[test]
fn a_published_repository_answers_a_reader_who_has_no_account() {
    let s = served("browse", "@anon\topen/source.git\tread\nalice\t*\twrite\n");

    assert_eq!(
        anon(&format!("{}/r/open/source", s.base)),
        200,
        "a published repository still met the wall"
    );
    // Not `401`: the refusal a reader without a grant receives is the
    // one a reader of a repository that does not exist receives, and
    // keeping them identical is what stops the wall confirming which
    // private repositories are here.
    assert_eq!(
        anon(&format!("{}/r/closed/thing", s.base)),
        404,
        "an unpublished repository was visible, or answered differently \
         from one that does not exist"
    );
    assert_eq!(
        anon(&format!("{}/r/open/nosuch", s.base)),
        404,
        "the refusals stopped agreeing"
    );
}

/// Publishing a repository publishes the repository, not the node.
#[test]
fn the_op_log_and_the_node_scope_stay_behind_the_wall() {
    let s = served("scope", "@anon\topen/source.git\tread\nalice\t*\twrite\n");

    // `/api/view` and `/api/log` are `GET`s that serve the op log. A
    // gate that let anonymous *safe methods* through would have
    // published both, which is why the route test is an allowlist.
    for path in ["/api/view", "/api/log", "/api/repos"] {
        assert_eq!(
            anon(&format!("{}{path}", s.base)),
            401,
            "{path} answered a caller with no credential"
        );
    }
    let _ = &s.work;
}

/// The read half of git smart-HTTP opens; the write half does not.
#[test]
fn an_anonymous_clone_works_and_an_anonymous_push_does_not() {
    let s = served("git", "@anon\topen/source.git\tread\nalice\t*\twrite\n");

    let clone = s.work.join("clone");
    let url = format!("{}/open/source.git", s.base);
    let out = git(
        &s.work,
        &["clone", "-q", &url, clone.to_str().expect("utf-8 path")],
    );
    assert!(
        out.status.success(),
        "anonymous clone failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(clone.join("f.txt").exists(), "the clone fetched no tree");

    // The push half is `Level::Propose`, which `@anon` cannot parse its
    // way into holding, so the node challenges and git — with no
    // terminal and no helper — fails rather than becoming somebody.
    std::fs::write(clone.join("g.txt"), "mine\n").expect("file");
    git(&clone, &["config", "user.email", "m@b.invalid"]);
    git(&clone, &["config", "user.name", "m"]);
    git(&clone, &["add", "g.txt"]);
    git(&clone, &["commit", "-q", "-m", "second"]);
    let pushed = git(&clone, &["push", "-q", "origin", "main"]);
    assert!(
        !pushed.status.success(),
        "an anonymous push was accepted into a published repository"
    );

    // The unpublished repository is not fetchable either, by the same
    // refusal the browse surface gives.
    let denied = git(
        &s.work,
        &[
            "clone",
            "-q",
            &format!("{}/closed/thing.git", s.base),
            s.work.join("denied").to_str().expect("utf-8 path"),
        ],
    );
    assert!(
        !denied.status.success(),
        "an unpublished repository was cloneable"
    );
}

/// A node that never names the principal is the node it was before.
#[test]
fn a_table_that_does_not_publish_anything_still_meets_every_stranger_with_the_wall() {
    let s = served("closed", "alice\t*\twrite\n");

    for path in ["/r/open/source", "/r/closed/thing"] {
        assert_eq!(
            anon(&format!("{}{path}", s.base)),
            401,
            "{path} opened without anybody publishing it"
        );
    }
    let fetched = anon(&format!(
        "{}/open/source.git/info/refs?service=git-upload-pack",
        s.base
    ));
    assert_eq!(fetched, 401, "git opened without anybody publishing it");
}

/// The front door *is* the published source, not a page pointing at it.
///
/// D78 made publishing the ACL's answer; this is the front door giving
/// the same answer. A node serving source a stranger may read has
/// something better to show them than a page saying what choir is, so
/// `/` renders what `/r/` renders instead of a landing page whose whole
/// purpose was to offer a link to it.
#[test]
fn the_front_door_is_the_published_source_for_a_reader_with_no_account() {
    let s = served("door", "@anon\topen/source.git\tread\nalice\t*\twrite\n");

    assert_eq!(
        anon(&s.base),
        200,
        "the front door refused a reader this node publishes to"
    );
    let front = anon_body(&s.base);
    assert!(
        front.contains("open/source"),
        "the front door is not the index of what this node publishes: {front}"
    );
    assert!(
        !front.contains("closed/thing"),
        "the front door listed a repository nobody published: {front}"
    );
    // The landing page is gone from this node, not merely outranked: a
    // reader who has been shown the source has no use for a page asking
    // whether they would like some.
    assert!(
        !front.contains("class=\"hero-sub\""),
        "the landing page still answered a node that publishes: {front}"
    );
}

/// A node presenting one repository opens on that repository's files.
///
/// This is the shape the front door was asked for: the bare host name
/// answers with the source, the way a forge's project page does, rather
/// than with a list of one thing or a page about the software serving
/// it. Both halves of D78 have to hold at once for it -- the ACL
/// publishes the repository and `--site-repo` presents it -- and neither
/// alone gets a stranger to a file name.
#[test]
fn a_node_presenting_one_published_repository_opens_on_its_source() {
    let s = served_presenting(
        "site",
        "@anon\topen/source.git\tread\nalice\t*\twrite\n",
        Some("open/source"),
    );

    assert_eq!(anon(&s.base), 200, "the front door refused a stranger");
    let front = anon_body(&s.base);
    assert!(
        front.contains("f.txt"),
        "the front door is not the repository's tree: {front}"
    );
    assert!(
        !front.contains(">repositories</h1>"),
        "the front door is still an index: {front}"
    );
    assert!(
        !front.contains("class=\"hero-sub\""),
        "the landing page still answered a node that publishes: {front}"
    );
}

/// A node that publishes nothing still meets a stranger with the
/// landing page.
///
/// The landing page is what a node shows when it has nothing to show,
/// and that is the whole of its remaining job. Losing this direction
/// would turn every private node's front door into a `401`, which is
/// the thing D57 existed to replace.
#[test]
fn a_node_that_publishes_nothing_still_opens_on_the_landing_page() {
    let s = served("shut", "alice\t*\twrite\n");

    assert_eq!(
        anon(&s.base),
        200,
        "the front door met a stranger with a wall"
    );
    let front = anon_body(&s.base);
    assert!(
        front.contains("class=\"hero-sub\""),
        "a node with nothing to show dropped the page that says so: {front}"
    );
}

/// Presenting a repository is not publishing it.
///
/// `--site-repo` is presentation and the ACL is the grant, so a node
/// that presents one repository while publishing a different one has
/// published nothing a stranger typing the bare host name can reach.
/// The front door has to read the grant for the repository it would
/// actually render, not the table in general -- otherwise this node
/// answers a stranger with the refusal a denied reader gets, in place
/// of the page that would have told them where they are.
#[test]
fn presenting_a_repository_nobody_published_keeps_the_landing_page() {
    let s = served_presenting(
        "mismatch",
        "@anon\topen/source.git\tread\nalice\t*\twrite\n",
        Some("closed/thing"),
    );

    let front = anon_body(&s.base);
    assert!(
        front.contains("class=\"hero-sub\""),
        "the front door rendered something other than the landing page \
         for a repository this node has not published: {front}"
    );
}

/// Every link the bar offers a stranger is one the stranger can follow.
///
/// The bar has two shapes and `identified` picks between them, but D78
/// spells its principal `@anon` while that test only knew `anon`, so a
/// stranger was handed the signed-in shape: no way in, and links to
/// `/reviews`, `/account` and `/status`, each of which answered with the
/// sign-in page. The palette was the worst of them, because it is on
/// every page, nothing about it is private, and clicking `dark` is the
/// most ordinary thing a reader does.
///
/// Asserted by following the links rather than by naming them, so a link
/// added to the bar later is covered without anybody remembering to add
/// it here.
#[test]
fn the_bar_offers_a_stranger_no_link_they_cannot_follow() {
    let s = served("bar", "@anon\topen/source.git\tread\nalice\t*\twrite\n");
    let front = anon_body(&s.base);

    let nav = front
        .split_once("<nav class=\"chrome-nav\">")
        .and_then(|(_, rest)| rest.split_once("</nav>"))
        .map(|(nav, _)| nav.to_string())
        .expect("the front door draws a bar");

    let mut followed = 0;
    for piece in nav.split("href=\"").skip(1) {
        let href = piece.split('"').next().expect("a quoted href");
        // The book is a different origin and a different server (D76).
        if href.starts_with("http") {
            continue;
        }
        let href = href.replace("&amp;", "&");
        let target = format!("{}{href}", s.base);
        let status = anon(&target);
        assert!(
            status < 400,
            "the bar offers a stranger {href}, which answers {status}"
        );
        followed += 1;
    }
    assert!(followed >= 2, "the bar drew almost nothing: {nav}");

    // And it does *not* offer the way in, which is the reverse of what
    // this test asserted when it was written.
    //
    // The old assertion was right for the node it was written against:
    // one where a reader with no credential was somebody who had not
    // signed in yet, so the way in was the one thing to offer them. D78
    // made that reader the ordinary case instead -- a stranger reading
    // published source -- and this node issues them no account, so the
    // most prominent control in the bar led to a form that would refuse
    // them. The link came out; the route did not.
    assert!(
        !nav.contains("href=\"/signin\""),
        "the bar still sends a stranger to a form that has nothing for \
         them: {nav}"
    );
    // The way in is still there for whoever types it, which is what
    // makes this a rendering change rather than a removal. D74's `401`
    // is the deliberate status, so the assertion is on the form.
    assert!(
        anon_body(&format!("{}/signin", s.base)).contains("action=\"/signin\""),
        "typing /signin no longer reaches a sign-in form"
    );
}

/// A link to published source, pasted anywhere, renders as a card.
///
/// The invite page has had a preview since it existed; the browse
/// surface had none, which was the wrong way round once D76 put the
/// source at the apex and D78 made it readable -- the half a stranger
/// actually reaches was the half that rendered as a grey rectangle.
///
/// The image has to be absolute, because the server fetching it has no
/// page to resolve a relative path against, and the title has to come
/// from the repository rather than the page: `og:title` set from the
/// page title leaked a name out of `/p/<name>`, whose whole job is to be
/// indistinguishable from an unknown one.
#[test]
fn a_link_to_published_source_previews_as_a_card() {
    let s = served("card", "@anon\topen/source.git\tread\nalice\t*\twrite\n");

    let head = |path: &str| {
        let body = anon_body(&format!("{}{path}", s.base));
        body.split_once("</head>")
            .map(|(head, _)| head.to_string())
            .expect("a page with a head")
    };

    let repo = head("/r/open/source");
    assert!(
        repo.contains("property=\"og:image\" content=\"http://127.0.0.1:"),
        "the card has no absolute image: {repo}"
    );
    // The repository's own card, not the node's. That split arrived
    // later than this test did; `each_repository_has_a_card_of_its_own`
    // is where the drawing is asserted, and this is here so the two
    // cannot drift.
    assert!(
        repo.contains("/static/card/open/source.png"),
        "the image is not this repository's card: {repo}"
    );
    assert!(
        repo.contains("property=\"og:title\" content=\"open/source\""),
        "the card does not name the repository: {repo}"
    );
    // The image must actually be fetchable by a stranger's preview
    // server, which is a different question from it being named.
    assert_eq!(
        anon(&format!("{}/static/card.png", s.base)),
        200,
        "the card the preview points at is not reachable"
    );

    // A page about no repository carries the card and names none.
    let index = head("/r/");
    assert!(
        index.contains("property=\"og:image\""),
        "the index carries no card: {index}"
    );
    // A page about no repository falls back to the node's own card,
    // because there is no name to draw.
    assert!(
        index.contains("/static/card.png"),
        "the index did not fall back to the node's card: {index}"
    );
    assert!(
        !index.contains("open/source\">"),
        "a page about no repository named one in its card: {index}"
    );
}

/// The crawl policy follows the ACL, which is the table that decides
/// what is public in the first place (D78).
///
/// The old policy was static and said `Disallow: /`, justified in its
/// own comment by "every one of those paths already answers `401` to a
/// crawler". D78 made that false and nothing followed it: the node was
/// telling every search engine not to look at the one thing it had just
/// been changed to show them.
#[test]
fn the_crawl_policy_opens_exactly_what_the_table_published() {
    let s = served("robots", "@anon\topen/source.git\tread\nalice\t*\twrite\n");
    let policy = anon_body(&format!("{}/robots.txt", s.base));

    assert!(
        policy.contains("\nAllow: /r/open/source$\n"),
        "the published repository is not crawlable: {policy}"
    );
    assert!(
        policy.contains("\nAllow: /r/open/source/\n"),
        "the published repository's pages are not crawlable: {policy}"
    );
    // The unpublished one must not be named at all. A crawl policy that
    // listed it would disclose a private repository to every crawler on
    // the internet, which is a worse leak than the browse surface's,
    // because this file is fetched by things that keep it.
    assert!(
        !policy.contains("closed"),
        "the crawl policy named an unpublished repository: {policy}"
    );
    // And the blanket refusal stays under the specific permissions.
    // Longest match wins, so this is what keeps everything else shut.
    assert!(
        policy.contains("\nDisallow: /\n"),
        "the blanket disallow is gone: {policy}"
    );
    assert!(
        policy.contains(&format!("Sitemap: {}/sitemap.xml", s.base)),
        "the policy points at no sitemap: {policy}"
    );

    // A node that publishes nothing is unchanged, which is what stops
    // this inviting crawlers into a wall of 401s on the next node.
    let shut = served("robots-shut", "alice\t*\twrite\n");
    let policy = anon_body(&format!("{}/robots.txt", shut.base));
    assert!(
        !policy.contains("Allow: /r/"),
        "a node publishing nothing offered its repositories to crawlers: {policy}"
    );
}

/// The sitemap lists what the ACL published, and nothing else.
#[test]
fn the_sitemap_lists_the_published_repositories() {
    let s = served("sitemap", "@anon\topen/source.git\tread\nalice\t*\twrite\n");
    let map = anon_body(&format!("{}/sitemap.xml", s.base));

    assert!(map.starts_with("<?xml"), "not an XML document: {map}");
    assert!(
        map.contains(&format!("<loc>{}/r/open/source</loc>", s.base)),
        "the published repository is missing: {map}"
    );
    assert!(
        map.contains(&format!("<loc>{}/</loc>", s.base)),
        "the front door is missing: {map}"
    );
    assert!(
        !map.contains("closed"),
        "the sitemap named an unpublished repository: {map}"
    );
}

/// A repository page's preview image is drawn for that repository.
///
/// Every card being the same image made a channel full of links look
/// like one link repeated. The route draws from the name in the URL and
/// checks nothing, which is what keeps it from becoming a way to ask
/// whether a repository exists.
#[test]
fn each_repository_has_a_card_of_its_own() {
    let s = served("cards", "@anon\topen/source.git\tread\nalice\t*\twrite\n");

    let head = anon_body(&format!("{}/r/open/source", s.base));
    assert!(
        head.contains(&format!(
            "og:image\" content=\"{}/static/card/open/source.png\"",
            s.base
        )),
        "the page does not point at its own card: {head}"
    );

    let png = std::process::Command::new("curl")
        .args(["-s", "-o", "-"])
        .arg(format!("{}/static/card/open/source.png", s.base))
        .output()
        .expect("curl runs");
    assert_eq!(
        &png.stdout[..8],
        &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a],
        "the card is not a PNG"
    );

    // Two repositories, two different images. Identical bytes would mean
    // the name never reached the drawing.
    let other = std::process::Command::new("curl")
        .args(["-s", "-o", "-"])
        .arg(format!("{}/static/card/open/other.png", s.base))
        .output()
        .expect("curl runs");
    assert_ne!(
        png.stdout, other.stdout,
        "two repositories were given the same card"
    );

    // A name outside the grammar is refused rather than drawn.
    for bad in [
        "/static/card/open.png",
        "/static/card/a/b/c.png",
        "/static/card/open/source",
    ] {
        assert_eq!(anon(&format!("{}{bad}", s.base)), 404, "{bad} was drawn");
    }

    // And a crawler that honours the policy may fetch them.
    let policy = anon_body(&format!("{}/robots.txt", s.base));
    assert!(
        policy.contains("\nAllow: /static/card/\n"),
        "the cards are disallowed to the crawlers that render them: {policy}"
    );
}

/// The way in is in the footer, on every page, for a reader who is not
/// signed in and nobody else.
#[test]
fn the_way_in_is_quiet_but_present() {
    let s = served("wayin", "@anon\topen/source.git\tread\nalice\t*\twrite\n");
    let page = anon_body(&format!("{}/r/open/source", s.base));

    let (_, footer) = page.split_once("<footer>").expect("a footer");
    assert!(
        footer.contains("href=\"/signin\""),
        "an operator has no way in but to type the path: {footer}"
    );
    let (_, nav) = page
        .split_once("<nav class=\"chrome-nav\">")
        .expect("a bar");
    let (nav, _) = nav.split_once("</nav>").expect("a closed bar");
    assert!(
        !nav.contains("href=\"/signin\""),
        "the bar is selling the door again: {nav}"
    );
}
