//! Contract tests for the external Symphony workspace backend adapter.
//!
//! The adapter is a translation layer over `choir runner`, so these drive
//! it against a real node with the real `choir` binary. Stubbing `choir`
//! here would mean reimplementing the seam in bash, and the translation
//! between Symphony's shapes and the seam's is precisely what a stub
//! cannot check.

#![cfg(unix)]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use choir_identity::{ActorKey, Registry};
use choir_node::{Node, Platform};
use choir_oplog::MemLog;

fn tempdir() -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "choir-symphony-backend-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::remove_dir_all(&path).ok();
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn git(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(["-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main"])
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

struct Fixture {
    root: PathBuf,
    script: PathBuf,
    config: PathBuf,
    api: String,
    /// The commit `refs/heads/main` pointed at when the fixture was built.
    head: String,
}

impl Fixture {
    fn new() -> Self {
        let root = tempdir();
        let owner_key = ActorKey::from_secret_bytes(&[3; 32]);
        let key_file = root.join("symphony.key");
        std::fs::write(&key_file, owner_key.secret_bytes()).unwrap();
        let mut registry = Registry::new();
        registry.register(&owner_key.public_key_bytes()).unwrap();

        let mut node = Node::bind(&root.join("repos"), 0).unwrap();
        node.enable_platform(
            Platform::start(registry, Box::new(MemLog::new()), ActorKey::generate()).unwrap(),
        );
        node.create_repo("owner/repo.git").unwrap();
        let port = node.port();
        let node = std::sync::Arc::new(node);
        std::thread::spawn(move || node.serve_forever());
        let api = format!("http://127.0.0.1:{port}");

        // `base_ref` only resolves once a commit exists on the branch.
        let seed = root.join("seed");
        let url = format!("{api}/owner/repo.git");
        assert!(git(&root, &["clone", "-q", &url, seed.to_str().unwrap()])
            .status
            .success());
        std::fs::write(seed.join("f.txt"), "v1\n").unwrap();
        git(&seed, &["add", "."]);
        git(&seed, &["commit", "-q", "-m", "first"]);
        assert!(git(&seed, &["push", "-q", "origin", "HEAD:main"])
            .status
            .success());
        let head = String::from_utf8_lossy(&git(&seed, &["rev-parse", "HEAD"]).stdout)
            .trim()
            .to_string();

        let config = root.join("config.json");
        std::fs::write(
            &config,
            serde_json::json!({
                "api": api,
                "repo": "owner/repo",
                "owner": "operator/symphony",
                "namespace": "sy",
                "key_file": key_file,
                "base_ref": "owner/repo.git:refs/heads/main",
                "choir_bin": env!("CARGO_BIN_EXE_choir"),
                "git_bin": "git",
            })
            .to_string(),
        )
        .unwrap();

        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../templates/symphony/choir-workspace-backend.sh");
        Self {
            root,
            script,
            config,
            api,
            head,
        }
    }

    fn request(&self, operation: &str, run_id: &str, workspace_path: Option<&Path>) -> String {
        self.request_for(operation, run_id, "change-generation-1", workspace_path)
    }

    fn request_for(
        &self,
        operation: &str,
        run_id: &str,
        generation: &str,
        workspace_path: Option<&Path>,
    ) -> String {
        let mut request = serde_json::json!({
            "protocol_version": 1,
            "operation": operation,
            "issue": {"id": "tracker-opaque-7", "identifier": "ABC-123"},
            "workspace_key": "ABC-123",
            "generation": generation,
            "run_id": run_id,
        });
        if let Some(path) = workspace_path {
            request["workspace_path"] = serde_json::json!(path);
        }
        request.to_string()
    }

    fn run(&self, request: &str) -> Output {
        let mut child = Command::new("bash")
            .arg(&self.script)
            .arg(&self.config)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(request.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn ok(&self, request: &str) -> serde_json::Value {
        let output = self.run(request);
        assert!(
            output.status.success(),
            "adapter failed\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("the adapter answers JSON on stdout")
    }

    fn view(&self) -> serde_json::Value {
        let out = Command::new(env!("CARGO_BIN_EXE_choir"))
            .args(["view", &self.api])
            .output()
            .expect("choir view");
        serde_json::from_slice(&out.stdout).expect("view is JSON")
    }
}

#[test]
fn backend_converges_replacement_workers_and_completes_the_lifecycle() {
    let fixture = Fixture::new();

    let first = fixture.ok(&fixture.request("ensure", "run-1", None));
    assert_eq!(first["workspace"]["created_now"], true);
    assert_eq!(first["metadata"]["issue_id"], "tracker-opaque-7");
    assert_eq!(first["metadata"]["run_id"], "run-1");
    assert_eq!(first["metadata"]["issue_identifier"], "ABC-123");
    // The change identity is Choir's, derived from the tracker id rather
    // than being it: an adapter that passed the tracker id through would
    // let a second orchestrator on the same repo collide with it.
    assert_ne!(first["binding"]["change_id"], first["metadata"]["issue_id"]);
    assert_eq!(first["binding"]["base"], fixture.head.as_str());
    assert_eq!(first["binding"]["repo"], "owner/repo");
    assert_eq!(first["binding"]["owner"], "operator/symphony");
    // Symphony's binding is a fixed set of fields. A field arriving from
    // the seam must be a decision, not a leak.
    let mut binding_fields: Vec<&String> = first["binding"]
        .as_object()
        .expect("binding is an object")
        .keys()
        .collect();
    binding_fields.sort();
    assert_eq!(
        binding_fields,
        ["base", "change_id", "idempotency_key", "owner", "repo", "workspace_id"]
    );

    let path = PathBuf::from(
        first["workspace"]["path"]
            .as_str()
            .expect("a workspace path"),
    );
    assert_eq!(path, std::fs::canonicalize(&path).unwrap());
    assert!(
        path.join("f.txt").exists(),
        "the workspace was not provisioned from the base revision"
    );
    let sidecar: serde_json::Value = serde_json::from_slice(
        &std::fs::read(path.join(".git/choir/symphony-backend.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(sidecar["binding"], first["binding"]);
    assert_eq!(sidecar["generation"], "change-generation-1");

    // A replacement worker on the same generation is the same change and
    // the same directory, which is the reason the binding is derived
    // rather than allocated.
    let replacement = fixture.ok(&fixture.request("ensure", "run-2", None));
    assert_eq!(replacement["workspace"]["created_now"], false);
    assert_eq!(replacement["metadata"]["run_id"], "run-2");
    assert_eq!(replacement["binding"], first["binding"]);

    // The change reached the node under the derived identity.
    let change_id = first["binding"]["change_id"].as_str().unwrap().to_string();
    let view = fixture.view();
    assert!(
        view["changes"][&change_id].is_object(),
        "the derived change is absent from the view: {}",
        view["changes"]
    );
    assert_eq!(
        view["changes"][&change_id]["workspace_id"],
        first["binding"]["workspace_id"]
    );

    // Checkpoint names an exact revision, so the agent's work has to be
    // committed in the workspace first.
    std::fs::write(path.join("f.txt"), "v2\n").unwrap();
    git(&path, &["add", "."]);
    git(&path, &["commit", "-q", "-m", "agent work"]);
    let revision = String::from_utf8_lossy(&git(&path, &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();

    let checkpoint = fixture.ok(&fixture.request("checkpoint", "run-2", Some(&path)));
    assert_eq!(checkpoint["checkpoint"]["revision_id"], revision.as_str());
    assert_eq!(checkpoint["checkpoint"]["change_id"], change_id.as_str());
    // The revision is immutable and independent of any branch, so it has
    // to be reachable on the node under its own id.
    let after = fixture.view();
    assert_eq!(
        after["refs"][format!("owner/repo.git:refs/choir/revisions/{revision}")],
        serde_json::json!(format!("11-{revision}")),
        "the checkpoint object was not published: {}",
        after["refs"]
    );
    assert_eq!(
        after["changes"][&change_id]["revision_id"],
        serde_json::json!(format!("11-{revision}"))
    );

    let archived = fixture.ok(&fixture.request("archive", "run-2", Some(&path)));
    assert_eq!(archived["archive"]["already_archived"], false);
    assert_eq!(archived["archive"]["change_id"], change_id.as_str());
    assert!(
        archived["archive"]["archived_path"].is_string(),
        "archive returned no recoverable path: {archived}"
    );
    assert!(!path.exists(), "the live workspace survived the archive");

    let retried = fixture.ok(&fixture.request("archive", "run-3", Some(&path)));
    assert_eq!(retried["archive"]["already_archived"], true);
}

/// A directory is not proof of a binding. Symphony hands back a path on
/// later calls, and pointing one generation's path at another
/// generation's request must refuse rather than checkpoint one attempt's
/// work onto the other attempt's change.
#[test]
fn a_workspace_is_refused_for_a_generation_it_is_not_bound_to() {
    let fixture = Fixture::new();
    let first = fixture.ok(&fixture.request("ensure", "run-1", None));
    let path = PathBuf::from(first["workspace"]["path"].as_str().unwrap());

    let second = fixture.ok(&fixture.request_for(
        "ensure",
        "run-1",
        "change-generation-2",
        None,
    ));
    assert_ne!(
        second["binding"]["change_id"], first["binding"]["change_id"],
        "a new generation reused the previous change"
    );

    std::fs::write(path.join("f.txt"), "v2\n").unwrap();
    git(&path, &["add", "."]);
    git(&path, &["commit", "-q", "-m", "agent work"]);
    let revision = String::from_utf8_lossy(&git(&path, &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();

    let output = fixture.run(&fixture.request_for(
        "checkpoint",
        "run-1",
        "change-generation-2",
        Some(&path),
    ));
    assert!(
        !output.status.success(),
        "checkpointed generation 1's workspace under generation 2: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let error: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(error["error"]["code"], "state_mismatch");
    assert_eq!(error["error"]["retryable"], false);

    // Refusing after the fact would be worth little: a checkpoint
    // publishes an immutable object to the node before it is recorded,
    // and that push cannot be taken back.
    let view = fixture.view();
    assert!(
        view["refs"][format!("owner/repo.git:refs/choir/revisions/{revision}")].is_null(),
        "the refused checkpoint published its object anyway: {}",
        view["refs"]
    );
    // A change carries a revision from the moment it is created, so the
    // property is that it was never advanced to the foreign commit.
    assert_ne!(
        view["changes"][second["binding"]["change_id"].as_str().unwrap()]["revision_id"],
        serde_json::json!(format!("11-{revision}")),
        "the refused checkpoint was recorded against the other generation's change"
    );
}

#[test]
fn malformed_identity_fails_with_structured_json_before_any_workspace_exists() {
    let fixture = Fixture::new();
    let request = serde_json::json!({
        "protocol_version": 1,
        "operation": "ensure",
        "issue": {"id": "tracker-opaque-7", "identifier": "ABC-123"},
        "workspace_key": "../escape",
        "generation": "change-generation-1"
    })
    .to_string();

    let output = fixture.run(&request);
    assert!(!output.status.success());
    let error: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(error["protocol_version"], 1);
    assert_eq!(error["error"]["code"], "invalid_request");
    assert_eq!(error["error"]["retryable"], false);
    assert!(
        fixture.view()["changes"]
            .as_object()
            .is_none_or(serde_json::Map::is_empty),
        "a refused request still reached the node"
    );
}

/// A config the runner will not accept has to fail as a typed refusal
/// rather than as a shell error, because Symphony branches on the JSON.
#[test]
fn a_stale_config_is_refused_in_the_adapter_wire_format() {
    let fixture = Fixture::new();
    let mut config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&fixture.config).unwrap()).unwrap();
    // `state_dir` was this adapter's own durable state before the runner
    // seam made it unnecessary. A config carrying it is stale, and
    // ignoring the field would silently change what the operator asked
    // for.
    config["state_dir"] = serde_json::json!("/tmp/symphony-state");
    std::fs::write(&fixture.config, config.to_string()).unwrap();

    let output = fixture.run(&fixture.request("ensure", "run-1", None));
    assert!(!output.status.success());
    let error: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(error["error"]["code"], "invalid_config");
    assert_eq!(error["error"]["retryable"], false);
}

#[test]
fn backend_runs_with_http_auth_configured() {
    let fixture = Fixture::new();
    let auth_file = fixture.root.join("auth");
    std::fs::write(&auth_file, "scheduler:placeholder\n").unwrap();
    let mut config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&fixture.config).unwrap()).unwrap();
    config["auth_file"] = serde_json::json!(auth_file);
    config["auth_user"] = serde_json::json!("scheduler");
    std::fs::write(&fixture.config, config.to_string()).unwrap();

    let result = fixture.ok(&fixture.request("ensure", "run-1", None));
    assert_eq!(result["workspace"]["created_now"], true);
    let stderr = String::from_utf8_lossy(&fixture.run(&fixture.request("ensure", "run-2", None)).stderr)
        .to_string();
    assert!(
        !stderr.contains("placeholder"),
        "the adapter echoed a credential to stderr"
    );
}

#[test]
fn shipped_adapter_and_example_are_safe_to_install() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let script = root.join("templates/symphony/choir-workspace-backend.sh");
    assert_ne!(
        std::fs::metadata(&script).unwrap().permissions().mode() & 0o111,
        0
    );
    let config: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.join("templates/symphony/config.example.json")).unwrap(),
    )
    .unwrap();
    assert!(config["key_file"].as_str().unwrap().starts_with('/'));
    assert!(config.get("token").is_none());
    // The runner refuses a config without a namespace, so an example
    // missing one would ship an adapter that cannot run at all.
    assert!(config["namespace"].as_str().is_some_and(|n| !n.is_empty()));
    assert!(config.get("state_dir").is_none());
}
