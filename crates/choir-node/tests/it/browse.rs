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
        git(&work, &["clone", "-q", &url, clone.to_str().unwrap()]).status.success(),
        "seeding clone failed"
    );
    std::fs::create_dir_all(clone.join("src")).unwrap();
    std::fs::write(clone.join("README.md"), "hello\nsecond line\n").unwrap();
    // Content that is markup, in a file whose name is also markup: both
    // halves reach the page, and both must arrive inert.
    std::fs::write(clone.join("src/lib.rs"), "// <script>alert('x')</script>\nfn main() {}\n").unwrap();
    std::fs::write(clone.join("src/<img src=x onerror=alert(1)>.txt"), "named like markup\n").unwrap();
    std::fs::write(clone.join("binary.dat"), [0u8, 1, 2, 3, 0, 9]).unwrap();
    assert!(git(&clone, &["add", "."]).status.success());
    assert!(git(&clone, &["commit", "-q", "-m", "seed the tree"]).status.success());
    let push = git(&clone, &["push", "-q", "origin", "HEAD:main"]);
    assert!(push.status.success(), "{}", String::from_utf8_lossy(&push.stderr));
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
    assert!(index.contains("agents/one"), "the index listed no repository: {index}");

    let (status, _, root) = get(&format!("{base}/r/agents/one"), &["-u", "alice:a"]);
    assert_eq!(status, 200);
    assert!(root.contains("README.md"), "the root listing is missing a file: {root}");
    assert!(root.contains("src"), "the root listing is missing a directory");

    let (status, _, dir) = get(&format!("{base}/r/agents/one/tree/main/src"), &["-u", "alice:a"]);
    assert_eq!(status, 200);
    assert!(dir.contains("lib.rs"), "the directory listing is missing its file: {dir}");

    let (status, _, file) =
        get(&format!("{base}/r/agents/one/blob/main/README.md"), &["-u", "alice:a"]);
    assert_eq!(status, 200);
    assert!(file.contains("second line"), "the file did not render: {file}");
}

/// A repository can hold anything, so file content is attacker input
/// with a byline. Both the content and the name have to arrive inert.
#[test]
fn file_content_and_file_names_cannot_inject() {
    let (base, _work, _oid, _) = served("inject", "");

    let (_, _, file) =
        get(&format!("{base}/r/agents/one/blob/main/src/lib.rs"), &["-u", "alice:a"]);
    assert!(file.contains("&lt;script&gt;"), "the escaper did not run: {file}");
    assert!(
        !file.contains("<script>alert"),
        "a file's contents reached the page as live markup"
    );

    let (_, _, dir) = get(&format!("{base}/r/agents/one/tree/main/src"), &["-u", "alice:a"]);
    assert!(
        !dir.contains("<img src=x onerror"),
        "a file name reached the page as live markup: {dir}"
    );
    assert!(dir.contains("&lt;img"), "the file named like markup vanished instead of escaping");

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
    let (status, _, page) =
        get(&format!("{base}/r/agents/one/blob/main/binary.dat"), &["-u", "alice:a"]);
    assert_eq!(status, 200);
    assert!(page.contains("Binary file"), "a binary blob was rendered as text: {page}");
}

/// History and one commit, including the diff a reader came for.
#[test]
fn history_and_a_commit_diff_render() {
    let (base, _work, oid, _) = served("history", "");

    let (status, _, log) = get(&format!("{base}/r/agents/one/commits/main"), &["-u", "alice:a"]);
    assert_eq!(status, 200);
    assert!(log.contains("seed the tree"), "the subject is missing: {log}");
    assert!(log.contains(&oid[..12]), "the commit is missing from its own history");

    let (status, _, commit) =
        get(&format!("{base}/r/agents/one/commit/{oid}"), &["-u", "alice:a"]);
    assert_eq!(status, 200);
    assert!(commit.contains("seed the tree"), "the commit page lost its subject");
    assert!(commit.contains("README.md"), "the diff names no file: {commit}");
    assert!(commit.contains("class=\"add\""), "the diff has no added lines");
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

    let (status, _, body) = get(&url, &["-u", "alice:a", "-H", &format!("If-None-Match: {tag}")]);
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
    let file = std::fs::File::options().write(true).open(&acl).expect("reopen acl");
    let ahead = std::time::SystemTime::now() + std::time::Duration::from_secs(1);
    file.set_times(std::fs::FileTimes::new().set_modified(ahead)).expect("stamp mtime");

    let (status, _, _) = get(&format!("{base}/r/agents/one"), &["-u", "bob:b"]);
    assert_eq!(status, 200, "a granted reader was still refused after a reload");
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
    assert!(page.contains("No commits yet"), "an empty repository read as broken: {page}");

    // Give it a commit before cloning. An empty clone stops after the
    // ref advertisement, so it never sends `POST .../git-upload-pack` —
    // which is the one request an owner named `r` shares a prefix with,
    // and therefore the only one that can prove the routing rule.
    let seed = work.join("r-seed");
    let url = format!("http://alice:a@{host}/r/project.git");
    assert!(git(&work, &["clone", "-q", &url, seed.to_str().unwrap()]).status.success());
    std::fs::write(seed.join("only.txt"), "content\n").unwrap();
    assert!(git(&seed, &["add", "."]).status.success());
    assert!(git(&seed, &["commit", "-q", "-m", "first"]).status.success());
    assert!(git(&seed, &["push", "-q", "origin", "HEAD:main"]).status.success());

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
    assert!(page.contains("only.txt"), "the pushed file is not listed: {page}");
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
    registry.register(&author.public_key_bytes()).expect("register author");

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
    assert!(git(&work, &["clone", "-q", &url, clone.to_str().unwrap()]).status.success());
    std::fs::write(clone.join("f.txt"), "base\n").unwrap();
    assert!(git(&clone, &["add", "."]).status.success());
    assert!(git(&clone, &["commit", "-q", "-m", "base"]).status.success());
    assert!(git(&clone, &["push", "-q", "origin", "HEAD:main"]).status.success());
    std::fs::write(clone.join("f.txt"), "proposed change\n").unwrap();
    assert!(git(&clone, &["add", "."]).status.success());
    assert!(git(&clone, &["commit", "-q", "-m", "the proposal"]).status.success());
    assert!(git(&clone, &["push", "-q", "origin", "HEAD:refs/heads/topic"]).status.success());
    let proposal = String::from_utf8_lossy(&git(&clone, &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();
    // Main moves on after the proposal branched, which is the ordinary
    // state of any review that took more than an afternoon. It is also
    // what makes the two-dot/three-dot distinction observable: a two-dot
    // diff would show this later commit as though the proposal reverted
    // it, crediting one author with another's work.
    assert!(git(&clone, &["checkout", "-q", "-B", "later", "HEAD~1"]).status.success());
    std::fs::write(clone.join("other.txt"), "landed by somebody else\n").unwrap();
    assert!(git(&clone, &["add", "."]).status.success());
    assert!(git(&clone, &["commit", "-q", "-m", "unrelated landing"]).status.success());
    assert!(git(&clone, &["push", "-q", "origin", "HEAD:main"]).status.success());

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
    assert!(page.contains(&proposal[..12]), "the proposed commit is missing: {page}");
    assert!(page.contains("refs/heads/main"), "the destination ref is missing");
    // The people, including the one who has not answered.
    assert!(page.contains("ana") && page.contains("bot"), "a reviewer is missing");
    assert!(page.contains("waiting"), "an unanswered reviewer is not shown as waiting");
    assert!(page.contains("open"), "a live review is not shown as open");
    // The diff — and specifically the proposal's own change, not the
    // whole difference between two branches.
    assert!(page.contains("proposed change"), "the diff is missing: {page}");
    assert!(page.contains("class=\"add\""), "the diff has no added lines");
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
    assert!(page.contains("reads fine to me"), "the verdict note is missing: {page}");
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
    assert!(!page.contains("fatal:"), "a non-git target reached git anyway: {page}");
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
    registry.register(&author.public_key_bytes()).expect("register author");

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
    assert!(page.contains("Discussion"), "the page has no discussion section: {page}");
    assert!(page.contains("Nothing said yet"), "an empty thread is not reported: {page}");

    for (id, who, body) in [
        ("c1", "ana", "the base looks wrong to me"),
        ("c2", "author", "<script>alert('x')</script> it is the merge base"),
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
    assert!(page.contains("the base looks wrong to me"), "a comment is missing: {page}");
    assert!(page.contains("it is the merge base"), "a comment is missing: {page}");
    let first = page.find("the base looks wrong").expect("first comment");
    let second = page.find("it is the merge base").expect("second comment");
    assert!(first < second, "the thread is not rendered in log order: {page}");
    // A comment body is text a stranger wrote, and this page is served to
    // a browser. Same rule as file contents on the D30 pages.
    assert!(
        !page.contains("<script>alert('x')</script>"),
        "a comment body reached the page unescaped: {page}"
    );
    assert!(page.contains("&lt;script&gt;"), "the body was dropped rather than escaped: {page}");

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

/// An anonymous reader gets nothing, exactly as on the D28 page. A
/// human-readable surface is the kind of thing that acquires an
/// exception, so this is asserted rather than assumed.
#[test]
fn browsing_is_behind_the_same_auth_wall() {
    let (base, _work, _oid, _) = served("anon", "");
    for path in ["/r/", "/r/agents/one", "/r/agents/one/blob/main/README.md"] {
        let (status, _, _) = get(&format!("{base}{path}"), &[]);
        assert_eq!(status, 401, "{path} served an anonymous reader");
    }
}
