//! Bounded aggregate responses over the wire (`internal/oak.md` item 4).
//!
//! The unit tests beside `bound.rs` cover the slicing arithmetic against
//! a hand-built document. What only a served node can show is the part
//! that made this a design decision rather than a helper: that the cap
//! runs *after* the ACL narrows, so an omission count never counts rows
//! the caller was refused. Cap-then-filter produces a document that looks
//! correct to every assertion about lengths and leaks the size of the
//! node to a reader granted one repository.

use choir_identity::{ActorKey, Registry};
use choir_node::{AuthTable, Node, Platform};
use choir_oplog::MemLog;
use choir_view::{OpKind, ViewOp};

use crate::support::{curl, submit_body};

/// A served node with an ACL, `alice`/`carol` credentials, and a key that
/// may write anywhere. `tag` keeps temp directories distinct: this
/// harness shares one process.
fn served(tag: &str, acl: &str, repos: &[&str]) -> (String, ActorKey) {
    let work = std::env::temp_dir().join(format!("choir-node-bounded-{tag}"));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");
    let acl_path = work.join("acl");
    std::fs::write(&acl_path, acl).expect("acl file");

    let node_key = ActorKey::generate();
    let platform = Platform::start(
        Registry::new(),
        Box::new(MemLog::new()),
        ActorKey::from_secret_bytes(&node_key.secret_bytes()),
    )
    .expect("platform starts");

    let mut table = AuthTable::new();
    for (user, token) in [("alice", "a"), ("carol", "c")] {
        table.insert(user.into(), token.into());
    }

    let mut node =
        Node::bind_with_auth(&work.join("repos"), 0, Some(table)).expect("node binds free port");
    let port = node.port();
    for repo in repos {
        node.create_repo(repo).expect("repo created");
    }
    if !acl.is_empty() {
        node.watch_acl_file(acl_path).expect("acl loads");
    }
    node.enable_platform(platform);
    std::thread::spawn(move || node.serve_forever());

    (format!("http://127.0.0.1:{port}"), node_key)
}

/// Lands `count` refs under `repo` as one batch, named so their sorted
/// order is stable and readable.
fn seed_refs(base: &str, key: &ActorKey, creds: &str, repo: &str, count: usize) {
    let ops: Vec<serde_json::Value> = (0..count)
        .map(|i| {
            let op = ViewOp::new(OpKind::SetRef {
                name: format!("{repo}:refs/heads/t{i:04}"),
                commit: choir_oplog::ContentHash::blake3(format!("{repo}{i}").as_bytes()),
                prev: None,
            });
            serde_json::from_str(&submit_body(key, "node/test", &op)).expect("op body")
        })
        .collect();
    let (status, body) = curl(&[
        "-u",
        creds,
        "-d",
        &serde_json::json!({ "ops": ops }).to_string(),
        &format!("{base}/api/submit-batch"),
    ]);
    assert_eq!(status, 200, "seeding batch refused: {body}");
}

/// The default is a budget, not a suggestion. A client that passes
/// nothing gets a bounded document and is told, in band, how much it did
/// not get and where the rest is.
#[test]
fn the_view_is_bounded_by_default_and_names_the_next_page() {
    let (base, key) = served("default", "carol * write\n", &["agents/one.git"]);
    seed_refs(&base, &key, "carol:c", "agents/one.git", 260);

    let (status, view) = curl(&["-u", "carol:c", &format!("{base}/api/view")]);
    assert_eq!(status, 200);
    let refs = view["refs"].as_object().expect("refs is a map");
    assert_eq!(refs.len(), 200, "the default cap did not apply");
    assert_eq!(view["refs_omitted"], 60);
    assert_eq!(view["paging"]["limit"], 200);
    assert_eq!(view["paging"]["offset"], 0);
    assert_eq!(
        view["paging"]["next"],
        serde_json::json!("/api/view?limit=200&offset=200"),
        "the rest must be one named request away"
    );
}

/// Following `paging.next` visits every ref exactly once. A page with a
/// hole in it is what `/api/log` refuses outright rather than serve, and
/// a paged view is held to the same standard.
#[test]
fn following_the_next_page_reaches_every_ref_exactly_once() {
    let (base, key) = served("walk", "carol * write\n", &["agents/one.git"]);
    seed_refs(&base, &key, "carol:c", "agents/one.git", 130);

    let mut seen: Vec<String> = Vec::new();
    let mut next = Some("/api/view?limit=50&offset=0".to_string());
    let mut hops = 0;
    while let Some(path) = next {
        hops += 1;
        assert!(hops < 20, "paging did not terminate");
        let (status, page) = curl(&["-u", "carol:c", &format!("{base}{path}")]);
        assert_eq!(status, 200);
        seen.extend(page["refs"].as_object().expect("refs is a map").keys().cloned());
        next = page["paging"]["next"].as_str().map(ToString::to_string);
    }
    assert_eq!(hops, 3, "130 refs at 50 a page is three pages");

    let mut expected: Vec<String> = (0..130)
        .map(|i| format!("agents/one.git:refs/heads/t{i:04}"))
        .collect();
    expected.sort();
    assert_eq!(seen, expected, "pages overlapped, skipped, or reordered");
}

/// **The ordering test.** `alice` may read one repository; the node holds
/// a second one with far more refs than the cap. Her omission count must
/// describe her own slice and nothing else.
///
/// Capping before the ACL narrows passes every length assertion above and
/// fails here: she would be served her ten readable refs alongside an
/// `refs_omitted` computed from a page she never saw, which is a
/// measurement of the node's size handed to someone granted one corner of
/// it. D29's non-disclosure is not only about which rows are served.
#[test]
fn an_omission_count_never_counts_rows_the_caller_may_not_read() {
    let (base, key) = served(
        "disclosure",
        "alice  agents/one  read\n\
         carol  *          write\n",
        &["agents/one.git", "agents/two.git"],
    );
    seed_refs(&base, &key, "carol:c", "agents/one.git", 10);
    seed_refs(&base, &key, "carol:c", "agents/two.git", 300);

    let (status, view) = curl(&["-u", "alice:a", &format!("{base}/api/view")]);
    assert_eq!(status, 200);
    let refs = view["refs"].as_object().expect("refs is a map");
    assert_eq!(refs.len(), 10, "alice was served another repository's refs");
    assert!(
        refs.keys().all(|k| k.starts_with("agents/one.git:")),
        "a ref outside the grant survived: {:?}",
        refs.keys().collect::<Vec<_>>()
    );
    assert_eq!(
        view["refs_omitted"], 0,
        "the omission count leaked how many refs alice cannot read"
    );
    assert!(
        view["paging"]["next"].is_null(),
        "alice was sent to a next page that holds nothing she may read"
    );

    // The same node, read by the node-wide credential, is genuinely
    // truncated — otherwise the assertion above passes on a node that
    // simply never bounded anything.
    let (_, whole) = curl(&["-u", "carol:c", &format!("{base}/api/view")]);
    assert_eq!(whole["refs"].as_object().unwrap().len(), 200);
    assert_eq!(whole["refs_omitted"], 110);
}

/// The growth tripwire measures the view, not the page served. Bounding
/// the response must not shrink the number the D30 series is tracked
/// against, or the alarm goes quiet exactly when it should fire.
#[test]
fn the_growth_measurement_describes_the_view_and_not_the_page() {
    // `view_growth` is a node-wide section and `*` deliberately never
    // covers node scope, so reading the metric takes an explicit
    // `@node auditor` row beside the write grant.
    let (base, key) = served(
        "growth",
        "carol  *      write\n\
         carol  @node  auditor\n",
        &["agents/one.git"],
    );
    seed_refs(&base, &key, "carol:c", "agents/one.git", 260);

    let (_, view) = curl(&["-u", "carol:c", &format!("{base}/api/view")]);
    assert_eq!(view["refs"].as_object().unwrap().len(), 200);
    assert_eq!(view["view_growth"]["counts"]["refs"], 260, "the metric was capped too");
    let measured = view["view_growth"]["serialized_bytes"]["refs"]
        .as_u64()
        .expect("refs byte count");
    let served = serde_json::to_vec(&view["refs"]).unwrap().len() as u64;
    assert!(
        measured > served,
        "the byte measurement ({measured}) shrank to the page ({served})"
    );
}

/// Only the two aggregate reads are bounded. `/api/log` pages by `from`
/// and a chain segment is not a slice of a map, so growing it a second
/// `paging` block would offer a client two paging protocols on one
/// endpoint and let it follow the wrong one.
#[test]
fn an_endpoint_outside_the_contract_is_left_alone() {
    // The raw log is node-wide by the same rule `view_growth` is.
    let (base, key) = served(
        "gate",
        "carol  *      write\n\
         carol  @node  auditor\n",
        &["agents/one.git"],
    );
    seed_refs(&base, &key, "carol:c", "agents/one.git", 3);

    let (status, body) = curl(&["-u", "carol:c", &format!("{base}/api/log?from=0")]);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["entries"].as_array().expect("entries").len(), 3);
    assert!(body.get("paging").is_none(), "/api/log grew a second paging protocol");
    assert!(body.get("entries_omitted").is_none());
}
