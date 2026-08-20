//! The read surface with scripting disabled — D39's tripwire, written
//! before the thing it guards.
//!
//! D39 reverses D28's no-JavaScript rule in one narrow place: a browser
//! write needs the browser to *produce a signature*, and an HTML form
//! submits values rather than computing them. Its row scopes that
//! reversal, and one clause of the scope is a tripwire:
//!
//! > A read page stops rendering with scripting disabled → the
//! > enhancement became a dependency; fix it as a defect rather than
//! > documenting the requirement, because working anywhere is the read
//! > surface's whole value.
//!
//! Today there is no script on any page, so the property holds by
//! accident. That is precisely why it is worth an assertion now: a
//! tripwire nobody can trip is a sentence in a document, and this one has
//! to survive the commit that makes it possible to violate.
//!
//! # Why this is not "assert there is no script"
//!
//! Because that assertion would be *wrong* the moment D39 lands, and a
//! test that must be deleted to ship the next change protects nothing —
//! whoever deletes it will not stop to ask what it meant. So the check
//! models the reader instead of the markup: strip every `<script>`
//! element from the served bytes, which is the document a browser with
//! scripting disabled actually renders, and require that everything a
//! human came for is still in it. That stays true and stays meaningful
//! after D39 adds its buttons, and it fails exactly when a page starts
//! *depending* on script to say what it knows.
//!
//! It also fails for the subtler regression the row is really about: a
//! page that renders a placeholder server-side and fills it in from
//! script. Such a page would still contain `<script`, still return 200,
//! and still look right in a browser with scripting on.

use choir_identity::{ActorKey, Registry};
use choir_node::{AuthTable, Node, Platform};
use choir_oplog::MemLog;
use choir_view::{OpKind, ViewOp};

/// One request, as `(status, body)`.
fn get(url: &str) -> (u16, String) {
    let out = std::process::Command::new("curl")
        .args(["-s", "-w", "\n%{http_code}", "-u", "u:t", url])
        .output()
        .expect("curl runs");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let (body, code) = text.rsplit_once('\n').unwrap_or((text.as_str(), "0"));
    (code.trim().parse().unwrap_or(0), body.to_string())
}

fn git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
            "-c",
            "credential.helper=",
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

/// The document a browser with scripting disabled renders: every
/// `<script>` element gone, everything else untouched.
///
/// An unclosed `<script` swallows the rest of the document in a real
/// parser, so it does here too rather than optimistically keeping the
/// tail — modelling the lenient case would let exactly the bug this
/// guards against slip through.
fn without_scripts(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(at) = rest.find("<script") {
        out.push_str(&rest[..at]);
        rest = match rest[at..].find("</script>") {
            Some(end) => &rest[at + end + "</script>".len()..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

/// A node with everything a reader can open: a repository with real
/// content pushed into it, a review with a verdict and a comment, and the
/// view those pushes produced.
fn served() -> (String, String) {
    let work = std::env::temp_dir().join("choir-node-noscript");
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");

    let author = ActorKey::generate();
    let mut registry = Registry::new();
    registry
        .register(&author.public_key_bytes())
        .expect("register author");

    let mut table = AuthTable::new();
    table.insert("u".into(), "t".into());

    let mut node = Node::bind_with_auth(&work.join("repos"), 0, Some(table))
        .expect("node binds on a free port");
    let port = node.port();
    node.create_repo("agents/one.git").expect("repo created");
    node.enable_platform(
        Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate())
            .expect("platform starts"),
    );
    std::thread::spawn(move || node.serve_forever());
    let base = format!("http://127.0.0.1:{port}");

    let clone = work.join("clone");
    let url = format!("http://u:t@127.0.0.1:{port}/agents/one.git");
    assert!(
        git(
            &work,
            &["clone", "-q", &url, clone.to_str().expect("utf-8 path")]
        )
        .status
        .success(),
        "seeding clone failed"
    );
    std::fs::create_dir_all(clone.join("src")).expect("src dir");
    std::fs::write(clone.join("README.md"), "the readme body line\n").expect("readme");
    std::fs::write(clone.join("src/lib.rs"), "fn distinctive_symbol() {}\n").expect("source");
    assert!(git(&clone, &["add", "."]).status.success());
    assert!(git(&clone, &["commit", "-q", "-m", "the commit subject"])
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

    let post = |op: &ViewOp, channel: &str| {
        let (code, resp) = crate::support::curl(&[
            "-u",
            "u:t",
            "-X",
            "POST",
            "-d",
            &crate::support::submit_body_legacy(&author, channel, op),
            &format!("{base}/api/submit"),
        ]);
        assert_eq!(code, 200, "{resp}");
    };
    post(
        &ViewOp::new(OpKind::RequestReview {
            id: "r-noscript".into(),
            target: choir_oplog::ContentHash::from_git_oid(&oid).expect("a git oid"),
            reviewers: vec!["ana".into()],
            target_ref: Some("agents/one.git:refs/heads/main".into()),
        }),
        "author",
    );
    post(
        &ViewOp::new(OpKind::PostVerdict {
            id: "r-noscript".into(),
            reviewer: "ana".into(),
            verdict: choir_view::Verdict::Approve,
            note: "the verdict note".into(),
        }),
        "ana",
    );
    post(
        &ViewOp::new(OpKind::PostComment {
            id: "r-noscript".into(),
            comment: "c1".into(),
            author: "ana".into(),
            body: "the comment body".into(),
        }),
        "ana",
    );

    (base, oid)
}

/// Every page says what it knows in the bytes it serves.
///
/// One test rather than one per page on purpose: the property is about
/// the surface, and a reader who can open six pages and not the seventh
/// has still lost the surface. The fixture is expensive enough that
/// splitting it would mean six pushes instead of one.
#[test]
fn every_page_renders_its_content_with_scripting_disabled() {
    let (base, oid) = served();

    // (path, what a human came to this page to read)
    let pages: Vec<(String, Vec<&str>)> = vec![
        (
            "/status".to_string(),
            // Node state: the sequence, the ref the push created, the
            // section headings, and the health figures.
            vec!["seq", "refs/heads/main", "Refs", "Reviews", "Health"],
        ),
        // The front door and its older address both reach the index.
        ("/".to_string(), vec!["agents/one"]),
        ("/r/".to_string(), vec!["agents/one"]),
        (
            "/r/agents/one".to_string(),
            vec!["README.md", "src", "history", "reviews"],
        ),
        (
            "/r/agents/one/tree/main/src".to_string(),
            vec!["lib.rs", "root"],
        ),
        (
            "/r/agents/one/blob/main/README.md".to_string(),
            vec!["the readme body line"],
        ),
        (
            "/r/agents/one/blob/main/src/lib.rs".to_string(),
            vec!["distinctive_symbol"],
        ),
        (
            "/r/agents/one/commits/main".to_string(),
            vec!["the commit subject", &oid[..12]],
        ),
        (
            format!("/r/agents/one/commit/{oid}"),
            vec!["the commit subject", "README.md", "the readme body line"],
        ),
        (
            "/r/agents/one/reviews".to_string(),
            vec!["r-noscript", "approved"],
        ),
        (
            "/r/agents/one/review/r-noscript".to_string(),
            vec![
                "r-noscript",
                "ana",
                "approve",
                "the verdict note",
                "the comment body",
                "refs/heads/main",
                "Discussion",
            ],
        ),
        // A refusal is a read page too, and the one most likely to be
        // reached for a client-side rewrite later.
        (
            "/r/agents/one/tree/no-such-branch".to_string(),
            vec!["no such", "next"],
        ),
        (
            "/definitely-not-a-route".to_string(),
            vec!["next", "node state"],
        ),
        // D39's account page, added when it landed rather than when
        // this list was written. This node runs no `--accounts-file`,
        // so the page says so — and saying so is exactly the content a
        // reader with scripting off must still get, because the
        // alternative is a blank page that looks like a broken route.
        (
            "/account".to_string(),
            vec!["account self-service", "auth file"],
        ),
        // D57's front door. It carries the only `<form method="post">` on
        // this surface — every other write path here is script-driven —
        // so it is the page where "works with scripting off" is a claim
        // about the feature rather than about the prose. This node runs
        // no `--accounts-file`, so what it must still say is that it
        // takes no invites, which sends the reader to the operator
        // instead of leaving them on a blank page.
        ("/join".to_string(), vec!["does not accept invites", "next"]),
    ];

    for (path, wanted) in pages {
        let (status, body) = get(&format!("{base}{path}"));
        assert!(
            (200..500).contains(&status),
            "{path} did not answer at all: {status}"
        );
        let readable = without_scripts(&body);
        assert!(
            readable.contains("</html>"),
            "{path} lost its document when scripts were removed, so a script \
             element is swallowing the page"
        );
        for want in wanted {
            assert!(
                readable.contains(want),
                "{path} needs script to say {want:?}; with scripting disabled a \
                 reader sees a page that does not contain it.\n{readable}"
            );
        }
    }
}

/// The stripper has to actually strip, or the test above passes because
/// it never removed anything. Checked directly rather than trusted: it is
/// the one piece of this module that can silently make the guard vacuous.
#[test]
fn the_script_stripper_removes_what_a_disabled_browser_would_not_run() {
    assert_eq!(without_scripts("<p>a</p>"), "<p>a</p>");
    assert_eq!(
        without_scripts("<p>a</p><script>hidden()</script><p>b</p>"),
        "<p>a</p><p>b</p>"
    );
    assert_eq!(
        without_scripts("<p>a</p><script type=\"module\">x</script><p>b</p>"),
        "<p>a</p><p>b</p>"
    );
    // Two of them, and an unclosed one that takes the rest with it.
    assert_eq!(
        without_scripts("a<script>x</script>b<script>y</script>c"),
        "abc"
    );
    assert_eq!(without_scripts("a<script>never closed"), "a");
}
