//! `GET /api/repos` — which repositories a credential can see.
//!
//! The interesting property is that it narrows rather than refuses. A
//! reader granted one repository has a legitimate view of that
//! repository, and a listing that answered 403 because the node also
//! holds somebody else's would make the grant useless — the same
//! reasoning the other aggregate reads already follow.
//!
//! The second property is that the two empty answers stay
//! distinguishable. "You can see none of them" and "there are none" are
//! the same empty array and completely different problems, so the
//! response says which one it is giving.

use choir_identity::{ActorKey, Registry};
use choir_node::{AuthTable, Node, Platform};
use choir_oplog::MemLog;

struct Served {
    base: String,
}

fn get(url: &str, user: &str) -> (u16, serde_json::Value) {
    let out = std::process::Command::new("curl")
        .args(["-s", "-w", "\n%{http_code}", "-u", user])
        .arg(url)
        .output()
        .expect("curl runs");
    let text = String::from_utf8_lossy(&out.stdout);
    let (body, code) = text.rsplit_once('\n').expect("status line");
    (
        code.trim().parse().expect("numeric status"),
        serde_json::from_str(body).unwrap_or_default(),
    )
}

/// A node holding three repositories, with an ACL only if asked for one.
fn served(tag: &str, acl: Option<&str>) -> Served {
    let work = std::env::temp_dir().join(format!("choir-node-repos-{tag}"));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");
    let mut table = AuthTable::new();
    table.insert("alice".into(), "a".into());
    table.insert("bob".into(), "b".into());
    let root = work.join("repos");
    let mut node = Node::bind_with_auth(&root, 0, Some(table)).expect("node binds free port");
    let port = node.port();
    for name in ["one/alpha.git", "one/beta.git", "two/gamma.git"] {
        node.create_repo(name).expect("repo created");
    }
    if let Some(acl) = acl {
        let path = work.join("acl");
        std::fs::write(&path, acl).expect("acl file");
        node.watch_acl_file(path).expect("acl loads");
    }
    node.enable_platform(
        Platform::start(
            Registry::new(),
            Box::new(MemLog::new()),
            ActorKey::generate(),
        )
        .expect("platform starts"),
    );
    std::thread::spawn(move || node.serve_forever());
    Served {
        base: format!("http://127.0.0.1:{port}"),
    }
}

fn names(body: &serde_json::Value) -> Vec<String> {
    body["repos"]
        .as_array()
        .expect("a repos array")
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

/// With no ACL there is nobody to narrow to, so every repository is
/// listed — and sorted, because an order that depends on the filesystem
/// is an order that changes between two machines holding the same node.
#[test]
fn lists_every_repository_when_no_acl_is_configured() {
    let served = served("all", None);
    let (status, body) = get(&format!("{}/api/repos", served.base), "alice:a");
    assert_eq!(status, 200);
    assert_eq!(
        names(&body),
        ["one/alpha.git", "one/beta.git", "two/gamma.git"]
    );
    assert_eq!(body["narrowed"], serde_json::json!(false));
}

/// The point of the endpoint: a narrower grant sees less, rather than
/// being refused the question.
#[test]
fn an_acl_narrows_the_list_rather_than_refusing_it() {
    let served = served("narrow", Some("alice * read\nbob one/beta.git read\n"));
    let (status, body) = get(&format!("{}/api/repos", served.base), "bob:b");
    assert_eq!(status, 200, "narrowed, not refused");
    assert_eq!(names(&body), ["one/beta.git"]);
    assert_eq!(body["narrowed"], serde_json::json!(true));

    // The same node, asked by the credential that holds `*`.
    let (status, body) = get(&format!("{}/api/repos", served.base), "alice:a");
    assert_eq!(status, 200);
    assert_eq!(names(&body).len(), 3);
}

/// "You can see none of them" and "there are none" are the same empty
/// array, and a caller that cannot tell them apart tells its reader to
/// go and create a repository that already exists.
#[test]
fn an_empty_answer_says_which_kind_of_empty_it_is() {
    let served = served("empty", Some("alice @node auditor\n"));
    let (status, body) = get(&format!("{}/api/repos", served.base), "alice:a");
    assert_eq!(status, 200);
    assert!(names(&body).is_empty(), "a node grant is not a repo grant");
    assert_eq!(
        body["narrowed"],
        serde_json::json!(true),
        "an empty list under an ACL is not an empty node"
    );
}

/// A write grant implies read, so the listing must not ask for exactly
/// `read` and drop everybody who holds more.
#[test]
fn a_write_grant_can_see_what_it_can_write() {
    let served = served("write", Some("bob two/gamma.git write\n"));
    let (_, body) = get(&format!("{}/api/repos", served.base), "bob:b");
    assert_eq!(names(&body), ["two/gamma.git"]);
}

/// The credential still has to be one the node knows; narrowing is
/// about which repositories, never about whether to authenticate.
#[test]
fn a_bad_credential_is_still_refused() {
    let served = served("badcred", None);
    let (status, _) = get(&format!("{}/api/repos", served.base), "alice:wrong");
    assert_eq!(status, 401);
}
