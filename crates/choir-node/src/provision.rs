//! Instant workspace provisioning (`POST /api/workspace`) — the D21
//! "productized provisioning" wedge feature.
//!
//! A workspace is a copy-on-write clone of a per-repo template checkout
//! (APFS `clonefile` on macOS, `--reflink=auto` on Linux), so creation
//! cost is O(directory entries), not O(bytes). Each created workspace is
//! registered in the platform view with a node-signed
//! `SetWorkspaceHead` op, so `/api/view` is the workspace inventory.
//!
//! The workspace's `origin` is rewritten to the daemon's own HTTP URL:
//! pushes from a workspace go through the sequenced smart-HTTP path,
//! never straight at the bare repo on disk.

use std::path::{Path, PathBuf};

use crate::platform::Platform;

/// Path segment allowed in repo/workspace names: no traversal, no
/// hidden files, no separators.
fn safe_segment(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('.')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Runs git, returning stdout or a failure description.
fn git(args: &[&str], dir: Option<&Path>) -> Result<String, String> {
    let mut cmd = std::process::Command::new("git");
    cmd.args(args).env("GIT_TERMINAL_PROMPT", "0");
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    let out = cmd.output().map_err(|e| format!("spawn git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

/// Copy-on-write directory copy: `clonefile` on macOS, reflink (with
/// silent fallback to plain copy on non-CoW filesystems) on Linux.
fn cow_copy(src: &Path, dst: &Path) -> Result<(), String> {
    let (cmd, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("cp", vec!["-Rc"])
    } else {
        ("cp", vec!["-R", "--reflink=auto"])
    };
    let out = std::process::Command::new(cmd)
        .args(&args)
        .arg(src)
        .arg(dst)
        .output()
        .map_err(|e| format!("spawn cp: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("cow copy: {}", String::from_utf8_lossy(&out.stderr)))
    }
}

/// One mutex per repo: template refresh and CoW copy must be serialized
/// per repo, or a concurrent request can copy a mid-checkout template
/// (and two same-name requests can both pass the exists check).
/// Different repos provision in parallel.
fn repo_lock(repo: &str) -> std::sync::Arc<std::sync::Mutex<()>> {
    static LOCKS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::BTreeMap<String, std::sync::Arc<std::sync::Mutex<()>>>>,
    > = std::sync::OnceLock::new();
    LOCKS
        .get_or_init(Default::default)
        .lock()
        .expect("lock table")
        .entry(repo.to_string())
        .or_default()
        .clone()
}

/// Handles `POST /api/workspace`; body `{"repo": "owner/repo",
/// "name": "<workspace>"}`. Returns `(status, json_body)` like the rest
/// of the platform API. `base_url` is the daemon's own address, used as
/// the workspace's `origin` so pushes stay on the sequenced path.
pub fn create_workspace(
    root: &Path,
    platform: &Platform,
    base_url: &str,
    user: &str,
    body: &[u8],
) -> (u16, String) {
    let req: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return (400, format!(r#"{{"error":"bad json: {e}"}}"#)),
    };
    let field = |k: &str| req.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let (repo, name) = (field("repo"), field("name"));
    let mut segs = repo.split('/');
    let (Some(owner), Some(reponame), None) = (segs.next(), segs.next(), segs.next()) else {
        return (400, r#"{"error":"repo must be owner/repo"}"#.to_string());
    };
    if !safe_segment(owner) || !safe_segment(reponame) || !safe_segment(name) {
        return (400, r#"{"error":"bad repo or workspace name"}"#.to_string());
    }

    let bare = root.join(format!("{repo}.git"));
    if !bare.join("HEAD").exists() {
        return (404, r#"{"error":"no such repo"}"#.to_string());
    }
    let head = match git(&["rev-parse", "--verify", "HEAD^{commit}"], Some(&bare)) {
        Ok(h) => h.trim().to_string(),
        Err(_) => return (400, r#"{"error":"repository has no commits"}"#.to_string()),
    };

    let lock = repo_lock(repo);
    let _guard = lock.lock().expect("repo lock");
    let ws_dir = root.join(".choir").join("workspaces").join(repo).join(name);
    if ws_dir.exists() {
        return (409, r#"{"error":"workspace already exists"}"#.to_string());
    }

    match provision(root, &bare, repo, &head, &ws_dir, base_url) {
        Ok(copy_ms) => {
            let workspace = format!("{repo}/{name}");
            if let Err(reason) = platform.set_workspace_head(&workspace, &head, user) {
                // Roll back the directory so a rejected registration
                // leaves no half-created workspace.
                std::fs::remove_dir_all(&ws_dir).ok();
                return (409, serde_json::json!({ "error": reason }).to_string());
            }
            (
                200,
                serde_json::json!({
                    "workspace": workspace,
                    "path": ws_dir.display().to_string(),
                    "head": head,
                    "copy_ms": copy_ms,
                })
                .to_string(),
            )
        }
        Err(e) => (500, serde_json::json!({ "error": e }).to_string()),
    }
}

/// Ensures the template checkout is at `head`, then CoW-copies it to
/// `ws_dir`. Returns the copy's wall-clock milliseconds (the number the
/// wedge advertises; template refresh is amortized across workspaces).
fn provision(
    root: &Path,
    bare: &Path,
    repo: &str,
    head: &str,
    ws_dir: &Path,
    base_url: &str,
) -> Result<f64, String> {
    let template: PathBuf = root.join(".choir").join("checkouts").join(repo);
    if template.join(".git").exists() {
        git(&["fetch", "-q", "origin"], Some(&template))?;
    } else {
        std::fs::create_dir_all(template.parent().expect("has parent"))
            .map_err(|e| format!("create template dir: {e}"))?;
        git(
            &["clone", "-q", bare.to_str().expect("utf8"), template.to_str().expect("utf8")],
            None,
        )?;
    }
    git(&["checkout", "-q", "--detach", head], Some(&template))?;

    std::fs::create_dir_all(ws_dir.parent().expect("has parent"))
        .map_err(|e| format!("create workspace dir: {e}"))?;
    let started = std::time::Instant::now();
    cow_copy(&template, ws_dir)?;
    let copy_ms = started.elapsed().as_secs_f64() * 1000.0;

    // Pushes from the workspace must ride the sequenced smart-HTTP
    // path, never the bare repo's filesystem path.
    git(
        &["remote", "set-url", "origin", &format!("{base_url}/{repo}.git")],
        Some(ws_dir),
    )?;
    Ok(copy_ms)
}
