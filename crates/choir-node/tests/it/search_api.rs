//! Search with no browser in it.
//!
//! The search that shipped with D30 was a page and only a page, on a
//! platform whose primary reader is an agent. These drive the JSON
//! surface that closes that gap, against a real node with real pushed
//! repositories, because the thing under test is `git grep` against a
//! bare repository and a mock of it would test the mock.
//!
//! The grant tests are the reason this file exists at all. A search
//! endpoint is the classic way to leak the existence of something the
//! caller may not read, and "it filters" is a claim that has to fail
//! when the filter is removed.

use choir_node::{AuthTable, Node};

fn git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
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
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git runs")
}

/// One `GET`, as `(status, parsed body)`.
fn get(url: &str, user: &str) -> (u16, serde_json::Value) {
    let out = std::process::Command::new("curl")
        .args(["-s", "-w", "\n%{http_code}", "-u", user])
        .arg(url)
        .output()
        .expect("curl runs");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let (body, code) = text.rsplit_once('\n').unwrap_or((text.as_str(), "0"));
    (
        code.trim().parse().unwrap_or(0),
        serde_json::from_str(body).unwrap_or(serde_json::Value::Null),
    )
}

/// A served node with two repositories, each carrying a distinct term.
///
/// Two rather than one because every interesting claim here is about
/// which repositories a caller reaches, and that is unobservable on a
/// node holding one.
fn served(tag: &str, acl: &str) -> String {
    let work = std::env::temp_dir().join(format!("choir-node-search-api-{tag}"));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");

    let mut table = AuthTable::new();
    table.insert("alice".into(), "a".into());
    table.insert("bob".into(), "b".into());

    let mut node =
        Node::bind_with_auth(&work.join("repos"), 0, Some(table)).expect("node binds free port");
    let port = node.port();
    node.create_repo("agents/one.git").expect("repo created");
    node.create_repo("agents/two.git").expect("repo created");
    if !acl.is_empty() {
        let path = work.join("acl");
        std::fs::write(&path, acl).expect("acl file");
        node.watch_acl_file(path).expect("acl loads");
    }
    std::thread::spawn(move || node.serve_forever());
    let base = format!("http://127.0.0.1:{port}");

    for (repo, term) in [("one", "sequencer"), ("two", "quarantine")] {
        let clone = work.join(repo);
        let url = format!("http://alice:a@127.0.0.1:{port}/agents/{repo}.git");
        assert!(
            git(&work, &["clone", "-q", &url, clone.to_str().unwrap()])
                .status
                .success(),
            "seeding clone of {repo} failed"
        );
        std::fs::create_dir_all(clone.join("src")).unwrap();
        std::fs::write(
            clone.join(format!("src/{term}.rs")),
            format!("fn hold() {{\n    // the {term} decides\n}}\n"),
        )
        .unwrap();
        // A second file carrying the same term, so a `limit` of one has
        // something to cut and truncation is observable.
        std::fs::write(
            clone.join("src/notes.md"),
            format!("the {term} again, on its own line\n"),
        )
        .unwrap();
        assert!(git(&clone, &["add", "."]).status.success());
        assert!(
            git(&clone, &["commit", "-q", "-m", &format!("seed {term}")])
                .status
                .success()
        );
        assert!(
            git(&clone, &["push", "-q", "origin", "HEAD:main"])
                .status
                .success(),
            "seeding push of {repo} failed"
        );
    }
    base
}

/// Every match row, as `repo:path` pairs, for a `code` search.
fn paths(body: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    for row in body["results"].as_array().expect("results is an array") {
        let repo = row["repo"].as_str().expect("repo is a string");
        for hit in row["matches"].as_array().expect("matches is an array") {
            out.push(format!("{repo}:{}", hit["path"].as_str().unwrap_or("?")));
        }
    }
    out.sort();
    out
}

/// A node-wide content search reaches every repository, names which one
/// each match came from, and pins the match to the commit it was found
/// at rather than to the ref name that resolved to it.
#[test]
fn a_node_wide_search_names_the_repository_and_the_commit_each_match_came_from() {
    let base = served("wide", "");
    let (code, body) = get(
        &format!("{base}/api/search?q=quarantine&in=code"),
        "alice:a",
    );
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["repositories"], 2, "both repositories were searched");
    assert_eq!(
        paths(&body),
        vec![
            "agents/two:src/notes.md".to_string(),
            "agents/two:src/quarantine.rs".to_string(),
        ],
        "{body}"
    );
    for row in body["results"].as_array().unwrap() {
        let oid = row["oid"].as_str().expect("every row resolved a commit");
        assert_eq!(oid.len(), 40, "the row carries a full oid: {oid}");
    }
}

/// The grant, which is the whole reason this endpoint needed its own
/// tests: a caller sees no match from a repository they may not read,
/// and asking for it by name is answered exactly as a repository that
/// does not exist. Learning that something exists is itself the leak.
#[test]
fn a_search_cannot_see_or_confirm_a_repository_the_caller_was_not_granted() {
    let base = served(
        "grant",
        "alice  agents/one  write\nalice  agents/two  write\nbob  agents/one  read\n",
    );

    let (code, body) = get(&format!("{base}/api/search?q=quarantine&in=code"), "bob:b");
    assert_eq!(code, 200, "{body}");
    assert_eq!(
        body["repositories"], 1,
        "bob reaches one repository: {body}"
    );
    assert_eq!(body["matches"], 0, "and none of its lines match: {body}");
    assert!(
        !body.to_string().contains("agents/two"),
        "the ungranted repository is named in the response: {body}"
    );

    let (denied, denied_body) = get(
        &format!("{base}/api/search?q=quarantine&repo=agents/two"),
        "bob:b",
    );
    let (absent, absent_body) = get(
        &format!("{base}/api/search?q=quarantine&repo=agents/nowhere"),
        "bob:b",
    );
    assert_eq!(denied, 404, "{denied_body}");
    assert_eq!(
        (denied, denied_body),
        (absent, absent_body),
        "a repository bob may not read answers differently from one that is not there"
    );

    // Alice holds both grants, so the same question has a different
    // answer for her. Without this the test above would pass against an
    // endpoint that finds nothing for anybody.
    let (code, body) = get(
        &format!("{base}/api/search?q=quarantine&in=code"),
        "alice:a",
    );
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["repositories"], 2, "{body}");
    assert_eq!(body["matches"], 2, "{body}");
}

/// Each scope answers in its own shape, and the shapes are what a caller
/// can act on: a path, a path with a line number, a commit oid.
#[test]
fn every_scope_answers_in_the_shape_that_scope_is_for() {
    let base = served("scopes", "");

    let (code, body) = get(
        &format!("{base}/api/search?q=sequencer&in=files"),
        "alice:a",
    );
    assert_eq!(code, 200, "{body}");
    let files: Vec<&str> = body["results"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|row| row["matches"].as_array().unwrap())
        .map(|hit| hit.as_str().expect("a file match is a bare path"))
        .collect();
    assert_eq!(files, vec!["src/sequencer.rs"], "{body}");

    let (code, body) = get(&format!("{base}/api/search?q=sequencer&in=code"), "alice:a");
    assert_eq!(code, 200, "{body}");
    let hit = body["results"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|row| row["matches"].as_array().unwrap())
        .find(|hit| hit["path"] == "src/sequencer.rs")
        .expect("the line inside the file matched");
    assert_eq!(hit["line"], 2, "the line number is a number: {hit}");
    assert!(
        hit["text"]
            .as_str()
            .unwrap()
            .contains("the sequencer decides"),
        "{hit}"
    );

    let (code, body) = get(
        &format!("{base}/api/search?q=seed+sequencer&in=commits"),
        "alice:a",
    );
    assert_eq!(code, 200, "{body}");
    let commit = body["results"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|row| row["matches"].as_array().unwrap())
        .next()
        .expect("the seeding commit matched its own message");
    assert_eq!(
        commit["subject"], "seed sequencer",
        "the commit carries its subject: {commit}"
    );
    assert_eq!(
        commit["oid"].as_str().unwrap_or_default().len(),
        40,
        "{commit}"
    );
}

/// `limit` cuts the rows and says it did, and the count of what was
/// found is the count of what was found — not the count of what fits.
/// A short list that looks complete is the failure this guards.
#[test]
fn a_truncated_search_reports_the_matches_it_did_not_return() {
    let base = served("limit", "");
    let (code, body) = get(
        &format!("{base}/api/search?q=sequencer&in=code&limit=1"),
        "alice:a",
    );
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["truncated"], true, "{body}");
    assert_eq!(body["matches"], 2, "both matches were counted: {body}");
    assert_eq!(paths(&body).len(), 1, "one was returned: {body}");

    let (code, body) = get(&format!("{base}/api/search?q=sequencer&in=code"), "alice:a");
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["truncated"], false, "{body}");
    assert_eq!(paths(&body).len(), 2, "{body}");
}

/// A request the endpoint cannot read is refused with the reason, where
/// the page falls back to its cheapest scope. The page has no way to
/// explain itself to a reader who typed the URL; a caller here does.
#[test]
fn an_unreadable_request_is_refused_with_its_reason() {
    let base = served("refuse", "");
    for (query, needle) in [
        ("q=&in=code", "q is required"),
        ("q=sequencer&in=cod", "in must be one of"),
        ("q=sequencer&limit=0", "limit must be"),
        ("q=sequencer&limit=9000", "limit must be"),
        ("q=sequencer&limit=lots", "limit must be"),
        ("q=sequencer&rev=main", "rev applies to a single repo"),
    ] {
        let (code, body) = get(&format!("{base}/api/search?{query}"), "alice:a");
        assert_eq!(code, 400, "{query} was not refused: {body}");
        let error = body["error"].as_str().unwrap_or_default();
        assert!(
            error.contains(needle),
            "{query} was refused without saying why: {body}"
        );
    }
}

/// A named repository can be searched at a revision that is not its
/// default, which is the whole point of accepting `rev` for one.
#[test]
fn a_named_repository_can_be_searched_at_an_older_commit() {
    let base = served("rev", "");
    let (code, body) = get(
        &format!("{base}/api/search?q=sequencer&in=code&repo=agents/one&rev=main"),
        "alice:a",
    );
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["repositories"], 1, "{body}");
    assert_eq!(body["matches"], 2, "{body}");

    let (code, body) = get(
        &format!("{base}/api/search?q=sequencer&in=code&repo=agents/one&rev=main~1"),
        "alice:a",
    );
    assert_eq!(code, 200, "{body}");
    // One commit back from the only commit is no commit at all: the row
    // is reported with a null oid rather than dropped, so the count of
    // repositories searched stays honest.
    assert_eq!(body["repositories"], 1, "{body}");
    assert_eq!(body["results"][0]["oid"], serde_json::Value::Null, "{body}");
}

/// A `repo=` that is not a repository name cannot become a path. The
/// name reaches `git` as a directory under the node's root, so the
/// question is not whether the answer is wrong but whether the argument
/// can be constructed at all.
#[test]
fn a_repo_parameter_cannot_climb_out_of_the_root() {
    let base = served("traversal", "");
    let (code, body) = get(
        &format!("{base}/api/search?q=sequencer&repo=agents/nowhere"),
        "alice:a",
    );
    assert_eq!(code, 404, "{body}");
    for hostile in [
        "../../etc",
        "agents/..",
        "..%2f..%2fetc",
        "agents/%2e%2e",
        "/etc/passwd",
        "agents",
    ] {
        let (status, answer) = get(
            &format!("{base}/api/search?q=sequencer&repo={hostile}"),
            "alice:a",
        );
        assert_eq!(
            (status, &answer),
            (code, &body),
            "`{hostile}` was answered differently from a name that is merely absent"
        );
    }
}
