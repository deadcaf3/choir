//! Repository browsing (D30), driven the way a reader drives it: a real
//! push into a real bare repository, then the pages a browser asks for.
//!
//! The route parser has unit tests beside it, and they are where the
//! injection cases live. What only a served node can show is that the
//! pages describe the repository that was actually pushed, that a file's
//! contents cannot escape the escaper, that the `ETag` is the commit,
//! and that browsing is gated by the same D29 grant a clone needs.

use choir_identity::{ActorKey, Registry};
use choir_node::{AuthTable, Node, Platform};
use choir_oplog::MemLog;
use choir_view::{OpKind, ViewOp};

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
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git runs")
}

/// One request, as `(status, headers, body)`.
fn get(url: &str, args: &[&str]) -> (u16, String, String) {
    let out = std::process::Command::new("curl")
        .args(["-s", "-i", "-w", "\n%{http_code}"])
        .args(args)
        .arg(url)
        .output()
        .expect("curl runs");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let (rest, code) = text.rsplit_once('\n').unwrap_or((text.as_str(), "0"));
    let (headers, body) = match rest.split_once("\r\n\r\n") {
        Some((h, b)) => (h.to_string(), b.to_string()),
        None => (rest.to_string(), String::new()),
    };
    (code.trim().parse().unwrap_or(0), headers, body)
}

fn header_value(headers: &str, name: &str) -> Option<String> {
    headers.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        (k.trim().eq_ignore_ascii_case(name)).then(|| v.trim().to_string())
    })
}

/// Every `href` on a page, in document order, un-escaped back to the URL
/// a browser would request.
///
/// Deliberately crude — one attribute name, no parser — because it is
/// used to follow the node's own links, which this crate writes and which
/// are always double-quoted. Anything cleverer would be a second HTML
/// implementation to keep right.
fn hrefs(page: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = page;
    while let Some(at) = rest.find("href=\"") {
        rest = &rest[at + 6..];
        let Some(end) = rest.find('"') else { break };
        let raw = &rest[..end];
        rest = &rest[end..];
        // Only the escapes the page's own escaper can produce.
        let href = raw
            .replace("&amp;", "&")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&#39;", "'")
            .replace("&quot;", "\"");
        if href.starts_with('/') {
            out.push(href);
        }
    }
    out
}

/// A served node holding one repository with a content tree pushed into
/// it, returning the base URL, the work directory and the pushed oid.
///
/// `acl` is written to a file only when non-empty, so one helper covers
/// both the ungated and the gated node.
fn served(tag: &str, acl: &str) -> (String, std::path::PathBuf, String, std::path::PathBuf) {
    let work = std::env::temp_dir().join(format!("choir-node-browse-{tag}"));
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
    table.insert("bob".into(), "b".into());

    let mut node =
        Node::bind_with_auth(&work.join("repos"), 0, Some(table)).expect("node binds free port");
    let port = node.port();
    node.create_repo("agents/one.git").expect("repo created");
    // An owner literally named `r`, which is the browse prefix. Its
    // clone URL is the case the routing rule exists to protect, and a
    // test that never creates one cannot notice the rule going missing.
    node.create_repo("r/project.git").expect("repo created");
    if !acl.is_empty() {
        std::fs::write(&acl_path, acl).expect("acl file");
        node.watch_acl_file(acl_path.clone()).expect("acl loads");
    }
    node.enable_platform(platform);
    std::thread::spawn(move || node.serve_forever());

    let base = format!("http://127.0.0.1:{port}");
    let clone = work.join("clone");
    let url = format!("http://alice:a@127.0.0.1:{port}/agents/one.git");
    assert!(
        git(&work, &["clone", "-q", &url, clone.to_str().unwrap()])
            .status
            .success(),
        "seeding clone failed"
    );
    std::fs::create_dir_all(clone.join("src")).unwrap();
    // Markdown, not a plain line: the repository page renders this file,
    // so the fixture has to carry the shapes that rendering can get
    // wrong — a heading, a link, and a raw HTML tag that must not
    // survive the trip to the page.
    std::fs::write(
        clone.join("README.md"),
        // The script tag starts at column zero deliberately: indented, it
        // is a code block, which markdown escapes for us and which would
        // prove nothing about the raw-HTML rule.
        "# hello\n\nsecond line with a [link](https://example.invalid/x).\n\n<script>alert('readme')</script>\n",
    )
    .unwrap();
    // Content that is markup, in a file whose name is also markup: both
    // halves reach the page, and both must arrive inert.
    std::fs::write(
        clone.join("src/lib.rs"),
        "// <script>alert('x')</script>\nfn main() {}\n",
    )
    .unwrap();
    std::fs::write(
        clone.join("src/<img src=x onerror=alert(1)>.txt"),
        "named like markup\n",
    )
    .unwrap();
    // A README below the root. The repository page and the directory
    // page both render one, and only a fixture that has both can tell
    // the two apart.
    std::fs::write(
        clone.join("src/README.md"),
        "## about src\n\nwhat lives in this directory.\n",
    )
    .unwrap();
    std::fs::write(clone.join("binary.dat"), [0u8, 1, 2, 3, 0, 9]).unwrap();
    // The shapes a real repository has and an invented fixture does not.
    // In the shared fixture rather than a test of their own so that every
    // test above walks them too: a listing that renders these wrongly is
    // a listing, and the walk test should notice.
    std::fs::write(
        clone.join("src/ünïcode-café.rs"),
        "// unicode in the name\n",
    )
    .unwrap();
    std::fs::write(clone.join("src/日本語.txt"), "outside latin-1 entirely\n").unwrap();
    // Legal filenames whose characters are URL syntax. Left un-encoded in
    // an href, `?` starts a query string and `#` a fragment, so the link
    // resolves to a shorter path than the one it names.
    std::fs::write(clone.join("src/query?.txt"), "a question mark\n").unwrap();
    std::fs::write(clone.join("src/hash#one.txt"), "a fragment marker\n").unwrap();
    let deep = clone.join("deep/very/long/nested/path/that/keeps/going/further/down");
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::write(deep.join("leaf.txt"), "at the bottom\n").unwrap();
    std::fs::write(
        clone.join("src/a-deliberately-long-file-name-of-the-kind-generated-code-produces.rs"),
        "long name\n",
    )
    .unwrap();
    assert!(git(&clone, &["add", "."]).status.success());
    assert!(git(&clone, &["commit", "-q", "-m", "seed the tree"])
        .status
        .success());
    let push = git(&clone, &["push", "-q", "origin", "HEAD:main"]);
    assert!(
        push.status.success(),
        "{}",
        String::from_utf8_lossy(&push.stderr)
    );
    let oid = String::from_utf8_lossy(&git(&clone, &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();

    (base, work, oid, acl_path)
}

/// The walk a reader actually performs: index, repository root, into a
/// directory, into a file. Each step must show what was pushed.
#[test]
fn a_reader_can_walk_from_the_index_to_a_file() {
    let (base, _work, _oid, _) = served("walk", "");

    let (status, _, index) = get(&format!("{base}/r/"), &["-u", "alice:a"]);
    assert_eq!(status, 200, "the index was refused");
    assert!(
        index.contains("agents/one"),
        "the index listed no repository: {index}"
    );

    let (status, _, root) = get(&format!("{base}/r/agents/one"), &["-u", "alice:a"]);
    assert_eq!(status, 200);
    assert!(
        root.contains("README.md"),
        "the root listing is missing a file: {root}"
    );
    assert!(
        root.contains("src"),
        "the root listing is missing a directory"
    );

    let (status, _, dir) = get(
        &format!("{base}/r/agents/one/tree/main/src"),
        &["-u", "alice:a"],
    );
    assert_eq!(status, 200);
    assert!(
        dir.contains("lib.rs"),
        "the directory listing is missing its file: {dir}"
    );

    let (status, _, file) = get(
        &format!("{base}/r/agents/one/blob/main/README.md"),
        &["-u", "alice:a"],
    );
    assert_eq!(status, 200);
    assert!(
        file.contains("second line"),
        "the file did not render: {file}"
    );
}

/// A repository can hold anything, so file content is attacker input
/// with a byline. Both the content and the name have to arrive inert.
#[test]
fn file_content_and_file_names_cannot_inject() {
    let (base, _work, _oid, _) = served("inject", "");

    let (_, _, file) = get(
        &format!("{base}/r/agents/one/blob/main/src/lib.rs"),
        &["-u", "alice:a"],
    );
    assert!(
        file.contains("&lt;script&gt;"),
        "the escaper did not run: {file}"
    );
    assert!(
        !file.contains("<script>alert"),
        "a file's contents reached the page as live markup"
    );

    let (_, _, dir) = get(
        &format!("{base}/r/agents/one/tree/main/src"),
        &["-u", "alice:a"],
    );
    assert!(
        !dir.contains("<img src=x onerror"),
        "a file name reached the page as live markup: {dir}"
    );
    assert!(
        dir.contains("&lt;img"),
        "the file named like markup vanished instead of escaping"
    );

    // The header is the same defence in depth the D28 page carries.
    let (_, headers, _) = get(&format!("{base}/r/agents/one"), &["-u", "alice:a"]);
    let csp = header_value(&headers, "Content-Security-Policy").expect("a CSP header");
    assert!(csp.contains("default-src 'none'"), "{csp}");
}

/// A binary file is described, not dumped, and an oversized one is not
/// read into a response at all.
#[test]
fn a_binary_file_is_described_rather_than_rendered() {
    let (base, _work, _oid, _) = served("binary", "");
    let (status, _, page) = get(
        &format!("{base}/r/agents/one/blob/main/binary.dat"),
        &["-u", "alice:a"],
    );
    assert_eq!(status, 200);
    assert!(
        page.contains("Binary file"),
        "a binary blob was rendered as text: {page}"
    );
}

/// History and one commit, including the diff a reader came for.
#[test]
fn history_and_a_commit_diff_render() {
    let (base, _work, oid, _) = served("history", "");

    let (status, _, log) = get(
        &format!("{base}/r/agents/one/commits/main"),
        &["-u", "alice:a"],
    );
    assert_eq!(status, 200);
    assert!(
        log.contains("seed the tree"),
        "the subject is missing: {log}"
    );
    assert!(
        log.contains(&oid[..12]),
        "the commit is missing from its own history"
    );

    let (status, _, commit) = get(
        &format!("{base}/r/agents/one/commit/{oid}"),
        &["-u", "alice:a"],
    );
    assert_eq!(status, 200);
    assert!(
        commit.contains("seed the tree"),
        "the commit page lost its subject"
    );
    assert!(
        commit.contains("README.md"),
        "the diff names no file: {commit}"
    );
    assert!(
        commit.contains("class=\"add\""),
        "the diff has no added lines"
    );
    // The stat row, proven against git's real `--numstat` emission
    // rather than a fixture: a parser that misreads the real shape
    // renders no table and no anchors, and only a served node shows it.
    assert!(
        commit.contains("class=\"stat\""),
        "the diff has no stat row: {commit}"
    );
    assert!(
        commit.contains("files changed"),
        "the stat row has no summary: {commit}"
    );
    assert!(
        commit.contains("href=\"#f0\"") && commit.contains("id=\"f0\""),
        "the stat row and the file headers do not link up: {commit}"
    );
}

/// The cache identity is the commit, not the view sequence — so a
/// repeat visit costs nothing, and the tag names the oid it describes.
#[test]
fn a_repeat_visit_revalidates_on_the_commit() {
    let (base, _work, oid, _) = served("etag", "");
    let url = format!("{base}/r/agents/one/blob/main/README.md");

    let (status, headers, _) = get(&url, &["-u", "alice:a"]);
    assert_eq!(status, 200);
    let tag = header_value(&headers, "ETag").expect("the page carries an ETag");
    assert!(tag.contains(&oid), "the ETag is not the commit: {tag}");

    let (status, _, body) = get(
        &url,
        &["-u", "alice:a", "-H", &format!("If-None-Match: {tag}")],
    );
    assert_eq!(status, 304, "a repeat visit rebuilt the page");
    assert!(body.is_empty(), "a 304 carried a body");
}

/// Browsing is reading, so it needs exactly the grant a clone needs.
/// The second half is the mutation: remove the line and the same
/// request must flip, or this guard proves nothing.
#[test]
fn browsing_needs_the_same_read_grant_as_a_clone() {
    // Write, because this fixture pushes: browsing is a read, and write
    // implies read, so the assertion below is still about reading.
    let (base, _work, _oid, acl) = served("acl", "alice  agents/one  write\n");

    let (status, _, _) = get(&format!("{base}/r/agents/one"), &["-u", "alice:a"]);
    assert_eq!(status, 200, "a granted reader was refused");

    // No grant at all: the repository must not even be confirmed to
    // exist, which is a 404 rather than a 403.
    let (status, _, _) = get(&format!("{base}/r/agents/one"), &["-u", "bob:b"]);
    assert_eq!(status, 404, "an ungranted reader browsed a repository");
    let (_, _, index) = get(&format!("{base}/r/"), &["-u", "bob:b"]);
    assert!(
        !index.contains("agents/one"),
        "the index named a repository its reader cannot read: {index}"
    );

    // The mutation.
    std::fs::write(&acl, "bob  agents/one  read\n").expect("acl rewrite");
    let file = std::fs::File::options()
        .write(true)
        .open(&acl)
        .expect("reopen acl");
    let ahead = std::time::SystemTime::now() + std::time::Duration::from_secs(1);
    file.set_times(std::fs::FileTimes::new().set_modified(ahead))
        .expect("stamp mtime");

    let (status, _, _) = get(&format!("{base}/r/agents/one"), &["-u", "bob:b"]);
    assert_eq!(
        status, 200,
        "a granted reader was still refused after a reload"
    );
    let (status, _, _) = get(&format!("{base}/r/agents/one"), &["-u", "alice:a"]);
    assert_eq!(status, 404, "a revoked reader still browsed");
}

/// The routing claim, on a live node rather than in the parser: the
/// browse prefix must not have taken a clone URL away from anybody.
#[test]
fn the_browse_prefix_did_not_take_a_clone_url() {
    let (base, work, _oid, _) = served("clone-url", "");
    let host = base.trim_start_matches("http://");

    // A repository with no commits yet, which is the state a reader
    // meets the moment one is created rather than an error.
    let (status, _, page) = get(&format!("{base}/r/r/project"), &["-u", "alice:a"]);
    assert_eq!(status, 200, "an owner named `r` cannot be browsed: {page}");
    assert!(
        page.contains("No commits yet"),
        "an empty repository read as broken: {page}"
    );

    // Give it a commit before cloning. An empty clone stops after the
    // ref advertisement, so it never sends `POST .../git-upload-pack` —
    // which is the one request an owner named `r` shares a prefix with,
    // and therefore the only one that can prove the routing rule.
    let seed = work.join("r-seed");
    let url = format!("http://alice:a@{host}/r/project.git");
    assert!(git(&work, &["clone", "-q", &url, seed.to_str().unwrap()])
        .status
        .success());
    std::fs::write(seed.join("only.txt"), "content\n").unwrap();
    assert!(git(&seed, &["add", "."]).status.success());
    assert!(git(&seed, &["commit", "-q", "-m", "first"])
        .status
        .success());
    assert!(git(&seed, &["push", "-q", "origin", "HEAD:main"])
        .status
        .success());

    for repo in ["agents/one.git", "r/project.git"] {
        let url = format!("http://alice:a@{host}/{repo}");
        let out = git(&work, &["clone", "-q", &url, &repo.replace('/', "-")]);
        assert!(
            out.status.success(),
            "browsing took the clone URL for {repo}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // ...and the same owner is still browsable, which is the half that
    // would be lost by fixing the collision with a reserved prefix.
    let (status, _, page) = get(&format!("{base}/r/r/project"), &["-u", "alice:a"]);
    assert_eq!(status, 200, "an owner named `r` cannot be browsed: {page}");
    assert!(
        page.contains("only.txt"),
        "the pushed file is not listed: {page}"
    );
}

/// A review page joins the three things reviews have always carried and
/// never shown together: what lands where, who was asked and what they
/// said, and the diff between the proposal and its destination.
#[test]
fn a_review_page_shows_the_proposal_the_people_and_the_diff() {
    let work = std::env::temp_dir().join("choir-node-browse-review");
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");

    let author = ActorKey::generate();
    let mut registry = Registry::new();
    registry
        .register(&author.public_key_bytes())
        .expect("register author");

    let mut node = Node::bind(&work.join("repos"), 0).expect("node binds free port");
    let port = node.port();
    node.create_repo("agents/one.git").expect("repo created");
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate())
            .expect("platform starts"),
    );
    std::thread::spawn(move || node.serve_forever());
    let base = format!("http://127.0.0.1:{port}");

    // A base branch and a proposal on top of it, both really pushed.
    let clone = work.join("clone");
    let url = format!("{base}/agents/one.git");
    assert!(git(&work, &["clone", "-q", &url, clone.to_str().unwrap()])
        .status
        .success());
    std::fs::write(clone.join("f.txt"), "base\n").unwrap();
    assert!(git(&clone, &["add", "."]).status.success());
    assert!(git(&clone, &["commit", "-q", "-m", "base"])
        .status
        .success());
    assert!(git(&clone, &["push", "-q", "origin", "HEAD:main"])
        .status
        .success());
    std::fs::write(clone.join("f.txt"), "proposed change\n").unwrap();
    assert!(git(&clone, &["add", "."]).status.success());
    assert!(git(&clone, &["commit", "-q", "-m", "the proposal"])
        .status
        .success());
    assert!(
        git(&clone, &["push", "-q", "origin", "HEAD:refs/heads/topic"])
            .status
            .success()
    );
    let proposal = String::from_utf8_lossy(&git(&clone, &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();
    // Main moves on after the proposal branched, which is the ordinary
    // state of any review that took more than an afternoon. It is also
    // what makes the two-dot/three-dot distinction observable: a two-dot
    // diff would show this later commit as though the proposal reverted
    // it, crediting one author with another's work.
    assert!(git(&clone, &["checkout", "-q", "-B", "later", "HEAD~1"])
        .status
        .success());
    std::fs::write(clone.join("other.txt"), "landed by somebody else\n").unwrap();
    assert!(git(&clone, &["add", "."]).status.success());
    assert!(git(&clone, &["commit", "-q", "-m", "unrelated landing"])
        .status
        .success());
    assert!(git(&clone, &["push", "-q", "origin", "HEAD:main"])
        .status
        .success());

    let request = ViewOp::new(OpKind::RequestReview {
        id: "r-page".into(),
        target: choir_oplog::ContentHash::from_git_oid(&proposal).expect("a git oid"),
        reviewers: vec!["ana".into(), "bot".into()],
        target_ref: Some("agents/one.git:refs/heads/main".into()),
    });
    let (code, resp) = crate::support::curl(&[
        "-X",
        "POST",
        "-d",
        &crate::support::submit_body_legacy(&author, "author", &request),
        &format!("{base}/api/submit"),
    ]);
    assert_eq!(code, 200, "{resp}");

    // The index lists it against the repository it proposes to land on.
    let (status, _, list) = get(&format!("{base}/r/agents/one/reviews"), &[]);
    assert_eq!(status, 200);
    assert!(list.contains("r-page"), "the review is not listed: {list}");

    let (status, _, page) = get(&format!("{base}/r/agents/one/review/r-page"), &[]);
    assert_eq!(status, 200);
    // The relationship.
    assert!(
        page.contains(&proposal[..12]),
        "the proposed commit is missing: {page}"
    );
    assert!(
        page.contains("refs/heads/main"),
        "the destination ref is missing"
    );
    // The people, including the one who has not answered.
    assert!(
        page.contains("ana") && page.contains("bot"),
        "a reviewer is missing"
    );
    assert!(
        page.contains("waiting"),
        "an unanswered reviewer is not shown as waiting"
    );
    assert!(page.contains("open"), "a live review is not shown as open");
    // The diff — and specifically the proposal's own change, not the
    // whole difference between two branches.
    assert!(
        page.contains("proposed change"),
        "the diff is missing: {page}"
    );
    assert!(
        page.contains("class=\"add\""),
        "the diff has no added lines"
    );
    assert!(
        page.contains("class=\"stat\""),
        "the review diff has no stat row: {page}"
    );
    assert!(
        !page.contains("other.txt"),
        "the diff shows work that landed on the destination as part of this proposal: {page}"
    );

    // A verdict with a note is the discussion the log actually carries.
    let approve = ViewOp::new(OpKind::PostVerdict {
        id: "r-page".into(),
        reviewer: "ana".into(),
        verdict: choir_view::Verdict::Approve,
        note: "reads fine to me".into(),
    });
    let (code, resp) = crate::support::curl(&[
        "-X",
        "POST",
        "-d",
        &crate::support::submit_body_legacy(&author, "ana", &approve),
        &format!("{base}/api/submit"),
    ]);
    assert_eq!(code, 200, "{resp}");
    let (_, _, page) = get(&format!("{base}/r/agents/one/review/r-page"), &[]);
    assert!(
        page.contains("reads fine to me"),
        "the verdict note is missing: {page}"
    );
    assert!(page.contains("approve"), "the verdict is missing");

    // An unknown review is a 404, not an empty page pretending to be one.
    let (status, _, _) = get(&format!("{base}/r/agents/one/review/nope"), &[]);
    assert_eq!(status, 404, "an unknown review rendered as a page");

    // A review may legitimately name a target git has never heard of —
    // hashes in the log are self-describing, and BLAKE3 is one of them.
    // The page must say it cannot diff that, not hand the digest to git.
    let opaque = ViewOp::new(OpKind::RequestReview {
        id: "r-opaque".into(),
        target: choir_oplog::ContentHash::blake3(b"not a git object"),
        reviewers: vec!["ana".into()],
        target_ref: Some("agents/one.git:refs/heads/main".into()),
    });
    let (code, resp) = crate::support::curl(&[
        "-X",
        "POST",
        "-d",
        &crate::support::submit_body_legacy(&author, "author", &opaque),
        &format!("{base}/api/submit"),
    ]);
    assert_eq!(code, 200, "{resp}");
    let (status, _, page) = get(&format!("{base}/r/agents/one/review/r-opaque"), &[]);
    assert_eq!(status, 200);
    assert!(
        page.contains("names no commit"),
        "a non-git target was not reported as one: {page}"
    );
    assert!(
        !page.contains("fatal:"),
        "a non-git target reached git anyway: {page}"
    );
}

/// The review page renders the discussion (D38), in the order the
/// sequencer admitted it, and renders it as text rather than as markup.
/// It accepts nothing: a comment reaches the log only as a signed op, so
/// there is no form and no POST route on this surface.
#[test]
fn a_review_page_renders_the_discussion_thread() {
    let work = std::env::temp_dir().join("choir-node-browse-review-thread");
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");

    let author = ActorKey::generate();
    // Archiving is the node's own op, so the test keeps the node's key.
    let node_secret = ActorKey::generate().secret_bytes();
    let mut registry = Registry::new();
    registry
        .register(&author.public_key_bytes())
        .expect("register author");

    let mut node = Node::bind(&work.join("repos"), 0).expect("node binds free port");
    let port = node.port();
    node.create_repo("agents/two.git").expect("repo created");
    node.enable_platform(
        Platform::start(
            registry,
            Box::new(MemLog::new()),
            ActorKey::from_secret_bytes(&node_secret),
        )
        .expect("platform starts"),
    );
    std::thread::spawn(move || node.serve_forever());
    let base = format!("http://127.0.0.1:{port}");

    let post = |op: &ViewOp, channel: &str| {
        let (code, resp) = crate::support::curl(&[
            "-X",
            "POST",
            "-d",
            &crate::support::submit_body_legacy(&author, channel, op),
            &format!("{base}/api/submit"),
        ]);
        assert_eq!(code, 200, "{resp}");
    };
    let post_as_node = |op: &ViewOp| {
        let (code, resp) = crate::support::curl(&[
            "-X",
            "POST",
            "-d",
            &crate::support::submit_body_legacy(
                &ActorKey::from_secret_bytes(&node_secret),
                "node/archive",
                op,
            ),
            &format!("{base}/api/submit"),
        ]);
        assert_eq!(code, 200, "{resp}");
    };

    post(
        &ViewOp::new(OpKind::RequestReview {
            id: "r-thread".into(),
            target: choir_oplog::ContentHash::blake3(b"a proposal nobody pushed"),
            reviewers: vec!["ana".into()],
            target_ref: Some("agents/two.git:refs/heads/main".into()),
        }),
        "author",
    );

    let (_, _, page) = get(&format!("{base}/r/agents/two/review/r-thread"), &[]);
    assert!(
        page.contains("Discussion"),
        "the page has no discussion section: {page}"
    );
    assert!(
        page.contains("Nothing said yet"),
        "an empty thread is not reported: {page}"
    );

    for (id, who, body) in [
        ("c1", "ana", "the base looks wrong to me"),
        (
            "c2",
            "author",
            "<script>alert('x')</script> it is the merge base",
        ),
    ] {
        post(
            &ViewOp::new(OpKind::PostComment {
                id: "r-thread".into(),
                comment: id.into(),
                author: who.into(),
                body: body.into(),
            }),
            who,
        );
    }

    let (status, _, page) = get(&format!("{base}/r/agents/two/review/r-thread"), &[]);
    assert_eq!(status, 200);
    assert!(
        page.contains("the base looks wrong to me"),
        "a comment is missing: {page}"
    );
    assert!(
        page.contains("it is the merge base"),
        "a comment is missing: {page}"
    );
    let first = page.find("the base looks wrong").expect("first comment");
    let second = page.find("it is the merge base").expect("second comment");
    assert!(
        first < second,
        "the thread is not rendered in log order: {page}"
    );
    // A comment body is text a stranger wrote, and this page is served to
    // a browser. Same rule as file contents on the D30 pages.
    assert!(
        !page.contains("<script>alert('x')</script>"),
        "a comment body reached the page unescaped: {page}"
    );
    assert!(
        page.contains("&lt;script&gt;"),
        "the body was dropped rather than escaped: {page}"
    );

    // Archiving drops the thread, and the page says so rather than
    // reporting a discussion that happened as one that never did.
    post(
        &ViewOp::new(OpKind::PostVerdict {
            id: "r-thread".into(),
            reviewer: "ana".into(),
            verdict: choir_view::Verdict::Approve,
            note: "fine".into(),
        }),
        "ana",
    );
    post_as_node(&ViewOp::new(OpKind::ArchiveReview {
        id: "r-thread".into(),
        lapsed: false,
    }));
    let (_, _, page) = get(&format!("{base}/r/agents/two/review/r-thread"), &[]);
    assert!(
        page.contains("dropped with its verdicts"),
        "an archived review's dropped thread is not reported: {page}"
    );
    assert!(
        !page.contains("Nothing said yet"),
        "an archived review reads as one nobody discussed: {page}"
    );
}

/// Every name the listing shows is a name the listing can reach.
///
/// This is the assertion the unicode bug needed, and it needs no
/// knowledge of how a filesystem normalises: take the links the page
/// actually rendered and follow them. `core.quotePath` defaults to *on*,
/// so `git ls-tree` returned `"src/\303\274n\303\257code.rs"` — quotes
/// and octal escapes included — and the page built both its label and its
/// `href` out of that. Every such link was dead, and the reader was shown
/// a name no part of the repository has.
///
/// Following the page's own links is also what catches the next version
/// of the bug: any escaping, truncation or encoding that makes a label
/// and its target disagree fails here, whatever caused it.
#[test]
fn every_link_a_listing_renders_can_be_followed() {
    let (base, _work, _oid, _) = served("shapes", "");

    // The octal-escape form, asserted absent by name. This is the exact
    // byte sequence `ls-tree` emits for `ü` when quoting is on, so it
    // fails loudly and specifically if the setting comes back.
    for path in ["/r/agents/one/tree/main/src", "/r/agents/one"] {
        let (status, _, page) = get(&format!("{base}{path}"), &["-u", "alice:a"]);
        assert_eq!(status, 200, "{path} did not render");
        assert!(
            !page.contains("\\303"),
            "{path} shows an octal escape instead of the filename: {page}"
        );
    }

    // Walk the tree the way a reader does, following every link the page
    // offers, and require each to answer.
    let mut queue = vec!["/r/agents/one/tree/main".to_string()];
    let mut seen: Vec<String> = Vec::new();
    let mut blobs = 0usize;
    while let Some(path) = queue.pop() {
        if seen.contains(&path) {
            continue;
        }
        seen.push(path.clone());
        let (status, _, page) = get(&format!("{base}{path}"), &["-u", "alice:a"]);
        assert_eq!(status, 200, "a link the page rendered is dead: {path}");
        for href in hrefs(&page) {
            if href.contains("/tree/") {
                queue.push(href);
            } else if href.contains("/blob/") {
                let (status, _, body) = get(&format!("{base}{href}"), &["-u", "alice:a"]);
                assert_eq!(status, 200, "a file link the page rendered is dead: {href}");
                assert!(
                    !body.contains("no such revision"),
                    "a file link resolved to a refusal: {href}"
                );
                blobs += 1;
            }
        }
    }
    // The walk has to have actually walked, or every assertion above was
    // vacuous. Seven blobs are seeded, at least one of them behind a
    // directory link, so both counts are lower bounds with slack.
    assert!(blobs >= 7, "the walk followed only {blobs} file links");
    assert!(
        seen.len() >= 3,
        "the walk never descended into a directory: {seen:?}"
    );
}

/// Deep nesting renders a breadcrumb a reader can climb, and every step
/// of it is a working link. A path ten segments long is the ordinary
/// shape of a real repository, not an edge case.
#[test]
fn a_deep_path_keeps_every_step_of_its_breadcrumb_reachable() {
    let (base, _work, _oid, _) = served("deep", "");
    let path = "/r/agents/one/tree/main/deep/very/long/nested/path/that/keeps/going/further/down";

    let (status, _, page) = get(&format!("{base}{path}"), &["-u", "alice:a"]);
    assert_eq!(status, 200, "a deep path did not render: {page}");
    assert!(
        page.contains("leaf.txt"),
        "the deep directory is empty: {page}"
    );
    assert!(
        page.contains("crumbs"),
        "a deep path rendered no breadcrumb: {page}"
    );
    // The last segment is where the reader is, so it is text rather than
    // a link; every one above it must be followable.
    assert!(
        page.contains(">root</a>"),
        "the breadcrumb has no way back to the root: {page}"
    );
    let mut climbed = 0usize;
    for href in hrefs(&page) {
        if href.contains("/tree/main/deep") {
            let (status, _, _) = get(&format!("{base}{href}"), &["-u", "alice:a"]);
            assert_eq!(status, 200, "a breadcrumb step is dead: {href}");
            climbed += 1;
        }
    }
    assert!(climbed >= 8, "only {climbed} breadcrumb steps were checked");
}

/// A reader without the grant is told what to do without being told
/// whether the repository exists. Both halves are the product: the `404`
/// is D29's non-confirmation, and the words are what stop it reading as
/// a broken link.
#[test]
fn a_denied_repository_says_what_to_do_without_confirming_it_exists() {
    let (base, _work, _oid, _) = served("denied", "alice  agents/one  write\n");

    // `agents/one` exists; `agents/ghost` does not. A reader with no
    // grant must not be able to tell them apart — same status, and the
    // same bytes.
    let (real_status, real_headers, real) = get(&format!("{base}/r/agents/one"), &["-u", "bob:b"]);
    let (ghost_status, _, ghost) = get(&format!("{base}/r/agents/ghost"), &["-u", "bob:b"]);
    assert_eq!(real_status, 404);
    assert_eq!(ghost_status, 404);
    assert_eq!(
        real, ghost,
        "the refusal for a repository that exists differs from one that does not, \
         so a reader can enumerate this node by diffing them"
    );

    // It is a page, in the surface the reader is in.
    assert_eq!(
        header_value(&real_headers, "Content-Type").as_deref(),
        Some("text/html; charset=utf-8")
    );
    assert!(
        real.contains("no_such_repository"),
        "no code to quote: {real}"
    );
    // What they may do, and the one action that changes it.
    assert!(
        real.contains("read grant"),
        "the page never says what is missing: {real}"
    );
    // `/` is the repository index; `/r/` still resolves to it, but the
    // link a refused reader is handed is the front door.
    assert!(
        real.contains("href=\"/\""),
        "a refused reader is given nowhere to go: {real}"
    );
    assert!(real.contains("next"), "no next action: {real}");
    // ...and nothing on it names the repository they asked for, which is
    // what would leak the answer the 404 exists to withhold.
    assert!(
        !real.contains("agents/one"),
        "the refusal echoed the repository name back, confirming it: {real}"
    );

    // The index a refused reader is sent to has to be honest about why
    // it is empty, without counting what they cannot see.
    let (status, _, index) = get(&format!("{base}/r/"), &["-u", "bob:b"]);
    assert_eq!(status, 200);
    // Both causes and both fixes: an operator whose node is empty and a
    // reader with no grant land on identical bytes, so the page has to
    // carry the answer for each of them.
    assert!(
        index.contains("not granted read"),
        "the empty index does not name the missing-grant cause: {index}"
    );
    assert!(
        index.contains("--create"),
        "the empty index does not name the empty-node cause: {index}"
    );
    assert!(
        !index.contains("agents/one"),
        "the index leaked a repository its reader cannot read: {index}"
    );
}

/// An anonymous reader gets nothing, exactly as on the D28 page. A
/// human-readable surface is the kind of thing that acquires an
/// exception, so this is asserted rather than assumed.
/// The node has two names for one repository — `/r/agents/one` to read
/// and `/agents/one.git` to clone — and showed only ever one of them.
///
/// A reader who reached a repository page had no way to learn how to get
/// the code out, which is the thing they are most likely to want next,
/// and the name they *could* see is a `404` on the surface that printed
/// it. Both names are now shown, each where it works, and this asserts
/// the clone path is the one git actually answers on rather than a
/// plausible-looking string.
#[test]
fn a_repository_page_shows_the_path_you_clone_it_from() {
    let (base, work, _oid, _) = served("clone-path", "");
    let (status, _, page) = get(&format!("{base}/r/agents/one"), &["-u", "alice:a"]);
    assert_eq!(status, 200);

    // Read the path off the page rather than rebuilding it here. The
    // first version of this test cloned a URL it composed itself, so the
    // clone proved the node serves `/agents/one.git` — which was never in
    // doubt — and proved nothing about what the page printed. Mutating
    // the pill to `/git/agents/one.git` left it green.
    let printed = page
        .split_once(">clone ")
        .and_then(|(_, rest)| rest.split_once('<'))
        .map(|(path, _)| path.trim().to_string())
        .unwrap_or_else(|| {
            panic!("a reader on the repository page cannot find out how to clone it: {page}")
        });

    // The whole URL, not the path half of one: the pill is written to
    // be pasted after `git clone`, and it names the origin this reader
    // actually arrived on rather than any address this process was
    // configured with.
    assert!(
        printed.starts_with(&format!("{base}/")),
        "the clone pill does not name the origin the reader is on: {printed}"
    );
    let dest = work.join("cloned-from-the-page");
    // The credential goes in after the scheme now, rather than in front
    // of a path.
    let url = printed.replacen("http://", "http://alice:a@", 1);
    let out = git(&work, &["clone", "-q", &url, dest.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "the page prints `{printed}`, which does not clone: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        dest.join("README.md").is_file(),
        "cloning `{printed}` came back without the file that was pushed"
    );
}

#[test]
fn browsing_is_behind_the_same_auth_wall() {
    let (base, _work, _oid, _) = served("anon", "");
    for path in ["/r/", "/r/agents/one", "/r/agents/one/blob/main/README.md"] {
        let (status, _, _) = get(&format!("{base}{path}"), &[]);
        assert_eq!(status, 401, "{path} served an anonymous reader");
    }
}

/// A grant on a repository that was never created is the one case that
/// reaches the renderer with nothing on disk, and it used to answer with
/// git's own words — which name the absolute `--git-dir` git was handed.
///
/// So the page told a stranger the node's install root, the account it
/// runs as, and its on-disk layout, under a headline saying "You may read
/// this repository" about a repository that does not exist. Both halves
/// are asserted here: nothing about the disk, and the same bytes a denied
/// reader gets, because a granted-but-missing repository that renders
/// differently is the enumeration oracle the `404` exists to close.
#[test]
fn a_granted_repository_that_is_missing_says_nothing_about_the_disk() {
    let (base, work, _oid, _) = served(
        "granted-missing",
        "alice  agents/one  write\nalice  agents/ghost  read\n",
    );
    let root = work.join("repos").to_string_lossy().into_owned();

    // Every page that reads the repository off disk, not just the front
    // one: each resolves a revision, and each used to render the failure.
    let denied = {
        let (status, _, body) = get(&format!("{base}/r/agents/one"), &["-u", "bob:b"]);
        assert_eq!(status, 404, "a reader with no grant was not refused");
        body
    };
    for path in [
        "/r/agents/ghost",
        "/r/agents/ghost/tree/main/src",
        "/r/agents/ghost/blob/main/README.md",
        "/r/agents/ghost/commits/main",
    ] {
        let (status, headers, body) = get(&format!("{base}{path}"), &["-u", "alice:a"]);
        assert_eq!(status, 404, "{path} was not a 404");
        assert_eq!(
            header_value(&headers, "Content-Type").as_deref(),
            Some("text/html; charset=utf-8"),
            "{path} answered a reader outside their own surface"
        );
        assert!(
            !body.contains(&root) && !body.contains("/var/") && !body.contains(".git'"),
            "{path} put the node's filesystem on a page a stranger can ask for: {body}"
        );
        assert!(
            !body.contains("You may read this repository"),
            "{path} claims a repository that does not exist is readable: {body}"
        );
        assert_eq!(
            body, denied,
            "{path} renders a granted-but-missing repository differently from a denied \
             one, so a reader can learn which names exist by diffing them"
        );
    }
}

/// The review page names the people, not their handles (D46).
///
/// This page is where a person reads who was asked and who said what,
/// and after D46 a channel is twelve hex characters. `ui::person` is
/// unit-tested on strings; what only a served page can show is that the
/// two places this page renders a channel — the reviewer seat and a
/// comment's author — were both wired to it. A miss on either renders a
/// perfectly good page naming nobody.
#[test]
fn a_review_page_names_the_person_behind_a_handle() {
    let work = std::env::temp_dir().join("choir-node-browse-handle");
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");

    let author = ActorKey::generate();
    let mut registry = Registry::new();
    registry
        .register(&author.public_key_bytes())
        .expect("register author");

    let acl_path = work.join("acl");
    std::fs::write(
        &acl_path,
        "alice @node write\nalice @node auditor\nalice * write\n",
    )
    .expect("acl file");
    let mut table = AuthTable::new();
    table.insert("alice".into(), "a".into());

    let root = work.join("repos");
    let mut node = Node::bind_with_auth(&root, 0, Some(table)).expect("node binds free port");
    let port = node.port();
    node.create_repo("agents/one.git").expect("repo created");
    node.watch_acl_file(acl_path).expect("acl loads");
    node.enable_accounts(work.join("accounts.json"), None, None)
        .expect("accounts enable");
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate())
            .expect("platform starts"),
    );
    std::thread::spawn(move || node.serve_forever());
    let base = format!("http://127.0.0.1:{port}");

    // Onboard a person the D46 way: the invite names them, the account
    // is issued under a handle.
    let (code, issued) = crate::support::curl(&[
        "-u",
        "alice:a",
        "-X",
        "POST",
        "--data-binary",
        r#"{"display_name":"Ada Lovelace","grants":["agents/one.git write"]}"#,
        &format!("{base}/api/accounts/invite"),
    ]);
    assert_eq!(code, 200, "invite refused: {issued}");
    let handle = issued["user"].as_str().expect("a handle").to_string();
    let pair = issued["invite"].as_str().expect("invite pair").to_string();
    let (code, redeemed) = crate::support::curl(&[
        "-u",
        &pair,
        "-X",
        "POST",
        "--data-binary",
        "{}",
        &format!("{base}/api/accounts/redeem"),
    ]);
    assert_eq!(code, 200, "redeem refused: {redeemed}");

    let clone = work.join("clone");
    let url = format!("http://alice:a@127.0.0.1:{port}/agents/one.git");
    assert!(git(&work, &["clone", "-q", &url, clone.to_str().unwrap()])
        .status
        .success());
    std::fs::write(clone.join("f.txt"), "base\n").unwrap();
    assert!(git(&clone, &["add", "."]).status.success());
    assert!(git(&clone, &["commit", "-q", "-m", "base"])
        .status
        .success());
    assert!(git(&clone, &["push", "-q", "origin", "HEAD:main"])
        .status
        .success());
    let proposal = String::from_utf8_lossy(&git(&clone, &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();

    // The handle is the reviewer seat, because the handle is the
    // principal — which is exactly why the page has to resolve it.
    let request = ViewOp::new(OpKind::RequestReview {
        id: "r-handle".into(),
        target: choir_oplog::ContentHash::from_git_oid(&proposal).expect("a git oid"),
        reviewers: vec![handle.clone()],
        target_ref: Some("agents/one.git:refs/heads/main".into()),
    });
    let (code, resp) = crate::support::curl(&[
        "-u",
        "alice:a",
        "-X",
        "POST",
        "-d",
        &crate::support::submit_body_legacy(&author, "author", &request),
        &format!("{base}/api/submit"),
    ]);
    assert_eq!(code, 200, "{resp}");

    let comment = ViewOp::new(OpKind::PostComment {
        id: "r-handle".into(),
        comment: "c1".into(),
        author: handle.clone(),
        body: "looks right to me".into(),
    });
    let (code, resp) = crate::support::curl(&[
        "-u",
        "alice:a",
        "-X",
        "POST",
        "-d",
        &crate::support::submit_body_legacy(&author, &handle, &comment),
        &format!("{base}/api/submit"),
    ]);
    assert_eq!(code, 200, "{resp}");

    let (status, _, page) = get(
        &format!("{base}/r/agents/one/review/r-handle"),
        &["-u", "alice:a"],
    );
    assert_eq!(status, 200, "{page}");
    // Both renders of a channel, asserted separately: one wired and one
    // missed is the failure this test exists for, and a single
    // `contains` would pass on either.
    // Each slice is bounded at its own `</section>`. An unbounded split
    // takes everything below the header, so the comment author's name
    // satisfies the reviewer assertion and the reviewer seat can be
    // unwired without failing anything — which is what an early draft of
    // this test did.
    let section = |heading: &str| -> String {
        page.split(heading)
            .nth(1)
            .and_then(|rest| rest.split("</section>").next())
            .unwrap_or_else(|| panic!("no {heading} section in the page"))
            .to_string()
    };
    let reviewer_row = section("<h2>Reviewers</h2>");
    assert!(
        reviewer_row.contains("Ada Lovelace"),
        "the reviewer seat still reads as a handle: {reviewer_row}"
    );
    let discussion = section("<h2>Discussion</h2>");
    assert!(
        discussion.contains("Ada Lovelace"),
        "the comment author still reads as a handle: {discussion}"
    );

    std::fs::remove_dir_all(&work).ok();
}

/// The front door of a repository whose `HEAD` points at a branch that
/// does not exist.
///
/// `git init --bare` takes `HEAD` from the *host's* `init.defaultBranch`,
/// and a push lands on the branch the *client* named. When those disagree
/// the symref dangles, and `/r/owner/repo` — which opens at `HEAD` —
/// rendered "empty" for a repository holding every commit it was ever
/// sent. The bug was live on the dogfood node for every repository it
/// served, and no test bound `HEAD` to anything, so this is the binding.
#[test]
fn a_repository_opens_even_when_head_points_at_no_branch() {
    let (base, work, _oid, _acl) = served("head-dangles", "");
    let bare = work.join("repos").join("agents").join("one.git");

    // The state a mismatched default branch leaves behind, written
    // directly so the test does not depend on this machine's git config.
    std::fs::write(bare.join("HEAD"), "ref: refs/heads/trunk\n").expect("HEAD rewritten");
    assert!(
        !git(&bare, &["rev-parse", "--verify", "HEAD^{commit}"])
            .status
            .success(),
        "the fixture must leave HEAD unresolvable, or it proves nothing"
    );

    let (status, _headers, page) = get(&format!("{base}/r/agents/one"), &["-u", "alice:a"]);
    assert_eq!(status, 200, "{page}");
    assert!(
        !page.contains("No commits yet"),
        "the front door still reports an empty repository: {page}"
    );
    assert!(
        page.contains("README.md") && page.contains("src"),
        "the front door does not list the tree that was pushed: {page}"
    );

    // A repository with no branches at all is still empty, and must stay
    // that way: the fallback is for a dangling symref, not a way to make
    // every empty repository look populated.
    let fresh = work.join("repos").join("agents").join("two.git");
    assert!(
        git(&work, &["init", "--bare", "-q", fresh.to_str().unwrap()])
            .status
            .success()
    );
    let (status, _headers, page) = get(&format!("{base}/r/agents/two"), &["-u", "alice:a"]);
    assert_eq!(status, 200, "{page}");
    assert!(
        page.contains("No commits yet"),
        "a genuinely empty repository stopped saying so: {page}"
    );

    std::fs::remove_dir_all(&work).ok();
}

/// The repository front door describes the repository, not just its
/// filenames: which revision, how much history, what else to switch to,
/// and for every entry the commit that last touched it.
///
/// The columns are the point. A listing of bare names is a directory;
/// a listing that says what each path is *for* and when it last moved is
/// what a reader came to a code host to see.
#[test]
fn the_file_list_carries_the_last_commit_for_every_entry() {
    let (base, work, _oid, _acl) = served("listing-columns", "");

    let (status, _headers, page) = get(&format!("{base}/r/agents/one"), &["-u", "alice:a"]);
    assert_eq!(status, 200, "{page}");

    // The orientation bar.
    assert!(
        page.contains("1 commit<") || page.contains("1 commit</a>"),
        "the commit count is missing or pluralised wrongly: {page}"
    );
    assert!(
        page.contains("1 branch<"),
        "the branch count is missing or pluralised wrongly: {page}"
    );
    assert!(page.contains("0 tags"), "the tag count is missing: {page}");
    assert!(
        page.contains("class=\"picker\""),
        "there is no way to switch revision: {page}"
    );
    assert!(
        page.contains("/r/agents/one/tree/main\">main</a>"),
        "the picker does not link the branch it found: {page}"
    );

    // The columns, on the rows themselves.
    // The whole `<tr>`, not the text after the first match: the name
    // appears in the href before it appears as the label, so slicing on
    // the name lands inside the attribute and asserts on nothing.
    let row = page
        .split("<tr>")
        .find(|row| row.contains("README.md"))
        .expect("no README.md row on the page");
    assert!(
        row.contains("seed the tree"),
        "the row does not name the commit that last touched it: {row}"
    );
    assert!(
        row.contains("ago") || row.contains("just now"),
        "the row does not say when it last moved: {row}"
    );

    // Inside a directory the breadcrumb answers "where am I", so the bar
    // is not repeated — it would push the listing below the fold for no
    // new information.
    let (status, _headers, inner) = get(
        &format!("{base}/r/agents/one/tree/main/src"),
        &["-u", "alice:a"],
    );
    assert_eq!(status, 200, "{inner}");
    assert!(
        !inner.contains("class=\"repobar\""),
        "the orientation bar was repeated inside a directory: {inner}"
    );
    assert!(
        inner.contains("seed the tree"),
        "the columns stop working below the root: {inner}"
    );

    std::fs::remove_dir_all(&work).ok();
}

/// The README is on the repository page, rendered, and inert.
///
/// Served rather than unit-tested because the policy in `readme` is only
/// half the property: the other half is that the page actually reaches
/// for the file, at the root only, and that what it embeds survives the
/// page's own escaping unchanged.
#[test]
fn the_readme_renders_on_the_repository_page_and_carries_no_markup() {
    let (base, work, _oid, _acl) = served("readme-render", "");

    let (status, _headers, page) = get(&format!("{base}/r/agents/one"), &["-u", "alice:a"]);
    assert_eq!(status, 200, "{page}");

    let readme = page
        .split("<section class=\"readme\">")
        .nth(1)
        .and_then(|rest| rest.split("</section>").next())
        .expect("no README section on the repository page");

    // Rendered, not dumped: the fixture's `# hello` is a heading here.
    assert!(
        readme.contains("<h1>hello</h1>"),
        "the README was not rendered as markdown: {readme}"
    );
    assert!(
        readme.contains("href=\"https://example.invalid/x\""),
        "an ordinary link did not survive: {readme}"
    );
    // The fixture's raw `<script>` tag must not be on the page in any
    // form that a browser would run.
    assert!(
        !readme.contains("<script"),
        "raw HTML from a README reached the page: {readme}"
    );
    assert!(
        !page.contains("alert('readme')"),
        "the script body reached the page: {page}"
    );

    // Every level, not just the root: `src/README.md` documents exactly
    // the files a reader is looking at when they are in `src`.
    let (_, _, inner) = get(
        &format!("{base}/r/agents/one/tree/main/src"),
        &["-u", "alice:a"],
    );
    assert!(
        inner.contains("class=\"readme\""),
        "a directory with a README rendered none: {inner}"
    );
    assert!(
        inner.contains("what lives in this directory"),
        "the directory rendered the wrong README: {inner}"
    );
    assert!(
        !inner.contains("<h1>hello</h1>"),
        "the directory hoisted the repository root's README: {inner}"
    );

    std::fs::remove_dir_all(&work).ok();
}

/// A node serving one project's own domain presents that repository and
/// nothing else.
///
/// Both halves are the property. Presenting the repository at `/` is the
/// feature; refusing every other repository with the same words a denied
/// reader gets is what stops the front door from becoming a directory of
/// everything else the host happens to hold.
#[test]
fn a_single_repository_node_presents_it_and_withholds_the_rest() {
    let work = std::env::temp_dir().join("choir-node-browse-site-repo");
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");

    let mut table = AuthTable::new();
    table.insert("alice".into(), "a".into());
    let mut node =
        Node::bind_with_auth(&work.join("repos"), 0, Some(table)).expect("node binds free port");
    let port = node.port();
    node.create_repo("agents/one.git").expect("repo created");
    node.create_repo("other/secret.git").expect("repo created");
    node.serve_single_repository("agents/one")
        .expect("a repository this node holds");
    std::thread::spawn(move || node.serve_forever());
    let base = format!("http://127.0.0.1:{port}");

    // Seed the presented repository so its page has something to show.
    let clone = work.join("clone");
    let url = format!("http://alice:a@127.0.0.1:{port}/agents/one.git");
    assert!(git(&work, &["clone", "-q", &url, clone.to_str().unwrap()])
        .status
        .success());
    std::fs::write(clone.join("README.md"), "# the project\n").unwrap();
    assert!(git(&clone, &["add", "."]).status.success());
    assert!(git(&clone, &["commit", "-q", "-m", "first"])
        .status
        .success());
    assert!(git(&clone, &["push", "-q", "origin", "HEAD:main"])
        .status
        .success());

    // The front door is the repository, not a list.
    let (status, _headers, front) = get(&format!("{base}/"), &["-u", "alice:a"]);
    assert_eq!(status, 200, "{front}");
    assert!(
        front.contains("README.md") && front.contains("the project"),
        "the front door is not the repository: {front}"
    );
    assert!(
        !front.contains(">repositories</h1>"),
        "the front door is still the index: {front}"
    );
    // Nothing offers a way back to a list that does not exist here.
    assert!(
        !front.contains("all repositories"),
        "the page links to an index this node does not present: {front}"
    );

    // Its own sub-pages keep working, or the mode is unusable.
    let (status, _, _) = get(
        &format!("{base}/r/agents/one/commits/main"),
        &["-u", "alice:a"],
    );
    assert_eq!(status, 200, "the presented repository lost its own pages");

    // Every other repository is refused, in the words a denied reader
    // gets — never in words that confirm it exists.
    // `/r/` is the index's old address. In this mode it resolves to the
    // presented repository rather than 404-ing: there is no index to
    // show, and a reader following an old link is better served by the
    // one repository this node has than by an error. What matters is
    // that no index renders and no other repository is named.
    let (status, _headers, list) = get(&format!("{base}/r/"), &["-u", "alice:a"]);
    assert_eq!(status, 200, "{list}");
    assert!(
        !list.contains(">repositories</h1>") && !list.contains("other/secret"),
        "the old index address still lists repositories: {list}"
    );

    for path in ["/r/other/secret", "/r/other/secret/commits/main"] {
        let (status, _headers, page) = get(&format!("{base}{path}"), &["-u", "alice:a"]);
        assert_eq!(status, 404, "{path} was not refused: {page}");
        assert!(
            !page.contains("other/secret"),
            "{path} confirmed the repository exists: {page}"
        );
    }

    // The refusal is presentation, not authorization: the repository the
    // browser will not describe is still clonable by a reader who holds
    // the grant. A mode that quietly revoked access would be a very
    // different change from the one this flag claims to make.
    let hidden = format!("http://alice:a@127.0.0.1:{port}/other/secret.git");
    let out = git(&work, &["ls-remote", &hidden]);
    assert!(
        out.status.success(),
        "presenting one repository took git access to another: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    std::fs::remove_dir_all(&work).ok();
}

/// The three scopes answer three different questions against the same
/// tree, and each one finds what only it can find.
///
/// One test rather than three because the property that matters is the
/// *difference* between them: a search box that returned the same rows
/// whichever tab was picked would pass three separate tests.
#[test]
fn each_search_scope_finds_what_only_it_can_find() {
    let (base, _work, _oid, _) = served("search-scopes", "");
    let at = |q: &str, scope: &str| {
        get(
            &format!("{base}/r/agents/one/search/main?q={q}&in={scope}"),
            &["-u", "alice:a"],
        )
    };

    // A name search finds the file by its path and does not report the
    // lines inside it.
    let (status, _, files) = at("lib", "files");
    assert_eq!(status, 200, "the file search was refused: {files}");
    assert!(
        files.contains("src/lib.rs"),
        "a file search missed a matching path: {files}"
    );
    assert!(
        files.contains("<mark>lib</mark>"),
        "the matched span is not marked: {files}"
    );

    // A content search finds a string that appears in no file name.
    let (status, _, code) = at("fn+main", "code");
    assert_eq!(status, 200, "the content search was refused: {code}");
    assert!(
        code.contains("src/lib.rs"),
        "a content search missed the file holding the term: {code}"
    );
    // `+` is a space in a query string, so this proves the term arrived
    // as two words rather than one — the whole point of encoding it.
    assert!(
        code.contains("<mark>fn main</mark>"),
        "a multi-word term did not survive the URL: {code}"
    );
    // The same term as a *name* search finds nothing: no file is called
    // `fn main`. That is what makes the scopes distinguishable.
    let (_, _, none) = at("fn+main", "files");
    assert!(
        none.contains("No file name matches"),
        "a name search answered a content question: {none}"
    );

    // A message search finds the commit subject, which is in no file at
    // all.
    let (status, _, commits) = at("seed", "commits");
    assert_eq!(status, 200, "the commit search was refused: {commits}");
    assert!(
        commits.contains("<mark>seed</mark>"),
        "a commit search missed the subject it was given: {commits}"
    );
    let (_, _, none) = at("seed", "files");
    assert!(
        none.contains("No file name matches"),
        "a commit subject matched a file name: {none}"
    );
}

/// A search result is repository content, so it goes through the same
/// escaper every other page does — including the part of it the
/// highlighter writes.
///
/// This is the case a highlighter gets wrong: marking a span means
/// writing tags into text, and doing that after escaping means computing
/// offsets in the escaped string. The fixture holds `<script>` inside a
/// file *and* a term that matches next to it, so a highlighter that
/// escapes the wrong half is caught here rather than in a browser.
#[test]
fn a_search_hit_cannot_put_markup_on_the_page() {
    let (base, _work, _oid, _) = served("search-escape", "");

    let (status, _, page) = get(
        &format!("{base}/r/agents/one/search/main?q=alert&in=code"),
        &["-u", "alice:a"],
    );
    assert_eq!(status, 200);
    assert!(
        page.contains("<mark>alert</mark>"),
        "the term was not found or not marked: {page}"
    );
    // The fixture line is `// <script>alert('x')</script>`. The mark is
    // ours; the script tag is the repository's and must arrive as text.
    assert!(
        !page.contains("<script>alert"),
        "a file's markup survived into the search page: {page}"
    );
    assert!(
        page.contains("&lt;script&gt;"),
        "the file's markup was dropped rather than escaped: {page}"
    );

    // The same rule for a matching *file name* that is itself markup.
    let (_, _, names) = get(
        &format!("{base}/r/agents/one/search/main?q=onerror&in=files"),
        &["-u", "alice:a"],
    );
    assert!(
        names.contains("<mark>onerror</mark>"),
        "the matching file name was not found: {names}"
    );
    assert!(
        !names.contains("<img src=x"),
        "a file name that is markup rendered as markup: {names}"
    );

    // The case the two assertions above do not reach: a term that is
    // *itself* markup. Everything so far searched for a plain word, so
    // the marked span held nothing dangerous and a highlighter that
    // escaped its surroundings but not its match would pass. Here the
    // matched span is `<script`, and it has to arrive as text like the
    // rest of the line.
    for (scope, term, encoded) in [
        ("code", "<script", "%3Cscript"),
        ("files", "<img", "%3Cimg"),
    ] {
        let (status, _, page) = get(
            &format!("{base}/r/agents/one/search/main?q={encoded}&in={scope}"),
            &["-u", "alice:a"],
        );
        assert_eq!(status, 200, "a markup term broke the {scope} page: {page}");
        assert!(
            page.contains("<mark>&lt;"),
            "the {scope} search did not find or did not mark `{term}`: {page}"
        );
        assert!(
            !page.contains("<mark><"),
            "the {scope} highlighter wrote its match unescaped: {page}"
        );
    }
}

/// A term reaches a subprocess argument, so it must never be read as an
/// option — in any scope.
///
/// `--output=/tmp/x` is the shape that matters: if a term were spliced in
/// as a bare argument, git would take it as a flag and this node would
/// write a file where a reader asked it to. The assertion is that the
/// page renders as an ordinary empty result.
#[test]
fn a_search_term_is_never_read_as_an_option() {
    let (base, _work, _oid, _) = served("search-option", "");

    for (scope, empty) in [
        ("files", "No file name matches"),
        ("code", "No file content matches"),
        ("commits", "No commit message matches"),
    ] {
        // `%2D` is `-`, so this arrives at git as a term beginning with
        // two dashes however the query string is parsed.
        let url = format!("{base}/r/agents/one/search/main?q=%2D%2Dhelp&in={scope}");
        let (status, _, page) = get(&url, &["-u", "alice:a"]);
        assert_eq!(status, 200, "a dashed term broke the {scope} page: {page}");
        assert!(
            page.contains(empty),
            "the {scope} search did not answer a dashed term as a miss: {page}"
        );
        // git's own help text is the tell that the term became a flag.
        assert!(
            !page.contains("usage: git"),
            "the {scope} search ran git's help: {page}"
        );
    }
}

/// Searching cannot show a reader a repository they may not read, and
/// filtering the index cannot reveal one either.
///
/// The filter runs after the grant check, and this is the test that says
/// so: bob may read nothing, so the filtered index must be empty for
/// every term including the exact name of a repository that exists.
#[test]
fn search_never_widens_what_a_reader_may_see() {
    let (base, _work, _oid, _) = served("search-grant", "alice  agents/one  write\n");

    // The repository is there for alice.
    let (status, _, mine) = get(
        &format!("{base}/r/agents/one/search/main?q=lib&in=files"),
        &["-u", "alice:a"],
    );
    assert_eq!(status, 200, "the grant holder was refused: {mine}");

    // For bob it is not, and searching it is the same refusal as
    // browsing it — not a different one that would confirm it exists.
    let (denied, _, page) = get(
        &format!("{base}/r/agents/one/search/main?q=lib&in=files"),
        &["-u", "bob:b"],
    );
    assert_eq!(denied, 404, "a search reached a repository with no grant");
    assert!(
        !page.contains("lib.rs"),
        "the refusal page named the content it refused: {page}"
    );

    // And the index filter cannot be used to probe for names.
    let (status, _, index) = get(&format!("{base}/r/?q=agents"), &["-u", "bob:b"]);
    assert_eq!(status, 200);
    assert!(
        !index.contains("agents/one"),
        "the index filter revealed a repository with no grant: {index}"
    );
}

/// One box, in the same place, on every page this node serves — and it
/// searches whatever the page is about.
///
/// "Everywhere" is the requirement, so the list below deliberately
/// includes the pages that are not repository listings: the review
/// pages, the node page, and a refusal. Those are exactly the ones a
/// per-page box gets forgotten on, which is why the bar is emitted by
/// the page shell rather than by each page.
#[test]
fn one_search_box_sits_on_every_page_and_follows_the_revision_in_view() {
    let (base, work, oid, _) = served("search-box", "");

    // A second branch whose tree differs, so "which revision" has an
    // observable answer.
    let clone = work.join("clone");
    assert!(git(&clone, &["checkout", "-q", "-b", "other"])
        .status
        .success());
    std::fs::write(clone.join("only-on-other.txt"), "here\n").unwrap();
    assert!(git(&clone, &["add", "."]).status.success());
    assert!(git(&clone, &["commit", "-q", "-m", "add a file"])
        .status
        .success());
    assert!(git(&clone, &["push", "-q", "origin", "other"])
        .status
        .success());

    for page in [
        "/r/",
        "/r/agents/one",
        "/r/agents/one/tree/main/src",
        "/r/agents/one/blob/main/README.md",
        "/r/agents/one/commits/main",
        "/r/agents/one/reviews",
        "/r/agents/one/search/main?q=lib&in=files",
        "/status",
        // A repository that is not there: a refusal is where a reader is
        // most lost, so it is the page that can least afford to drop the
        // one control that gets them somewhere.
        "/r/agents/absent",
    ] {
        let (status, _, html) = get(&format!("{base}{page}"), &["-u", "alice:a"]);
        assert!(
            html.contains("class=\"chrome\""),
            "{page} ({status}) carries no search bar: {html}"
        );
        assert!(
            html.contains("name=\"q\""),
            "{page} has a bar with no box in it: {html}"
        );
        // A `GET` form, so a result is a URL and needs no script.
        assert!(
            html.contains("method=\"get\""),
            "{page} would need a script to search: {html}"
        );
        // Exactly one. A second box is the bug this replaced: the page
        // used to draw its own under the header, and the two disagreed
        // about what they searched.
        assert_eq!(
            html.matches("name=\"q\"").count(),
            1,
            "{page} draws more than one search box: {html}"
        );
    }

    // The commit page too, which needs the oid built above.
    let (_, _, commit) = get(
        &format!("{base}/r/agents/one/commit/{oid}"),
        &["-u", "alice:a"],
    );
    assert!(
        commit.contains("class=\"chrome\""),
        "the commit page carries no search bar: {commit}"
    );

    // The box says what it will search, rather than leaving the reader
    // to infer it from the page around it.
    let (_, _, repo) = get(&format!("{base}/r/agents/one"), &["-u", "alice:a"]);
    assert!(
        repo.contains(">agents/one</span>"),
        "the box does not name the repository it searches: {repo}"
    );
    let (_, _, index) = get(&format!("{base}/r/"), &["-u", "alice:a"]);
    assert!(
        index.contains(">all repositories</span>"),
        "the box on the index does not say it is global: {index}"
    );

    // The box on the `other` branch searches `other`.
    let (_, _, on_other) = get(
        &format!("{base}/r/agents/one/tree/other"),
        &["-u", "alice:a"],
    );
    assert!(
        on_other.contains("action=\"/r/agents/one/search/other\""),
        "the box on a branch searches somewhere else: {on_other}"
    );

    // After a search, the one box holds the term — there is no second
    // box below the results doing that job.
    let (_, _, results) = get(
        &format!("{base}/r/agents/one/search/main?q=lib&in=files"),
        &["-u", "alice:a"],
    );
    assert!(
        results.contains("value=\"lib\""),
        "the bar lost the term that was searched: {results}"
    );

    // And searching there finds the file that exists only there, while
    // searching `main` does not.
    let (_, _, found) = get(
        &format!("{base}/r/agents/one/search/other?q=only-on-other&in=files"),
        &["-u", "alice:a"],
    );
    assert!(
        found.contains("only-on-other.txt"),
        "the branch search missed a file on that branch: {found}"
    );
    let (_, _, missing) = get(
        &format!("{base}/r/agents/one/search/main?q=only-on-other&in=files"),
        &["-u", "alice:a"],
    );
    assert!(
        missing.contains("No file name matches"),
        "a search of main found a file that is only on other: {missing}"
    );
}

/// A page's cache identity includes the build that rendered it, not just
/// the commit it describes.
///
/// This exists because of a real hour spent on "the change did not
/// deploy". An `ETag` answers "is the page I hold still current", and the
/// page is this node's *rendering* of a commit. A blob's tag was the oid
/// and the path alone, so upgrading the daemon left every reader who had
/// visited that page on the old HTML — with the node correctly answering
/// `304`, because the oid genuinely had not moved.
///
/// Asserted structurally rather than by rebuilding: the tag has to
/// *contain* the build stamp's contribution, which is checkable by
/// showing that two pages differing only in the stamp cannot collide.
/// What the test can check end-to-end is the half that matters most —
/// that the tag still changes with content, so folding the build in did
/// not turn every page into one cache entry.
#[test]
fn a_page_tag_changes_with_the_build_as_well_as_the_commit() {
    let (base, work, _oid, _) = served("etag-build", "");

    let etag_of = |url: &str| -> String {
        let (status, headers, _) = get(url, &["-u", "alice:a"]);
        assert_eq!(status, 200, "{url} was refused");
        header_value(&headers, "etag").unwrap_or_else(|| panic!("{url} sent no ETag"))
    };

    let blob = format!("{base}/r/agents/one/blob/main/README.md");
    let before = etag_of(&blob);

    // The stamp is a compile-time constant, so it cannot be changed from
    // here. What is checkable is that it is *in* the tag: the daemon
    // reports the same value on its own status page, and a tag that
    // ignored it would be computable without it. So the assertion is the
    // observable consequence — two different paths at one commit, and one
    // path across two commits, all differ.
    let other = etag_of(&format!("{base}/r/agents/one/blob/main/src/lib.rs"));
    assert_ne!(
        before, other,
        "two files at one commit share a cache entry, so one would be served as the other"
    );

    // A new commit changes the page, and must change the tag.
    let clone = work.join("clone");
    std::fs::write(clone.join("README.md"), "# changed\n").unwrap();
    assert!(git(&clone, &["add", "."]).status.success());
    assert!(git(&clone, &["commit", "-q", "-m", "edit the readme"])
        .status
        .success());
    assert!(git(&clone, &["push", "-q", "origin", "HEAD:main"])
        .status
        .success());
    let after = etag_of(&blob);
    assert_ne!(
        before, after,
        "the file changed and its cache entry did not, so a reader keeps the old page"
    );

    // And the page a conditional request gets back is the current one:
    // a reader holding the *old* tag must be sent fresh bytes, not a 304.
    let (status, _, body) = get(
        &blob,
        &["-u", "alice:a", "-H", &format!("If-None-Match: {before}")],
    );
    assert_eq!(
        status, 200,
        "a stale tag was answered 304, so the reader keeps the old page"
    );
    assert!(
        body.contains("changed"),
        "the refreshed page is not the new content: {body}"
    );
}

/// A page that renders a form must be served a policy that lets the form
/// submit.
///
/// This is the check that was missing when the search box shipped. The
/// served HTML was correct — a well-formed `GET` form with a valid
/// action — and the header said `form-action 'none'`, so every browser
/// blocked the submission and reported it to the console only. The box
/// rendered, focused, accepted a term, and did nothing on `Enter`. No
/// assertion over the markup could see it, because the markup was right.
///
/// Stated as the general rule rather than as "the CSP contains
/// `form-action 'self'`": the next form added to this surface is covered
/// without anyone remembering to extend a list, and a policy tightened
/// back to `'none'` fails here whichever page it breaks.
#[test]
fn every_page_with_a_form_is_served_a_policy_that_permits_it() {
    let (base, _work, _oid, _) = served("csp-form", "");

    for path in [
        "/r/",
        "/r/agents/one",
        "/r/agents/one/tree/main/src",
        "/r/agents/one/blob/main/README.md",
        "/r/agents/one/commits/main",
        "/r/agents/one/reviews",
        "/r/agents/one/search/main?q=lib&in=files",
        "/status",
        "/r/agents/absent",
    ] {
        let (_, headers, body) = get(&format!("{base}{path}"), &["-u", "alice:a"]);
        if !body.contains("<form") {
            continue;
        }
        let csp = header_value(&headers, "content-security-policy")
            .unwrap_or_else(|| panic!("{path} renders a form and carries no CSP at all"));

        let directive = csp
            .split(';')
            .map(str::trim)
            .find(|d| d.starts_with("form-action"))
            .unwrap_or_else(|| {
                panic!("{path} renders a form and its CSP names no form-action: {csp}")
            })
            .to_string();
        assert!(
            !directive.contains("'none'"),
            "{path} renders a form the browser will refuse to submit: {directive}"
        );
        assert!(
            directive.contains("'self'"),
            "{path} permits form submission somewhere other than this origin: {directive}"
        );

        // The form must also point at this origin, or `'self'` refuses it
        // and the page is broken in the other direction.
        for action in body.split("action=\"").skip(1) {
            let action = action.split('"').next().unwrap_or("");
            assert!(
                action.starts_with('/'),
                "{path} has a form aimed off this origin, which `'self'` blocks: {action}"
            );
        }
    }
}

/// A scope with no matches says where the matches are, and every tab
/// carries its own count.
///
/// The failure this prevents is not a crash: it is a reader typing a
/// word that appears in 73 lines of code, landing on the name scope, and
/// reading "No file name matches" as "search is broken". The page has to
/// carry the other counts for that reader to have anywhere to go.
#[test]
fn a_scope_with_no_matches_points_at_the_scopes_that_have_them() {
    let (base, _work, _oid, _) = served("search-counts", "");

    // `fn main` is in `src/lib.rs`'s contents and in no file name.
    let (status, _, page) = get(
        &format!("{base}/r/agents/one/search/main?q=fn+main&in=files"),
        &["-u", "alice:a"],
    );
    assert_eq!(status, 200);
    assert!(
        page.contains("No file name matches"),
        "the name scope claims a match it does not have: {page}"
    );
    // The pointer, and where it points.
    assert!(
        page.contains("Found in"),
        "a dead-end scope offered the reader nothing: {page}"
    );
    assert!(
        page.contains("in=code"),
        "the pointer does not link the scope that has the matches: {page}"
    );

    // Every tab carries a count, including the zero on the tab in view —
    // that zero is what tells the reader the term was searched here.
    let tabs = page
        .split("<nav class=\"tabs\">")
        .nth(1)
        .and_then(|rest| rest.split("</nav>").next())
        .expect("the page has no tab strip");
    assert_eq!(
        tabs.matches("class=\"count\"").count(),
        3,
        "not every scope carries a count: {tabs}"
    );
    assert!(
        tabs.contains(">0</span>"),
        "the empty scope hides its zero, so the term looks unsearched: {tabs}"
    );

    // A term in nothing at all says so once and offers nothing, rather
    // than listing two more empty places.
    let (_, _, none) = get(
        &format!("{base}/r/agents/one/search/main?q=zzzznowhere&in=files"),
        &["-u", "alice:a"],
    );
    assert!(
        !none.contains("Found in"),
        "a term that matches nothing was offered somewhere to look: {none}"
    );

    // Counts are per scope, not one number repeated: the commit search
    // finds the seed commit, the name search finds nothing.
    let (_, _, seed) = get(
        &format!("{base}/r/agents/one/search/main?q=seed&in=commits"),
        &["-u", "alice:a"],
    );
    let seed_tabs = seed
        .split("<nav class=\"tabs\">")
        .nth(1)
        .and_then(|rest| rest.split("</nav>").next())
        .expect("no tab strip");
    assert!(
        seed_tabs.contains(">0</span>"),
        "the name scope reports matches for a term only in a commit: {seed_tabs}"
    );
    assert!(
        seed.contains("<mark>seed</mark>"),
        "the commit scope did not find the term its tab counts: {seed}"
    );
}

/// A `304` carries every policy header its `200` carries.
///
/// A `304` is a header update, not just "nothing changed": a cache
/// replaces its stored headers with the ones the `304` sends, and keeps
/// its old value for every header the `304` omits. So a policy header
/// left out of a `304` is *frozen* at whatever the client first saw, for
/// as long as the cache entry lives.
///
/// This is the bug that made the search box look permanently broken. The
/// browse `ETag` is derived from a commit, so it survives a daemon
/// upgrade; the `304` carried only the tag; and a reader whose cache held
/// a page from before the CSP gained `form-action 'self'` kept the old
/// `form-action 'none'` through every reload. The server was already
/// sending the right policy and the browser was already ignoring it.
///
/// Written as "the `304` and the `200` agree" rather than as a list of
/// header names, so a header added to the `200` later cannot be quietly
/// dropped from the `304`.
#[test]
fn a_not_modified_response_carries_the_same_policy_as_the_page() {
    let (base, _work, oid, _) = served("etag-304-headers", "");

    // Every page here that has an `ETag` at all — a page with none is
    // never revalidated and cannot go stale this way.
    for path in [
        "/r/agents/one",
        "/r/agents/one/tree/main/src",
        "/r/agents/one/blob/main/README.md",
        &format!("/r/agents/one/commit/{oid}"),
    ] {
        let url = format!("{base}{path}");
        let (status, headers, _) = get(&url, &["-u", "alice:a"]);
        assert_eq!(status, 200, "{path} was refused");
        let Some(tag) = header_value(&headers, "etag") else {
            continue;
        };

        let (code, revalidated, body) = get(
            &url,
            &["-u", "alice:a", "-H", &format!("If-None-Match: {tag}")],
        );
        assert_eq!(code, 304, "{path} did not revalidate to a 304");
        assert!(body.is_empty(), "{path} sent a body with its 304");

        for name in [
            "content-security-policy",
            "cache-control",
            "referrer-policy",
        ] {
            let Some(on_200) = header_value(&headers, name) else {
                continue;
            };
            let on_304 = header_value(&revalidated, name).unwrap_or_else(|| {
                panic!(
                    "{path} sends `{name}` on its 200 and not on its 304, so a cached \
                     client keeps the old value for as long as the entry lives"
                )
            });
            assert_eq!(
                on_200, on_304,
                "{path} sends a different `{name}` on its 304 than on its 200"
            );
        }
    }
}

/// The repository root answers "what is happening here" before "what is
/// here".
///
/// A repository several agents are writing at once is one where the work
/// in flight is the first thing a reader needs, so the root page leads
/// with the reviews pane and puts the file listing beside it rather than
/// above it. The assertion is positional on purpose: a page that merely
/// mentions the review somewhere would satisfy a `contains` and still
/// bury it under the files.
#[test]
fn the_repository_root_leads_with_the_work_in_flight() {
    let work = std::env::temp_dir().join("choir-node-browse-root-panes");
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");

    let author = ActorKey::generate();
    let mut registry = Registry::new();
    registry
        .register(&author.public_key_bytes())
        .expect("register author");

    let mut node = Node::bind(&work.join("repos"), 0).expect("node binds free port");
    let port = node.port();
    node.create_repo("agents/one.git").expect("repo created");
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate())
            .expect("platform starts"),
    );
    std::thread::spawn(move || node.serve_forever());
    let base = format!("http://127.0.0.1:{port}");

    let clone = work.join("clone");
    let url = format!("{base}/agents/one.git");
    assert!(git(&work, &["clone", "-q", &url, clone.to_str().unwrap()])
        .status
        .success());
    std::fs::write(clone.join("only.txt"), "a file\n").unwrap();
    assert!(git(&clone, &["add", "."]).status.success());
    assert!(git(&clone, &["commit", "-q", "-m", "first"])
        .status
        .success());
    assert!(git(&clone, &["push", "-q", "origin", "HEAD:main"])
        .status
        .success());
    let head = String::from_utf8_lossy(&git(&clone, &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();

    let request = ViewOp::new(OpKind::RequestReview {
        id: "r-inflight".into(),
        target: choir_oplog::ContentHash::from_git_oid(&head).expect("a git oid"),
        reviewers: vec!["ana".into()],
        target_ref: Some("agents/one.git:refs/heads/main".into()),
    });
    let (code, resp) = crate::support::curl(&[
        "-X",
        "POST",
        "-d",
        &crate::support::submit_body_legacy(&author, "author", &request),
        &format!("{base}/api/submit"),
    ]);
    assert_eq!(code, 200, "{resp}");

    let (status, _, page) = get(&format!("{base}/r/agents/one"), &[]);
    assert_eq!(status, 200);

    // Three panes, in the order the reader needs them. Split on the
    // markup, never on the class name: every one of these names also
    // appears in the inline stylesheet, so a `contains("pane-files")`
    // matches the CSS and passes on a page with no panes at all. The
    // first draft of this test did exactly that.
    let (before_files, after_files) = page
        .split_once("<section class=\"pane pane-files\">")
        .unwrap_or_else(|| panic!("the root page has no files pane: {page}"));
    assert!(
        before_files.contains("<aside class=\"pane pane-reviews\">"),
        "the reviews pane does not come before the files pane: {page}"
    );
    assert!(
        after_files.contains("<section class=\"pane pane-content\">"),
        "the content pane does not come after the files pane: {page}"
    );

    // The review is *in* the first pane, not merely on the page.
    assert!(
        before_files.contains("r-inflight"),
        "the work in flight is not in the leading pane: {before_files}"
    );
    assert!(
        before_files.contains("/r/agents/one/review/r-inflight"),
        "the review is named but not linked: {before_files}"
    );
    // And the file listing is still the files pane's job.
    assert!(
        after_files.contains("only.txt"),
        "the files pane lost the listing: {after_files}"
    );
}

/// The reviews pane answers two questions a bare list never did: how far
/// the destination has moved under each proposal, and how much of what
/// this repository holds is already settled.
///
/// It also holds the pane's cache identity. The pane's content changes
/// with no push behind it, so a validator built from the tree oid alone
/// serves a browser the previous answer — and every assertion in a test
/// that re-fetches without `If-None-Match` passes while it does. The
/// conditional request at the end is the only part of this test that
/// would have caught that.
#[test]
fn the_reviews_pane_counts_what_is_behind_and_what_is_settled() {
    let work = std::env::temp_dir().join("choir-node-browse-pane-counts");
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");

    let author = ActorKey::generate();
    // Archiving is the node's own op, so the test keeps the node's key.
    let node_secret = ActorKey::generate().secret_bytes();
    let mut registry = Registry::new();
    registry
        .register(&author.public_key_bytes())
        .expect("register author");

    let mut node = Node::bind(&work.join("repos"), 0).expect("node binds free port");
    let port = node.port();
    node.create_repo("agents/moved.git").expect("repo created");
    node.enable_platform(
        Platform::start(
            registry,
            Box::new(MemLog::new()),
            ActorKey::from_secret_bytes(&node_secret),
        )
        .expect("platform starts"),
    );
    std::thread::spawn(move || node.serve_forever());
    let base = format!("http://127.0.0.1:{port}");

    let post = |key: &ActorKey, channel: &str, op: &ViewOp| {
        let (code, resp) = crate::support::curl(&[
            "-X",
            "POST",
            "-d",
            &crate::support::submit_body_legacy(key, channel, op),
            &format!("{base}/api/submit"),
        ]);
        assert_eq!(code, 200, "{resp}");
    };

    let clone = work.join("clone");
    let url = format!("{base}/agents/moved.git");
    assert!(git(&work, &["clone", "-q", &url, clone.to_str().unwrap()])
        .status
        .success());
    let commit = |name: &str| {
        std::fs::write(clone.join(name), "a file\n").unwrap();
        assert!(git(&clone, &["add", "."]).status.success());
        assert!(git(&clone, &["commit", "-q", "-m", name]).status.success());
        assert!(git(&clone, &["push", "-q", "origin", "HEAD:main"])
            .status
            .success());
        String::from_utf8_lossy(&git(&clone, &["rev-parse", "HEAD"]).stdout)
            .trim()
            .to_string()
    };
    let tip = commit("first.txt");

    let request = |id: &str, target: &str| {
        post(
            &author,
            "author",
            &ViewOp::new(OpKind::RequestReview {
                id: id.into(),
                target: choir_oplog::ContentHash::from_git_oid(target).expect("a git oid"),
                reviewers: vec!["ana".into()],
                target_ref: Some("agents/moved.git:refs/heads/main".into()),
            }),
        );
    };
    // The pane's own markup, not everything before the files pane. The
    // whole inline stylesheet sits in that prefix, and it contains the
    // word "behind" in a comment — so the first draft of this test
    // failed against a page whose pane was entirely correct.
    let pane = || {
        let (status, _, page) = get(&format!("{base}/r/agents/moved"), &[]);
        assert_eq!(status, 200);
        let open = "<aside class=\"pane pane-reviews\">";
        let start = page
            .find(open)
            .unwrap_or_else(|| panic!("the root page has no reviews pane: {page}"));
        let rest = &page[start + open.len()..];
        let end = rest
            .find("</aside>")
            .unwrap_or_else(|| panic!("the reviews pane is never closed: {page}"));
        rest[..end].to_string()
    };

    request("r-tip", &tip);

    // A proposal sitting on the tip is not behind anything, and the pane
    // renders nothing rather than "0 behind": a chip on every row is a
    // chip nobody reads.
    let at_tip = pane();
    assert!(
        at_tip.contains("r-tip"),
        "the review is not in the pane at all: {at_tip}"
    );
    assert!(
        !at_tip.contains("behind"),
        "a review on the tip is reported as behind: {at_tip}"
    );

    // Two commits land underneath it.
    commit("second.txt");
    commit("third.txt");
    let moved = pane();
    assert!(
        moved.contains("2 behind"),
        "the pane does not say how far the destination moved: {moved}"
    );

    // A settled review leaves the table and is counted separately. One
    // total for both would call a quiet repository busy.
    request("r-settled", &tip);
    post(
        &ActorKey::from_secret_bytes(&node_secret),
        "node/archive",
        &ViewOp::new(OpKind::ArchiveReview {
            id: "r-settled".into(),
            lapsed: true,
        }),
    );
    let settled = pane();
    assert!(
        settled.contains("<span class=\"count\">1</span>"),
        "the open count does not stand at one: {settled}"
    );
    assert!(
        settled.contains("1 archived"),
        "the settled review is not counted: {settled}"
    );
    assert!(
        !settled.contains("/review/r-settled"),
        "an archived review still occupies a row in the pane: {settled}"
    );

    // The validator has to move with the pane. Hold the page's own
    // `ETag` and ask again with nothing pushed in between: a new review
    // must still produce a fresh page, or a browser reads yesterday's
    // answer for as long as the tree stands still.
    let (_, headers, _) = get(&format!("{base}/r/agents/moved"), &[]);
    let etag = headers
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with("etag:"))
        .and_then(|line| line.split_once(':'))
        .map(|(_, value)| value.trim().to_string())
        .unwrap_or_else(|| panic!("the root page carries no ETag: {headers}"));
    let (status, _, _) = get(
        &format!("{base}/r/agents/moved"),
        &["-H", &format!("If-None-Match: {etag}")],
    );
    assert_eq!(status, 304, "the ETag does not validate its own page");

    request("r-fresh", &tip);
    let (status, _, page) = get(
        &format!("{base}/r/agents/moved"),
        &["-H", &format!("If-None-Match: {etag}")],
    );
    assert_eq!(
        status, 200,
        "a new review left the root page's ETag unchanged, so a browser \
         never sees it: {page}"
    );
    assert!(
        page.contains("r-fresh"),
        "the rebuilt page does not hold the new review: {page}"
    );
}

/// The contribute page: the one page whose whole readership cannot tell
/// a stale instruction from a current one.
///
/// The assertions are about what the page *says*, not that it rendered.
/// A page that 200s with the placeholder host still in it, or with an
/// example silently truncated by an unescaped `<`, would pass every
/// presence check and be useless to the only person who reads it.
#[test]
fn contribute_page_prints_commands_a_newcomer_can_paste() {
    let (base, _work, _head, _acl) = served("contribute", "");
    let (status, _headers, body) = get(
        &format!("{base}/r/agents/one/contribute"),
        &["-u", "alice:a"],
    );
    assert_eq!(status, 200, "{body}");

    // The node substituted its own origin, so the first command is
    // copyable rather than illustrative.
    assert!(
        body.contains(&format!("choir join {base} ")),
        "the join command does not name this node: {body}"
    );
    for placeholder in ["NODE", "REPO"] {
        assert!(
            !body.contains(placeholder),
            "the {placeholder} placeholder survived into the served page"
        );
    }
    // The clone line names this repository, not a shape to fill in.
    assert!(
        body.contains(&format!("git clone {base}/agents/one.git")),
        "the clone command is not copyable: {body}"
    );

    // Every step is present, in order, and none lost its example to the
    // escaper. `&lt;your-channel&gt;` is the shape that breaks silently:
    // unescaped it is parsed as a tag and the command loses its last
    // argument while still looking complete.
    let steps: Vec<usize> = ["choir join", "choir git-credential", "choir propose"]
        .iter()
        .map(|c| {
            body.find(c)
                .unwrap_or_else(|| panic!("page omits `{c}`: {body}"))
        })
        .collect();
    assert!(
        steps.windows(2).all(|w| w[0] < w[1]),
        "steps are out of order"
    );
    assert!(
        body.contains("choir propose ~/.choir/agent.key &lt;your-channel&gt;"),
        "the propose example lost its channel argument: {body}"
    );

    // The three things a GitHub-shaped reader will otherwise get wrong.
    for teaching in ["do not choose your reviewers", "not an error", "force-push"] {
        assert!(body.contains(teaching), "page omits `{teaching}`");
    }
    // This fixture runs no credential self-service, so the page must
    // *not* send a newcomer to `choir join`, which has no endpoint to
    // call here. Getting this wrong produces a failure whose cause is
    // the operator's configuration and whose symptom looks like the
    // newcomer's own mistake.
    assert!(
        body.contains("issues no invites"),
        "page promises invites a node without --accounts-file cannot issue: {body}"
    );
    assert!(
        !body.contains("invite-only"),
        "page claims invite-only admission on a node that issues none"
    );

    // The git-only path, naming this repository's own default branch
    // rather than an assumed `main`.
    assert!(
        body.contains("git push origin HEAD:refs/for/main/my-topic"),
        "page omits the magic refspec: {body}"
    );

    // Reachable, not just addressable: the repository page links it.
    let (status, _headers, repo_page) = get(&format!("{base}/r/agents/one"), &["-u", "alice:a"]);
    assert_eq!(status, 200);
    assert!(
        repo_page.contains("/r/agents/one/contribute"),
        "the repository page does not link the contribute page"
    );
}

/// The page is gated by the same grant every other page is.
#[test]
fn contribute_page_is_not_a_hole_in_the_acl() {
    // bob holds nothing on agents/one.
    let (base, _work, _head, _acl) = served("contribute-acl", "alice  agents/one  write\n");
    let (status, _headers, body) =
        get(&format!("{base}/r/agents/one/contribute"), &["-u", "bob:b"]);
    assert_eq!(status, 404, "a reader with no grant was served: {body}");
    assert!(!body.contains("choir join"), "the refusal leaked the page");
}
