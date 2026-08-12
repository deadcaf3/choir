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
