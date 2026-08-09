//! Contract tests for the thin Claude Code worktree hook adapter.

#![cfg(unix)]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn tempdir() -> PathBuf {
    let path = std::env::temp_dir().join(format!("choir-claude-hooks-{}", std::process::id()));
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
    log: PathBuf,
    git_log: PathBuf,
    path: String,
}

impl Fixture {
    fn new() -> Self {
        let root = tempdir();
        let fake_bin = root.join("bin");
        let cwd = root.join("project");
        let workspace = root.join("cc-feature-auth-session-123");
        std::fs::create_dir_all(&fake_bin).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(workspace.join(".git")).unwrap();
        let log = root.join("choir.log");
        let git_log = root.join("git.log");
        write_executable(
            &fake_bin.join("git"),
            "#!/usr/bin/env bash\nset -eu\nprintf '%s\\n' \"$@\" >>\"$CHOIR_TEST_GIT_LOG\"\nprintf '%s\\n' \"$CHOIR_TEST_HEAD\"\n",
        );
        let fake_choir = fake_bin.join("choir");
        write_executable(
            &fake_choir,
            r#"#!/usr/bin/env bash
set -eu
{
  printf 'CALL\n'
  printf '%s\n' "$@"
} >>"$CHOIR_TEST_LOG"
if [[ "${CHOIR_TEST_FAIL:-}" == "1" ]]; then
  printf '%s\n' '{"code":"synthetic","detail":"try again"}'
  exit 1
fi
while [[ "${1:-}" == "--auth-file" || "${1:-}" == "--auth-user" ]]; do
  shift 2
done
case "${1:-}" in
  workspace)
    printf '{"workspace":"%s/%s","path":"%s","change_id":"%s"}\n' \
      "$3" "$4" "$CHOIR_TEST_WORKSPACE" "${10}"
    ;;
  workspace-archive)
    printf '{"workspace":"%s/%s","change_id":"%s","already_archived":false}\n' \
      "$5" "$6" "$7"
    ;;
  *)
    printf '%s\n' '{"error":"unexpected command"}'
    exit 1
    ;;
esac
"#,
        );

        let key_file = root.join("agent.key");
        let auth_file = root.join("auth");
        std::fs::write(&key_file, [7_u8; 32]).unwrap();
        std::fs::write(&auth_file, "hook-user:secret-token\n").unwrap();
        let config = root.join("hook.json");
        std::fs::write(
            &config,
            serde_json::json!({
                "api": "http://127.0.0.1:8417",
                "repo": "owner/repo",
                "owner": "operator/claude",
                "key_file": key_file,
                "auth_file": auth_file,
                "auth_user": "hook-user",
                "choir_bin": fake_choir,
            })
            .to_string(),
        )
        .unwrap();
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../templates/claude-code/choir-worktree.sh");
        let path = std::fs::canonicalize(&workspace)
            .unwrap()
            .display()
            .to_string();
        Self {
            root,
            script,
            config,
            workspace,
            log,
            git_log,
            path,
        }
    }

    fn create_input(&self, name: &str) -> String {
        serde_json::json!({
            "session_id": "session-123",
            "transcript_path": self.root.join("transcript.jsonl"),
            "cwd": self.root.join("project"),
            "hook_event_name": "WorktreeCreate",
            "name": name,
        })
        .to_string()
    }

    fn remove_input(&self, path: &str) -> String {
        serde_json::json!({
            "session_id": "session-123",
            "transcript_path": self.root.join("transcript.jsonl"),
            "cwd": self.root.join("project"),
            "hook_event_name": "WorktreeRemove",
            "worktree_path": path,
        })
        .to_string()
    }

    fn run(&self, mode: &str, input: &str, fail: bool) -> Output {
        let system_path = std::env::var("PATH").unwrap_or_default();
        let fake_path = self.root.join("bin");
        let mut child = Command::new("bash")
            .arg(&self.script)
            .args([mode, self.config.to_str().unwrap()])
            .env("PATH", format!("{}:{system_path}", fake_path.display()))
            .env("CHOIR_TEST_LOG", &self.log)
            .env("CHOIR_TEST_GIT_LOG", &self.git_log)
            .env(
                "CHOIR_TEST_HEAD",
                "1111111111111111111111111111111111111111",
            )
            .env("CHOIR_TEST_WORKSPACE", &self.path)
            .env("CHOIR_TEST_FAIL", if fail { "1" } else { "0" })
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("hook starts");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn call_count(&self) -> usize {
        self.calls().len()
    }

    fn calls(&self) -> Vec<Vec<String>> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .split("CALL\n")
            .skip(1)
            .map(|call| call.lines().map(str::to_string).collect())
            .collect()
    }
}

#[test]
fn create_and_remove_translate_the_claude_hook_contract() {
    let fixture = Fixture::new();
    let workspace_name = "cc-feature-auth-session-123";
    let workspace_id = format!("owner/repo/{workspace_name}");
    let change_id = format!("claude-code:owner/repo:{workspace_name}");
    let idempotency_key = format!("claude-code-create:owner/repo:{workspace_name}");

    let created = fixture.run("create", &fixture.create_input("feature-auth"), false);
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&created.stdout),
        format!("{}\n", fixture.path)
    );
    assert!(created.stderr.is_empty());
    let sidecar = fixture.workspace.join(".git/choir/claude-worktree.json");
    let metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&sidecar).unwrap()).unwrap();
    assert_eq!(metadata["workspace"], workspace_id);
    assert_eq!(metadata["change"], change_id);
    assert_eq!(metadata["idempotency_key"], idempotency_key);
    assert_eq!(metadata["path"], fixture.path);

    assert_eq!(
        fixture.calls()[0],
        [
            "--auth-file".to_string(),
            fixture.root.join("auth").display().to_string(),
            "--auth-user".to_string(),
            "hook-user".to_string(),
            "workspace".to_string(),
            "http://127.0.0.1:8417".to_string(),
            "owner/repo".to_string(),
            workspace_name.to_string(),
            "--base".to_string(),
            "1111111111111111111111111111111111111111".to_string(),
            "--owner".to_string(),
            "operator/claude".to_string(),
            "--change".to_string(),
            change_id.clone(),
            "--idempotency-key".to_string(),
            idempotency_key.clone(),
        ]
    );
    assert_eq!(
        std::fs::read_to_string(&fixture.git_log).unwrap(),
        format!(
            "-C\n{}\nrev-parse\n--verify\nHEAD^{{commit}}\n",
            fixture.root.join("project").display()
        )
    );

    // An identical Claude retry reaches the idempotent lifecycle call and
    // returns the same one-line absolute path.
    let retried = fixture.run("create", &fixture.create_input("feature-auth"), false);
    assert!(retried.status.success());
    assert_eq!(retried.stdout, created.stdout);
    assert_eq!(fixture.call_count(), 2);

    // Unsafe input fails before either git or choir receives it.
    let unsafe_name = fixture.run("create", &fixture.create_input("../escape"), false);
    assert!(!unsafe_name.status.success());
    assert!(unsafe_name.stdout.is_empty());
    assert_eq!(fixture.call_count(), 2);

    // A live path must carry exact non-secret metadata. Absence or mismatch cannot
    // accidentally archive a different workspace.
    std::fs::remove_file(&sidecar).unwrap();
    let missing = fixture.run("remove", &fixture.remove_input(&fixture.path), false);
    assert!(!missing.status.success());
    assert!(missing.stdout.is_empty());
    assert_eq!(fixture.call_count(), 2);
    assert!(fixture
        .run("create", &fixture.create_input("feature-auth"), false)
        .status
        .success());
    let mut wrong = metadata;
    wrong["owner"] = serde_json::json!("other/agent");
    std::fs::write(&sidecar, wrong.to_string()).unwrap();
    let refused = fixture.run("remove", &fixture.remove_input(&fixture.path), false);
    assert!(!refused.status.success());
    assert!(refused.stdout.is_empty());
    assert_eq!(fixture.call_count(), 3);

    // Re-running create repairs the sidecar without creating a second
    // logical workspace; removal then maps to the exact archive binding.
    assert!(fixture
        .run("create", &fixture.create_input("feature-auth"), false)
        .status
        .success());
    let removed = fixture.run("remove", &fixture.remove_input(&fixture.path), false);
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
    assert!(removed.stdout.is_empty());
    assert!(removed.stderr.is_empty());
    assert_eq!(
        fixture.calls()[4],
        [
            "--auth-file".to_string(),
            fixture.root.join("auth").display().to_string(),
            "--auth-user".to_string(),
            "hook-user".to_string(),
            "workspace-archive".to_string(),
            "http://127.0.0.1:8417".to_string(),
            fixture.root.join("agent.key").display().to_string(),
            "operator/claude".to_string(),
            "owner/repo".to_string(),
            workspace_name.to_string(),
            change_id,
            idempotency_key,
        ]
    );

    // Response-loss cleanup retries still work after the live path and
    // its sidecar have moved into Choir's recoverable archive.
    std::fs::remove_dir_all(&fixture.workspace).unwrap();
    let removed_again = fixture.run("remove", &fixture.remove_input(&fixture.path), false);
    assert!(removed_again.status.success());
    assert!(removed_again.stdout.is_empty());

    // Claude must pass back the canonical path printed by create.
    let target = fixture.root.join("symlink-target");
    let link = fixture.root.join(workspace_name);
    std::fs::create_dir_all(&target).unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let symlinked = fixture.run(
        "remove",
        &fixture.remove_input(link.to_str().unwrap()),
        false,
    );
    assert!(!symlinked.status.success());
    assert!(symlinked.stdout.is_empty());
    std::fs::remove_file(&link).unwrap();

    // Failures are diagnostics-only and do not reflect credential values.
    let failed = fixture.run("remove", &fixture.remove_input(&fixture.path), true);
    assert!(!failed.status.success());
    assert!(failed.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(stderr.contains("synthetic: try again"), "{stderr}");
    assert!(
        !stderr.contains("secret-token"),
        "credential leaked: {stderr}"
    );
}

#[test]
fn shipped_settings_name_both_unmatched_lifecycle_hooks() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let script = root.join("templates/claude-code/choir-worktree.sh");
    assert_ne!(
        std::fs::metadata(&script).unwrap().permissions().mode() & 0o111,
        0
    );
    let settings: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.join("templates/claude-code/settings.worktree.example.json")).unwrap(),
    )
    .unwrap();
    for event in ["WorktreeCreate", "WorktreeRemove"] {
        let entry = &settings["hooks"][event][0];
        assert!(
            entry.get("matcher").is_none(),
            "{event} must not use a matcher"
        );
        let command = entry["hooks"][0]["command"].as_str().unwrap();
        assert!(command.contains("choir-worktree.sh"));
    }
}
