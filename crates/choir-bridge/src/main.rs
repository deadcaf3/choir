//! Bridge v0 (plan.md D21, read-replica stage): mirror an upstream git
//! repo and record every upstream ref movement as a signed, CAS-checked
//! op through the choir platform API.
//!
//! Single-canonical invariant: the upstream forge is canonical; choir is
//! a follower of the sequencer. This binary never writes back to the
//! upstream — the write-back stage arrives with the GitHub App
//! credentials story (risk register #15).
//!
//! Usage:
//!   choir-bridge <upstream-url> <mirror-path> <api-base> <bridge-key-file> <label> [--once]
//!
//! - `mirror-path`: where the `git clone --mirror` lives (created on
//!   first run, `remote update --prune`d afterwards).
//! - `api-base`: e.g. `http://127.0.0.1:8417` — a choir-node with the
//!   platform API enabled and the bridge's public key registered.
//! - `bridge-key-file`: 32 secret bytes for the bridge's actor key
//!   (created 0600 on first run if absent).
//! - `label`: ref namespace prefix, e.g. `github/git/git`.
//! - `--once`: single sync instead of a 60 s loop.

use choir_bridge::github;

use std::collections::BTreeMap;
use std::path::Path;

use choir_hash::ContentHash;
use choir_identity::ActorKey;
use choir_view::{OpKind, ViewOp};

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Runs git in the mirror, returning stdout or the failure text (a
/// transient fetch failure must not kill the sync loop).
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

/// Loopback API call via curl; returns (status, body).
fn api(method: &str, url: &str, body: Option<&str>) -> (u16, String) {
    let mut args = vec!["-sk", "-w", "\n%{http_code}", "-X", method];
    if let Some(b) = body {
        args.extend(["-d", b]);
    }
    args.push(url);
    let out = std::process::Command::new("curl")
        .args(&args)
        .output()
        .expect("curl runs");
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    match text.rsplit_once('\n') {
        Some((b, code)) => (code.trim().parse().unwrap_or(0), b.to_string()),
        None => (0, text),
    }
}

/// Current upstream refs of the mirror: refname -> oid hex.
fn mirror_refs(mirror: &Path) -> Result<BTreeMap<String, String>, String> {
    Ok(git(
        &["for-each-ref", "--format=%(objectname) %(refname)"],
        Some(mirror),
    )?
    .lines()
    .filter_map(|l| {
        let (oid, name) = l.split_once(' ')?;
        Some((name.to_string(), oid.to_string()))
    })
    .collect())
}

/// Choir-side view of this label's refs: refname -> ContentHash.
fn view_refs(api_base: &str, label: &str) -> BTreeMap<String, ContentHash> {
    let (status, body) = api("GET", &format!("{api_base}/api/view"), None);
    assert_eq!(status, 200, "view: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).expect("view json");
    let prefix = format!("{label}:");
    v["refs"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(k, hex)| {
                    let name = k.strip_prefix(&prefix)?;
                    let hex = hex.as_str()?;
                    let (codec, digest) = hex.split_once('-')?;
                    Some((
                        name.to_string(),
                        ContentHash {
                            codec: u8::from_str_radix(codec, 16).ok()?,
                            digest: (0..digest.len())
                                .step_by(2)
                                .map(|i| u8::from_str_radix(&digest[i..i + 2], 16).ok())
                                .collect::<Option<Vec<u8>>>()?,
                        },
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Ops per `/api/submit-batch` request; keeps request bodies well under
/// a megabyte.
const BATCH: usize = 500;

/// Signs one op into its submit-request JSON object.
fn signed_op(key: &ActorKey, workspace: &str, op: ViewOp) -> serde_json::Value {
    let payload = op.to_payload();
    let sig = key.sign_submission(workspace, &payload);
    serde_json::json!({
        "workspace": workspace,
        "payload_hex": hex_encode(&payload),
        "key_id": sig.key_id,
        "signature_hex": hex_encode(&sig.signature),
    })
}

/// Submits ops in batches; returns the accepted count, printing each
/// rejection. Request bodies go through a temp file — 500 signed ops
/// exceed argv limits.
fn submit_batch(api_base: &str, ops: &[serde_json::Value]) -> Result<usize, String> {
    let mut accepted = 0;
    for chunk in ops.chunks(BATCH) {
        let body = serde_json::json!({ "ops": chunk }).to_string();
        let tmp = std::env::temp_dir().join(format!(
            "choir-bridge-batch-{}-{}",
            std::process::id(),
            accepted
        ));
        std::fs::write(&tmp, &body).map_err(|e| format!("write batch: {e}"))?;
        let out = std::process::Command::new("curl")
            .args(["-sk", "-X", "POST", "--data-binary"])
            .arg(format!("@{}", tmp.display()))
            .arg(format!("{api_base}/api/submit-batch"))
            .output()
            .map_err(|e| format!("spawn curl: {e}"))?;
        std::fs::remove_file(&tmp).ok();
        let resp: serde_json::Value = serde_json::from_slice(&out.stdout)
            .map_err(|e| format!("batch response: {e}"))?;
        accepted += resp["accepted"].as_u64().unwrap_or(0) as usize;
        if resp["rejected"].as_u64().unwrap_or(0) > 0 {
            for r in resp["results"].as_array().into_iter().flatten() {
                if let Some(err) = r.get("error").and_then(|e| e.as_str()) {
                    eprintln!("bridge: op rejected: {err}");
                }
            }
        }
    }
    Ok(accepted)
}

/// One sync round: fetch upstream, diff against the choir view, submit
/// the delta in batches. Returns (set, deleted, unchanged).
fn sync_once(
    upstream: &str,
    mirror: &Path,
    api_base: &str,
    key: &ActorKey,
    label: &str,
) -> Result<(usize, usize, usize), String> {
    if mirror.join("HEAD").exists() {
        git(&["remote", "update", "--prune"], Some(mirror))?;
    } else {
        std::fs::create_dir_all(mirror.parent().unwrap_or(Path::new(".")))
            .map_err(|e| format!("create mirror dir: {e}"))?;
        git(
            &["clone", "--mirror", upstream, mirror.to_str().expect("utf8 path")],
            None,
        )?;
    }

    let upstream_refs = mirror_refs(mirror)?;
    let choir_refs = view_refs(api_base, label);
    let workspace = format!("bridge/{label}");

    let mut sets = Vec::new();
    let mut unchanged = 0;
    for (name, oid) in &upstream_refs {
        let commit = match ContentHash::from_git_oid(oid) {
            Some(c) => c,
            None => continue, // non-oid ref (should not happen)
        };
        let prev = choir_refs.get(name);
        if prev == Some(&commit) {
            unchanged += 1;
            continue;
        }
        let op = ViewOp::new(OpKind::SetRef {
            name: format!("{label}:{name}"),
            commit,
            prev: prev.cloned(),
        });
        sets.push(signed_op(key, &workspace, op));
    }
    let mut deletes = Vec::new();
    for (name, prev) in &choir_refs {
        if !upstream_refs.contains_key(name) {
            let op = ViewOp::new(OpKind::DeleteRef {
                name: format!("{label}:{name}"),
                prev: Some(prev.clone()),
            });
            deletes.push(signed_op(key, &workspace, op));
        }
    }
    let set = submit_batch(api_base, &sets)?;
    let deleted = submit_batch(api_base, &deletes)?;
    Ok((set, deleted, unchanged))
}

/// Loads the 32-byte actor key at `path`, creating it (0600) if absent.
fn load_or_create_key(path: &str) -> ActorKey {
    if Path::new(path).exists() {
        let bytes = std::fs::read(path).expect("read key file");
        let bytes: [u8; 32] = bytes.as_slice().try_into().expect("key file must be 32 bytes");
        ActorKey::from_secret_bytes(&bytes)
    } else {
        let key = ActorKey::generate();
        std::fs::write(path, key.secret_bytes()).expect("write key file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .expect("chmod key file");
        }
        key
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `choir-bridge --pubkey <key-file>`: ensure the key exists and print
    // its public key hex (for the daemon's --keys-file), then exit.
    if args.first().map(String::as_str) == Some("--pubkey") {
        let key_file = args.get(1).expect("--pubkey needs a key file path");
        let key = load_or_create_key(key_file);
        println!("{}", hex_encode(&key.public_key_bytes()));
        return;
    }
    // `choir-bridge app-debug <app-id> <pem-path>`: print the accepted
    // permission set of each installation.
    if args.first().map(String::as_str) == Some("app-debug") {
        let (Some(app_id), Some(pem)) = (args.get(1), args.get(2)) else {
            eprintln!("usage: choir-bridge app-debug <app-id> <pem-path>");
            std::process::exit(2);
        };
        match github::app_jwt(app_id, Path::new(pem))
            .and_then(|jwt| github::installations_debug(&jwt))
        {
            Ok(body) => {
                let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
                for i in v.as_array().into_iter().flatten() {
                    println!(
                        "installation {}: permissions {}",
                        i["id"],
                        i["permissions"]
                    );
                }
            }
            Err(e) => {
                eprintln!("app-debug failed: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    // `choir-bridge post-status <app-id> <pem-path> <owner/repo> <sha> <state> <description>`
    // Write-back v0: one commit status through the GitHub App flow.
    if args.first().map(String::as_str) == Some("post-status") {
        let [app_id, pem, repo, sha, state, desc] = match &args[1..] {
            [a, b, c, d, e, f] => [a, b, c, d, e, f],
            _ => {
                eprintln!("usage: choir-bridge post-status <app-id> <pem-path> <owner/repo> <sha> <state> <description>");
                std::process::exit(2);
            }
        };
        let result = github::app_jwt(app_id, Path::new(pem))
            .and_then(|jwt| github::installation_token(&jwt))
            .and_then(|token| {
                let sha = if sha == "HEAD" {
                    github::head_sha(&token, repo)?
                } else {
                    sha.clone()
                };
                github::post_status(&token, repo, &sha, state, desc).map(|()| sha)
            });
        match result {
            Ok(sha) => println!("status posted: {repo}@{sha} -> {state}"),
            Err(e) => {
                eprintln!("post-status failed: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    let [upstream, mirror, api_base, key_file, label] = match args.as_slice() {
        [a, b, c, d, e, rest @ ..] if rest.iter().all(|r| r == "--once") => {
            [a.clone(), b.clone(), c.clone(), d.clone(), e.clone()]
        }
        _ => {
            eprintln!(
                "usage: choir-bridge <upstream-url> <mirror-path> <api-base> <bridge-key-file> <label> [--once]"
            );
            std::process::exit(2);
        }
    };
    let once = args.iter().any(|a| a == "--once");
    let mirror = std::path::PathBuf::from(mirror);

    let key = load_or_create_key(&key_file);

    loop {
        let started = std::time::Instant::now();
        match sync_once(&upstream, &mirror, &api_base, &key, &label) {
            Ok((set, deleted, unchanged)) => eprintln!(
                "bridge: {label}: {set} set, {deleted} deleted, {unchanged} unchanged in {:?}",
                started.elapsed()
            ),
            // Transient (network, rate limit): log and retry next round.
            Err(e) => eprintln!("bridge: {label}: sync failed (will retry): {e}"),
        }
        if once {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}
