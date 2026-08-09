//! Instant workspace provisioning (`POST /api/workspace`) — the D21
//! "productized provisioning" wedge feature.
//!
//! A workspace is a copy-on-write clone of a per-repo template checkout
//! (APFS `clonefile` on macOS, `--reflink=auto` on Linux), so creation
//! cost is O(directory entries), not O(bytes). Each created workspace is
//! registered in the platform view with a node-signed operation, so
//! `/api/view` is the durable workspace inventory. Legacy requests use
//! `SetWorkspaceHead`; adapter-grade requests atomically create a stable
//! change binding at an exact revision.
//!
//! The workspace's `origin` is rewritten to the daemon's own HTTP URL:
//! pushes from a workspace go through the sequenced smart-HTTP path,
//! never straight at the bare repo on disk.

use std::path::{Path, PathBuf};

use choir_oplog::ContentHash;

use crate::platform::Platform;
use crate::reject::{Code, Rejection};

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
    authenticated_user: &str,
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

    let advanced_fields = ["base", "owner", "change", "idempotency_key"];
    let advanced = advanced_fields.iter().any(|key| req.get(*key).is_some());
    if advanced
        && advanced_fields
            .iter()
            .any(|key| req.get(*key).and_then(|v| v.as_str()).is_none_or(str::is_empty))
    {
        return problem(
            400,
            Code::MalformedRequest,
            "advanced workspace creation requires non-empty base, owner, change and idempotency_key",
            "send all four fields, or omit all four to use the legacy HEAD-based request",
        );
    }
    let attribution = format!("git/{authenticated_user}");

    let bare = root.join(format!("{repo}.git"));
    if !bare.join("HEAD").exists() {
        return (404, r#"{"error":"no such repo"}"#.to_string());
    }

    let lock = repo_lock(repo);
    let _guard = lock.lock().expect("repo lock");
    let requested_base = field("base");
    let head = if advanced {
        if ContentHash::from_git_oid(requested_base).is_none() {
            return problem(
                400,
                Code::MalformedRequest,
                "base must be a full 40- or 64-character Git object id",
                "resolve the desired commit to its full object id and retry",
            );
        }
        match git(&["cat-file", "-t", requested_base], Some(&bare)) {
            Ok(kind) if kind.trim() == "commit" => match git(
                &["rev-parse", "--verify", &format!("{requested_base}^{{commit}}")],
                Some(&bare),
            ) {
                Ok(commit) => commit.trim().to_string(),
                Err(_) => {
                    return problem(
                        400,
                        Code::MalformedRequest,
                        "base does not resolve to a commit in this repository",
                        "fetch the exact commit into the repository, then retry with its full object id",
                    )
                }
            },
            _ => {
                return problem(
                    400,
                    Code::MalformedRequest,
                    "base does not name a commit in this repository",
                    "fetch the exact commit into the repository, then retry with its full object id",
                )
            }
        }
    } else {
        match git(&["rev-parse", "--verify", "HEAD^{commit}"], Some(&bare)) {
            Ok(h) => h.trim().to_string(),
            Err(_) => return (400, r#"{"error":"repository has no commits"}"#.to_string()),
        }
    };

    let ws_dir = root.join(".choir").join("workspaces").join(repo).join(name);
    let workspace = format!("{repo}/{name}");
    if ws_dir.exists() {
        if advanced {
            return reuse_or_conflict(
                platform,
                &workspace,
                &ws_dir,
                AdvancedBinding {
                    base_hex: &head,
                    owner: field("owner"),
                    change_id: field("change"),
                    idempotency_key: field("idempotency_key"),
                    attribution: &attribution,
                },
            );
        }
        return (409, r#"{"error":"workspace already exists"}"#.to_string());
    }
    if advanced
        && (platform.change_state(field("change")).is_some()
            || platform
                .change_for_idempotency(field("owner"), field("idempotency_key"))
                .is_some()
            || platform.workspace_head(&workspace).is_some())
    {
        return lifecycle_conflict(
            "the requested change, idempotency key or workspace already has a different durable state",
        );
    }

    match provision(root, &bare, repo, &head, &ws_dir, base_url) {
        Ok(copy_ms) => {
            let registration = if advanced {
                platform
                    .create_change(
                        field("change"),
                        field("owner"),
                        &workspace,
                        &head,
                        field("idempotency_key"),
                        &attribution,
                    )
                    .map(Some)
            } else {
                platform
                    .set_workspace_head(&workspace, &head, &attribution)
                    .map(|()| None)
            };
            let accepted = match registration {
                Ok(accepted) => accepted,
                Err(reason) => {
                    // Roll back the directory so a rejected registration
                    // leaves no half-created workspace.
                    std::fs::remove_dir_all(&ws_dir).ok();
                    return (409, Rejection::decode(&reason).body());
                }
            };
            let mut response = serde_json::json!({
                "workspace": workspace,
                "path": ws_dir.display().to_string(),
                "head": head,
                "copy_ms": copy_ms,
                "created": true,
            });
            if advanced {
                response["change_id"] = serde_json::json!(field("change"));
                response["owner"] = serde_json::json!(field("owner"));
                response["idempotency_key"] = serde_json::json!(field("idempotency_key"));
                response["base_revision"] = serde_json::json!(
                    ContentHash::from_git_oid(&head)
                        .expect("verified git oid")
                        .to_hex()
                );
                if let Some(accepted) = accepted {
                    response["operation"] = serde_json::json!({
                        "seq": accepted.seq,
                        "hash": accepted.hash.to_hex(),
                    });
                }
            }
            (
                200,
                response.to_string(),
            )
        }
        Err(e) => (500, serde_json::json!({ "error": e }).to_string()),
    }
}

/// Handles `POST /api/workspace/archive`. The live checkout is moved to a
/// recoverable same-filesystem archive before the durable view operation.
/// A sequencing refusal restores the live path.
pub fn archive_workspace(
    root: &Path,
    platform: &Platform,
    authenticated_user: &str,
    body: &[u8],
) -> (u16, String) {
    let req: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            return problem(
                400,
                Code::MalformedRequest,
                format!("request body is not valid JSON: {e}"),
                "send repo, name, change, idempotency_key and an owner-signed archive authorization",
            )
        }
    };
    let field = |key: &str| req.get(key).and_then(|v| v.as_str()).unwrap_or("");
    let (repo, name) = (field("repo"), field("name"));
    let mut segs = repo.split('/');
    let (Some(repo_owner), Some(reponame), None) =
        (segs.next(), segs.next(), segs.next())
    else {
        return problem(
            400,
            Code::MalformedRequest,
            "repo must be owner/repo",
            "send a two-segment repository name",
        );
    };
    if !safe_segment(repo_owner) || !safe_segment(reponame) || !safe_segment(name) {
        return problem(
            400,
            Code::MalformedRequest,
            "bad repo or workspace name",
            "use non-hidden alphanumeric, dot, underscore or hyphen path segments",
        );
    }
    if ["change", "idempotency_key", "channel"]
        .iter()
        .any(|key| field(key).is_empty())
    {
        return problem(
            400,
            Code::MalformedRequest,
            "archive requires non-empty change, idempotency_key and signed channel",
            "retry with the exact binding returned by workspace creation and an owner-signed ArchiveAuthorization payload",
        );
    }

    let lock = repo_lock(repo);
    let _guard = lock.lock().expect("repo lock");
    let attribution = format!("git/{authenticated_user}");
    let workspace = format!("{repo}/{name}");
    let Some(change) = platform.change_state(field("change")) else {
        return lifecycle_conflict("no durable change has the requested identity");
    };
    if change.owner != field("channel")
        || change.workspace_id != workspace
        || change.idempotency_key != field("idempotency_key")
    {
        return lifecycle_conflict(
            "archive binding does not match the durable change channel, workspace and idempotency key",
        );
    }

    let live = root.join(".choir").join("workspaces").join(repo).join(name);
    let archived = root.join(".choir").join("archive").join("workspaces").join(repo).join(name);
    if change.active_workspace.is_none() {
        if archived.exists() && !live.exists() {
            let operation = platform.archive_change_receipt(
                &req,
                field("change"),
                &workspace,
                &change.revision_id,
                &attribution,
            );
            let mut response = serde_json::json!({
                "workspace": workspace,
                "change_id": field("change"),
                "archived_path": archived.display().to_string(),
                "already_archived": true,
            });
            if let Some((seq, hash)) = operation {
                response["operation"] = serde_json::json!({
                    "seq": seq,
                    "hash": hash.to_hex(),
                    "already_applied": true,
                });
            }
            return (
                200,
                response.to_string(),
            );
        }
        return lifecycle_conflict(
            "the change is archived but its filesystem state is inconsistent",
        );
    }
    if let Err(reason) = platform.validate_archive_change_request(
        &req,
        field("change"),
        &workspace,
        &change.revision_id,
    ) {
        let rejection = Rejection::decode(&reason);
        let status = if rejection.code == Code::WorkspaceState.as_str() {
            409
        } else {
            400
        };
        return (status, rejection.body());
    }

    let recovering_rename = archived.exists() && !live.exists();
    if !recovering_rename {
        if !live.exists() || archived.exists() {
            return lifecycle_conflict(
                "the active workspace filesystem state does not match the durable view",
            );
        }
        if let Err(e) = std::fs::create_dir_all(archived.parent().expect("archive has parent")) {
            return (500, serde_json::json!({ "error": format!("create archive dir: {e}") }).to_string());
        }
        if let Err(e) = std::fs::rename(&live, &archived) {
            return (500, serde_json::json!({ "error": format!("archive workspace: {e}") }).to_string());
        }
    }

    match platform.submit_archive_change(
        &req,
        field("change"),
        &workspace,
        &change.revision_id,
        &attribution,
    ) {
        Ok(accepted) => (
            200,
            serde_json::json!({
                "workspace": workspace,
                "change_id": field("change"),
                "archived_path": archived.display().to_string(),
                "already_archived": false,
                "operation": {
                    "seq": accepted.seq,
                    "hash": accepted.hash.to_hex(),
                },
            })
            .to_string(),
        ),
        Err(reason) => {
            if let Err(e) = std::fs::rename(&archived, &live) {
                return (500, serde_json::json!({
                    "error": format!("sequencing failed and archive rollback failed: {e}"),
                    "sequencer_error": Rejection::decode(&reason).to_json(),
                }).to_string());
            }
            (409, Rejection::decode(&reason).body())
        }
    }
}

struct AdvancedBinding<'a> {
    base_hex: &'a str,
    owner: &'a str,
    change_id: &'a str,
    idempotency_key: &'a str,
    attribution: &'a str,
}

fn reuse_or_conflict(
    platform: &Platform,
    workspace: &str,
    path: &Path,
    binding: AdvancedBinding<'_>,
) -> (u16, String) {
    let AdvancedBinding {
        base_hex,
        owner,
        change_id,
        idempotency_key,
        attribution,
    } = binding;
    let Some(change) = platform.change_state(change_id) else {
        return lifecycle_conflict(
            "workspace directory exists without the requested durable change binding",
        );
    };
    let base = ContentHash::from_git_oid(base_hex).expect("validated base");
    if change.owner != owner
        || change.workspace_id != workspace
        || change.active_workspace.as_deref() != Some(workspace)
        || change.base_revision != base
        || change.idempotency_key != idempotency_key
    {
        return lifecycle_conflict(
            "workspace exists with a different base, owner, change or idempotency key",
        );
    }
    let Some(head) = platform.workspace_head(workspace) else {
        return lifecycle_conflict(
            "workspace directory and change exist but the active workspace head is missing",
        );
    };
    if head != change.revision_id {
        return lifecycle_conflict(
            "workspace and change revisions disagree; refusing to guess which state is current",
        );
    }
    let head_hex = git_oid_hex(&head).expect("workspace revisions are verified Git oids");
    let mut response = serde_json::json!({
            "workspace": workspace,
            "path": path.display().to_string(),
            "head": head_hex,
            "base_revision": change.base_revision.to_hex(),
            "revision_id": change.revision_id.to_hex(),
            "change_id": change_id,
            "owner": owner,
            "idempotency_key": idempotency_key,
            "created": false,
            "reused": true,
        });
    if let Some((seq, hash)) = platform.create_change_receipt(
        change_id,
        owner,
        workspace,
        base_hex,
        idempotency_key,
        attribution,
    ) {
        response["operation"] = serde_json::json!({
            "seq": seq,
            "hash": hash.to_hex(),
            "already_applied": true,
        });
    }
    (200, response.to_string())
}

fn git_oid_hex(hash: &ContentHash) -> Option<String> {
    let expected = match hash.codec {
        0x11 => 20,
        0x12 => 32,
        _ => return None,
    };
    if hash.digest.len() != expected {
        return None;
    }
    Some(hash.digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn lifecycle_conflict(error: impl Into<String>) -> (u16, String) {
    problem(
        409,
        Code::WorkspaceState,
        error,
        "read GET /api/view and retry only with the exact durable binding, or choose a new workspace and change id",
    )
}

fn problem(
    status: u16,
    code: Code,
    error: impl Into<String>,
    next: impl Into<String>,
) -> (u16, String) {
    (status, Rejection::new(code, error, next).body())
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
