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
    assert_eq!(status, 401, "the browser surface served an anonymous reader");
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
    let (url, _) = served_node();

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
    let (url, node_key) = served_node();

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
    let submit = format!("{}api/submit", url);
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

    let (status, headers, body) = get(&format!("http://127.0.0.1:{port}/"), &["-u", "u:t"]);
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
    assert!(body.contains("platform_disabled"), "no code to quote: {body}");
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
    let (_, _, hostile) = get(&format!("{url}<img src=x onerror=alert(1)>"), &["-u", "u:t"]);
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
