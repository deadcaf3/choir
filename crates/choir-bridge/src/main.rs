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
fn signed_op(key: &ActorKey, channel: &str, op: ViewOp) -> serde_json::Value {
    let payload = op.to_payload();
    let sig = key.sign_submission(channel, &payload);
    serde_json::json!({
        "channel": channel,
        "workspace": channel,
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

/// How long to wait for CI on the train commit before giving up.
const CI_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);
/// Poll interval while waiting on CI.
const CI_POLL: std::time::Duration = std::time::Duration::from_secs(15);
/// How long to watch the landed tip for post-land CI (D23: train green
/// is necessary, never sufficient — the default-branch push can trigger
/// branch-conditional workflows the train branch never ran).
const POST_LAND_WATCH: std::time::Duration = std::time::Duration::from_secs(180);

/// One queue-as-bot round (D21 queue stage, verdict-only): fetch the
/// open PRs, build the speculative train locally, publish it as the
/// `choir/train` branch so the forge's CI runs on it, wait for the
/// check verdict, and post a per-PR `choir/queue` commit status. With
/// `land`, a green train is then fast-forwarded onto the default
/// branch (non-force push: a lost race is a rejection, not a clobber).
/// One speculative-train round.
///
/// **Decision inputs are structured only** (risk #16, Rule of Two): the
/// train is built from PR numbers and git oids, the verdict comes from CI
/// `status`/`conclusion` enums, and landing is gated on that verdict plus
/// the caller's `land` argument. No pull-request title, body, branch
/// name, author, commit message, or check-run text reaches any
/// conditional here. `land` is passed in from a per-invocation `--land`
/// flag and has no configuration default, so a bridge started without it
/// cannot be talked into landing anything.
fn queue_round(
    app_id: &str,
    pem: &Path,
    repo: &str,
    workdir: &Path,
    land: bool,
) -> Result<(), String> {
    let token = github::app_jwt(app_id, pem).and_then(|jwt| github::installation_token(&jwt))?;
    let base_branch = github::default_branch(&token, repo)?;
    let prs = github::list_open_prs(&token, repo)?;
    if prs.is_empty() {
        println!("queue: no open PRs on {repo}; nothing to do");
        return Ok(());
    }
    // The installation token lives in this URL for the duration of the
    // round; it is passed per-invocation and never written to git
    // config or disk.
    let url = format!("https://x-access-token:{token}@github.com/{repo}.git");

    if !workdir.join(".git").exists() {
        std::fs::create_dir_all(workdir).map_err(|e| format!("create workdir: {e}"))?;
        git(&["init", "-q"], Some(workdir))?;
    }
    let mut fetch: Vec<String> = vec!["fetch".into(), "-q".into(), url.clone()];
    fetch.push(format!("+refs/heads/{base_branch}:refs/choirq/base"));
    for pr in &prs {
        fetch.push(format!("+refs/pull/{}/head:refs/choirq/pr/{}", pr.number, pr.number));
    }
    let fetch_refs: Vec<&str> = fetch.iter().map(String::as_str).collect();
    git(&fetch_refs, Some(workdir))?;
    let base = git(&["rev-parse", "refs/choirq/base"], Some(workdir))?.trim().to_string();

    let heads: Vec<(u64, String)> =
        prs.iter().map(|p| (p.number, format!("refs/choirq/pr/{}", p.number))).collect();
    let train = choir_bridge::queue::build_train(workdir, &base, &heads)?;
    if train.tip == base {
        println!("queue: no PR merged cleanly; train == base, skipping CI");
    } else {
        git(
            &["push", "-q", &url, &format!("+{}:refs/heads/choir/train", train.tip)],
            Some(workdir),
        )?;
        println!("queue: train {} pushed ({} PRs considered)", train.tip, prs.len());
    }

    let verdict = if train.tip == base {
        // Nothing new to test; base is presumed already checked.
        github::Verdict::Success
    } else {
        let deadline = std::time::Instant::now() + CI_TIMEOUT;
        loop {
            let v = github::check_verdict(&token, repo, &train.tip)?;
            match v {
                github::Verdict::Success | github::Verdict::Failure => break v,
                github::Verdict::Pending | github::Verdict::NoRuns => {
                    if std::time::Instant::now() >= deadline {
                        break v;
                    }
                    std::thread::sleep(CI_POLL);
                }
            }
        }
    };

    for entry in &train.entries {
        // Statuses land on the PR head sha, which the fetched ref points at.
        let sha = git(&["rev-parse", &entry.head], Some(workdir))?.trim().to_string();
        let (state, desc) = if !entry.merged {
            ("failure", entry.note.as_str())
        } else {
            match verdict {
                github::Verdict::Success => ("success", "speculative train green"),
                github::Verdict::Failure => ("failure", "train CI failed"),
                github::Verdict::Pending => ("error", "train CI timed out"),
                github::Verdict::NoRuns => ("error", "no CI signal on train"),
            }
        };
        github::post_status(&token, repo, &sha, "choir/queue", state, desc)?;
        println!("queue: PR #{}: {state} ({desc})", entry.id);
    }
    if land && train.tip != base && verdict == github::Verdict::Success {
        choir_bridge::queue::land(workdir, &url, &train.tip, &base_branch)?;
        println!("queue: landed train {} -> {base_branch}", train.tip);
        // Immediately after the push the aggregated verdict is still the
        // pre-land green, so an early Success is only trusted once a new
        // run has been seen Pending; otherwise watch the full window.
        let deadline = std::time::Instant::now() + POST_LAND_WATCH;
        let mut saw_pending = false;
        loop {
            let v = github::check_verdict(&token, repo, &train.tip)?;
            match v {
                github::Verdict::Failure => {
                    let new_tip = choir_bridge::queue::revert_train(
                        workdir, &url, &base, &train.tip, &base_branch,
                    )?;
                    println!(
                        "queue: post-land CI red; reverted train, {base_branch} -> {new_tip}"
                    );
                    for entry in train.entries.iter().filter(|e| e.merged) {
                        let sha =
                            git(&["rev-parse", &entry.head], Some(workdir))?.trim().to_string();
                        github::post_status(
                            &token, repo, &sha, "choir/queue", "failure",
                            "landed train reverted: post-land CI red",
                        )?;
                        println!("queue: PR #{}: reverted", entry.id);
                    }
                    break;
                }
                github::Verdict::Success if saw_pending => break,
                github::Verdict::Pending | github::Verdict::NoRuns | github::Verdict::Success => {
                    saw_pending |= v == github::Verdict::Pending;
                    if std::time::Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(CI_POLL);
                }
            }
        }
    }
    Ok(())
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
                github::post_status(&token, repo, &sha, "choir/bridge", state, desc).map(|()| sha)
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
    // `choir-bridge queue <app-id> <pem-path> <owner/repo> <workdir> [--land] [--watch <secs>]`
    // Queue-as-bot: speculative-train rounds. Verdict-only by default;
    // --land fast-forwards the default branch on a green train (and
    // auto-reverts if post-land CI goes red); --watch repeats rounds
    // forever, sleeping <secs> between them.
    if args.first().map(String::as_str) == Some("queue") {
        let mut land = false;
        let mut watch: Option<u64> = None;
        let mut rest: Vec<&String> = Vec::new();
        let mut it = args[1..].iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--land" => land = true,
                "--watch" => {
                    watch = Some(it.next().and_then(|s| s.parse().ok()).unwrap_or_else(|| {
                        eprintln!("--watch needs an interval in seconds");
                        std::process::exit(2);
                    }));
                }
                _ => rest.push(a),
            }
        }
        let [app_id, pem, repo, workdir] = match rest.as_slice() {
            [a, b, c, d] => [*a, *b, *c, *d],
            _ => {
                eprintln!(
                    "usage: choir-bridge queue <app-id> <pem-path> <owner/repo> <workdir> [--land] [--watch <secs>]"
                );
                std::process::exit(2);
            }
        };
        loop {
            match queue_round(app_id, Path::new(pem), repo, Path::new(workdir), land) {
                Ok(()) => {}
                // In watch mode a failed round (network, rate limit) is
                // logged and retried; one-shot mode exits nonzero.
                Err(e) if watch.is_some() => eprintln!("queue round failed (will retry): {e}"),
                Err(e) => {
                    eprintln!("queue round failed: {e}");
                    std::process::exit(1);
                }
            }
            match watch {
                Some(secs) => std::thread::sleep(std::time::Duration::from_secs(secs)),
                None => break,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_ops_carry_both_channel_spellings() {
        let key = ActorKey::from_secret_bytes(&[19; 32]);
        let op = ViewOp::new(choir_view::OpKind::RecordProvenance {
            subject: "repo/shared".to_string(),
            kind: "plan".to_string(),
            body: "test".to_string(),
        });
        let body = signed_op(&key, "operator/bridge", op);

        assert_eq!(body["channel"], "operator/bridge");
        assert_eq!(body["workspace"], body["channel"]);
    }
}
