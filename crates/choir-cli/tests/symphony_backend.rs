//! Contract tests for the external Symphony workspace backend adapter.

#![cfg(unix)]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

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

fn write_executable(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

struct Fixture {
    root: PathBuf,
    script: PathBuf,
    config: PathBuf,
    workspace: PathBuf,
    archived: PathBuf,
    choir_log: PathBuf,
    git_log: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempdir();
        let bin = root.join("bin");
        let workspaces = root.join("workspaces");
        let workspace = workspaces.join("sy-ABC-123-aaaaaaaaaaaaaaaa");
        let archived = root.join("archive/change");
        std::fs::create_dir_all(&bin).unwrap();
        let choir_log = root.join("choir.log");
        let git_log = root.join("git.log");
        write_executable(
            &bin.join("git"),
            r#"#!/usr/bin/env bash
set -eu
{
  printf 'CALL\n'
  printf '%s\n' "$@"
} >>"$SYMPHONY_TEST_GIT_LOG"
if [[ "${1:-}" == "hash-object" ]]; then
  printf '%040d\n' 0 | tr '0' 'a'
elif [[ "${1:-}" == "-C" && "${3:-}" == "rev-parse" ]]; then
  printf '%s\n' '2222222222222222222222222222222222222222'
elif [[ "${1:-}" == "-C" && "${3:-}" == "push" ]]; then
  exit 0
else
  exit 1
fi
"#,
        );
        write_executable(
            &bin.join("choir"),
            r#"#!/usr/bin/env bash
set -eu
{
  printf 'CALL\n'
  printf '%s\n' "$@"
} >>"$SYMPHONY_TEST_CHOIR_LOG"
while [[ "${1:-}" == "--auth-file" || "${1:-}" == "--auth-user" ]]; do
  shift 2
done
case "${1:-}" in
  view)
    printf '%s\n' '{"refs":{"owner/repo.git:refs/heads/main":"11-1111111111111111111111111111111111111111"}}'
    ;;
  workspace)
    mkdir -p "$SYMPHONY_TEST_WORKSPACE/.git"
    if [[ -f "$SYMPHONY_TEST_CREATED_MARKER" ]]; then
      created=false
      reused=',"reused":true'
    else
      : >"$SYMPHONY_TEST_CREATED_MARKER"
      created=true
      reused=''
    fi
    printf '{"workspace":"%s/%s","path":"%s","change_id":"%s","created":%s%s,"operation":{"seq":7,"hash":"1e-receipt"}}\n' \
      "$3" "$4" "$SYMPHONY_TEST_WORKSPACE" "${12}" "$created" "$reused"
    ;;
  checkpoint)
    printf '%s\n' '{"seq":8,"hash":"1e-checkpoint"}'
    ;;
  workspace-archive)
    if [[ -d "$SYMPHONY_TEST_WORKSPACE" ]]; then
      mkdir -p "$(dirname "$SYMPHONY_TEST_ARCHIVED")"
      mv "$SYMPHONY_TEST_WORKSPACE" "$SYMPHONY_TEST_ARCHIVED"
      already=false
    else
      already=true
    fi
    printf '{"workspace":"%s/%s","change_id":"%s","archived_path":"%s","already_archived":%s,"operation":{"seq":9,"hash":"1e-archive"}}\n' \
      "$5" "$6" "$7" "$SYMPHONY_TEST_ARCHIVED" "$already"
    ;;
  *)
    printf '%s\n' '{"code":"synthetic","detail":"unexpected command"}'
    exit 1
    ;;
esac
"#,
        );
        let key_file = root.join("symphony.key");
        let auth_file = root.join("auth");
        std::fs::write(&key_file, [3_u8; 32]).unwrap();
        std::fs::write(&auth_file, "scheduler:placeholder\n").unwrap();
        let config = root.join("config.json");
        std::fs::write(
            &config,
            serde_json::json!({
                "api": "http://127.0.0.1:8417",
                "repo": "owner/repo",
                "owner": "operator/symphony",
                "key_file": key_file,
                "auth_file": auth_file,
                "auth_user": "scheduler",
                "base_ref": "owner/repo.git:refs/heads/main",
                "state_dir": root.join("state"),
                "choir_bin": bin.join("choir"),
                "git_bin": bin.join("git"),
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
            workspace,
            archived,
            choir_log,
            git_log,
        }
    }

    fn request(&self, operation: &str, run_id: &str, workspace_path: Option<&Path>) -> String {
        let mut request = serde_json::json!({
            "protocol_version": 1,
            "operation": operation,
            "issue": {"id": "tracker-opaque-7", "identifier": "ABC-123"},
            "workspace_key": "ABC-123",
            "generation": "change-generation-1",
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
            .env("SYMPHONY_TEST_CHOIR_LOG", &self.choir_log)
            .env("SYMPHONY_TEST_GIT_LOG", &self.git_log)
            .env("SYMPHONY_TEST_WORKSPACE", &self.workspace)
            .env("SYMPHONY_TEST_ARCHIVED", &self.archived)
            .env("SYMPHONY_TEST_CREATED_MARKER", self.root.join("created"))
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

    fn calls(path: &Path) -> Vec<Vec<String>> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .split("CALL\n")
            .skip(1)
            .map(|call| call.lines().map(str::to_string).collect())
            .collect()
    }
}

#[test]
fn backend_converges_replacement_workers_and_completes_the_lifecycle() {
    let fixture = Fixture::new();
    let first = fixture.run(&fixture.request("ensure", "run-1", None));
    assert!(
        first.status.success(),
        "{} {}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );
    let first: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(first["workspace"]["created_now"], true);
    assert_eq!(
        first["workspace"]["path"],
        std::fs::canonicalize(&fixture.workspace)
            .unwrap()
            .display()
            .to_string()
    );
    assert_eq!(first["metadata"]["issue_id"], "tracker-opaque-7");
    assert_eq!(first["metadata"]["run_id"], "run-1");
    assert_ne!(first["binding"]["change_id"], first["metadata"]["issue_id"]);
    assert_eq!(
        first["binding"]["base"],
        "1111111111111111111111111111111111111111"
    );

    let replacement = fixture.run(&fixture.request("ensure", "run-2", None));
    assert!(replacement.status.success());
    let replacement: serde_json::Value = serde_json::from_slice(&replacement.stdout).unwrap();
    assert_eq!(replacement["workspace"]["created_now"], false);
    assert_eq!(replacement["metadata"]["run_id"], "run-2");
    assert_eq!(replacement["binding"], first["binding"]);

    let choir_calls = Fixture::calls(&fixture.choir_log);
    assert_eq!(
        choir_calls.len(),
        3,
        "one view lookup and two create attempts"
    );
    assert_eq!(choir_calls[0][4], "view");
    for call in [&choir_calls[1], &choir_calls[2]] {
        assert_eq!(call[4], "workspace");
        assert_eq!(call[7], "sy-ABC-123-aaaaaaaaaaaaaaaa");
        assert_eq!(
            call[8..12],
            [
                "--base",
                "1111111111111111111111111111111111111111",
                "--owner",
                "operator/symphony"
            ]
        );
        assert_eq!(call[12], "--key-file");
        assert_eq!(call[14], "--change");
        assert_eq!(call[16], "--idempotency-key");
    }
    assert_eq!(
        std::fs::read_dir(fixture.root.join("state"))
            .unwrap()
            .count(),
        1
    );

    let checkpoint = fixture.run(&fixture.request("checkpoint", "run-2", Some(&fixture.workspace)));
    assert!(
        checkpoint.status.success(),
        "{} {}",
        String::from_utf8_lossy(&checkpoint.stdout),
        String::from_utf8_lossy(&checkpoint.stderr)
    );
    let checkpoint: serde_json::Value = serde_json::from_slice(&checkpoint.stdout).unwrap();
    assert_eq!(
        checkpoint["checkpoint"]["revision_id"],
        "2222222222222222222222222222222222222222"
    );
    let git_calls = Fixture::calls(&fixture.git_log);
    assert!(git_calls.iter().any(|call| {
        call.windows(2).any(|pair| pair == ["push", "origin"])
            && call.iter().any(|arg| {
                arg == "2222222222222222222222222222222222222222:refs/choir/revisions/2222222222222222222222222222222222222222"
            })
    }));

    let archived = fixture.run(&fixture.request("archive", "run-2", Some(&fixture.workspace)));
    assert!(archived.status.success());
    let archived: serde_json::Value = serde_json::from_slice(&archived.stdout).unwrap();
    assert_eq!(archived["archive"]["already_archived"], false);
    assert!(!fixture.workspace.exists());
    assert!(fixture.archived.exists());

    let retried = fixture.run(&fixture.request("archive", "run-3", Some(&fixture.workspace)));
    assert!(retried.status.success());
    let retried: serde_json::Value = serde_json::from_slice(&retried.stdout).unwrap();
    assert_eq!(retried["archive"]["already_archived"], true);
}

#[test]
fn malformed_identity_fails_with_structured_json_before_any_backend_call() {
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
    assert_eq!(error["error"]["code"], "invalid_request");
    assert_eq!(error["error"]["retryable"], false);
    assert!(!String::from_utf8_lossy(&output.stderr).contains("placeholder"));
    assert!(Fixture::calls(&fixture.choir_log).is_empty());
}

#[test]
fn backend_runs_without_http_auth() {
    let fixture = Fixture::new();
    let mut config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&fixture.config).unwrap()).unwrap();
    config.as_object_mut().unwrap().remove("auth_file");
    config.as_object_mut().unwrap().remove("auth_user");
    std::fs::write(&fixture.config, config.to_string()).unwrap();

    let output = fixture.run(&fixture.request("ensure", "run-1", None));
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = Fixture::calls(&fixture.choir_log);
    assert_eq!(calls[0][0], "view");
    assert_eq!(calls[1][0], "workspace");
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
}
