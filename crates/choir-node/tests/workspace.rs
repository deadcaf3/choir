//! Instant workspace provisioning acceptance (`POST /api/workspace`):
//! a CoW workspace appears on disk, registers in the view, is
//! independent of its siblings, and pushes ride the sequenced HTTP
//! path. Includes a small p50 measurement (printed with --nocapture).

use choir_identity::{ActorKey, Registry};
use choir_node::{Node, Platform};
use choir_oplog::MemLog;

fn git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false", "-c", "init.defaultBranch=main"])
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

fn api(port: u16, method: &str, path: &str, body: Option<&str>) -> (u16, serde_json::Value) {
    let url = format!("http://127.0.0.1:{port}{path}");
    let mut args = vec!["-s", "-w", "\n%{http_code}", "-X", method];
    if let Some(b) = body {
        args.extend(["-d", b]);
    }
    args.push(&url);
    let out = std::process::Command::new("curl").args(&args).output().expect("curl runs");
    let text = String::from_utf8_lossy(&out.stdout);
    let (body, code) = text.rsplit_once('\n').expect("status line");
    (code.trim().parse().expect("numeric status"), serde_json::from_str(body).expect("json"))
}

#[test]
fn workspace_provisioning_end_to_end() {
    let work = std::env::temp_dir().join(format!("choir-node-ws-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let root = work.join("repos");

    let mut node = Node::bind(&root, 0).unwrap();
    node.enable_platform(
        Platform::start(Registry::new(), Box::new(MemLog::new()), ActorKey::generate()).unwrap(),
    );
    let port = node.port();
    node.create_repo("agents/demo.git").unwrap();
    let node = std::sync::Arc::new(node);
    {
        let node = node.clone();
        std::thread::spawn(move || node.serve_forever());
    }

    // Empty repo: provisioning must refuse, not create a broken dir.
    let (code, resp) = api(port, "POST", "/api/workspace",
        Some(r#"{"repo":"agents/demo","name":"too-early"}"#));
    assert_eq!(code, 400, "{resp}");

    // Seed a commit through the daemon (the sequenced path).
    let url = format!("http://127.0.0.1:{port}/agents/demo.git");
    let seed = work.join("seed");
    assert!(git(&work, &["clone", "-q", &url, seed.to_str().unwrap()]).status.success());
    std::fs::write(seed.join("f.txt"), "v1\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "first"]);
    assert!(git(&seed, &["push", "-q", "origin", "HEAD:main"]).status.success());
    let head = String::from_utf8_lossy(&git(&seed, &["rev-parse", "HEAD"]).stdout).trim().to_string();

    // Two workspaces provision and are independent.
    let (code, ws1) = api(port, "POST", "/api/workspace",
        Some(r#"{"repo":"agents/demo","name":"agent-1"}"#));
    assert_eq!(code, 200, "{ws1}");
    assert_eq!(ws1["head"].as_str().unwrap(), head);
    let (code, ws2) = api(port, "POST", "/api/workspace",
        Some(r#"{"repo":"agents/demo","name":"agent-2"}"#));
    assert_eq!(code, 200, "{ws2}");

    let p1 = std::path::PathBuf::from(ws1["path"].as_str().unwrap());
    let p2 = std::path::PathBuf::from(ws2["path"].as_str().unwrap());
    assert_eq!(std::fs::read_to_string(p1.join("f.txt")).unwrap(), "v1\n");
    std::fs::write(p1.join("f.txt"), "agent-1 edit\n").unwrap();
    assert_eq!(std::fs::read_to_string(p2.join("f.txt")).unwrap(), "v1\n",
        "workspaces must not share mutable state");

    // Both registered in the view under repo/name.
    let (_, view) = api(port, "GET", "/api/view", None);
    assert!(view["workspaces"].get("agents/demo/agent-1").is_some(), "{view}");
    assert!(view["workspaces"].get("agents/demo/agent-2").is_some());

    // Workspace origin is the daemon URL, not the on-disk bare path.
    let origin = String::from_utf8_lossy(&git(&p1, &["remote", "get-url", "origin"]).stdout)
        .trim()
        .to_string();
    assert_eq!(origin, url);
    // And a push from the workspace rides the sequenced path.
    git(&p1, &["add", "."]);
    git(&p1, &["commit", "-q", "-m", "ws edit"]);
    // Workspaces start on a detached HEAD, so pushes name the full ref.
    let push = git(&p1, &["push", "-q", "origin", "HEAD:refs/heads/agent-1-work"]);
    assert!(push.status.success(), "{}", String::from_utf8_lossy(&push.stderr));
    let (_, view) = api(port, "GET", "/api/view", None);
    assert!(view["refs"].get("agents/demo.git:refs/heads/agent-1-work").is_some(), "{view}");

    // Guard rails: duplicate name, traversal, missing repo.
    let (code, _) = api(port, "POST", "/api/workspace",
        Some(r#"{"repo":"agents/demo","name":"agent-1"}"#));
    assert_eq!(code, 409);
    let (code, _) = api(port, "POST", "/api/workspace",
        Some(r#"{"repo":"agents/demo","name":"../escape"}"#));
    assert_eq!(code, 400);
    let (code, _) = api(port, "POST", "/api/workspace",
        Some(r#"{"repo":"agents/nope","name":"x"}"#));
    assert_eq!(code, 404);

    // Small provisioning sample: p50 copy time over 9 more workspaces.
    let mut times: Vec<f64> = (0..9)
        .map(|i| {
            let body = format!(r#"{{"repo":"agents/demo","name":"bench-{i}"}}"#);
            let (code, resp) = api(port, "POST", "/api/workspace", Some(&body));
            assert_eq!(code, 200, "{resp}");
            resp["copy_ms"].as_f64().unwrap()
        })
        .collect();
    times.sort_by(f64::total_cmp);
    println!("workspace copy p50: {:.2} ms (n=9)", times[4]);

    node.unblock();
}
