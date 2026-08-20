//! The browser surface, driven the way a browser drives it.
//!
//! The unit tests beside the renderer cover escaping, cache identity
//! and the stylesheet rules. What only a real node can show is the
//! part a reader actually depends on: that the page sits behind the
//! same auth wall as everything else, that a second visit costs no
//! bytes, and that the page moves when the log moves.

use choir_identity::{ActorKey, Registry};
use choir_node::{AuthTable, Node, Platform};
use choir_oplog::MemLog;
use choir_view::{OpKind, ViewOp};

use crate::support::submit_body;

/// Runs one request and returns `(status, headers, body)`.
///
/// `support::curl` parses JSON, which this surface is deliberately not,
/// and headers are the whole point of two of these tests — so this
/// module keeps its own header-preserving variant rather than widening
/// the shared helper for one caller.
fn get(url: &str, args: &[&str]) -> (u16, String, String) {
    let out = std::process::Command::new("curl")
        .args(["-s", "-i", "-w", "\n%{http_code}"])
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

/// Finds one header's value, case-insensitively as HTTP requires.
fn header_value(headers: &str, name: &str) -> Option<String> {
    headers.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        (k.trim().eq_ignore_ascii_case(name)).then(|| v.trim().to_string())
    })
}

/// A served node with auth on and the platform enabled, plus its base
/// URL. The temp dir is derived from the port, which is unique per
/// test in a shared-process harness where pids are not.
fn served_node() -> (String, ActorKey) {
    let node_key = ActorKey::generate();
    let platform = Platform::start(
        Registry::new(),
        Box::new(MemLog::new()),
        ActorKey::from_secret_bytes(&node_key.secret_bytes()),
    )
    .expect("platform starts");

    let mut table = AuthTable::new();
    table.insert("u".into(), "t".into());

    let root = std::env::temp_dir().join("choir-node-ui");
    std::fs::create_dir_all(&root).expect("temp root");
    let mut node = Node::bind_with_auth(&root, 0, Some(table)).expect("node binds on a free port");
    let port = node.port();
    node.enable_platform(platform);
    std::thread::spawn(move || node.serve_forever());

    (format!("http://127.0.0.1:{port}/"), node_key)
}

/// The page is not a hole in the auth wall. This is the property the
/// public deployment rests on: the anonymous internet gets nothing,
/// and "nothing" has to include the human-readable surface.
#[test]
fn the_page_is_behind_the_same_auth_wall_as_everything_else() {
    let (url, _) = served_node();

    let (status, headers, body) = get(&url, &[]);
    assert_eq!(
        status, 401,
        "the browser surface served an anonymous reader"
    );
    assert!(!body.contains("<!doctype html>"), "a 401 leaked the page");
    assert!(
        header_value(&headers, "WWW-Authenticate").is_some(),
        "no challenge header, so a browser would never prompt"
    );

    let (status, _, body) = get(&url, &["-u", "u:t"]);
    assert_eq!(status, 200);
    assert!(body.starts_with("<!doctype html>"), "not an HTML page");
}

/// A repeat visit transfers no body. An operator refreshing the page,
/// or a wall display polling it, costs the node nothing while the
/// state is unchanged — the "never lags" claim, as an assertion.
#[test]
fn an_unchanged_node_answers_a_repeat_visit_with_no_body() {
    let (base, _) = served_node();
    // Node telemetry moved to `/status` when `/` became the repository
    // index; this module is about that page, not the front door.
    let url = format!("{base}status");

    let (status, headers, _) = get(&url, &["-u", "u:t"]);
    assert_eq!(status, 200);
    let tag = header_value(&headers, "ETag").expect("the page must carry an ETag");

    let (status, _, body) = get(&url, &["-u", "u:t", "-H", &format!("If-None-Match: {tag}")]);
    assert_eq!(status, 304, "unchanged state re-sent the whole page");
    assert!(body.is_empty(), "a 304 carried {} bytes", body.len());
}

/// The page follows the log: a ref that reaches the view appears, and
/// the validator stops matching. Both halves matter — a page that
/// updates but keeps its `ETag` strands every browser holding it.
#[test]
fn a_new_op_changes_both_the_page_and_its_etag() {
    let (base, node_key) = served_node();
    let url = format!("{base}status");

    let (_, headers, before) = get(&url, &["-u", "u:t"]);
    let tag_before = header_value(&headers, "ETag").expect("etag");
    assert!(
        before.contains("No refs yet"),
        "an empty node should say so plainly"
    );

    let op = ViewOp::new(OpKind::SetRef {
        name: "o/r.git:refs/heads/main".into(),
        commit: choir_oplog::ContentHash::blake3(b"ui test commit"),
        prev: None,
    });
    let body = submit_body(&node_key, "node/test", &op);
    let submit = format!("{base}api/submit");
    let (status, response) = crate::support::curl(&["-u", "u:t", "-d", &body, &submit]);
    assert_eq!(
        status, 200,
        "the op was not accepted, so this test would prove nothing: {response}"
    );

    let (_, headers, after) = get(&url, &["-u", "u:t"]);
    let tag_after = header_value(&headers, "ETag").expect("etag");
    assert_ne!(tag_before, tag_after, "the ETag did not move with the log");
    assert!(
        after.contains("refs/heads/main"),
        "the new ref never reached the page"
    );
    assert!(
        !after.contains("No refs yet"),
        "the page still claims to be empty"
    );

    let (status, _, _) = get(
        &url,
        &["-u", "u:t", "-H", &format!("If-None-Match: {tag_before}")],
    );
    assert_eq!(status, 200, "a stale validator was answered with 304");
}

/// The response tells the browser to run nothing and fetch nothing.
/// The escaper is the real defence; this is the layer that holds if
/// the escaper ever misses something.
#[test]
fn the_response_forbids_scripts_and_remote_loads() {
    let (url, _) = served_node();
    let (status, headers, _) = get(&url, &["-u", "u:t"]);
    assert_eq!(status, 200);

    let csp = header_value(&headers, "Content-Security-Policy").expect("a CSP must be sent");
    assert!(
        csp.contains("default-src 'none'"),
        "CSP is too permissive: {csp}"
    );
    assert!(
        csp.contains("frame-ancestors 'none'"),
        "the page may be framed: {csp}"
    );
    assert_eq!(
        header_value(&headers, "X-Content-Type-Options").as_deref(),
        Some("nosniff")
    );
    assert_eq!(
        header_value(&headers, "Content-Type").as_deref(),
        Some("text/html; charset=utf-8")
    );
}

/// A node started without a sequencer is a supported configuration —
/// it serves git and nothing else — and its front door is the first
/// thing anyone opens. It used to answer one line of plain text, which
/// reads as a broken node rather than as a deliberate one.
///
/// The assertion is on the words a person reads, not on the `503`: the
/// status was already right and told nobody anything.
#[test]
fn a_node_with_no_platform_explains_itself_rather_than_looking_broken() {
    let root = std::env::temp_dir().join("choir-node-ui-headless");
    std::fs::create_dir_all(&root).expect("temp root");
    let mut table = AuthTable::new();
    table.insert("u".into(), "t".into());
    let node = Node::bind_with_auth(&root, 0, Some(table)).expect("node binds on a free port");
    let port = node.port();
    std::thread::spawn(move || node.serve_forever());

    let (status, headers, body) = get(&format!("http://127.0.0.1:{port}/status"), &["-u", "u:t"]);
    assert_eq!(status, 503);
    assert_eq!(
        header_value(&headers, "Content-Type").as_deref(),
        Some("text/html; charset=utf-8"),
        "a browser was answered in a format it does not render"
    );
    // What happened, in words about the reader's situation.
    assert!(
        body.contains("platform API"),
        "the page does not say what is switched off: {body}"
    );
    // What is still true — the half a reader would otherwise assume is
    // broken as well.
    assert!(
        body.contains("cloning and pushing work"),
        "the page does not say what still works: {body}"
    );
    // The one next action, and somewhere to go.
    assert!(body.contains("next"), "no next action: {body}");
    assert!(
        body.contains("href=\"/r/\""),
        "a reader met a wall and was given nowhere to go: {body}"
    );
    // The machine-readable reason, for quoting to an operator.
    assert!(
        body.contains("platform_disabled"),
        "no code to quote: {body}"
    );
}

/// A mistyped address used to fall through to `git http-backend`, whose
/// CGI 404 says "Not Found" and nothing about where the reader is. Two
/// nodes, two configurations, and the answer must be the same page:
/// before this, an ACL node said "no such repository" and a node without
/// one said whatever git said.
#[test]
fn a_mistyped_address_names_the_addresses_that_do_work() {
    let (url, _) = served_node();
    let (status, headers, body) = get(&format!("{url}not-a-real-page"), &["-u", "u:t"]);

    assert_eq!(status, 404);
    assert_eq!(
        header_value(&headers, "Content-Type").as_deref(),
        Some("text/html; charset=utf-8")
    );
    assert!(body.contains("no_such_page"), "no code to quote: {body}");
    assert!(
        body.contains("href=\"/\"") && body.contains("href=\"/r/\""),
        "the page names no address that does work: {body}"
    );
    // It says the path back rather than only that this one failed.
    assert!(
        body.contains("ends in .git"),
        "a reader who mistyped a clone URL is not told how one differs: {body}"
    );
    // ...and it echoes what was actually asked for, escaped.
    let (_, _, hostile) = get(
        &format!("{url}<img src=x onerror=alert(1)>"),
        &["-u", "u:t"],
    );
    assert!(
        !hostile.contains("<img src=x"),
        "the echoed path reached the page as markup: {hostile}"
    );
}

/// A refusal is told in the surface the caller is already in. The same
/// limit, hit from a browser and from an agent, has to produce a page
/// and JSON respectively — and both have to carry `Retry-After`, because
/// that is the part a machine acts on and a person is told in words.
#[test]
fn a_rate_limited_reader_gets_a_page_and_an_agent_still_gets_json() {
    let platform = Platform::start(
        Registry::new(),
        Box::new(MemLog::new()),
        ActorKey::generate(),
    )
    .expect("platform starts");
    let mut table = AuthTable::new();
    table.insert("u".into(), "t".into());
    let root = std::env::temp_dir().join("choir-node-ui-ratelimit");
    std::fs::create_dir_all(&root).expect("temp root");
    let mut node = Node::bind_with_auth(&root, 0, Some(table)).expect("node binds on a free port");
    let port = node.port();
    // One request per minute per class, so the second of anything is over.
    node.enable_rate_limit(std::num::NonZeroU32::new(1), None);
    node.enable_platform(platform);
    std::thread::spawn(move || node.serve_forever());
    let base = format!("http://127.0.0.1:{port}");

    let (first, _, _) = get(&format!("{base}/"), &["-u", "u:t"]);
    assert_eq!(first, 200, "the first request was already refused");
    let (status, headers, body) = get(&format!("{base}/"), &["-u", "u:t"]);
    assert_eq!(status, 429, "the second request was not refused");
    assert_eq!(
        header_value(&headers, "Content-Type").as_deref(),
        Some("text/html; charset=utf-8"),
        "a browser was handed the agent's body"
    );
    assert!(
        header_value(&headers, "Retry-After").is_some(),
        "the page dropped the header a client acts on"
    );
    assert!(body.contains("rate_limited"), "no code to quote: {body}");
    assert!(
        body.contains("Nothing is wrong with the node"),
        "a reader is left thinking they broke something: {body}"
    );

    // The agent's surface is untouched, which is the half that would
    // break every client if this were negotiated on `Accept` instead.
    let (status, headers, body) = get(&format!("{base}/api/view"), &["-u", "u:t"]);
    assert_eq!(status, 429);
    assert_eq!(
        header_value(&headers, "Content-Type").as_deref(),
        Some("application/json"),
        "an agent was handed a web page"
    );
    assert!(
        body.contains("retry_after_secs"),
        "the JSON refusal lost its machine-readable field: {body}"
    );
}

/// Concurrent readers are served correctly and never see a broken
/// page. The cache is shared mutable state on the request path, so
/// "several browsers at once" is a case worth holding down.
#[test]
fn concurrent_readers_all_get_a_whole_page() {
    let (url, _) = served_node();
    let _ = get(&url, &["-u", "u:t"]);

    let readers: Vec<_> = (0..4)
        .map(|_| {
            let url = url.clone();
            std::thread::spawn(move || {
                for _ in 0..5 {
                    let (status, _, body) = get(&url, &["-u", "u:t"]);
                    assert_eq!(status, 200, "a concurrent reader was refused");
                    assert!(body.starts_with("<!doctype html>"), "torn page");
                    assert!(body.ends_with("</html>"), "truncated page");
                }
            })
        })
        .collect();
    for r in readers {
        r.join().expect("a reader thread panicked");
    }
}

/// A query string does not un-serve the node state page.
///
/// The dispatcher matched `request.url()` against `"/status"` whole, so
/// this one page — alone on the surface — answered 404 to any address
/// carrying a `?`: a cache-buster, a tracking parameter a mail client
/// appended to a shared link, anything. Every other route splits the
/// query off first, including this file's own `is_browser_path` helper,
/// which is what made the outlier hard to see.
///
/// The refusal was also a convincing one. It says "nothing is served at
/// that address" and prints the address, which is exactly the page a
/// reader gets for a genuine typo, so the report would have arrived as
/// "your link is broken" rather than as a routing bug.
#[test]
fn the_node_state_page_survives_a_query_string() {
    let (base, _key) = served_node();
    let plain = format!("{base}status");
    let (status, _, body) = get(&plain, &["-u", "u:t"]);
    assert_eq!(status, 200, "the bare address stopped working");
    assert!(body.contains("Refs"), "not the state page: {body}");

    for query in ["?v=2", "?utm_source=mail", "?a=1&b=2"] {
        let (status, _, body) = get(&format!("{plain}{query}"), &["-u", "u:t"]);
        assert_eq!(status, 200, "`/status{query}` was refused");
        assert!(
            body.contains("Refs"),
            "`/status{query}` served something other than the state page: {body}"
        );
    }
}

/// The palette a reader chooses is theirs, and choosing it cannot be
/// turned into a way of sending them somewhere else.
///
/// The stylesheet has carried `:root[data-theme="light"]` and its dark
/// twin since it was written and nothing ever set the attribute, so the
/// override was decoration: a reader whose system said light read a
/// light page and had no way to say otherwise.
///
/// The `to=` parameter is the part that needs the test. A redirector
/// that checks only "starts with a slash" is an open redirect, because
/// a browser reads `//evil.test` as another host — so the refusals are
/// asserted by name rather than trusted to a reading of the code.
#[test]
fn a_reader_can_choose_a_palette_and_cannot_be_redirected_off_the_node() {
    let (base, _key) = served_node();
    let set = |query: &str| {
        let out = std::process::Command::new("curl")
            .args([
                "-s",
                "-i",
                "-o",
                "/dev/null",
                "-w",
                "%{http_code} %{redirect_url}",
            ])
            .args(["-u", "u:t"])
            .arg(format!("{base}theme{query}"))
            .output()
            .expect("curl runs");
        String::from_utf8_lossy(&out.stdout).to_string()
    };

    // Setting one sends the reader back to the page they were reading.
    let answer = set("?set=dark&to=/status");
    assert!(answer.starts_with("303 "), "not a redirect: {answer}");
    assert!(answer.ends_with("/status"), "wrong destination: {answer}");

    // ...and the page then renders in it, which is the whole point.
    let (status, _, body) = get(
        &format!("{base}status"),
        &["-u", "u:t", "-H", "Cookie: theme=dark"],
    );
    assert_eq!(status, 200);
    assert!(
        body.contains(r#"<html lang="en" data-theme="dark""#),
        "the page ignored the cookie: {body}"
    );
    let (_, _, light) = get(
        &format!("{base}status"),
        &["-u", "u:t", "-H", "Cookie: theme=light"],
    );
    assert!(light.contains(r#"data-theme="light""#), "{light}");

    // No cookie is not a third palette: it is the absence of the
    // attribute, which is what leaves `prefers-color-scheme` in charge.
    // Asserted on the opening tag, not on the document: the stylesheet
    // names `data-theme` in its own rules, so a document-wide `contains`
    // is true on every page and would pass whatever the tag said.
    let (_, _, plain) = get(&format!("{base}status"), &["-u", "u:t"]);
    assert!(
        plain.contains(r#"<html lang="en">"#),
        "a reader who chose nothing was given a palette anyway: {}",
        &plain[..plain.len().min(120)]
    );

    // A value this node did not write is not a palette either. The
    // cookie is never trusted for anything but which colours to paint,
    // and this is what keeps it from reaching the attribute unchecked.
    let (_, _, forged) = get(
        &format!("{base}status"),
        &[
            "-u",
            "u:t",
            "-H",
            r#"Cookie: theme="><script>alert(1)</script>"#,
        ],
    );
    assert!(!forged.contains("<script>alert"), "{forged}");
    assert!(
        forged.contains(r#"<html lang="en">"#),
        "a cookie this node never wrote reached the attribute: {}",
        &forged[..forged.len().min(120)]
    );

    // The open-redirect cases, each by name.
    for hostile in [
        "//evil.test/",
        "https://evil.test/",
        "/\\evil.test",
        "evil.test",
    ] {
        let answer = set(&format!("?set=dark&to={hostile}"));
        assert!(
            answer.starts_with("303 "),
            "{hostile} was not answered at all: {answer}"
        );
        assert!(
            !answer.contains("evil.test"),
            "`to={hostile}` sent the reader off this node: {answer}"
        );
    }

    // Clearing is a real state, not a missing one: it is how a reader
    // goes back to following their system.
    let answer = set("?set=auto&to=/status");
    assert!(answer.starts_with("303 "), "{answer}");
    let (_, headers, _) = get(&format!("{base}theme?set=auto&to=/status"), &["-u", "u:t"]);
    assert!(
        headers.contains("Max-Age=0"),
        "auto did not clear the cookie: {headers}"
    );
}

/// Two readers holding two palettes are not served each other's page.
///
/// The state page is memoized per reader and revalidated with an
/// `ETag`; both were keyed on things that do not include the palette,
/// so the first visitor's colours would have been handed to the next
/// one and a reader who switched would have been handed their own old
/// page back by their browser.
#[test]
fn the_palette_is_part_of_a_page_s_cache_identity() {
    let (base, _key) = served_node();
    let tag = |cookie: &str| {
        let (_, headers, _) = get(
            &format!("{base}status"),
            &["-u", "u:t", "-H", &format!("Cookie: theme={cookie}")],
        );
        header_value(&headers, "ETag").expect("the state page carries an ETag")
    };
    let dark = tag("dark");
    let light = tag("light");
    assert_ne!(dark, light, "two palettes of one page share a cache entry");

    // And the body follows the tag rather than the other way around.
    let (_, _, dark_body) = get(
        &format!("{base}status"),
        &["-u", "u:t", "-H", "Cookie: theme=dark"],
    );
    assert!(dark_body.contains(r#"data-theme="dark""#), "{dark_body}");

    // A conditional request with the wrong palette's tag is not a hit.
    let (status, _, _) = get(
        &format!("{base}status"),
        &[
            "-u",
            "u:t",
            "-H",
            "Cookie: theme=light",
            "-H",
            &format!("If-None-Match: {dark}"),
        ],
    );
    assert_eq!(
        status, 200,
        "a light reader was answered 304 for a dark page"
    );
}

/// ...and the same cookie over plain HTTP is *not* marked `Secure`.
///
/// The other half of the D56 flag, asserted separately because it is
/// the half that fails silently: a `Secure` cookie on a plain-HTTP node
/// is dropped by the browser, so the palette control would render, be
/// clickable, answer `303` — and never once stick. Nothing in the
/// response would say why.
#[test]
fn a_plain_http_node_does_not_mark_the_palette_cookie_secure() {
    let (base, _key) = served_node();
    let (_, headers, _) = get(&format!("{base}theme?set=dark&to=/"), &["-u", "u:t"]);
    let cookie = headers
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("set-cookie:"))
        .unwrap_or_else(|| panic!("no cookie was set: {headers}"));
    assert!(
        !cookie.contains("Secure"),
        "a plain-http node set a cookie the browser will drop: {cookie}"
    );
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    assert!(cookie.contains("SameSite=Lax"), "{cookie}");
}
