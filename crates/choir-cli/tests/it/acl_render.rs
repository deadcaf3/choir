//! `choir acl render` against a real node (D46).
//!
//! The rewriting rules are unit-tested in `choir_cli::acl` on text, and
//! text is the easy half. What only a real node can check is the shape
//! of the roster this command parses: it reads `accounts[].user` and
//! `accounts[].display_name` out of `GET /api/accounts`, and a field
//! name invented from memory produces a command that runs, succeeds,
//! and names nobody. That failure has happened here before — the D28
//! health section was written against key names the API has never
//! emitted, and it rendered as a plausible page.

use choir_identity::{ActorKey, Registry};
use choir_node::{AuthTable, Node, Platform};
use choir_oplog::MemLog;

fn choir(args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_choir"))
        .args(args)
        .output()
        .expect("choir runs")
}

fn curl(args: &[&str]) -> (u16, String) {
    let out = std::process::Command::new("curl")
        .args(["-sS", "-w", "\n%{http_code}"])
        .args(args)
        .output()
        .expect("curl runs");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let (body, status) = text.rsplit_once('\n').expect("curl writes a status line");
    (
        status.trim().parse().expect("numeric status"),
        body.to_string(),
    )
}

/// The operator's row: node-wide write mints invites, node-wide auditor
/// reads the roster. Both are operator-file only — self-service can
/// never issue either — which is why `acl render` is an operator command
/// and not an agent tool.
const OPERATOR_ACL: &str = "alice @node write\nalice @node auditor\nalice * write\n";

#[test]
fn acl_render_names_the_handles_a_real_node_reports() {
    let work = std::env::temp_dir().join(format!("choir-acl-render-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");

    let acl_path = work.join("acl");
    std::fs::write(&acl_path, OPERATOR_ACL).expect("acl file");
    let mut table = AuthTable::new();
    table.insert("alice".into(), "a".into());

    let root = work.join("repos");
    let mut node = Node::bind_with_auth(&root, 0, Some(table)).expect("node binds");
    let port = node.port();
    node.create_repo("agents/demo.git").expect("repo created");
    node.watch_acl_file(acl_path.clone()).expect("acl loads");
    node.enable_accounts(work.join("accounts.json"), None, None)
        .expect("accounts enable");
    node.enable_platform(
        Platform::start(
            Registry::new(),
            Box::new(MemLog::new()),
            ActorKey::generate(),
        )
        .expect("platform starts"),
    );
    std::thread::spawn(move || node.serve_forever());
    let api = format!("http://127.0.0.1:{port}");

    // Onboard the way an operator actually would: an invite that says
    // what to call somebody and leaves the username to them (D75).
    let (status, body) = curl(&[
        "-u",
        "alice:a",
        "-X",
        "POST",
        "--data-binary",
        r#"{"display_name":"Ada Lovelace","grants":["agents/demo read"]}"#,
        &format!("{api}/api/accounts/invite"),
    ]);
    assert_eq!(status, 200, "invite refused: {body}");
    let issued: serde_json::Value = serde_json::from_str(&body).expect("invite json");
    let pair = issued["invite"].as_str().expect("invite pair").to_string();

    let (status, body) = curl(&[
        "-u",
        &pair,
        "-X",
        "POST",
        "--data-binary",
        r#"{"user":"ada"}"#,
        &format!("{api}/api/accounts/redeem"),
    ]);
    assert_eq!(status, 200, "redeem refused: {body}");
    let redeemed: serde_json::Value = serde_json::from_str(&body).expect("redeem json");
    let handle = redeemed["user"].as_str().expect("a principal").to_string();

    // The operator grants by username, because that is the principal,
    // and writes the file the way a person writes files.
    let granted = format!("{OPERATOR_ACL}{handle} agents/demo.git write\n");
    std::fs::write(&acl_path, &granted).expect("grant written");

    // The credential the way the CLI takes it: a file holding
    // `user:secret`, named by `--auth-file`.
    let auth_file = work.join("cli-auth");
    std::fs::write(&auth_file, "alice:a\n").expect("cli auth file");
    let out = choir(&[
        "--auth-file",
        auth_file.to_str().expect("utf-8 path"),
        "--auth-user",
        "alice",
        "acl",
        "render",
        &api,
        acl_path.to_str().expect("utf-8 path"),
    ]);
    assert!(
        out.status.success(),
        "acl render failed: {} {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // The consequence, read off the file rather than off the summary:
    // the person is named, beside the handle that is actually granted.
    let rendered = std::fs::read_to_string(&acl_path).expect("acl reread");
    assert!(
        rendered.contains(&format!("{handle} agents/demo.git write  # Ada Lovelace")),
        "the roster was fetched and nobody was named — check the field \
         names this command reads: {rendered}"
    );
    // ...and the grants are untouched, which is the property that makes
    // rewriting an authorization file safe at all.
    for line in OPERATOR_ACL.lines() {
        assert!(
            rendered.contains(line),
            "an operator grant was changed: {rendered}"
        );
    }

    let summary: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("acl render prints json");
    assert_eq!(summary["named"], 1, "{summary}");
    assert_eq!(
        summary["unresolved"], 3,
        "the operator's own rows: {summary}"
    );
    assert_eq!(summary["wrote"], true, "{summary}");

    std::fs::remove_dir_all(&work).ok();
}
